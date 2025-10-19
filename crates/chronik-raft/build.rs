//! Build script for generating Rust code from protobuf definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile(&["proto/raft_rpc.proto"], &["proto"])?;

    // Tell Cargo to recompile if proto files change
    println!("cargo:rerun-if-changed=proto/raft_rpc.proto");

    Ok(())
}
