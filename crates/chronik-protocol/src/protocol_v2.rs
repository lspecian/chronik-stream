//! Enhanced Kafka wire protocol implementation using kafka-protocol crate.
//! This module provides full protocol support for Kafka 2.x through 4.x.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use kafka_protocol::messages::{
    ApiKey, ApiVersionsRequest, ApiVersionsResponse, ApiVersion,
    FetchRequest, FetchResponse, MetadataRequest, MetadataResponse,
    ProduceRequest, ProduceResponse, ListOffsetsRequest, ListOffsetsResponse,
    OffsetCommitRequest, OffsetCommitResponse, OffsetFetchRequest, OffsetFetchResponse,
    FindCoordinatorRequest, FindCoordinatorResponse, JoinGroupRequest, JoinGroupResponse,
    HeartbeatRequest, HeartbeatResponse, LeaveGroupRequest, LeaveGroupResponse,
    SyncGroupRequest, SyncGroupResponse, DescribeGroupsRequest, DescribeGroupsResponse,
    ListGroupsRequest, ListGroupsResponse, SaslHandshakeRequest, SaslHandshakeResponse,
    SaslAuthenticateRequest, SaslAuthenticateResponse, CreateTopicsRequest, CreateTopicsResponse,
    DeleteTopicsRequest, DeleteTopicsResponse, DeleteRecordsRequest, DeleteRecordsResponse,
    InitProducerIdRequest, InitProducerIdResponse, AddPartitionsToTxnRequest, AddPartitionsToTxnResponse,
    AddOffsetsToTxnRequest, AddOffsetsToTxnResponse, EndTxnRequest, EndTxnResponse,
    WriteTxnMarkersRequest, WriteTxnMarkersResponse, TxnOffsetCommitRequest, TxnOffsetCommitResponse,
    DescribeAclsRequest, DescribeAclsResponse, CreateAclsRequest, CreateAclsResponse,
    DeleteAclsRequest, DeleteAclsResponse, DescribeConfigsRequest, DescribeConfigsResponse,
    AlterConfigsRequest, AlterConfigsResponse, AlterReplicaLogDirsRequest, AlterReplicaLogDirsResponse,
    DescribeLogDirsRequest, DescribeLogDirsResponse, CreatePartitionsRequest, CreatePartitionsResponse,
    CreateDelegationTokenRequest, CreateDelegationTokenResponse, RenewDelegationTokenRequest,
    RenewDelegationTokenResponse, ExpireDelegationTokenRequest, ExpireDelegationTokenResponse,
    DescribeDelegationTokenRequest, DescribeDelegationTokenResponse, DeleteGroupsRequest,
    DeleteGroupsResponse, ElectLeadersRequest, ElectLeadersResponse, IncrementalAlterConfigsRequest,
    IncrementalAlterConfigsResponse, AlterPartitionReassignmentsRequest, AlterPartitionReassignmentsResponse,
    ListPartitionReassignmentsRequest, ListPartitionReassignmentsResponse, OffsetDeleteRequest,
    OffsetDeleteResponse, RequestHeader, ResponseHeader,
};
use kafka_protocol::protocol::{Decodable, Encodable, Message, HeaderVersion};
use kafka_protocol::error::ParseError;

use chronik_common::{Result, Error};
use tracing::{debug, trace, warn};

/// Protocol version negotiation info
#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub min_version: i16,
    pub max_version: i16,
}

/// Request with parsed header and body
#[derive(Debug)]
pub struct Request {
    pub header: RequestHeader,
    pub api_key: ApiKey,
    pub api_version: i16,
    pub body: RequestBody,
}

