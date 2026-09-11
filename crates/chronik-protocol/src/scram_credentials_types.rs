//! DescribeUserScramCredentials (API 50) and AlterUserScramCredentials (API 51).
//!
//! These are how `kafka-configs.sh` manages SCRAM users:
//!
//! ```text
//! kafka-configs.sh --bootstrap-server ... --alter --entity-type users \
//!   --entity-name alice --add-config 'SCRAM-SHA-256=[iterations=8192,password=secret]'
//! ```
//!
//! # The password never crosses the wire
//!
//! `AlterUserScramCredentials` carries a **salt** and a **salted password** —
//! the client runs `Hi(password, salt, iterations)` itself. The broker stores
//! only the two keys derived from that, so it never sees and cannot recover the
//! password. That also means the broker cannot validate the credential against
//! anything: it stores what it is given.
//!
//! Both APIs are flexible at every version (there is no v0 non-compact form), so
//! the encoding here is compact strings/bytes and tagged fields throughout.

use bytes::{BufMut, BytesMut};
use chronik_common::{Error, Result};

use crate::parser::Decoder;

/// Kafka `ScramMechanism` codes.
pub const MECHANISM_UNKNOWN: i8 = 0;
pub const MECHANISM_SCRAM_SHA_256: i8 = 1;
pub const MECHANISM_SCRAM_SHA_512: i8 = 2;

/// One user named in a DescribeUserScramCredentials request.
#[derive(Debug, Clone)]
pub struct UserName {
    pub name: String,
}

/// DescribeUserScramCredentials request.
///
/// `users == None` means "describe every user", which is what
/// `kafka-configs.sh --describe --entity-type users` sends with no name.
#[derive(Debug, Clone)]
pub struct DescribeUserScramCredentialsRequest {
    pub users: Option<Vec<UserName>>,
}

/// One credential in a describe response: mechanism plus iteration count.
///
/// Deliberately no key material — describing a user must not hand out anything
/// that helps authenticate as them.
#[derive(Debug, Clone)]
pub struct CredentialInfo {
    pub mechanism: i8,
    pub iterations: i32,
}

#[derive(Debug, Clone)]
pub struct DescribeUserScramCredentialsResult {
    pub user: String,
    pub error_code: i16,
    pub error_message: Option<String>,
    pub credential_infos: Vec<CredentialInfo>,
}

#[derive(Debug, Clone)]
pub struct DescribeUserScramCredentialsResponse {
    pub throttle_time_ms: i32,
    pub error_code: i16,
    pub error_message: Option<String>,
    pub results: Vec<DescribeUserScramCredentialsResult>,
}

/// A credential to remove.
#[derive(Debug, Clone)]
pub struct ScramCredentialDeletion {
    pub name: String,
    pub mechanism: i8,
}

