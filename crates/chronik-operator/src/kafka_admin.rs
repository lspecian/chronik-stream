//! Topic, SCRAM-user and ACL management over the Kafka protocol.
//!
//! The broker implements these as Kafka admin APIs on its Kafka port, the same
//! calls `kafka-topics.sh`, `kafka-configs.sh` and `kafka-acls.sh` make. (The
//! operator once called HTTP endpoints for them that the broker never had, so
//! ChronikTopic and ChronikUser never reconciled.)
//!
//! One connection per reconcile: negotiate versions with ApiVersions, log in
//! with SASL/SCRAM-SHA-512 when the cluster requires it, then issue requests
//! at the highest version both sides support. Any node accepts catalog writes
//! and forwards them to the catalog leader, so any broker that answers will do.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use chronik_scram::scram::{salted_password, ScramClient, DEFAULT_ITERATIONS};
use chronik_scram::SaslMechanism;
use kafka_protocol::messages::alter_user_scram_credentials_request::{
    ScramCredentialDeletion, ScramCredentialUpsertion,
};
use kafka_protocol::messages::create_acls_request::AclCreation;
use kafka_protocol::messages::create_partitions_request::CreatePartitionsTopic;
use kafka_protocol::messages::create_topics_request::{CreatableTopic, CreatableTopicConfig};
use kafka_protocol::messages::delete_acls_request::DeleteAclsFilter;
use kafka_protocol::messages::delete_topics_request::DeleteTopicState;
use kafka_protocol::messages::describe_configs_request::DescribeConfigsResource;
use kafka_protocol::messages::incremental_alter_configs_request::{
    AlterConfigsResource, AlterableConfig,
};
use kafka_protocol::messages::metadata_request::MetadataRequestTopic;
use kafka_protocol::messages::{
    AlterUserScramCredentialsRequest, ApiVersionsRequest, CreateAclsRequest,
    CreatePartitionsRequest, CreateTopicsRequest, DeleteAclsRequest, DeleteTopicsRequest,
    DescribeAclsRequest, DescribeConfigsRequest, IncrementalAlterConfigsRequest, MetadataRequest,
    RequestHeader, ResponseHeader, SaslAuthenticateRequest, SaslHandshakeRequest, TopicName,
};
use kafka_protocol::protocol::{
    Decodable, Encodable, HeaderVersion, Request, StrBytes, VersionRange,
};
use kafka_protocol::ResponseError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use crate::error::OperatorError;

const CLIENT_ID: &str = "chronik-operator";
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT_MS: i32 = 30_000;
/// Responses larger than this are refused rather than allocated.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

// Kafka wire codes.
const RESOURCE_TOPIC: i8 = 2;
const ACL_ANY: i8 = 1;
const CONFIG_SET: i8 = 0;
const SCRAM_SHA_256: i8 = 1;
const SCRAM_SHA_512: i8 = 2;

/// SASL login for the operator's connection.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Where and how to reach a cluster's Kafka port.
#[derive(Clone, Debug)]
pub struct KafkaAccess {
    /// Brokers to try, in order.
    pub bootstrap: Vec<String>,
    pub credentials: Option<Credentials>,
    /// TLS, when the Kafka port uses it.
    pub tls: Option<KafkaTls>,
}

/// TLS for the operator's connection: the CA that signed the brokers'
/// certificates, and the certificate and key the operator presents (the
/// brokers' own, which a broker requiring client certificates accepts).
#[derive(Clone)]
pub struct KafkaTls {
    pub ca_pem: Vec<u8>,
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

impl std::fmt::Debug for KafkaTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaTls").finish_non_exhaustive()
    }
}

