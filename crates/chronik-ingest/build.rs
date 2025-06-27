fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Always skip proto compilation for now since proto file doesn't exist
    println!("cargo:warning=Skipping proto compilation - proto file not found");
    Ok(())
}