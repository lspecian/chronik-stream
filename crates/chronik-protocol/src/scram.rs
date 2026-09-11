//! SCRAM-SHA-256 / SCRAM-SHA-512 (RFC 5802, RFC 7677) for SASL.
//!
//! # What was here before
//!
//! An earlier "implementation" generated a server-first message with the literal
//! salt `base64("salt")`, hardcoded `i=4096`, and then accepted **any**
//! client-final message with the comment *"For this stub, we'll accept any
//! client-final message"*. The client proof was never verified and the server
//! signature was `base64("server-signature")`. It was advertised in the
//! handshake, so a client that selected SCRAM authenticated with any password.
//! That code was deleted; this is a real implementation.
//!
//! # The exchange
//!
//! ```text
//! client-first  n,,n=<user>,r=<client-nonce>
//! server-first  r=<client-nonce><server-nonce>,s=<base64 salt>,i=<iterations>
//! client-final  c=biws,r=<combined-nonce>,p=<base64 client-proof>
//! server-final  v=<base64 server-signature>
//! ```
//!
//! With `SaltedPassword = Hi(Normalize(password), salt, i)`:
//!
//! ```text
//! ClientKey       = HMAC(SaltedPassword, "Client Key")
//! StoredKey       = H(ClientKey)
//! ServerKey       = HMAC(SaltedPassword, "Server Key")
//! AuthMessage     = client-first-bare + "," + server-first + "," + client-final-without-proof
//! ClientSignature = HMAC(StoredKey, AuthMessage)
//! ClientProof     = ClientKey XOR ClientSignature
//! ServerSignature = HMAC(ServerKey, AuthMessage)
//! ```
//!
//! The server stores only `StoredKey` and `ServerKey`, never the password. It
//! verifies by recovering `ClientKey = ClientProof XOR ClientSignature` and
//! checking `H(ClientKey) == StoredKey`.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256, Sha512};
use subtle::ConstantTimeEq;

use crate::sasl::{SaslError, SaslMechanism};

/// Default PBKDF2 iteration count for newly derived credentials.
///
/// Kafka's own default is 4096, which RFC 7677 also names as the minimum a
/// server should accept. It is low by modern password-hashing standards, but
/// the iteration count is carried in the server-first message and clients must
/// agree, so raising it beyond what clients tolerate breaks interoperability.
pub const DEFAULT_ITERATIONS: u32 = 4096;

/// Minimum iteration count accepted when constructing credentials.
pub const MIN_ITERATIONS: u32 = 4096;

const CLIENT_KEY_LABEL: &[u8] = b"Client Key";
const SERVER_KEY_LABEL: &[u8] = b"Server Key";

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// A stored SCRAM credential. Contains no password-equivalent material usable
/// to authenticate *as* the user (an attacker with `StoredKey` alone cannot
/// produce a valid `ClientProof` without `ClientKey`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScramCredential {
    pub mechanism: SaslMechanism,
    pub salt: Vec<u8>,
    pub stored_key: Vec<u8>,
    pub server_key: Vec<u8>,
    pub iterations: u32,
}

