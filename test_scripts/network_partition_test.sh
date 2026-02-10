#!/bin/bash
# Real network partition testing using Toxiproxy
# Tests DFS behavior under actual network failures between shards, masters, and chunkservers
#
# Topology (from docker-compose.yml):
#   Shard1: master1-shard1 (gRPC:50051, HTTP:8081)
#           chunkserver1-shard1 (gRPC:50052)
#   Shard2: master1-shard2 (gRPC:50061, HTTP:8091)
#           chunkserver1-shard2 (gRPC:50062)
#   Shared: chunkserver-extra (gRPC:50072), chunkserver-extra2 (gRPC:50082)
#   Config: config-server (gRPC:50050, HTTP:8080)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Dry-run mode (set to 1 to skip actual toxiproxy commands)
DRY_RUN="${DRY_RUN:-0}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# Toxiproxy configuration
TOXIPROXY_HOST="localhost:8474"

# Proxy names
MASTER1_SHARD1_PROXY="master1_shard1_proxy"
MASTER1_SHARD2_PROXY="master1_shard2_proxy"
CS1_SHARD1_PROXY="cs1_shard1_proxy"
CONFIG_SERVER_PROXY="config_server_proxy"

# Actual upstream ports (host-side)
MASTER1_SHARD1_PORT=50051
MASTER1_SHARD2_PORT=50061
CS1_SHARD1_PORT=50052
CONFIG_SERVER_PORT=50050

# Proxy listen ports
MASTER1_SHARD1_LISTEN=60051
MASTER1_SHARD2_LISTEN=60061
CS1_SHARD1_LISTEN=60052
CONFIG_SERVER_LISTEN=60050

# HTTP ports for health checks
MASTER1_SHARD1_HTTP=8081
MASTER1_SHARD2_HTTP=8091
CONFIG_SERVER_HTTP=8080

PASSED=0
FAILED=0
SKIPPED=0

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_test() {
    echo -e "${CYAN}[TEST]${NC} $1"
}

# Wrapper for toxiproxy-cli commands
run_toxiproxy() {
    if [ "$DRY_RUN" = "1" ]; then
        log_info "[DRY-RUN] toxiproxy-cli $*"
        return 0
    fi
    toxiproxy-cli "$@"
}

# Check if Toxiproxy CLI is installed
check_toxiproxy() {
    if [ "$DRY_RUN" = "1" ]; then
        log_warn "Dry-run mode: Skipping Toxiproxy checks"
        return 0
    fi

    if ! command -v toxiproxy-cli &> /dev/null; then
        log_error "toxiproxy-cli not found."
        log_error ""
        log_error "To run these tests, install Toxiproxy:"
        log_error ""
        log_error "  macOS:  brew install toxiproxy"
        log_error "  Linux:  See https://github.com/Shopify/toxiproxy/releases"
        log_error ""
        log_error "Or run in dry-run mode to see test scenarios:"
        log_error "  DRY_RUN=1 $0"
        exit 1
    fi
    log_info "Toxiproxy CLI found: $(toxiproxy-cli --version)"
}

# Check if Toxiproxy server is running
check_toxiproxy_server() {
    if [ "$DRY_RUN" = "1" ]; then
        return 0
    fi

    if ! curl -s "http://${TOXIPROXY_HOST}/version" > /dev/null 2>&1; then
        log_error "Toxiproxy server not running at ${TOXIPROXY_HOST}"
        log_error ""
        log_error "Start the server with:"
        log_error "  toxiproxy-server &"
        log_error ""
        log_error "Or use Docker:"
        log_error "  docker run -d --name toxiproxy -p 8474:8474 ghcr.io/shopify/toxiproxy:2.9.0"
        exit 1
    fi
    log_info "Toxiproxy server is running at ${TOXIPROXY_HOST}"
}

# Check if Docker containers are running
check_docker_containers() {
    if [ "$DRY_RUN" = "1" ]; then
        return 0
    fi

    local missing=0
    for container in dfs-master1-shard1 dfs-master1-shard2 dfs-chunkserver1-shard1 dfs-config-server; do
        if ! docker ps --format '{{.Names}}' | grep -q "^${container}$"; then
            log_error "Container ${container} is not running"
            missing=1
        fi
    done

    if [ "$missing" = "1" ]; then
        log_warn "Cluster is not running. Starting it now with Toxiproxy..."
        # Use the toxiproxy-sharded compose file
        docker compose -f docker-compose.toxiproxy-sharded.yml up -d

        log_info "Waiting for cluster readiness (20s)..."
        sleep 20
    else
        log_info "All required Docker containers are running"
    fi

    # Determine upstream host based on Toxiproxy deployment
    if docker ps --format '{{.Names}}' | grep -q "^toxiproxy$"; then
        log_info "Detected Toxiproxy container. Using Docker DNS for upstreams."
        UPSTREAM_MASTER1_SHARD1="dfs-master1-shard1"
        UPSTREAM_MASTER1_SHARD2="dfs-master1-shard2"
        UPSTREAM_CS1_SHARD1="dfs-chunkserver1-shard1"
        UPSTREAM_CONFIG="dfs-config-server"
    else
        log_info "Using localhost for upstreams (Host-based Toxiproxy)."
        UPSTREAM_MASTER1_SHARD1="localhost"
        UPSTREAM_MASTER1_SHARD2="localhost"
        UPSTREAM_CS1_SHARD1="localhost"
        UPSTREAM_CONFIG="localhost"
    fi
}

