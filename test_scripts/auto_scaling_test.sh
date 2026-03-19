#!/bin/bash

# Shard Auto-scaling Integration Test Script
# Verifies shard split and merge triggers.

set -e

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

cleanup() {
    echo "🧹 Cleaning up..."
    docker compose -f docker-compose.auto-scaling.yml down -v || true
    pkill -f "target/release/master" || true
}

trap cleanup EXIT

pass() { echo -e "${GREEN}✓ $1${NC}"; }
fail() { echo -e "${RED}✗ $1${NC}"; exit 1; }

echo "🧪 Shard Auto-scaling Test"
echo "=========================="

# Cleanup initially
cleanup

# Start cluster
echo "🚀 Starting cluster..."
docker compose -f docker-compose.auto-scaling.yml up -d --build --force-recreate

# Wait for readiness
echo "Waiting for cluster readiness (20s)..."
sleep 20

# 1. Initial State: 1 Shard
echo "Checking initial shard count..."
for i in $(seq 1 30); do
    SHARD_COUNT=$(curl -s http://localhost:8090/shards | jq '.shards | length' 2>/dev/null || echo 0)
    if [ "${SHARD_COUNT:-0}" -eq 1 ]; then
        break
    fi
    sleep 1
done

if [ "${SHARD_COUNT:-0}" -eq 1 ]; then
    pass "Initial shard count is 1"
else
    fail "Expected 1 shard, found $SHARD_COUNT"
fi

# 2. Generate Load to Trigger Split
echo "Generating hot prefix load on /hot/ (target > 10 RPS)..."
# We'll use a simple loop of 'ls' or 'get_file_info' calls
# Since split detection happens every 5s, we need to sustain load for a bit.

GENERATE_LOAD() {
    local duration=$1
    local rps=$2
    # Use a single docker exec to run a bash script that generates load
    docker exec dfs-master-test bash -c "
        end=\$((SECONDS + $duration))
        while [ \$SECONDS -lt \$end ]; do
            for i in \$(seq 1 $rps); do
                /app/dfs_cli --master http://localhost:50051 inspect /hot/file_\$i > /dev/null 2>&1 &
            done
            sleep 1
        done
        wait
    "
}

echo "Generating load for 20 seconds..."
GENERATE_LOAD 20 10

echo "Checking for shard split..."
# Wait up to 30 seconds for split and propagation
for i in $(seq 1 30); do
    SHARD_COUNT=$(curl -s http://localhost:8090/shards | jq '.shards | length' 2>/dev/null || echo 0)
    if [ "${SHARD_COUNT:-0}" -gt 1 ]; then
        break
    fi
    sleep 1
done

if [ "${SHARD_COUNT:-0}" -gt 1 ]; then
    pass "Shard split triggered! New shard count: $SHARD_COUNT"
else
    # Check logs if failed
    docker logs dfs-master-test | grep "Hot prefix detected" || echo "No 'Hot prefix detected' in logs"
    fail "Shard split not triggered"
fi

# 3. Stop Load and Wait for Merge
echo "Stopping load to trigger merge (< 1 RPS)..."
echo "Wait for merge (up to 30s)..."

for i in $(seq 1 30); do
    SHARD_COUNT=$(curl -s http://localhost:8090/shards | jq '.shards | length' 2>/dev/null || echo 0)
    if [ "${SHARD_COUNT:-0}" -eq 1 ]; then
        break
    fi
    sleep 2 # Merge threshold check might be slower
done

if [ "${SHARD_COUNT:-0}" -eq 1 ]; then
    pass "Shard merge triggered! Shard count returned to 1"
else
    docker logs dfs-master-test | grep "underutilized" || echo "No 'underutilized' in logs"
    echo "=== MASTER LOGS ==="
    docker logs dfs-master-test
    echo "=== CONFIG LOGS ==="
    docker logs dfs-config-server-test
    fail "Shard merge not triggered. Shard count: $SHARD_COUNT"
fi

# No manual cleanup needed here, trap handles it.

echo ""
echo "============================================"
echo -e "${GREEN}🎉 Shard Auto-scaling Test Passed!${NC}"
echo "============================================"