/// Supported request body variants
#[derive(Debug)]
pub enum RequestBody {
    ApiVersions(ApiVersionsRequest),
    Metadata(MetadataRequest),
    Produce(ProduceRequest),
    Fetch(FetchRequest),
    ListOffsets(ListOffsetsRequest),
    OffsetCommit(OffsetCommitRequest),
    OffsetFetch(OffsetFetchRequest),
    FindCoordinator(FindCoordinatorRequest),
    JoinGroup(JoinGroupRequest),
    Heartbeat(HeartbeatRequest),
    LeaveGroup(LeaveGroupRequest),
    SyncGroup(SyncGroupRequest),
    DescribeGroups(DescribeGroupsRequest),
    ListGroups(ListGroupsRequest),
    SaslHandshake(SaslHandshakeRequest),
    SaslAuthenticate(SaslAuthenticateRequest),
    CreateTopics(CreateTopicsRequest),
    DeleteTopics(DeleteTopicsRequest),
    DeleteRecords(DeleteRecordsRequest),
    InitProducerId(InitProducerIdRequest),
    AddPartitionsToTxn(AddPartitionsToTxnRequest),
    AddOffsetsToTxn(AddOffsetsToTxnRequest),
    EndTxn(EndTxnRequest),
    WriteTxnMarkers(WriteTxnMarkersRequest),
    TxnOffsetCommit(TxnOffsetCommitRequest),
    DescribeAcls(DescribeAclsRequest),
    CreateAcls(CreateAclsRequest),
    DeleteAcls(DeleteAclsRequest),
    DescribeConfigs(DescribeConfigsRequest),
    AlterConfigs(AlterConfigsRequest),
    AlterReplicaLogDirs(AlterReplicaLogDirsRequest),
    DescribeLogDirs(DescribeLogDirsRequest),
    CreatePartitions(CreatePartitionsRequest),
    CreateDelegationToken(CreateDelegationTokenRequest),
    RenewDelegationToken(RenewDelegationTokenRequest),
    ExpireDelegationToken(ExpireDelegationTokenRequest),
    DescribeDelegationToken(DescribeDelegationTokenRequest),
    DeleteGroups(DeleteGroupsRequest),
    ElectLeaders(ElectLeadersRequest),
    IncrementalAlterConfigs(IncrementalAlterConfigsRequest),
    AlterPartitionReassignments(AlterPartitionReassignmentsRequest),
    ListPartitionReassignments(ListPartitionReassignmentsRequest),
    OffsetDelete(OffsetDeleteRequest),
}

/// Response wrapper
#[derive(Debug)]
pub struct Response {
    pub header: ResponseHeader,
    pub body: ResponseBody,
}

/// Supported response body variants
#[derive(Debug)]
pub enum ResponseBody {
    ApiVersions(ApiVersionsResponse),
    Metadata(MetadataResponse),
    Produce(ProduceResponse),
    Fetch(FetchResponse),
    ListOffsets(ListOffsetsResponse),
    OffsetCommit(OffsetCommitResponse),
    OffsetFetch(OffsetFetchResponse),
    FindCoordinator(FindCoordinatorResponse),
    JoinGroup(JoinGroupResponse),
    Heartbeat(HeartbeatResponse),
    LeaveGroup(LeaveGroupResponse),
    SyncGroup(SyncGroupResponse),
    DescribeGroups(DescribeGroupsResponse),
    ListGroups(ListGroupsResponse),
    SaslHandshake(SaslHandshakeResponse),
    SaslAuthenticate(SaslAuthenticateResponse),
    CreateTopics(CreateTopicsResponse),
    DeleteTopics(DeleteTopicsResponse),
    DeleteRecords(DeleteRecordsResponse),
    InitProducerId(InitProducerIdResponse),
    AddPartitionsToTxn(AddPartitionsToTxnResponse),
    AddOffsetsToTxn(AddOffsetsToTxnResponse),
    EndTxn(EndTxnResponse),
    WriteTxnMarkers(WriteTxnMarkersResponse),
    TxnOffsetCommit(TxnOffsetCommitResponse),
    DescribeAcls(DescribeAclsResponse),
    CreateAcls(CreateAclsResponse),
    DeleteAcls(DeleteAclsResponse),
    DescribeConfigs(DescribeConfigsResponse),
    AlterConfigs(AlterConfigsResponse),
    AlterReplicaLogDirs(AlterReplicaLogDirsResponse),
    DescribeLogDirs(DescribeLogDirsResponse),
    CreatePartitions(CreatePartitionsResponse),
    CreateDelegationToken(CreateDelegationTokenResponse),
    RenewDelegationToken(RenewDelegationTokenResponse),
    ExpireDelegationToken(ExpireDelegationTokenResponse),
    DescribeDelegationToken(DescribeDelegationTokenResponse),
    DeleteGroups(DeleteGroupsResponse),
    ElectLeaders(ElectLeadersResponse),
    IncrementalAlterConfigs(IncrementalAlterConfigsResponse),
    AlterPartitionReassignments(AlterPartitionReassignmentsResponse),
    ListPartitionReassignments(ListPartitionReassignmentsResponse),
    OffsetDelete(OffsetDeleteResponse),
}

