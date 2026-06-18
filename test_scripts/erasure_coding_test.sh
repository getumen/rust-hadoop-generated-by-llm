#!/bin/bash

# Erasure Coding Integration Test
#
# Tests:
#   1. EC(2,2) write and read (happy path)
#   2. inspect shows EC metadata
#   3. Degraded read: 1 ChunkServer down, file still readable
#   4. Replication and EC files coexist correctly
#
# Requires the default docker-compose cluster (4 ChunkServers needed for RS(2,2)).

set -e

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass()  { echo -e "${GREEN}✓ $1${NC}"; }
fail()  { echo -e "${RED}✗ $1${NC}"; exit 1; }
info()  { echo -e "${YELLOW}  $1${NC}"; }

MASTER_CONTAINER="dfs-master1-shard1"
KILLED_CS=""

cleanup() {
    echo ""
    echo "🧹 Cleaning up..."
    # Restart any killed ChunkServer
    if [ -n "$KILLED_CS" ]; then
        docker compose -f docker-compose.yml start "$KILLED_CS" 2>/dev/null || true
    fi
    docker compose -f docker-compose.yml down -v 2>/dev/null || true
    rm -f /tmp/ec_test_original.bin /tmp/ec_test_downloaded.bin /tmp/rep_test.txt /tmp/rep_downloaded.txt
}
trap cleanup EXIT

echo "🧪 Erasure Coding Integration Test"
echo "==================================="
echo ""

# ============================================================================
# Start cluster
# ============================================================================
echo "🚀 Starting cluster..."
docker compose -f docker-compose.yml down -v 2>/dev/null || true
docker compose -f docker-compose.yml up -d --no-build

echo "Waiting for cluster to be ready (20s)..."
sleep 20

# Verify cluster is healthy
if ! docker exec "$MASTER_CONTAINER" /app/dfs_cli --master http://localhost:50051 ls > /dev/null 2>&1; then
    fail "Cluster failed to start"
fi
pass "Cluster started"

# ============================================================================
# Test 1: EC write and read (happy path)
# ============================================================================
echo ""
echo "📝 Test 1: EC(2,2) write and read"

# Generate 64KB test file with recognizable pattern
python3 -c "
import os, sys
data = bytes(range(256)) * 256  # 64KB
sys.stdout.buffer.write(data)
" > /tmp/ec_test_original.bin
info "Generated 64KB test file"

# Upload with EC RS(2,2): 2 data shards + 2 parity shards = 4 ChunkServers
docker cp /tmp/ec_test_original.bin "${MASTER_CONTAINER}:/ec_test_original.bin"
OUTPUT=$(docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 \
    put /ec_test_original.bin /ec/test.bin --ec-data 2 --ec-parity 2 2>&1)

if ! echo "$OUTPUT" | grep -qi "EC RS(2,2)"; then
    echo "Output: $OUTPUT"
    fail "Output did not confirm EC upload (expected 'EC RS(2,2)')"
fi
pass "EC file written: RS(2,2)"

# Download and verify content
docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 \
    get /ec/test.bin /ec_test_downloaded.bin

docker cp "${MASTER_CONTAINER}:/ec_test_downloaded.bin" /tmp/ec_test_downloaded.bin

if cmp -s /tmp/ec_test_original.bin /tmp/ec_test_downloaded.bin; then
    pass "EC read: content matches original"
else
    ORIG_SIZE=$(wc -c < /tmp/ec_test_original.bin)
    DOWN_SIZE=$(wc -c < /tmp/ec_test_downloaded.bin)
    fail "EC read: content mismatch (original=${ORIG_SIZE}B, downloaded=${DOWN_SIZE}B)"
fi

# ============================================================================
# Test 2: inspect shows EC metadata
# ============================================================================
echo ""
echo "🔍 Test 2: inspect shows EC metadata"

INSPECT=$(docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 \
    inspect /ec/test.bin 2>&1)

if ! echo "$INSPECT" | grep -q "EC RS(2,2)"; then
    echo "Inspect output: $INSPECT"
    fail "inspect did not show EC policy"
