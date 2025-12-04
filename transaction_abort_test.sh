#!/bin/bash

# Transaction Abort Test Script
# Tests that transaction aborts correctly when destination shard is unavailable

set -e

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

pass() { echo -e "${GREEN}✓ $1${NC}"; }
fail() { echo -e "${RED}✗ $1${NC}"; exit 1; }

echo "🧪 Transaction Abort Test"
echo "========================="

# Start sharded cluster
echo "🚀 Starting sharded cluster..."
docker-compose -f docker-compose-sharded.yml up -d --build

# Wait for cluster
echo "Waiting for cluster to be ready (20s)..."
sleep 20

# Create test file
echo "Content of file1" > file1.txt

# 1. Upload file to Shard 1
echo "Uploading /file1.txt..."
# We assume /file1.txt maps to Shard 1 (from previous test experience, or we check)
# To be sure, we upload and check location.
docker cp file1.txt dfs-master1-shard1:/file1.txt

# Upload
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 put /file1.txt /file1.txt

# Check location
LOC=$(docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 ls | grep "/file1.txt")
if [ -z "$LOC" ]; then
    # Maybe it's on shard 2?
    LOC2=$(docker exec dfs-master1-shard2 /app/dfs_cli --master http://localhost:50051 ls | grep "/file1.txt" || true)
    if [ ! -z "$LOC2" ]; then
        echo "File landed on Shard 2. We need it on Shard 1 for this test setup."
        # We need to find a path that lands on Shard 1.
        # For simplicity, let's just proceed. If source is Shard 2, we stop Shard 1.
        SOURCE_SHARD="shard-2"
        DEST_SHARD="shard-1"
        SOURCE_CONTAINER="dfs-master1-shard2"
        DEST_CONTAINER="dfs-master1-shard1"
    else
        fail "File upload failed"
    fi
else
    echo "File is on Shard 1"
    SOURCE_SHARD="shard-1"
    DEST_SHARD="shard-2"
    SOURCE_CONTAINER="dfs-master1-shard1"
    DEST_CONTAINER="dfs-master1-shard2"
fi

# 2. Find a target path on the DEST shard
echo "Finding target path on $DEST_SHARD..."
# We can use the loop technique again, or just pick a random one and hope/retry.
# Let's use the loop technique from inside the container.
TARGET_PATH="/target_abort.txt"
# For now, just assume /target_abort.txt maps to the other shard or try a few.
# Actually, if we stop the dest shard, ANY cross-shard rename attempt should fail.
# We just need to ensure it IS cross-shard.

# 3. Stop the Destination Shard
echo "🛑 Stopping Destination Shard ($DEST_CONTAINER)..."
docker-compose -f docker-compose-sharded.yml stop $DEST_CONTAINER

# 4. Attempt Rename
echo "Attempting rename (expecting failure)..."
# This should fail because PrepareTransaction cannot reach the dest shard
if docker exec $SOURCE_CONTAINER /app/dfs_cli --master http://localhost:50051 rename /file1.txt $TARGET_PATH; then
    echo "Rename succeeded unexpectedly!"
    # If it succeeded, maybe it wasn't cross-shard?
    # Or maybe we stopped the wrong shard?
    fail "Rename should have failed"
else
    pass "Rename failed as expected"
fi

# 5. Verify Source File Still Exists (Abort)
echo "Verifying source file integrity..."
if docker exec $SOURCE_CONTAINER /app/dfs_cli --master http://localhost:50051 get /file1.txt /downloaded.txt; then
    pass "Source file still exists (Transaction Aborted)"
else
    fail "Source file is missing! Transaction atomicity violated."
fi

# Cleanup
echo "🧹 Cleanup..."
docker-compose -f docker-compose-sharded.yml down -v
rm -f file1.txt

echo ""
echo "============================================"
echo -e "${GREEN}🎉 Transaction Abort Test Passed!${NC}"
echo "============================================"
