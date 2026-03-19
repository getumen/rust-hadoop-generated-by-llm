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
export AUDIT_HMAC_SECRET="test_audit_hmac_secret_key_16bytes"

echo "=== Audit Log Test ==="

# Start s3-server
echo "Starting S3 Server..."
cd "$PROJECT_ROOT"
cargo build -p s3-server --bin s3-server --bin audit_reader
./target/debug/s3-server &
S3_PID=$!

cleanup() {
    echo "🧹 Cleaning up..."
    [ -n "$S3_PID" ] && kill $S3_PID 2>/dev/null || true
    pkill -f "target/debug/s3-server" || true
    rm -rf "$AUDIT_DB_PATH"
}

trap cleanup EXIT

# Wait for server readiness by polling /health
echo "Waiting for S3 Server to start..."
for i in $(seq 1 30); do
    if curl -s -o /dev/null -w '%{http_code}' http://localhost:9005/health | grep -q 200; then
        echo "  Server is ready (attempt $i)"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "Error: S3 Server did not start within 30 seconds"
        exit 1
    fi
    sleep 1
done

echo "Performing requests..."

# Request 1: Anonymous access without credentials (should fail and log as anonymous)
echo "Request 1: Anonymous access to /test-bucket"
curl -s http://localhost:9005/test-bucket > /dev/null || true

# Request 2: Invalid credentials with required SigV4 headers
echo "Request 2: Invalid credentials with x-amz-date"
curl -s \
    -H "Authorization: AWS4-HMAC-SHA256 Credential=INVALID/20240313/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=bad" \
    -H "x-amz-date: 20240313T120000Z" \
    http://localhost:9005/test-bucket > /dev/null || true

# Wait for log flush
sleep 2

echo "Shutting down S3 Server..."
kill $S3_PID
wait $S3_PID 2>/dev/null || true

echo "Reading Audit Logs..."
OUTPUT=$(./target/debug/audit_reader "$AUDIT_DB_PATH" --json)
echo "$OUTPUT"

echo "Verifying Audit Hash Chain..."
./target/debug/audit_reader "$AUDIT_DB_PATH" --verify-chain "$AUDIT_HMAC_SECRET" | grep -q "Hash Chain Verification Successful!"
echo "✓ Audit Log Chain verified"

# Verify that audit records were actually written
RECORD_COUNT=$(echo "$OUTPUT" | wc -l | xargs)
if [ "$RECORD_COUNT" -lt 2 ]; then
    echo "Error: Expected at least 2 audit records, got $RECORD_COUNT"
    exit 1
fi

echo "✓ Found $RECORD_COUNT audit records"
echo "=== Audit Log Test PASSED ==="
