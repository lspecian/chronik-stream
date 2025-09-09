# Docker Image Publishing Instructions

## Building the Docker Image

### 1. Build Multi-Architecture Images (Recommended)

For production releases, build both AMD64 and ARM64 images:

```bash
# Setup Docker Buildx for multi-arch builds
docker buildx create --name chronik-builder --use
docker buildx inspect --bootstrap

# Build and push multi-arch image to GitHub Container Registry
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --tag ghcr.io/lspecian/chronik-stream:v0.5.0 \
  --tag ghcr.io/lspecian/chronik-stream:latest \
  --push \
  .
```

### 2. Build Local Image (for Testing)

```bash
# Build for local architecture only
docker build -t chronik-server:latest -t chronik-server:v0.5.0 .

# Verify the build
docker images | grep chronik-server
```

## Testing the Image

### Quick Test
```bash
# Run the container
docker run -d --name chronik-test -p 9092:9092 chronik-server:latest

# Check logs
docker logs chronik-test

# Test connectivity with netcat
nc -zv localhost 9092

# Test with Python Kafka client
python3 -c "
from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='localhost:9092')
print('✓ Connected to Chronik Server')
"

# Stop and remove test container
docker stop chronik-test && docker rm chronik-test
```

### Full Integration Test
```bash
# Run with persistent storage
docker run -d --name chronik-prod \
  -p 9092:9092 \
  -v chronik-data:/data \
  -e RUST_LOG=info \
  chronik-server:latest

# Monitor performance
docker stats chronik-prod

# Check health
docker exec chronik-prod nc -zv localhost 9092
```

## Publishing to Registries

### GitHub Container Registry (ghcr.io)

1. **Login to GitHub Container Registry:**
```bash
echo $GITHUB_TOKEN | docker login ghcr.io -u USERNAME --password-stdin
```

2. **Tag the image:**
```bash
docker tag chronik-server:latest ghcr.io/lspecian/chronik-stream:v0.5.0
docker tag chronik-server:latest ghcr.io/lspecian/chronik-stream:latest
```

3. **Push to registry:**
```bash
docker push ghcr.io/lspecian/chronik-stream:v0.5.0
docker push ghcr.io/lspecian/chronik-stream:latest
```

### Docker Hub (Optional)

1. **Login to Docker Hub:**
```bash
docker login
```

2. **Tag for Docker Hub:**
```bash
docker tag chronik-server:latest yourusername/chronik-stream:v0.5.0
docker tag chronik-server:latest yourusername/chronik-stream:latest
```

3. **Push to Docker Hub:**
```bash
docker push yourusername/chronik-stream:v0.5.0
docker push yourusername/chronik-stream:latest
```

## Multi-Architecture Build with GitHub Actions

The repository includes GitHub Actions workflows that automatically build and publish multi-architecture images on release:

1. **Create a new release:**
```bash
git tag v0.5.0
git push origin v0.5.0
```

2. **GitHub Actions will automatically:**
   - Build AMD64 and ARM64 images
   - Push to ghcr.io with proper tags
   - Create release artifacts

## Verification After Publishing

```bash
# Pull and test the published image
docker pull ghcr.io/lspecian/chronik-stream:v0.5.0

# Verify multi-arch support
docker manifest inspect ghcr.io/lspecian/chronik-stream:v0.5.0

# Should show both architectures:
# - linux/amd64
# - linux/arm64
```

## Image Size Optimization

The current Dockerfile uses a multi-stage build to minimize image size:
- Build stage: Uses full Rust image (~1.5GB)
- Runtime stage: Uses debian:bookworm-slim (~80MB base)
- Final image: ~50-60MB (excluding binary)

### Further Optimizations (Optional)

For even smaller images, consider using:

```dockerfile
# Use distroless for minimal attack surface
FROM gcr.io/distroless/cc-debian12

# Or Alpine Linux (requires musl compilation)
FROM alpine:3.19
```

## Troubleshooting

### Build Failures

1. **Out of memory during build:**
```bash
# Increase Docker memory limit
docker system prune -a  # Clean up first
# Then adjust Docker Desktop memory settings
```

2. **Slow builds:**
```bash
# Use buildkit for better caching
DOCKER_BUILDKIT=1 docker build -t chronik-server:latest .
```

3. **Architecture mismatch:**
```bash
# Check your platform
docker version --format '{{.Server.Platform.Name}}'

# Build for specific platform
docker build --platform linux/amd64 -t chronik-server:latest .
```

## Security Considerations

1. **Don't include secrets in the image**
2. **Run as non-root user** (already configured in Dockerfile)
3. **Scan for vulnerabilities:**
```bash
docker scout cves chronik-server:latest
```

4. **Sign images with cosign (optional):**
```bash
cosign sign ghcr.io/lspecian/chronik-stream:v0.5.0
```

## Release Checklist

- [ ] Update version in Cargo.toml files
- [ ] Update version in README.md
- [ ] Build and test image locally
- [ ] Run integration tests
- [ ] Push to registry
- [ ] Verify multi-arch support
- [ ] Update documentation
- [ ] Create GitHub release
- [ ] Announce release

## Support

For issues or questions:
- GitHub Issues: https://github.com/lspecian/chronik-stream/issues
- Docker Hub: https://hub.docker.com/r/lspecian/chronik-stream