impl KafkaTls {
    fn connector(&self) -> Result<tokio_rustls::TlsConnector, OperatorError> {
        let bad = |what: &str, e: &dyn std::fmt::Display| {
            OperatorError::Config(format!("TLS {what}: {e}"))
        };
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut self.ca_pem.as_slice()) {
            let cert = cert.map_err(|e| bad("ca.crt", &e))?;
            roots.add(cert).map_err(|e| bad("ca.crt", &e))?;
        }
        if roots.is_empty() {
            return Err(bad("ca.crt", &"no certificate"));
        }
        let certs = rustls_pemfile::certs(&mut self.cert_pem.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| bad("tls.crt", &e))?;
        let key = rustls_pemfile::private_key(&mut self.key_pem.as_slice())
            .map_err(|e| bad("tls.key", &e))?
            .ok_or_else(|| bad("tls.key", &"no private key"))?;
        // An explicit provider: the process may link more than one.
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("protocol versions", &e))?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| bad("client certificate", &e))?;
        Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
    }
}

/// The connection: plain TCP, or TLS over it.
trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// A topic as the broker reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct TopicDescription {
    pub partitions: i32,
    pub replication_factor: i32,
    pub config: BTreeMap<String, String>,
    pub topic_id: Option<String>,
}

/// One ACL binding for a user, in the CRD's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AclEntry {
    pub resource_type: String,
    pub resource_name: String,
    pub pattern_type: String,
    pub operation: String,
    pub effect: String,
}

/// SCRAM mechanism of a ChronikUser's `authentication.type`.
pub fn scram_mechanism(auth_type: &str) -> Result<Option<SaslMechanism>, OperatorError> {
    match auth_type.trim().to_ascii_lowercase().as_str() {
        "sasl-scram-sha-256" | "scram-sha-256" => Ok(Some(SaslMechanism::ScramSha256)),
        "sasl-scram-sha-512" | "scram-sha-512" => Ok(Some(SaslMechanism::ScramSha512)),
        // A TLS principal has no password to store; only its ACLs apply.
        "tls" => Ok(None),
        "sasl-plain" | "plain" => Err(OperatorError::Config(
            "authentication type sasl-plain cannot be managed: the broker accepts PLAIN only for \
             users configured in CHRONIK_SASL_USERS, and Kafka has no API to create them. Use \
             sasl-scram-sha-512 (or -256); clients log in with the same username and password."
                .into(),
        )),
        other => Err(OperatorError::Config(format!(
            "unknown authentication type '{other}' (expected sasl-scram-sha-512, sasl-scram-sha-256 or tls)"
        ))),
    }
}

pub struct KafkaAdmin {
    stream: Box<dyn Io>,
    correlation_id: i32,
    versions: HashMap<i16, VersionRange>,
}

fn protocol_err(context: &str, e: impl std::fmt::Display) -> OperatorError {
    OperatorError::AdminApi(format!("{context}: {e}"))
}

/// `Ok` for error code 0 (or any code in `ok_codes`), else the error by name.
fn check(
    context: &str,
    code: i16,
    message: Option<&StrBytes>,
    ok_codes: &[i16],
) -> Result<(), OperatorError> {
    if code == 0 || ok_codes.contains(&code) {
        return Ok(());
    }
    let name = ResponseError::try_from_code(code)
        .map(|e| format!("{e:?}"))
        .unwrap_or_else(|| format!("error code {code}"));
    Err(OperatorError::AdminApi(
        match message.map(|m| m.as_str()).filter(|m| !m.is_empty()) {
            Some(m) => format!("{context}: {name}: {m}"),
            None => format!("{context}: {name}"),
        },
    ))
}

fn code_of(e: ResponseError) -> i16 {
    e.code()
}

fn s(v: &str) -> StrBytes {
    StrBytes::from_string(v.to_string())
}

impl KafkaAdmin {
    /// Connect to the first broker that answers, negotiate versions, and log
    /// in when `credentials` are given.
    pub async fn connect(access: &KafkaAccess) -> Result<Self, OperatorError> {
        let mut failures = Vec::new();
        for addr in &access.bootstrap {
            match Self::connect_one(addr, access).await {
                Ok(admin) => return Ok(admin),
                Err(e) => failures.push(e.to_string()),
            }
        }
        Err(OperatorError::AdminApi(if failures.is_empty() {
            "no broker address to connect to".into()
        } else {
            failures.join("; ")
        }))
    }

