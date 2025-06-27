# Chronik CLI Batch Operations

The Chronik CLI now supports batch operations, allowing you to execute multiple administrative tasks from a single YAML file. This is particularly useful for:

- Initial cluster setup
- Environment provisioning
- Disaster recovery
- Automated deployment pipelines

## Features

### 1. YAML-based Batch Files

Define multiple operations in a structured YAML format:

```yaml
version: "1.0"
description: "Production environment setup"
operations:
  - type: create_topic
    name: events-stream
    partitions: 10
    replication_factor: 3
    config:
      retention_ms: 604800000  # 7 days
      segment_bytes: 1073741824  # 1GB
  
  - type: create_user
    username: app-service
    password: secure-password
    roles:
      - producer
      - consumer
```

### 2. Supported Operations

- **create_topic**: Create a new topic with configuration
- **delete_topic**: Delete an existing topic
- **create_user**: Create a new user with roles
- **delete_user**: Delete an existing user
- **update_topic_config**: Update topic configuration
- **create_consumer_group**: Create a consumer group

### 3. Execution Modes

#### Dry Run Mode
Validate operations without executing them:
```bash
chronik-ctl batch execute batch.yaml --dry-run
```

#### Continue on Error
Continue executing remaining operations even if one fails:
```bash
chronik-ctl batch execute batch.yaml --continue-on-error
```

### 4. Output Formats

All CLI commands now support multiple output formats:

```bash
# Table format (default)
chronik-ctl topic list

# JSON format
chronik-ctl topic list --output json

# YAML format
chronik-ctl topic list --output yaml
```

## Usage Examples

### Generate Example Batch File
```bash
chronik-ctl batch example --output example-batch.yaml
```

### Execute Batch Operations
```bash
# Execute with default settings
chronik-ctl batch execute production-setup.yaml

# Execute with detailed JSON output
chronik-ctl batch execute production-setup.yaml --output json

# Dry run with YAML output
chronik-ctl batch execute production-setup.yaml --dry-run --output yaml
```

### Real-World Example: Multi-Environment Setup

```yaml
version: "1.0"
description: "Setup development, staging, and production topics"
operations:
  # Development topics
  - type: create_topic
    name: dev-events
    partitions: 3
    replication_factor: 1
    config:
      retention_ms: 86400000  # 1 day
      segment_bytes: 104857600  # 100MB
  
  # Staging topics
  - type: create_topic
    name: staging-events
    partitions: 5
    replication_factor: 2
    config:
      retention_ms: 259200000  # 3 days
      segment_bytes: 536870912  # 512MB
  
  # Production topics
  - type: create_topic
    name: prod-events
    partitions: 20
    replication_factor: 3
    config:
      retention_ms: 2592000000  # 30 days
      segment_bytes: 1073741824  # 1GB
      min_insync_replicas: 2
      compression_type: snappy
  
  # Create service accounts
  - type: create_user
    username: dev-service
    password: dev-pass-123
    roles: [producer, consumer]
  
  - type: create_user
    username: prod-service
    password: prod-pass-456
    roles: [producer, consumer, admin]
  
  # Setup consumer groups
  - type: create_consumer_group
    group_id: dev-analytics
    topics: [dev-events]
  
  - type: create_consumer_group
    group_id: prod-analytics
    topics: [prod-events]
```

## Error Handling

The batch executor provides detailed error reporting:

1. **Operation-level errors**: Each operation reports success or failure
2. **Summary statistics**: Total operations, successful, and failed counts
3. **Detailed error messages**: Full error context for debugging

Example output:
```
[1/5] Executing: Create topic 'events-stream'
  ✓ Topic 'events-stream' created
[2/5] Executing: Create topic 'logs-stream'
  ✗ Error: Topic already exists
[3/5] Executing: Create user 'app-service'
  ✓ User 'app-service' created
...

============================================================
Batch execution completed: 4 successful, 1 failed
```

## Best Practices

1. **Use dry-run mode** before executing in production
2. **Version control** your batch files
3. **Include descriptions** for documentation
4. **Use meaningful names** for resources
5. **Set appropriate security** for files containing passwords
6. **Test in lower environments** before production

## Integration with CI/CD

The batch operations feature integrates well with CI/CD pipelines:

```bash
# GitLab CI example
deploy:
  script:
    - chronik-ctl batch execute deploy/${CI_ENVIRONMENT_NAME}.yaml --dry-run
    - chronik-ctl batch execute deploy/${CI_ENVIRONMENT_NAME}.yaml
  only:
    - master
```

## Environment Variables

The CLI supports environment variables for configuration:

```bash
export CHRONIK_ADMIN_URL=https://chronik-admin.example.com
export CHRONIK_API_TOKEN=your-secure-token

# Now run without specifying URL and token
chronik-ctl batch execute setup.yaml
```