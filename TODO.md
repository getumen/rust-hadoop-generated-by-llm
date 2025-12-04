# Rust Hadoop DFS - TODO List

## 🔴 High Priority (Critical for Production)

### 1. Master Server Sharding
**Status**: Not Started
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
- [ ] Design sharding key strategy (e.g., hash of filename, directory subtrees)
- [ ] Implement Shard Coordinator (or configuration service) to manage shard mappings
- [ ] Update Client to cache shard mappings and route requests
- [ ] Deploy multiple Master Raft groups (Shards)
- [ ] Implement cross-shard operations (e.g., rename file across shards)

---

### 2. ChunkServer Liveness (Lease-based)
**Status**: Working
**Priority**: High
**Effort**: Medium

**Potential Improvements**:
- [x] Implement Lease-based Liveness Check (Heartbeat)
  - [x] Add `Heartbeat` RPC
  - [x] Implement Liveness manager in Master (15s timeout)
  - [x] Implement Heartbeat loop in ChunkServer (5s interval)
- [x] Implement ChunkServer heartbeat to all Masters
- [x] Add ChunkServer re-registration on Master failover (handled by heartbeat)
- [x] Implement ChunkServer load balancing (based on available space)
- [x] Add ChunkServer health scoring (basic stats collection)
- [x] Implement automatic replica rebalancing (Balancer)

---

## 🟡 Medium Priority (Important for Stability)

### 3. Safe Mode
**Status**: Not Started
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
- [ ] Implement Safe Mode state machine
- [ ] Add block reporting threshold logic (e.g., 99% of blocks reported)
- [ ] Block modification RPCs during Safe Mode
- [ ] Add CLI command to manually enter/leave Safe Mode
- [ ] Show Safe Mode status in web UI/metrics

---

### 4. Raft Configuration Management
**Status**: Not Started
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
- [ ] Design configuration change protocol
- [ ] Implement AddServer RPC
- [ ] Implement RemoveServer RPC
- [ ] Add configuration log entries to Raft log
- [ ] Implement joint consensus phase
- [ ] Add CLI commands for cluster management
- [ ] Add safety checks (prevent removing majority)

---

### 5. Health Checks and Monitoring
**Status**: Basic
**Priority**: Medium
**Effort**: Small

**Current State**:
- Basic Docker health checks exist
- No Raft-specific health metrics

**Tasks**:
- [ ] Add `/health` endpoint to HTTP server
- [ ] Expose Raft state (role, term, commit_index) via HTTP
- [ ] Add Prometheus metrics endpoint
- [ ] Implement metrics for:
  - [ ] Current role (Leader/Follower/Candidate)
  - [ ] Term number
  - [ ] Log size
  - [ ] Commit index
  - [ ] Last applied index
  - [ ] Number of votes received
  - [ ] Heartbeat latency
- [ ] Add Grafana dashboard template

---

## 🟢 Low Priority (Nice to Have)

### 6. Read Optimization
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

---

### 7. Raft Performance Optimizations
**Status**: Not Started
**Priority**: Low
**Effort**: Large

**Optimizations**:
- [ ] Batch log entries for replication
- [ ] Pipeline AppendEntries RPCs
- [ ] Implement pre-vote to reduce unnecessary elections
- [ ] Add leadership transfer for graceful shutdown
- [ ] Optimize heartbeat frequency based on cluster size
- [ ] Implement log entry compression

---

### 8. Testing Infrastructure
**Status**: Basic (chaos tests exist)
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
- [ ] Add integration tests
  - [ ] Multi-node scenarios
  - [ ] Network partition simulation
  - [ ] Clock skew simulation
- [ ] Add property-based tests (using proptest)
- [ ] Implement Jepsen-style consistency tests
- [ ] Add performance benchmarks
- [ ] Add stress tests for high write throughput

---

### 9. Documentation
**Status**: Partial
**Priority**: Medium
**Effort**: Medium

**Existing Docs**:
- ✅ MASTER_HA.md (basic HA documentation)
- ✅ CHAOS_TEST.md (chaos testing guide)