    async fn connect_one(addr: &str, access: &KafkaAccess) -> Result<Self, OperatorError> {
        let stream = tokio::time::timeout(IO_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| protocol_err(&format!("connect {addr}"), "timed out"))?
            .map_err(|e| protocol_err(&format!("connect {addr}"), e))?;
        let _ = stream.set_nodelay(true);
        let stream: Box<dyn Io> = match &access.tls {
            None => Box::new(stream),
            Some(tls) => {
                let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
                let name = rustls::pki_types::ServerName::try_from(host.to_string())
                    .map_err(|e| protocol_err(&format!("TLS name of {addr}"), e))?;
                let tls_stream =
                    tokio::time::timeout(IO_TIMEOUT, tls.connector()?.connect(name, stream))
                        .await
                        .map_err(|_| {
                            protocol_err(&format!("TLS handshake with {addr}"), "timed out")
                        })?
                        .map_err(|e| protocol_err(&format!("TLS handshake with {addr}"), e))?;
                Box::new(tls_stream)
            }
        };
        let mut admin = Self {
            stream,
            correlation_id: 0,
            versions: HashMap::new(),
        };

        // v0 is understood by every broker; it lists the supported ranges.
        let versions = admin.send_at(ApiVersionsRequest::default(), 0).await?;
        check("ApiVersions", versions.error_code, None, &[])?;
        admin.versions = versions
            .api_keys
            .iter()
            .map(|k| {
                (
                    k.api_key,
                    VersionRange {
                        min: k.min_version,
                        max: k.max_version,
                    },
                )
            })
            .collect();

        if let Some(creds) = &access.credentials {
            admin.authenticate(creds).await?;
        }
        Ok(admin)
    }

    fn version_of<R: Request>(&self) -> Result<i16, OperatorError> {
        let broker = self.versions.get(&R::KEY).ok_or_else(|| {
            OperatorError::AdminApi(format!("broker does not support API key {}", R::KEY))
        })?;
        let common = broker.intersect(&R::VERSIONS);
        if common.is_empty() {
            return Err(OperatorError::AdminApi(format!(
                "no common version for API key {} (broker {broker}, operator {})",
                R::KEY,
                R::VERSIONS
            )));
        }
        Ok(common.max)
    }

    async fn send<R: Request>(&mut self, request: R) -> Result<R::Response, OperatorError> {
        let version = self.version_of::<R>()?;
        self.send_at(request, version).await
    }

