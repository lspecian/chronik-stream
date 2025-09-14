#!/bin/bash

# Create WAL data directories with proper permissions
echo "Creating WAL data directories..."
sudo mkdir -p /tmp/chronik-server-data
sudo chmod 777 /tmp/chronik-server-data

# Start chronik-server with bind mount for WAL data
docker-compose down -v
docker run -d \
  --name chronik-server \
  --network chronik-stream_chronik \
  -p 9092:9092 \
  -p 9093:9093 \
  -e RUST_LOG=info \
  -e DATA_DIR=/data \
  -v /tmp/chronik-server-data:/data \
  chronik-stream-server \
  --wal-metadata --data-dir /data

echo "Chronik server started with proper permissions"
docker ps | grep chronik