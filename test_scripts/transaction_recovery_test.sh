#!/bin/bash
set -e

echo "=== Transaction Recovery Test ==="
echo "Tests: (1) normal cross-shard rename, (2) coordinator restart recovery"

COMPOSE_FILE="docker-compose.yml"

cleanup() {
    echo "Cleaning up..."
    docker compose -f $COMPOSE_FILE down -v 2>/dev/null || true
    rm -f /tmp/recovery_test_*.txt 2>/dev/null || true
}
trap cleanup EXIT

# Start cluster
echo "Starting cluster..."
docker compose -f $COMPOSE_FILE up -d --no-build
echo "Waiting for cluster to stabilize (20s)..."
sleep 20

# ========================================
# Test 1: Normal cross-shard rename
# ========================================
echo ""
echo "--- Test 1: Normal cross-shard rename ---"
docker exec dfs-master1-shard1 sh -c 'echo "recovery test data" > /tmp/test_file.txt'
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 put /tmp/test_file.txt /a_recovery_test.txt

# Cross-shard rename: /a_... (shard-1) → /z_... (shard-2)
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 rename /a_recovery_test.txt /z_recovery_test.txt

# Verify destination exists
DEST_CHECK=$(docker exec dfs-master1-shard2 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
echo "$DEST_CHECK" | grep -q "z_recovery_test.txt" || { echo "FAIL: dest not found"; exit 1; }

# Verify source gone
SOURCE_CHECK=$(docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
if echo "$SOURCE_CHECK" | grep -q "a_recovery_test.txt"; then
    echo "FAIL: source still exists"; exit 1
fi
echo "PASS: Normal rename succeeded"

# ========================================
# Test 2: Coordinator restart recovery
# ========================================
echo ""
echo "--- Test 2: Coordinator restart recovery ---"

# Upload another file
docker exec dfs-master1-shard1 sh -c 'echo "restart test" > /tmp/restart_file.txt'
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 put /tmp/restart_file.txt /a_restart_test.txt

# Kill coordinator (shard-1) abruptly
echo "Killing coordinator (master1-shard1)..."
docker kill dfs-master1-shard1

# Wait briefly
sleep 5

# Restart coordinator
echo "Restarting coordinator..."
docker compose -f $COMPOSE_FILE start master1-shard1
echo "Waiting for coordinator recovery (30s)..."
sleep 30

# Verify the file is still accessible after restart
echo "Verifying file survives coordinator restart..."
FILE_CHECK=$(docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
echo "$FILE_CHECK" | grep -q "a_restart_test.txt" || { echo "FAIL: file lost after restart"; exit 1; }

# Now do a cross-shard rename after restart to verify 2PC works post-recovery
echo "Cross-shard rename after restart..."
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 rename /a_restart_test.txt /z_restart_test.txt

# Verify
DEST_CHECK2=$(docker exec dfs-master1-shard2 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
echo "$DEST_CHECK2" | grep -q "z_restart_test.txt" || { echo "FAIL: rename after restart failed"; exit 1; }
echo "PASS: Rename after coordinator restart succeeded"

echo ""
echo "=== Transaction Recovery Test PASSED ==="