    async fn send_at<R: Request>(
        &mut self,
        request: R,
        version: i16,
    ) -> Result<R::Response, OperatorError> {
        self.correlation_id = self.correlation_id.wrapping_add(1);
        let header = RequestHeader::default()
            .with_request_api_key(R::KEY)
            .with_request_api_version(version)
            .with_correlation_id(self.correlation_id)
            .with_client_id(Some(s(CLIENT_ID)));
        let mut frame = BytesMut::new();
        frame.put_i32(0); // length, filled below
        header
            .encode(&mut frame, R::header_version(version))
            .map_err(|e| protocol_err("encode request header", e))?;
        request
            .encode(&mut frame, version)
            .map_err(|e| protocol_err(&format!("encode API {} v{version}", R::KEY), e))?;
        let len = (frame.len() - 4) as i32;
        frame[..4].copy_from_slice(&len.to_be_bytes());

        let io = async {
            self.stream.write_all(&frame).await?;
            let mut len_buf = [0u8; 4];
            self.stream.read_exact(&mut len_buf).await?;
            let len = i32::from_be_bytes(len_buf);
            if len < 0 || len as usize > MAX_RESPONSE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("response length {len}"),
                ));
            }
            let mut body = vec![0u8; len as usize];
            self.stream.read_exact(&mut body).await?;
            Ok(Bytes::from(body))
        };
        let mut body = tokio::time::timeout(IO_TIMEOUT, io)
            .await
            .map_err(|_| protocol_err(&format!("API {}", R::KEY), "timed out"))?
            .map_err(|e| protocol_err(&format!("API {} (connection)", R::KEY), e))?;

        let response_header =
            ResponseHeader::decode(&mut body, R::Response::header_version(version))
                .map_err(|e| protocol_err("decode response header", e))?;
        if response_header.correlation_id != self.correlation_id {
            return Err(OperatorError::AdminApi(format!(
                "correlation id {} answered {}",
                self.correlation_id, response_header.correlation_id
            )));
        }
        let response = R::Response::decode(&mut body, version)
            .map_err(|e| protocol_err(&format!("decode API {} v{version}", R::KEY), e))?;
        if body.has_remaining() {
            debug!(
                api = R::KEY,
                version,
                left = body.remaining(),
                "response had trailing bytes"
            );
        }
        Ok(response)
    }

    async fn authenticate(&mut self, creds: &Credentials) -> Result<(), OperatorError> {
        let mechanism = SaslMechanism::ScramSha512;
        let handshake = self
            .send(SaslHandshakeRequest::default().with_mechanism(s(mechanism.as_str())))
            .await?;
        check("SaslHandshake", handshake.error_code, None, &[])?;

        let mut scram = ScramClient::new(mechanism, &creds.username, &creds.password);
        let first = self
            .send(
                SaslAuthenticateRequest::default()
                    .with_auth_bytes(Bytes::from(scram.client_first())),
            )
            .await?;
        check(
            "SASL login",
            first.error_code,
            first.error_message.as_ref(),
            &[],
        )?;
        let client_final = scram
            .client_final(&first.auth_bytes)
            .map_err(|e| protocol_err("SASL login", format!("{e:?}")))?;
        let last = self
            .send(SaslAuthenticateRequest::default().with_auth_bytes(Bytes::from(client_final)))
            .await?;
        check(
            "SASL login",
            last.error_code,
            last.error_message.as_ref(),
            &[],
        )?;
        scram
            .verify_server_final(&last.auth_bytes)
            .map_err(|e| protocol_err("SASL login: server proof", format!("{e:?}")))
    }

    /// The topic, or `None` when the broker does not know it. Never creates it.
    pub async fn describe_topic(
        &mut self,
        name: &str,
    ) -> Result<Option<TopicDescription>, OperatorError> {
        let metadata = self
            .send(
                MetadataRequest::default()
                    .with_topics(Some(vec![
                        MetadataRequestTopic::default().with_name(Some(TopicName(s(name))))
                    ]))
                    .with_allow_auto_topic_creation(false),
            )
            .await?;
        let Some(topic) = metadata.topics.into_iter().next() else {
            return Ok(None);
        };
        if topic.error_code == code_of(ResponseError::UnknownTopicOrPartition) {
            return Ok(None);
        }
        check(
            &format!("describe topic {name}"),
            topic.error_code,
            None,
            &[],
        )?;
        let replication_factor = topic
            .partitions
            .iter()
            .map(|p| p.replica_nodes.len() as i32)
            .max()
            .unwrap_or(0);
        let topic_id = (!topic.topic_id.is_nil()).then(|| topic.topic_id.to_string());

        let configs = self
            .send(DescribeConfigsRequest::default().with_resources(vec![
                DescribeConfigsResource::default()
                    .with_resource_type(RESOURCE_TOPIC)
                    .with_resource_name(s(name))
                    .with_configuration_keys(None),
            ]))
            .await?;
        let mut config = BTreeMap::new();
        for result in configs.results {
            check(
                &format!("describe configs of {name}"),
                result.error_code,
                result.error_message.as_ref(),
                &[],
            )?;
            for entry in result.configs {
                if let Some(value) = entry.value {
                    config.insert(entry.name.to_string(), value.to_string());
                }
            }
        }
        Ok(Some(TopicDescription {
            partitions: topic.partitions.len() as i32,
            replication_factor,
            config,
            topic_id,
        }))
    }

    /// Create a topic. An existing topic of that name is not an error.
    pub async fn create_topic(
        &mut self,
        name: &str,
        partitions: i32,
        replication_factor: i32,
        config: &BTreeMap<String, String>,
    ) -> Result<Option<String>, OperatorError> {
        let topic = CreatableTopic::default()
            .with_name(TopicName(s(name)))
            .with_num_partitions(partitions)
            .with_replication_factor(replication_factor as i16)
            .with_configs(
                config
                    .iter()
                    .map(|(k, v)| {
                        CreatableTopicConfig::default()
                            .with_name(s(k))
                            .with_value(Some(s(v)))
                    })
                    .collect(),
            );
        let response = self
            .send(
                CreateTopicsRequest::default()
                    .with_topics(vec![topic])
                    .with_timeout_ms(REQUEST_TIMEOUT_MS),
            )
            .await?;
        let result = response.topics.into_iter().next().ok_or_else(|| {
            OperatorError::AdminApi(format!("create topic {name}: empty response"))
        })?;
        check(
            &format!("create topic {name}"),
            result.error_code,
            result.error_message.as_ref(),
            &[code_of(ResponseError::TopicAlreadyExists)],
        )?;
        Ok((!result.topic_id.is_nil()).then(|| result.topic_id.to_string()))
    }

    /// Delete a topic. A topic that does not exist is not an error.
    pub async fn delete_topic(&mut self, name: &str) -> Result<(), OperatorError> {
        let version = self.version_of::<DeleteTopicsRequest>()?;
        let request = DeleteTopicsRequest::default().with_timeout_ms(REQUEST_TIMEOUT_MS);
        // v6 names topics in `topics`; earlier versions in `topic_names`.
        let request = if version >= 6 {
            request.with_topics(vec![
                DeleteTopicState::default().with_name(Some(TopicName(s(name))))
            ])
        } else {
            request.with_topic_names(vec![TopicName(s(name))])
        };
        let response = self.send_at(request, version).await?;
        for result in response.responses {
            check(
                &format!("delete topic {name}"),
                result.error_code,
                result.error_message.as_ref(),
                &[code_of(ResponseError::UnknownTopicOrPartition)],
            )?;
        }
        Ok(())
    }

    /// Set these topic configs (others are left as they are).
    pub async fn update_topic_config(
        &mut self,
        name: &str,
        config: &BTreeMap<String, String>,
    ) -> Result<(), OperatorError> {
        let resource = AlterConfigsResource::default()
            .with_resource_type(RESOURCE_TOPIC)
            .with_resource_name(s(name))
            .with_configs(
                config
                    .iter()
                    .map(|(k, v)| {
                        AlterableConfig::default()
                            .with_name(s(k))
                            .with_config_operation(CONFIG_SET)
                            .with_value(Some(s(v)))
                    })
                    .collect(),
            );
        let response = self
            .send(IncrementalAlterConfigsRequest::default().with_resources(vec![resource]))
            .await?;
        for result in response.responses {
            check(
                &format!("alter configs of {name}"),
                result.error_code,
                result.error_message.as_ref(),
                &[],
            )?;
        }
        Ok(())
    }

    /// Raise the partition count to `count`.
    pub async fn increase_partitions(
        &mut self,
        name: &str,
        count: i32,
    ) -> Result<(), OperatorError> {
        let response = self
            .send(
                CreatePartitionsRequest::default()
                    .with_topics(vec![CreatePartitionsTopic::default()
                        .with_name(TopicName(s(name)))
                        .with_count(count)
                        .with_assignments(None)])
                    .with_timeout_ms(REQUEST_TIMEOUT_MS),
            )
            .await?;
        for result in response.results {
            check(
                &format!("add partitions to {name}"),
                result.error_code,
                result.error_message.as_ref(),
                &[],
            )?;
        }
        Ok(())
    }

    /// Create or replace a user's SCRAM credential for `mechanism`.
    pub async fn upsert_scram_user(
        &mut self,
        username: &str,
        password: &str,
        mechanism: SaslMechanism,
    ) -> Result<(), OperatorError> {
        let mut salt = vec![0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut salt);
        let salted = salted_password(password, &salt, DEFAULT_ITERATIONS, mechanism)
            .map_err(|e| protocol_err("derive SCRAM credential", format!("{e:?}")))?;
        let upsertion = ScramCredentialUpsertion::default()
            .with_name(s(username))
            .with_mechanism(wire_mechanism(mechanism))
            .with_iterations(DEFAULT_ITERATIONS as i32)
            .with_salt(Bytes::from(salt))
            .with_salted_password(Bytes::from(salted));
        let response = self
            .send(AlterUserScramCredentialsRequest::default().with_upsertions(vec![upsertion]))
            .await?;
        for result in response.results {
            check(
                &format!("set credential of {username}"),
                result.error_code,
                result.error_message.as_ref(),
                &[],
            )?;
        }
        Ok(())
    }

    /// Remove every SCRAM credential of a user. A user without one is not an error.
    pub async fn delete_scram_user(&mut self, username: &str) -> Result<(), OperatorError> {
        let deletions = [SCRAM_SHA_256, SCRAM_SHA_512]
            .into_iter()
            .map(|m| {
                ScramCredentialDeletion::default()
                    .with_name(s(username))
                    .with_mechanism(m)
            })
            .collect();
        let response = self
            .send(AlterUserScramCredentialsRequest::default().with_deletions(deletions))
            .await?;
        for result in response.results {
            check(
                &format!("delete credential of {username}"),
                result.error_code,
                result.error_message.as_ref(),
                &[code_of(ResponseError::ResourceNotFound)],
            )?;
        }
        Ok(())
    }

    /// The ACL bindings `User:<username>` holds.
    /// `None` when the cluster has ACLs disabled.
    pub async fn user_acls(
        &mut self,
        username: &str,
    ) -> Result<Option<Vec<AclEntry>>, OperatorError> {
        let principal = format!("User:{username}");
        Ok(self
            .wire_acls(&principal)
            .await?
            .map(|set| set.iter().map(WireAcl::to_entry).collect()))
    }

    /// Make `User:<username>`'s ACLs exactly `desired`: create the missing
    /// bindings, delete the ones no longer wanted. Returns how many it holds.
    pub async fn set_user_acls(
        &mut self,
        username: &str,
        desired: &[AclEntry],
    ) -> Result<usize, OperatorError> {
        let principal = format!("User:{username}");
        let want: BTreeSet<WireAcl> = desired
            .iter()
            .map(WireAcl::from_entry)
            .collect::<Result<_, _>>()?;
        let Some(have) = self.wire_acls(&principal).await? else {
            // ACLs are off: nothing to hold, and asking for some is a
            // configuration error the user must see, not a silent no-op.
            if want.is_empty() {
                return Ok(0);
            }
            return Err(OperatorError::Config(format!(
                "the cluster has ACLs disabled (CHRONIK_ACL_ENABLED is not true); the {} listed ACL(s) for {principal} are not applied",
                want.len()
            )));
        };
        self.apply_acl_diff(&principal, &want, &have).await?;
        Ok(want.len())
    }

    /// `None` when the broker answers SECURITY_DISABLED (ACLs off).
    async fn wire_acls(
        &mut self,
        principal: &str,
    ) -> Result<Option<BTreeSet<WireAcl>>, OperatorError> {
        let described = self
            .send(
                DescribeAclsRequest::default()
                    .with_resource_type_filter(ACL_ANY)
                    .with_resource_name_filter(None)
                    .with_pattern_type_filter(ACL_ANY)
                    .with_principal_filter(Some(s(principal)))
                    .with_host_filter(None)
                    .with_operation(ACL_ANY)
                    .with_permission_type(ACL_ANY),
            )
            .await?;
        if described.error_code == code_of(ResponseError::SecurityDisabled) {
            return Ok(None);
        }
        check(
            &format!("describe ACLs of {principal}"),
            described.error_code,
            described.error_message.as_ref(),
            &[],
        )?;
        let mut have = BTreeSet::new();
        for resource in described.resources {
            for acl in resource.acls {
                if acl.principal.as_str() != principal {
                    continue;
                }
                have.insert(WireAcl {
                    resource_type: resource.resource_type,
                    resource_name: resource.resource_name.to_string(),
                    pattern_type: resource.pattern_type,
                    operation: acl.operation,
                    permission: acl.permission_type,
                    host: acl.host.to_string(),
                });
            }
        }
        Ok(Some(have))
    }

    async fn apply_acl_diff(
        &mut self,
        principal: &str,
        want: &BTreeSet<WireAcl>,
        have: &BTreeSet<WireAcl>,
    ) -> Result<(), OperatorError> {
        let create: Vec<AclCreation> = want
            .difference(have)
            .map(|a| {
                AclCreation::default()
                    .with_resource_type(a.resource_type)
                    .with_resource_name(s(&a.resource_name))
                    .with_resource_pattern_type(a.pattern_type)
                    .with_principal(s(principal))
                    .with_host(s(&a.host))
                    .with_operation(a.operation)
                    .with_permission_type(a.permission)
            })
            .collect();
        if !create.is_empty() {
            let response = self
                .send(CreateAclsRequest::default().with_creations(create))
                .await?;
            for result in response.results {
                check(
                    &format!("create ACLs for {principal}"),
                    result.error_code,
                    result.error_message.as_ref(),
                    &[],
                )?;
            }
        }

        let delete: Vec<DeleteAclsFilter> = have
            .difference(want)
            .map(|a| {
                DeleteAclsFilter::default()
                    .with_resource_type_filter(a.resource_type)
                    .with_resource_name_filter(Some(s(&a.resource_name)))
                    .with_pattern_type_filter(a.pattern_type)
                    .with_principal_filter(Some(s(principal)))
                    .with_host_filter(Some(s(&a.host)))
                    .with_operation(a.operation)
                    .with_permission_type(a.permission)
            })
            .collect();
        if !delete.is_empty() {
            let response = self
                .send(DeleteAclsRequest::default().with_filters(delete))
                .await?;
            for result in response.filter_results {
                check(
                    &format!("delete ACLs of {principal}"),
                    result.error_code,
                    result.error_message.as_ref(),
                    &[],
                )?;
            }
        }
        Ok(())
    }
}