/// A credential to create or replace.
#[derive(Debug, Clone)]
pub struct ScramCredentialUpsertion {
    pub name: String,
    pub mechanism: i8,
    pub iterations: i32,
    pub salt: Vec<u8>,
    pub salted_password: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AlterUserScramCredentialsRequest {
    pub deletions: Vec<ScramCredentialDeletion>,
    pub upsertions: Vec<ScramCredentialUpsertion>,
}

#[derive(Debug, Clone)]
pub struct AlterUserScramCredentialsResult {
    pub user: String,
    pub error_code: i16,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AlterUserScramCredentialsResponse {
    pub throttle_time_ms: i32,
    pub results: Vec<AlterUserScramCredentialsResult>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a DescribeUserScramCredentials request.
pub fn parse_describe_user_scram_credentials_request(
    decoder: &mut Decoder,
) -> Result<DescribeUserScramCredentialsRequest> {
    // Compact nullable array: 0 = null ("all users"), n = n-1 entries.
    let count = decoder.read_unsigned_varint()? as i64;
    let users = if count == 0 {
        None
    } else {
        let n = (count - 1) as usize;
        let mut users = Vec::with_capacity(n);
        for _ in 0..n {
            let name = decoder
                .read_compact_string()?
                .ok_or_else(|| Error::Protocol("user name cannot be null".into()))?;
            decoder.skip_tagged_fields()?;
            users.push(UserName { name });
        }
        Some(users)
    };
    decoder.skip_tagged_fields()?;
    Ok(DescribeUserScramCredentialsRequest { users })
}

/// Parse an AlterUserScramCredentials request.
pub fn parse_alter_user_scram_credentials_request(
    decoder: &mut Decoder,
) -> Result<AlterUserScramCredentialsRequest> {
    let deletion_count = decoder.read_unsigned_varint()?;
    let mut deletions = Vec::new();
    if deletion_count > 0 {
        for _ in 0..(deletion_count - 1) {
            let name = decoder
                .read_compact_string()?
                .ok_or_else(|| Error::Protocol("deletion name cannot be null".into()))?;
            let mechanism = decoder.read_i8()?;
            decoder.skip_tagged_fields()?;
            deletions.push(ScramCredentialDeletion { name, mechanism });
        }
    }

    let upsertion_count = decoder.read_unsigned_varint()?;
    let mut upsertions = Vec::new();
    if upsertion_count > 0 {
        for _ in 0..(upsertion_count - 1) {
            let name = decoder
                .read_compact_string()?
                .ok_or_else(|| Error::Protocol("upsertion name cannot be null".into()))?;
            let mechanism = decoder.read_i8()?;
            let iterations = decoder.read_i32()?;
            let salt = decoder
                .read_compact_bytes()?
                .ok_or_else(|| Error::Protocol("salt cannot be null".into()))?
                .to_vec();
            let salted_password = decoder
                .read_compact_bytes()?
                .ok_or_else(|| Error::Protocol("salted password cannot be null".into()))?
                .to_vec();
            decoder.skip_tagged_fields()?;
            upsertions.push(ScramCredentialUpsertion {
                name,
                mechanism,
                iterations,
                salt,
                salted_password,
            });
        }
    }

    decoder.skip_tagged_fields()?;
    Ok(AlterUserScramCredentialsRequest {
        deletions,
        upsertions,
    })
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn put_uvarint(buf: &mut BytesMut, mut value: u32) {
    loop {
        if value < 0x80 {
            buf.put_u8(value as u8);
            return;
        }
        buf.put_u8(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
}

fn put_compact_string(buf: &mut BytesMut, value: Option<&str>) {
    match value {
        Some(s) => {
            put_uvarint(buf, s.len() as u32 + 1);
            buf.put_slice(s.as_bytes());
        }
        None => put_uvarint(buf, 0),
    }
}

pub fn encode_describe_user_scram_credentials_response(
    response: &DescribeUserScramCredentialsResponse,
) -> BytesMut {
    let mut buf = BytesMut::new();
    buf.put_i32(response.throttle_time_ms);
    buf.put_i16(response.error_code);
    put_compact_string(&mut buf, response.error_message.as_deref());

    put_uvarint(&mut buf, response.results.len() as u32 + 1);
    for result in &response.results {
        put_compact_string(&mut buf, Some(&result.user));
        buf.put_i16(result.error_code);
        put_compact_string(&mut buf, result.error_message.as_deref());

        put_uvarint(&mut buf, result.credential_infos.len() as u32 + 1);
        for info in &result.credential_infos {
            buf.put_i8(info.mechanism);
            buf.put_i32(info.iterations);
            put_uvarint(&mut buf, 0); // tagged fields
        }
        put_uvarint(&mut buf, 0); // tagged fields
    }
    put_uvarint(&mut buf, 0); // tagged fields
    buf
}

pub fn encode_alter_user_scram_credentials_response(
    response: &AlterUserScramCredentialsResponse,
) -> BytesMut {
    let mut buf = BytesMut::new();
    buf.put_i32(response.throttle_time_ms);

    put_uvarint(&mut buf, response.results.len() as u32 + 1);
    for result in &response.results {
        put_compact_string(&mut buf, Some(&result.user));
        buf.put_i16(result.error_code);
        put_compact_string(&mut buf, result.error_message.as_deref());
        put_uvarint(&mut buf, 0); // tagged fields
    }
    put_uvarint(&mut buf, 0); // tagged fields
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// Build the request bytes `kafka-configs.sh --describe --entity-type users`
    /// sends with no entity name: a null users array.
    #[test]
    fn parses_describe_all_users() {
        let mut raw = BytesMut::new();
        put_uvarint(&mut raw, 0); // null array
        put_uvarint(&mut raw, 0); // tagged fields
        let mut bytes = Bytes::from(raw.to_vec());
        let mut decoder = Decoder::new(&mut bytes);

        let request = parse_describe_user_scram_credentials_request(&mut decoder).unwrap();
        assert!(request.users.is_none(), "null array means all users");
    }

    #[test]
    fn parses_describe_named_users() {
        let mut raw = BytesMut::new();
        put_uvarint(&mut raw, 3); // 2 entries
        for name in ["alice", "bob"] {
            put_compact_string(&mut raw, Some(name));
            put_uvarint(&mut raw, 0);
        }
        put_uvarint(&mut raw, 0);
        let mut bytes = Bytes::from(raw.to_vec());
        let mut decoder = Decoder::new(&mut bytes);

        let request = parse_describe_user_scram_credentials_request(&mut decoder).unwrap();
        let users = request.users.expect("named users");
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].name, "alice");
        assert_eq!(users[1].name, "bob");
    }

    #[test]
    fn parses_an_upsertion() {
        let mut raw = BytesMut::new();
        put_uvarint(&mut raw, 1); // 0 deletions
        put_uvarint(&mut raw, 2); // 1 upsertion
        put_compact_string(&mut raw, Some("alice"));
        raw.put_i8(MECHANISM_SCRAM_SHA_256);
        raw.put_i32(8192);
        put_uvarint(&mut raw, 5); // salt: 4 bytes
        raw.put_slice(&[1, 2, 3, 4]);
        put_uvarint(&mut raw, 33); // salted password: 32 bytes
        raw.put_slice(&[7u8; 32]);
        put_uvarint(&mut raw, 0); // upsertion tagged fields
        put_uvarint(&mut raw, 0); // request tagged fields

        let mut bytes = Bytes::from(raw.to_vec());
        let mut decoder = Decoder::new(&mut bytes);
        let request = parse_alter_user_scram_credentials_request(&mut decoder).unwrap();

        assert!(request.deletions.is_empty());
        assert_eq!(request.upsertions.len(), 1);
        let up = &request.upsertions[0];
        assert_eq!(up.name, "alice");
        assert_eq!(up.mechanism, MECHANISM_SCRAM_SHA_256);
        assert_eq!(up.iterations, 8192);
        assert_eq!(up.salt, vec![1, 2, 3, 4]);
        assert_eq!(up.salted_password.len(), 32);
    }

    #[test]
    fn parses_a_deletion() {
        let mut raw = BytesMut::new();
        put_uvarint(&mut raw, 2); // 1 deletion
        put_compact_string(&mut raw, Some("bob"));
        raw.put_i8(MECHANISM_SCRAM_SHA_512);
        put_uvarint(&mut raw, 0);
        put_uvarint(&mut raw, 1); // 0 upsertions
        put_uvarint(&mut raw, 0);

        let mut bytes = Bytes::from(raw.to_vec());
        let mut decoder = Decoder::new(&mut bytes);
        let request = parse_alter_user_scram_credentials_request(&mut decoder).unwrap();

        assert_eq!(request.deletions.len(), 1);
        assert_eq!(request.deletions[0].name, "bob");
        assert_eq!(request.deletions[0].mechanism, MECHANISM_SCRAM_SHA_512);
        assert!(request.upsertions.is_empty());
    }

    /// A describe response must not leak key material — only mechanism and
    /// iteration count, as Kafka's schema allows.
    #[test]
    fn describe_response_carries_no_key_material() {
        let response = DescribeUserScramCredentialsResponse {
            throttle_time_ms: 0,
            error_code: 0,
            error_message: None,
            results: vec![DescribeUserScramCredentialsResult {
                user: "alice".to_string(),
                error_code: 0,
                error_message: None,
                credential_infos: vec![CredentialInfo {
                    mechanism: MECHANISM_SCRAM_SHA_256,
                    iterations: 8192,
                }],
            }],
        };

        let encoded = encode_describe_user_scram_credentials_response(&response);
        // 4 (throttle) + 2 (error) + 1 (null msg) + 1 (array len) + 6 (user)
        // + 2 + 1 + 1 + 5 (info) + 1 + 1 = small and fixed. The point is that
        // no 32/64-byte key blob can fit.
        assert!(
            encoded.len() < 40,
            "describe response is {} bytes - large enough to be carrying key \
             material, which it must never do",
            encoded.len()
        );
    }

    #[test]
    fn alter_response_round_trips_an_error() {
        let response = AlterUserScramCredentialsResponse {
            throttle_time_ms: 0,
            results: vec![AlterUserScramCredentialsResult {
                user: "alice".to_string(),
                error_code: 37,
                error_message: Some("unsupported mechanism".to_string()),
            }],
        };
        let encoded = encode_alter_user_scram_credentials_response(&response);
        assert!(!encoded.is_empty());
        // The error message must actually be present on the wire.
        assert!(
            String::from_utf8_lossy(&encoded).contains("unsupported mechanism"),
            "error message was dropped from the response"
        );
    }
}