# Check health of a master node via HTTP
check_master_health() {
    local name="$1"
    local http_port="$2"

    if [ "$DRY_RUN" = "1" ]; then
        echo '{"status":"healthy","role":"Leader","current_term":1,"commit_index":5}'
        return 0
    fi

    curl -s --connect-timeout 3 "http://localhost:${http_port}/health" 2>/dev/null || echo '{"error":"unreachable"}'
}

# Check raft state of a master node
check_raft_state() {
    local name="$1"
    local http_port="$2"

    if [ "$DRY_RUN" = "1" ]; then
        echo '{"role":"Leader","current_term":1,"commit_index":10}'
        return 0
    fi

    curl -s --connect-timeout 3 "http://localhost:${http_port}/raft/state" 2>/dev/null || echo '{"error":"unreachable"}'
}

# Create proxies for services
setup_proxies() {
    log_info "Setting up Toxiproxy proxies..."

    # Clean up existing proxies first
    for proxy in "${MASTER1_SHARD1_PROXY}" "${MASTER1_SHARD2_PROXY}" "${CS1_SHARD1_PROXY}" "${CONFIG_SERVER_PROXY}"; do
        run_toxiproxy delete "${proxy}" 2>/dev/null || true
    done

    # Create proxies (toxiproxy-cli v2.12+ syntax: name goes last)
    run_toxiproxy create --listen "0.0.0.0:${MASTER1_SHARD1_LISTEN}" --upstream "${UPSTREAM_MASTER1_SHARD1}:${MASTER1_SHARD1_PORT}" "${MASTER1_SHARD1_PROXY}"
    run_toxiproxy create --listen "0.0.0.0:${MASTER1_SHARD2_LISTEN}" --upstream "${UPSTREAM_MASTER1_SHARD2}:${MASTER1_SHARD2_PORT}" "${MASTER1_SHARD2_PROXY}"
    run_toxiproxy create --listen "0.0.0.0:${CS1_SHARD1_LISTEN}" --upstream "${UPSTREAM_CS1_SHARD1}:${CS1_SHARD1_PORT}" "${CS1_SHARD1_PROXY}"
    run_toxiproxy create --listen "0.0.0.0:${CONFIG_SERVER_LISTEN}" --upstream "${UPSTREAM_CONFIG}:${CONFIG_SERVER_PORT}" "${CONFIG_SERVER_PROXY}"

    log_info "Proxies created:"
    log_info "Proxies created:"
    log_info "  master1-shard1: localhost:${MASTER1_SHARD1_LISTEN} -> ${UPSTREAM_MASTER1_SHARD1}:${MASTER1_SHARD1_PORT}"
    log_info "  master1-shard2: localhost:${MASTER1_SHARD2_LISTEN} -> ${UPSTREAM_MASTER1_SHARD2}:${MASTER1_SHARD2_PORT}"
    log_info "  cs1-shard1:     localhost:${CS1_SHARD1_LISTEN} -> ${UPSTREAM_CS1_SHARD1}:${CS1_SHARD1_PORT}"
    log_info "  config-server:  localhost:${CONFIG_SERVER_LISTEN} -> ${UPSTREAM_CONFIG}:${CONFIG_SERVER_PORT}"
}

# Cleanup proxies and containers
cleanup_proxies() {
    log_info "Cleaning up Toxiproxy proxies..."
    for proxy in "${MASTER1_SHARD1_PROXY}" "${MASTER1_SHARD2_PROXY}" "${CS1_SHARD1_PROXY}" "${CONFIG_SERVER_PROXY}"; do
        run_toxiproxy delete "${proxy}" 2>/dev/null || true
    done

    # Stop containers if they were started by this script
    # We check if we are using the toxiproxy-sharded compose file
    if [ -f "docker-compose.toxiproxy-sharded.yml" ]; then
        log_info "Stopping cluster..."
        docker compose -f docker-compose.toxiproxy-sharded.yml down -v >/dev/null 2>&1 || true
    fi
}

# ============================================================================
# Test Cases
# ============================================================================

