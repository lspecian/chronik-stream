# Chronik Stream CLI

Command-line interface for managing Chronik Stream clusters.

## Installation

```bash
cargo install --path crates/chronik-cli
```

## Usage

### Authentication

```bash
# Login to get an access token
chronik-ctl auth login -u admin

# Set token as environment variable
export CHRONIK_API_TOKEN="<token>"
```

### Cluster Management

```bash
# Show cluster information
chronik-ctl cluster info

# Check cluster health
chronik-ctl cluster health

# Show cluster metrics
chronik-ctl cluster metrics
```

### Topic Management

```bash
# List topics
chronik-ctl topic list

# Create a topic
chronik-ctl topic create my-topic -p 6 -r 3

# Show topic details
chronik-ctl topic get my-topic

# Delete a topic
chronik-ctl topic delete my-topic --yes
```

### Broker Management

```bash
# List brokers
chronik-ctl broker list

# Show broker details
chronik-ctl broker get 1
```

### Consumer Group Management

```bash
# List consumer groups
chronik-ctl group list

# Show group details
chronik-ctl group get my-group

# Show group offsets
chronik-ctl group offsets my-group

# Delete a group
chronik-ctl group delete my-group --yes
```

## Output Formats

The CLI supports multiple output formats:

```bash
# Table format (default)
chronik-ctl topic list

# JSON format
chronik-ctl topic list -o json

# YAML format
chronik-ctl topic list -o yaml
```

## Environment Variables

- `CHRONIK_ADMIN_URL`: Admin API endpoint (default: http://localhost:8080)
- `CHRONIK_API_TOKEN`: API authentication token