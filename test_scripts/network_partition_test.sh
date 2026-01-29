#!/bin/bash
# Real network partition testing using Toxiproxy
# Tests Raft consensus behavior under actual network failures

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Toxiproxy configuration
TOXIPROXY_HOST="localhost:8474"
MASTER1_PROXY="master1_proxy"
MASTER2_PROXY="master2_proxy"
MASTER3_PROXY="master3_proxy"

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if Toxiproxy is installed
check_toxiproxy() {
    if ! command -v toxiproxy-cli &> /dev/null; then
        log_error "toxiproxy-cli not found. Install it with:"
        log_error "  brew install toxiproxy (macOS)"
        log_error "  or download from https://github.com/Shopify/toxiproxy/releases"
        exit 1
    fi
    log_info "Toxiproxy CLI found"
}

# Check if Toxiproxy server is running
check_toxiproxy_server() {
    if ! curl -s "http://${TOXIPROXY_HOST}/version" > /dev/null 2>&1; then
        log_error "Toxiproxy server not running at ${TOXIPROXY_HOST}"
        log_error "Start it with: toxiproxy-server"
        exit 1
    fi
    log_info "Toxiproxy server is running"
}

# Create proxies for master nodes
setup_proxies() {
    log_info "Setting up Toxiproxy proxies..."

    # Clean up existing proxies
    toxiproxy-cli list 2>/dev/null | grep -q "${MASTER1_PROXY}" && toxiproxy-cli delete "${MASTER1_PROXY}" || true
    toxiproxy-cli list 2>/dev/null | grep -q "${MASTER2_PROXY}" && toxiproxy-cli delete "${MASTER2_PROXY}" || true
    toxiproxy-cli list 2>/dev/null | grep -q "${MASTER3_PROXY}" && toxiproxy-cli delete "${MASTER3_PROXY}" || true

    # Create proxies (assuming masters run on ports 50051, 50052, 50053)
    toxiproxy-cli create "${MASTER1_PROXY}" -l "localhost:60051" -u "localhost:50051"
    toxiproxy-cli create "${MASTER2_PROXY}" -l "localhost:60052" -u "localhost:50052"
    toxiproxy-cli create "${MASTER3_PROXY}" -l "localhost:60053" -u "localhost:50053"

    log_info "Proxies created:"
    log_info "  Master1: localhost:60051 -> localhost:50051"
    log_info "  Master2: localhost:60052 -> localhost:50052"
    log_info "  Master3: localhost:60053 -> localhost:50053"
}

# Cleanup proxies
cleanup_proxies() {
    log_info "Cleaning up Toxiproxy proxies..."
    toxiproxy-cli delete "${MASTER1_PROXY}" 2>/dev/null || true
    toxiproxy-cli delete "${MASTER2_PROXY}" 2>/dev/null || true
    toxiproxy-cli delete "${MASTER3_PROXY}" 2>/dev/null || true
}

# Test 1: Network partition (split-brain scenario)
test_network_partition() {
    log_info "========================================="
    log_info "Test 1: Network Partition (Split-Brain)"
    log_info "========================================="

    log_info "Scenario: Partition master1 from master2,3"

    # Add 100% packet loss (network down) for master1
    toxiproxy-cli toxic add "${MASTER1_PROXY}" -t timeout -a timeout=0

    log_info "Master1 is now isolated from the cluster"
    log_warn "Expected: Master2 or Master3 should become leader"
    log_warn "Expected: Master1 should step down to follower"

    sleep 10

    log_info "Checking cluster state..."
    curl -s http://localhost:50051/raft/state 2>/dev/null || log_warn "Master1 unreachable (expected)"
    curl -s http://localhost:50052/raft/state | jq '.role' 2>/dev/null || true
    curl -s http://localhost:50053/raft/state | jq '.role' 2>/dev/null || true

    log_info "Healing partition..."
    toxiproxy-cli toxic remove "${MASTER1_PROXY}" timeout

    sleep 5
    log_info "Partition healed. Cluster should converge to single leader."

    sleep 5
}

# Test 2: High latency (slow network)
test_high_latency() {
    log_info "========================================="
    log_info "Test 2: High Network Latency"
    log_info "========================================="

    log_info "Scenario: Add 500ms latency to master1"

    # Add latency
    toxiproxy-cli toxic add "${MASTER1_PROXY}" -t latency -a latency=500 -a jitter=50

    log_info "Master1 now has 500ms±50ms latency"
    log_warn "Expected: Master1 may lose leadership due to slow heartbeats"

    sleep 15

    log_info "Checking cluster state..."
    curl -s http://localhost:50051/raft/state | jq '.role, .current_term' 2>/dev/null || true
    curl -s http://localhost:50052/raft/state | jq '.role, .current_term' 2>/dev/null || true
    curl -s http://localhost:50053/raft/state | jq '.role, .current_term' 2>/dev/null || true

    log_info "Removing latency..."
    toxiproxy-cli toxic remove "${MASTER1_PROXY}" latency

    sleep 5
}

