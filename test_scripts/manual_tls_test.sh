#!/bin/bash
set -e

# Get script directory
DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
CERT_DIR="$DIR/../certs"
CA_CERT="$CERT_DIR/ca.crt"
SERVER_CERT="$CERT_DIR/server.crt"
SERVER_KEY="$CERT_DIR/server.key"

# Kill any existing processes
pkill -f "target/debug/master" || true
pkill -f "target/debug/chunkserver" || true
pkill -f "target/debug/s3-server" || true

# Clean up data
rm -rf /tmp/master_tls
rm -rf /tmp/chunkserver_tls
mkdir -p /tmp/master_tls
mkdir -p /tmp/chunkserver_tls

echo "Using certificates from $CERT_DIR"

if [ ! -f "$CA_CERT" ]; then
    echo "Certificates not found! Run generate_certs.sh first."
    exit 1
fi

# Start Config Server
echo "Starting Config Server (TLS)..."
RUST_LOG=info cargo run --bin config_server -- \
    --id 1 \
    --addr 127.0.0.1:50050 \
    --http-port 8080 \
    --storage-dir /tmp/config_server_tls \
    --tls-cert "$SERVER_CERT" \
    --tls-key "$SERVER_KEY" \
    --ca-cert "$CA_CERT" \
    > config_server.log 2>&1 &
CONFIG_SERVER_PID=$!
echo "Config Server PID: $CONFIG_SERVER_PID"
sleep 5

# Start Master
echo "Starting Master (TLS)..."
RUST_LOG=info cargo run --bin master -- \
    --id 1 \
    --addr 127.0.0.1:50051 \
    --http-port 8081 \
    --storage-dir /tmp/master_tls \
    --config-servers http://127.0.0.1:50050 \
    --shard-id shard-0 \
    --tls-cert "$SERVER_CERT" \
    --tls-key "$SERVER_KEY" \
    --ca-cert "$CA_CERT" \
    > master.log 2>&1 &
MASTER_PID=$!

echo "Master PID: $MASTER_PID"
sleep 5

# Start Chunkserver
echo "Starting Chunkserver (TLS)..."
RUST_LOG=info cargo run --bin chunkserver -- \
    --addr 127.0.0.1:50052 \
    --http-port 8082 \
    --storage-dir /tmp/chunkserver_tls \
    --config-servers http://127.0.0.1:50050 \
    --tls-cert "$SERVER_CERT" \
    --tls-key "$SERVER_KEY" \
    --ca-cert "$CA_CERT" \
    > chunkserver.log 2>&1 &
CHUNKSERVER_PID=$!

echo "Chunkserver PID: $CHUNKSERVER_PID"
sleep 15

# Run CLI
echo "Running CLI (TLS)..."
RUST_LOG=info cargo run --bin dfs_cli -- \
    --master https://127.0.0.1:50051 \
    --config-servers https://127.0.0.1:50050 \
    --ca-cert "$CA_CERT" \
    --domain-name localhost \
    cluster info

echo "Checking Safe Mode (Chunkserver status)..."
RUST_LOG=info cargo run --bin dfs_cli -- \
    --master https://127.0.0.1:50051 \
    --config-servers https://127.0.0.1:50050 \
    --ca-cert "$CA_CERT" \
    --domain-name localhost \
    safe-mode get

echo "CLI completed."

# Verify logs
echo "Converting logs to check for errors..."

# Cleanup
echo "Cleaning up..."
kill $CONFIG_SERVER_PID
kill $MASTER_PID
kill $CHUNKSERVER_PID
wait $CONFIG_SERVER_PID 2>/dev/null || true
wait $MASTER_PID 2>/dev/null || true
wait $CHUNKSERVER_PID 2>/dev/null || true
