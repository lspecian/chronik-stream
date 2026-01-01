// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.
// Vendored and modified for prost 0.12 compatibility.

fn main() {
    let mut config = prost_build::Config::new();
    // Use bytes for the data fields
    config.bytes(["."]);
    config
        .compile_protos(&["proto/eraftpb.proto"], &["proto/"])
        .expect("Failed to compile protos");
}
