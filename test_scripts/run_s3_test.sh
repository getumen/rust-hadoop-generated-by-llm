#!/bin/bash
set -e

# Change to project root directory
cd "$(dirname "$0")/.."

echo "=== S3 Integration Test Runner ==="

# 1. Install Requirements
echo "Installing Python dependencies..."
pip install -r test_scripts/requirements.txt

# 2. Build and Start Cluster
echo "Cleaning up old data..."
docker compose -f test_scripts/spark-s3-test/docker-compose.yml down -v
# Force rebuild to ensure code changes (like sharding logic) are picked up
echo "Building Docker images (no cache)..."
docker compose -f test_scripts/spark-s3-test/docker-compose.yml build --no-cache

echo "Starting Cluster..."
docker compose -f test_scripts/spark-s3-test/docker-compose.yml up -d

# 3. Wait for S3 Server
echo "Waiting for S3 Server (port 9000)..."
RETRIES=30
while [ $RETRIES -gt 0 ]; do
    if nc -z localhost 9000; then
        echo "S3 Server is ready!"
        break
    fi
    echo "Waiting... ($RETRIES)"
    sleep 2
    RETRIES=$((RETRIES-1))
done

if [ $RETRIES -eq 0 ]; then
    echo "Error: S3 Server failed to start."
    docker compose logs s3-server
    docker compose -f test_scripts/spark-s3-test/docker-compose.yml down -v
    exit 1
fi

# 4. Run Test
echo "Running Integration Test..."
# Allow Masters to elect leader (takes ~2-5s)
sleep 10

set +e
python3 test_scripts/s3_integration_test.py > test_output.log 2>&1
EXIT_CODE=$?
set -e

# Check Failure and Log
if [ $EXIT_CODE -ne 0 ]; then
    echo "=== TEST FAILED (Exit Code: $EXIT_CODE) ==="
    echo "--- Python Test Output ---"
    cat test_output.log
    echo "--- S3 Server Logs ---"
    docker compose -f test_scripts/spark-s3-test/docker-compose.yml logs s3-server
    echo "--- Master Logs ---"
    docker compose -f test_scripts/spark-s3-test/docker-compose.yml logs master1-shard1
fi

# 5. Cleanup
echo "Stopping Cluster..."
docker compose -f test_scripts/spark-s3-test/docker-compose.yml down -v

# 6. Report/Exit
if [ $EXIT_CODE -ne 0 ]; then
    exit 1
else
    echo "=== TEST PASSED ==="
    exit 0
fi
