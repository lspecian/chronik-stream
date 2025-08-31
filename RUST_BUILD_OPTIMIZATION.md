# Rust Build Optimization Guide for Chronik Stream

## Current Build Issues
- Full release builds take 10+ minutes
- Memory usage exceeds 8GB during linking
- No incremental compilation in Docker
- Redundant dependency compilation
- Single-threaded linking bottleneck

## Optimization Strategies

### 1. Cargo Configuration Optimization

Create `.cargo/config.toml`:
```toml
[build]
jobs = 8                      # Parallel compilation jobs
incremental = true            # Enable incremental compilation
target-dir = "target"         # Shared target directory

[target.x86_64-unknown-linux-musl]
linker = "clang"
rustflags = [
    "-C", "link-arg=-fuse-ld=lld",     # Use faster LLD linker
    "-C", "target-cpu=x86-64-v2",       # Modern CPU optimizations
    "-C", "opt-level=2",                # Balanced optimization
    "-Z", "share-generics=y",           # Share generic instantiations
    "-Z", "threads=8",                  # Parallel LLVM
]

[target.aarch64-unknown-linux-musl]
linker = "clang"
rustflags = [
    "-C", "link-arg=-fuse-ld=lld",
    "-C", "target-cpu=generic",
    "-C", "opt-level=2",
    "-Z", "share-generics=y",
    "-Z", "threads=8",
]

[profile.dev]
opt-level = 0
debug = 0                    # Disable debug info for faster builds
incremental = true
overflow-checks = false

[profile.dev.package."*"]    # Optimize dependencies even in dev
opt-level = 2

[profile.release]
opt-level = 2                # Use 2 instead of 3 for faster builds
lto = "thin"                 # Thin LTO is much faster than fat LTO
codegen-units = 16           # More parallelism
incremental = true           # Incremental even in release
strip = true                 # Strip symbols to reduce size
panic = "abort"              # Smaller binaries

[profile.release-fast]       # New profile for CI/CD
inherits = "release"
opt-level = 2
lto = false                  # No LTO for faster builds
codegen-units = 256          # Maximum parallelism

[profile.release-optimized]  # For final production builds only
inherits = "release"
opt-level = 3
lto = "fat"
codegen-units = 1
```

### 2. Workspace Optimization

Update root `Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = [
    "crates/chronik-common",
    "crates/chronik-protocol",
    "crates/chronik-storage",
    "crates/chronik-ingest",
    "crates/chronik-server",
    # ... other crates
]

[workspace.dependencies]
# Pin all dependencies at workspace level
tokio = { version = "1.35", features = ["full"], default-features = false }
serde = { version = "1.0", features = ["derive"], default-features = false }
# ... consolidate all deps here

[workspace.lints.rust]
unsafe_code = "warn"
missing_docs = "warn"

# Workspace-wide dependency optimization
[workspace.metadata.cargo-machete]
ignored = ["build-dependencies"]
```

### 3. Dependency Optimization

Create `cargo-machete.toml`:
```toml
# Remove unused dependencies
ignored = ["chronik-benchmarks", "tests"]
```

Run dependency audit:
```bash
cargo install cargo-machete cargo-udeps
cargo machete          # Find unused dependencies
cargo +nightly udeps   # Find unused dependencies (nightly)
```

### 4. Build Caching with sccache

```bash
# Install sccache
cargo install sccache

# Configure for local development
export RUSTC_WRAPPER=sccache
export SCCACHE_DIR=$HOME/.cache/sccache
export SCCACHE_CACHE_SIZE="50G"

# For CI/CD with S3 backend
export SCCACHE_BUCKET=chronik-build-cache
export SCCACHE_REGION=us-east-1
export AWS_ACCESS_KEY_ID=xxx
export AWS_SECRET_ACCESS_KEY=xxx
```

### 5. Parallel Build Script

Create `build-fast.sh`:
```bash
#!/bin/bash
set -e

# Enable all optimizations
export RUSTC_WRAPPER=sccache
export CARGO_BUILD_JOBS=8
export CARGO_INCREMENTAL=1
export RUSTFLAGS="-C link-arg=-fuse-ld=lld -Z share-generics=y -Z threads=8"

# Build different crates in parallel
build_crate() {
    local crate=$1
    echo "Building $crate..."
    cargo build --release --package $crate &
}

# Build independent crates in parallel
build_crate chronik-common
build_crate chronik-protocol
wait

build_crate chronik-storage
build_crate chronik-auth
wait

build_crate chronik-ingest
build_crate chronik-monitoring
wait

# Final binary
cargo build --release --bin chronik-server

echo "Build complete!"
sccache --show-stats
```

### 6. Docker Layer Caching

Create `Dockerfile.layered`:
```dockerfile
# Build dependencies separately for caching
FROM rust:slim as deps

WORKDIR /usr/src/chronik

# Copy only Cargo files first
COPY Cargo.toml Cargo.lock ./
COPY crates/chronik-common/Cargo.toml crates/chronik-common/
COPY crates/chronik-protocol/Cargo.toml crates/chronik-protocol/
# ... copy all Cargo.toml files

# Create dummy main.rs files to build dependencies
RUN mkdir -p crates/chronik-server/src && \
    echo "fn main() {}" > crates/chronik-server/src/main.rs && \
    cargo build --release --bin chronik-server && \
    rm -rf crates/*/src

# Now copy actual source and build
FROM deps as builder

COPY crates/ ./crates/

# This will be fast - only compiles our code, not deps
RUN touch crates/chronik-server/src/main.rs && \
    cargo build --release --bin chronik-server

# Runtime stage
FROM debian:bookworm-slim
COPY --from=builder /usr/src/chronik/target/release/chronik-server /usr/local/bin/
# ... rest of runtime setup
```

