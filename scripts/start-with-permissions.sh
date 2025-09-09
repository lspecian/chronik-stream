#!/bin/bash

# Create metadata directories with proper permissions
echo "Creating metadata directories..."
sudo mkdir -p /tmp/chronik-controller-metadata
sudo mkdir -p /tmp/chronik-admin-metadata
sudo chmod 777 /tmp/chronik-controller-metadata
sudo chmod 777 /tmp/chronik-admin-metadata

# Start services with bind mounts instead of volumes
docker-compose down -v
docker run -d \
  --name chronik-controller \
  --network chronik-stream_chronik \
  -p 9090:9090 \
  -e NODE_ID=1 \
  -e RUST_LOG=info \
  -e METADATA_PATH=/home/chronik/metadata \
  -v /tmp/chronik-controller-metadata:/home/chronik/metadata \
  chronik-stream-controller

docker run -d \
  --name chronik-admin \
  --network chronik-stream_chronik \
  -p 8080:8080 \
  -p 8081:8081 \
  -e CONTROLLER_ENDPOINTS=chronik-controller:9090 \
  -e RUST_LOG=info \
  -e METADATA_PATH=/home/chronik/metadata \
  -v /tmp/chronik-admin-metadata:/home/chronik/metadata \
  chronik-stream-admin

echo "Services started with proper permissions"
docker ps | grep chronik