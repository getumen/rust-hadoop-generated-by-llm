# Rust Hadoop DFS - TODO List

## 🎯 Production Readiness Roadmap

本番運用に向けた優先順位で整理しています。

---

## 🔴 Tier 1: 運用に必須（最優先）

### 1. Testing Infrastructure
**Status**: Basic
**Priority**: 🔴 Critical
**Effort**: Large
**Rationale**: 本番運用前にネットワーク分断・障害シナリオのテストは必須。Jepsen風テストで信頼性を担保。

**Tasks**:
- [x] Add unit tests for Raft logic (16 tests covering leader election, log replication, commit, term management, ReadIndex)
- [x] Add integration tests for network partitions (11 tests with mock network layer)
  - [x] Multi-node scenarios (split-brain prevention, leader election, partition healing)
  - [x] Partition simulation (2-way, 3-way, symmetric, cascading)
  - [x] Real network partition testing with `toxiproxy` (5 test scenarios: partition, latency, packet loss, bandwidth limit, cascading failures)
  - [ ] Clock skew simulation
- [x] Add property-based tests (15 tests using proptest - log invariants, quorum intersection, state machine determinism)
- [x] Implement Jepsen-style consistency tests (12 tests - history recording, linearizability checker, bank account invariant, concurrent operations, fault injection)
- [ ] Add performance benchmarks
- [ ] Add stress tests for high write throughput

---

### 2. Observability - Alerting & Dashboards
**Status**: Partially Completed (Phase 1 & 2 done)
**Priority**: 🔴 Critical
**Effort**: Medium
**Rationale**: 障害検知ができないと本番運用は不可能。アラートルールとダッシュボードは必須。

**Completed**:
- [x] Structured logging (standardized with `tracing` macros)
- [x] Implement distributed tracing (End-to-End Request ID)
- [x] Add request ID propagation (gRPC interceptors & S3 headers)

**Remaining Tasks**:
- [ ] Implement log aggregation (Loki/Jaeger exporters)
- [ ] Add alerting rules for:
  - [ ] Leader election failures
  - [ ] Log replication lag
  - [ ] Disk space for logs
  - [ ] Network partition detection
  - [ ] ChunkServer heartbeat failures
- [ ] Create operational dashboards (Grafana metrics integration)

---

### 3. Build and Deployment
**Status**: Not Started
**Priority**: 🔴 Critical
**Effort**: Medium
**Rationale**: CI/CD、ローリングアップデート、K8s対応がないと運用コストが高い。

**Tasks**:
- [ ] Add CI/CD pipeline (GitHub Actions)
- [ ] Optimize Docker image size (multi-stage build)
- [ ] Kubernetes manifests
- [ ] Add Helm chart
- [ ] Implement rolling update support
- [ ] Implement blue-green deployment
- [ ] Implement backup and restore procedures

---

## 🟡 Tier 2: 安定運用に重要

### 4. ChunkServer Improvements
**Status**: Mostly Working
**Priority**: 🟡 High
**Effort**: Small-Medium
**Rationale**: etcd風のLease CheckでChunkServerの正確な生存確認を実現。

**Remaining Tasks**:
- [ ] Etcd-style Lease Check (GrantLease/KeepAlive RPCs)
- [ ] Rack Awareness (Initial implementation) → 詳細は #9 参照

---

### 5. ReadIndex-based Follower Read
**Status**: Not Started
**Priority**: 🟡 High
**Effort**: Medium
**Rationale**: Leaderへの読み取り負荷を分散し、読み取りスケーラビリティを向上。Linearizable整合性を維持。

**Background**:
現在、すべてのRead操作はRaft Leaderのみが処理。FollowerがLeaderにReadIndexを問い合わせ、自身のState Machineから読み取ることで負荷分散を実現。

