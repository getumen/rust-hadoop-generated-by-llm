# Rust Hadoop DFS - TODO List

## 🔴 High Priority (Production Readiness & Stability)

### 12. Observability (Improved Tracking)
**Status**: **Mostly Completed** (Phase 1 & 2)
**Priority**: High
**Effort**: Medium

**Tasks**:
- [x] Structured logging (standardized with `tracing` macros)
- [x] Implement distributed tracing (End-to-end Request ID)
- [x] Add request ID propagation (gRPC interceptors & S3 headers)
- [ ] Implement log aggregation (Loki/Jaeger exporters)
- [ ] Add alerting rules for:
  - [ ] Leader election failures
  - [ ] Log replication lag
  - [ ] Disk space for logs
  - [ ] Network partition detection
- [ ] Create operational dashboards (Grafana metrics integration)

### 16. Code Quality & Technical Debt
**Status**: Mostly Completed
**Priority**: High
**Effort**: Medium

**Tasks**:
- [x] remove unused dependencies
- [x] Add comprehensive error handling (remove unwrap() calls)
- [x] Implement proper async error propagation
- [x] Add type aliases for common types (`SharedAppState`, `SharedShardMap`, `RaftResult`)
- [/] Refactor large functions into smaller units (identified `handle_rpc` as 446 lines)
- [x] Add code comments for complex logic (module docs, `RaftNode` struct docs)
- [x] Run clippy and fix warnings
- [x] Add rustfmt configuration and enforce formatting
- [x] Fix deprecated `rand` usage in `simple_raft.rs`

### 7. Read Optimization
**Status**: **Completed**
**Priority**: High
**Effort**: Medium

**Solution**:
- Implement ReadIndex optimization
- Allow Followers to serve reads with bounded staleness
- Add read-only mode configuration
- Partial block reads with offset/length parameters
- Concurrent block fetching for improved throughput
- LRU block cache on ChunkServer
- Optimized S3 range requests

**Tasks**:
- [x] Implement ReadIndex protocol
- [x] **Partial Read Support**: Added offset/length parameters to ReadBlockRequest/Response
- [x] **ChunkServer Optimization**: Implemented partial block reads with seek-based I/O
- [x] **Block Caching**: Added LRU cache (configurable via BLOCK_CACHE_SIZE env var, default: 100 blocks)
- [x] **Concurrent Downloads**: Implemented `get_file_concurrent()` for parallel block fetching
- [x] **Range Read API**: Added `read_file_range()` method to Client library
- [x] **S3 Gateway**: Optimized Range requests to use partial reads instead of full downloads
- [ ] Add lease-based read optimization
- [ ] Add configuration for read consistency level
- [ ] Implement stale read detection
- [ ] Allow Follower reads
- [ ] Add metrics for read latency by consistency level
- [ ] Implement streaming block response support (gRPC streaming)
- [ ] Add read-ahead strategy for sequential workloads

### 13. ChunkServer Improvements
**Status**: Working
**Priority**: High
**Effort**: Medium

**Remaining Tasks**:
- [ ] Etcd-style Lease Check (GrantLease/KeepAlive RPCs)
- [ ] Rack Awareness (Initial implementation)

---

## 🟡 Medium Priority (Infrastructure & Performance)

### 5. Dynamic Membership Changes (Raft Configuration Management)
**Status**: **Completed** ✅
**Priority**: Medium
**Effort**: Medium

**Background**:
動的なメンバーシップ変更は、Raftクラスタの稼働中にノードを追加・削除する機能です。
Raft標準のJoint Consensus、Leader Transfer、Catch-upプロトコルを完全実装しました。

**Tasks**:
- [x] Design configuration change protocol
- [x] Implement AddServer/RemoveServer RPC
- [x] Add configuration log entries to Raft log
- [x] **Implement joint consensus phase** (Raftの標準安全メカニズム、Split Brain防止)
- [x] **Implement automatic leader transfer** (削除対象ノードがLeaderの場合)
- [x] **Implement catch-up protocol** (新サーバーの安全な追加)
- [x] **Integration tests** (17 unit tests + integration test script)
- [x] Add CLI commands for cluster management
- [x] Add safety checks (prevent removing majority)
- [x] **HTTP API extensions** (`/raft/state` with cluster_config and config_change_state)
- [x] **Test documentation** ([DYNAMIC_MEMBERSHIP_TESTS.md](test_scripts/DYNAMIC_MEMBERSHIP_TESTS.md))

**Test Coverage**:
- ✅ 17 unit tests (全て成功)
- ✅ Integration test script: [dynamic_membership_test.sh](test_scripts/dynamic_membership_test.sh)
- ✅ 6つのテストフェーズ（起動、追加、設定確認、安全機構、レプリケーション、バージョニング）

### 8. Raft Performance Optimizations
**Status**: Not Started
**Priority**: Medium
**Effort**: Large

**Optimizations**:
- [ ] Batch log entries
- [ ] Pipeline AppendEntries
- [ ] Implement pre-vote to reduce unnecessary elections
- [ ] Add leadership transfer for graceful shutdown
- [ ] Optimize heartbeat frequency based on cluster size
- [ ] Implement log entry compression

### 9. Testing Infrastructure
**Status**: Basic
**Priority**: Medium
**Effort**: Large

**Tasks**:
- [ ] Add unit tests for Raft logic
- [ ] Add integration tests for network partitions
  - [ ] Multi-node scenarios
  - [ ] Network partition simulation
  - [ ] Clock skew simulation
