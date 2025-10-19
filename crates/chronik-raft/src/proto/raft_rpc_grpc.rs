// This file is generated. Do not edit
// @generated

// https://github.com/Manishearth/rust-clippy/issues/702
#![allow(unknown_lints)]
#![allow(clippy::all)]

#![allow(box_pointers)]
#![allow(dead_code)]
#![allow(missing_docs)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(trivial_casts)]
#![allow(unsafe_code)]
#![allow(unused_imports)]
#![allow(unused_results)]

const METHOD_RAFT_SERVICE_APPEND_ENTRIES: ::grpcio::Method<super::raft_rpc::AppendEntriesRequest, super::raft_rpc::AppendEntriesResponse> = ::grpcio::Method {
    ty: ::grpcio::MethodType::Unary,
    name: "/chronik.raft.RaftService/AppendEntries",
    req_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
    resp_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
};

const METHOD_RAFT_SERVICE_REQUEST_VOTE: ::grpcio::Method<super::raft_rpc::RequestVoteRequest, super::raft_rpc::RequestVoteResponse> = ::grpcio::Method {
    ty: ::grpcio::MethodType::Unary,
    name: "/chronik.raft.RaftService/RequestVote",
    req_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
    resp_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
};

const METHOD_RAFT_SERVICE_INSTALL_SNAPSHOT: ::grpcio::Method<super::raft_rpc::InstallSnapshotRequest, super::raft_rpc::InstallSnapshotResponse> = ::grpcio::Method {
    ty: ::grpcio::MethodType::ClientStreaming,
    name: "/chronik.raft.RaftService/InstallSnapshot",
    req_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
    resp_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
};

const METHOD_RAFT_SERVICE_READ_INDEX: ::grpcio::Method<super::raft_rpc::ReadIndexRequest, super::raft_rpc::ReadIndexResponse> = ::grpcio::Method {
    ty: ::grpcio::MethodType::Unary,
    name: "/chronik.raft.RaftService/ReadIndex",
    req_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
    resp_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
};

const METHOD_RAFT_SERVICE_STEP: ::grpcio::Method<super::raft_rpc::RaftMessage, super::raft_rpc::StepResponse> = ::grpcio::Method {
    ty: ::grpcio::MethodType::Unary,
    name: "/chronik.raft.RaftService/Step",
    req_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
    resp_mar: ::grpcio::Marshaller { ser: ::grpcio::pb_ser, de: ::grpcio::pb_de },
};

#[derive(Clone)]
pub struct RaftServiceClient {
    client: ::grpcio::Client,
}

impl RaftServiceClient {
    pub fn new(channel: ::grpcio::Channel) -> Self {
        RaftServiceClient {
            client: ::grpcio::Client::new(channel),
        }
    }

    pub fn append_entries_opt(&self, req: &super::raft_rpc::AppendEntriesRequest, opt: ::grpcio::CallOption) -> ::grpcio::Result<super::raft_rpc::AppendEntriesResponse> {
        self.client.unary_call(&METHOD_RAFT_SERVICE_APPEND_ENTRIES, req, opt)
    }

    pub fn append_entries(&self, req: &super::raft_rpc::AppendEntriesRequest) -> ::grpcio::Result<super::raft_rpc::AppendEntriesResponse> {
        self.append_entries_opt(req, ::grpcio::CallOption::default())
    }

    pub fn append_entries_async_opt(&self, req: &super::raft_rpc::AppendEntriesRequest, opt: ::grpcio::CallOption) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::AppendEntriesResponse>> {
        self.client.unary_call_async(&METHOD_RAFT_SERVICE_APPEND_ENTRIES, req, opt)
    }

    pub fn append_entries_async(&self, req: &super::raft_rpc::AppendEntriesRequest) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::AppendEntriesResponse>> {
        self.append_entries_async_opt(req, ::grpcio::CallOption::default())
    }

    pub fn request_vote_opt(&self, req: &super::raft_rpc::RequestVoteRequest, opt: ::grpcio::CallOption) -> ::grpcio::Result<super::raft_rpc::RequestVoteResponse> {
        self.client.unary_call(&METHOD_RAFT_SERVICE_REQUEST_VOTE, req, opt)
    }

    pub fn request_vote(&self, req: &super::raft_rpc::RequestVoteRequest) -> ::grpcio::Result<super::raft_rpc::RequestVoteResponse> {
        self.request_vote_opt(req, ::grpcio::CallOption::default())
    }

    pub fn request_vote_async_opt(&self, req: &super::raft_rpc::RequestVoteRequest, opt: ::grpcio::CallOption) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::RequestVoteResponse>> {
        self.client.unary_call_async(&METHOD_RAFT_SERVICE_REQUEST_VOTE, req, opt)
    }

    pub fn request_vote_async(&self, req: &super::raft_rpc::RequestVoteRequest) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::RequestVoteResponse>> {
        self.request_vote_async_opt(req, ::grpcio::CallOption::default())
    }

    pub fn install_snapshot_opt(&self, opt: ::grpcio::CallOption) -> ::grpcio::Result<(::grpcio::ClientCStreamSender<super::raft_rpc::InstallSnapshotRequest>, ::grpcio::ClientCStreamReceiver<super::raft_rpc::InstallSnapshotResponse>)> {
        self.client.client_streaming(&METHOD_RAFT_SERVICE_INSTALL_SNAPSHOT, opt)
    }

