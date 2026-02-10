# Network Partition Tests

This document describes real network partition testing using Toxiproxy for the Rust Hadoop DFS project.

## Overview

Network partition tests validate that the Raft consensus implementation correctly handles real-world network failures:

1. **Network Partitions**: Complete network isolation between nodes
2. **High Latency**: Delayed packet delivery simulating slow networks
3. **Packet Loss**: Random packet drops simulating unreliable networks
4. **Bandwidth Limitations**: Throttled connections simulating constrained networks
5. **Cascading Failures**: Sequential failures testing recovery paths

## Prerequisites

### Install Toxiproxy

**macOS**:
```bash
brew install toxiproxy
```

**Linux**:
```bash
wget https://github.com/Shopify/toxiproxy/releases/download/v2.9.0/toxiproxy-server-linux-amd64
wget https://github.com/Shopify/toxiproxy/releases/download/v2.9.0/toxiproxy-cli-linux-amd64
chmod +x toxiproxy-*
sudo mv toxiproxy-server-linux-amd64 /usr/local/bin/toxiproxy-server
sudo mv toxiproxy-cli-linux-amd64 /usr/local/bin/toxiproxy-cli
```

**Docker**:
```bash
docker run -d --name toxiproxy -p 8474:8474 ghcr.io/shopify/toxiproxy:2.9.0
```

### Install jq (for JSON parsing)

```bash
# macOS
brew install jq

# Linux
sudo apt-get install jq  # Debian/Ubuntu
sudo yum install jq      # CentOS/RHEL
```

## Running Tests

### Option 1: Manual Testing

1. **Start Toxiproxy server**:
```bash
toxiproxy-server &
```

2. **Start three master nodes** (in separate terminals):
```bash
# Terminal 1
cargo run --bin master -- --node-id 1 --port 50051

# Terminal 2
cargo run --bin master -- --node-id 2 --port 50052

# Terminal 3
cargo run --bin master -- --node-id 3 --port 50053
```

3. **Run the test script**:
```bash
./test_scripts/network_partition_test.sh
```

### Option 2: Docker Compose (Recommended)

1. **Start the cluster with Toxiproxy**:
```bash
docker compose -f docker-compose.toxiproxy.yml up -d
```

2. **Verify all services are running**:
```bash
docker compose -f docker-compose.toxiproxy.yml ps
```

3. **Run tests**:
```bash
./test_scripts/network_partition_test.sh
```

4. **Stop the cluster**:
```bash
docker compose -f docker-compose.toxiproxy.yml down -v
```

## Test Scenarios

### Test 1: Network Partition (Split-Brain)

**Scenario**: Isolate one node from the cluster

**Expected Behavior**:
- Isolated node (minority) cannot commit writes
- Majority partition elects a new leader
- Isolated node steps down to follower
- After healing, cluster converges to single leader

**Verification**:
```bash
# Check Raft state
curl -s http://localhost:50051/raft/state | jq
curl -s http://localhost:50052/raft/state | jq
curl -s http://localhost:50053/raft/state | jq

# Expected: Only one leader, same term across all nodes
```

### Test 2: High Network Latency

**Scenario**: Add 500ms latency to one node

**Expected Behavior**:
- Node with high latency may lose leadership
- Heartbeats timeout, triggering election
- Cluster remains functional with low-latency nodes

**Toxiproxy Command**:
```bash
toxiproxy-cli toxic add master1_proxy -t latency -a latency=500 -a jitter=50
```

### Test 3: Connection Instability (Slow Close)

**Scenario**: Simulate unstable connection by closing it after a delay
**Description**: Uses `slow_close` toxic to allow connection establishment but close it after 1s, checking system resilience to unstable peers.

**Expected Behavior**:
- Raft retries ensure eventual consistency
- Node may temporarily lag but catches up

**Toxiproxy Command**:
```bash
toxiproxy-cli toxic add --name slow_close_downstream master2_proxy -t slow_close -a delay=1000
```

### Test 4: Bandwidth Limitation

**Scenario**: Limit bandwidth to 1KB/s

**Expected Behavior**:
- Log replication slows down
- Cluster remains consistent
- No data loss occurs

**Toxiproxy Command**:
```bash
toxiproxy-cli toxic add master1_proxy -t bandwidth -a rate=1024
```

### Test 5: Cascading Failures

