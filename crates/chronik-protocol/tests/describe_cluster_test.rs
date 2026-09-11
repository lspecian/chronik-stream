//! DescribeCluster wire format, and the ApiVersions promise behind it (#35).
//!
//! `kafka-topics.sh --describe` failed with
//! `The AdminClient thread has exited. Call: listNodes`, reported as a null
//! clusterId. The id was never null — the response was unparseable, so nothing
//! arrived at all.
//!
//! Both faults were of a kind unit tests on our own encoder cannot catch, because
//! the encoder was self-consistent. Only decoding the bytes the way a client does,
//! against the published schema, shows it.

use bytes::Bytes;
use chronik_protocol::handler::{ProtocolHandler, CLUSTER_ID};
use chronik_protocol::parser::{supported_api_versions, ApiKey, Decoder};

/// Build a DescribeCluster request: flexible header (v2) + body.
fn describe_cluster_request(api_version: i16, correlation_id: i32) -> Bytes {
    let mut buf = Vec::new();
    buf.extend_from_slice(&60i16.to_be_bytes()); // api_key
    buf.extend_from_slice(&api_version.to_be_bytes());
    buf.extend_from_slice(&correlation_id.to_be_bytes());
    // client_id as a regular STRING, which is what a flexible request header uses
    let client = b"adminclient-1";
    buf.extend_from_slice(&(client.len() as i16).to_be_bytes());
    buf.extend_from_slice(client);
    buf.push(0); // header tagged fields

    buf.push(0); // include_cluster_authorized_operations = false
    if api_version >= 1 {
        buf.push(1); // endpoint_type = brokers
    }
    buf.push(0); // body tagged fields

    Bytes::from(buf)
}

/// Decode a DescribeCluster v1 response exactly as the Java client does.
///
/// Schema (flexible at every version):
///   throttle_time_ms INT32, error_code INT16,
///   error_message COMPACT_NULLABLE_STRING,
///   endpoint_type INT8 (v1+),
///   cluster_id COMPACT_STRING, controller_id INT32,
///   brokers COMPACT_ARRAY{ broker_id INT32, host COMPACT_STRING, port INT32,
///                          rack COMPACT_NULLABLE_STRING, TAG_BUFFER },
///   cluster_authorized_operations INT32, TAG_BUFFER
#[tokio::test]
async fn describe_cluster_v1_matches_the_published_schema() {
    let handler = ProtocolHandler::new();
    let response = handler
        .handle_request(&describe_cluster_request(1, 7))
        .await
        .expect("DescribeCluster v1 must be answered");

    let mut body = response.body;
    let mut d = Decoder::new(&mut body);

    // The field that was missing. Read as anything else, every subsequent field
    // lands at the wrong offset.
    let throttle = d.read_i32().expect("throttle_time_ms");
    assert_eq!(throttle, 0, "throttle_time_ms must be the FIRST field");

    let error_code = d.read_i16().expect("error_code");
    assert_eq!(error_code, 0, "error_code, immediately after throttle_time_ms");

    let error_message = d.read_compact_string().expect("error_message");
    assert_eq!(error_message, None, "no error, so a null message");

    // Added at v1 by KIP-919. Fixing only the throttle would leave the body one
    // byte short of the schema and shift everything after it.
    let endpoint_type = d.read_i8().expect("endpoint_type (v1+)");
    assert_eq!(endpoint_type, 1, "endpoint_type 1 = brokers");

    let cluster_id = d.read_compact_string().expect("cluster_id");
    assert_eq!(
        cluster_id.as_deref(),
        Some(CLUSTER_ID),
        "cluster_id must arrive intact — this is what #35 reported as null"
    );

    let controller_id = d.read_i32().expect("controller_id");
    assert!(controller_id >= 0, "a controller must be named");

    let broker_count = d.read_unsigned_varint().expect("brokers array length");
    assert_eq!(broker_count, 2, "compact array of 1 broker encodes as 2");

    let broker_id = d.read_i32().expect("broker_id");
    assert_eq!(broker_id, controller_id, "the single broker is the controller");
    let host = d.read_compact_string().expect("host");
    assert!(host.is_some_and(|h| !h.is_empty()), "a host clients can dial");
    let port = d.read_i32().expect("port");
    assert!(port > 0, "a usable port");
    assert_eq!(d.read_compact_string().expect("rack"), None, "no rack");
    assert_eq!(d.read_unsigned_varint().expect("broker tags"), 0);

    let _ops = d.read_i32().expect("cluster_authorized_operations");
    assert_eq!(d.read_unsigned_varint().expect("response tags"), 0);

    // Nothing may remain: a client that finds trailing bytes has mis-parsed, and
    // a body that ends early is the underflow this test exists to prevent.
    assert_eq!(d.remaining(), 0, "response must be exactly the schema, no more");
}

