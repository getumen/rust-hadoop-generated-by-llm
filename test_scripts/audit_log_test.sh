#!/bin/bash
set -e

# Setup
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
AUDIT_DB_PATH="/tmp/s3_audit_test_db"
rm -rf "$AUDIT_DB_PATH"

export AUDIT_LOG_ENABLED="true"
export AUDIT_LOG_DIR="$AUDIT_DB_PATH"
export AUDIT_LOG_BATCH_SIZE="1"
export S3_AUTH_ENABLED="true"
export S3_REGION="us-east-1"
export PORT="9005"
export MASTER_ADDR="http://localhost:50051"

echo "=== Audit Log Test ==="

# Start s3-server
echo "Starting S3 Server..."
cd "$PROJECT_ROOT"
cargo build -p s3-server
./target/debug/s3-server &
S3_PID=$!

# Cleanup on exit
trap "kill $S3_PID 2>/dev/null || true; rm -rf $AUDIT_DB_PATH" EXIT

# Wait for server
echo "Waiting for S3 Server to start..."
sleep 5

echo "Performing requests..."

# Attempt access without credentials (should fail and log as anonymous)
echo "Request 1: Anonymous access to /test-bucket"
curl -s http://localhost:9005/test-bucket > /dev/null || true

# Attempt with invalid credentials
echo "Request 2: Invalid credentials"
curl -s -H "Authorization: AWS4-HMAC-SHA256 Credential=INVALID/20240313/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=bad" http://localhost:9005/test-bucket > /dev/null || true

# Wait for log flush
sleep 2

echo "Shutting down S3 Server..."
kill $S3_PID
wait $S3_PID 2>/dev/null || true

echo "Reading Audit Logs..."
./target/debug/audit_reader "$AUDIT_DB_PATH"

echo "=== Audit Log Test Completed ==="
