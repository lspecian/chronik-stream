# Dynamic Configuration System

Chronik Stream now includes a comprehensive dynamic configuration management system that allows runtime updates without service restarts. This document describes the features and usage of the configuration system.

## Overview

The dynamic configuration system provides:

- **Runtime Updates**: Change configuration values without restarting services
- **Multiple Sources**: Load configuration from files, environment variables, and APIs
- **File Watching**: Automatically reload configuration when files change
- **Validation**: Schema-based validation with custom validators
- **Change Notifications**: Subscribe to configuration changes
- **Multi-format Support**: YAML, JSON, TOML configuration files

## Architecture

```
┌─────────────────┐     ┌──────────────┐     ┌────────────┐
│ ConfigManager   │────▶│ Providers    │────▶│ Sources    │
│                 │     │              │     │            │
│ - Load/Reload   │     │ - File       │     │ - YAML     │
│ - Get/Set       │     │ - Env        │     │ - JSON     │
│ - Subscribe     │     │ - API        │     │ - TOML     │
└─────────────────┘     └──────────────┘     └────────────┘
         │
         ▼
┌─────────────────┐     ┌──────────────┐
│ Validators      │     │ Watchers     │
│                 │     │              │
│ - Schema        │     │ - File       │
│ - Range         │     │ - Directory  │
│ - Pattern       │     │              │
└─────────────────┘     └──────────────┘
```

## Configuration Sources

### 1. File Provider

Load configuration from YAML, JSON, or TOML files:

```rust
let provider = FileProvider::new("config/chronik.yaml", priority: 10)?;
manager.add_provider(Box::new(provider));
```

### 2. Environment Provider

Override configuration with environment variables:

```rust
let provider = EnvironmentProvider::new("CHRONIK", priority: 20)
    .with_separator("__");
manager.add_provider(Box::new(provider));
```

Environment variable mapping:
- `CHRONIK__SERVER__PORT=9000` → `server.port = 9000`
- `CHRONIK__FEATURES__SEARCH_ENABLED=true` → `features.search_enabled = true`

### 3. Priority System

Higher priority values override lower ones:
- Default config file: priority 0
- User config file: priority 10
- Environment variables: priority 20
- Runtime updates: priority 30

## Schema Validation

Define configuration schemas with validation rules:

```rust
let schema = Schema::new()
    .field("server", FieldSchema::new(FieldType::Object(
        Schema::new()
            .field("port", FieldSchema::new(FieldType::Integer)
                .required()
                .validator(Box::new(RangeValidator::new()
                    .min(1.0)
                    .max(65535.0))))
    )));
```

### Built-in Validators

1. **RangeValidator**: Validate numeric ranges
2. **PatternValidator**: Validate string patterns with regex
3. **Custom validators**: Implement the `FieldValidator` trait

## Runtime Updates

Update configuration values at runtime:

```rust
// Update a single value
manager.set("server.port", ConfigValue::Integer(9000)).await?;

// Update nested values
manager.set("features.search.enabled", ConfigValue::Bool(true)).await?;

// Updates are validated before applying
manager.set("server.port", ConfigValue::Integer(70000)).await
    .expect_err("Port out of range");
```

## Change Notifications

Subscribe to configuration changes:

```rust
let mut receiver = manager.subscribe();

tokio::spawn(async move {
    while let Ok(update) = receiver.recv().await {
        println!("Config updated: {} = {:?}", 
            update.path, update.new_value);
        
        // React to specific changes
        match update.path.as_str() {
            "server.port" => restart_server(),
            "features.rate_limit" => update_rate_limiter(),
            _ => {}
        }
    }
});
```

## File Watching

Automatically reload configuration when files change:

```rust
// Watch a configuration file
manager.watch_file("config/chronik.yaml").await?;

// Changes to the file trigger automatic reload
// with validation and change notifications
```

## Usage Examples

### Basic Configuration

```yaml
# config/chronik.yaml
server:
  host: localhost
  port: 8080
  workers: 4

storage:
  backend: s3
  bucket: chronik-data
  cache_size_mb: 256

features:
  search_enabled: true
  geo_queries: false
  rate_limit: 1000
```

### Service Integration

```rust
use chronik_config::{ConfigManager, ConfigUpdate};

struct MyService {
    config: Arc<ConfigManager>,
    // ... other fields
}

impl MyService {
    async fn new() -> Result<Self> {
        let mut manager = ConfigManager::new();
        
        // Setup providers and validators
        manager.add_provider(Box::new(
            FileProvider::new("config/service.yaml", 10)?
        ));
        
        // Load initial configuration
        manager.load().await?;
        
        // Setup change listener
        let config_clone = manager.clone();
        tokio::spawn(async move {
            let mut receiver = config_clone.subscribe();
            while let Ok(update) = receiver.recv().await {
                Self::handle_config_change(update).await;
            }
        });
        
        Ok(Self {
            config: Arc::new(manager),
        })
    }
    
    fn get_port(&self) -> u16 {
        self.config.get_i64("server.port")
            .unwrap_or(8080) as u16
    }
}
```

### Dynamic Feature Flags

```rust
// Check feature flags
if config.get_bool("features.new_algorithm").unwrap_or(false) {
    use_new_algorithm();
} else {
    use_legacy_algorithm();
}

// Update feature flag at runtime
config.set("features.new_algorithm", ConfigValue::Bool(true)).await?;
```

### Configuration Hot Reload

```rust
// Setup automatic reload on SIGHUP
tokio::spawn(async move {
    let mut signal = unix::signal(SignalKind::hangup())?;
    while signal.recv().await.is_some() {
        info!("Received SIGHUP, reloading configuration");
        if let Err(e) = config_manager.reload().await {
            error!("Failed to reload config: {}", e);
        }
    }
});
```

## Best Practices

1. **Validation**: Always define schemas for your configuration
2. **Defaults**: Provide sensible defaults for all configuration values
3. **Documentation**: Document all configuration options
4. **Gradual Rollout**: Use feature flags for gradual rollouts
5. **Monitoring**: Log configuration changes for audit trails
6. **Testing**: Test configuration changes in lower environments first

## Security Considerations

1. **Sensitive Data**: Use environment variables for secrets
2. **Access Control**: Restrict who can modify configuration
3. **Audit Logging**: Log all configuration changes
4. **Encryption**: Encrypt sensitive configuration files at rest
5. **Validation**: Validate all inputs to prevent injection attacks

## Performance Impact

The configuration system is designed for minimal performance impact:

- **Caching**: Frequently accessed values are cached
- **Copy-on-Write**: Configuration updates use Arc for efficiency
- **Async Operations**: All I/O operations are asynchronous
- **Debouncing**: File changes are debounced to prevent rapid reloads

## Integration with Kubernetes

Use ConfigMaps and mounted volumes for dynamic updates:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: chronik-config
data:
  chronik.yaml: |
    server:
      port: 8080
    features:
      search_enabled: true
---
apiVersion: apps/v1
kind: Deployment
spec:
  template:
    spec:
      containers:
      - name: chronik
        volumeMounts:
        - name: config
          mountPath: /config
      volumes:
      - name: config
        configMap:
          name: chronik-config
```

The service watches `/config/chronik.yaml` and automatically reloads when Kubernetes updates the ConfigMap.