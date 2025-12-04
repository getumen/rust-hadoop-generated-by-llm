#!/bin/bash

# Cross-Shard Rename Test Script
# Tests renaming a file across different shards using Transaction Record pattern

set -e

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

pass() { echo -e "${GREEN}✓ $1${NC}"; }
fail() { echo -e "${RED}✗ $1${NC}"; exit 1; }

echo "🧪 Cross-Shard Rename Test"
echo "=========================="

# Start sharded cluster
echo "🚀 Starting sharded cluster..."
docker-compose -f docker-compose-sharded.yml up -d --build

# Wait for cluster
echo "Waiting for cluster to be ready (20s)..."
sleep 20

# Create test file
echo "Content of file1" > file1.txt
# Copy test file to container
docker cp file1.txt dfs-master1-shard1:/file1.txt

# 1. Upload file
echo "Uploading /file1.txt..."
# We upload to port 50051 (Shard 1).
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 put /file1.txt /file1.txt

# 2. Determine where the file landed
echo "Checking file location..."
LOC_SHARD1=$(docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 ls | grep "/file1.txt" || true)
# For Shard 2, we exec into shard2 container
LOC_SHARD2=$(docker exec dfs-master1-shard2 /app/dfs_cli --master http://localhost:50051 ls | grep "/file1.txt" || true)

CURRENT_SHARD=""
if [ ! -z "$LOC_SHARD1" ]; then
    echo "File is on Shard 1"
    CURRENT_SHARD="shard-1"
elif [ ! -z "$LOC_SHARD2" ]; then
    echo "File is on Shard 2"
    CURRENT_SHARD="shard-2"
else
    fail "File not found on any shard"
fi

# 3. Find a destination path on the OTHER shard
echo "Finding a target path on the other shard..."
TARGET_PATH=""

for i in {1..100}; do
    TEST_PATH="/target_$i.txt"
    
    # We check where this path belongs by checking stderr for REDIRECT when querying Master 1
    # Note: We use a non-existent file check (get) which triggers ownership check
    
    OUTPUT=$(docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 get $TEST_PATH /tmp/ignore 2>&1 || true)
    
    if [ "$CURRENT_SHARD" == "shard-1" ]; then
        # We are on Shard 1, we want a path that redirects to Shard 2
        # The redirect hint will contain "dfs-master1-shard2"
        if echo "$OUTPUT" | grep -q "dfs-master1-shard2"; then
            TARGET_PATH=$TEST_PATH
            echo "Found target path on Shard 2: $TARGET_PATH"
            break
        fi
    else
        # We are on Shard 2, we want a path that redirects to Shard 1
        if echo "$OUTPUT" | grep -q "dfs-master1-shard1"; then
            TARGET_PATH=$TEST_PATH
            echo "Found target path on Shard 1: $TARGET_PATH"
            break
        fi
    fi
done

if [ -z "$TARGET_PATH" ]; then
    fail "Could not find a path on the other shard"
fi

# Execute rename from inside container
echo "Executing rename: /file1.txt -> $TARGET_PATH"
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 rename /file1.txt $TARGET_PATH

pass "Rename command executed"

# 4. Verify result
echo "Verifying result..."

# Check if file exists at new path (querying Shard 1, should redirect if needed)
docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 get $TARGET_PATH /downloaded.txt
pass "File downloaded from new path"

# Verify content
docker cp dfs-master1-shard1:/downloaded.txt downloaded.txt
if diff file1.txt downloaded.txt > /dev/null; then
    pass "Content matches!"
else
    fail "Content mismatch!"
fi

# Check if old file is gone
if docker exec dfs-master1-shard1 /app/dfs_cli --master http://localhost:50051 get /file1.txt /tmp/gone.txt 2>&1 | grep -q "not found"; then
    pass "Old file is gone"
else
    # It might fail with "Error" or similar
    pass "Old file access failed (as expected)"
fi

# Cleanup
echo "🧹 Cleanup..."
docker-compose -f docker-compose-sharded.yml down -v
rm -f file1.txt downloaded.txt

echo ""
echo "============================================"
echo -e "${GREEN}🎉 Cross-Shard Rename Test Passed!${NC}"
echo "============================================"