**Tasks**:
- [ ] Add `GetReadIndex` RPC to proto for Follower→Leader communication
- [ ] Add `WaitForApply` event to Raft layer (wait until `last_applied >= read_index`)
- [ ] Modify `ensure_linearizable_read` to support Follower path
- [ ] Implement Follower→Leader ReadIndex forwarding via gRPC
- [ ] Add `allow_follower_read` option to read RPCs (client選択可能)
- [ ] Create `follower_read_test.sh` integration test
- [ ] Add unit tests for ReadIndex forwarding logic

---

### 6. Raft Performance Optimizations
**Status**: Not Started
**Priority**: 🟡 High
**Effort**: Large
**Rationale**: 書き込みスループット向上、大規模クラスタでの効率改善。

**Optimizations**:
- [ ] Batch log entries
- [ ] Batch metadata updates (multiple files in single Raft commit)
- [ ] Pipeline AppendEntries
- [ ] Implement pre-vote to reduce unnecessary elections
- [ ] Add leadership transfer for graceful shutdown
- [ ] Optimize heartbeat frequency based on cluster size
- [ ] Implement log entry compression
- [ ] Group commit (batch multiple client writes)

---

### 7. Refactor RPC Responses
**Status**: Not Started
**Priority**: 🟡 High
**Effort**: Small
**Rationale**: gRPC error detailsを使った統一的なエラーハンドリングでデバッグ効率向上。

**Tasks**:
- [ ] Standardize RPC response formats (consistent success/error/hint fields)
- [ ] Use gRPC error details for structured error information instead of custom string parsing

---

### 8. Code Quality & Technical Debt
**Status**: Mostly Completed
**Priority**: 🟡 Medium
**Effort**: Small
**Rationale**: 継続的なコード品質維持。

**Completed**:
- [x] Remove unused dependencies
- [x] Add comprehensive error handling (remove unwrap() calls)
- [x] Implement proper async error propagation
- [x] Add type aliases for common types (`SharedAppState`, `SharedShardMap`, `RaftResult`)
- [x] Add code comments for complex logic (module docs, `RaftNode` struct docs)
- [x] Run clippy and fix warnings
- [x] Add rustfmt configuration and enforce formatting
- [x] Fix deprecated `rand` usage in `simple_raft.rs`

**Remaining**:
- [/] Refactor large functions into smaller units (identified `handle_rpc` as 446 lines)

---

## 🟢 Tier 3: スケール・セキュリティ

### 9. Security Enhancements
**Status**: Not Started
**Priority**: 🟢 Medium (本番では必須だが後回し可)
**Effort**: Large
**Rationale**: 暗号化通信と認証は本番環境では必須。最低限TLSのみ先行実装も選択肢。

**Tasks**:
- [ ] TLS for Raft communication
- [ ] TLS for Client-Master/ChunkServer communication
- [ ] Implement authentication for Master-to-Master communication
- [ ] Add authorization for client requests (ACL)
- [ ] Implement audit logging
- [ ] Add encryption at rest for logs
- [ ] Implement secure key rotation

---

### 10. Rack Awareness
**Status**: Not Started
**Priority**: 🟢 Medium
**Effort**: Medium
**Rationale**: 障害耐性向上、データセンター障害への対応。

**Solution**:
- Implement rack-aware replica placement policy
- Configurable topology script (like Hadoop)

**Tasks**:
- [ ] Add rack configuration to ChunkServer registration
- [ ] Implement topology mapping logic in Master
- [ ] Update block placement policy (1 local, 1 remote rack, 1 same remote rack)
- [ ] Add rack awareness to Balancer

---

### 11. Storage Efficiency (Erasure Coding)
**Status**: Not Started
**Priority**: 🟢 Low
**Effort**: Large
**Rationale**: ストレージ効率向上（冷データ向け、後回しでOK）。RS(6,3)でストレージコスト約75%削減可能。

**Tasks**:
- [ ] Research Rust Erasure Coding libraries (e.g., `reed-solomon-erasure`)
- [ ] Implement EC encoding/decoding logic in ChunkServer
- [ ] Update Master to handle EC block placement
- [ ] Implement background encoding for cold files
- [ ] Add reconstruction logic for failed EC blocks

