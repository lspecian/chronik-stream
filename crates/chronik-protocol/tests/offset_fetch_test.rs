//! OffsetFetch request-parse conformance — the multi-topic bug.
//!
//! A consumer group that spans MORE THAN ONE topic sends an OffsetFetch whose
//! `topics` array carries a `partition_indexes` array per topic. Our v0-v7 parser
//! skipped that partition array (a comment claimed "v0-v7: No partitions in
//! request"), which is wrong for EVERY version — OffsetFetch has per-topic
//! partitions from v0. Skipping them misaligned the decoder: the first topic's
//! partition bytes were read as the second topic's name, yielding
//! `Topic name cannot be null` and stalling (crashing) the consumer. Single-topic
//! requests limped by accident, which is why it only bit multi-topic groups.
//!
//! These tests decode a real multi-topic request the way the broker must; before
//! the fix both panic on the `.expect(...)`.

use bytes::Bytes;
use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::{ApiKey, RequestHeader};

fn uvarint(mut v: u32, out: &mut Vec<u8>) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn compact_str(s: &str, out: &mut Vec<u8>) {
    uvarint(s.len() as u32 + 1, out); // compact string length is len + 1
    out.extend_from_slice(s.as_bytes());
}

fn header(api_version: i16) -> RequestHeader {
    RequestHeader {
        api_key: ApiKey::OffsetFetch,
        api_version,
        correlation_id: 1,
        client_id: None,
    }
}

/// v7 (flexible): group_id + topics[{name, partition_indexes, tags}] + require_stable + tags.
#[test]
fn offset_fetch_v7_multi_topic_with_partitions() {
    let mut body = Vec::new();
    compact_str("cg1", &mut body); // group_id
    uvarint(3, &mut body); // topics compact array: 2 topics -> count 3
    // topic 1: "t.one" partitions [0, 1, 2]
    compact_str("t.one", &mut body);
    uvarint(4, &mut body); // 3 partitions -> compact count 4
    body.extend_from_slice(&0i32.to_be_bytes());
    body.extend_from_slice(&1i32.to_be_bytes());
    body.extend_from_slice(&2i32.to_be_bytes());
    uvarint(0, &mut body); // topic-level tagged fields
    // topic 2: "t.two" partitions [7]
    compact_str("t.two", &mut body);
    uvarint(2, &mut body); // 1 partition -> compact count 2
    body.extend_from_slice(&7i32.to_be_bytes());
    uvarint(0, &mut body); // topic-level tagged fields
    body.push(0); // require_stable = false (v7)
    uvarint(0, &mut body); // request-level tagged fields

    let handler = ProtocolHandler::new();
    let mut buf = Bytes::from(body);
    let req = handler
        .parse_offset_fetch_request(&header(7), &mut buf)
        .expect("v7 multi-topic OffsetFetch must parse");

    assert_eq!(req.group_id, "cg1");
    let topics = req.topics.expect("topics present");
    assert_eq!(topics.len(), 2, "both topics must be parsed");
    assert_eq!(topics[0].name, "t.one");
    assert_eq!(topics[0].partitions, vec![0, 1, 2]);
    assert_eq!(topics[1].name, "t.two");
    assert_eq!(topics[1].partitions, vec![7]);
}

/// v5 (non-flexible): same shape, no compact encoding / tags / require_stable.
#[test]
fn offset_fetch_v5_multi_topic_with_partitions() {
    let mut body = Vec::new();
    // group_id "cg1" as a regular STRING (i16 length)
    body.extend_from_slice(&3i16.to_be_bytes());
    body.extend_from_slice(b"cg1");
    // topics array (i32 count): 2
    body.extend_from_slice(&2i32.to_be_bytes());
    // topic 1: "t.one" partitions [0, 1, 2]
    body.extend_from_slice(&5i16.to_be_bytes());
    body.extend_from_slice(b"t.one");
    body.extend_from_slice(&3i32.to_be_bytes());
    body.extend_from_slice(&0i32.to_be_bytes());
    body.extend_from_slice(&1i32.to_be_bytes());
    body.extend_from_slice(&2i32.to_be_bytes());
    // topic 2: "t.two" partitions [7]
    body.extend_from_slice(&5i16.to_be_bytes());
    body.extend_from_slice(b"t.two");
    body.extend_from_slice(&1i32.to_be_bytes());
    body.extend_from_slice(&7i32.to_be_bytes());

    let handler = ProtocolHandler::new();
    let mut buf = Bytes::from(body);
    let req = handler
        .parse_offset_fetch_request(&header(5), &mut buf)
        .expect("v5 multi-topic OffsetFetch must parse");

    assert_eq!(req.group_id, "cg1");
    let topics = req.topics.expect("topics present");
    assert_eq!(topics.len(), 2);
    assert_eq!(topics[0].name, "t.one");
    assert_eq!(topics[0].partitions, vec![0, 1, 2]);
    assert_eq!(topics[1].name, "t.two");
    assert_eq!(topics[1].partitions, vec![7]);
}