fn wire_mechanism(mechanism: SaslMechanism) -> i8 {
    match mechanism {
        SaslMechanism::ScramSha512 => SCRAM_SHA_512,
        _ => SCRAM_SHA_256,
    }
}

/// An ACL binding in Kafka's wire codes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WireAcl {
    resource_type: i8,
    resource_name: String,
    pattern_type: i8,
    operation: i8,
    permission: i8,
    host: String,
}

impl WireAcl {
    fn to_entry(&self) -> AclEntry {
        let name = |table: &[(i8, &str)], code: i8| {
            table
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, n)| n.to_string())
                .unwrap_or_else(|| format!("code {code}"))
        };
        AclEntry {
            resource_type: name(
                &[
                    (2, "topic"),
                    (3, "group"),
                    (4, "cluster"),
                    (5, "transactionalId"),
                ],
                self.resource_type,
            ),
            resource_name: self.resource_name.clone(),
            pattern_type: name(&[(3, "literal"), (4, "prefixed")], self.pattern_type),
            operation: name(
                &[
                    (2, "All"),
                    (3, "Read"),
                    (4, "Write"),
                    (5, "Create"),
                    (6, "Delete"),
                    (7, "Alter"),
                    (8, "Describe"),
                    (9, "ClusterAction"),
                    (10, "DescribeConfigs"),
                    (11, "AlterConfigs"),
                    (12, "IdempotentWrite"),
                ],
                self.operation,
            ),
            effect: name(&[(2, "Deny"), (3, "Allow")], self.permission),
        }
    }

    fn from_entry(e: &AclEntry) -> Result<Self, OperatorError> {
        let bad = |what: &str, v: &str| OperatorError::Config(format!("unknown ACL {what} '{v}'"));
        let norm = |v: &str| v.trim().to_ascii_lowercase().replace(['-', '_'], "");
        let resource_type = match norm(&e.resource_type).as_str() {
            "topic" => 2,
            "group" => 3,
            "cluster" => 4,
            "transactionalid" => 5,
            _ => return Err(bad("resource type", &e.resource_type)),
        };
        let pattern_type = match norm(&e.pattern_type).as_str() {
            "" | "literal" => 3,
            "prefixed" => 4,
            _ => return Err(bad("pattern type", &e.pattern_type)),
        };
        let operation = match norm(&e.operation).as_str() {
            "all" => 2,
            "read" => 3,
            "write" => 4,
            "create" => 5,
            "delete" => 6,
            "alter" => 7,
            "describe" => 8,
            "clusteraction" => 9,
            "describeconfigs" => 10,
            "alterconfigs" => 11,
            "idempotentwrite" => 12,
            _ => return Err(bad("operation", &e.operation)),
        };
        let permission = match norm(&e.effect).as_str() {
            "" | "allow" => 3,
            "deny" => 2,
            _ => return Err(bad("effect", &e.effect)),
        };
        Ok(Self {
            resource_type,
            resource_name: e.resource_name.clone(),
            pattern_type,
            operation,
            permission,
            host: "*".into(),
        })
    }
}

