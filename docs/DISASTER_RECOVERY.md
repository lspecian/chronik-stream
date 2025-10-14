# Disaster Recovery Guide

## Overview

Chronik Stream implements comprehensive disaster recovery capabilities by automatically uploading both **data** and **metadata** to object storage (S3/GCS/Azure). This ensures that even if you completely lose a node, all topics, partitions, consumer offsets, and high watermarks can be reconstructed from object storage.

## Architecture

### 3-Tier Storage with Full DR

```
┌─────────────────────────────────────────────────────────────────┐
│              Chronik 3-Tier Seamless Storage                     │
│            WITH Metadata Disaster Recovery (v1.3.65+)            │
├─────────────────────────────────────────────────────────────────┤
│  DATA STORAGE (Message Content):                                │
│                                                                   │
│  Tier 1: WAL (Hot - Local Disk)                                 │
│  ├─ Location: ./data/wal/{topic}/{partition}/wal_*.log          │
│  ├─ Retention: Until sealed (~30min or 256MB)                   │
│  └─ Uploaded to S3: ✅ YES (raw segments)                       │
│                                                                   │
│  Tier 2: Raw Segments in S3 (Warm)                              │
│  ├─ Location: s3://.../segments/{topic}/{partition}/*.segment   │
│  ├─ Retention: Unlimited                                         │
│  └─ Purpose: Message consumption after local WAL deletion        │
│                                                                   │
│  Tier 3: Tantivy Indexes in S3 (Cold - Searchable)              │
│  ├─ Location: s3://.../indexes/{topic}/partition-*/*.tar.gz     │
│  ├─ Retention: Unlimited                                         │
│  └─ Purpose: Full-text search, timestamp range queries          │
│                                                                   │
│  METADATA STORAGE (Topics, Offsets, Watermarks):                │
│                                                                   │
│  Local: Metadata WAL                                             │
│  ├─ Location: ./data/wal/__meta/0/wal_0_0.log                   │
│  ├─ Content: All metadata operations (events)                   │
│  └─ Uploaded to S3: ✅ YES (every 60s by default)               │
│                                                                   │
│  S3: Metadata WAL Backup                                         │
│  ├─ Location: s3://.../metadata-wal/__meta/0/*.wal              │
│  ├─ Purpose: Metadata recovery on node loss                     │
│  └─ Upload frequency: Every 60 seconds (configurable)           │
│                                                                   │
│  Local: Metadata Snapshots                                       │
│  ├─ Location: ./data/metadata_snapshots/latest.snapshot         │
│  ├─ Content: Full metadata state (every 1000 events)            │
│  └─ Uploaded to S3: ✅ YES                                      │
│                                                                   │
│  S3: Metadata Snapshot Backup                                    │
│  ├─ Location: s3://.../metadata-snapshots/*.snapshot            │
│  ├─ Purpose: Fast recovery without full WAL replay              │
│  └─ Upload frequency: When created (~every 1000 events)         │
└─────────────────────────────────────────────────────────────────┘
```

## What Gets Backed Up

### Data (Already Implemented in v1.3.64)

1. **Raw Message Segments** (Tier 2)
   - Bincode-serialized CanonicalRecords
   - Includes all message headers, keys, values, timestamps
   - Preserves original compressed wire bytes for CRC integrity
   - Location: `s3://bucket/segments/{topic}/{partition}/{min_offset}-{max_offset}.segment`

2. **Tantivy Search Indexes** (Tier 3)
   - Compressed tar.gz archives
   - Full-text searchable message content
   - Timestamp and offset indexes
   - Location: `s3://bucket/indexes/{topic}/partition-{p}/segment-{base}-{last}.tar.gz`

### Metadata (New in v1.3.65)

1. **Metadata WAL Segments**
   - All metadata operations: topic creation, offset commits, consumer groups
   - Event-sourced format (replay to restore state)
   - Location: `s3://bucket/metadata-wal/__meta/0/{timestamp}-wal_0_0.log`

2. **Metadata Snapshots**
   - Full metadata state at a point in time
   - Includes: topics, partitions, high watermarks, consumer offsets, consumer groups
   - Location: `s3://bucket/metadata-snapshots/{timestamp}.snapshot`

