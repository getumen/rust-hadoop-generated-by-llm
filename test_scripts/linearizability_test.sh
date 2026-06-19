#!/bin/bash
set -e

echo "=== Linearizability Testing Framework ==="

COMPOSE_FILE="docker-compose.toxiproxy-sharded.yml"

cleanup() {
    echo "Cleaning up..."
    docker compose -f $COMPOSE_FILE down -v 2>/dev/null || true
}
trap cleanup EXIT

# Start cluster
echo "Starting cluster with Toxiproxy..."
docker compose -f $COMPOSE_FILE up -d --no-build
echo "Waiting for cluster to stabilize (25s)..."
sleep 25

# Self-test the checker
echo ""
echo "=== Checker Self-Test ==="
docker exec dfs-chunkserver-extra /app/dfs_cli check-history --self-test
echo "Checker self-test PASSED"

# Helper: cleanup test files between scenarios
cleanup_test_files() {
    echo "Cleaning test files..."
    for path in /a/lin_0 /a/lin_1 /a/lin_2 /a/lin_3 /z/lin_0 /z/lin_1 /z/lin_2 /z/lin_3; do
        docker exec dfs-chunkserver-extra /app/dfs_cli --config-servers http://config-server:50050 delete "$path" 2>/dev/null || true
    done
    sleep 2
}

# Helper: run workload and check
run_scenario() {
    local name=$1
    echo ""
    echo "=== Scenario: $name ==="

    cleanup_test_files

    # Run workload (50 ops x 5 clients, 4 keys, 30% rename)
    docker exec dfs-chunkserver-extra /app/dfs_cli \
        --config-servers http://config-server:50050 \
        workload --ops 50 --clients 5 --key-space 4 --rename-ratio 0.3 \
        --history /tmp/hist_${name}.jsonl &
    WORKLOAD_PID=$!

    # Wait for workload to warm up
    sleep 5

    # Inject fault (caller sets up inject/heal functions)
    if type inject_${name} &>/dev/null; then
        inject_${name}
    fi

    # Wait for fault duration
    if type duration_${name} &>/dev/null; then
        duration_${name}
    fi

    # Heal fault
    if type heal_${name} &>/dev/null; then
        heal_${name}
    fi

    # Wait for workload to finish
    wait $WORKLOAD_PID || true

    # Check linearizability
    echo "Checking linearizability for $name..."
    docker exec dfs-chunkserver-extra /app/dfs_cli check-history /tmp/hist_${name}.jsonl

    echo "=== $name: PASSED ==="
}

# ===== Scenario definitions =====

# 1. Baseline (no fault)
run_scenario "baseline"

# 2. Coordinator kill
inject_coordinator_kill() {
    echo "Killing coordinator (master1-shard1)..."
    docker kill dfs-master1-shard1 2>/dev/null || true
}
duration_coordinator_kill() { sleep 30; }
heal_coordinator_kill() {
    echo "Restarting coordinator..."
    docker compose -f $COMPOSE_FILE start master1-shard1
    sleep 15
}
run_scenario "coordinator_kill"

# 3. Participant kill
inject_participant_kill() {
    echo "Killing participant (master1-shard2)..."
    docker kill dfs-master1-shard2 2>/dev/null || true
}
duration_participant_kill() { sleep 30; }
heal_participant_kill() {
    echo "Restarting participant..."
    docker compose -f $COMPOSE_FILE start master1-shard2
    sleep 15
}
run_scenario "participant_kill"

# 4. Network partition (cross-shard)
inject_network_partition() {
    echo "Injecting network partition (timeout on shard1 proxy)..."
    docker exec toxiproxy /go/bin/toxiproxy-cli toxic add -t timeout -a timeout=0 master1-shard1 2>/dev/null || true
}
duration_network_partition() { sleep 30; }
heal_network_partition() {
    echo "Healing network partition..."
    docker exec toxiproxy /go/bin/toxiproxy-cli toxic remove -n timeout_downstream master1-shard1 2>/dev/null || true
    sleep 5
}
run_scenario "network_partition"

# 5. ChunkServer kill
inject_chunkserver_kill() {
    echo "Killing chunkserver1-shard1..."
    docker kill dfs-chunkserver1-shard1 2>/dev/null || true
}
duration_chunkserver_kill() { sleep 15; }
heal_chunkserver_kill() {
    echo "Restarting chunkserver..."
    docker compose -f $COMPOSE_FILE start chunkserver1-shard1
    sleep 10
}
run_scenario "chunkserver_kill"

# 6. Leader failover (graceful stop)
inject_leader_failover() {
    echo "Stopping master1-shard1 (graceful)..."
    docker stop dfs-master1-shard1 2>/dev/null || true
}
duration_leader_failover() { sleep 20; }
heal_leader_failover() {
    echo "Restarting master..."
    docker compose -f $COMPOSE_FILE start master1-shard1
    sleep 15
}
run_scenario "leader_failover"

# 7. Delayed commit RPC
inject_delayed_commit() {
    echo "Adding 10s latency to shard2 proxy..."
    docker exec toxiproxy /go/bin/toxiproxy-cli toxic add -t latency -a latency=10000 master1-shard2 2>/dev/null || true
    sleep 3
    echo "Killing coordinator during delayed commit..."
    docker kill dfs-master1-shard1 2>/dev/null || true
}
duration_delayed_commit() { sleep 15; }
heal_delayed_commit() {
    echo "Removing latency and restarting..."
    docker exec toxiproxy /go/bin/toxiproxy-cli toxic remove -n latency_downstream master1-shard2 2>/dev/null || true
    docker compose -f $COMPOSE_FILE start master1-shard1
    sleep 20
}
run_scenario "delayed_commit"

echo ""
echo "=== All linearizability tests PASSED ==="
