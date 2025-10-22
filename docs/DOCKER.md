# Docker Setup for Chronik Stream

## Overview

Chronik Stream uses a unified `chronik-server` binary that can run in multiple modes. We maintain a minimal set of Docker configurations for simplicity.

## Docker Files

### Production Files

| File | Purpose | Usage |
|------|---------|-------|
| `Dockerfile` | Main production image | Builds the `chronik-server` binary in a multi-stage build |
| `docker-compose.yml` | Standard deployment | Run Chronik Server with persistent storage |
| `Cargo.docker.toml` | Docker-specific Cargo config | Excludes test crates to speed up Docker builds |
| `.dockerignore` | Build optimization | Excludes unnecessary files from Docker context |

### Testing Files

| File | Purpose | Usage |
|------|---------|-------|
| `test_docker.sh` | Quick Docker test | Tests the built image locally |
| `tests/Dockerfile.client-tests` | Client compatibility testing | Tests various Kafka clients against Chronik |
| `tests/kafka-test-compose.yml` | Real Kafka comparison | Runs actual Kafka for comparison testing |

## Quick Start

### 1. Build the Image

```bash
# Build for local architecture
docker build -t chronik-server:latest .

# Build multi-arch (AMD64 + ARM64)
docker buildx build --platform linux/amd64,linux/arm64 -t chronik-server:latest .
```

### 2. Run with Docker

```bash
# Quick start
docker run -d -p 9092:9092 chronik-server:latest

# With persistent storage
docker run -d \
  --name chronik \
  -p 9092:9092 \
  -v chronik-data:/data \
  chronik-server:latest
```

### 3. Run with Docker Compose

```bash
# Start the service
docker-compose up -d

# View logs
docker-compose logs -f

# Stop the service
docker-compose down
```

## Operational Modes

The Docker image supports all chronik-server operational modes:

```bash
# Standalone mode (default)
docker run chronik-server:latest

# Explicitly set mode
docker run chronik-server:latest chronik-server standalone

# All components mode
docker run chronik-server:latest chronik-server all

# Future: Distributed modes
docker run chronik-server:latest chronik-server ingest --controller-url <url>
docker run chronik-server:latest chronik-server search --storage-url <url>
```

## Environment Variables

Configure the server using environment variables:

```bash
docker run -d \
  -e RUST_LOG=info \
  -e CHRONIK_KAFKA_PORT=9092 \
  -e CHRONIK_ADMIN_PORT=3000 \
  -e CHRONIK_DATA_DIR=/data \
  -e CHRONIK_BIND_ADDR=0.0.0.0 \
  chronik-server:latest
```

## Testing

### Test the Docker Image

```bash
# Run the test script
./test_docker.sh
```

### Test Client Compatibility

```bash
# Build the client test image
docker build -f tests/Dockerfile.client-tests -t chronik-client-tests .

# Run client tests
docker run --network host chronik-client-tests
```

### Compare with Real Kafka

```bash
# Start real Kafka
docker-compose -f tests/kafka-test-compose.yml up -d

# Run comparison tests
# (Kafka on port 9095, Chronik on port 9092)
```

## Docker Image Details

- **Base Image**: `debian:bookworm-slim` (minimal size)
- **Multi-stage Build**: Separates build and runtime
- **Non-root User**: Runs as `chronik` user for security
- **Final Size**: ~50-60MB (excluding binary)
- **Architectures**: AMD64, ARM64

## Publishing

See [DOCKER_PUBLISH.md](DOCKER_PUBLISH.md) for detailed publishing instructions.

## Troubleshooting

### Build Issues

```bash
# Clean Docker cache
docker system prune -a

# Build with progress output
DOCKER_BUILDKIT=1 docker build --progress=plain -t chronik-server:latest .

# Check build logs
docker build -t chronik-server:latest . 2>&1 | tee build.log
```

### Runtime Issues

```bash
# Check container logs
docker logs <container-id>

# Debug inside container
docker run -it --entrypoint /bin/bash chronik-server:latest

# Check resource usage
docker stats <container-id>
```

### Network Issues

```bash
# Test port accessibility
nc -zv localhost 9092

# Check container network
docker inspect <container-id> | grep -A 10 NetworkMode

# Test from another container
docker run --rm --network container:<container-id> alpine nc -zv localhost 9092
```

## Migration from Old Images

If you were using the old `chronik` or `chronik-ingest` images:

1. **Old**: `docker run chronik`
   **New**: `docker run chronik-server:latest`

2. **Old**: `docker run chronik-ingest`
   **New**: `docker run chronik-server:latest standalone`

3. **Old**: Multiple containers for different components
   **New**: Single container with `chronik-server all`

## Development

### Local Development with Docker

```bash
# Mount source code for development
docker run -it \
  -v $(pwd):/workspace \
  -w /workspace \
  rust:latest \
  cargo build --bin chronik-server
```

### Running Tests in Docker

```bash
# Build test image
docker build -f Dockerfile --target builder -t chronik-test .

# Run tests
docker run chronik-test cargo test
```

## Security

- Images run as non-root user
- Minimal base image (debian:bookworm-slim)
- No unnecessary packages installed
- Regular security scanning recommended:
  ```bash
  docker scout cves chronik-server:latest
  ```

## Support

For Docker-specific issues:
- Check the [troubleshooting section](#troubleshooting)
- Review [DOCKER_PUBLISH.md](DOCKER_PUBLISH.md)
- Open an issue on GitHub