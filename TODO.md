# Rust Hadoop DFS - TODO List

## 🔴 High Priority (Critical for Production)

### 1. Master Server Sharding
**Status**: **Completed** (Phase 1)
**Priority**: High
**Effort**: Large

**Problem**:
- Single Master limits the number of files (metadata fits in RAM)
- Single Master limits metadata operation throughput

**Solution**:
- Partition the file namespace across multiple Master groups (Shards)
- Each Shard operates as an independent Raft cluster for HA
- Implement a mechanism to route client requests to the correct Shard

**Tasks**:
- [x] **1.1 Core Sharding Logic**
- [x] **1.2 Cluster Topology & Configuration**
- [x] **1.3 Request Routing (Server-Side)**
- [x] **1.4 Client-Side Routing**
- [x] **1.5 Shard Management (Raft-based)**
- [x] **1.6 Cross-Shard Operations (Transaction Record方式)**
  - [x] Design cross-shard rename using Transaction Record pattern
  - [x] Implement Transaction Record state management
  - [x] Implement `rename` RPC handler (coordinator)
  - [x] Implement `prepare_transaction` RPC handler
  - [x] Implement `commit_transaction` RPC handler
  - [x] Implement `abort_transaction` RPC handler
  - [x] Implement timeout cleanup
  - [x] Add Rename subcommand to dfs_cli.rs
  - [x] **Testing**
    - [x] Test same-shard rename
    - [x] Test cross-shard rename with Transaction Record
    - [x] Test transaction timeout and abort
    - [x] Test fault recovery (shard crash during Prepare/Commit)

---

### 2. Client Library Refactoring
**Status**: **Completed**
**Priority**: High
**Effort**: Medium

**Problem**:
- Current `dfs_cli` contains all client logic tightly coupled with CLI argument parsing
- Cannot easily reuse client logic in other applications (e.g., Fuse client, API server)
- `ShardMap` caching and smart routing logic belongs in a library, not the CLI tool

**Solution**:
- Extract client logic into a `rust-hadoop-client` library (or module `src/client`)
- `dfs_cli` should become a thin wrapper around this library

**Tasks**:
- [x] Create `Client` struct in `src/client/mod.rs`
- [x] Move RPC connection management (Leader discovery, retry logic) to `Client`
- [x] Move `ShardMap` caching and smart routing logic to `Client`
- [x] Expose clean async API (`create_file`, `write_file`, `read_file`, `rename`, etc.)
- [x] Refactor `dfs_cli.rs` to use the new `Client`
- [x] Add integration tests using the usage of `Client` library directly

---

### 3. ChunkServer Liveness & Balancer
**Status**: Completed
**Priority**: High
**Effort**: Medium

**Completed Improvements**:
- [x] Implement Lease-based Liveness Check (Heartbeat)
- [x] Implement ChunkServer heartbeat to all Masters (Shared Storage Pool)
- [x] ChunkServer load balancing (based on available space)
- [x] Automatic replica rebalancing (Balancer)

---

## 🟡 Medium Priority (Important for Stability)

### 4. Safe Mode
**Status**: **Completed**
**Priority**: Medium
**Effort**: Medium

**Problem**:
- Cluster accepts writes immediately upon startup
- Risk of unnecessary replication before all ChunkServers register
- Incomplete view of cluster state during startup

**Solution**:
- Implement Safe Mode state in Master
- Block write operations during Safe Mode
- Exit Safe Mode only when threshold of blocks are reported

**Tasks**:
- [x] Implement Safe Mode state machine
- [x] Add block reporting threshold logic (e.g., 99% of blocks reported)
- [x] Block modification RPCs during Safe Mode
- [x] Add CLI command to manually enter/leave Safe Mode
- [ ] Show Safe Mode status in web UI/metrics

---

### 5. Raft Configuration Management
**Status**: **Mostly Completed**
**Priority**: Medium
**Effort**: Medium

**Problem**:
- Cluster membership is static (defined in docker-compose.yml)
- Cannot add/remove Masters dynamically
- No support for cluster reconfiguration

**Solution**:
- Implement Raft joint consensus for membership changes
- Add AddServer/RemoveServer RPCs
- Implement configuration log entries

**Tasks**:
- [x] Design configuration change protocol
- [x] Implement AddServer/RemoveServer RPC
- [x] Add configuration log entries to Raft log
- [ ] Implement joint consensus phase (using single-server changes for safety)
- [x] Add CLI commands for cluster management
- [x] Add safety checks (prevent removing majority)

