# Rust Hadoop DFS - TODO List

## 🔴 High Priority (Critical for Production)

### 1. CLI Leader Discovery & Retry Logic
**Status**: Not Started  
**Priority**: Critical  
**Effort**: Medium

**Problem**: 
- CLI connects to the first available Master but doesn't retry if that Master is not the Leader
- Users get "Not Leader" errors and operations fail

**Solution**:
```rust
// Implement retry logic in dfs_cli.rs
async fn execute_with_retry<T>(
    masters: &[String],
    operation: impl Fn(&mut MasterClient) -> Future<Output = Result<T>>
) -> Result<T> {
    for master in masters {
        match operation(master).await {
            Ok(result) => return Ok(result),
            Err(Status::Unavailable(msg)) if msg.contains("Not Leader") => continue,
            Err(e) => return Err(e),
        }
    }
    Err("No available leader found")
}
```

**Tasks**:
- [ ] Add retry logic for all write operations (create_file, allocate_block)
- [ ] Implement exponential backoff for retries
- [ ] Add timeout configuration
- [ ] Update CLI help text with retry behavior documentation

---

### 2. Leader Information Propagation
**Status**: Not Started  
**Priority**: High  
**Effort**: Small

**Problem**:
- Followers return "Not Leader" but don't tell clients who the Leader is
- Clients must try all Masters sequentially

**Solution**:
- Modify `CreateFileResponse` and `AllocateBlockResponse` to include optional `leader_hint` field
- Update proto definitions:
```protobuf
message CreateFileResponse {
    bool success = 1;
    string error_message = 2;
    string leader_hint = 3;  // Add this
}
```

**Tasks**:
- [ ] Update `proto/dfs.proto` with leader_hint fields
- [ ] Modify Master service to include current leader in error responses
- [ ] Update CLI to use leader_hint for faster failover
- [ ] Add leader discovery cache in CLI

---

### 3. Raft Log Persistence
**Status**: Not Started  
**Priority**: Critical  
**Effort**: Large

**Problem**:
- Raft logs are currently in-memory only
- If all Masters restart simultaneously, all metadata is lost
- No durability guarantee

**Solution**:
- Implement Write-Ahead Log (WAL) to disk
- Use `sled` or `rocksdb` for persistent storage
- Implement log compaction/snapshotting

**Tasks**:
- [ ] Add persistent storage backend (sled/rocksdb)
- [ ] Implement WAL for Raft logs
- [ ] Add log entry serialization/deserialization
- [ ] Implement snapshot creation on log size threshold
- [ ] Add snapshot restoration on startup
- [ ] Add configuration for log directory path
- [ ] Implement log compaction (remove applied entries)
- [ ] Add fsync configuration for durability vs performance tradeoff

**Files to modify**:
- `src/simple_raft.rs` - Add storage layer
- `src/bin/master.rs` - Add log directory argument
- `docker-compose.yml` - Add volume for log persistence

---

## 🟡 Medium Priority (Important for Stability)

### 4. Raft Snapshot Implementation
**Status**: Not Started  
**Priority**: Medium  
**Effort**: Large

**Problem**:
- Logs grow unbounded
- New nodes or restarted nodes must replay entire log
- Memory usage increases over time

**Solution**:
- Implement periodic snapshots of MasterState
- Transfer snapshots to new/lagging followers
- Truncate logs after successful snapshot

**Tasks**:
- [ ] Define snapshot format (JSON/Binary)
- [ ] Implement snapshot creation trigger (log size threshold)
- [ ] Add InstallSnapshot RPC handler
- [ ] Implement snapshot transfer mechanism
- [ ] Add snapshot restoration on startup
- [ ] Implement log truncation after snapshot
- [ ] Add snapshot compression (optional)

---

### 5. Improved Network Error Handling
**Status**: Partial  
**Priority**: Medium  
**Effort**: Medium

**Current Issues**:
- Network errors are silently ignored in some places
- No retry logic for transient failures
- No circuit breaker pattern

**Tasks**:
- [ ] Add structured error types for network failures
- [ ] Implement retry with exponential backoff for RPC calls
- [ ] Add circuit breaker for repeatedly failing nodes
- [ ] Implement request timeouts
- [ ] Add metrics for network error rates
- [ ] Log network errors with appropriate severity levels

---

### 6. Raft Configuration Management
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

### 7. Health Checks and Monitoring
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

### 8. Read Optimization
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

### 9. Raft Performance Optimizations
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

### 10. Testing Infrastructure
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

### 11. Documentation
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

### 12. Security Enhancements
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

### 13. Observability
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

### 14. ChunkServer Improvements
**Status**: Working  
**Priority**: Low  
**Effort**: Medium

**Potential Improvements**:
- [ ] Implement ChunkServer heartbeat to all Masters
- [ ] Add ChunkServer re-registration on Master failover
- [ ] Implement ChunkServer load balancing
- [ ] Add ChunkServer health scoring
- [ ] Implement automatic replica rebalancing

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

### 16. Build and Deployment
- [ ] Optimize Docker image size (multi-stage builds)
- [ ] Add CI/CD pipeline
- [ ] Implement blue-green deployment
- [ ] Add rolling update support
- [ ] Create Kubernetes manifests
- [ ] Add Helm chart
- [ ] Implement backup and restore procedures

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

### Phase 1: Stability (Current)
- ✅ Basic Raft implementation
- ✅ Leader election
- ✅ Log replication
- ✅ Basic chaos testing

### Phase 2: Production Readiness (Next 2-4 weeks)
- CLI retry logic (#1)
- Leader information propagation (#2)
- Raft log persistence (#3)
- Snapshot implementation (#4)
- Improved error handling (#5)

### Phase 3: Scalability (4-8 weeks)
- Dynamic cluster membership (#6)
- Read optimizations (#8)
- Performance optimizations (#9)
- Comprehensive testing (#10)

### Phase 4: Enterprise Features (8-12 weeks)
- Security enhancements (#12)
- Advanced observability (#13)
- Operational tooling
- Production documentation

---

## 📝 Notes

- Priority levels are subject to change based on user feedback
- Effort estimates are rough and may vary
- Some tasks can be parallelized
- Security features are marked low priority for prototype but would be critical for production

**Last Updated**: 2025-11-30
**Maintainer**: Development Team
