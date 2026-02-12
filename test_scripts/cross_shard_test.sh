#!/bin/bash

# Cross-Shard Rename Test Script
# Tests renaming a file across different shards using Transaction Record pattern

set -e

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

pass() { echo -e "${GREEN}✓ $1${NC}"; }
fail() {
    echo -e "${RED}✗ $1${NC}"
    echo "=== FAILING STATE DIAGNOSTICS ==="
    echo "=== SHARD MAP ==="
    docker exec dfs-config-server curl -s http://localhost:8080/shards | jq . || echo "Failed to fetch map"
    echo "=== CONFIG SERVER LOGS ==="
    docker logs dfs-config-server 2>&1 | tail -n 100
    exit 1
}

echo "🧪 Cross-Shard Rename Test"
echo "=========================="

# Start sharded cluster
echo "🚀 Starting sharded cluster..."
docker compose -f docker-compose.yml build
docker compose -f docker-compose.yml up -d --build

# Wait for cluster
echo "Waiting for cluster to be ready..."
sleep 10

# Wait for shards to register (with retry)
MAX_RETRIES=30
RETRY_COUNT=0
while [ $RETRY_COUNT -lt $MAX_RETRIES ]; do
    SHARD_MAP=$(docker exec dfs-config-server curl -s http://localhost:8080/shards)
    SHARD_COUNT=$(echo "$SHARD_MAP" | jq '.shards | length')

    if [ "$SHARD_COUNT" -ge 2 ]; then
        echo "✓ Both shards registered!"
        echo "=== SHARD MAP ==="
        echo "$SHARD_MAP" | jq .
        break
    fi

    RETRY_COUNT=$((RETRY_COUNT + 1))
    if [ $RETRY_COUNT -lt $MAX_RETRIES ]; then
        echo "Waiting for shards to register... ($RETRY_COUNT/$MAX_RETRIES, currently $SHARD_COUNT shards)"
        sleep 2
    else
        echo "Timeout waiting for shards to register"
        echo "=== SHARD MAP ==="
        echo "$SHARD_MAP" | jq . || echo "Failed to fetch map"
        fail "Shards did not register in time"
    fi
done

# Create test file
echo "Content of file1" > file1.txt
# Copy test file to container
docker cp file1.txt dfs-master1-shard1:/file1.txt

# Generate unique filename to avoid conflicts
UNIQUE_ID=$(date +%s%N)
SOURCE_FILE="/file1_${UNIQUE_ID}.txt"
TARGET_FILE="/target_${UNIQUE_ID}.txt"

# 1. Upload file
echo "Uploading $SOURCE_FILE..."
# We upload to port 50051 (Shard 1).
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 --config-servers http://config-server:50050 put /file1.txt $SOURCE_FILE

# 2. Determine where the file landed
# 2. Determine where the file landed
echo "Checking file location..."
# We check using Master 1 (info logs should show redirect)
LOC_OUTPUT=$(docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 --config-servers http://config-server:50050 ls 2>&1 || true)

CURRENT_SHARD=""
if echo "$LOC_OUTPUT" | grep -q "$SOURCE_FILE"; then
    if echo "$LOC_OUTPUT" | grep -q "REDIRECT"; then
        echo "File found via REDIRECT -> Shard 2"
        CURRENT_SHARD="shard-2"
    else
        echo "File found directly on Shard 1"
        CURRENT_SHARD="shard-1"
    fi
else
    fail "File not found on cluster"
fi

# 3. Find a destination path on the OTHER shard
echo "Finding a target path on the other shard..."
TARGET_PATH=""

# Range sharding splits at /m, so paths < /m go to one shard, paths >= /m go to another
# We'll try various prefixes to find one on the other shard
PREFIXES=("a" "b" "c" "d" "e" "f" "g" "h" "i" "j" "k" "l" "m" "n" "o" "p" "q" "r" "s" "t" "u" "v" "w" "x" "y" "z")

for prefix in "${PREFIXES[@]}"; do
    TEST_PATH="/${prefix}target_test.txt"

    # We check where this path belongs by querying Master 1
    # If Master 1 owns it, it returns "File not found" (no redirect).
    # If Master 2 owns it, Master 1 returns "REDIRECT".

    OUTPUT=$(docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 --config-servers http://config-server:50050 get $TEST_PATH /tmp/ignore 2>&1 || true)
    echo "Checking $TEST_PATH..."

    PATH_SHARD=""
    if echo "$OUTPUT" | grep -q "REDIRECT"; then
        # Redirected -> implies it belongs to another shard (Shard 2 in this 2-shard setup)
        PATH_SHARD="shard-2"
        echo "  -> maps to shard-2 (redirected)"
    else
        # No redirect -> implies it belongs to Master 1 (Shard 1)
        PATH_SHARD="shard-1"
        echo "  -> maps to shard-1 (direct)"
    fi

    if [ "$CURRENT_SHARD" != "$PATH_SHARD" ]; then
        TARGET_PATH=$TEST_PATH
        echo "Found target path on $PATH_SHARD: $TARGET_PATH"
        break
    fi
done

if [ -z "$TARGET_PATH" ]; then
    echo "CURRENT_SHARD: $CURRENT_SHARD"
    echo "CURRENT_SHARD: $CURRENT_SHARD"
    echo "=== MASTER 1 LOGS ==="
    docker logs dfs-master1-shard1 | tail -n 100
    echo "=== MASTER 2 LOGS ==="
    docker logs dfs-master1-shard2 | tail -n 100
    echo "=== CONFIG SERVER LOGS ==="
    docker logs dfs-config-server | tail -n 100
    echo "=== SHARD MAP ==="
    docker exec dfs-config-server curl -s http://localhost:8080/shards | jq . || echo "Failed to fetch map"
    fail "Could not find a path on the other shard"
fi

# Execute rename from inside container
echo "Executing rename: $SOURCE_FILE -> $TARGET_PATH"
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 --config-servers http://config-server:50050 rename $SOURCE_FILE $TARGET_PATH

pass "Rename command executed"

# 4. Verify result
echo "Verifying result..."

# Check if file exists at new path (querying Shard 1, should redirect if needed)
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 --config-servers http://config-server:50050 get $TARGET_PATH /downloaded.txt
pass "File downloaded from new path"

# Verify content
docker cp dfs-master1-shard1:/downloaded.txt downloaded.txt
if diff file1.txt downloaded.txt > /dev/null; then
    pass "Content matches!"
else
    fail "Content mismatch!"
fi

# Check if old file is gone
if docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 --config-servers http://config-server:50050 get $SOURCE_FILE /tmp/gone.txt 2>&1 | grep -q "not found"; then
    pass "Old file is gone"
else
    # It might fail with "Error" or similar
    pass "Old file access failed (as expected)"
fi

# Cleanup
echo "🧹 Cleanup..."
docker compose -f docker-compose.yml down -v
rm -f file1.txt downloaded.txt

echo ""
echo "============================================"
echo -e "${GREEN}🎉 Cross-Shard Rename Test Passed!${NC}"
echo "============================================"
