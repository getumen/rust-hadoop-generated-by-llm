#!/bin/bash

# Script to run all test scripts in test_scripts/ directory sequentially
# Stops on first failure due to set -e

set -e

# -------------------------------------------------------
# Wait for Docker to be available (Lima VM can temporarily
# lose SSH connectivity during heavy Docker operations).
# -------------------------------------------------------
wait_for_docker() {
    local max_wait=120
    local waited=0
    while ! docker ps >/dev/null 2>&1; do
        if [ $waited -ge $max_wait ]; then
            echo "❌ Docker not available after ${max_wait}s, aborting."
            exit 1
        fi
        echo "⏳ Waiting for Docker... (${waited}s)"
        sleep 5
        waited=$((waited + 5))
    done
}

# -------------------------------------------------------
# Global Docker cleanup: run before each test to ensure
# no stale containers or ports from a previous test cause
# the next one to fail.
# -------------------------------------------------------
docker_cleanup() {
    echo "🧹 [run_all_tests] Cleaning up Docker state before next test..."
    wait_for_docker
    # Bring down all known compose stacks
    docker compose                               down -v 2>/dev/null || true
    docker compose -f docker-compose.toxiproxy-sharded.yml down -v 2>/dev/null || true
    if [ -f docker-compose.auto-scaling.yml ]; then
        docker compose -f docker-compose.auto-scaling.yml down -v 2>/dev/null || true
    fi
    # Remove any remaining project containers by label
    docker ps -aq --filter "name=dfs-" | xargs docker rm -f 2>/dev/null || true
    # NOTE: docker network prune is intentionally omitted - it can temporarily
    # destabilize the Lima VM's network and break the Docker socket connection.
    # Prune unused images/build cache to prevent disk exhaustion in the Lima VM.
    # (Disk full caused VM crashes - 75%+ usage observed before adding this.)
    docker image prune -f 2>/dev/null || true
    echo "🧹 [run_all_tests] Docker cleanup done."
}

echo "🧪 Running all test scripts in test_scripts/ directory..."
echo "======================================================="

# -------------------------------------------------------
# Reserve ports used by TLS/manual tests so the Lima VM
# kernel never assigns them as ephemeral ports to k3s or
# other processes.  This prevents "address already in use"
# errors when local binaries bind to these ports.
# -------------------------------------------------------
docker run --rm --net=host --privileged alpine \
    sysctl -w net.ipv4.ip_local_reserved_ports=50050-50082,8085,9000 \
    2>/dev/null || true

# -------------------------------------------------------
# Pre-build all Docker images once before running any tests.
# Individual test scripts use 'docker compose up -d --no-build'
# to avoid SSL/cache issues during cargo compilation inside Docker.
# -------------------------------------------------------
echo "🔨 Pre-building Docker images..."
if docker compose build 2>&1; then
    echo "✅ Docker images built successfully"
else
    echo "⚠️  docker compose build failed — using existing images"
fi
if [ -f docker-compose.auto-scaling.yml ]; then
    docker compose -f docker-compose.auto-scaling.yml build 2>&1 || true
fi

# Tag main images for spark-s3-test compose (same Dockerfile, different project name)
echo "🏷️  Tagging images for spark-s3-test..."
docker tag rust-hadoop-generated-by-llm-master1-shard1:latest      spark-s3-test-master1-shard1:latest      2>/dev/null || true
docker tag rust-hadoop-generated-by-llm-master1-shard2:latest      spark-s3-test-master1-shard2:latest      2>/dev/null || true
docker tag rust-hadoop-generated-by-llm-chunkserver1-shard1:latest spark-s3-test-chunkserver1-shard1:latest 2>/dev/null || true
docker tag rust-hadoop-generated-by-llm-chunkserver1-shard2:latest spark-s3-test-chunkserver1-shard2:latest 2>/dev/null || true
docker tag rust-hadoop-generated-by-llm-s3-server:latest           spark-s3-test-s3-server:latest           2>/dev/null || true

# Initial cleanup before the first test
docker_cleanup

# Get the list of scripts in alphabetical order
scripts=$(ls test_scripts/*.sh | sort)

for script in $scripts; do
    echo ""
    echo "▶️  Running $script..."
    echo "----------------------------------------"
    bash "$script"
    echo "✅ $script completed successfully"
    # Cleanup Docker state between tests
    docker_cleanup
done

echo ""
echo "🎉 All test scripts completed successfully!"
