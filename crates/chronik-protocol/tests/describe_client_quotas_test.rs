//! DescribeClientQuotas (API 48): the broker has no quotas and says so, in the
//! exact schema a client decodes. `kafka-configs.sh --describe --entity-type
//! users` calls it before listing SCRAM credentials, and failed with
//! "The node does not support DESCRIBE_CLIENT_QUOTAS" while it was unadvertised.

use bytes::Bytes;
use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::{supported_api_versions, ApiKey, Decoder};

/// Request with one component (entity_type "user", match_type 1 = default/any).
fn request(api_version: i16) -> Bytes {
    let mut b = Vec::new();
    b.extend_from_slice(&48i16.to_be_bytes());
    b.extend_from_slice(&api_version.to_be_bytes());
    b.extend_from_slice(&9i32.to_be_bytes()); // correlation id
    let client = b"adminclient-1";
    b.extend_from_slice(&(client.len() as i16).to_be_bytes());
    b.extend_from_slice(client);
    if api_version >= 1 {
        b.push(0); // header tagged fields
        b.push(2); // components: compact array of 1
        b.push(5); // entity_type "user" (compact: len + 1)
        b.extend_from_slice(b"user");
        b.push(1); // match_type
        b.push(0); // match: null
        b.push(0); // component tagged fields
        b.push(0); // strict = false
        b.push(0); // body tagged fields
    } else {
        b.extend_from_slice(&1i32.to_be_bytes()); // components: 1
        b.extend_from_slice(&4i16.to_be_bytes());
        b.extend_from_slice(b"user");
        b.push(1); // match_type
        b.extend_from_slice(&(-1i16).to_be_bytes()); // match: null
        b.push(0); // strict
    }
    Bytes::from(b)
}

#[test]
fn describe_client_quotas_is_advertised_at_v0_and_v1() {
    let range = supported_api_versions()
        .get(&ApiKey::DescribeClientQuotas)
        .map(|r| (r.min, r.max))
        .expect("DescribeClientQuotas must be advertised");
    assert_eq!(range, (0, 1));
    assert!(
        !supported_api_versions().contains_key(&ApiKey::AlterClientQuotas),
        "AlterClientQuotas must stay unadvertised: a quota would be accepted and never enforced"
    );
}

/// v1 (flexible): throttle INT32, error_code INT16, error_message
/// COMPACT_NULLABLE_STRING, entries COMPACT_NULLABLE_ARRAY, TAG_BUFFER.
#[tokio::test]
async fn v1_answers_no_entries_in_the_published_schema() {
    let response = ProtocolHandler::new().handle_request(&request(1)).await.expect("answered");
    let mut body = response.body;
    let mut d = Decoder::new(&mut body);
    assert_eq!(d.read_i32().unwrap(), 0, "throttle_time_ms");
    assert_eq!(d.read_i16().unwrap(), 0, "error_code");
    assert_eq!(d.read_compact_string().unwrap(), None, "error_message");
    assert_eq!(d.read_unsigned_varint().unwrap(), 1, "entries: empty (not null)");
    assert_eq!(d.read_unsigned_varint().unwrap(), 0, "tagged fields");
    assert_eq!(d.remaining(), 0, "exactly the schema");
}

/// v0: throttle INT32, error_code INT16, error_message NULLABLE_STRING,
/// entries ARRAY.
#[tokio::test]
async fn v0_answers_no_entries_in_the_published_schema() {
    let response = ProtocolHandler::new().handle_request(&request(0)).await.expect("answered");
    let mut body = response.body;
    let mut d = Decoder::new(&mut body);
    assert_eq!(d.read_i32().unwrap(), 0, "throttle_time_ms");
    assert_eq!(d.read_i16().unwrap(), 0, "error_code");
    assert_eq!(d.read_string().unwrap(), None, "error_message");
    assert_eq!(d.read_i32().unwrap(), 0, "entries: empty");
    assert_eq!(d.remaining(), 0, "exactly the schema");
}
