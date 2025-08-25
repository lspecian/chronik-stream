## Docker Images

Pull the latest images from GitHub Container Registry:

```bash
# All-in-one image (recommended for quick start)
docker pull ghcr.io/lspecian/chronik-stream:latest

# Individual service images
docker pull ghcr.io/lspecian/chronik-stream-ingest:latest
docker pull ghcr.io/lspecian/chronik-stream-controller:latest
docker pull ghcr.io/lspecian/chronik-stream-search:latest
docker pull ghcr.io/lspecian/chronik-stream-query:latest
```

Run the all-in-one container:

```bash
docker run -d \
  --name chronik \
  -p 9092:9092 \
  -p 3000:3000 \
  -v chronik-data:/data \
  ghcr.io/lspecian/chronik-stream:latest
```