## Configuration

### Enable Disaster Recovery (Default: Enabled)

Metadata DR is enabled by default when using WAL-based metadata (which is also the default). No configuration required!

```bash
# Default (DR enabled)
cargo run --bin chronik-server -- standalone

# Explicitly disable DR
CHRONIK_METADATA_DR=false cargo run --bin chronik-server -- standalone

# Change upload interval (default: 60s)
CHRONIK_METADATA_UPLOAD_INTERVAL=120 cargo run --bin chronik-server -- standalone
```

### Object Store Configuration

**S3 (AWS or compatible):**
```bash
OBJECT_STORE_BACKEND=s3
S3_REGION=us-west-2
S3_BUCKET=chronik-prod
S3_ACCESS_KEY=your-access-key
S3_SECRET_KEY=your-secret-key

cargo run --bin chronik-server -- standalone
```

**MinIO (S3-compatible):**
```bash
OBJECT_STORE_BACKEND=s3
S3_ENDPOINT=http://minio:9000
S3_BUCKET=chronik-storage
S3_ACCESS_KEY=minioadmin
S3_SECRET_KEY=minioadmin
S3_PATH_STYLE=true
S3_DISABLE_SSL=true

cargo run --bin chronik-server -- standalone
```

**Google Cloud Storage:**
```bash
OBJECT_STORE_BACKEND=gcs
GCS_BUCKET=chronik-storage
GCS_PROJECT_ID=my-project

cargo run --bin chronik-server -- standalone
```

**Azure Blob Storage:**
```bash
OBJECT_STORE_BACKEND=azure
AZURE_ACCOUNT_NAME=myaccount
AZURE_CONTAINER=chronik-storage

cargo run --bin chronik-server -- standalone
```

## Disaster Recovery Scenarios

### Scenario 1: Complete Node Loss

**Problem:** EC2 instance terminated, local disk lost. All local data gone.

**Recovery Steps:**

1. **Provision new node**
   ```bash
   # New EC2 instance, empty disk
   ```

2. **Configure same S3 bucket**
   ```bash
   export OBJECT_STORE_BACKEND=s3
   export S3_REGION=us-west-2
   export S3_BUCKET=chronik-prod  # Same bucket as before
   # Credentials from IAM role or environment
   ```

3. **Start Chronik (automatic recovery)**
   ```bash
   cargo run --bin chronik-server -- standalone --advertised-addr new-host
   ```

4. **What happens automatically:**
   - Chronik detects empty local WAL
   - Downloads latest metadata snapshot from S3
   - Downloads metadata WAL segments newer than snapshot
   - Replays metadata events to restore:
     - ✅ All topics and partitions
     - ✅ High watermarks for all partitions
     - ✅ Consumer group state
     - ✅ Consumer offsets
   - Server starts normally with full state restored

5. **Verify recovery:**
   ```bash
   # List topics (should show all previous topics)
   kafka-topics --bootstrap-server new-host:9092 --list

   # Check consumer offsets (should be preserved)
   kafka-consumer-groups --bootstrap-server new-host:9092 --group my-group --describe

   # Consume from old offset (fetches from S3 if needed)
   kafka-console-consumer --bootstrap-server new-host:9092 --topic my-topic --from-beginning
   ```

### Scenario 2: Partial Data Loss

**Problem:** Local disk corruption, some WAL segments lost, but node still running.

**Recovery Steps:**

1. **Stop Chronik gracefully**
   ```bash
   # Send SIGTERM
   killall -SIGTERM chronik-server
   ```

2. **Clear corrupted local data**
   ```bash
   rm -rf ./data/wal/__meta/
   rm -rf ./data/metadata_snapshots/
   ```

3. **Restart Chronik (automatic recovery)**
   ```bash
   cargo run --bin chronik-server -- standalone
   ```

4. **Metadata recovers from S3, data already in S3**

### Scenario 3: Multi-Region Failover

**Problem:** Primary region (us-west-2) goes down. Need to failover to us-east-1.

**Setup (Before Disaster):**

1. **Enable S3 cross-region replication**
   ```bash
   # AWS CLI: Create replication rule
   aws s3api put-bucket-replication \
     --bucket chronik-prod-us-west-2 \
     --replication-configuration file://replication.json
   ```

