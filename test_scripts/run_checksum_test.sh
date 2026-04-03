#!/bin/bash
set -e

# Configuration
TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$TEST_DIR/.." && pwd)"
S3_ENDPOINT=${S3_ENDPOINT:-"http://localhost:9000"}

echo "--- Rust Hadoop DFS Checksum Functional Test ---"

# Check dependencies
if ! command -v python3 &> /dev/null; then
    echo "Error: python3 is not installed."
    exit 1
fi

if ! python3 -c "import boto3" &> /dev/null; then
    echo "Error: boto3 is not installed. Please run: pip install boto3"
    exit 1
fi

cleanup() {
    echo "🧹 Cleaning up Docker cluster..."
    cd "$PROJECT_ROOT"
    docker compose down -v 2>/dev/null || true
}
trap cleanup EXIT

cd "$PROJECT_ROOT"

# Start the cluster (includes S3 server on port 9000)
echo "🚀 Starting cluster..."
docker compose down -v 2>/dev/null || true
docker compose up -d --build

# Wait for S3 server to be ready
echo "Waiting for S3 server to be ready at $S3_ENDPOINT..."
for i in $(seq 1 30); do
    if curl -s -o /dev/null -w '%{http_code}' "$S3_ENDPOINT/health" 2>/dev/null | grep -q "200"; then
        echo "  S3 server is ready (attempt $i)"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "Error: S3 server did not start within 30 seconds"
        exit 1
    fi
    sleep 1
done

# Run the test
echo "Running checksum_verification_test.py against $S3_ENDPOINT..."
export S3_ENDPOINT
python3 "$TEST_DIR/checksum_verification_test.py"

echo "Checksum verification successful."