**Missing Docs**:
- [ ] Raft implementation details
- [ ] Operational runbook
  - [ ] How to add a Master node
  - [ ] How to remove a Master node
  - [ ] How to recover from split-brain
  - [ ] How to restore from backup
- [ ] Architecture decision records (ADRs)
- [ ] API documentation
- [ ] Troubleshooting guide
- [ ] Performance tuning guide

---

### 10. Security Enhancements
**Status**: Not Started
**Priority**: Low (for prototype)
**Effort**: Large

**Tasks**:
- [ ] Add TLS for Raft communication
- [ ] Implement authentication for Master-to-Master communication
- [ ] Add authorization for client requests
- [ ] Implement audit logging
- [ ] Add encryption at rest for logs
- [ ] Implement secure key rotation

---

### 11. Observability
**Status**: Minimal
**Priority**: Medium
**Effort**: Medium

**Tasks**:
- [ ] Add structured logging (using `tracing`)
- [ ] Implement distributed tracing
- [ ] Add request ID propagation
- [ ] Implement log aggregation
- [ ] Add alerting rules for:
  - [ ] Leader election failures
  - [ ] Log replication lag
  - [ ] Disk space for logs
  - [ ] Network partition detection
- [ ] Create operational dashboards

---

### 12. ChunkServer Improvements
**Status**: Working
**Priority**: High
**Effort**: Medium

**Potential Improvements**:
- [ ] Implement Lease-based Liveness Check (etcd-style)
  - [ ] Add `GrantLease`, `KeepAlive` RPCs
  - [ ] Implement Lease manager in Master
  - [ ] Implement KeepAlive loop in ChunkServer
- [ ] Implement ChunkServer heartbeat to all Masters
- [ ] Add ChunkServer re-registration on Master failover
- [ ] Implement ChunkServer load balancing
- [ ] Add ChunkServer health scoring
- [ ] Implement automatic replica rebalancing (Balancer)

---

### 13. Rack Awareness
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

### 14. Storage Efficiency (Erasure Coding)
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

### 15. Code Quality
- [ ] Remove unused dependencies (`fs2`, `raft_types.rs`, `raft_network.rs`)
- [ ] Add comprehensive error handling (remove unwrap() calls)
- [ ] Implement proper async error propagation
- [ ] Add type aliases for common types
- [ ] Refactor large functions into smaller units
- [ ] Add code comments for complex logic
- [ ] Run clippy and fix all warnings
- [ ] Add rustfmt configuration and enforce formatting
- [ ] Fix deprecated `rand` usage in `simple_raft.rs`

### 16. Build and Deployment
- [ ] Optimize Docker image size (multi-stage builds)
- [ ] Add CI/CD pipeline
- [ ] Implement blue-green deployment
- [ ] Add rolling update support
- [ ] Create Kubernetes manifests
- [ ] Add Helm chart
- [ ] Implement backup and restore procedures

### 17. Refactor RPC Responses
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
- ✅ Basic Raft implementation
- ✅ Leader election
- ✅ Log replication
- ✅ Basic chaos testing
- ✅ CLI retry logic
- ✅ Leader information propagation
- ✅ Raft log persistence

### Phase 2: Production Readiness (Current - Next 2-4 weeks)
- ✅ Snapshot implementation
- ✅ Improved error handling
- ✅ Data Integrity
- ChunkServer Liveness (Lease-based) (#2)
- Refactor RPC Responses (#16)

### Phase 3: Scalability (4-8 weeks)
- Master Server Sharding (#1)
- Safe Mode (#3)
- Dynamic cluster membership (#4)
- Read optimizations (#9)
- Performance optimizations (#10)
- Comprehensive testing (#6)
- Storage Efficiency (Erasure Coding) (#13)

### Phase 4: Enterprise Features (8-12 weeks)
- Security enhancements (#10)
- Advanced observability (#11)
- Rack Awareness (#13)
- Operational tooling
- Production documentation

---

## 📝 Notes

- Priority levels are subject to change based on user feedback
- Effort estimates are rough and may vary
- Some tasks can be parallelized
- Security features are marked low priority for prototype but would be critical for production

**Last Updated**: 2025-12-01
**Maintainer**: Development Team
