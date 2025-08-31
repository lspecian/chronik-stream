#!/bin/bash
# Trigger release via GitHub Actions

set -e

VERSION="${1:-v0.5.2}"

echo "Triggering release for version: $VERSION"

# Create and push tag
git tag -a "$VERSION" -m "Release $VERSION - Fix Kafka compatibility and Linux binary packaging"
git push origin "$VERSION"

echo ""
echo "Release triggered! Monitor progress at:"
echo "https://github.com/lspecian/chronik-stream/actions"
echo ""
echo "The workflow will:"
echo "1. Cross-compile Linux binaries (AMD64 and ARM64)"
echo "2. Build multi-arch Docker images"
echo "3. Push to ghcr.io/lspecian/chronik-stream:$VERSION"
echo "4. Create GitHub release with binaries"