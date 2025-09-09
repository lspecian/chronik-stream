#!/bin/bash
# Fast incremental build script for Chronik Stream
set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}Starting optimized build...${NC}"

# Check for sccache
if command -v sccache &> /dev/null; then
    export RUSTC_WRAPPER=sccache
    echo -e "${GREEN}✓ Using sccache for compilation caching${NC}"
else
    echo -e "${YELLOW}! sccache not found. Install with: cargo install sccache${NC}"
fi

# Enable optimizations
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-8}
export CARGO_INCREMENTAL=1

# Check for lld linker
if command -v lld &> /dev/null; then
    export RUSTFLAGS="${RUSTFLAGS} -C link-arg=-fuse-ld=lld"
    echo -e "${GREEN}✓ Using LLD linker for faster linking${NC}"
fi

# Function to build a crate
build_crate() {
    local crate=$1
    echo -e "${YELLOW}Building $crate...${NC}"
    cargo build --profile release-fast --package $crate
}

# Build in dependency order
echo -e "${GREEN}Phase 1: Core libraries${NC}"
build_crate chronik-common
build_crate chronik-protocol

echo -e "${GREEN}Phase 2: Storage and auth${NC}"
build_crate chronik-storage &
build_crate chronik-auth &
wait

echo -e "${GREEN}Phase 3: Services${NC}"
build_crate chronik-monitoring &
build_crate chronik-ingest &
wait

echo -e "${GREEN}Phase 4: Server binary${NC}"
cargo build --profile release-fast --bin chronik-server

# Show results
echo -e "${GREEN}Build complete!${NC}"
ls -lah target/release-fast/chronik-server 2>/dev/null || ls -lah target/release/chronik-server

# Show cache stats if sccache is available
if command -v sccache &> /dev/null; then
    echo -e "${GREEN}Cache statistics:${NC}"
    sccache --show-stats
fi