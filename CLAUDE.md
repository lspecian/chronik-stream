# Chronik Stream - Claude Memory

## Metadata Storage Migration to Sled

Successfully migrated from PostgreSQL to Sled embedded database for metadata storage.

### Services Running:
- **Controller**: ✓ Running on port 9090 (gRPC protocol) with Sled metadata store
- **Ingest**: ✓ Running on port 9092 (Kafka protocol)

### Migration Completed:
1. Implemented MetadataStore trait for abstract metadata operations
2. Created Sled-based storage implementation
3. Updated controller to use embedded Sled database
4. Removed PostgreSQL dependency entirely

### Fixes Applied:
1. Updated Rust version in all Dockerfiles from 1.75 to latest
2. Increased ingest buffer size from 64KB to 100MB
3. Removed Prometheus scraping of ingest port 9092

### Verified:
- Sled metadata store initialized successfully
- No more "Request too large" errors
- Services are stable and not crashing
- Metadata persistence working via volume mounts

# Chronik Stream - Claude Memory

## Docker Compose Debugging Session

### Issue Summary
User reported docker-compose crashed the machine. Investigation revealed multiple issues:

### Root Cause Analysis
1. **Rust Version Incompatibility**
   - Cargo.lock v4 format requires Rust 1.79+
   - Dockerfiles were using rust:1.75
   - Fixed by updating all Dockerfiles to use `rust:latest`

2. **Port Conflict**
   - Port 3000 already in use
   - Changed Grafana port from 3000 to 3001 in docker-compose.yml

3. **Large Request Errors (Main Issue)**
   - Ingest service showing: "Request too large: 1195725856 bytes"
   - Source: Prometheus container (172.20.0.3)
   - Cause: Prometheus scraping Kafka protocol port 9092 for metrics
   - HTTP "GET " (0x47455420) interpreted as 1.1GB message size by Kafka handler
   - Fixed by removing ingest:9092 from Prometheus scrape targets

### System Resources
- RAM: 47GB total, 38GB available
- CPU: 12 cores
- Disk: 648GB free space
- Resources are sufficient for running all services

### Configuration Changes Made
1. Updated buffer size in ingest service from 64KB to 100MB
2. Removed incorrect Prometheus scrape target for ingest service
3. Changed Grafana port to avoid conflicts

### Commands to Run
```bash
docker-compose down
docker-compose build  # Rebuild with new Rust version
docker-compose up -d  # Start in detached mode
```