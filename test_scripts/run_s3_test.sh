#!/bin/bash
set -e

# Change to project root directory
cd "$(dirname "$0")/.."

echo "=== S3 Integration Test Runner ==="

# 1. Install Requirements
echo "Installing Python dependencies..."
pip install --trusted-host pypi.org --trusted-host files.pythonhosted.org -r test_scripts/requirements.txt

# 2. Build and Start Cluster
echo "Cleaning up old data..."
docker compose -f test_scripts/spark-s3-test/docker-compose.yml down -v
echo "Starting Cluster..."
docker compose -f test_scripts/spark-s3-test/docker-compose.yml up -d --no-build

# 3. Wait for S3 Server
echo "Waiting for S3 Server (port 9000)..."
RETRIES=30
while [ $RETRIES -gt 0 ]; do
    if nc -z localhost 9000; then
        echo "S3 Server is ready!"
        break
    fi
    echo "Waiting... ($RETRIES)"
    sleep 2
    RETRIES=$((RETRIES-1))
done

if [ $RETRIES -eq 0 ]; then
    echo "Error: S3 Server failed to start."
    docker compose logs s3-server
    docker compose -f test_scripts/spark-s3-test/docker-compose.yml down -v
    exit 1
fi

# 4. Wait for Raft leader election
echo "Waiting for Raft leader election..."
for i in $(seq 1 30); do
    LEADER=$(curl -s http://localhost:8080/raft/state 2>/dev/null | grep -o '"role":"Leader"' || true)
    if [ -n "$LEADER" ]; then
        echo "  Raft leader elected (attempt $i)"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "Warning: Raft leader not confirmed after 30s, proceeding anyway..."
    fi
    sleep 1
done
# Extra buffer after leader election
sleep 3

# 4. Run Test
echo "Running Integration Test..."

set +e
python3 test_scripts/s3_integration_test.py > test_output.log 2>&1
EXIT_CODE=$?
set -e

# Check Failure and Log
if [ $EXIT_CODE -ne 0 ]; then
    echo "=== TEST FAILED (Exit Code: $EXIT_CODE) ==="
    echo "--- Python Test Output ---"
    cat test_output.log
    echo "--- S3 Server Logs ---"
    docker compose -f test_scripts/spark-s3-test/docker-compose.yml logs s3-server
    echo "--- Master Logs ---"
    docker compose -f test_scripts/spark-s3-test/docker-compose.yml logs master1-shard1
fi

# 5. Presigned URL Test
echo ""
echo "=== Presigned URL Test ==="

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

presign_pass() {
    echo -e "${GREEN}✓ $1${NC}"
}

presign_fail() {
    echo -e "${RED}✗ $1${NC}"
    PRESIGN_FAILED=1
}

PRESIGN_FAILED=0
S3_ENDPOINT="http://localhost:9000"
export AWS_ACCESS_KEY_ID="dummy"
export AWS_SECRET_ACCESS_KEY="dummy"
export AWS_REGION="us-east-1"
export S3_ENDPOINT
PRESIGN_BUCKET="presign-test-bucket"
PRESIGN_KEY="presign-test-object.txt"
PRESIGN_CONTENT="Hello from presigned URL test"
DFS_CLI="./target/release/dfs_cli"

# Build dfs_cli if not present
if [ ! -f "$DFS_CLI" ]; then
    echo "Building dfs_cli..."
    cargo build -p dfs-client --release 2>/dev/null || cargo build --release 2>/dev/null
fi
if [ ! -f "$DFS_CLI" ]; then
    echo "✗ Failed to build dfs_cli binary at $DFS_CLI" >&2
    exit 1
fi

# 5a. Create bucket and upload test object via direct curl PUT (auth disabled on S3 server)
echo "Creating bucket '$PRESIGN_BUCKET'..."
curl -s -X PUT "$S3_ENDPOINT/$PRESIGN_BUCKET" -o /dev/null || true

UPLOAD_STATUS=$(curl -s -o /dev/null -w '%{http_code}' -X PUT \
    -H "Content-Type: text/plain" \
    -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" \
    --data-raw "$PRESIGN_CONTENT" \
    "$S3_ENDPOINT/$PRESIGN_BUCKET/$PRESIGN_KEY")
if [ "$UPLOAD_STATUS" = "200" ] || [ "$UPLOAD_STATUS" = "204" ] || [ "$UPLOAD_STATUS" = "201" ]; then
    presign_pass "Object uploaded to s3://$PRESIGN_BUCKET/$PRESIGN_KEY"
else
    presign_fail "Object upload failed (HTTP $UPLOAD_STATUS)"
fi

# 5a-verify. Confirm content is correct before testing presigned URL
if [ $PRESIGN_FAILED -eq 0 ]; then
    VERIFY_CONTENT=$(curl -s -f "$S3_ENDPOINT/$PRESIGN_BUCKET/$PRESIGN_KEY" 2>/dev/null)
    if echo "$VERIFY_CONTENT" | grep -q "$PRESIGN_CONTENT"; then
        presign_pass "Upload verified (direct GET returned correct content)"
    else
        presign_fail "Upload verification failed: direct GET returned '$VERIFY_CONTENT'"
    fi
fi

# 5b. Generate presigned GET URL
echo "Generating presigned GET URL..."
set +e
PRESIGNED_GET_URL=$("$DFS_CLI" presign "s3://$PRESIGN_BUCKET/$PRESIGN_KEY" --method GET --expires 300 2>&1)
PRESIGN_CLI_EXIT=$?
set -e
if [ $PRESIGN_CLI_EXIT -ne 0 ] || [ -z "$PRESIGNED_GET_URL" ]; then
    presign_fail "Failed to generate presigned GET URL: $PRESIGNED_GET_URL"
else
    presign_pass "Generated presigned GET URL: $PRESIGNED_GET_URL"
fi

# 5c. Download using presigned GET URL (no auth headers)
if [ $PRESIGN_FAILED -eq 0 ]; then
    echo "Downloading object via presigned GET URL (no auth headers)..."
    DOWNLOADED_CONTENT=$(curl -s -f "$PRESIGNED_GET_URL" 2>/dev/null)
    CURL_EXIT=$?
    if [ $CURL_EXIT -ne 0 ]; then
        presign_fail "Presigned GET failed (curl exit code: $CURL_EXIT)"
    elif echo "$DOWNLOADED_CONTENT" | grep -q "$PRESIGN_CONTENT"; then
        presign_pass "Presigned GET returned correct content"
    else
        presign_fail "Presigned GET returned unexpected content: '$DOWNLOADED_CONTENT'"
    fi
fi

# 5d. Generate presigned DELETE URL
echo "Generating presigned DELETE URL..."
set +e
PRESIGNED_DELETE_URL=$("$DFS_CLI" presign "s3://$PRESIGN_BUCKET/$PRESIGN_KEY" --method DELETE --expires 300 2>&1)
PRESIGN_CLI_EXIT=$?
set -e
if [ $PRESIGN_CLI_EXIT -ne 0 ] || [ -z "$PRESIGNED_DELETE_URL" ]; then
    presign_fail "Failed to generate presigned DELETE URL: $PRESIGNED_DELETE_URL"
else
    presign_pass "Generated presigned DELETE URL"
fi

# 5e. Delete using presigned DELETE URL (no auth headers)
if [ $PRESIGN_FAILED -eq 0 ]; then
    echo "Deleting object via presigned DELETE URL (no auth headers)..."
    DELETE_RESPONSE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$PRESIGNED_DELETE_URL" 2>/dev/null)
    if [ "$DELETE_RESPONSE" = "204" ] || [ "$DELETE_RESPONSE" = "200" ]; then
        presign_pass "Presigned DELETE succeeded (HTTP $DELETE_RESPONSE)"
    else
        presign_fail "Presigned DELETE returned unexpected HTTP status: $DELETE_RESPONSE"
    fi
fi

# 5f. Verify object is gone (GET should return 404)
if [ $PRESIGN_FAILED -eq 0 ]; then
    echo "Verifying object is deleted (GET should return 404)..."
    VERIFY_STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$PRESIGNED_GET_URL" 2>/dev/null)
    if [ "$VERIFY_STATUS" = "404" ] || [ "$VERIFY_STATUS" = "403" ]; then
        presign_pass "Object correctly not found after DELETE (HTTP $VERIFY_STATUS)"
    else
        presign_fail "Object still accessible after DELETE (HTTP $VERIFY_STATUS)"
    fi
fi

if [ $PRESIGN_FAILED -ne 0 ]; then
    echo "=== PRESIGNED URL TEST FAILED ==="
    EXIT_CODE=1
else
    echo "=== PRESIGNED URL TEST PASSED ==="
fi

# 6. Cleanup
echo "Stopping Cluster..."
docker compose -f test_scripts/spark-s3-test/docker-compose.yml down -v

# 7. Report/Exit
if [ $EXIT_CODE -ne 0 ]; then
    exit 1
else
    echo "=== TEST PASSED ==="
    exit 0
fi
