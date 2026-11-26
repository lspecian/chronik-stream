//! Build script for generating Rust code from protobuf definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/catalog_rpc.proto"], &["proto"])?;

    // Tell Cargo to recompile if proto files change
    println!("cargo:rerun-if-changed=proto/catalog_rpc.proto");

    Ok(())
}