### 7. Conditional Compilation

Reduce compile time by feature gating:

```toml
# In Cargo.toml
[features]
default = ["base"]
base = ["tokio", "serde"]
full = ["base", "search", "backup", "monitoring"]
search = ["tantivy"]
backup = ["opendal"]
monitoring = ["prometheus", "opentelemetry"]

# Minimal build for development
minimal = []
```

### 8. Build Benchmarking

Create `benchmark-build.sh`:
```bash
#!/bin/bash

# Clean build with timing
echo "Benchmarking clean build..."
cargo clean
time cargo build --release --bin chronik-server 2>&1 | tee build-clean.log

# Incremental build
echo "Benchmarking incremental build..."
touch crates/chronik-server/src/main.rs
time cargo build --release --bin chronik-server 2>&1 | tee build-incremental.log

# With sccache
export RUSTC_WRAPPER=sccache
sccache --zero-stats
echo "Benchmarking with sccache..."
cargo clean
time cargo build --release --bin chronik-server 2>&1 | tee build-sccache.log
sccache --show-stats
```

### 9. Cargo Chef for Docker

Use cargo-chef for optimal Docker caching:

```dockerfile
FROM lukemathwalker/cargo-chef:latest-rust-slim AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies - this is cached
RUN cargo chef cook --release --recipe-path recipe.json
# Build application
COPY . .
RUN cargo build --release --bin chronik-server

FROM debian:bookworm-slim AS runtime
COPY --from=builder /app/target/release/chronik-server /usr/local/bin/
# ... runtime setup
```

### 10. Mold Linker (Fastest Option)

```bash
# Install mold (fastest linker available)
git clone https://github.com/rui314/mold.git
cd mold
make -j$(nproc) CXX=clang++ CC=clang
sudo make install

# Use in builds
RUSTFLAGS="-C link-arg=-fuse-ld=mold" cargo build --release
```

## Build Time Comparison

| Method | Clean Build | Incremental | Docker |
|--------|------------|-------------|---------|
| Baseline | 10-15 min | 2-3 min | 15+ min |
| With sccache | 5-7 min | 30s | N/A |
| Parallel builds | 4-5 min | 20s | N/A |
| Mold linker | 3-4 min | 15s | N/A |
| All optimizations | 2-3 min | 10s | 5 min |

## Memory Usage Optimization

```toml
# In .cargo/config.toml
[env]
CARGO_BUILD_JOBS = "4"        # Reduce parallel jobs
CARGO_INCREMENTAL = "1"       # Use incremental compilation
RUSTC_THREADS = "4"           # Limit LLVM threads

[profile.release]
codegen-units = 256           # More units = less memory per unit
lto = false                   # LTO uses lots of memory
```

## CI/CD Optimization

GitHub Actions with all optimizations:

```yaml
name: Optimized Build

on: [push, pull_request]

env:
  CARGO_TERM_COLOR: always
  RUSTC_WRAPPER: sccache
  SCCACHE_GHA_ENABLED: true

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
    - uses: actions/checkout@v3
    
    - name: Install mold linker
      run: |
        wget https://github.com/rui314/mold/releases/download/v2.0.0/mold-2.0.0-x86_64-linux.tar.gz
        tar -xzf mold-2.0.0-x86_64-linux.tar.gz
        sudo cp mold-2.0.0-x86_64-linux/bin/mold /usr/local/bin/
    
    - name: Setup Rust with cache
      uses: dtolnay/rust-toolchain@stable
      with:
        components: rustfmt, clippy
    
    - name: Setup sccache
      uses: mozilla-actions/sccache-action@v0.0.3
    
    - name: Cache cargo registry
      uses: Swatinem/rust-cache@v2
      with:
        cache-targets: true
        cache-on-failure: true
    
    - name: Build with optimizations
      run: |
        export RUSTFLAGS="-C link-arg=-fuse-ld=mold -Z share-generics=y"
        cargo build --release --bin chronik-server
      
    - name: Show build stats
      run: |
        sccache --show-stats
        ls -lah target/release/chronik-server
```

## Quick Commands

```bash
# Fast local build
./build-fast.sh

# Minimal build for testing
cargo build --features minimal

# Cross-compile with caching
RUSTC_WRAPPER=sccache cross build --release --target x86_64-unknown-linux-musl

# Clean and benchmark
./benchmark-build.sh
```

## Expected Results

With all optimizations:
- **Clean build**: 10+ min → 2-3 min (80% faster)
- **Incremental build**: 2-3 min → 10-15 sec (90% faster)
- **Memory usage**: 8GB → 2-3GB (60% less)
- **Docker build**: 15+ min → 5 min (66% faster)
- **CI/CD**: 20+ min → 5 min (75% faster)