---

### 6. Health Checks and Monitoring
**Status**: **Mostly Completed**
**Priority**: Medium
**Effort**: Small

**Current State**:
- Basic Docker health checks exist
- No Raft-specific health metrics

**Tasks**:
- [x] Add `/health` endpoint
- [x] Expose Raft state (role, term, commit_index) via HTTP
- [x] Add Prometheus metrics endpoint
- [x] Implement metrics (Role, Term, Log size, Latency)
  - [x] Current role (Leader/Follower/Candidate)
  - [x] Term number
  - [x] Log size
  - [x] Commit index
  - [x] Last applied index
  - [x] Number of votes received
  - [ ] Heartbeat latency
- [x] Add Grafana dashboard template

---

### 19. S3 REST API Compatibility
**Status**: **In Progress**
**Priority**: Medium
**Effort**: Very Large

**Problem**:
- Clients currently must use custom gRPC/TCP client or CLI
- No support for standard tools (AWS CLI, SDKs, Cyberduck, etc.)
- Difficult to integrate with existing ecosystem

**Solution**:
- Implement an S3-compatible API Gateway
- Translate S3 REST calls to internal DFS gRPC calls

**Milestones**:

#### Milestone 1: Basic Operations & Spark Prerequisites
- [x] Implement `s3_server` binary (using Axum)
- [x] Implement Bucket operations (CreateBucket, DeleteBucket, ListBuckets, HeadBucket)
  - *Note: Map Buckets to top-level directories*
- [x] Implement Object operations (PutObject, GetObject, DeleteObject, HeadObject)
  - [x] Simple single-part upload/download
- [x] Support for Directory Simulation (ListObjects with `prefix` and `delimiter`)
- [x] Basic authentication (V4 Signature or dummy auth for dev)

#### Milestone 2: Multipart Upload & Rename Support (Crucial for Spark)
- [x] Implement InitiateMultipartUpload
- [x] Implement UploadPart (map to DFS blocks)
- [x] Implement CompleteMultipartUpload
- [x] Implement AbortMultipartUpload
- [x] Implement CopyObject (required for `rename()` simulation)

#### Milestone 3: Advanced Features
- [ ] Support for Object Metadata (User-defined tags)
- [ ] Support for Range Requests (Partial content)
- [ ] Support for ListObjectsV2 (Pagination)
- [ ] Presigned URLs

---

## 🟢 Low Priority (Nice to Have)

### 7. Read Optimization
**Status**: Not Started
**Priority**: Low
**Effort**: Medium

**Problem**:
- All reads currently go through the Leader
- Unnecessary load on Leader for read-heavy workloads

**Solution**:
- Implement ReadIndex optimization
- Allow Followers to serve reads with bounded staleness
- Add read-only mode configuration

**Tasks**:
- [ ] Implement ReadIndex protocol
- [ ] Add lease-based read optimization
- [ ] Add configuration for read consistency level
- [ ] Implement stale read detection
- [ ] Add metrics for read latency by consistency level
- [ ] Allow Follower reads

---

### 8. Raft Performance Optimizations
**Status**: Not Started
**Priority**: Low
**Effort**: Large

**Optimizations**:
- [ ] Batch log entries
- [ ] Pipeline AppendEntries
- [ ] Implement pre-vote to reduce unnecessary elections
- [ ] Add leadership transfer for graceful shutdown
- [ ] Optimize heartbeat frequency based on cluster size
- [ ] Implement log entry compression
- [ ] Pre-Vote

---

### 9. Testing Infrastructure
**Status**: Basic
**Priority**: Medium
**Effort**: Large

**Current State**:
- Chaos monkey tests for ChunkServer failures
- Basic Master failure test

**Tasks**:
- [ ] Add unit tests for Raft logic
  - [ ] Leader election
  - [ ] Log replication
  - [ ] Vote counting
  - [ ] Term updates
- [ ] Add integration tests for network partitions
  - [ ] Multi-node scenarios
  - [ ] Network partition simulation
  - [ ] Clock skew simulation
- [ ] Add property-based tests (using proptest)
- [ ] Implement Jepsen-style tests
- [ ] Add performance benchmarks
- [ ] Add stress tests for high write throughput

---

### 10. Documentation
**Status**: **Mostly Completed**
**Priority**: Medium
**Effort**: Medium

**Docs**:
- ✅ README.md
- ✅ MASTER_HA.md
- ✅ REPLICATION.md
- ✅ CHAOS_TEST.md

---

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
- [ ] Authentication/Authorization

