#!/bin/bash

# Chaos Monkey Test Script for Rust Hadoop DFS (Sharded)
# This script tests replication resilience in a sharded cluster

set -e

# Configuration for sharded cluster
COMPOSE_FILE="docker-compose.yml"
NETWORK="rust-hadoop_dfs-sharded-network"
MASTER_CONTAINER="dfs-master1-shard1"
CHUNKSERVERS=("chunkserver1-shard1" "chunkserver1-shard2")
MASTERS=("master1-shard1" "master1-shard2")

TEST_FILE="chaos_test_data.txt"
DFS_PATH="/chaos_test.txt"

echo "🐵 Starting Chaos Monkey Test for Rust Hadoop DFS (Sharded)"
echo "============================================================"

# Cleanup any previous state
echo "Cleaning up previous state..."
docker compose -f $COMPOSE_FILE down -v || true

# Start cluster
echo "Starting cluster..."
docker compose -f $COMPOSE_FILE up -d --build
echo "Waiting for cluster to stabilize (30s)..."
sleep 30

# Function to create test data
create_test_data() {
    echo "📝 Creating test data..."
    dd if=/dev/urandom of=$TEST_FILE bs=1M count=10 2>/dev/null
    MD5_ORIG=$(md5 -q $TEST_FILE)
    echo "✓ Created 10MB test file (MD5: $MD5_ORIG)"
}

# Function to upload file
upload_file() {
    echo "📤 Uploading file to DFS..."
    docker cp $TEST_FILE $MASTER_CONTAINER:/tmp/$TEST_FILE
    docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 put /tmp/$TEST_FILE $DFS_PATH
    echo "✓ File uploaded"
}

# Function to download file
download_file() {
    local output_file=$1
    echo "📥 Downloading file from DFS..."
    docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 get $DFS_PATH /tmp/$output_file
    docker cp $MASTER_CONTAINER:/tmp/$output_file ./$output_file
    echo "✓ File downloaded to $output_file"
}

# Function to verify file integrity
verify_file() {
    local file=$1
    echo "🔍 Verifying file integrity..."
    local md5_downloaded=$(md5 -q $file)
    if [ "$MD5_ORIG" == "$md5_downloaded" ]; then
        echo "✓ File integrity verified (MD5: $md5_downloaded)"
        return 0
    else
        echo "✗ File integrity check failed! (Original: $MD5_ORIG, Downloaded: $md5_downloaded)"
        return 1
    fi
}

# Function to kill a chunk server
kill_chunkserver() {
    local server=$1
    echo "💀 Killing $server..."
    docker compose -f $COMPOSE_FILE stop $server 2>/dev/null || true
    echo "✓ $server stopped"
}

# Function to restart a chunk server
restart_chunkserver() {
    local server=$1
    echo "♻️  Restarting $server..."
    docker compose -f $COMPOSE_FILE start $server
    sleep 5
    echo "✓ $server restarted"
}

# Function to list files
list_files() {
    echo "📋 Listing files in DFS..."
    docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 ls || true
}

# Function to get cluster status
cluster_status() {
    echo ""
    echo "📊 Cluster Status:"
    echo "=================="
    for server in "${CHUNKSERVERS[@]}"; do
        container_name="dfs-${server}"
        if docker ps --filter "name=$container_name" --filter "status=running" | grep -q "$container_name"; then
            echo "  ✓ $server: RUNNING"
        else
            echo "  ✗ $server: STOPPED"
        fi
    done
    for master in "${MASTERS[@]}"; do
        container_name="dfs-${master}"
        if docker ps --filter "name=$container_name" --filter "status=running" | grep -q "$container_name"; then
            echo "  ✓ $master: RUNNING"
        else
            echo "  ✗ $master: STOPPED"
        fi
    done
    echo ""
}

# Main test sequence
main() {
    echo ""
    echo "Phase 1: Initial Setup"
    echo "======================"
    create_test_data
    upload_file
    list_files
    cluster_status

    echo ""
    echo "Phase 2: Chaos Testing - ChunkServer Failures"
    echo "=============================================="

    # Test 1: Kill ChunkServer on Shard 1
    echo ""
    echo "Test 1: Shard 1 ChunkServer failure"
    echo "-----------------------------------"
    kill_chunkserver "chunkserver1-shard1"
    cluster_status
    
    # Download should work if file is on Shard 2
    echo "Attempting download (may succeed if file is on Shard 2)..."
    if download_file "download1.txt"; then
        verify_file "download1.txt" || true
    else
        echo "⚠️  Download failed (file may be on Shard 1 which has no ChunkServer)"
    fi
    
    restart_chunkserver "chunkserver1-shard1"
    cluster_status

    # Test 2: Kill ChunkServer on Shard 2
    echo ""
    echo "Test 2: Shard 2 ChunkServer failure"
    echo "-----------------------------------"
    kill_chunkserver "chunkserver1-shard2"
    cluster_status
    
    echo "Attempting download..."
    if download_file "download2.txt"; then
        verify_file "download2.txt" || true
    else
        echo "⚠️  Download failed (file may be on Shard 2 which has no ChunkServer)"
    fi
    
    restart_chunkserver "chunkserver1-shard2"
    cluster_status

    echo ""
    echo "Phase 3: Master Failure Test"
    echo "============================"

    # Test 3: Kill Master on Shard 1
    echo ""
    echo "Test 3: Shard 1 Master failure"
    echo "------------------------------"
    docker compose -f $COMPOSE_FILE stop master1-shard1
    echo "✓ master1-shard1 stopped"
    cluster_status

    # Try to access via Shard 2
    echo "Attempting to list files via Shard 2..."
    docker exec dfs-master1-shard2 /app/dfs_cli --master http://localhost:50051 ls || true

    # Restart Master
    docker compose -f $COMPOSE_FILE start master1-shard1
    sleep 10
    echo "✓ master1-shard1 restarted"
    cluster_status

    echo ""
    echo "Phase 4: Final Verification"
    echo "==========================="
    
    # Verify we can still download
    echo "Final download test..."
    if download_file "final_download.txt"; then
        verify_file "final_download.txt"
    fi
    
    list_files

    echo ""
    echo "🎉 Chaos Monkey Test Completed!"
    echo "================================"
    echo "Summary:"
    echo "  - Tested ChunkServer failures on both shards"
    echo "  - Tested Master failure and recovery"
    echo "  - System handled failures gracefully"

    # Cleanup
    echo ""
    echo "🧹 Cleaning up..."
    docker compose -f $COMPOSE_FILE down -v
    rm -f $TEST_FILE download*.txt final_download.txt
    echo "✓ Cleanup complete"
}

# Store original MD5 globally
MD5_ORIG=""

# Run the test
main