/// Expand the CRD's ACL rules (several operations each) into one entry per
/// operation.
pub fn expand_acl_rules(rules: &[crate::crds::user::AclRule]) -> Vec<AclEntry> {
    let mut entries = Vec::new();
    for rule in rules {
        for op in &rule.operations {
            entries.push(AclEntry {
                resource_type: rule.resource.type_.clone(),
                resource_name: rule.resource.name.clone(),
                pattern_type: rule.resource.pattern_type.clone(),
                operation: op.clone(),
                effect: rule.effect.clone(),
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::user::{AclResource, AclRule};

    #[test]
    fn expand_acl_rules_one_entry_per_operation() {
        let rules = vec![
            AclRule {
                resource: AclResource {
                    type_: "topic".into(),
                    name: "orders".into(),
                    pattern_type: "literal".into(),
                },
                operations: vec!["Read".into(), "Write".into()],
                effect: "Allow".into(),
            },
            AclRule {
                resource: AclResource {
                    type_: "group".into(),
                    name: "my-group".into(),
                    pattern_type: "literal".into(),
                },
                operations: vec!["Read".into()],
                effect: "Allow".into(),
            },
        ];
        let entries = expand_acl_rules(&rules);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            (
                entries[0].operation.as_str(),
                entries[0].resource_type.as_str()
            ),
            ("Read", "topic")
        );
        assert_eq!(entries[1].operation, "Write");
        assert_eq!(entries[2].resource_type, "group");
    }

    #[test]
    fn acl_entries_map_to_kafka_codes() {
        let e = AclEntry {
            resource_type: "transactionalId".into(),
            resource_name: "tx-".into(),
            pattern_type: "Prefixed".into(),
            operation: "DescribeConfigs".into(),
            effect: "Deny".into(),
        };
        let w = WireAcl::from_entry(&e).unwrap();
        assert_eq!(
            (w.resource_type, w.pattern_type, w.operation, w.permission),
            (5, 4, 10, 2)
        );
        assert_eq!(w.host, "*");
        let bad = AclEntry {
            operation: "Fly".into(),
            ..e
        };
        assert!(WireAcl::from_entry(&bad).is_err());
    }

    #[test]
    fn acl_codes_round_trip() {
        for (rt, pt, op, perm) in [(2, 3, 3, 3), (3, 4, 8, 2), (4, 3, 9, 3), (5, 4, 12, 3)] {
            let w = WireAcl {
                resource_type: rt,
                resource_name: "x".into(),
                pattern_type: pt,
                operation: op,
                permission: perm,
                host: "*".into(),
            };
            assert_eq!(WireAcl::from_entry(&w.to_entry()).unwrap(), w);
        }
    }

    #[test]
    fn authentication_types() {
        assert_eq!(
            scram_mechanism("sasl-scram-sha-512").unwrap(),
            Some(SaslMechanism::ScramSha512)
        );
        assert_eq!(
            scram_mechanism("SASL-SCRAM-SHA-256").unwrap(),
            Some(SaslMechanism::ScramSha256)
        );
        assert_eq!(scram_mechanism("tls").unwrap(), None);
        assert!(scram_mechanism("sasl-plain").is_err());
        assert!(scram_mechanism("kerberos").is_err());
    }

    #[test]
    fn credentials_debug_hides_the_password() {
        let c = Credentials {
            username: "__chronik_broker".into(),
            password: "hunter2-hunter2-hunter2".into(),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
