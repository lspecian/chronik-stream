# Docker Build Instructions for v1.3.12

## Quick Build (Local Testing)

### Option 1: Multi-arch build with pre-built binaries

```bash
# Build the release binaries
cargo build --release --bin chronik-server

# Create artifacts directory structure
mkdir -p artifacts/linux/amd64 artifacts/linux/arm64

# Copy binary to appropriate architecture
# For x86_64 (amd64)
cp target/release/chronik-server artifacts/linux/amd64/

# For ARM64 (if cross-compiling)
# cp target/aarch64-unknown-linux-gnu/release/chronik-server artifacts/linux/arm64/

# Build Docker image
docker build -f Dockerfile.binary -t chronik-stream:1.3.12 .

# Test it
docker run -d -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  --name chronik-test \
  chronik-stream:1.3.12

# Check logs
docker logs chronik-test

# Test with Python
python3 test_producer_fix.py

# Clean up
docker stop chronik-test
docker rm chronik-test
```

### Option 2: Build from source in Docker

```bash
# Create a build Dockerfile
cat > Dockerfile.build <<'EOF'
FROM rust:1.75-slim as builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy source
COPY . .

# Build
RUN cargo build --release --bin chronik-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 chronik

COPY --from=builder /build/target/release/chronik-server /usr/local/bin/

RUN chmod +x /usr/local/bin/chronik-server && \
    mkdir -p /data && \
    chown chronik:chronik /data

USER chronik
VOLUME ["/data"]
EXPOSE 9092 9093

HEALTHCHECK --interval=30s --timeout=3s CMD nc -z localhost 9092 || exit 1

ENTRYPOINT ["chronik-server"]
CMD ["--bind-addr", "0.0.0.0", "--data-dir", "/data", "standalone"]
EOF

# Build
docker build -f Dockerfile.build -t chronik-stream:1.3.12 .
```

## Production Release

### 1. Tag the release

```bash
git add -A
git commit -m "Release v1.3.12: KSQLDB compatibility + Transaction APIs"
git tag -a v1.3.12 -m "Release v1.3.12

Highlights:
- Fixed flexible protocol format for KSQLDB compatibility
- Added full transaction API support (InitProducerId, AddPartitionsToTxn, EndTxn)
- Resolved Fetch v13 flexible format issues
- Full Kafka Streams support with exactly-once semantics
"
git push origin main
git push origin v1.3.12
```

### 2. Build multi-arch images

```bash
# Enable Docker buildx
docker buildx create --name chronik-builder --use
docker buildx inspect --bootstrap

# Build for Linux (amd64 + arm64)
cargo build --release --target x86_64-unknown-linux-gnu --bin chronik-server
cargo build --release --target aarch64-unknown-linux-gnu --bin chronik-server

# Create artifacts
mkdir -p artifacts/linux/{amd64,arm64}
cp target/x86_64-unknown-linux-gnu/release/chronik-server artifacts/linux/amd64/
cp target/aarch64-unknown-linux-gnu/release/chronik-server artifacts/linux/arm64/

# Build and push multi-arch image
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -f Dockerfile.binary \
  -t ghcr.io/lspecian/chronik-stream:1.3.12 \
  -t ghcr.io/lspecian/chronik-stream:latest \
  --push \
  .
```

### 3. Alternative: GitHub Actions build

Create `.github/workflows/docker-build.yml`:

```yaml
name: Build and Push Docker Image

on:
  push:
    tags:
      - 'v*'

jobs:
  build:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write

    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Set up QEMU
        uses: docker/setup-qemu-action@v3

      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v3

      - name: Login to GitHub Container Registry
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Extract metadata
        id: meta
        uses: docker/metadata-action@v5
        with:
          images: ghcr.io/lspecian/chronik-stream
          tags: |
            type=semver,pattern={{version}}
            type=semver,pattern={{major}}.{{minor}}
            type=raw,value=latest,enable={{is_default_branch}}

      - name: Build Rust binaries
        uses: rust-build/rust-build.action@v1.4.5
        with:
          RUSTTARGET: x86_64-unknown-linux-musl
          UPLOAD_MODE: none

      - name: Build and push
        uses: docker/build-push-action@v5
        with:
          context: .
          file: Dockerfile.binary
          platforms: linux/amd64,linux/arm64
          push: true
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

## Verification

### Test the Docker image

```bash
# Pull the image
docker pull ghcr.io/lspecian/chronik-stream:1.3.12

# Run it
docker run -d \
  -p 9092:9092 \
  -e CHRONIK_ADVERTISED_ADDR=localhost \
  -e RUST_LOG=info \
  --name chronik \
  ghcr.io/lspecian/chronik-stream:1.3.12

# Check it started
docker logs chronik

# Should see:
# Chronik Server v1.3.12
# Kafka protocol listening on 0.0.0.0:9092
# Ready to accept Kafka client connections

# Test with kafka-python
python3 test_producer_fix.py