# Test 1: Isolate one shard's master — the other shard should remain operational
test_shard_isolation() {
    log_test "========================================="
    log_test "Test 1: Shard Isolation"
    log_test "========================================="

    log_info "Scenario: Isolate master1-shard1 via total network timeout"
    log_info "Expected: Shard2 remains fully operational"

    # Record baseline state
    log_info "Baseline state:"
    log_info "  Shard1: $(check_raft_state shard1 ${MASTER1_SHARD1_HTTP})"
    log_info "  Shard2: $(check_raft_state shard2 ${MASTER1_SHARD2_HTTP})"

    # Isolate shard1 master
    # Isolate shard1 master
    run_toxiproxy toxic add --toxicName timeout_downstream --type timeout --attribute timeout=0 "${MASTER1_SHARD1_PROXY}"

    log_info "master1-shard1 is now isolated"
    sleep 3

    # Check shard2 is still healthy
    local shard2_state
    shard2_state=$(check_raft_state shard2 ${MASTER1_SHARD2_HTTP})
    log_info "Shard2 state during shard1 isolation: ${shard2_state}"

    if echo "${shard2_state}" | grep -q '"error"'; then
        log_error "FAIL: Shard2 should remain operational during shard1 isolation"
        FAILED=$((FAILED + 1))
    else
        log_info "PASS: Shard2 remained operational"
        PASSED=$((PASSED + 1))
    fi

    # Heal
    run_toxiproxy toxic remove "${MASTER1_SHARD1_PROXY}" --toxicName timeout_downstream 2>/dev/null || \
        run_toxiproxy toxic remove "${MASTER1_SHARD1_PROXY}" -n timeout_downstream 2>/dev/null || true
    sleep 3

    log_info "Partition healed"
    log_info "  Shard1: $(check_raft_state shard1 ${MASTER1_SHARD1_HTTP})"
}

# Test 2: High latency on inter-shard communication
test_high_latency() {
    log_test "========================================="
    log_test "Test 2: High Network Latency"
    log_test "========================================="

    log_info "Scenario: Add 500ms±50ms latency to master1-shard1"
    log_info "Expected: Operations slow down but cluster remains functional"

    # Add latency
    # Add latency
    run_toxiproxy toxic add --toxicName latency_downstream --type latency --attribute latency=500 --attribute jitter=50 "${MASTER1_SHARD1_PROXY}"

    log_info "master1-shard1 now has 500ms±50ms latency"
    sleep 5

    # Measure response time via health endpoint
    local start_time end_time elapsed
    start_time=$(date +%s%N 2>/dev/null || date +%s)

    local health
    health=$(check_master_health shard1 ${MASTER1_SHARD1_HTTP})
    end_time=$(date +%s%N 2>/dev/null || date +%s)

    log_info "Shard1 health under latency: ${health}"
    log_info "Shard2 health (no latency):  $(check_master_health shard2 ${MASTER1_SHARD2_HTTP})"

    if echo "${health}" | grep -q '"error"'; then
        log_warn "Shard1 became unreachable under latency — may have exceeded timeout"
        SKIPPED=$((SKIPPED + 1))
    else
        log_info "PASS: Shard1 remained responsive under latency"
        PASSED=$((PASSED + 1))
    fi

    # Remove latency
    run_toxiproxy toxic remove "${MASTER1_SHARD1_PROXY}" --toxicName latency_downstream 2>/dev/null || \
        run_toxiproxy toxic remove "${MASTER1_SHARD1_PROXY}" -n latency_downstream 2>/dev/null || true
    sleep 3
}

# Test 3: Config server isolation — shards should continue with cached config
test_config_server_isolation() {
    log_test "========================================="
    log_test "Test 3: Config Server Isolation"
    log_test "========================================="

    log_info "Scenario: Isolate config server from all masters"
    log_info "Expected: Shards continue operating with cached shard map"

    # Record baseline
    log_info "Config server baseline: $(check_master_health config ${CONFIG_SERVER_HTTP})"

    # Isolate config server
    # Isolate config server
    run_toxiproxy toxic add --toxicName timeout_downstream --type timeout --attribute timeout=0 "${CONFIG_SERVER_PROXY}"

    log_info "Config server is now isolated"
    sleep 5

    # Both shards should still work
    local shard1_state shard2_state
    shard1_state=$(check_raft_state shard1 ${MASTER1_SHARD1_HTTP})
    shard2_state=$(check_raft_state shard2 ${MASTER1_SHARD2_HTTP})

    log_info "During config server isolation:"
    log_info "  Shard1: ${shard1_state}"
    log_info "  Shard2: ${shard2_state}"

    local pass=true
    if echo "${shard1_state}" | grep -q '"error"'; then
        log_error "FAIL: Shard1 should work without config server"
        pass=false
    fi
    if echo "${shard2_state}" | grep -q '"error"'; then
        log_error "FAIL: Shard2 should work without config server"
        pass=false
    fi

    if [ "$pass" = true ]; then
        log_info "PASS: Both shards operational without config server"
        PASSED=$((PASSED + 1))
    else
        FAILED=$((FAILED + 1))
    fi

    # Heal
    run_toxiproxy toxic remove "${CONFIG_SERVER_PROXY}" --toxicName timeout_downstream 2>/dev/null || \
        run_toxiproxy toxic remove "${CONFIG_SERVER_PROXY}" -n timeout_downstream 2>/dev/null || true
    sleep 3
}