---

### 12. Observability
**Status**: Minimal
**Priority**: Medium
**Effort**: Medium

**Tasks**:
- [ ] Structured logging
- [ ] Implement distributed tracing
- [ ] Add request ID propagation
- [ ] Implement log aggregation
- [ ] Add alerting rules for:
  - [ ] Leader election failures
  - [ ] Log replication lag
  - [ ] Disk space for logs
  - [ ] Network partition detection
- [ ] Create operational dashboards
- [ ] Distributed tracing

---

### 13. ChunkServer Improvements
**Status**: Working
**Priority**: High
**Effort**: Medium

**Remaining Tasks**:
- [ ] Etcd-style Lease Check (GrantLease/KeepAlive RPCs)
- [ ] Rack Awareness

---

### 14. Rack Awareness
**Status**: Not Started
**Priority**: Low
**Effort**: Medium

**Problem**:
- Replicas might be placed on the same rack (SPOF)
- No awareness of network topology

**Solution**:
- Implement rack-aware replica placement policy
- Configurable topology script (like Hadoop)

**Tasks**:
- [ ] Add rack configuration to ChunkServer registration
- [ ] Implement topology mapping logic in Master
- [ ] Update block placement policy (1 local, 1 remote rack, 1 same remote rack)
- [ ] Add rack awareness to Balancer

---

### 15. Storage Efficiency (Erasure Coding)
**Status**: Not Started
**Priority**: Low
**Effort**: Large

**Problem**:
- 3x Replication consumes 300% storage overhead
- Cost inefficient for cold data

**Solution**:
- Implement Reed-Solomon Erasure Coding (e.g., 6+3 or 10+4)
- Reduce storage overhead to 1.5x or 1.4x while maintaining durability

**Tasks**:
- [ ] Research Rust Erasure Coding libraries (e.g., `reed-solomon-erasure`)
- [ ] Implement EC encoding/decoding logic in ChunkServer
- [ ] Update Master to handle EC block placement
- [ ] Implement background encoding for cold files
- [ ] Add reconstruction logic for failed EC blocks

---

## 🔧 Technical Debt

### 16. Code Quality
- [ ] Remove unused dependencies
- [ ] Add comprehensive error handling (remove unwrap() calls)
- [ ] Implement proper async error propagation
- [ ] Add type aliases for common types
- [ ] Refactor large functions into smaller units
- [ ] Add code comments for complex logic
- [ ] Run clippy and fix warnings
- [ ] Add rustfmt configuration and enforce formatting
- [ ] Fix deprecated `rand` usage in `simple_raft.rs`

### 17. Build and Deployment
- [ ] Optimize Docker image size
- [ ] Add CI/CD pipeline
- [ ] Implement blue-green deployment
- [ ] Add rolling update support
- [ ] Kubernetes manifests
- [ ] Add Helm chart
- [ ] Implement backup and restore procedures

### 18. Refactor RPC Responses
- [ ] Standardize RPC response formats (consistent success/error/hint fields)
- [ ] Use gRPC error details for structured error information instead of custom string parsing

---

## 📊 Metrics and Success Criteria

### Performance Targets
- [ ] Leader election completes in < 5 seconds
- [ ] Write latency < 100ms (p99)
- [ ] Read latency < 10ms (p99)
- [ ] Support 1000+ writes/second
- [ ] Support 10000+ reads/second

### Reliability Targets
- [ ] 99.9% uptime
- [ ] Zero data loss on single Master failure
- [ ] Automatic recovery from network partitions
- [ ] Support for rolling upgrades with zero downtime

---

## 🎯 Roadmap

### Phase 1: Stability (Completed)
- ✅ Basic Raft & HA

### Phase 2: Production Readiness (Completed)
- ✅ Persistence & Snapshots
- ✅ ChunkServer Liveness & Balancer

### Phase 3: Scalability (Current)
- ✅ **Master Server Sharding (Completed)**
- ✅ Client Library Refactoring (#2)
- Safe Mode (#4)
- Dynamic cluster membership (#5)
- Read optimizations (#7)

### Phase 4: Enterprise Features (Future)
- S3 Compatibility (#19)
- Security enhancements (#11)
- Observability (#12)
- Rack Awareness (#14)

---

## 📝 Notes

- Priority levels are subject to change based on user feedback
- Effort estimates are rough and may vary
- Some tasks can be parallelized
- Security features are marked low priority for prototype but would be critical for production

**Last Updated**: 2025-12-15
**Maintainer**: Development Team