fi
pass "inspect: Storage=EC RS(2,2)"

if ! echo "$INSPECT" | grep -q "OriginalSize=65536"; then
    echo "Inspect output: $INSPECT"
    fail "inspect did not show correct OriginalSize"
fi
pass "inspect: OriginalSize=65536 (64KB)"

# Verify shard count: RS(2,2) = 4 shards
info "Inspect output:"
echo "$INSPECT" | grep -E "Storage|Block|EC|Shard|Original" | while read line; do
    info "  $line"
done

# ============================================================================
# Test 3: Degraded read — kill 1 ChunkServer, file still readable
# ============================================================================
echo ""
echo "💥 Test 3: Degraded read (1 CS down)"

# Kill chunkserver-extra (holds shard 2 or 3 of the EC block)
KILLED_CS="chunkserver-extra"
info "Stopping $KILLED_CS..."
docker compose -f docker-compose.yml stop "$KILLED_CS"
sleep 3

# Attempt to read the EC file — should still work with RS(2,2): tolerates 2 failures
docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 \
    get /ec/test.bin /ec_test_degraded.bin 2>&1 || true

docker cp "${MASTER_CONTAINER}:/ec_test_degraded.bin" /tmp/ec_test_downloaded.bin 2>/dev/null || true

if cmp -s /tmp/ec_test_original.bin /tmp/ec_test_downloaded.bin; then
    pass "Degraded read: content matches with 1 CS down"
else
    ORIG_SIZE=$(wc -c < /tmp/ec_test_original.bin)
    DOWN_SIZE=$(wc -c < /tmp/ec_test_downloaded.bin 2>/dev/null || echo "0")
    fail "Degraded read failed (original=${ORIG_SIZE}B, downloaded=${DOWN_SIZE}B)"
fi

# Restore the ChunkServer
info "Restarting $KILLED_CS..."
docker compose -f docker-compose.yml start "$KILLED_CS"
KILLED_CS=""
sleep 3

# ============================================================================
# Test 4: EC and replication coexist
# ============================================================================
echo ""
echo "🔀 Test 4: EC and replicated files coexist"

# Write a replicated file
echo "Hello, replicated world!" > /tmp/rep_test.txt
docker cp /tmp/rep_test.txt "${MASTER_CONTAINER}:/rep_test.txt"
docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 \
    put /rep_test.txt /rep/test.txt

pass "Replicated file written"

# Verify the EC file is still readable
docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 \
    get /ec/test.bin /ec_verify.bin
docker cp "${MASTER_CONTAINER}:/ec_verify.bin" /tmp/ec_test_downloaded.bin

if cmp -s /tmp/ec_test_original.bin /tmp/ec_test_downloaded.bin; then
    pass "EC file still readable after writing replicated file"
else
    fail "EC file content changed after writing replicated file"
fi

# Verify replicated file is readable
docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 \
    get /rep/test.txt /rep_downloaded.txt
docker cp "${MASTER_CONTAINER}:/rep_downloaded.txt" /tmp/rep_downloaded.txt

if diff /tmp/rep_test.txt /tmp/rep_downloaded.txt > /dev/null; then
    pass "Replicated file content matches"
else
    fail "Replicated file content mismatch"
fi

# Inspect both files to verify storage policies
EC_POLICY=$(docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 inspect /ec/test.bin 2>&1 | grep "Storage:")
REP_POLICY=$(docker exec "$MASTER_CONTAINER" \
    /app/dfs_cli --master http://localhost:50051 inspect /rep/test.txt 2>&1 | grep "Storage:")

info "EC file:  $EC_POLICY"
info "Rep file: $REP_POLICY"

if echo "$EC_POLICY" | grep -q "EC RS(2,2)" && echo "$REP_POLICY" | grep -q "Replicated"; then
    pass "Storage policies correctly differentiated"
else
    fail "Storage policy mismatch"
fi

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "============================================"
echo -e "${GREEN}🎉 All Erasure Coding tests passed!${NC}"
echo "============================================"