---

### 12. Storage Tiering (Hot/Warm/Cold)
**Status**: Not Started
**Priority**: 🟢 Medium
**Effort**: Large
**Rationale**: アクセス頻度に基づいてデータを階層化し、ストレージコストを大幅削減。

**Tiers**:
- **Hot (SSD)**: 頻繁アクセス、3x replication
- **Warm (HDD)**: 1週間未アクセス、2x replication
- **Cold (External/S3)**: 30日未アクセス、Erasure Coding

**Tasks**:
- [ ] Add `last_access_time` metadata to files
- [ ] Implement background tier migration daemon
- [ ] Add promotion logic (Cold→Hot on read)
- [ ] Create lifecycle policy configuration (YAML)
- [ ] Add CLI for manual tier migration

---

## 🟡 Tier 2: パフォーマンス最適化（追加項目）

### 13. Data Compression
**Status**: Not Started
**Priority**: 🟡 High
**Effort**: Small
**Rationale**: ネットワーク帯域とストレージ使用量を削減。即効性が高くコスト対効果良好。

**Compression Options**:
- **LZ4**: 高速、Hot Tier向け
- **Zstd**: バランス良好、Warm Tier向け
- **Zstd -19**: 高圧縮率、Cold/Archive向け

**Tasks**:
- [ ] Add block-level compression (64KB - 1MB chunks)
- [ ] Store compression algorithm in block metadata
- [ ] Implement transparent decompression on read
- [ ] Add compression ratio metrics
- [ ] Make compression configurable per-file or per-directory

---

### 14. Connection Pooling & Network Optimization
**Status**: Not Started
**Priority**: 🟡 High
**Effort**: Small
**Rationale**: gRPC接続のreuse、レイテンシー削減。即効性が高い。

**Tasks**:
- [ ] Implement gRPC connection pooling for Master→ChunkServer
- [ ] Add Client-side connection caching for multiple Masters
- [ ] Implement gRPC keep-alive configuration
- [ ] Add network transfer compression (LZ4 for RPC payloads)
- [ ] Locality-aware routing (prefer same-rack ChunkServer)

---

## 🟢 Tier 3: コスト最適化（長期）

### 15. Block-level Deduplication
**Status**: Not Started
**Priority**: 🟢 Low
**Effort**: Medium
**Rationale**: バックアップやログファイルで50-90%のストレージ削減可能。

**Tasks**:
- [ ] Implement content-addressable block storage (hash-based)
- [ ] Add reference counting for shared blocks
- [ ] Implement garbage collection for unreferenced blocks
- [ ] Add deduplication ratio metrics

---

## 🔵 Future Enhancements (Phase 2+)

### Read Optimization - Phase 2
**Status**: Phase 1 Completed ✅
**Priority**: 🔵 Future
**Effort**: Medium

**Completed (Phase 1)**:
- ✅ ReadIndex optimization for Leader reads
- ✅ Partial block reads with offset/length parameters
- ✅ Concurrent block fetching for improved throughput
- ✅ LRU block cache on ChunkServer (configurable via BLOCK_CACHE_SIZE, default: 100 blocks)
- ✅ Optimized S3 range requests (HTTP 206 Partial Content)
- ✅ Block size adjustment based on total file size upon completion
- ✅ Seek-based I/O for efficient partial block reads

**Future Enhancements (Phase 2)**:
- [ ] Add lease-based read optimization
- [ ] Add configuration for read consistency level
- [ ] Implement stale read detection
- [ ] Allow Follower reads with bounded staleness
- [ ] Add metrics for read latency by consistency level
- [ ] Implement streaming block response support (gRPC streaming)
- [ ] Add read-ahead strategy for sequential workloads
- [ ] Predictive prefetch for sequential access patterns
- [ ] Client-side block cache (complement to ChunkServer LRU cache)