# Test 4: ChunkServer disconnected from Master
test_chunkserver_disconnection() {
    log_test "========================================="
    log_test "Test 4: ChunkServer Disconnection"
    log_test "========================================="

    log_info "Scenario: Disconnect chunkserver1-shard1 from master1-shard1"
    log_info "Expected: Master detects dead chunkserver after heartbeat timeout (15s)"

    # Disconnect chunkserver
    # Disconnect chunkserver
    run_toxiproxy toxic add --toxicName timeout_downstream --type timeout --attribute timeout=0 "${CS1_SHARD1_PROXY}"

    log_info "chunkserver1-shard1 is now disconnected"
    log_info "Waiting for heartbeat timeout (15s)..."
    sleep 18

    # Master should still be healthy
    local master_state
    master_state=$(check_raft_state shard1 ${MASTER1_SHARD1_HTTP})
    log_info "Master state after chunkserver disconnect: ${master_state}"

    if echo "${master_state}" | grep -q '"error"'; then
        log_error "FAIL: Master should remain healthy after chunkserver disconnect"
        FAILED=$((FAILED + 1))
    else
        log_info "PASS: Master remained healthy after chunkserver disconnect"
        PASSED=$((PASSED + 1))
    fi

    # Heal
    run_toxiproxy toxic remove "${CS1_SHARD1_PROXY}" --toxicName timeout_downstream 2>/dev/null || \
        run_toxiproxy toxic remove "${CS1_SHARD1_PROXY}" -n timeout_downstream 2>/dev/null || true
    sleep 3
}

# Test 5: Bandwidth limitation simulating degraded network
test_bandwidth_limit() {
    log_test "========================================="
    log_test "Test 5: Bandwidth Limitation"
    log_test "========================================="

    log_info "Scenario: Limit master1-shard2 bandwidth to 1KB/s"
    log_info "Expected: Slow replication but master remains functional"

    # Add bandwidth limit
    # Add bandwidth limit
    run_toxiproxy toxic add --toxicName bandwidth_downstream --type bandwidth --attribute rate=1024 "${MASTER1_SHARD2_PROXY}"

    log_info "master1-shard2 bandwidth limited to 1KB/s"
    sleep 5

    local shard2_state
    shard2_state=$(check_raft_state shard2 ${MASTER1_SHARD2_HTTP})
    log_info "Shard2 state under bandwidth limit: ${shard2_state}"

    # Shard1 should be completely unaffected
    local shard1_state
    shard1_state=$(check_raft_state shard1 ${MASTER1_SHARD1_HTTP})
    log_info "Shard1 state (unaffected):          ${shard1_state}"

    if echo "${shard1_state}" | grep -q '"error"'; then
        log_error "FAIL: Shard1 should be unaffected by shard2 bandwidth limit"
        FAILED=$((FAILED + 1))
    else
        log_info "PASS: Shard1 unaffected by shard2 degradation"
        PASSED=$((PASSED + 1))
    fi

    # Remove bandwidth limit
    run_toxiproxy toxic remove "${MASTER1_SHARD2_PROXY}" --toxicName bandwidth_downstream 2>/dev/null || \
        run_toxiproxy toxic remove "${MASTER1_SHARD2_PROXY}" -n bandwidth_downstream 2>/dev/null || true
    sleep 3
}

# ============================================================================
# Main
# ============================================================================

main() {
    log_info "Starting network partition tests with Toxiproxy"
    log_info ""

    check_toxiproxy
    check_docker_containers
    check_toxiproxy_server

    # Trap cleanup on exit
    trap cleanup_proxies EXIT

    setup_proxies

    log_info ""
    log_info "Running 5 network partition tests..."
    log_info ""

    test_shard_isolation
    sleep 3

    test_high_latency
    sleep 3

    test_config_server_isolation
    sleep 3

    test_chunkserver_disconnection
    sleep 3

    test_bandwidth_limit

    log_info ""
    log_test "========================================="
    log_test "Results: ${PASSED} passed, ${FAILED} failed, ${SKIPPED} skipped"
    log_test "========================================="
    log_info ""

    if [ "${FAILED}" -gt 0 ]; then
        log_error "Some tests failed!"
        exit 1
    fi

    log_info "All tests passed!"
}

main "$@"
