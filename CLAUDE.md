# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

A distributed file system (DFS) implemented in Rust, inspired by GFS/HDFS. Key features: Raft consensus for HA, range-based dynamic sharding, cross-shard 2-phase commit transactions with Presumed Abort recovery, synchronous pipeline replication with replica count tracking, epoch-based fencing, erasure coding (RS(6,3)), and an S3-compatible API with SigV4/OIDC/STS/IAM/SSE/Bucket Policy.

## Build Commands

```bash
# Full release build (all crates)
cargo build --release

# Single crate
cargo build -p dfs-metaserver --release

# Binaries produced: target/release/{master,config_server,chunkserver,dfs_cli,s3-server}
```

Requires: Rust 1.91+, `protoc` (protobuf compiler). The build auto-generates gRPC code from `proto/dfs.proto`.

## Testing

```bash
# Unit tests (fast, no Docker needed)
cargo test --lib
cargo test -p dfs-metaserver --lib

# Integration tests (require Docker Compose cluster running)
cargo test -- --ignored

# Full shell-based integration test suite (builds Docker images, runs ~31 scripts sequentially)
./run_all_tests.sh

# Run a single shell test
bash test_scripts/rename_test.sh
```

Debug logging: `RUST_LOG=debug cargo run --bin master -- [args]`

## Architecture

### Workspace Structure

5-crate Cargo workspace under `dfs/`:
- **metaserver** — Master server (file metadata, block allocation) + Config Server (shard topology); contains the custom Raft implementation (`simple_raft.rs`, 132KB)
- **chunkserver** — Data block storage with LRU read cache and pipeline replication
- **client** — Client library with ShardMap caching + `dfs_cli` CLI tool
- **s3_server** — Axum-based S3-compatible REST API gateway
- **common** — Shared sharding logic, TLS/auth utilities, IAM/OIDC credential handling

### System Topology

```
Config Servers (3-node Raft) — own the ShardMap (which Master shard handles which path range)
     │
     ├── Shard 1 Masters (3-node Raft) — paths "" to "/m"
     └── Shard 2 Masters (3-node Raft) — paths "/m" to ""
              │
         ChunkServers (3x replication pipeline)
```

Clients talk to Config Servers to get the ShardMap, then route metadata RPCs to the correct shard's Master leader. Data reads/writes go directly to ChunkServers.

### Key Design Points

- **Raft** (`simple_raft.rs`): Leader election, log replication via RocksDB, snapshots, joint consensus for membership changes. Commit-wait ensures ClientRequest replies only after majority commit. Leader-side log batching (up to 256 events per `WriteBatch`). `step_down_to_follower` centralizes all 8 role transitions.
- **Dynamic sharding**: Masters monitor RPS/BPS; when `split_threshold_rps` is exceeded, the shard finds a midpoint, registers a new shard in Config Server, and migrates metadata via `InitiateShuffle`/`IngestMetadata`
- **Cross-shard transactions**: 2PC with Presumed Abort recovery — `PrepareTransaction` locks resources, `CommitTransaction` applies, `InquireTransaction` enables participant recovery. Source deletion deferred until participant commit confirmed. Idempotency guards on all phases. Coordinator recovery task retries committed-but-unacked transactions.
- **Epoch-based fencing**: ChunkServers track the highest Raft term (epoch) seen and reject stale leader writes. `master_term` propagated through AllocateBlock, Heartbeat, and ChunkServerCommand.
- **Pipeline replication**: Synchronous — each ChunkServer waits for downstream ack. `WriteBlockResponse.replicas_written` reports actual replica count.
- **Background Healer**: Detects under-replicated blocks (replication and EC), schedules replication commands. Triggered by dead server detection and periodic 5-minute scan.
- **MasterState** stores `files: BTreeMap<String, FileMetadata>` (range-friendly for shard splits)

### gRPC API

Defined in `proto/dfs.proto`. Three services: `MasterService` (file ops, block allocation, Raft membership, transactions, InquireTransaction), `ChunkServerService` (WriteBlock/ReadBlock/ReplicateBlock with `master_term` fencing), `ConfigService` (ShardMap management with linearizable reads).

### Linearizability Testing

```bash
# Run WGL linearizability checker self-test
./target/release/dfs_cli check-history --self-test

# Run concurrent workload with history recording
./target/release/dfs_cli workload --ops 50 --clients 5 --key-space 4 --rename-ratio 0.3 --history /tmp/hist.jsonl

# Full linearizability test suite (7 fault scenarios via Docker+Toxiproxy)
bash test_scripts/linearizability_test.sh
```

### Docker Compose Variants

| File | Purpose |
|------|---------|
| `docker-compose.yml` | Default: 2 shards, 3 config servers, 4 chunkservers, S3 server |
| `docker-compose.toxiproxy*.yml` | Adds Toxiproxy for network fault injection tests |
| `docker-compose.auto-scaling.yml` | Tests dynamic shard split under load |

```bash
docker compose up -d --build   # Start full cluster
```

### HTTP Debug Endpoints (per Master/ChunkServer)

- `GET /health` — liveness
- `GET /raft/state` — Raft node state as JSON
- `GET /metrics` — Prometheus metrics