impl ScramCredential {
    /// Derive a credential from a plaintext password, generating a fresh random
    /// salt.
    pub fn derive(
        password: &str,
        mechanism: SaslMechanism,
        iterations: u32,
    ) -> Result<Self, SaslError> {
        let mut salt = vec![0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        Self::derive_with_salt(password, mechanism, iterations, salt)
    }

    /// Build a credential from a client-supplied salt and *salted password*.
    ///
    /// This is what `AlterUserScramCredentials` (API 51) carries: the client
    /// runs `Hi(password, salt, iterations)` itself and sends the result, so the
    /// plaintext password never crosses the wire and the broker never sees it.
    /// The broker only needs the two derived keys.
    ///
    /// Note there is deliberately no verification possible here — the broker
    /// cannot check that the salted password corresponds to any particular
    /// password, which is the whole point. It stores what it is given.
    pub fn from_salted_password(
        salt: Vec<u8>,
        salted_password: &[u8],
        mechanism: SaslMechanism,
        iterations: u32,
    ) -> Result<Self, SaslError> {
        if iterations < MIN_ITERATIONS {
            return Err(SaslError::InternalError(format!(
                "SCRAM iteration count {} is below the minimum {}",
                iterations, MIN_ITERATIONS
            )));
        }
        if salted_password.len() != digest_len(mechanism) {
            return Err(SaslError::InternalError(format!(
                "salted password is {} bytes, expected {} for {}",
                salted_password.len(),
                digest_len(mechanism),
                mechanism.as_str()
            )));
        }

        let client_key = hmac(salted_password, CLIENT_KEY_LABEL, mechanism)?;
        let stored_key = hash(&client_key, mechanism);
        let server_key = hmac(salted_password, SERVER_KEY_LABEL, mechanism)?;

        Ok(Self {
            mechanism,
            salt,
            stored_key,
            server_key,
            iterations,
        })
    }

    /// Derive a credential using a caller-supplied salt (used by tests and by
    /// credential import, where the salt must be reproduced exactly).
    pub fn derive_with_salt(
        password: &str,
        mechanism: SaslMechanism,
        iterations: u32,
        salt: Vec<u8>,
    ) -> Result<Self, SaslError> {
        if iterations < MIN_ITERATIONS {
            return Err(SaslError::InternalError(format!(
                "SCRAM iteration count {} is below the minimum {}",
                iterations, MIN_ITERATIONS
            )));
        }

        let salted = hi(password.as_bytes(), &salt, iterations, mechanism)?;
        let client_key = hmac(&salted, CLIENT_KEY_LABEL, mechanism)?;
        let stored_key = hash(&client_key, mechanism);
        let server_key = hmac(&salted, SERVER_KEY_LABEL, mechanism)?;

        Ok(Self {
            mechanism,
            salt,
            stored_key,
            server_key,
            iterations,
        })
    }
}

/// Server-side state for one in-flight SCRAM exchange.
#[derive(Debug, Clone)]
pub struct ScramExchange {
    mechanism: SaslMechanism,
    username: String,
    /// `client-first-bare`, retained verbatim for the AuthMessage.
    client_first_bare: String,
    server_first: String,
    combined_nonce: String,
    credential: ScramCredential,
}

impl ScramExchange {
    /// Process `client-first` and produce `server-first`.
    ///
    /// `lookup` resolves the username to a stored credential. When the user is
    /// unknown the caller should still run the exchange against a dummy
    /// credential so that timing does not distinguish "no such user" from "wrong
    /// password"; [`ScramExchange::start`] does not decide that policy, it just
    /// takes whatever credential it is given.
    pub fn start(
        mechanism: SaslMechanism,
        client_first: &[u8],
        credential_for: impl FnOnce(&str) -> Option<ScramCredential>,
    ) -> Result<(Self, Vec<u8>), SaslError> {
        let message = std::str::from_utf8(client_first)
            .map_err(|_| SaslError::ProtocolError("client-first is not valid UTF-8".into()))?;

        // GS2 header: "n,," / "y,," / "p=<name>,,". We do not support channel
        // binding, so a client demanding it ("p=") must be refused rather than
        // silently downgraded.
        if message.starts_with("p=") {
            return Err(SaslError::ProtocolError(
                "channel binding requested but not supported".into(),
            ));
        }
        let bare_start = gs2_header_len(message)?;
        let client_first_bare = &message[bare_start..];

        let username = attribute(client_first_bare, 'n')
            .ok_or_else(|| SaslError::ProtocolError("client-first has no username".into()))?;
        let client_nonce = attribute(client_first_bare, 'r')
            .ok_or_else(|| SaslError::ProtocolError("client-first has no nonce".into()))?;

        if client_nonce.is_empty() {
            return Err(SaslError::ProtocolError("client nonce is empty".into()));
        }

        let username = saslprep_decode(&username);

        // A missing user must not short-circuit: returning a distinct error here
        // tells an attacker which usernames exist, and skips the PBKDF2 work so
        // the timing differs too. Run the full exchange against a dummy
        // credential and fail at proof verification like any wrong password.
        let credential = credential_for(&username)
            .filter(|c| c.mechanism == mechanism)
            .unwrap_or_else(|| dummy_credential(mechanism));

        let server_nonce = generate_nonce();
        let combined_nonce = format!("{}{}", client_nonce, server_nonce);

        let server_first = format!(
            "r={},s={},i={}",
            combined_nonce,
            b64().encode(&credential.salt),
            credential.iterations
        );

        let exchange = Self {
            mechanism,
            username,
            client_first_bare: client_first_bare.to_string(),
            server_first: server_first.clone(),
            combined_nonce,
            credential,
        };

        Ok((exchange, server_first.into_bytes()))
    }