/// Protocol parser for handling Kafka wire protocol
pub struct ProtocolParser {
    /// Supported API versions
    supported_versions: Arc<HashMap<ApiKey, VersionInfo>>,
}

impl ProtocolParser {
    /// Create a new protocol parser with default version support
    pub fn new() -> Self {
        Self {
            supported_versions: Arc::new(Self::default_supported_versions()),
        }
    }

    /// Create parser with custom version support
    pub fn with_versions(versions: HashMap<ApiKey, VersionInfo>) -> Self {
        Self {
            supported_versions: Arc::new(versions),
        }
    }

    /// Get default supported API versions (Kafka 2.x - 4.x compatibility)
    fn default_supported_versions() -> HashMap<ApiKey, VersionInfo> {
        let mut versions = HashMap::new();
        
        // Core APIs with broad version support
        versions.insert(ApiKey::Produce, VersionInfo { min_version: 0, max_version: 9 });
        versions.insert(ApiKey::Fetch, VersionInfo { min_version: 0, max_version: 13 });
        versions.insert(ApiKey::ListOffsets, VersionInfo { min_version: 0, max_version: 7 });
        versions.insert(ApiKey::Metadata, VersionInfo { min_version: 0, max_version: 12 });
        versions.insert(ApiKey::OffsetCommit, VersionInfo { min_version: 0, max_version: 8 });
        versions.insert(ApiKey::OffsetFetch, VersionInfo { min_version: 0, max_version: 8 });
        versions.insert(ApiKey::FindCoordinator, VersionInfo { min_version: 0, max_version: 4 });
        versions.insert(ApiKey::JoinGroup, VersionInfo { min_version: 0, max_version: 9 });
        versions.insert(ApiKey::Heartbeat, VersionInfo { min_version: 0, max_version: 4 });
        versions.insert(ApiKey::LeaveGroup, VersionInfo { min_version: 0, max_version: 5 });
        versions.insert(ApiKey::SyncGroup, VersionInfo { min_version: 0, max_version: 5 });
        versions.insert(ApiKey::DescribeGroups, VersionInfo { min_version: 0, max_version: 5 });
        versions.insert(ApiKey::ListGroups, VersionInfo { min_version: 0, max_version: 4 });
        versions.insert(ApiKey::SaslHandshake, VersionInfo { min_version: 0, max_version: 1 });
        versions.insert(ApiKey::ApiVersions, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::CreateTopics, VersionInfo { min_version: 0, max_version: 7 });
        versions.insert(ApiKey::DeleteTopics, VersionInfo { min_version: 0, max_version: 6 });
        versions.insert(ApiKey::DeleteRecords, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::InitProducerId, VersionInfo { min_version: 0, max_version: 4 });
        versions.insert(ApiKey::AddPartitionsToTxn, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::AddOffsetsToTxn, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::EndTxn, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::WriteTxnMarkers, VersionInfo { min_version: 0, max_version: 1 });
        versions.insert(ApiKey::TxnOffsetCommit, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::DescribeAcls, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::CreateAcls, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::DeleteAcls, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::DescribeConfigs, VersionInfo { min_version: 0, max_version: 4 });
        versions.insert(ApiKey::AlterConfigs, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::AlterReplicaLogDirs, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::DescribeLogDirs, VersionInfo { min_version: 0, max_version: 4 });
        versions.insert(ApiKey::SaslAuthenticate, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::CreatePartitions, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::CreateDelegationToken, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::RenewDelegationToken, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::ExpireDelegationToken, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::DescribeDelegationToken, VersionInfo { min_version: 0, max_version: 3 });
        versions.insert(ApiKey::DeleteGroups, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::ElectLeaders, VersionInfo { min_version: 0, max_version: 2 });
        versions.insert(ApiKey::IncrementalAlterConfigs, VersionInfo { min_version: 0, max_version: 1 });
        versions.insert(ApiKey::AlterPartitionReassignments, VersionInfo { min_version: 0, max_version: 0 });
        versions.insert(ApiKey::ListPartitionReassignments, VersionInfo { min_version: 0, max_version: 0 });
        versions.insert(ApiKey::OffsetDelete, VersionInfo { min_version: 0, max_version: 0 });
        
        versions
    }

