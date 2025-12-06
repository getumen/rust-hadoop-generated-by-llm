#!/bin/bash

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${GREEN}Starting Chaos Monkey Test for Sharded Cluster...${NC}"

# Clean up any previous state
echo "Cleaning up previous state..."
docker compose -f docker-compose.yml down -v || true

# Start cluster
echo "Starting cluster..."
docker compose -f docker-compose.yml up -d --build
echo "Waiting for cluster to stabilize (30s)..."
sleep 30

# Create test file
echo "Creating test file..."
dd if=/dev/urandom of=test_sharded.bin bs=1M count=10
MD5_ORIG=$(md5 -q test_sharded.bin)
echo "Original MD5: $MD5_ORIG"

# Master container for Shard 1
MASTER_CONTAINER="dfs-master1-shard1"

# Copy file to master container
echo "Copying file to $MASTER_CONTAINER container..."
docker cp test_sharded.bin $MASTER_CONTAINER:/tmp/test_sharded.bin

# Upload file via Master (will be routed to the correct shard)
echo "Uploading file..."
docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 put /tmp/test_sharded.bin /test_sharded.bin

if [ $? -ne 0 ]; then
    echo -e "${RED}Upload failed!${NC}"
    docker compose -f docker-compose.yml down -v
    rm -f test_sharded.bin
    exit 1
fi

echo -e "${GREEN}Upload successful!${NC}"

# Verify we can download the file
echo "Downloading file (normal state)..."
docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 get /test_sharded.bin /tmp/downloaded_normal.bin

if [ $? -ne 0 ]; then
    echo -e "${RED}Download failed!${NC}"
    docker compose -f docker-compose.yml down -v
    rm -f test_sharded.bin
    exit 1
fi

docker cp $MASTER_CONTAINER:/tmp/downloaded_normal.bin downloaded_normal.bin
MD5_NORMAL=$(md5 -q downloaded_normal.bin)
echo "Downloaded MD5 (normal): $MD5_NORMAL"

if [ "$MD5_ORIG" == "$MD5_NORMAL" ]; then
    echo -e "${GREEN}Data integrity verified (normal state)!${NC}"
else
    echo -e "${RED}Data corruption detected (normal state)!${NC}"
    docker compose -f docker-compose.yml down -v
    rm -f test_sharded.bin downloaded_normal.bin
    exit 1
fi

# Chaos Test: Kill one ChunkServer
# We need to determine which shard the file was uploaded to, then kill that ChunkServer
# For simplicity, let's kill the ChunkServer on Shard 1
TARGET_CS="chunkserver1-shard1"
echo -e "${RED}Killing $TARGET_CS...${NC}"
docker compose -f docker-compose.yml stop $TARGET_CS

echo "Waiting for failure detection (10s)..."
sleep 10

# Try to download with degraded cluster
# Note: If the file is on shard-1 and we killed chunkserver1-shard1, this might fail
# unless there are replicas. With default config (1 ChunkServer per shard), it will fail.
# Let's just check that the system handles it gracefully.
echo "Downloading file (degraded state)..."
docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 get /test_sharded.bin /tmp/downloaded_degraded.bin 2>&1

DOWNLOAD_RESULT=$?
if [ $DOWNLOAD_RESULT -ne 0 ]; then
    echo -e "${RED}Download failed in degraded state (expected if file was on stopped ChunkServer).${NC}"
    echo "This is expected behavior with single-replica configuration."
else
    docker cp $MASTER_CONTAINER:/tmp/downloaded_degraded.bin downloaded_degraded.bin
    MD5_DOWN=$(md5 -q downloaded_degraded.bin)
    echo "Downloaded MD5 (degraded): $MD5_DOWN"

    if [ "$MD5_ORIG" == "$MD5_DOWN" ]; then
        echo -e "${GREEN}Data integrity verified in degraded state!${NC}"
    else
        echo -e "${RED}Data corruption detected in degraded state!${NC}"
        docker compose -f docker-compose.yml down -v
        rm -f test_sharded.bin downloaded_normal.bin downloaded_degraded.bin
        exit 1
    fi
fi

# Recover ChunkServer
echo -e "${GREEN}Recovering $TARGET_CS...${NC}"
docker compose -f docker-compose.yml start $TARGET_CS
echo "Waiting for recovery (10s)..."
sleep 10

# Verify download works after recovery
echo "Downloading file (after recovery)..."
docker exec $MASTER_CONTAINER /app/dfs_cli --master http://localhost:50051 get /test_sharded.bin /tmp/downloaded_recovery.bin

if [ $? -ne 0 ]; then
    echo -e "${RED}Download failed after recovery!${NC}"
    docker compose -f docker-compose.yml down -v
    rm -f test_sharded.bin downloaded_normal.bin downloaded_degraded.bin
    exit 1
fi

docker cp $MASTER_CONTAINER:/tmp/downloaded_recovery.bin downloaded_recovery.bin
MD5_RECOVERY=$(md5 -q downloaded_recovery.bin)
echo "Downloaded MD5 (after recovery): $MD5_RECOVERY"

if [ "$MD5_ORIG" == "$MD5_RECOVERY" ]; then
    echo -e "${GREEN}Data integrity verified after recovery!${NC}"
else
    echo -e "${RED}Data corruption detected after recovery!${NC}"
    docker compose -f docker-compose.yml down -v
    rm -f test_sharded.bin downloaded_normal.bin downloaded_degraded.bin downloaded_recovery.bin
    exit 1
fi

echo -e "${GREEN}Chaos Test Passed!${NC}"

# Cleanup
echo "Cleaning up..."
docker compose -f docker-compose.yml down -v
rm -f test_sharded.bin downloaded_normal.bin downloaded_degraded.bin downloaded_recovery.bin
echo "Cleanup complete!"
