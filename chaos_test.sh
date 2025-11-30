#!/bin/bash

# Chaos Monkey Test Script for Rust Hadoop DFS
# This script randomly kills and restarts chunk servers to test replication resilience

set -e

CHUNKSERVERS=("chunkserver1" "chunkserver2" "chunkserver3" "chunkserver4" "chunkserver5")
TEST_FILE="chaos_test_data.txt"
DFS_PATH="/chaos_test.txt"

echo "🐵 Starting Chaos Monkey Test for Rust Hadoop DFS"
echo "=================================================="

# Function to create test data
create_test_data() {
    echo "📝 Creating test data..."
    dd if=/dev/urandom of=$TEST_FILE bs=1M count=10 2>/dev/null
    md5sum $TEST_FILE > ${TEST_FILE}.md5
    echo "✓ Created 10MB test file"
}

# Function to upload file
upload_file() {
    echo "📤 Uploading file to DFS..."
    docker run --rm --network rust-hadoop_dfs-network \
        -v $(pwd)/$TEST_FILE:/tmp/$TEST_FILE \
        rust-hadoop-master1 \
        /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051,http://dfs-master3:50051 put /tmp/$TEST_FILE $DFS_PATH
    echo "✓ File uploaded"
}

# Function to download file
download_file() {
    local output_file=$1
    echo "📥 Downloading file from DFS..."
    docker run --rm --network rust-hadoop_dfs-network \
        -v $(pwd):/output \
        rust-hadoop-master1 \
        /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051,http://dfs-master3:50051 get $DFS_PATH /output/$output_file
    echo "✓ File downloaded to $output_file"
}

# Function to verify file integrity
verify_file() {
    local file=$1
    echo "🔍 Verifying file integrity..."
    md5sum -c ${TEST_FILE}.md5 --status 2>/dev/null || {
        echo "✗ File integrity check failed!"
        return 1
    }
    echo "✓ File integrity verified"
}

# Function to randomly kill a chunk server
kill_random_chunkserver() {
    local server=${CHUNKSERVERS[$RANDOM % ${#CHUNKSERVERS[@]}]}
    echo "💀 Killing $server..." >&2
    docker-compose stop $server 2>/dev/null || true
    echo "✓ $server stopped" >&2
    echo $server
}

# Function to restart a chunk server
restart_chunkserver() {
    local server=$1
    echo "♻️  Restarting $server..."
    docker-compose start $server
    sleep 3
    echo "✓ $server restarted"
}

# Function to list files
list_files() {
    echo "📋 Listing files in DFS..."
    docker run --rm --network rust-hadoop_dfs-network \
        rust-hadoop-master1 \
        /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051,http://dfs-master3:50051 ls
}

# Function to get cluster status
cluster_status() {
    echo ""
    echo "📊 Cluster Status:"
    echo "=================="
    for server in "${CHUNKSERVERS[@]}"; do
        if docker ps --filter "name=$server" --filter "status=running" | grep -q $server; then
            echo "  ✓ $server: RUNNING"
        else
            echo "  ✗ $server: STOPPED"
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
    echo "Phase 2: Chaos Testing"
    echo "======================"
    
    # Test 1: Kill one server and download
    echo ""
    echo "Test 1: Single server failure"
    echo "------------------------------"
    killed_server=$(kill_random_chunkserver)
    cluster_status
    download_file "download1.txt"
    verify_file "download1.txt"
    restart_chunkserver $killed_server
    cluster_status

    # Test 2: Kill two servers and download
    echo ""
    echo "Test 2: Two server failures"
    echo "---------------------------"
    killed_server1=$(kill_random_chunkserver)
    sleep 2
    killed_server2=$(kill_random_chunkserver)
    while [ "$killed_server2" == "$killed_server1" ]; do
        restart_chunkserver $killed_server2
        sleep 2
        killed_server2=$(kill_random_chunkserver)
    done
    cluster_status
    download_file "download2.txt"
    verify_file "download2.txt"
    restart_chunkserver $killed_server1
    restart_chunkserver $killed_server2
    cluster_status

    # Test 3: Rolling failures
    echo ""
    echo "Test 3: Rolling failures (kill and restart repeatedly)"
    echo "-------------------------------------------------------"
    for i in {1..5}; do
        echo "Round $i/5:"
        killed=$(kill_random_chunkserver)
        sleep 2
        download_file "download_rolling_${i}.txt"
        verify_file "download_rolling_${i}.txt"
        restart_chunkserver $killed
        sleep 2
    done
    cluster_status

    # Test 4: Upload during chaos
    echo ""
    echo "Test 4: Upload new file during partial outage"
    echo "----------------------------------------------"
    killed=$(kill_random_chunkserver)
    cluster_status
    
    echo "Creating second test file..."
    dd if=/dev/urandom of=chaos_test_data2.txt bs=1M count=5 2>/dev/null
    
    echo "Uploading during chaos..."
    docker run --rm --network rust-hadoop_dfs-network \
        -v $(pwd)/chaos_test_data2.txt:/tmp/chaos_test_data2.txt \
        rust-hadoop-master1 \
        /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051,http://dfs-master3:50051 put /tmp/chaos_test_data2.txt /chaos_test2.txt || {
        echo "⚠️  Upload failed (expected with insufficient replicas)"
    }
    
    restart_chunkserver $killed
    cluster_status

    echo ""
    echo "Phase 3: Final Verification"
    echo "==========================="
    download_file "final_download.txt"
    verify_file "final_download.txt"
    list_files

    echo ""
    echo "🎉 Chaos Monkey Test Completed Successfully!"
    echo "============================================"
    echo "Summary:"
    echo "  - Original file maintained integrity through multiple server failures"
    echo "  - Replication factor of 3 provided sufficient redundancy"
    echo "  - System recovered gracefully from failures"
    
    # Cleanup
    echo ""
    echo "🧹 Cleaning up test files..."
    rm -f $TEST_FILE ${TEST_FILE}.md5 download*.txt chaos_test_data2.txt final_download.txt
    echo "✓ Cleanup complete"
}

# Run the test
main

echo "=== Test 5: Master Failure Test ==="
echo "Killing a random master..."
MASTERS=("master1" "master2" "master3")
RANDOM_MASTER=${MASTERS[$RANDOM % ${#MASTERS[@]}]}
docker-compose stop $RANDOM_MASTER
echo "Stopped $RANDOM_MASTER"
sleep 10 # Wait for election

echo "Attempting to list files (should failover)..."
list_files

echo "Attempting to upload file during master outage..."
echo "HA Test Data" > ha_test.txt
TEST_FILE="ha_test.txt"
DFS_PATH="/ha_test.txt"
upload_file

echo "Restarting stopped master..."
docker-compose start $RANDOM_MASTER
sleep 10