2. **Replication config (replication.json):**
   ```json
   {
     "Role": "arn:aws:iam::account:role/replication-role",
     "Rules": [{
       "Status": "Enabled",
       "Priority": 1,
       "Destination": {
         "Bucket": "arn:aws:s3:::chronik-prod-us-east-1"
       }
     }]
   }
   ```

**Recovery (During Disaster):**

1. **Start Chronik in new region**
   ```bash
   export S3_REGION=us-east-1
   export S3_BUCKET=chronik-prod-us-east-1  # Replica bucket
   cargo run --bin chronik-server -- standalone --advertised-addr us-east-1-host
   ```

2. **Update DNS/Load Balancer**
   ```bash
   # Point kafka.example.com to us-east-1-host
   ```

3. **Clients reconnect automatically** (Kafka clients have retry logic)

### Scenario 4: Time-Based Recovery (Point-in-Time Restore)

**Problem:** Accidental metadata corruption at 10:00 AM. Need to restore to 9:00 AM state.

**Recovery Steps:**

1. **List snapshots in S3**
   ```bash
   aws s3 ls s3://chronik-prod/metadata-snapshots/
   # 1710828000.snapshot  # 9:00 AM
   # 1710831600.snapshot  # 10:00 AM (corrupted)
   ```

2. **Download specific snapshot**
   ```bash
   aws s3 cp s3://chronik-prod/metadata-snapshots/1710828000.snapshot \
     ./data/metadata_snapshots/latest.snapshot
   ```

3. **Download WAL segments between 9:00-10:00**
   ```bash
   # Manual restoration of specific WAL segments if needed
   ```

4. **Start Chronik**
   ```bash
   cargo run --bin chronik-server -- standalone
   ```

## Monitoring

### Key Metrics

1. **Metadata Upload Stats**
   - `metadata_uploader_segments_uploaded` - WAL segments uploaded
   - `metadata_uploader_snapshots_uploaded` - Snapshots uploaded
   - `metadata_uploader_bytes_uploaded` - Total bytes uploaded
   - `metadata_uploader_errors` - Upload failures

2. **Data Upload Stats** (Existing)
   - `wal_indexer_segments_processed` - Data segments uploaded
   - `wal_indexer_records_indexed` - Total records in S3
   - `wal_indexer_errors` - Indexing failures

### Logs to Watch

```bash
# Metadata uploads
grep "Metadata Uploader" logs/chronik.log

# Look for:
# - "Metadata Uploader started successfully"
# - "Uploaded metadata WAL segment"
# - "Uploaded metadata snapshot"
# - "Metadata upload run complete"

# Data uploads (existing)
grep "WAL indexer" logs/chronik.log
```

### Health Checks

```bash
# Check if metadata is being uploaded
aws s3 ls s3://chronik-prod/metadata-wal/__meta/0/ --recursive | tail -5

# Check snapshot freshness
aws s3 ls s3://chronik-prod/metadata-snapshots/ | tail -1

# Verify data uploads
aws s3 ls s3://chronik-prod/segments/ --recursive | wc -l
```

## Cost Optimization

### S3 Storage Costs

