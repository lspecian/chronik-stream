#!/bin/bash
# Build script for Docker image with memory optimization

set -e

echo "======================================"
echo "Building Chronik Server Docker Image"
echo "======================================"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Parse arguments
USE_OPTIMIZED=false
PUSH_TO_REGISTRY=false
MULTI_ARCH=false
REGISTRY="ghcr.io/lspecian/chronik-stream"

while [[ $# -gt 0 ]]; do
    case $1 in
        --optimized)
            USE_OPTIMIZED=true
            shift
            ;;
        --push)
            PUSH_TO_REGISTRY=true
            shift
            ;;
        --multi-arch)
            MULTI_ARCH=true
            shift
            ;;
        --registry)
            REGISTRY="$2"
            shift 2
            ;;
        --help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --optimized    Use memory-optimized Dockerfile"
            echo "  --push         Push to registry after build"
            echo "  --multi-arch   Build for multiple architectures"
            echo "  --registry     Registry to push to (default: ghcr.io/lspecian/chronik-stream)"
            echo "  --help         Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Check Docker daemon
if ! docker info > /dev/null 2>&1; then
    echo -e "${RED}Error: Docker daemon is not running${NC}"
    exit 1
fi

# Clean up any failed builds
echo "Cleaning up old builds..."
docker builder prune -f 2>/dev/null || true

# Increase Docker memory if possible (macOS/Windows)
if [[ "$OSTYPE" == "darwin"* ]]; then
    echo -e "${YELLOW}Note: On macOS, ensure Docker Desktop has at least 4GB RAM allocated${NC}"
    echo "  Docker Desktop -> Preferences -> Resources -> Memory"
fi

# Choose Dockerfile
if [ "$USE_OPTIMIZED" = true ]; then
    DOCKERFILE="Dockerfile.optimized"
    echo "Using optimized Dockerfile for low memory..."
else
    DOCKERFILE="Dockerfile"
    echo "Using standard Dockerfile..."
fi

# Build the image
if [ "$MULTI_ARCH" = true ]; then
    echo "Building multi-architecture image..."
    
    # Setup buildx if not exists
    if ! docker buildx ls | grep -q chronik-builder; then
        echo "Creating buildx builder..."
        docker buildx create --name chronik-builder --use
        docker buildx inspect --bootstrap
    else
        docker buildx use chronik-builder
    fi
    
    # Build and optionally push
    if [ "$PUSH_TO_REGISTRY" = true ]; then
        echo "Building and pushing to $REGISTRY..."
        docker buildx build \
            --platform linux/amd64,linux/arm64 \
            --tag "${REGISTRY}:latest" \
            --tag "${REGISTRY}:v0.5.1" \
            --push \
            -f "$DOCKERFILE" \
            .
    else
        echo "Building locally (no push)..."
        docker buildx build \
            --platform linux/amd64,linux/arm64 \
            --tag chronik-server:latest \
            --tag chronik-server:v0.5.1 \
            --load \
            -f "$DOCKERFILE" \
            .
    fi
else
    echo "Building for local architecture only..."
    
    # Use BuildKit for better performance
    export DOCKER_BUILDKIT=1
    
    # Build with progress output
    docker build \
        --progress=plain \
        --tag chronik-server:latest \
        --tag chronik-server:v0.5.1 \
        -f "$DOCKERFILE" \
        . 2>&1 | tee build.log
    
    # Check if build succeeded
    if [ ${PIPESTATUS[0]} -ne 0 ]; then
        echo -e "${RED}Build failed! Check build.log for details${NC}"
        echo ""
        echo "Common solutions:"
        echo "1. Free up memory: docker system prune -a"
        echo "2. Use optimized build: $0 --optimized"
        echo "3. Increase Docker memory allocation"
        echo "4. Build on a machine with more RAM"
        exit 1
    fi
    
    # Push if requested
    if [ "$PUSH_TO_REGISTRY" = true ]; then
        echo "Tagging for registry..."
        docker tag chronik-server:latest "${REGISTRY}:latest"
        docker tag chronik-server:v0.5.1 "${REGISTRY}:v0.5.1"
        
        echo "Pushing to $REGISTRY..."
        docker push "${REGISTRY}:latest"
        docker push "${REGISTRY}:v0.5.1"
    fi
fi

echo ""
echo -e "${GREEN}======================================"
echo "Build completed successfully!"
echo "======================================${NC}"
echo ""

# Show image info
docker images | grep chronik-server | head -5

echo ""
echo "Next steps:"
echo "1. Test locally: ./test_docker.sh"
echo "2. Push to registry: $0 --push"
echo "3. Build multi-arch: $0 --multi-arch --push"