    pub fn install_snapshot(&self) -> ::grpcio::Result<(::grpcio::ClientCStreamSender<super::raft_rpc::InstallSnapshotRequest>, ::grpcio::ClientCStreamReceiver<super::raft_rpc::InstallSnapshotResponse>)> {
        self.install_snapshot_opt(::grpcio::CallOption::default())
    }

    pub fn read_index_opt(&self, req: &super::raft_rpc::ReadIndexRequest, opt: ::grpcio::CallOption) -> ::grpcio::Result<super::raft_rpc::ReadIndexResponse> {
        self.client.unary_call(&METHOD_RAFT_SERVICE_READ_INDEX, req, opt)
    }

    pub fn read_index(&self, req: &super::raft_rpc::ReadIndexRequest) -> ::grpcio::Result<super::raft_rpc::ReadIndexResponse> {
        self.read_index_opt(req, ::grpcio::CallOption::default())
    }

    pub fn read_index_async_opt(&self, req: &super::raft_rpc::ReadIndexRequest, opt: ::grpcio::CallOption) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::ReadIndexResponse>> {
        self.client.unary_call_async(&METHOD_RAFT_SERVICE_READ_INDEX, req, opt)
    }

    pub fn read_index_async(&self, req: &super::raft_rpc::ReadIndexRequest) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::ReadIndexResponse>> {
        self.read_index_async_opt(req, ::grpcio::CallOption::default())
    }

    pub fn step_opt(&self, req: &super::raft_rpc::RaftMessage, opt: ::grpcio::CallOption) -> ::grpcio::Result<super::raft_rpc::StepResponse> {
        self.client.unary_call(&METHOD_RAFT_SERVICE_STEP, req, opt)
    }

    pub fn step(&self, req: &super::raft_rpc::RaftMessage) -> ::grpcio::Result<super::raft_rpc::StepResponse> {
        self.step_opt(req, ::grpcio::CallOption::default())
    }

    pub fn step_async_opt(&self, req: &super::raft_rpc::RaftMessage, opt: ::grpcio::CallOption) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::StepResponse>> {
        self.client.unary_call_async(&METHOD_RAFT_SERVICE_STEP, req, opt)
    }

    pub fn step_async(&self, req: &super::raft_rpc::RaftMessage) -> ::grpcio::Result<::grpcio::ClientUnaryReceiver<super::raft_rpc::StepResponse>> {
        self.step_async_opt(req, ::grpcio::CallOption::default())
    }
    pub fn spawn<F>(&self, f: F) where F: ::futures::Future<Output = ()> + Send + 'static {
        self.client.spawn(f)
    }
}

pub trait RaftService {
    fn append_entries(&mut self, ctx: ::grpcio::RpcContext, req: super::raft_rpc::AppendEntriesRequest, sink: ::grpcio::UnarySink<super::raft_rpc::AppendEntriesResponse>);
    fn request_vote(&mut self, ctx: ::grpcio::RpcContext, req: super::raft_rpc::RequestVoteRequest, sink: ::grpcio::UnarySink<super::raft_rpc::RequestVoteResponse>);
    fn install_snapshot(&mut self, ctx: ::grpcio::RpcContext, stream: ::grpcio::RequestStream<super::raft_rpc::InstallSnapshotRequest>, sink: ::grpcio::ClientStreamingSink<super::raft_rpc::InstallSnapshotResponse>);
    fn read_index(&mut self, ctx: ::grpcio::RpcContext, req: super::raft_rpc::ReadIndexRequest, sink: ::grpcio::UnarySink<super::raft_rpc::ReadIndexResponse>);
    fn step(&mut self, ctx: ::grpcio::RpcContext, req: super::raft_rpc::RaftMessage, sink: ::grpcio::UnarySink<super::raft_rpc::StepResponse>);
}

pub fn create_raft_service<S: RaftService + Send + Clone + 'static>(s: S) -> ::grpcio::Service {
    let mut builder = ::grpcio::ServiceBuilder::new();
    let mut instance = s.clone();
    builder = builder.add_unary_handler(&METHOD_RAFT_SERVICE_APPEND_ENTRIES, move |ctx, req, resp| {
        instance.append_entries(ctx, req, resp)
    });
    let mut instance = s.clone();
    builder = builder.add_unary_handler(&METHOD_RAFT_SERVICE_REQUEST_VOTE, move |ctx, req, resp| {
        instance.request_vote(ctx, req, resp)
    });
    let mut instance = s.clone();
    builder = builder.add_client_streaming_handler(&METHOD_RAFT_SERVICE_INSTALL_SNAPSHOT, move |ctx, req, resp| {
        instance.install_snapshot(ctx, req, resp)
    });
    let mut instance = s.clone();
    builder = builder.add_unary_handler(&METHOD_RAFT_SERVICE_READ_INDEX, move |ctx, req, resp| {
        instance.read_index(ctx, req, resp)
    });
    let mut instance = s;
    builder = builder.add_unary_handler(&METHOD_RAFT_SERVICE_STEP, move |ctx, req, resp| {
        instance.step(ctx, req, resp)
    });
    builder.build()
}