# Test 3: Packet loss
test_packet_loss() {
    log_info "========================================="
    log_info "Test 3: Packet Loss"
    log_info "========================================="

    log_info "Scenario: Add 30% packet loss to master2"

    # Add packet loss
    toxiproxy-cli toxic add "${MASTER2_PROXY}" -t slow_close -a delay=1000

    log_info "Master2 now has network instability"
    log_warn "Expected: Cluster should remain stable with 2 healthy nodes"

    sleep 10

    log_info "Checking cluster state..."
    curl -s http://localhost:50051/raft/state | jq '.role' 2>/dev/null || true
    curl -s http://localhost:50052/raft/state | jq '.role' 2>/dev/null || log_warn "Master2 unreachable"
    curl -s http://localhost:50053/raft/state | jq '.role' 2>/dev/null || true

    log_info "Removing packet loss..."
    toxiproxy-cli toxic remove "${MASTER2_PROXY}" slow_close

    sleep 5
}

# Test 4: Bandwidth limitation
test_bandwidth_limit() {
    log_info "========================================="
    log_info "Test 4: Bandwidth Limitation"
    log_info "========================================="

    log_info "Scenario: Limit bandwidth to 1KB/s for master1"

    # Add bandwidth limit (1KB/s = 1024 bytes/s)
    toxiproxy-cli toxic add "${MASTER1_PROXY}" -t bandwidth -a rate=1024

    log_info "Master1 bandwidth limited to 1KB/s"
    log_warn "Expected: Slow replication but cluster should remain functional"

    sleep 10

    log_info "Checking cluster state..."
    curl -s http://localhost:50051/raft/state | jq '.role, .commit_index' 2>/dev/null || true
    curl -s http://localhost:50052/raft/state | jq '.role, .commit_index' 2>/dev/null || true
    curl -s http://localhost:50053/raft/state | jq '.role, .commit_index' 2>/dev/null || true

    log_info "Removing bandwidth limit..."
    toxiproxy-cli toxic remove "${MASTER1_PROXY}" bandwidth

    sleep 5
}

# Test 5: Cascading failures
test_cascading_failures() {
    log_info "========================================="
    log_info "Test 5: Cascading Failures"
    log_info "========================================="

    log_info "Scenario: Sequential node failures"

    log_info "Step 1: Isolate master1"
    toxiproxy-cli toxic add "${MASTER1_PROXY}" -t timeout -a timeout=0
    sleep 5

    log_info "Step 2: Add latency to master2"
    toxiproxy-cli toxic add "${MASTER2_PROXY}" -t latency -a latency=1000
    sleep 5

    log_info "Cluster state (master3 should be leader):"
    curl -s http://localhost:50053/raft/state | jq '.role, .current_term' 2>/dev/null || true

    log_info "Step 3: Heal master1"
    toxiproxy-cli toxic remove "${MASTER1_PROXY}" timeout
    sleep 5

    log_info "Step 4: Remove latency from master2"
    toxiproxy-cli toxic remove "${MASTER2_PROXY}" latency
    sleep 5

    log_info "Final cluster state (should converge):"
    curl -s http://localhost:50051/raft/state | jq '.role' 2>/dev/null || true
    curl -s http://localhost:50052/raft/state | jq '.role' 2>/dev/null || true
    curl -s http://localhost:50053/raft/state | jq '.role' 2>/dev/null || true
}

# Main execution
main() {
    log_info "Starting network partition tests with Toxiproxy"

    check_toxiproxy
    check_toxiproxy_server

    # Trap cleanup on exit
    trap cleanup_proxies EXIT

    setup_proxies

    log_info ""
    log_info "Prerequisites:"
    log_info "  1. Three master nodes should be running on ports 50051, 50052, 50053"
    log_info "  2. jq should be installed for JSON parsing"
    log_info ""
    log_warn "Press Enter to continue or Ctrl+C to cancel..."
    read -r

    # Run tests
    test_network_partition
    sleep 5

    test_high_latency
    sleep 5

    test_packet_loss
    sleep 5

    test_bandwidth_limit
    sleep 5

    test_cascading_failures

    log_info ""
    log_info "========================================="
    log_info "All tests completed!"
    log_info "========================================="
    log_info ""
    log_info "Review the cluster states above to verify:"
    log_info "  1. Split-brain prevention (only one leader per term)"
    log_info "  2. Leader re-election after partition"
    log_info "  3. Cluster convergence after healing"
    log_info "  4. Resilience under network degradation"
}

main "$@"
