//! ACL checks for the transactional APIs.
//!
//! Kafka authorizes every transactional request against the TransactionalId it
//! names (Write), and the requests that touch other resources against those too:
//! AddPartitionsToTxn needs Write on each topic, AddOffsetsToTxn needs Read on
//! the group, TxnOffsetCommit needs Read on the group and on each topic.
//!
//! Before this module none of it was checked. With ACLs on, any authenticated
//! principal could take over another application's transactional id (an
//! InitProducerId bumps the epoch and fences the real owner) and could commit a
//! consumer group's offsets through TxnOffsetCommit without any right on the
//! group — the check OffsetCommit has had since v2.14.0, walked around by the
//! transactional twin.
//!
//! The denial is built per API, in the shape its response carries errors, so
//! the client raises `TransactionalIdAuthorizationException`,
//! `GroupAuthorizationException` or `TopicAuthorizationException`.

use bytes::{Bytes, BytesMut};
use chronik_common::Result;
use chronik_protocol::describe_acls_types::AclOperation;
use chronik_protocol::handler::Response;
use chronik_protocol::parser::{
    is_flexible_version, ApiKey, Decoder, Encoder, KafkaDecodable, KafkaEncodable, RequestHeader,
    ResponseHeader,
};
use tracing::warn;

use crate::authorizer::{
    Authorizer, ERROR_GROUP_AUTHORIZATION_FAILED, ERROR_TOPIC_AUTHORIZATION_FAILED,
    ERROR_TRANSACTIONAL_ID_AUTHORIZATION_FAILED,
};
use crate::connection::ConnectionContext;

/// Kafka error code: not attempted because another part of the request failed.
const ERROR_OPERATION_NOT_ATTEMPTED: i16 = 55;

/// Whether `api_key` is one of the transactional APIs this module guards.
pub fn is_transactional_api(api_key: ApiKey) -> bool {
    matches!(
        api_key,
        ApiKey::InitProducerId
            | ApiKey::AddPartitionsToTxn
            | ApiKey::AddOffsetsToTxn
            | ApiKey::EndTxn
            | ApiKey::TxnOffsetCommit
    )
}

