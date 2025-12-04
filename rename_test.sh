#!/bin/bash

# Rename Test Script - Tests same-shard and cross-shard rename operations
# using Transaction Record pattern

set -e

echo "🧪 Rename Test"
echo "=============="

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m' # No Color

# Helper function
pass() {
    echo -e "${GREEN}✓ $1${NC}"
}

fail() {
    echo -e "${RED}✗ $1${NC}"
    exit 1
}

# Create test file
echo "Creating test file..."
echo "Hello from Rust Hadoop DFS! Testing rename operation." > rename_test.txt

# Start cluster
echo "🚀 Starting cluster..."
docker-compose up -d --build
echo "Waiting for cluster (15s)..."
sleep 15

# Copy test file to container
echo "Copying test file to container..."
docker cp rename_test.txt dfs-master1:/rename_test.txt

# ============================================================================
# Test 1: Upload a file
# ============================================================================
echo ""
echo "📁 Test 1: Upload file"
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 put /rename_test.txt /original.txt
pass "File uploaded as /original.txt"

# List files
echo ""
echo "Files in DFS:"
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 ls

# ============================================================================
# Test 2: Same-shard rename
# ============================================================================
echo ""
echo "📝 Test 2: Same-shard rename"
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 rename /original.txt /renamed.txt
pass "File renamed to /renamed.txt"

# Verify the file is renamed
echo ""
echo "Files after rename:"
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 ls

# Check original file no longer exists (should fail or show not found)
echo ""
echo "Verifying original file is gone..."
if docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 get /original.txt /tmp/should_not_exist.txt 2>&1 | grep -q "not found\|Error"; then
    pass "Original file correctly removed"
else
    fail "Original file should not exist after rename"
fi

# Download renamed file and verify content
echo ""
echo "📥 Downloading renamed file..."
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 get /renamed.txt /renamed_downloaded.txt
pass "Renamed file downloaded"

# Verify content
echo ""
echo "Verifying content..."
docker cp dfs-master1:/renamed_downloaded.txt renamed_downloaded.txt
if diff rename_test.txt renamed_downloaded.txt > /dev/null; then
    pass "Content matches after rename!"
else
    fail "Content mismatch after rename!"
fi

# ============================================================================
# Test 3: Rename to a nested path
# ============================================================================
echo ""
echo "📂 Test 3: Rename to nested path"
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 rename /renamed.txt /folder/nested/file.txt
pass "File renamed to /folder/nested/file.txt"

# Verify
echo ""
echo "Files after nested rename:"
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 ls

# Download and verify content again
echo ""
echo "📥 Downloading nested file..."
docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 get /folder/nested/file.txt /nested_downloaded.txt
pass "Nested file downloaded"

docker cp dfs-master1:/nested_downloaded.txt nested_downloaded.txt
if diff rename_test.txt nested_downloaded.txt > /dev/null; then
    pass "Content matches for nested file!"
else
    fail "Content mismatch for nested file!"
fi

# ============================================================================
# Test 4: Rename non-existent file (should fail)
# ============================================================================
echo ""
echo "❌ Test 4: Rename non-existent file (expected to fail)"
if docker exec dfs-master1 /app/dfs_cli --master http://localhost:50051 rename /does_not_exist.txt /another.txt 2>&1 | grep -qi "not found\|error\|failed"; then
    pass "Correctly rejected rename of non-existent file"
else
    fail "Should have rejected rename of non-existent file"
fi

# ============================================================================
# Cleanup
# ============================================================================
echo ""
echo "🧹 Cleanup..."
docker-compose down -v
rm -f rename_test.txt renamed_downloaded.txt nested_downloaded.txt

echo ""
echo "============================================"
echo -e "${GREEN}🎉 All rename tests passed!${NC}"
echo "============================================"
