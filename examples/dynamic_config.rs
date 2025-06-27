//! Example demonstrating dynamic configuration updates in Chronik Stream

use chronik_config::{
    ConfigManager, ConfigUpdate,
    provider::{FileProvider, EnvironmentProvider},
    validation::{SchemaValidator, Schema, FieldSchema, FieldType, RangeValidator},
    types::ConfigValue,
};
use std::{collections::HashMap, time::Duration};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt::init();
    
    println!("=== Chronik Stream Dynamic Configuration Demo ===\n");
    
    // Create configuration manager
    let mut manager = ConfigManager::new();
    
    // Add file provider for base configuration
    println!("1. Setting up configuration providers...");
    let file_provider = FileProvider::new("config/chronik.yaml", 10)?;
    manager.add_provider(Box::new(file_provider));
    
    // Add environment provider for overrides
    let env_provider = EnvironmentProvider::new("CHRONIK", 20)
        .with_separator("__");
    manager.add_provider(Box::new(env_provider));
    
    // Define configuration schema
    println!("2. Defining configuration schema...");
    let schema = Schema::new()
        .field("server", FieldSchema::new(FieldType::Object(
            Schema::new()
                .field("host", FieldSchema::new(FieldType::String).required())
                .field("port", FieldSchema::new(FieldType::Integer)
                    .required()
                    .validator(Box::new(RangeValidator::new().min(1.0).max(65535.0))))
                .field("workers", FieldSchema::new(FieldType::Integer)
                    .validator(Box::new(RangeValidator::new().min(1.0).max(256.0))))
        )).required())
        .field("storage", FieldSchema::new(FieldType::Object(
            Schema::new()
                .field("backend", FieldSchema::new(FieldType::String).required())
                .field("path", FieldSchema::new(FieldType::String))
                .field("cache_size_mb", FieldSchema::new(FieldType::Integer)
                    .validator(Box::new(RangeValidator::new().min(0.0))))
        )).required())
        .field("features", FieldSchema::new(FieldType::Object(
            Schema::new()
                .field("search_enabled", FieldSchema::new(FieldType::Bool))
                .field("geo_queries", FieldSchema::new(FieldType::Bool))
                .field("rate_limit", FieldSchema::new(FieldType::Integer))
        )));
    
    manager.add_validator(Box::new(SchemaValidator::new(schema)));
    
    // Load initial configuration
    println!("3. Loading initial configuration...");
    manager.load().await?;
    
    // Display current configuration
    display_config(&manager);
    
    // Subscribe to configuration updates
    println!("\n4. Setting up configuration update listener...");
    let mut receiver = manager.subscribe();
    
    // Spawn task to listen for updates
    tokio::spawn(async move {
        while let Ok(update) = receiver.recv().await {
            println!("\n📢 Configuration Update Detected!");
            println!("   Path: {}", update.path);
            println!("   Old Value: {:?}", update.old_value);
            println!("   New Value: {:?}", update.new_value);
            println!("   Timestamp: {}", update.timestamp);
        }
    });
    
    // Demonstrate runtime updates
    println!("\n5. Demonstrating runtime configuration updates...");
    
    // Update server port
    println!("\n   a) Updating server port to 9000...");
    manager.set("server.port", ConfigValue::Integer(9000)).await?;
    sleep(Duration::from_millis(100)).await;
    
    // Update storage cache size
    println!("\n   b) Updating storage cache size to 512 MB...");
    manager.set("storage.cache_size_mb", ConfigValue::Integer(512)).await?;
    sleep(Duration::from_millis(100)).await;
    
    // Enable new feature
    println!("\n   c) Enabling geo queries feature...");
    manager.set("features.geo_queries", ConfigValue::Bool(true)).await?;
    sleep(Duration::from_millis(100)).await;
    
    // Try invalid update (should fail validation)
    println!("\n   d) Attempting invalid update (port > 65535)...");
    match manager.set("server.port", ConfigValue::Integer(70000)).await {
        Ok(_) => println!("      ✗ Unexpected success"),
        Err(e) => println!("      ✓ Validation correctly failed: {}", e),
    }
    
    // Display final configuration
    println!("\n6. Final configuration state:");
    display_config(&manager);
    
    // Demonstrate file watching
    println!("\n7. File watching demo:");
    println!("   To see file watching in action:");
    println!("   1. Create config/chronik.yaml");
    println!("   2. Modify the file while this program is running");
    println!("   3. Configuration will automatically reload");
    
    // Export configuration
    println!("\n8. Exporting configuration...");
    let exported = manager.export();
    println!("   Exported configuration to ConfigValue");
    
    // Show metadata
    let metadata = manager.metadata().await;
    println!("\n9. Configuration metadata:");
    println!("   Loaded at: {}", metadata.loaded_at);
    println!("   Sources: {:?}", metadata.sources);
    
    println!("\n✅ Demo completed successfully!");
    
    Ok(())
}

fn display_config(manager: &ConfigManager) {
    println!("\n   Current Configuration:");
    println!("   ├─ server:");
    if let Some(host) = manager.get_string("server.host") {
        println!("   │  ├─ host: {}", host);
    }
    if let Some(port) = manager.get_i64("server.port") {
        println!("   │  ├─ port: {}", port);
    }
    if let Some(workers) = manager.get_i64("server.workers") {
        println!("   │  └─ workers: {}", workers);
    }
    
    println!("   ├─ storage:");
    if let Some(backend) = manager.get_string("storage.backend") {
        println!("   │  ├─ backend: {}", backend);
    }
    if let Some(path) = manager.get_string("storage.path") {
        println!("   │  ├─ path: {}", path);
    }
    if let Some(cache) = manager.get_i64("storage.cache_size_mb") {
        println!("   │  └─ cache_size_mb: {}", cache);
    }
    
    println!("   └─ features:");
    if let Some(search) = manager.get_bool("features.search_enabled") {
        println!("      ├─ search_enabled: {}", search);
    }
    if let Some(geo) = manager.get_bool("features.geo_queries") {
        println!("      ├─ geo_queries: {}", geo);
    }
    if let Some(rate) = manager.get_i64("features.rate_limit") {
        println!("      └─ rate_limit: {}", rate);
    }
}