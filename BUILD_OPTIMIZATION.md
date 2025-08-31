# Chronik Stream Build Optimization Strategy

## Current Problems
1. **Full rebuilds every time** - Docker builds from scratch, taking 10+ minutes
2. **Memory intensive** - Requires 8GB+ RAM, often getting killed
3. **Platform mismatch** - Building on macOS produces Mach-O binaries unusable in Linux containers
4. **No caching** - Every Docker build downloads and compiles all dependencies
5. **Monolithic builds** - Building all crates even when only one changes

## Proposed Solution

### 1. Local Cross-Compilation Setup
Instead of building inside Docker, cross-compile locally for Linux targets:

```bash
# Install cross-compilation toolchains
rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl

# Install cross for easier cross-compilation
cargo install cross
```

### 2. Incremental Build Script
Create a smart build script that only rebuilds what changed:

```bash
#!/bin/bash
# build.sh - Smart incremental builder

# Check which crates changed
CHANGED_CRATES=$(git diff --name-only HEAD~1 | grep "^crates/" | cut -d/ -f2 | sort -u)

# Build only changed crates for Linux
for crate in $CHANGED_CRATES; do
    cross build --release --target x86_64-unknown-linux-musl --package chronik-$crate
    cross build --release --target aarch64-unknown-linux-musl --package chronik-$crate
done
```

### 3. Binary Artifact Storage
Store compiled binaries as artifacts:

```
artifacts/
├── linux/
│   ├── amd64/
│   │   ├── chronik-server
│   │   ├── chronik-server.sha256
│   │   └── chronik-server.version
│   └── arm64/
│       ├── chronik-server
│       ├── chronik-server.sha256
│       └── chronik-server.version
└── checksums.json
```

### 4. Optimized Dockerfile
Single Dockerfile that just copies pre-built binaries:

```dockerfile
# Dockerfile
FROM debian:bookworm-slim

ARG TARGETARCH

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 chronik

# Copy the pre-built binary for the target architecture
COPY artifacts/linux/${TARGETARCH}/chronik-server /usr/local/bin/chronik-server

RUN chmod +x /usr/local/bin/chronik-server && \
    mkdir -p /data && \
    chown chronik:chronik /data

USER chronik
VOLUME ["/data"]
EXPOSE 9092 9093

ENTRYPOINT ["chronik-server"]
CMD ["--bind-addr", "0.0.0.0", "--data-dir", "/data", "standalone"]
```

### 5. GitHub Actions Workflow
Automated CI/CD that builds incrementally:

```yaml
name: Build and Release

on:
  push:
    branches: [main]
  pull_request:

jobs:
  detect-changes:
    runs-on: ubuntu-latest
    outputs:
      matrix: ${{ steps.set-matrix.outputs.matrix }}
    steps:
      - uses: actions/checkout@v3
      - id: set-matrix
        run: |
          # Detect which crates changed
          CHANGED=$(git diff --name-only HEAD~1 | grep "^crates/" | cut -d/ -f2 | sort -u | jq -R -s -c 'split("\n")[:-1]')
          echo "matrix={\"crate\":${CHANGED}}" >> $GITHUB_OUTPUT

  build-binaries:
    needs: detect-changes
    strategy:
      matrix: ${{ fromJson(needs.detect-changes.outputs.matrix) }}
      platform: [linux-amd64, linux-arm64]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      
      - name: Setup Rust
        uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
          target: ${{ matrix.platform == 'linux-amd64' && 'x86_64-unknown-linux-musl' || 'aarch64-unknown-linux-musl' }}
      
      - name: Cache cargo registry
        uses: actions/cache@v3
        with:
          path: ~/.cargo/registry
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
      
      - name: Cache cargo build
        uses: actions/cache@v3
        with:
          path: target
          key: ${{ runner.os }}-${{ matrix.platform }}-cargo-build-${{ hashFiles('**/Cargo.lock') }}
      
      - name: Build changed crate
        run: |
          cargo build --release --target ${{ matrix.platform == 'linux-amd64' && 'x86_64-unknown-linux-musl' || 'aarch64-unknown-linux-musl' }} --package chronik-${{ matrix.crate }}
      
      - name: Upload artifact
        uses: actions/upload-artifact@v3
        with:
          name: chronik-${{ matrix.crate }}-${{ matrix.platform }}
          path: target/*/release/chronik-${{ matrix.crate }}

  build-docker:
    needs: build-binaries
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      
      - name: Download artifacts
        uses: actions/download-artifact@v3
        with:
          path: artifacts
      
      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@v2
      
      - name: Build and push
        uses: docker/build-push-action@v4
        with:
          context: .
          platforms: linux/amd64,linux/arm64
          push: true
          tags: |
            ghcr.io/chronik-stream/chronik:latest
            ghcr.io/chronik-stream/chronik:${{ github.sha }}
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

## Implementation Steps

### Phase 1: Local Development (Immediate)
1. Install cross-compilation tools
2. Create build script for local cross-compilation
3. Test binaries in Docker locally

### Phase 2: CI/CD Setup (Week 1)
1. Setup GitHub Actions workflow
2. Configure artifact storage
3. Implement incremental build detection

### Phase 3: Optimization (Week 2)
1. Add sccache for distributed compilation caching
2. Implement parallel builds for independent crates
3. Add build metrics and monitoring

## Benefits
- **Build time**: 10+ minutes → 30 seconds (when using cached binaries)
- **Memory usage**: 8GB+ → <500MB (just copying binaries)
- **Reliability**: No more OOM kills during builds
- **Cost**: Reduced CI/CD minutes and resource usage
- **Developer experience**: Faster iteration cycles

## Quick Start Commands

```bash
# Install dependencies
brew install filosottile/musl-cross/musl-cross  # macOS
cargo install cross

# Build for Linux locally
./build-linux.sh

# Build Docker image with pre-built binaries
docker buildx build --platform linux/amd64,linux/arm64 -t chronik:latest .

# Push to registry
docker push ghcr.io/chronik-stream/chronik:latest
```

## Caching Strategy

### Local Development
- Use `sccache` for Rust compilation caching
- Store artifacts in `~/.chronik-cache/`
- Incremental compilation enabled by default

### CI/CD
- GitHub Actions cache for cargo registry
- Docker layer caching with buildx
- Artifact storage between jobs

### Production
- Pre-built binaries in GitHub Releases
- Docker images with embedded binaries
- No compilation needed at deployment time