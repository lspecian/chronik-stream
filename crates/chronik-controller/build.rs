fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Always create empty proto module for now to avoid protoc dependency issues
    std::fs::create_dir_all("src/proto").ok();
    std::fs::write("src/proto/mod.rs", "// Proto definitions will be generated here when protoc is available").ok();
    
    // Check if protoc is available
    if std::process::Command::new("protoc").arg("--version").output().is_err() {
        println!("cargo:warning=protoc not found, skipping proto compilation");
        return Ok(());
    }
    
    // Check if proto file exists
    let proto_path = "../../proto/controller.proto";
    if !std::path::Path::new(proto_path).exists() {
        println!("cargo:warning=Proto file not found at {}, skipping compilation", proto_path);
        return Ok(());
    }
    
    tonic_build::compile_protos(proto_path)?;
    Ok(())
}