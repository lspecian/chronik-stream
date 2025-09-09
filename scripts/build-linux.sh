#!/bin/bash
# Cross-compile for Linux on macOS
set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}==================================${NC}"
echo -e "${GREEN}Building Chronik for Linux${NC}"
echo -e "${GREEN}==================================${NC}"

# Check if cross is installed
if ! command -v cross &> /dev/null; then
    echo -e "${YELLOW}Installing cross for cross-compilation...${NC}"
    cargo install cross --git https://github.com/cross-rs/cross
fi

# Create artifacts directory
mkdir -p artifacts/linux/{amd64,arm64}

# Function to build for a target
build_target() {
    local target=$1
    local arch=$2
    
    echo -e "${GREEN}Building for $target...${NC}"
    
    # Use cross for cross-compilation
    if cross build --release --target $target --bin chronik-server; then
        # Copy binary to artifacts
        cp target/$target/release/chronik-server artifacts/linux/$arch/
        
        # Generate checksum
        if command -v sha256sum &> /dev/null; then
            sha256sum artifacts/linux/$arch/chronik-server > artifacts/linux/$arch/chronik-server.sha256
        else
            shasum -a 256 artifacts/linux/$arch/chronik-server > artifacts/linux/$arch/chronik-server.sha256
        fi
        
        # Save version
        echo "v0.5.2" > artifacts/linux/$arch/chronik-server.version
        
        echo -e "${GREEN}✓ Built $arch binary${NC}"
        ls -lah artifacts/linux/$arch/chronik-server
    else
        echo -e "${RED}✗ Failed to build $arch binary${NC}"
        return 1
    fi
}

# Build for both architectures
echo -e "${YELLOW}Building for Linux AMD64...${NC}"
build_target x86_64-unknown-linux-musl amd64

echo -e "${YELLOW}Building for Linux ARM64...${NC}"
build_target aarch64-unknown-linux-musl arm64

echo -e "${GREEN}==================================${NC}"
echo -e "${GREEN}Build complete! Artifacts:${NC}"
echo -e "${GREEN}==================================${NC}"
ls -lah artifacts/linux/*/chronik-server

echo ""
echo -e "${GREEN}Next steps:${NC}"
echo "1. Test binaries: docker run --rm -v \$(pwd)/artifacts:/artifacts debian:bookworm-slim /artifacts/linux/amd64/chronik-server --version"
echo "2. Build Docker image: docker buildx build --platform linux/amd64,linux/arm64 -f Dockerfile.binary -t chronik:v0.5.2 ."
echo "3. Push to registry: docker push ghcr.io/lspecian/chronik-stream:v0.5.2"