---

### Write Path Optimization
**Status**: Not Started
**Priority**: 🟡 Medium
**Effort**: Medium
**Rationale**: 書き込みレイテンシー削減、スループット向上。

**Tasks**:
- [ ] Async Replication (1レプリカ確認でACK、残りは非同期)
- [ ] Write-back buffer on ChunkServer
- [ ] Parallel block upload from Client
- [ ] Zero-copy I/O (`sendfile`/`splice` for reduced memory copies)

---

### S3 REST API - Advanced Features
**Status**: Core Completed ✅
**Priority**: 🔵 Future
**Effort**: Medium

**Completed**:
- [x] Bucket & Object operations
- [x] Multipart Upload
- [x] CopyObject & Multi-Object Delete
- [x] MD5 ETag support

**Future**:
- [ ] Presigned URLs
- [ ] Versioning support
- [ ] Object tagging
- [ ] Lifecycle policies

---

## ✅ Completed & Archived

### Master Server Sharding
**Status**: ✅ Completed (Phase 1)
- [x] Core Sharding Logic
- [x] Cluster Topology & Configuration
- [x] Request Routing
- [x] Cross-Shard Operations (Transaction Record)

### Dynamic Sharding (Load-based Splitting)
**Status**: ✅ Completed
- [x] Transition from Consistent Hashing to Range-based Sharding
- [x] Implement throughput monitoring per prefix/shard
- [x] Implement Shard Split logic in Raft and Master state
- [x] Implement Client-side handling of shard redirects for dynamic ranges
- [x] Master registration & Heartbeats (Metadata migration support)
- [x] ChunkServer dynamic master discovery (Phase 3)
- [x] Implement actual block data migration (Data Shuffling)
- [x] Add auto-scaling/load-balancing logic for shards

### Dynamic Membership Changes (Raft Configuration Management)
**Status**: ✅ Completed
- [x] Design configuration change protocol
- [x] Implement AddServer/RemoveServer RPC
- [x] Add configuration log entries to Raft log
- [x] Implement joint consensus phase (Split Brain防止)
- [x] Implement automatic leader transfer
- [x] Implement catch-up protocol
- [x] Integration tests (17 unit tests + integration test script)
- [x] Add CLI commands for cluster management
- [x] Add safety checks (prevent removing majority)
- [x] HTTP API extensions (`/raft/state` with cluster_config and config_change_state)
- [x] Test documentation: [DYNAMIC_MEMBERSHIP_TESTS.md](test_scripts/DYNAMIC_MEMBERSHIP_TESTS.md)

### Client Library Refactoring
**Status**: ✅ Completed
- [x] Extracted `Client` struct and gRPC connection management
- [x] ShardMap caching and smart routing in library

### ChunkServer Liveness & Balancer
**Status**: ✅ Completed
- [x] Lease-based Liveness Check (Heartbeat)
- [x] ChunkServer load balancing
- [x] Automatic replica rebalancing (Balancer)

### Safe Mode
**Status**: ✅ Completed
- [x] Safe Mode state machine and block reporting threshold

### Health Checks and Monitoring
**Status**: ✅ Completed (Phase 1)
- [x] /health and Raft state endpoints
- [x] Prometheus metrics and Grafana template

### Documentation
**Status**: ✅ Completed
- [x] README, S3_COMPATIBILITY, MASTER_HA, REPLICATION, CHAOS_TEST guides

---

## 📅 Recommended Action Plan

```
Week 1-2:  Testing Infrastructure（ネットワーク分断テスト、Jepsen風テスト導入）
Week 3:    Alerting Rules + Grafana Dashboard完成
Week 4-5:  CI/CD + K8s Manifests + Helm Chart
Week 6:    ChunkServer Lease Check + RPC Refactor
Week 7+:   Raft Performance / Security
```

---

**Last Updated**: 2026-01-29
**Maintainer**: Development Team
