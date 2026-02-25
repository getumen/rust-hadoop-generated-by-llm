#!/bin/bash
set -e

# ============================================================================
# TLS End-to-End Test
# Tests: ConfigServer + Master + ChunkServer + CLI with TLS encryption
# ============================================================================

DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
CERT_DIR="$DIR/../certs"
CA_CERT="$CERT_DIR/ca.crt"
SERVER_CERT="$CERT_DIR/server.crt"
SERVER_KEY="$CERT_DIR/server.key"

PASS=0
FAIL=0

pass() { echo "  ✅ $1"; PASS=$((PASS+1)); }
fail() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }

cleanup() {
    echo ""
    echo "Cleaning up..."
    kill $CONFIG_SERVER_PID $MASTER_PID $CHUNKSERVER_PID 2>/dev/null || true
    wait $CONFIG_SERVER_PID $MASTER_PID $CHUNKSERVER_PID 2>/dev/null || true
    rm -f /tmp/tls_test_*.log
}
trap cleanup EXIT

# Verify certificates exist
if [ ! -f "$CA_CERT" ]; then
    echo "ERROR: Certificates not found! Run generate_certs.sh first."
    exit 1
fi

# Clean up data dirs
rm -rf /tmp/tls_test_config /tmp/tls_test_master /tmp/tls_test_chunk
mkdir -p /tmp/tls_test_config /tmp/tls_test_master /tmp/tls_test_chunk

echo "============================================"
echo " TLS End-to-End Test"
echo "============================================"
echo ""

# Build all binaries first
echo "Building binaries..."
cargo build --bin config_server --bin master --bin chunkserver --bin dfs_cli 2>&1 | tail -3
echo ""

# --- Start Config Server with TLS ---
echo "1. Starting Config Server (TLS)..."
RUST_LOG=info ./target/debug/config_server \
    --id 1 \
    --addr 127.0.0.1:60050 \
    --http-port 9080 \
    --storage-dir /tmp/tls_test_config \
    --tls-cert "$SERVER_CERT" \
    --tls-key "$SERVER_KEY" \
    --ca-cert "$CA_CERT" \
    > /tmp/tls_test_config.log 2>&1 &
CONFIG_SERVER_PID=$!
sleep 3

if kill -0 $CONFIG_SERVER_PID 2>/dev/null; then
    pass "Config Server started (PID: $CONFIG_SERVER_PID)"
else
    fail "Config Server failed to start"
    cat /tmp/tls_test_config.log
    exit 1
fi

# --- Start Master with TLS ---
echo "2. Starting Master (TLS)..."
RUST_LOG=info ./target/debug/master \
    --id 1 \
    --addr 127.0.0.1:60051 \
    --http-port 9081 \
    --storage-dir /tmp/tls_test_master \
    --config-servers https://127.0.0.1:60050 \
    --shard-id shard-0 \
    --tls-cert "$SERVER_CERT" \
    --tls-key "$SERVER_KEY" \
    --ca-cert "$CA_CERT" \
    --domain-name localhost \
    > /tmp/tls_test_master.log 2>&1 &
MASTER_PID=$!
sleep 5

if kill -0 $MASTER_PID 2>/dev/null; then
    pass "Master started (PID: $MASTER_PID)"
else
    fail "Master failed to start"
    cat /tmp/tls_test_master.log
    exit 1
fi

# --- Start Chunkserver with TLS ---
echo "3. Starting ChunkServer (TLS)..."
RUST_LOG=info ./target/debug/chunkserver \
    --addr 127.0.0.1:60052 \
    --http-port 9082 \
    --storage-dir /tmp/tls_test_chunk \
    --config-servers https://127.0.0.1:60050 \
    --tls-cert "$SERVER_CERT" \
    --tls-key "$SERVER_KEY" \
    --ca-cert "$CA_CERT" \
    --domain-name localhost \
    --advertise-addr 127.0.0.1:60052 \
    > /tmp/tls_test_chunk.log 2>&1 &
CHUNKSERVER_PID=$!
sleep 10

if kill -0 $CHUNKSERVER_PID 2>/dev/null; then
    pass "ChunkServer started (PID: $CHUNKSERVER_PID)"
else
    fail "ChunkServer failed to start"
    cat /tmp/tls_test_chunk.log
    exit 1
fi

echo ""
echo "--- CLI Tests over TLS ---"
echo ""

# CLI helper
CLI="./target/debug/dfs_cli --master https://127.0.0.1:60051 --config-servers https://127.0.0.1:60050 --ca-cert $CA_CERT --domain-name localhost"

# --- Test: cluster info ---
echo "4. Testing: cluster info"
if OUTPUT=$($CLI cluster info 2>&1); then
    if echo "$OUTPUT" | grep -q "Leader"; then
        pass "cluster info returned Leader status"
    else
        fail "cluster info didn't show Leader"
        echo "  Output: $OUTPUT"
    fi
else
    fail "cluster info command failed"
    echo "  Output: $OUTPUT"
fi

# --- Test: safe-mode get ---
echo "5. Testing: safe-mode get"
if OUTPUT=$($CLI safe-mode get 2>&1); then
    if echo "$OUTPUT" | grep -q "Active:"; then
        pass "safe-mode get returned status"
    else
        fail "safe-mode get didn't return expected output"
        echo "  Output: $OUTPUT"
    fi
else
    fail "safe-mode get command failed"
    echo "  Output: $OUTPUT"
fi

# --- Test: list files (empty) ---
echo "6. Testing: list files"
if OUTPUT=$($CLI ls 2>&1); then
    pass "ls succeeded over TLS"
else
    fail "ls failed"
    echo "  Output: $OUTPUT"
fi

# --- Test: create file ---
echo "7. Testing: create + upload file"
echo "Hello TLS World!" > /tmp/tls_test_file.txt
if OUTPUT=$($CLI put /tmp/tls_test_file.txt /tls_test/hello.txt 2>&1); then
    pass "File upload succeeded over TLS"
else
    fail "File upload failed"
    echo "  Output: $OUTPUT"
fi

# --- Test: download file ---
echo "8. Testing: get file"
if OUTPUT=$($CLI get /tls_test/hello.txt /tmp/tls_test_download.txt 2>&1); then
    if [ -f /tmp/tls_test_download.txt ] && grep -q "Hello TLS World" /tmp/tls_test_download.txt; then
        pass "File downloaded and content verified over TLS"
    else
        fail "File download content mismatch"
        echo "  Output: $OUTPUT"
    fi
else
    fail "File download failed"
    echo "  Output: $OUTPUT"
fi

# --- Test: verify non-TLS connection is rejected ---
echo "9. Testing: non-TLS connection is rejected"
if OUTPUT=$(./target/debug/dfs_cli --master http://127.0.0.1:60051 cluster info 2>&1); then
    fail "Non-TLS connection should have been rejected but succeeded"
else
    pass "Non-TLS connection correctly rejected"
fi

echo ""
echo "============================================"
echo " Results: $PASS passed, $FAIL failed"
echo "============================================"

if [ $FAIL -gt 0 ]; then
    echo ""
    echo "--- Config Server Log (last 10 lines) ---"
    tail -10 /tmp/tls_test_config.log 2>/dev/null || true
    echo "--- Master Log (last 10 lines) ---"
    tail -10 /tmp/tls_test_master.log 2>/dev/null || true
    echo "--- ChunkServer Log (last 10 lines) ---"
    tail -10 /tmp/tls_test_chunk.log 2>/dev/null || true
    exit 1
fi