**Metadata:**
- Typical metadata WAL: ~1-10 MB per segment (depends on # operations)
- Snapshots: ~1-100 MB per snapshot (depends on # topics/partitions)
- Upload frequency: Every 60s (configurable)
- Estimated cost: **$0.01-0.10 per month** (extremely low)

**Data:**
- Raw segments: Same size as local WAL (256MB per sealed segment by default)
- Tantivy indexes: ~50-70% of raw data size (compressed)
- Lifecycle policies recommended:
  - Transition to Glacier after 30 days
  - Delete after 90 days (if not needed)

**Cost Reduction Tips:**

1. **Increase metadata upload interval**
   ```bash
   # Upload every 5 minutes instead of 1 minute
   CHRONIK_METADATA_UPLOAD_INTERVAL=300 cargo run --bin chronik-server
   ```

2. **Use S3 Lifecycle Policies**
   ```json
   {
     "Rules": [{
       "Status": "Enabled",
       "Prefix": "metadata-wal/",
       "Transitions": [{
         "Days": 7,
         "StorageClass": "GLACIER"
       }],
       "Expiration": { "Days": 30 }
     }]
   }
   ```

3. **Use S3 Intelligent-Tiering** (automatic cost optimization)

## Troubleshooting

### Issue: Metadata not uploading to S3

**Diagnosis:**
```bash
# Check logs
grep "Metadata Uploader" logs/chronik.log | grep -i error

# Check S3 permissions
aws s3 ls s3://chronik-prod/metadata-wal/
```

**Fix:**
```bash
# Verify credentials
aws sts get-caller-identity

# Verify bucket permissions
aws s3api get-bucket-policy --bucket chronik-prod

# Check IAM policy includes:
# s3:PutObject, s3:GetObject, s3:ListBucket
```

### Issue: Cold start recovery fails

**Diagnosis:**
```bash
# Check S3 for metadata files
aws s3 ls s3://chronik-prod/metadata-snapshots/
aws s3 ls s3://chronik-prod/metadata-wal/__meta/0/

# Check Chronik startup logs
grep "metadata recovery" logs/chronik.log
```

**Fix:**
```bash
# If metadata missing in S3, restore from backup
# Or start fresh (will lose topic/offset metadata)
rm -rf ./data/wal/__meta/
cargo run --bin chronik-server -- standalone
```

### Issue: Slow recovery on cold start

**Diagnosis:**
- Recovery taking > 1 minute
- Large number of metadata WAL segments to replay

**Fix:**
1. **Increase snapshot frequency** (create snapshots more often)
2. **Pre-download snapshot** before starting Chronik
   ```bash
   aws s3 cp s3://chronik-prod/metadata-snapshots/latest.snapshot \
     ./data/metadata_snapshots/latest.snapshot
   ```
3. **Use local caching** for frequent restarts

## Testing Disaster Recovery

### Simulate Complete Node Loss

```bash
# 1. Start Chronik with S3 backend
OBJECT_STORE_BACKEND=s3 \
S3_BUCKET=chronik-test \
cargo run --bin chronik-server -- standalone

# 2. Create topics and produce data
kafka-topics --create --topic test --bootstrap-server localhost:9092
echo "test message" | kafka-console-producer --topic test --bootstrap-server localhost:9092

# 3. Wait for upload (60s default)
sleep 65

# 4. Simulate disaster: kill server and delete all local data
killall chronik-server
rm -rf ./data/

# 5. Recover: restart server (same S3 bucket)
OBJECT_STORE_BACKEND=s3 \
S3_BUCKET=chronik-test \
cargo run --bin chronik-server -- standalone

# 6. Verify: topic should still exist
kafka-topics --list --bootstrap-server localhost:9092
# Should show "test"

# 7. Verify: data should be consumable
kafka-console-consumer --topic test --from-beginning --bootstrap-server localhost:9092
# Should show "test message"
```

## Comparison with Kafka

| Feature | Kafka Tiered Storage | Chronik DR |
|---------|---------------------|------------|
| **Data in S3** | ✅ Yes | ✅ Yes |
| **Metadata in S3** | ❌ NO | ✅ **YES** |
| **Auto-recovery** | ❌ Manual | ✅ Automatic |
| **Search indexes** | ❌ NO | ✅ Yes (Tantivy) |
| **Zero-config** | ❌ Complex setup | ✅ Works out-of-box |
| **Cold start time** | Minutes (manual) | Seconds (automatic) |

**Key Advantage:** Kafka's tiered storage only backs up message data. If you lose a Kafka broker, you lose all metadata (topics, offsets, consumer groups) and must manually reconstruct it. **Chronik backs up everything** and recovers automatically.

## Conclusion

Chronik's disaster recovery system provides **complete protection** against data loss:

- ✅ **Message data** backed up to S3 (Tier 2 + Tier 3)
- ✅ **Metadata** backed up to S3 (new in v1.3.65)
- ✅ **Automatic recovery** on cold start
- ✅ **Zero configuration** required (works out-of-box)
- ✅ **Multi-region support** with S3 replication
- ✅ **Cost-effective** ($0.01-0.10/month for metadata)

You can now confidently deploy Chronik in production knowing that **losing a node does NOT mean losing data**.