    /// Verify `client-final` and produce `server-final`.
    pub fn finish(&self, client_final: &[u8]) -> Result<Vec<u8>, SaslError> {
        let message = std::str::from_utf8(client_final)
            .map_err(|_| SaslError::ProtocolError("client-final is not valid UTF-8".into()))?;

        let nonce = attribute(message, 'r')
            .ok_or_else(|| SaslError::ProtocolError("client-final has no nonce".into()))?;

        // The nonce ties client-final to the server-first we issued; without this
        // check the exchange is replayable across sessions.
        //
        // RFC 5802 says the client echoes the server's nonce verbatim, so the
        // obvious check is equality. It is wrong in practice: librdkafka
        // prepended its own client nonce *again* to the server-sent nonce (which
        // already begins with it), so a conforming server sees
        // `cnonce || cnonce || snonce`. The bug dates to librdkafka v0.0.99 and
        // was only fixed in v2.6 (librdkafka #4895) — every client built against
        // an older librdkafka, which is most of the deployed Go/Python/C++
        // ecosystem, still behaves this way.
        //
        // Apache Kafka accommodated it in 3.8.1 by relaxing its own check from
        // `equals` to `endsWith`, so this matches upstream rather than inventing
        // a private rule. It stays safe: the nonce must still END with the full
        // nonce we issued, which embeds our unpredictable server nonce, and the
        // proof below is computed over the client-final exactly as received, so
        // any prefix the client added is covered by the signature.
        if !nonce.ends_with(&self.combined_nonce) {
            return Err(SaslError::ProtocolError(
                "client-final nonce does not match server-first".into(),
            ));
        }

        let proof_b64 = attribute(message, 'p')
            .ok_or_else(|| SaslError::ProtocolError("client-final has no proof".into()))?;
        let client_proof = b64()
            .decode(proof_b64.as_bytes())
            .map_err(|_| SaslError::ProtocolError("client proof is not valid base64".into()))?;

        // AuthMessage uses client-final *without* the proof attribute.
        let without_proof = message
            .rsplit_once(",p=")
            .map(|(head, _)| head)
            .ok_or_else(|| SaslError::ProtocolError("malformed client-final".into()))?;

        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, self.server_first, without_proof
        );

        let client_signature = hmac(
            &self.credential.stored_key,
            auth_message.as_bytes(),
            self.mechanism,
        )?;

        if client_proof.len() != client_signature.len() {
            return Err(SaslError::InvalidCredentials);
        }

        // ClientKey = ClientProof XOR ClientSignature; the credential is valid
        // iff H(ClientKey) equals the StoredKey we hold.
        let recovered_client_key: Vec<u8> = client_proof
            .iter()
            .zip(client_signature.iter())
            .map(|(p, s)| p ^ s)
            .collect();
        let recovered_stored_key = hash(&recovered_client_key, self.mechanism);

        if recovered_stored_key
            .ct_eq(&self.credential.stored_key)
            .unwrap_u8()
            != 1
        {
            return Err(SaslError::InvalidCredentials);
        }

        let server_signature = hmac(
            &self.credential.server_key,
            auth_message.as_bytes(),
            self.mechanism,
        )?;

        Ok(format!("v={}", b64().encode(server_signature)).into_bytes())
    }

    pub fn username(&self) -> &str {
        &self.username
    }
}