**Scenario**: Sequential node failures and recoveries

**Expected Behavior**:
- Cluster handles failures one at a time
- Leadership transitions correctly
- Final state converges after all recoveries

**Steps**:
1. Isolate master1
2. Add latency to master2
3. Heal master1
4. Remove latency from master2
5. Verify convergence

## Validation Criteria

### 1. Split-Brain Prevention

```bash
# Count leaders
curl -s http://localhost:50051/raft/state | jq '.role' &
curl -s http://localhost:50052/raft/state | jq '.role' &
curl -s http://localhost:50053/raft/state | jq '.role' &
wait

# Expected: Exactly 1 "Leader", 2 "Follower"
```

### 2. Term Consistency

```bash
# Check terms
curl -s http://localhost:50051/raft/state | jq '.current_term' &
curl -s http://localhost:50052/raft/state | jq '.current_term' &
curl -s http://localhost:50053/raft/state | jq '.current_term' &
wait

# Expected: All nodes have same term or higher term on new leader
```

### 3. Log Consistency

```bash
# Check commit indices
curl -s http://localhost:50051/raft/state | jq '.commit_index' &
curl -s http://localhost:50052/raft/state | jq '.commit_index' &
curl -s http://localhost:50053/raft/state | jq '.commit_index' &
wait

# Expected: Commit indices converge after partition heals
```

### 4. Write Availability

```bash
# Test write during partition (should fail on minority)
curl -X POST http://localhost:50051/api/files -d '{"path":"/test","size":100}'

# Expected: Success on majority partition leader, failure on minority
```

## Troubleshooting

### Toxiproxy Not Found

```bash
# Verify installation
which toxiproxy-server
which toxiproxy-cli

# Check version
toxiproxy-server --version
toxiproxy-cli --version
```

### Proxies Not Working

```bash
# List existing proxies
toxiproxy-cli list

# Delete stale proxies
toxiproxy-cli delete master1_proxy
toxiproxy-cli delete master2_proxy
toxiproxy-cli delete master3_proxy

# Recreate proxies
toxiproxy-cli create master1_proxy -l "localhost:60051" -u "localhost:50051"
```

### Masters Not Responding

```bash
# Check if masters are running
curl http://localhost:50051/health
curl http://localhost:50052/health
curl http://localhost:50053/health

# Check logs
docker compose -f docker-compose.toxiproxy.yml logs master1
```

## Advanced Scenarios

### Asymmetric Partition

```bash
# Master1 can talk to Master2 but not Master3
toxiproxy-cli toxic add master1_proxy -t timeout -a timeout=0 --downstream
```

### Flaky Network

```bash
# Random packet loss 10%
toxiproxy-cli toxic add master2_proxy -t latency -a latency=100 -a jitter=50
```

### Network Slowdown

```bash
# Gradual bandwidth reduction
toxiproxy-cli toxic add master1_proxy -t bandwidth -a rate=10240  # 10KB/s
sleep 10
toxiproxy-cli toxic update master1_proxy bandwidth -a rate=1024   # 1KB/s
```

## Cleanup

```bash
# Remove all toxics
toxiproxy-cli toxic remove master1_proxy --toxicName=all
toxiproxy-cli toxic remove master2_proxy --toxicName=all
toxiproxy-cli toxic remove master3_proxy --toxicName=all

# Delete proxies
toxiproxy-cli delete master1_proxy
toxiproxy-cli delete master2_proxy
toxiproxy-cli delete master3_proxy

# Stop Toxiproxy server
pkill toxiproxy-server

# Or if using Docker
docker stop toxiproxy
docker rm toxiproxy
```

## References

- [Toxiproxy GitHub](https://github.com/Shopify/toxiproxy)
- [Raft Consensus Algorithm](https://raft.github.io/)
- [Jepsen: Testing Distributed Systems](https://jepsen.io/)
- [Network Partition Testing Best Practices](https://www.infoq.com/articles/testing-distributed-systems/)

## See Also

- [DYNAMIC_MEMBERSHIP_TESTS.md](DYNAMIC_MEMBERSHIP_TESTS.md) - Raft membership changes
- [CHAOS_TEST.md](../CHAOS_TEST.md) - General chaos engineering guide
- [../dfs/metaserver/tests/network_partition_tests.rs](../dfs/metaserver/tests/network_partition_tests.rs) - Unit tests
