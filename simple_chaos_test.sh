#!/bin/bash

# Simple chaos test - kills one server at a time

set -e

echo "🧪 Simple Chaos Test"
echo "===================="

# Create test file
echo "Creating test file..."
echo "Hello from Rust Hadoop DFS! This is a test of the replication system." > simple_test.txt

# Upload
echo "Uploading file..."
docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd)/simple_test.txt:/tmp/simple_test.txt \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 put /tmp/simple_test.txt /simple.txt

echo "✓ File uploaded"

# List files
echo ""
echo "Files in DFS:"
docker run --rm --network rust-hadoop_dfs-network \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 ls

# Kill a server
echo ""
echo "💀 Stopping chunkserver1..."
docker-compose stop dfs-chunkserver1

# Try to download
echo ""
echo "📥 Downloading file with one server down..."
docker run --rm --network rust-hadoop_dfs-network \
  -v $(pwd):/output \
  rust-hadoop-master \
  /app/dfs_cli --master http://dfs-master:50051 get /simple.txt /output/downloaded_simple.txt

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

# Restart server
echo ""
echo "♻️  Restarting chunkserver1..."
docker-compose start dfs-chunkserver1

# Cleanup
rm -f simple_test.txt downloaded_simple.txt

echo ""
echo "🎉 Simple chaos test passed!"