/// Length of the GS2 header, i.e. the offset at which `client-first-bare` begins.
fn gs2_header_len(message: &str) -> Result<usize, SaslError> {
    // "n,," or "y,," or "n,a=authzid,"
    let mut commas = 0;
    for (idx, ch) in message.char_indices() {
        if ch == ',' {
            commas += 1;
            if commas == 2 {
                return Ok(idx + 1);
            }
        }
    }
    Err(SaslError::ProtocolError(
        "client-first has no GS2 header".into(),
    ))
}

/// Read a single-letter SCRAM attribute (`n=`, `r=`, `s=`, `p=` …).
fn attribute(message: &str, key: char) -> Option<String> {
    let prefix = format!("{}=", key);
    message
        .split(',')
        .find(|part| part.starts_with(&prefix))
        .map(|part| part[prefix.len()..].to_string())
}

/// Undo SCRAM's username escaping: `=2C` is a comma, `=3D` an equals sign.
fn saslprep_decode(username: &str) -> String {
    username.replace("=2C", ",").replace("=3D", "=")
}

/// Escape a username for inclusion in a SCRAM message.
pub fn saslprep_encode(username: &str) -> String {
    username.replace('=', "=3D").replace(',', "=2C")
}

fn generate_nonce() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    // Base64 without padding; the nonce must not contain a comma.
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