- [ ] Add property-based tests (using proptest)
- [ ] Implement Jepsen-style tests
- [ ] Add performance benchmarks
- [ ] Add stress tests for high write throughput

### 17. Build and Deployment
**Status**: Not Started
**Priority**: Medium
**Effort**: Medium

**Tasks**:
- [ ] Optimize Docker image size
- [ ] Add CI/CD pipeline
- [ ] Implement blue-green deployment
- [ ] Add rolling update support
- [ ] Kubernetes manifests
- [ ] Add Helm chart
- [ ] Implement backup and restore procedures

### 18. Refactor RPC Responses
**Status**: Not Started
**Priority**: Medium
**Effort**: Small

**Tasks**:
- [ ] Standardize RPC response formats (consistent success/error/hint fields)
- [ ] Use gRPC error details for structured error information instead of custom string parsing

---

## 🟢 Low Priority (Future & Advanced Features)

### 11. Security Enhancements
**Status**: Not Started
**Priority**: Low (for prototype)
**Effort**: Large

**Tasks**:
- [ ] TLS for Raft
- [ ] Implement authentication for Master-to-Master communication
- [ ] Add authorization for client requests
- [ ] Implement audit logging
- [ ] Add encryption at rest for logs
- [ ] Implement secure key rotation

### 14. Rack Awareness
**Status**: Not Started
**Priority**: Low
**Effort**: Medium

**Solution**:
- Implement rack-aware replica placement policy
- Configurable topology script (like Hadoop)

**Tasks**:
- [ ] Add rack configuration to ChunkServer registration
- [ ] Implement topology mapping logic in Master
- [ ] Update block placement policy (1 local, 1 remote rack, 1 same remote rack)
- [ ] Add rack awareness to Balancer

### 15. Storage Efficiency (Erasure Coding)
**Status**: Not Started
**Priority**: Low
**Effort**: Large

**Tasks**:
- [ ] Research Rust Erasure Coding libraries (e.g., `reed-solomon-erasure`)
- [ ] Implement EC encoding/decoding logic in ChunkServer
- [ ] Update Master to handle EC block placement
- [ ] Implement background encoding for cold files
- [ ] Add reconstruction logic for failed EC blocks

### 20. Dynamic Sharding (Load-based Splitting)
**Status**: **Completed**
**Priority**: High
**Effort**: Large

**Objective**: Split shards based on read/write throughput (PPS/BPS) and ensure prefix locality (S3/Colossus style).

**Tasks**:
- [x] Transition from Consistent Hashing to Range-based Sharding
- [x] Implement throughput monitoring per prefix/shard
- [x] Implement Shard Split logic in Raft and Master state
- [x] Implement Client-side handling of shard redirects for dynamic ranges
- [x] Master registration & Heartbeats (Metadata migration support)
- [x] ChunkServer dynamic master discovery (Phase 3)
- [x] Implement actual block data migration (Data Shuffling)
- [x] Add auto-scaling/load-balancing logic for shards

## ✅ Completed & Archived

### 1. Master Server Sharding
**Status**: **Completed** (Phase 1)
- [x] Core Sharding Logic
- [x] Cluster Topology & Configuration
- [x] Request Routing
- [x] Cross-Shard Operations (Transaction Record)

### 2. Client Library Refactoring
**Status**: **Completed**
- [x] Extracted `Client` struct and gRPC connection management
- [x] ShardMap caching and smart routing in library

### 3. ChunkServer Liveness & Balancer
**Status**: **Completed**
- [x] Lease-based Liveness Check (Heartbeat)
- [x] ChunkServer load balancing
- [x] Automatic replica rebalancing (Balancer)

### 4. Safe Mode
**Status**: **Completed**
- [x] Safe Mode state machine and block reporting threshold


### 6. Health Checks and Monitoring
**Status**: **Completed** (Phase 1)
- [x] /health and Raft state endpoints
- [x] Prometheus metrics and Grafana template

### 10. Documentation
**Status**: **Completed**
- [x] README, S3_COMPATIBILITY, MASTER_HA, REPLICATION, CHAOS_TEST guides.

>>>>>>> 9c6690d13a61bc311f737db4d062ee3bc0654380
### 19. S3 REST API Compatibility
**Status**: **Completed** (Core)
- [x] Bucket & Object operations
- [x] Multipart Upload
- [x] CopyObject & Multi-Object Delete
- [x] MD5 ETag support
- [ ] *Optional: Presigned URLs (Deferred to Phase 5)*

---

## 🎯 Roadmap

### Phase 1-3: Foundation & Scalability (Completed)
- ✅ Basic Raft & HA
- ✅ Persistence & Snapshots
- ✅ ChunkServer Liveness & Balancer
- ✅ Master Server Sharding
- ✅ Core S3 Compatibility

### Phase 4: Production Readiness (Current)
- ✅ Observability: Structured Logging & End-to-End Tracing
- ✅ Read Index Optimization (Leader Reads)
- 🟡 Lease-based Heartbeats & Reliability
- ✅ Code Quality & Tech Debt Reduction (Phase 1)

### Phase 5: Advanced Ecosystem & Scalability (Next)
- ✅ Dynamic Sharding: Load-based range splitting (Completed)
- 🟢 High-performance S3 (Presigned URLs, efficient CopyObject)
- 🟢 Security (TLS, AuthN/AuthZ)
- 🟢 Storage Efficiency (Erasure Coding)
- 🟢 Rack Awareness

**Last Updated**: 2026-01-27
**Maintainer**: Development Team
