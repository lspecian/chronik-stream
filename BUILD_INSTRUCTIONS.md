# Build Instructions for v0.7.0 Release

Due to resource constraints on the current system, the build needs to be completed on a machine with sufficient memory (at least 8GB RAM recommended).

## Prerequisites

1. Install Rust:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

2. Install build tools:
```bash
# macOS
brew install cmake openssl

# Linux
sudo apt-get install build-essential cmake libssl-dev pkg-config
```

## Build Commands

### Option 1: Native Build (Fastest)
```bash
# Clone the repository
git clone https://github.com/lspecian/chronik-stream.git
cd chronik-stream
git checkout v0.7.0

# Build release binary
cargo build --release --bin chronik-server

# Binary will be at: target/release/chronik-server
```

### Option 2: Cross-Platform Build
```bash
# Install cross
cargo install cross

# Build for Linux x86_64
cross build --release --bin chronik-server --target x86_64-unknown-linux-gnu

# Build for Linux ARM64
cross build --release --bin chronik-server --target aarch64-unknown-linux-gnu

# Build for macOS x86_64
cross build --release --bin chronik-server --target x86_64-apple-darwin

# Build for macOS ARM64 (Apple Silicon)
cross build --release --bin chronik-server --target aarch64-apple-darwin
```

### Option 3: Docker Build
```bash
# Build Docker image with multi-arch support
docker buildx create --use
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t ghcr.io/lspecian/chronik-stream:v0.7.0 \
  -t ghcr.io/lspecian/chronik-stream:latest \
  --push .
```

## Creating Release Archives

After building, create release archives:

```bash
# Linux x86_64
tar czf chronik-server-v0.7.0-linux-amd64.tar.gz \
  -C target/x86_64-unknown-linux-gnu/release chronik-server

# Linux ARM64
tar czf chronik-server-v0.7.0-linux-arm64.tar.gz \
  -C target/aarch64-unknown-linux-gnu/release chronik-server

# macOS x86_64
tar czf chronik-server-v0.7.0-darwin-amd64.tar.gz \
  -C target/x86_64-apple-darwin/release chronik-server

# macOS ARM64
tar czf chronik-server-v0.7.0-darwin-arm64.tar.gz \
  -C target/aarch64-apple-darwin/release chronik-server
```

## Testing the Build

1. Start the server with advertised address:
```bash
./chronik-server --bind-addr 0.0.0.0 --advertised-addr localhost
```

2. Run the test script:
```bash
pip install kafka-python
python test_advertised_address.py
```

Expected output:
```
✅ SUCCESS: Chronik Stream is working correctly with advertised address!
```

## GitHub Actions Build (Recommended)

Create `.github/workflows/release.yml`:

```yaml
name: Release Build

on:
  push:
    tags:
      - 'v*'

jobs:
  build:
    strategy:
      matrix:
        include:
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-latest
          - target: aarch64-unknown-linux-gnu
            os: ubuntu-latest
          - target: x86_64-apple-darwin
            os: macos-latest
          - target: aarch64-apple-darwin
            os: macos-latest
    
    runs-on: ${{ matrix.os }}
    
    steps:
    - uses: actions/checkout@v3
    
    - name: Install Rust
      uses: actions-rs/toolchain@v1
      with:
        toolchain: stable
        target: ${{ matrix.target }}
        override: true
    
    - name: Build
      uses: actions-rs/cargo@v1
      with:
        use-cross: true
        command: build
        args: --release --bin chronik-server --target ${{ matrix.target }}
    
    - name: Create archive
      run: |
        cd target/${{ matrix.target }}/release
        tar czf ../../../chronik-server-${{ github.ref_name }}-${{ matrix.target }}.tar.gz chronik-server
    
    - name: Upload artifact
      uses: actions/upload-artifact@v3
      with:
        name: chronik-server-${{ matrix.target }}
        path: chronik-server-*.tar.gz

  release:
    needs: build
    runs-on: ubuntu-latest
    
    steps:
    - uses: actions/checkout@v3
    
    - name: Download artifacts
      uses: actions/download-artifact@v3
    
    - name: Create Release
      uses: softprops/action-gh-release@v1
      with:
        files: chronik-server-*/chronik-server-*.tar.gz
        body_path: RELEASE_NOTES_v0.7.0.md
        prerelease: false
      env:
        GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

## Manual Release Process

If building manually:

1. Build on a machine with at least 8GB RAM
2. Test the binary with the advertised address feature
3. Create release archives for each platform
4. Upload to GitHub Releases
5. Update Docker images
6. Notify users

## Troubleshooting

If build fails with SIGKILL:
- Increase system memory limits
- Use a cloud build service (GitHub Actions, CircleCI, etc.)
- Build on a dedicated build server
- Consider using Docker to build with resource limits

## Success Criteria

The build is successful when:
1. Binary compiles without errors
2. `chronik-server --version` shows v0.7.0
3. Test script passes with advertised address configuration
4. Kafka clients can connect successfully