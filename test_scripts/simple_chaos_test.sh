#!/bin/bash

# Simple chaos test - kills one server at a time (Sharded version)

set -e

echo "🧪 Simple Chaos Test (Sharded)"
echo "==============================="

COMPOSE_FILE="docker-compose.yml"
MASTER_CONTAINER="dfs-master1-shard1"

cleanup() {
    echo "🧹 Cleaning up..."
    docker compose -f $COMPOSE_FILE down -v 2>/dev/null || true
    rm -f simple_test.txt downloaded_simple.txt downloaded_recovery.txt
}
trap cleanup EXIT

# Clean up any previous state
echo "Cleaning up previous state..."
docker compose -f $COMPOSE_FILE down -v 2>/dev/null || true

# Start cluster
echo "Starting cluster..."
docker compose -f $COMPOSE_FILE up -d --build
echo "Waiting for cluster to stabilize (20s)..."
sleep 20

# Create test file
echo "Creating test file..."
echo "Hello from Rust Hadoop DFS! This is a test of the replication system." > simple_test.txt

# Upload via container
echo "Uploading file..."
docker cp simple_test.txt $MASTER_CONTAINER:/tmp/simple_test.txt
docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 put /tmp/simple_test.txt /simple.txt

echo "✓ File uploaded"

# List files
echo ""
echo "Files in DFS:"
docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 ls || true

# Kill a chunkserver
echo ""
echo "💀 Stopping chunkserver1-shard1..."
docker compose -f $COMPOSE_FILE stop chunkserver1-shard1

# Try to download (may fail if file is on shard1)
echo ""
echo "📥 Downloading file with one server down..."
if docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 get /simple.txt /tmp/downloaded_simple.txt; then
    docker cp $MASTER_CONTAINER:/tmp/downloaded_simple.txt ./downloaded_simple.txt
    echo "✓ File downloaded successfully!"

    # Verify
    echo ""
    echo "Verifying content..."
    if diff simple_test.txt downloaded_simple.txt > /dev/null; then
        echo "✅ Content matches! Replication works!"
    else
        echo "❌ Content mismatch!"
        exit 1
    fi
else
    echo "⚠️  Download failed (expected if file was on stopped ChunkServer)"
fi

# Restart server
echo ""
echo "♻️  Restarting chunkserver1-shard1..."
docker compose -f $COMPOSE_FILE start chunkserver1-shard1
sleep 5

# Verify download works after recovery
echo ""
echo "📥 Downloading file after recovery..."
docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 get /simple.txt /tmp/downloaded_recovery.txt
docker cp $MASTER_CONTAINER:/tmp/downloaded_recovery.txt ./downloaded_recovery.txt
echo "✓ File downloaded after recovery!"

if diff simple_test.txt downloaded_recovery.txt > /dev/null; then
    echo "✅ Content verified after recovery!"
else
    echo "❌ Content mismatch after recovery!"
    exit 1
fi

echo ""
echo "🎉 Simple chaos test passed!"