/// The cluster must report one identity.
///
/// Metadata answered `"chronik-stream"` while DescribeCluster answered a
/// different value, so a client reading the id from one API and checking it
/// against the other saw a cluster disagreeing with itself.
#[test]
fn the_cluster_id_has_a_single_definition() {
    assert_eq!(CLUSTER_ID.len(), 22, "Kafka cluster ids are 22 base64 chars");
    assert!(
        CLUSTER_ID.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "must be base64url-safe: some tooling validates the shape"
    );
}

/// ApiVersions is a promise, and breaking it is worse than staying silent.
///
/// These APIs have no implementation — only the catch-all, which returns a bare
/// error code rather than the API's response schema. Advertising them made the
/// Java AdminClient send them and then die parsing the reply, taking every other
/// call on that client with it. `kafka-topics.sh --describe` calls
/// `listPartitionReassignments`, which is how #35 surfaced.
///
/// Left out of ApiVersions, a client reports the API as unsupported and carries
/// on — `--describe` simply omits reassignment information and completes.
#[test]
fn unimplemented_apis_are_not_advertised() {
    let advertised = supported_api_versions();

    for api in [
        ApiKey::ListPartitionReassignments,
        ApiKey::AlterPartitionReassignments,
        ApiKey::ElectLeaders,
        ApiKey::AlterReplicaLogDirs,
        ApiKey::DescribeClientQuotas,
        ApiKey::AlterClientQuotas,
        // DescribeUserScramCredentials / AlterUserScramCredentials were on this
        // list and have been REMOVED because they are now genuinely
        // implemented — see `unimplemented_apis_are_not_advertised`'s companion
        // below. They are handled in `chronik-server`'s `kafka_handler`, which
        // matches them explicitly and so never reaches this crate's catch-all.
        ApiKey::AlterPartition,
        ApiKey::UpdateFeatures,
        ApiKey::Envelope,
        ApiKey::DescribeProducers,
        ApiKey::DescribeTransactions,
        ApiKey::ListTransactions,
        ApiKey::AllocateProducerIds,
        ApiKey::CreateDelegationToken,
        ApiKey::RenewDelegationToken,
        ApiKey::ExpireDelegationToken,
        ApiKey::DescribeDelegationToken,
    ] {
        assert!(
            !advertised.contains_key(&api),
            "{:?} is advertised but only the catch-all answers it. Implement it \
             properly or leave it unadvertised — a malformed response kills the \
             Java AdminClient's I/O thread and every later call with it.",
            api
        );
    }
}

/// The APIs clients actually depend on must still be advertised. A fix for the
/// above that over-reached would break every client instead of one command.
#[test]
fn the_apis_clients_depend_on_are_still_advertised() {
    let advertised = supported_api_versions();

    for api in [
        ApiKey::Produce,
        ApiKey::Fetch,
        ApiKey::Metadata,
        ApiKey::ApiVersions,
        ApiKey::CreateTopics,
        ApiKey::DeleteTopics,
        ApiKey::DescribeCluster,
        ApiKey::DescribeConfigs,
        ApiKey::FindCoordinator,
        ApiKey::JoinGroup,
        ApiKey::SyncGroup,
        ApiKey::Heartbeat,
        ApiKey::LeaveGroup,
        ApiKey::OffsetCommit,
        ApiKey::OffsetFetch,
        ApiKey::ListOffsets,
        ApiKey::ListGroups,
        ApiKey::DescribeGroups,
        ApiKey::DeleteGroups,
        ApiKey::DeleteRecords,
        ApiKey::OffsetForLeaderEpoch,
        ApiKey::InitProducerId,
    ] {
        assert!(
            advertised.contains_key(&api),
            "{:?} must stay advertised: clients require it",
            api
        );
    }
}

/// The SCRAM credential APIs must stay advertised, because they now have a real
/// implementation.
///
/// The rule this crate enforces is "advertise only what is implemented", in both
/// directions. Withdrawing these would silently disable `kafka-configs.sh
/// --entity-type users`, which is the only way to manage SCRAM credentials at
/// runtime; a client told the API is unsupported reports that the broker cannot
/// manage users at all.
///
/// The implementation lives in `chronik-server`'s `kafka_handler`, which matches
/// API 50 and 51 explicitly before the fall-through to this crate's catch-all,
/// so the malformed-response hazard that motivated the list above does not apply
/// to them. If that routing is ever removed, this test keeps passing while the
/// server regresses — so the end-to-end proof is
/// `tests/integration/sasl_enforcement_test.rs::scram_user_created_via_api_survives_restart`,
/// which drives the real wire protocol.
#[test]
fn implemented_scram_credential_apis_stay_advertised() {
    let advertised = supported_api_versions();

    for api in [
        ApiKey::DescribeUserScramCredentials,
        ApiKey::AlterUserScramCredentials,
    ] {
        assert!(
            advertised.contains_key(&api),
            "{:?} is implemented but not advertised — clients will report that \
             this broker cannot manage SCRAM users",
            api
        );
    }
}
