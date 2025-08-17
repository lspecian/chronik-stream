#!/bin/bash
# Optimized Docker build script with caching
set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}🚀 Chronik Stream - Optimized Docker Build${NC}"
echo "================================================"

# Check if BuildKit is enabled
if [ -z "$DOCKER_BUILDKIT" ]; then
    export DOCKER_BUILDKIT=1
    echo -e "${YELLOW}📦 Enabling Docker BuildKit for faster builds${NC}"
fi

# Build arguments for optimization
BUILD_ARGS=(
    --build-arg BUILDKIT_INLINE_CACHE=1
    --build-arg CARGO_INCREMENTAL=0
    --build-arg CARGO_NET_RETRY=10
)

# Cache configuration
CACHE_FROM=(
    --cache-from type=local,src=/tmp/chronik-buildcache
    --cache-from ghcr.io/lspecian/chronik-stream:buildcache
    --cache-from ghcr.io/lspecian/chronik-stream:latest
)

CACHE_TO=(
    --cache-to type=local,dest=/tmp/chronik-buildcache,mode=max
)

# Platform to build for
PLATFORM="linux/amd64"

# Function to build with timing
build_with_timing() {
    local dockerfile=$1
    local tag=$2
    
    echo -e "\n${GREEN}📦 Building: $tag${NC}"
    echo "Using Dockerfile: $dockerfile"
    
    START_TIME=$(date +%s)
    
    docker buildx build \
        -f "$dockerfile" \
        -t "$tag" \
        --platform "$PLATFORM" \
        "${BUILD_ARGS[@]}" \
        "${CACHE_FROM[@]}" \
        "${CACHE_TO[@]}" \
        --progress=plain \
        . 2>&1 | while IFS= read -r line; do
            if [[ $line == *"ERROR"* ]]; then
                echo -e "${RED}$line${NC}"
            elif [[ $line == *"CACHED"* ]]; then
                echo -e "${GREEN}$line${NC}"
            else
                echo "$line"
            fi
        done
    
    END_TIME=$(date +%s)
    DURATION=$((END_TIME - START_TIME))
    
    echo -e "${GREEN}✅ Build completed in ${DURATION} seconds${NC}"
    
    # Show image size
    SIZE=$(docker images --format "table {{.Repository}}:{{.Tag}}\t{{.Size}}" | grep "$tag" | awk '{print $2}')
    echo -e "${YELLOW}📏 Image size: $SIZE${NC}"
}

# Create buildx builder if it doesn't exist
if ! docker buildx ls | grep -q chronik-builder; then
    echo -e "${YELLOW}🔧 Creating buildx builder...${NC}"
    docker buildx create --name chronik-builder --driver docker-container --use
    docker buildx inspect --bootstrap
fi

# Ensure cache directory exists
mkdir -p /tmp/chronik-buildcache

# Build options
echo -e "\n${YELLOW}Select build option:${NC}"
echo "1) BuildKit optimized (Dockerfile.buildkit)"
echo "2) Cargo-chef optimized (Dockerfile.all-in-one.optimized)"
echo "3) Standard build (Dockerfile.all-in-one)"
echo "4) All of the above (benchmark)"
read -p "Enter choice [1-4]: " choice

case $choice in
    1)
        build_with_timing "Dockerfile.buildkit" "chronik:buildkit"
        ;;
    2)
        build_with_timing "Dockerfile.all-in-one.optimized" "chronik:chef"
        ;;
    3)
        build_with_timing "Dockerfile.all-in-one" "chronik:standard"
        ;;
    4)
        echo -e "\n${YELLOW}🏁 Running benchmark builds...${NC}"
        build_with_timing "Dockerfile.buildkit" "chronik:buildkit"
        build_with_timing "Dockerfile.all-in-one.optimized" "chronik:chef"
        build_with_timing "Dockerfile.all-in-one" "chronik:standard"
        
        echo -e "\n${GREEN}📊 Build Summary:${NC}"
        docker images | grep chronik | head -5
        ;;
    *)
        echo -e "${RED}Invalid option${NC}"
        exit 1
        ;;
esac

echo -e "\n${GREEN}🎉 Build process complete!${NC}"
echo -e "${YELLOW}Tip: Run the same build again to see caching in action!${NC}"