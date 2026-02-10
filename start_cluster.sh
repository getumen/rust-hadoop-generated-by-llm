#!/bin/bash

# Quick start script for Rust Hadoop DFS

set -e

echo "🚀 Starting Rust Hadoop DFS Cluster"
echo "===================================="

# Build the Docker image
echo "📦 Building Docker image..."
docker compose build

# Start all services
echo "🏃 Starting services..."
docker compose up -d

# Wait for services to be ready
echo "⏳ Waiting for services to start..."
sleep 10

# Check service status
echo ""
echo "📊 Service Status:"
docker compose ps

echo ""
echo "✅ Cluster is ready!"
echo ""
echo "Available commands:"
echo "  - Upload file:   docker run --rm --network rust-hadoop_dfs-network -v \$(pwd)/file.txt:/tmp/file.txt rust-hadoop-master1 /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051 put /tmp/file.txt /file.txt"
echo "  - List files:    docker run --rm --network rust-hadoop_dfs-network rust-hadoop-master1 /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051 ls"
echo "  - Download file: docker run --rm --network rust-hadoop_dfs-network -v \$(pwd):/output rust-hadoop-master1 /app/dfs_cli --master http://dfs-master1:50051,http://dfs-master2:50051 get /file.txt /output/downloaded.txt"
echo "  - Run chaos test: ./chaos_test.sh"
echo "  - Stop cluster:  docker compose down"
echo "  - View logs:     docker compose logs -f [service_name]"
