#!/bin/bash

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${GREEN}Starting Chaos Monkey Test for Sharded Cluster...${NC}"

# Ensure cluster is running
echo "Checking cluster status..."
if [ -z "$(docker compose -f docker-compose-sharded.yml ps -q)" ]; then
    echo "Cluster is not running. Starting..."
    docker compose -f docker-compose-sharded.yml up -d --build
    echo "Waiting for cluster to stabilize (30s)..."
    sleep 30
fi

# Create test file
echo "Creating test file..."
dd if=/dev/urandom of=test_sharded.bin bs=1M count=10
MD5_ORIG=$(md5sum test_sharded.bin | awk '{print $1}')
echo "Original MD5: $MD5_ORIG"

# Copy file to master-0-0 container
echo "Copying file to master-0-0 container..."
docker cp test_sharded.bin master-0-0:/tmp/test_sharded.bin

# Upload file to Shard 0 (via Master 0-0)
echo "Uploading file to Shard 0..."
docker compose -f docker-compose-sharded.yml exec -T master-0-0 \
  /app/dfs_cli --master http://master-0-0:50051 put /tmp/test_sharded.bin /test_sharded.bin

if [ $? -ne 0 ]; then
    echo -e "${RED}Upload to Shard 0 failed!${NC}"
    exit 1
fi

echo -e "${GREEN}Upload to Shard 0 successful!${NC}"

# Verify we can download the file
echo "Downloading file from Shard 0 (normal state)..."
docker compose -f docker-compose-sharded.yml exec -T master-0-0 \
  /app/dfs_cli --master http://master-0-0:50051 get /test_sharded.bin /tmp/downloaded_normal.bin

if [ $? -ne 0 ]; then
    echo -e "${RED}Download failed!${NC}"
    exit 1
fi

docker cp master-0-0:/tmp/downloaded_normal.bin downloaded_normal.bin
MD5_NORMAL=$(md5sum downloaded_normal.bin | awk '{print $1}')
echo "Downloaded MD5 (normal): $MD5_NORMAL"

if [ "$MD5_ORIG" == "$MD5_NORMAL" ]; then
    echo -e "${GREEN}Data integrity verified (normal state)!${NC}"
else
    echo -e "${RED}Data corruption detected (normal state)!${NC}"
    exit 1
fi

# Chaos Test: Kill one ChunkServer
TARGET_CS="chunkserver1"
echo -e "${RED}Killing $TARGET_CS...${NC}"
docker compose -f docker-compose-sharded.yml stop $TARGET_CS

echo "Waiting for failure detection (10s)..."
sleep 10

# Try to download from Shard 0 with degraded cluster
echo "Downloading file from Shard 0 (degraded state)..."
docker compose -f docker-compose-sharded.yml exec -T master-0-0 \
  /app/dfs_cli --master http://master-0-0:50051 get /test_sharded.bin /tmp/downloaded_degraded.bin

if [ $? -ne 0 ]; then
    echo -e "${RED}Download failed in degraded state!${NC}"
    exit 1
fi

docker cp master-0-0:/tmp/downloaded_degraded.bin downloaded_degraded.bin
MD5_DOWN=$(md5sum downloaded_degraded.bin | awk '{print $1}')
echo "Downloaded MD5 (degraded): $MD5_DOWN"

if [ "$MD5_ORIG" == "$MD5_DOWN" ]; then
    echo -e "${GREEN}Data integrity verified in degraded state!${NC}"
else
    echo -e "${RED}Data corruption detected in degraded state!${NC}"
    exit 1
fi

# Recover
echo -e "${GREEN}Recovering $TARGET_CS...${NC}"
docker compose -f docker-compose-sharded.yml start $TARGET_CS
echo "Waiting for recovery (10s)..."
sleep 10

echo -e "${GREEN}Chaos Test Passed!${NC}"

# Cleanup test files
echo "Cleaning up test files..."
rm -f test_sharded.bin downloaded_normal.bin downloaded_degraded.bin
echo "Cleanup complete!"