# Clean up
docker stop chronik
docker rm chronik
```

### Verify version

```bash
docker run --rm ghcr.io/lspecian/chronik-stream:1.3.12 version

# Should output:
# Chronik Server v1.3.12
# Build features:
#   - Search: enabled
#   - Backup: enabled
```

## Docker Compose Setup

### For development

```yaml
# docker-compose.yml
version: '3.8'

services:
  chronik:
    image: ghcr.io/lspecian/chronik-stream:1.3.12
    hostname: chronik-stream
    container_name: chronik
    environment:
      CHRONIK_ADVERTISED_ADDR: chronik-stream
      CHRONIK_KAFKA_PORT: 9092
      RUST_LOG: info
    ports:
      - "9092:9092"
      - "9093:9093"
    volumes:
      - chronik-data:/data
    healthcheck:
      test: ["CMD", "nc", "-z", "localhost", "9092"]
      interval: 30s
      timeout: 3s
      retries: 3
      start_period: 10s

volumes:
  chronik-data:
    driver: local
```

```bash
# /etc/hosts (for local testing)
echo "127.0.0.1 chronik-stream" | sudo tee -a /etc/hosts

# Start
docker-compose up -d

# View logs
docker-compose logs -f

# Stop
docker-compose down

# Stop and remove data
docker-compose down -v
```

### For production

```yaml
# docker-compose.prod.yml
version: '3.8'

services:
  chronik:
    image: ghcr.io/lspecian/chronik-stream:1.3.12
    hostname: ${HOSTNAME:-chronik-stream}
    container_name: chronik-prod
    restart: unless-stopped
    environment:
      CHRONIK_ADVERTISED_ADDR: ${CHRONIK_ADVERTISED_ADDR}
      CHRONIK_KAFKA_PORT: 9092
      CHRONIK_METRICS_PORT: 9093
      RUST_LOG: ${RUST_LOG:-info}
      # Optional: enable search
      # CHRONIK_ENABLE_SEARCH: "true"
    ports:
      - "9092:9092"
      - "9093:9093"
    volumes:
      - /var/lib/chronik:/data
    deploy:
      resources:
        limits:
          cpus: '2'
          memory: 4G
        reservations:
          cpus: '1'
          memory: 2G
    healthcheck:
      test: ["CMD-SHELL", "nc -z localhost 9092 || exit 1"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 30s
    logging:
      driver: "json-file"
      options:
        max-size: "100m"
        max-file: "10"
```

```bash
# Create .env file
cat > .env <<EOF
HOSTNAME=chronik-stream
CHRONIK_ADVERTISED_ADDR=your-hostname-or-ip
RUST_LOG=info,chronik_protocol=debug
EOF

# Start
docker-compose -f docker-compose.prod.yml up -d

# Monitor
docker-compose -f docker-compose.prod.yml logs -f --tail=100
```

## Troubleshooting

### Image won't build

```bash
# Check Rust installation
rustc --version

# Update Rust
rustup update

# Clean build
cargo clean
cargo build --release

# Check artifacts exist
ls -la artifacts/linux/amd64/chronik-server
```

### Container won't start

```bash
# Check logs
docker logs chronik

# Check environment
docker inspect chronik | grep -A 10 Env

# Test network connectivity
docker run --rm --network container:chronik nicolaka/netshoot nc -zv localhost 9092
```

### Can't connect from host

```bash
# Check advertised address
docker logs chronik | grep "Advertised:"

# Test from container
docker exec chronik nc -zv localhost 9092

# Test from host
nc -zv localhost 9092

# Check /etc/hosts
cat /etc/hosts | grep chronik-stream
```

## Size Optimization

### Strip debug symbols

```bash
# Build with stripped symbols
RUSTFLAGS='-C strip=symbols' cargo build --release

# Check size
ls -lh target/release/chronik-server
# Should be ~30-40MB instead of ~60MB
```

### Use alpine base

```dockerfile
FROM alpine:latest

RUN apk add --no-cache ca-certificates

COPY target/x86_64-unknown-linux-musl/release/chronik-server /usr/local/bin/

# ... rest of Dockerfile
```

## Registry Publishing

### GitHub Container Registry

```bash
# Login
echo $GITHUB_TOKEN | docker login ghcr.io -u USERNAME --password-stdin

# Tag
docker tag chronik-stream:1.3.12 ghcr.io/lspecian/chronik-stream:1.3.12
docker tag chronik-stream:1.3.12 ghcr.io/lspecian/chronik-stream:latest

# Push
docker push ghcr.io/lspecian/chronik-stream:1.3.12
docker push ghcr.io/lspecian/chronik-stream:latest
```

### Docker Hub

```bash
# Login
docker login

# Tag
docker tag chronik-stream:1.3.12 lspecian/chronik-stream:1.3.12
docker tag chronik-stream:1.3.12 lspecian/chronik-stream:latest

# Push
docker push lspecian/chronik-stream:1.3.12
docker push lspecian/chronik-stream:latest
```

---

**Chronik Stream v1.3.12** - Ready for production deployment!