/// Check a transactional request. `Ok(None)` lets it through; `Ok(Some(_))` is
/// the denial to send instead of handling it.
///
/// `body` is the request body after the header; it is decoded from a clone, so
/// the caller's buffer is untouched for the real handler.
pub async fn check(
    authorizer: &Authorizer,
    ctx: &ConnectionContext,
    header: &RequestHeader,
    body: &Bytes,
) -> Result<Option<Response>> {
    if !authorizer.is_enabled() || !is_transactional_api(header.api_key) {
        return Ok(None);
    }
    let mut buf = body.clone();
    let mut decoder = Decoder::new(&mut buf);
    let version = header.api_version;
    let txn_ok = |id: String| async move {
        authorizer
            .authorize_transactional_id(ctx, &id, AclOperation::Write)
            .await
    };

    let body = match header.api_key {
        ApiKey::InitProducerId => {
            use chronik_protocol::init_producer_id_types::{
                InitProducerIdRequest, InitProducerIdResponse,
            };
            let request = InitProducerIdRequest::decode(&mut decoder, version)?;
            // An idempotent producer names no transactional id; its writes are
            // authorized topic by topic at Produce.
            let Some(id) = request.transactional_id else {
                return Ok(None);
            };
            if txn_ok(id.clone()).await {
                return Ok(None);
            }
            warn!("InitProducerId DENIED for transactional id '{id}'");
            encode(
                &InitProducerIdResponse {
                    throttle_time_ms: 0,
                    error_code: ERROR_TRANSACTIONAL_ID_AUTHORIZATION_FAILED,
                    producer_id: -1,
                    producer_epoch: -1,
                },
                version,
            )?
        }
        ApiKey::EndTxn => {
            use chronik_protocol::end_txn_types::{EndTxnRequest, EndTxnResponse};
            let request = EndTxnRequest::decode(&mut decoder, version)?;
            if txn_ok(request.transactional_id.clone()).await {
                return Ok(None);
            }
            warn!("EndTxn DENIED for transactional id '{}'", request.transactional_id);
            encode(
                &EndTxnResponse {
                    throttle_time_ms: 0,
                    error_code: ERROR_TRANSACTIONAL_ID_AUTHORIZATION_FAILED,
                },
                version,
            )?
        }
        ApiKey::AddOffsetsToTxn => {
            use chronik_protocol::add_offsets_to_txn_types::{
                AddOffsetsToTxnRequest, AddOffsetsToTxnResponse,
            };
            let request = AddOffsetsToTxnRequest::decode(&mut decoder, version)?;
            let error_code = if !txn_ok(request.transactional_id.clone()).await {
                ERROR_TRANSACTIONAL_ID_AUTHORIZATION_FAILED
            } else if !authorizer
                .authorize_group(ctx, &request.consumer_group_id, AclOperation::Read)
                .await
            {
                ERROR_GROUP_AUTHORIZATION_FAILED
            } else {
                return Ok(None);
            };
            warn!(
                "AddOffsetsToTxn DENIED (txn '{}', group '{}', error {error_code})",
                request.transactional_id, request.consumer_group_id
            );
            encode(
                &AddOffsetsToTxnResponse {
                    throttle_time_ms: 0,
                    error_code,
                },
                version,
            )?
        }
        ApiKey::AddPartitionsToTxn => {
            use chronik_protocol::add_partitions_to_txn_types::{
                AddPartitionsToTxnPartitionResult, AddPartitionsToTxnRequest,
                AddPartitionsToTxnResponse, AddPartitionsToTxnTopicResult,
            };
            let request = AddPartitionsToTxnRequest::decode(&mut decoder, version)?;
            let txn_allowed = txn_ok(request.transactional_id.clone()).await;
            let mut denied_topics = std::collections::HashSet::new();
            if txn_allowed {
                for topic in &request.topics {
                    if !authorizer
                        .authorize(
                            ctx,
                            chronik_protocol::describe_acls_types::ResourceType::Topic,
                            &topic.name,
                            AclOperation::Write,
                        )
                        .await
                    {
                        denied_topics.insert(topic.name.clone());
                    }
                }
                if denied_topics.is_empty() {
                    return Ok(None);
                }
            }
            warn!(
                "AddPartitionsToTxn DENIED (txn '{}', transactional id allowed: {txn_allowed}, \
                 topics denied: {denied_topics:?})",
                request.transactional_id
            );
            // Kafka's answer: every partition of a denied topic carries
            // TOPIC_AUTHORIZATION_FAILED and the rest OPERATION_NOT_ATTEMPTED —
            // the request is all or nothing.
            let results = request
                .topics
                .iter()
                .map(|topic| {
                    let code = if !txn_allowed {
                        ERROR_TRANSACTIONAL_ID_AUTHORIZATION_FAILED
                    } else if denied_topics.contains(&topic.name) {
                        ERROR_TOPIC_AUTHORIZATION_FAILED
                    } else {
                        ERROR_OPERATION_NOT_ATTEMPTED
                    };
                    AddPartitionsToTxnTopicResult {
                        name: topic.name.clone(),
                        results: topic
                            .partitions
                            .iter()
                            .map(|p| AddPartitionsToTxnPartitionResult {
                                partition: p.partition,
                                error_code: code,
                            })
                            .collect(),
                    }
                })
                .collect();
            encode(
                &AddPartitionsToTxnResponse {
                    throttle_time_ms: 0,
                    results,
                },
                version,
            )?
        }
        ApiKey::TxnOffsetCommit => {
            use chronik_protocol::txn_offset_commit_types::{
                TxnOffsetCommitPartitionResponse, TxnOffsetCommitRequest,
                TxnOffsetCommitResponse, TxnOffsetCommitTopicResponse,
            };
            let request = TxnOffsetCommitRequest::decode(&mut decoder, version)?;
            let whole_request_code = if !txn_ok(request.transactional_id.clone()).await {
                Some(ERROR_TRANSACTIONAL_ID_AUTHORIZATION_FAILED)
            } else if !authorizer
                .authorize_group(ctx, &request.consumer_group_id, AclOperation::Read)
                .await
            {
                Some(ERROR_GROUP_AUTHORIZATION_FAILED)
            } else {
                None
            };
            let mut topic_codes = Vec::with_capacity(request.topics.len());
            for topic in &request.topics {
                let code = match whole_request_code {
                    Some(code) => code,
                    None if !authorizer
                        .authorize(
                            ctx,
                            chronik_protocol::describe_acls_types::ResourceType::Topic,
                            &topic.name,
                            AclOperation::Read,
                        )
                        .await =>
                    {
                        ERROR_TOPIC_AUTHORIZATION_FAILED
                    }
                    None => 0,
                };
                topic_codes.push(code);
            }
            if topic_codes.iter().all(|c| *c == 0) {
                return Ok(None);
            }
            warn!(
                "TxnOffsetCommit DENIED (txn '{}', group '{}')",
                request.transactional_id, request.consumer_group_id
            );
            // Kafka commits nothing when any topic is denied, and reports the
            // allowed ones as not attempted.
            let topics = request
                .topics
                .iter()
                .zip(topic_codes)
                .map(|(topic, code)| TxnOffsetCommitTopicResponse {
                    name: topic.name.clone(),
                    partitions: topic
                        .partitions
                        .iter()
                        .map(|p| TxnOffsetCommitPartitionResponse {
                            partition_index: p.partition_index,
                            error_code: if code == 0 { ERROR_OPERATION_NOT_ATTEMPTED } else { code },
                        })
                        .collect(),
                })
                .collect();
            encode(
                &TxnOffsetCommitResponse {
                    throttle_time_ms: 0,
                    topics,
                },
                version,
            )?
        }
        _ => return Ok(None),
    };

    Ok(Some(Response {
        header: ResponseHeader {
            correlation_id: header.correlation_id,
        },
        body,
        is_flexible: is_flexible_version(header.api_key, version),
        api_key: header.api_key,
        throttle_time_ms: None,
    }))
}

fn encode<T: KafkaEncodable>(response: &T, version: i16) -> Result<Bytes> {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    response.encode(&mut encoder, version)?;
    Ok(buf.freeze())
}