    /// Parse a request from bytes
    pub fn parse_request(&self, mut data: Bytes) -> Result<Request> {
        // Parse request header
        let header_version = RequestHeader::header_version(ApiKey::ApiVersions as i16, 0);
        let header = RequestHeader::decode(&mut data, header_version)
            .map_err(|e| Error::Protocol(format!("Failed to parse request header: {}", e)))?;
        
        let api_key = ApiKey::try_from(header.request_api_key)
            .map_err(|_| Error::Protocol(format!("Unknown API key: {}", header.request_api_key)))?;
        let api_version = header.request_api_version;

        debug!(
            "Parsing request - API: {:?}, Version: {}, CorrelationId: {}", 
            api_key, api_version, header.correlation_id
        );

        // Check if we support this API version
        if let Some(version_info) = self.supported_versions.get(&api_key) {
            if api_version < version_info.min_version || api_version > version_info.max_version {
                return Err(Error::Protocol(format!(
                    "Unsupported version {} for API {:?} (supported: {}-{})",
                    api_version, api_key, version_info.min_version, version_info.max_version
                )));
            }
        } else {
            return Err(Error::Protocol(format!("Unsupported API: {:?}", api_key)));
        }

        // Parse request body based on API key
        let body = match api_key {
            ApiKey::ApiVersions => {
                let req = ApiVersionsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse ApiVersions request: {}", e)))?;
                RequestBody::ApiVersions(req)
            }
            ApiKey::Metadata => {
                let req = MetadataRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse Metadata request: {}", e)))?;
                RequestBody::Metadata(req)
            }
            ApiKey::Produce => {
                let req = ProduceRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse Produce request: {}", e)))?;
                RequestBody::Produce(req)
            }
            ApiKey::Fetch => {
                let req = FetchRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse Fetch request: {}", e)))?;
                RequestBody::Fetch(req)
            }
            ApiKey::ListOffsets => {
                let req = ListOffsetsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse ListOffsets request: {}", e)))?;
                RequestBody::ListOffsets(req)
            }
            ApiKey::OffsetCommit => {
                let req = OffsetCommitRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse OffsetCommit request: {}", e)))?;
                RequestBody::OffsetCommit(req)
            }
            ApiKey::OffsetFetch => {
                let req = OffsetFetchRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse OffsetFetch request: {}", e)))?;
                RequestBody::OffsetFetch(req)
            }
            ApiKey::FindCoordinator => {
                let req = FindCoordinatorRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse FindCoordinator request: {}", e)))?;
                RequestBody::FindCoordinator(req)
            }
            ApiKey::JoinGroup => {
                let req = JoinGroupRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse JoinGroup request: {}", e)))?;
                RequestBody::JoinGroup(req)
            }
            ApiKey::Heartbeat => {
                let req = HeartbeatRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse Heartbeat request: {}", e)))?;
                RequestBody::Heartbeat(req)
            }
            ApiKey::LeaveGroup => {
                let req = LeaveGroupRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse LeaveGroup request: {}", e)))?;
                RequestBody::LeaveGroup(req)
            }
            ApiKey::SyncGroup => {
                let req = SyncGroupRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse SyncGroup request: {}", e)))?;
                RequestBody::SyncGroup(req)
            }
            ApiKey::DescribeGroups => {
                let req = DescribeGroupsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DescribeGroups request: {}", e)))?;
                RequestBody::DescribeGroups(req)
            }
            ApiKey::ListGroups => {
                let req = ListGroupsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse ListGroups request: {}", e)))?;
                RequestBody::ListGroups(req)
            }
            ApiKey::SaslHandshake => {
                let req = SaslHandshakeRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse SaslHandshake request: {}", e)))?;
                RequestBody::SaslHandshake(req)
            }
            ApiKey::SaslAuthenticate => {
                let req = SaslAuthenticateRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse SaslAuthenticate request: {}", e)))?;
                RequestBody::SaslAuthenticate(req)
            }
            ApiKey::CreateTopics => {
                let req = CreateTopicsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse CreateTopics request: {}", e)))?;
                RequestBody::CreateTopics(req)
            }
            ApiKey::DeleteTopics => {
                let req = DeleteTopicsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DeleteTopics request: {}", e)))?;
                RequestBody::DeleteTopics(req)
            }
            ApiKey::DeleteRecords => {
                let req = DeleteRecordsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DeleteRecords request: {}", e)))?;
                RequestBody::DeleteRecords(req)
            }
            ApiKey::InitProducerId => {
                let req = InitProducerIdRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse InitProducerId request: {}", e)))?;
                RequestBody::InitProducerId(req)
            }
            ApiKey::AddPartitionsToTxn => {
                let req = AddPartitionsToTxnRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse AddPartitionsToTxn request: {}", e)))?;
                RequestBody::AddPartitionsToTxn(req)
            }
            ApiKey::AddOffsetsToTxn => {
                let req = AddOffsetsToTxnRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse AddOffsetsToTxn request: {}", e)))?;
                RequestBody::AddOffsetsToTxn(req)
            }
            ApiKey::EndTxn => {
                let req = EndTxnRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse EndTxn request: {}", e)))?;
                RequestBody::EndTxn(req)
            }
            ApiKey::WriteTxnMarkers => {
                let req = WriteTxnMarkersRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse WriteTxnMarkers request: {}", e)))?;
                RequestBody::WriteTxnMarkers(req)
            }
            ApiKey::TxnOffsetCommit => {
                let req = TxnOffsetCommitRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse TxnOffsetCommit request: {}", e)))?;
                RequestBody::TxnOffsetCommit(req)
            }
            ApiKey::DescribeAcls => {
                let req = DescribeAclsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DescribeAcls request: {}", e)))?;
                RequestBody::DescribeAcls(req)
            }
            ApiKey::CreateAcls => {
                let req = CreateAclsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse CreateAcls request: {}", e)))?;
                RequestBody::CreateAcls(req)
            }
            ApiKey::DeleteAcls => {
                let req = DeleteAclsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DeleteAcls request: {}", e)))?;
                RequestBody::DeleteAcls(req)
            }
            ApiKey::DescribeConfigs => {
                let req = DescribeConfigsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DescribeConfigs request: {}", e)))?;
                RequestBody::DescribeConfigs(req)
            }
            ApiKey::AlterConfigs => {
                let req = AlterConfigsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse AlterConfigs request: {}", e)))?;
                RequestBody::AlterConfigs(req)
            }
            ApiKey::AlterReplicaLogDirs => {
                let req = AlterReplicaLogDirsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse AlterReplicaLogDirs request: {}", e)))?;
                RequestBody::AlterReplicaLogDirs(req)
            }
            ApiKey::DescribeLogDirs => {
                let req = DescribeLogDirsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DescribeLogDirs request: {}", e)))?;
                RequestBody::DescribeLogDirs(req)
            }
            ApiKey::CreatePartitions => {
                let req = CreatePartitionsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse CreatePartitions request: {}", e)))?;
                RequestBody::CreatePartitions(req)
            }
            ApiKey::CreateDelegationToken => {
                let req = CreateDelegationTokenRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse CreateDelegationToken request: {}", e)))?;
                RequestBody::CreateDelegationToken(req)
            }
            ApiKey::RenewDelegationToken => {
                let req = RenewDelegationTokenRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse RenewDelegationToken request: {}", e)))?;
                RequestBody::RenewDelegationToken(req)
            }
            ApiKey::ExpireDelegationToken => {
                let req = ExpireDelegationTokenRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse ExpireDelegationToken request: {}", e)))?;
                RequestBody::ExpireDelegationToken(req)
            }
            ApiKey::DescribeDelegationToken => {
                let req = DescribeDelegationTokenRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DescribeDelegationToken request: {}", e)))?;
                RequestBody::DescribeDelegationToken(req)
            }
            ApiKey::DeleteGroups => {
                let req = DeleteGroupsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse DeleteGroups request: {}", e)))?;
                RequestBody::DeleteGroups(req)
            }
            ApiKey::ElectLeaders => {
                let req = ElectLeadersRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse ElectLeaders request: {}", e)))?;
                RequestBody::ElectLeaders(req)
            }
            ApiKey::IncrementalAlterConfigs => {
                let req = IncrementalAlterConfigsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse IncrementalAlterConfigs request: {}", e)))?;
                RequestBody::IncrementalAlterConfigs(req)
            }
            ApiKey::AlterPartitionReassignments => {
                let req = AlterPartitionReassignmentsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse AlterPartitionReassignments request: {}", e)))?;
                RequestBody::AlterPartitionReassignments(req)
            }
            ApiKey::ListPartitionReassignments => {
                let req = ListPartitionReassignmentsRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse ListPartitionReassignments request: {}", e)))?;
                RequestBody::ListPartitionReassignments(req)
            }
            ApiKey::OffsetDelete => {
                let req = OffsetDeleteRequest::decode(&mut data, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to parse OffsetDelete request: {}", e)))?;
                RequestBody::OffsetDelete(req)
            }
            _ => {
                return Err(Error::Protocol(format!("Unsupported API key: {:?}", api_key)));
            }
        };

        Ok(Request {
            header,
            api_key,
            api_version,
            body,
        })
    }

    /// Encode a response to bytes
    pub fn encode_response(&self, response: Response, api_key: ApiKey, api_version: i16) -> Result<Bytes> {
        let mut buf = BytesMut::new();
        
        // Encode response header
        let header_version = ResponseHeader::header_version(api_key as i16, api_version);
        response.header.encode(&mut buf, header_version)
            .map_err(|e| Error::Protocol(format!("Failed to encode response header: {}", e)))?;

        // Encode response body
        match response.body {
            ResponseBody::ApiVersions(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode ApiVersions response: {}", e)))?;
            }
            ResponseBody::Metadata(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode Metadata response: {}", e)))?;
            }
            ResponseBody::Produce(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode Produce response: {}", e)))?;
            }
            ResponseBody::Fetch(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode Fetch response: {}", e)))?;
            }
            ResponseBody::ListOffsets(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode ListOffsets response: {}", e)))?;
            }
            ResponseBody::OffsetCommit(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode OffsetCommit response: {}", e)))?;
            }
            ResponseBody::OffsetFetch(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode OffsetFetch response: {}", e)))?;
            }
            ResponseBody::FindCoordinator(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode FindCoordinator response: {}", e)))?;
            }
            ResponseBody::JoinGroup(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode JoinGroup response: {}", e)))?;
            }
            ResponseBody::Heartbeat(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode Heartbeat response: {}", e)))?;
            }
            ResponseBody::LeaveGroup(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode LeaveGroup response: {}", e)))?;
            }
            ResponseBody::SyncGroup(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode SyncGroup response: {}", e)))?;
            }
            ResponseBody::DescribeGroups(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DescribeGroups response: {}", e)))?;
            }
            ResponseBody::ListGroups(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode ListGroups response: {}", e)))?;
            }
            ResponseBody::SaslHandshake(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode SaslHandshake response: {}", e)))?;
            }
            ResponseBody::SaslAuthenticate(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode SaslAuthenticate response: {}", e)))?;
            }
            ResponseBody::CreateTopics(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode CreateTopics response: {}", e)))?;
            }
            ResponseBody::DeleteTopics(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DeleteTopics response: {}", e)))?;
            }
            ResponseBody::DeleteRecords(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DeleteRecords response: {}", e)))?;
            }
            ResponseBody::InitProducerId(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode InitProducerId response: {}", e)))?;
            }
            ResponseBody::AddPartitionsToTxn(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode AddPartitionsToTxn response: {}", e)))?;
            }
            ResponseBody::AddOffsetsToTxn(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode AddOffsetsToTxn response: {}", e)))?;
            }
            ResponseBody::EndTxn(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode EndTxn response: {}", e)))?;
            }
            ResponseBody::WriteTxnMarkers(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode WriteTxnMarkers response: {}", e)))?;
            }
            ResponseBody::TxnOffsetCommit(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode TxnOffsetCommit response: {}", e)))?;
            }
            ResponseBody::DescribeAcls(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DescribeAcls response: {}", e)))?;
            }
            ResponseBody::CreateAcls(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode CreateAcls response: {}", e)))?;
            }
            ResponseBody::DeleteAcls(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DeleteAcls response: {}", e)))?;
            }
            ResponseBody::DescribeConfigs(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DescribeConfigs response: {}", e)))?;
            }
            ResponseBody::AlterConfigs(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode AlterConfigs response: {}", e)))?;
            }
            ResponseBody::AlterReplicaLogDirs(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode AlterReplicaLogDirs response: {}", e)))?;
            }
            ResponseBody::DescribeLogDirs(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DescribeLogDirs response: {}", e)))?;
            }
            ResponseBody::CreatePartitions(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode CreatePartitions response: {}", e)))?;
            }
            ResponseBody::CreateDelegationToken(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode CreateDelegationToken response: {}", e)))?;
            }
            ResponseBody::RenewDelegationToken(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode RenewDelegationToken response: {}", e)))?;
            }
            ResponseBody::ExpireDelegationToken(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode ExpireDelegationToken response: {}", e)))?;
            }
            ResponseBody::DescribeDelegationToken(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DescribeDelegationToken response: {}", e)))?;
            }
            ResponseBody::DeleteGroups(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode DeleteGroups response: {}", e)))?;
            }
            ResponseBody::ElectLeaders(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode ElectLeaders response: {}", e)))?;
            }
            ResponseBody::IncrementalAlterConfigs(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode IncrementalAlterConfigs response: {}", e)))?;
            }
            ResponseBody::AlterPartitionReassignments(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode AlterPartitionReassignments response: {}", e)))?;
            }
            ResponseBody::ListPartitionReassignments(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode ListPartitionReassignments response: {}", e)))?;
            }
            ResponseBody::OffsetDelete(resp) => {
                resp.encode(&mut buf, api_version)
                    .map_err(|e| Error::Protocol(format!("Failed to encode OffsetDelete response: {}", e)))?;
            }
        }

        Ok(buf.freeze())
    }

    /// Get supported API versions for ApiVersions response
    pub fn get_api_versions(&self) -> Vec<ApiVersion> {
        self.supported_versions
            .iter()
            .map(|(api_key, version_info)| ApiVersion {
                api_key: *api_key as i16,
                min_version: version_info.min_version,
                max_version: version_info.max_version,
            })
            .collect()
    }

    /// Check if an API version is supported
    pub fn is_supported(&self, api_key: ApiKey, version: i16) -> bool {
        if let Some(version_info) = self.supported_versions.get(&api_key) {
            version >= version_info.min_version && version <= version_info.max_version
        } else {
            false
        }
    }
}

impl Default for ProtocolParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_versions() {
        let parser = ProtocolParser::new();
        
        // Test common APIs are supported
        assert!(parser.is_supported(ApiKey::Produce, 0));
        assert!(parser.is_supported(ApiKey::Produce, 9));
        assert!(!parser.is_supported(ApiKey::Produce, 10));
        
        assert!(parser.is_supported(ApiKey::Fetch, 0));
        assert!(parser.is_supported(ApiKey::Fetch, 13));
        assert!(!parser.is_supported(ApiKey::Fetch, 14));
        
        assert!(parser.is_supported(ApiKey::ApiVersions, 0));
        assert!(parser.is_supported(ApiKey::ApiVersions, 3));
        assert!(!parser.is_supported(ApiKey::ApiVersions, 4));
    }

    #[test]
    fn test_get_api_versions() {
        let parser = ProtocolParser::new();
        let versions = parser.get_api_versions();
        
        // Should have all supported APIs
        assert!(!versions.is_empty());
        
        // Check a few key APIs
        let produce_version = versions.iter().find(|v| v.api_key == ApiKey::Produce as i16);
        assert!(produce_version.is_some());
        let produce_version = produce_version.unwrap();
        assert_eq!(produce_version.min_version, 0);
        assert_eq!(produce_version.max_version, 9);
    }
}