/// A credential no password can satisfy, used for unknown users so that the
/// exchange takes the same shape and roughly the same time as a real one.
fn dummy_credential(mechanism: SaslMechanism) -> ScramCredential {
    let mut salt = vec![0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let mut stored_key = vec![0u8; digest_len(mechanism)];
    rand::thread_rng().fill_bytes(&mut stored_key);
    let mut server_key = vec![0u8; digest_len(mechanism)];
    rand::thread_rng().fill_bytes(&mut server_key);
    ScramCredential {
        mechanism,
        salt,
        stored_key,
        server_key,
        iterations: DEFAULT_ITERATIONS,
    }
}

fn digest_len(mechanism: SaslMechanism) -> usize {
    match mechanism {
        SaslMechanism::ScramSha512 => 64,
        _ => 32,
    }
}

fn hash(data: &[u8], mechanism: SaslMechanism) -> Vec<u8> {
    match mechanism {
        SaslMechanism::ScramSha512 => Sha512::digest(data).to_vec(),
        _ => Sha256::digest(data).to_vec(),
    }
}

fn hmac(key: &[u8], data: &[u8], mechanism: SaslMechanism) -> Result<Vec<u8>, SaslError> {
    match mechanism {
        SaslMechanism::ScramSha512 => {
            let mut mac = Hmac::<Sha512>::new_from_slice(key)
                .map_err(|e| SaslError::InternalError(format!("HMAC key error: {}", e)))?;
            mac.update(data);
            Ok(mac.finalize().into_bytes().to_vec())
        }
        _ => {
            let mut mac = Hmac::<Sha256>::new_from_slice(key)
                .map_err(|e| SaslError::InternalError(format!("HMAC key error: {}", e)))?;
            mac.update(data);
            Ok(mac.finalize().into_bytes().to_vec())
        }
    }
}

/// `Hi(str, salt, i)` — PBKDF2 with the mechanism's hash.
fn hi(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    mechanism: SaslMechanism,
) -> Result<Vec<u8>, SaslError> {
    let mut out = vec![0u8; digest_len(mechanism)];
    let result = match mechanism {
        SaslMechanism::ScramSha512 => {
            pbkdf2::pbkdf2::<Hmac<Sha512>>(password, salt, iterations, &mut out)
        }
        _ => pbkdf2::pbkdf2::<Hmac<Sha256>>(password, salt, iterations, &mut out),
    };
    result.map_err(|e| SaslError::InternalError(format!("PBKDF2 failed: {}", e)))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the client half of SCRAM, so the tests exercise a full exchange
    /// rather than trusting the server's own arithmetic.
    fn client_final(
        password: &str,
        mechanism: SaslMechanism,
        client_first_bare: &str,
        server_first: &str,
    ) -> String {
        let salt = b64()
            .decode(attribute(server_first, 's').unwrap())
            .unwrap();
        let iterations: u32 = attribute(server_first, 'i').unwrap().parse().unwrap();
        let nonce = attribute(server_first, 'r').unwrap();

        let salted = hi(password.as_bytes(), &salt, iterations, mechanism).unwrap();
        let client_key = hmac(&salted, CLIENT_KEY_LABEL, mechanism).unwrap();
        let stored_key = hash(&client_key, mechanism);

        let without_proof = format!("c=biws,r={}", nonce);
        let auth_message = format!(
            "{},{},{}",
            client_first_bare, server_first, without_proof
        );
        let client_signature = hmac(&stored_key, auth_message.as_bytes(), mechanism).unwrap();
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_signature.iter())
            .map(|(k, s)| k ^ s)
            .collect();

        format!("{},p={}", without_proof, b64().encode(proof))
    }

    fn run_exchange(
        stored_password: &str,
        attempt_password: &str,
        mechanism: SaslMechanism,
    ) -> Result<Vec<u8>, SaslError> {
        let credential = ScramCredential::derive(stored_password, mechanism, DEFAULT_ITERATIONS)
            .unwrap();

        let client_first_bare = "n=alice,r=clientnonce123";
        let client_first = format!("n,,{}", client_first_bare);

        let (exchange, server_first) =
            ScramExchange::start(mechanism, client_first.as_bytes(), |_| {
                Some(credential.clone())
            })?;
        let server_first = String::from_utf8(server_first).unwrap();

        let final_msg = client_final(
            attempt_password,
            mechanism,
            client_first_bare,
            &server_first,
        );
        exchange.finish(final_msg.as_bytes())
    }

    #[test]
    fn correct_password_authenticates_sha256() {
        let server_final =
            run_exchange("hunter2", "hunter2", SaslMechanism::ScramSha256).unwrap();
        assert!(String::from_utf8(server_final).unwrap().starts_with("v="));
    }

    #[test]
    fn correct_password_authenticates_sha512() {
        let server_final =
            run_exchange("hunter2", "hunter2", SaslMechanism::ScramSha512).unwrap();
        assert!(String::from_utf8(server_final).unwrap().starts_with("v="));
    }

    /// The case the previous stub passed: it accepted any client-final message.
    #[test]
    fn wrong_password_is_rejected_sha256() {
        let result = run_exchange("hunter2", "wrong", SaslMechanism::ScramSha256);
        assert!(matches!(result, Err(SaslError::InvalidCredentials)));
    }

    #[test]
    fn wrong_password_is_rejected_sha512() {
        let result = run_exchange("hunter2", "wrong", SaslMechanism::ScramSha512);
        assert!(matches!(result, Err(SaslError::InvalidCredentials)));
    }

    #[test]
    fn unknown_user_is_rejected() {
        let client_first = "n,,n=nobody,r=clientnonce123";
        let (exchange, server_first) =
            ScramExchange::start(SaslMechanism::ScramSha256, client_first.as_bytes(), |_| None)
                .unwrap();
        let server_first = String::from_utf8(server_first).unwrap();

        // The exchange still proceeds (no user enumeration), then fails.
        let final_msg = client_final(
            "any-password",
            SaslMechanism::ScramSha256,
            "n=nobody,r=clientnonce123",
            &server_first,
        );
        assert!(matches!(
            exchange.finish(final_msg.as_bytes()),
            Err(SaslError::InvalidCredentials)
        ));
    }

    /// A tampered proof must not authenticate.
    #[test]
    fn tampered_proof_is_rejected() {
        let credential =
            ScramCredential::derive("hunter2", SaslMechanism::ScramSha256, DEFAULT_ITERATIONS)
                .unwrap();
        let client_first_bare = "n=alice,r=clientnonce123";
        let (exchange, server_first) = ScramExchange::start(
            SaslMechanism::ScramSha256,
            format!("n,,{}", client_first_bare).as_bytes(),
            |_| Some(credential.clone()),
        )
        .unwrap();
        let server_first = String::from_utf8(server_first).unwrap();

        let good = client_final(
            "hunter2",
            SaslMechanism::ScramSha256,
            client_first_bare,
            &server_first,
        );
        // Flip a bit in the base64 proof.
        let (head, proof) = good.rsplit_once(",p=").unwrap();
        let mut raw = b64().decode(proof).unwrap();
        raw[0] ^= 0x01;
        let tampered = format!("{},p={}", head, b64().encode(raw));

        assert!(matches!(
            exchange.finish(tampered.as_bytes()),
            Err(SaslError::InvalidCredentials)
        ));
    }

    /// Replaying a client-final from a different exchange must fail: the nonce
    /// binds it to the server-first we issued.
    #[test]
    fn replayed_nonce_is_rejected() {
        let credential =
            ScramCredential::derive("hunter2", SaslMechanism::ScramSha256, DEFAULT_ITERATIONS)
                .unwrap();
        let client_first_bare = "n=alice,r=clientnonce123";
        let client_first = format!("n,,{}", client_first_bare);

        let (first, server_first_a) = ScramExchange::start(
            SaslMechanism::ScramSha256,
            client_first.as_bytes(),
            |_| Some(credential.clone()),
        )
        .unwrap();
        let (second, _server_first_b) = ScramExchange::start(
            SaslMechanism::ScramSha256,
            client_first.as_bytes(),
            |_| Some(credential.clone()),
        )
        .unwrap();

        // A valid final message for exchange `first`...
        let msg = client_final(
            "hunter2",
            SaslMechanism::ScramSha256,
            client_first_bare,
            &String::from_utf8(server_first_a).unwrap(),
        );
        assert!(first.finish(msg.as_bytes()).is_ok());
        // ...must not satisfy exchange `second`, which issued a different nonce.
        assert!(second.finish(msg.as_bytes()).is_err());
    }

    #[test]
    fn server_nonces_are_unique() {
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b);
        assert!(!a.contains(','), "nonce must not contain the SCRAM separator");
    }

    #[test]
    fn channel_binding_request_is_refused() {
        let result = ScramExchange::start(
            SaslMechanism::ScramSha256,
            b"p=tls-unique,,n=alice,r=nonce",
            |_| None,
        );
        assert!(matches!(result, Err(SaslError::ProtocolError(_))));
    }

    #[test]
    fn iterations_below_minimum_are_refused() {
        let result = ScramCredential::derive("pw", SaslMechanism::ScramSha256, 1000);
        assert!(result.is_err());
    }

    #[test]
    fn username_escaping_round_trips() {
        assert_eq!(saslprep_decode(&saslprep_encode("a,b=c")), "a,b=c");
    }

    /// The mechanism is part of the credential: a SHA-256 credential must not
    /// satisfy a SHA-512 exchange.
    #[test]
    fn mechanism_mismatch_falls_back_to_dummy() {
        let sha256_cred =
            ScramCredential::derive("hunter2", SaslMechanism::ScramSha256, DEFAULT_ITERATIONS)
                .unwrap();
        let client_first_bare = "n=alice,r=clientnonce123";
        let (exchange, server_first) = ScramExchange::start(
            SaslMechanism::ScramSha512,
            format!("n,,{}", client_first_bare).as_bytes(),
            |_| Some(sha256_cred.clone()),
        )
        .unwrap();

        let msg = client_final(
            "hunter2",
            SaslMechanism::ScramSha512,
            client_first_bare,
            &String::from_utf8(server_first).unwrap(),
        );
        assert!(matches!(
            exchange.finish(msg.as_bytes()),
            Err(SaslError::InvalidCredentials)
        ));
    }

    /// librdkafka before v2.6 prepends its client nonce to the server-sent
    /// nonce, which already begins with it, producing
    /// `cnonce || cnonce || snonce` (librdkafka #4895). Apache Kafka accepts
    /// this from 3.8.1 by matching with `endsWith`; so must we, or SCRAM is
    /// unusable from most deployed clients.
    #[test]
    fn librdkafka_double_prefixed_nonce_is_accepted() {
        let mechanism = SaslMechanism::ScramSha256;
        let credential =
            ScramCredential::derive("hunter2", mechanism, DEFAULT_ITERATIONS).unwrap();
        let client_nonce = "clientnonce123";
        let client_first_bare = format!("n=alice,r={}", client_nonce);

        let (exchange, server_first) = ScramExchange::start(
            mechanism,
            format!("n,,{}", client_first_bare).as_bytes(),
            |_| Some(credential.clone()),
        )
        .unwrap();
        let server_first = String::from_utf8(server_first).unwrap();

        // Reproduce the buggy client exactly: r = cnonce + <server-first r>.
        let combined = attribute(&server_first, 'r').unwrap();
        let doubled = format!("{}{}", client_nonce, combined);

        let salt = b64().decode(attribute(&server_first, 's').unwrap()).unwrap();
        let iterations: u32 = attribute(&server_first, 'i').unwrap().parse().unwrap();
        let salted = hi(b"hunter2", &salt, iterations, mechanism).unwrap();
        let client_key = hmac(&salted, CLIENT_KEY_LABEL, mechanism).unwrap();
        let stored_key = hash(&client_key, mechanism);

        let without_proof = format!("c=biws,r={}", doubled);
        let auth_message = format!(
            "{},{},{}",
            client_first_bare, server_first, without_proof
        );
        let client_signature = hmac(&stored_key, auth_message.as_bytes(), mechanism).unwrap();
        let proof: Vec<u8> = client_key
            .iter()
            .zip(client_signature.iter())
            .map(|(k, s)| k ^ s)
            .collect();
        let final_msg = format!("{},p={}", without_proof, b64().encode(proof));

        assert!(
            exchange.finish(final_msg.as_bytes()).is_ok(),
            "a client with the librdkafka #4895 nonce bug must still authenticate"
        );
    }

    /// The relaxation must not become "any nonce goes": a nonce that does not
    /// end with the one we issued is still a replay and must fail.
    #[test]
    fn unrelated_nonce_is_still_rejected() {
        let mechanism = SaslMechanism::ScramSha256;
        let credential =
            ScramCredential::derive("hunter2", mechanism, DEFAULT_ITERATIONS).unwrap();
        let (exchange, _server_first) = ScramExchange::start(
            mechanism,
            b"n,,n=alice,r=clientnonce123",
            |_| Some(credential.clone()),
        )
        .unwrap();

        let forged = format!(
            "c=biws,r=totally-different-nonce,p={}",
            b64().encode([0u8; 32])
        );
        assert!(matches!(
            exchange.finish(forged.as_bytes()),
            Err(SaslError::ProtocolError(_))
        ));
    }
}

