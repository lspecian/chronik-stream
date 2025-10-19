#!/bin/bash
# Test script to verify object store environment variable parsing

set -e

echo "=========================================="
echo "Testing Chronik Object Store Env Vars"
echo "=========================================="
echo ""

# Build first
echo "Building chronik-server..."
cargo build --release --bin chronik-server
echo "✅ Build successful"
echo ""

# Test 1: S3/MinIO Configuration
echo "Test 1: S3/MinIO Configuration"
echo "-------------------------------------------"
export OBJECT_STORE_BACKEND=s3
export S3_ENDPOINT=http://localhost:9000
export S3_REGION=us-east-1
export S3_BUCKET=chronik-test
export S3_ACCESS_KEY=minioadmin
export S3_SECRET_KEY=minioadmin
export S3_PATH_STYLE=true
export S3_DISABLE_SSL=true

echo "Environment variables set:"
echo "  OBJECT_STORE_BACKEND=$OBJECT_STORE_BACKEND"
echo "  S3_ENDPOINT=$S3_ENDPOINT"
echo "  S3_REGION=$S3_REGION"
echo "  S3_BUCKET=$S3_BUCKET"
echo "  S3_ACCESS_KEY=$S3_ACCESS_KEY"
echo "  S3_PATH_STYLE=$S3_PATH_STYLE"
echo ""

# Start server in background and capture logs
echo "Starting server with S3 config..."
RUST_LOG=info ./target/release/chronik-server --advertised-addr localhost standalone > /tmp/chronik_s3_test.log 2>&1 &
SERVER_PID=$!
echo "Server PID: $SERVER_PID"

# Wait for startup
sleep 3

# Check logs for S3 configuration
echo ""
echo "Checking logs for S3 configuration..."
if grep -q "Configuring S3-compatible object store from environment variables" /tmp/chronik_s3_test.log; then
    echo "✅ S3 configuration detected in logs"
else
    echo "❌ S3 configuration NOT found in logs"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

if grep -q "S3 object store configured: bucket=chronik-test" /tmp/chronik_s3_test.log; then
    echo "✅ S3 bucket configuration correct"
else
    echo "❌ S3 bucket configuration NOT found"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

# Kill server
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true
echo "✅ Test 1 PASSED"
echo ""

# Test 2: Local Storage (default)
echo "Test 2: Default Local Storage"
echo "-------------------------------------------"
unset OBJECT_STORE_BACKEND
unset S3_ENDPOINT
unset S3_REGION
unset S3_BUCKET
unset S3_ACCESS_KEY
unset S3_SECRET_KEY
unset S3_PATH_STYLE
unset S3_DISABLE_SSL

echo "All OBJECT_STORE_BACKEND env vars unset (should use local default)"
echo ""

echo "Starting server with default config..."
RUST_LOG=info ./target/release/chronik-server --advertised-addr localhost standalone > /tmp/chronik_local_test.log 2>&1 &
SERVER_PID=$!
echo "Server PID: $SERVER_PID"

# Wait for startup
sleep 3

# Check logs for local storage
echo ""
echo "Checking logs for default local storage..."
if grep -q "Using default local filesystem object store" /tmp/chronik_local_test.log; then
    echo "✅ Default local storage detected"
else
    echo "❌ Default local storage NOT found in logs"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

# Kill server
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true
echo "✅ Test 2 PASSED"
echo ""

# Test 3: GCS Configuration
echo "Test 3: GCS Configuration"
echo "-------------------------------------------"
export OBJECT_STORE_BACKEND=gcs
export GCS_BUCKET=chronik-gcs-test
export GCS_PROJECT_ID=test-project

echo "Environment variables set:"
echo "  OBJECT_STORE_BACKEND=$OBJECT_STORE_BACKEND"
echo "  GCS_BUCKET=$GCS_BUCKET"
echo "  GCS_PROJECT_ID=$GCS_PROJECT_ID"
echo ""

echo "Starting server with GCS config..."
RUST_LOG=info ./target/release/chronik-server --advertised-addr localhost standalone > /tmp/chronik_gcs_test.log 2>&1 &
SERVER_PID=$!
echo "Server PID: $SERVER_PID"

# Wait for startup
sleep 3

# Check logs for GCS configuration
echo ""
echo "Checking logs for GCS configuration..."
if grep -q "Configuring Google Cloud Storage object store from environment variables" /tmp/chronik_gcs_test.log; then
    echo "✅ GCS configuration detected in logs"
else
    echo "❌ GCS configuration NOT found in logs"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

if grep -q "GCS object store configured: bucket=chronik-gcs-test" /tmp/chronik_gcs_test.log; then
    echo "✅ GCS bucket configuration correct"
else
    echo "❌ GCS bucket configuration NOT found"
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

# Kill server
kill $SERVER_PID 2>/dev/null || true
wait $SERVER_PID 2>/dev/null || true
echo "✅ Test 3 PASSED"
echo ""

echo "=========================================="
echo "All Tests PASSED! ✅"
echo "=========================================="
echo ""
echo "Summary:"
echo "  ✅ S3/MinIO environment variable parsing works"
echo "  ✅ Default local storage works when no env vars set"
echo "  ✅ GCS environment variable parsing works"
echo ""
echo "Next steps:"
echo "  1. Test with actual MinIO instance"
echo "  2. Verify Tantivy archives upload to S3"
echo "  3. Test end-to-end: produce → index → fetch from S3"