#[cfg(test)]
mod salted_password_tests {
    use super::*;

    /// A credential built from a client-supplied salted password must accept the
    /// password that produced it. This is the whole AlterUserScramCredentials
    /// path: the client computes Hi(password, salt, i) and sends only that.
    #[test]
    fn a_credential_from_a_salted_password_authenticates() {
        let mechanism = SaslMechanism::ScramSha256;
        let salt = vec![9u8; 16];
        let iterations = DEFAULT_ITERATIONS;

        // What kafka-configs.sh does client-side.
        let salted = hi(b"hunter2", &salt, iterations, mechanism).unwrap();
        let credential =
            ScramCredential::from_salted_password(salt.clone(), &salted, mechanism, iterations)
                .unwrap();

        // It must be identical to deriving from the password directly.
        let direct =
            ScramCredential::derive_with_salt("hunter2", mechanism, iterations, salt).unwrap();
        assert_eq!(credential.stored_key, direct.stored_key);
        assert_eq!(credential.server_key, direct.server_key);
    }

    /// A salted password of the wrong length is refused rather than stored,
    /// which would create a user nobody can ever authenticate as.
    #[test]
    fn a_wrong_length_salted_password_is_refused() {
        let result = ScramCredential::from_salted_password(
            vec![1u8; 16],
            &[0u8; 16], // SHA-256 needs 32
            SaslMechanism::ScramSha256,
            DEFAULT_ITERATIONS,
        );
        assert!(result.is_err());
    }

    #[test]
    fn iterations_below_the_floor_are_refused() {
        let result = ScramCredential::from_salted_password(
            vec![1u8; 16],
            &[0u8; 32],
            SaslMechanism::ScramSha256,
            1000,
        );
        assert!(result.is_err());
    }
}
