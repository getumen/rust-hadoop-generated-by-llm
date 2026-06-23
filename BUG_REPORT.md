# Bug Report: rust-hadoop-generated-by-llm

**Date**: 2026-06-23  
**Scope**: Full codebase audit — Raft consensus, 2PC transactions, pipeline replication, S3 API, sharding  

---

## Summary

| Severity | Count |
|----------|-------|
| CRITICAL | 12 |
| HIGH | 32 |
| MEDIUM | 26 |
| LOW | 8 |
| **Total** | **78** |

---

## CRITICAL Bugs

### BUG-001: `usize` underflow in `start_election()` when log is empty

**File**: `dfs/metaserver/src/simple_raft.rs:1296-1297`

```rust
let last_log_index = self.log.len() - 1 + self.last_included_index;
let last_log_term = self.log[self.log.len() - 1].term;
```

**Problem**: If `self.log` is empty, `self.log.len() - 1` underflows (`usize` wraparound). In debug builds this panics; in release builds it silently produces `usize::MAX`, corrupting the RequestVote RPC. The same pattern appears at the RequestVote handler (line ~1923).

**Impact**: Node crash during election → cluster unavailability. In release mode, silent corruption causes votes to be granted incorrectly → split-brain.

**Fix**: Guard with `if self.log.is_empty()` and use `last_included_index` / `last_included_term` as fallback.

---

### BUG-002: `usize` underflow in commit index advancement

**File**: `dfs/metaserver/src/simple_raft.rs:2229`

```rust
let absolute_index = self.log.len() - 1 + self.last_included_index;
server_match_indices.insert(self.id, absolute_index);
```

**Problem**: Same `self.log.len() - 1` underflow when log is empty. The leader inserts `usize::MAX` as its own match index, which causes the majority match index calculation to advance `commit_index` to an astronomically large value.

**Impact**: Silent state machine corruption — `commit_index` jumps to invalid value, `apply_logs()` loops trying to apply nonexistent entries. Data loss or infinite loop.

---

### BUG-003: Snapshot log truncation off-by-one

**File**: `dfs/metaserver/src/simple_raft.rs:1043-1044, 1081-1082`

```rust
// Line 1044: term lookup
let index = self.last_applied - self.last_included_index;
self.log.get(index)  // should be index-1 if log[0] is dummy NoOp

// Line 1081-1082: truncation
let new_log_start = self.last_applied - self.last_included_index + 1;
let new_log = self.log[new_log_start..].to_vec();
```

After `create_snapshot` (line 1083-1086), a dummy `NoOp` entry is prepended at `self.log[0]`. But `apply_logs()` at line 2433 calculates `log_index = last_applied - last_included_index` and accesses `self.log[log_index]` — this is offset by 1 from the actual entry because the dummy NoOp occupies index 0. The inconsistency means entries are applied from the wrong log position after a snapshot.

**Impact**: Wrong commands applied to state machine after snapshot → silent data corruption, file metadata mismatch.

---

### BUG-004: Pipeline replication returns `success: true` on downstream failure

**File**: `dfs/chunkserver/src/chunkserver.rs:797-825`

```rust
match client.replicate_block(replicate_req).await {
    Ok(resp) => {
        let inner = resp.into_inner();
        if inner.success {
            replicas_written += inner.replicas_written;
        } else {
            tracing::error!("Downstream replication failed ...");
            // No action — falls through
        }
    }
    Err(e) => {
        tracing::error!("Failed to replicate to {}: {}", next_server, e);
        // No action — falls through
    }
}

// Always returns success regardless of replication outcome
Ok(Response::new(WriteBlockResponse {
    success: true,
    error_message: "".to_string(),
    replicas_written,
}))
```

**Problem**: `success: true` is returned even when downstream replication completely fails. The client sees `replicas_written: 1` (local only) but believes the write succeeded.

**Impact**: Client assumes data is durable. If the single replica's ChunkServer dies, data is permanently lost despite client receiving success.

---

### BUG-005: Epoch fencing bypassed when `master_term == 0`

**File**: `dfs/chunkserver/src/chunkserver.rs:733-743`

```rust
let req_term = req.master_term;
let known = self.known_term.load(Ordering::Relaxed);
if req_term > 0 && req_term < known {
    return Err(Status::failed_precondition(...));
}
```

**Problem**: The condition `req_term > 0` means any request with `master_term = 0` bypasses the fencing check entirely. A stale or rogue leader that doesn't set `master_term` can write data unchecked. Additionally, `Ordering::Relaxed` provides no cross-thread visibility guarantee — a concurrent read may not see the latest term update.

**Impact**: Split-brain data corruption. A deposed leader with `master_term = 0` can continue writing blocks, causing divergent replicas.

**Fix**: Reject `master_term == 0` (require all writes to carry a valid term). Use `Ordering::SeqCst` or `Ordering::AcqRel`.

---

### BUG-006: 2PC `commit_transaction` ignores Raft state update failures

**File**: `dfs/metaserver/src/master.rs` (commit_transaction handler)

```rust
// Update state to Committed — errors silently ignored
let (tx, rx) = tokio::sync::oneshot::channel();
let _ = self.raft_tx.send(Event::ClientRequest {
    command: Command::Master(MasterCommand::UpdateTransactionState {
        tx_id: tx_id.clone(),
        new_state: TxState::Committed,
    }),
    reply_tx: tx,
}).await;
let _ = rx.await;  // Result discarded

Ok(Response::new(CommitTransactionResponse {
    success: true,  // Always returns success
    ...
}))
```

**Problem**: After applying the transaction operation, the state update to `Committed` is fire-and-forget (`let _ =`). If the Raft channel is full or the node loses leadership, the state remains `Prepared`. On coordinator recovery, `InquireTransaction` returns `Prepared`, and the recovery logic may abort a transaction that was actually applied.

**Impact**: Committed operations silently rolled back on recovery → data loss and violated atomicity.

---

## HIGH Bugs

### BUG-007: Shard split deletes files before confirming ingestion

**File**: `dfs/metaserver/src/simple_raft.rs:3148-3167`

```rust
MasterCommand::SplitShard { split_key, new_shard_id, .. } => {
    let to_move: Vec<String> = master_state.files.keys()
        .filter(|k| **k >= *split_key)
        .cloned()
        .collect();
    for path in to_move {
        master_state.files.remove(&path);  // Deleted immediately
    }
}
```

**Problem**: Files are removed from the source shard's state machine as soon as the `SplitShard` command is applied. There is no confirmation that the new shard has received these files via `IngestBatch`. If the new shard crashes before ingestion, the files are permanently lost from both shards.

**Impact**: Permanent metadata loss during shard splits.

---

### BUG-008: No transaction state machine validation

**File**: `dfs/metaserver/src/simple_raft.rs:3089-3098`

```rust
MasterCommand::UpdateTransactionState { tx_id, new_state } => {
    if let Some(record) = master_state.transaction_records.get_mut(tx_id) {
        record.state = new_state.clone();  // Any transition allowed
    }
}
```

**Problem**: No validation of state transitions. `Committed → Prepared`, `Aborted → Committed`, or any other illegal transition is silently accepted.

**Impact**: Corrupted transaction records. A bug or message reordering could revert a committed transaction to `Prepared` state.

---

### BUG-009: Cross-shard abort failures silently ignored

**File**: `dfs/metaserver/src/master.rs` (cross-shard rename logic)

```rust
let _ = self.send_abort_to_dest_shard(&tx_id, &dest_peers).await;
```

**Problem**: When `PrepareTransaction` fails on one shard, the coordinator sends `Abort` to the other — but ignores the result. If the abort RPC fails (network partition), the participant remains in `Prepared` state indefinitely with resources locked.

**Impact**: Orphaned locks preventing future operations on affected files. No timeout-based cleanup mechanism exists.

---

### BUG-010: TOCTOU race in `commit_transaction` operation application

**File**: `dfs/metaserver/src/master.rs` (commit_transaction handler)

```rust
// Step 1: Read operation under lock
let operation = {
    let state_lock = self.state.lock().expect("Mutex poisoned");
    // ... read transaction operation ...
};  // Lock released

// Step 2: Apply operation without lock (via Raft)
if let Some(op) = operation {
    self.raft_tx.send(Event::ClientRequest { ... }).await;
}
```

**Problem**: The lock is released between reading the transaction's operation and applying it. Another request could modify or abort the transaction in between.

**Impact**: Operation applied to an aborted transaction, or double-application if concurrent commits arrive.

---

### BUG-011: ShardMap refresh is async and fire-and-forget

**File**: `dfs/client/src/mod.rs:1452-1455`

```rust
let self_clone = self.clone();
tokio::spawn(async move {
    let _ = self_clone.refresh_shard_map().await;
});
```

**Problem**: After detecting a stale shard map (e.g., `NOT_FOUND` from wrong shard), the client refreshes asynchronously but immediately retries with the old map. The retry hits the same wrong shard.

**Impact**: Cascading failures during shard topology changes. Client requests fail repeatedly until the background refresh eventually completes.

---

### BUG-012: S3 SigV4 presigned request payload hash bypass

**File**: `dfs/s3_server/src/auth_middleware.rs:240-249`

```rust
if input.payload_hash == "UNSIGNED-PAYLOAD" && !state.allow_unsigned_payload && !is_presigned {
    return auth_failure(...);
}
```

**Problem**: The `!is_presigned` condition exempts all presigned requests from the unsigned-payload check. An attacker can take a legitimate presigned URL and modify the request body — the payload hash won't be verified.

**Impact**: Presigned URL payload tampering. Attackers can upload arbitrary data using a legitimate presigned PUT URL with a modified body.

---

### BUG-013: S3 `CopyObject` source path traversal

**File**: `dfs/s3_server/src/handlers.rs:495-508`

```rust
let decoded_source = percent_decode_str(source).decode_utf8_lossy().to_string();
let source_path = if decoded_source.starts_with('/') {
    decoded_source
} else {
    format!("/{}", decoded_source)
};
```

**Problem**: The `x-amz-copy-source` header value is percent-decoded but not sanitized for path traversal sequences (`../`). A crafted header like `x-amz-copy-source: /../../../etc/passwd` could read files outside the bucket namespace.

**Impact**: Unauthorized file access across bucket boundaries.

---

### BUG-014: Bucket policy `Principal: "*"` matches unauthenticated requests

**File**: `dfs/common/src/auth/bucket_policy.rs:83-92`

```rust
pub fn matches(&self, principal_arn: Option<&str>) -> bool {
    match self {
        PrincipalValue::Wildcard => true,  // Always matches, even None
        PrincipalValue::Aws(AwsPrincipal::Single(s)) => {
            if s == "*" { return true; }
            // ...
        }
    }
}
```

**Problem**: `Wildcard` returns `true` regardless of whether `principal_arn` is `Some` or `None`. A bucket policy with `"Principal": "*"` and `"Effect": "Allow"` grants access to completely unauthenticated requests (no credentials at all).

**Impact**: Privilege escalation — unauthenticated users gain access to buckets with wildcard policies.

---

## MEDIUM Bugs

### BUG-015: S3 range request panic on empty files

**File**: `dfs/s3_server/src/handlers.rs:1145-1163`

```rust
let end = parts[1].parse::<u64>().unwrap_or(total_size - 1);
```

**Problem**: When `total_size == 0` (empty file), `total_size - 1` underflows to `u64::MAX`. The subsequent slice operation `&body_bytes[start as usize..=end as usize]` panics with index out of bounds.

**Impact**: DoS — any range request on an empty S3 object crashes the S3 server process.

---

### BUG-016: Multipart upload part number not validated

**File**: `dfs/s3_server/src/handlers.rs:255-289`

```rust
async fn upload_part(..., part_number: i32, ...) {
    let part_path = format!("/.s3_mpu/{}/{}", upload_id, part_number);
```

**Problem**: No validation that `part_number` is in the S3-required range (1–10000). Negative numbers, zero, or huge values are accepted, creating arbitrary paths like `/.s3_mpu/id/-1`.

**Impact**: Resource exhaustion or path manipulation via crafted part numbers.

---

### BUG-017: `CompleteMultipartUpload` ignores request body

**File**: `dfs/s3_server/src/handlers.rs:322-363`

```rust
async fn complete_multipart_upload(..., _body: Bytes) -> Response {
```

**Problem**: The XML body (containing part ETags and numbers for validation) is completely ignored (`_body`). The implementation just lists files in the upload directory without validating that the parts match what the client specified.

**Impact**: Data integrity violation — parts can be missing, out of order, or corrupted without detection.

---

### BUG-018: S3 query string normalization incompatible with AWS SigV4

**File**: `dfs/s3_server/src/auth_middleware.rs:561-584`

**Problem**: Duplicate query parameters (e.g., `a=1&a=2`) are kept as separate entries. AWS SigV4 spec requires they be sorted by key then value. While the sorting is correct, the encoding step may differ from AWS's canonical encoding rules, causing signature mismatches for legitimate AWS SDK clients.

**Impact**: Legitimate clients using standard AWS SDKs may fail authentication.

---

### BUG-019: Shard merge removes config before file migration

**File**: `dfs/common/src/sharding.rs:210-245`

```rust
pub fn merge_shards(&mut self, victim_shard_id: &ShardId, retained_shard_id: &ShardId) -> bool {
    // ... updates range boundaries ...
    self.shards.remove(victim_shard_id);
    self.shard_peers.remove(victim_shard_id);
    return true;  // No file migration verification
}
```

**Problem**: The shard map is updated and broadcast to clients before file metadata is physically migrated from victim to retained shard. During this window, clients route to the retained shard which doesn't yet have the victim's files.

**Impact**: Transient file unavailability during merge operations. Reads return `NOT_FOUND` for files in transit.

---

### BUG-020: Non-voting member index tracking uses wrong array indices

**File**: `dfs/metaserver/src/simple_raft.rs` (send_heartbeats, combined_peers iteration)

**Problem**: For non-voting members in joint consensus, `peer_idx` from the `combined_peers` enumeration is used directly to index into `self.next_index` and `self.match_index`. But `combined_peers` includes both voting and non-voting members with different enumeration than `self.peers`. This causes non-voting members to receive entries meant for different peers.

**Impact**: Incorrect log replication to non-voting members during membership changes, potentially preventing them from catching up.

---

---

## Additional Findings (Round 2)

### HIGH Bugs (continued)

### BUG-021: Joint consensus C-new entry appended every tick (unbounded log growth)

**File**: `dfs/metaserver/src/simple_raft.rs` (`check_finalize_joint_consensus`)

**Problem**: After appending a `FinalizeConfiguration` (C-new) entry to the log, `config_change_state` is NOT updated. Every leader tick re-evaluates the same condition (`commit_index >= joint_config_index`) and appends another identical C-new entry. This causes unbounded log growth until a snapshot truncates it.

**Impact**: Log grows without bound during membership changes. Disk exhaustion, replication lag, snapshot failures.

---

### BUG-022: Non-atomic config persistence (crash between two RocksDB puts)

**File**: `dfs/metaserver/src/simple_raft.rs:954-967`

```rust
fn save_config(&self) -> Result<()> {
    let serialized = serde_json::to_vec(&self.cluster_config)?;
    self.db.put(b"cluster_config", serialized)?;       // Put 1
    let state_serialized = serde_json::to_vec(&self.config_change_state)?;
    self.db.put(b"config_change_state", state_serialized)?;  // Put 2
    Ok(())
}
```

**Problem**: Two separate RocksDB `put` calls instead of a `WriteBatch`. If the process crashes between them, `cluster_config` is updated but `config_change_state` is stale. After recovery, the node may be in Joint config with `config_change_state: None`, breaking membership change logic.

**Impact**: Permanent inconsistent state after crash during config change → cluster stuck in joint consensus.

---

### BUG-023: SSE encryption missing AAD (ciphertext not bound to object identity)

**File**: `dfs/common/src/auth/sse.rs:33`

```rust
// TODO: add per-object AAD (object path) to bind ciphertext to identity
let data_ciphertext = data_cipher
    .encrypt(data_nonce, plaintext)  // No AAD!
    .map_err(|e| ...)?;
```

**Problem**: AES-256-GCM encryption does not use Associated Authenticated Data (AAD). The ciphertext is not bound to the object's path or bucket. An attacker with storage access can **swap encrypted objects between paths** and decryption will succeed.

**Impact**: Cross-object ciphertext substitution attack — users can read another object's data.

---

### MEDIUM Bugs (continued)

### BUG-024: STS session duration unbounded

**File**: `dfs/s3_server/src/sts_handler.rs:276`

```rust
let duration = params.duration_seconds.unwrap_or(3600);  // No max check
let expiration_ts = now + duration;
```

**Problem**: No upper limit on `DurationSeconds`. An attacker can request sessions lasting billions of seconds. AWS limits this to 43200 seconds (12 hours).

**Impact**: Compromised temporary credentials remain valid indefinitely.

---

### BUG-025: Config server `FetchShardMap` returns no version number

**File**: `dfs/metaserver/src/config_server.rs:43-61`

**Problem**: `FetchShardMapResponse` contains no version or epoch field. Clients cannot detect whether their cached map is stale, and cannot refuse an older map delivered by a lagging config server replica.

**Impact**: During shard splits, clients may route to wrong shards due to stale-but-undetectable maps.

---

### BUG-026: TLS hostname fallback to "localhost"

**File**: `dfs/common/src/security.rs:88-98`

```rust
.unwrap_or("localhost")  // Fallback if URL parsing fails
```

**Problem**: If domain extraction from the URL fails, hostname verification uses `"localhost"`. A certificate for `localhost` would pass verification for any server.

**Impact**: Potential MITM attack when connecting to servers via IP address with TLS.

---

### BUG-027: `config_change_state` not persisted after `promote_non_voting_members`

**File**: `dfs/metaserver/src/simple_raft.rs` (promote_non_voting_members)

```rust
self.config_change_state = ConfigChangeState::InJointConsensus {
    joint_config_index: joint_index,
    target_config: new_members,
};
self.non_voting_members.clear();
// Missing: self.save_config()?;
```

**Problem**: `config_change_state` is updated in memory but not persisted to RocksDB. A crash immediately after loses the state. After recovery, the node doesn't know it's in joint consensus.

**Impact**: Membership change stuck or restarted from scratch after crash.

---

### BUG-028: `BeginJointConsensus` application doesn't update `config_change_state`

**File**: `dfs/metaserver/src/simple_raft.rs` (apply_membership_command)

**Problem**: When `BeginJointConsensus` is applied, `cluster_config` transitions to `Joint` but `config_change_state` is not updated to `InJointConsensus`. Lease-based read checks and finalization logic that inspect `config_change_state` see a stale value.

**Impact**: Incorrect lease behavior during membership changes; may allow stale reads.

---

### BUG-029: Pervasive `let _ =` pattern on Raft commands in background tasks

**Files**: `dfs/metaserver/src/master.rs` (scan_cold_move, scan_ec_conversion, run_data_shuffler, cross-shard rename steps 6-7)

**Problem**: At least 8 distinct locations silently discard both Raft send errors and response errors via `let _ =`. These span: tiering (cold move, EC conversion), shard shuffling (StopShuffle), and 2PC coordination (UpdateTransactionState, SetParticipantAcked).

**Impact**: Background operations silently fail. Metadata never transitions, files never tier, transactions never finalize. No alerts, no retries.

---

### BUG-030: Healer schedules replication to servers removed by concurrent liveness check

**File**: `dfs/metaserver/src/master.rs` (heal_under_replicated_blocks + liveness checker)

**Problem**: The healer iterates `state.files` and selects target ChunkServers for replication. Concurrently, the liveness checker may remove a dead ChunkServer from `chunk_servers`. The healer then queues a replication command to a server that no longer exists. The command sits in `pending_commands` permanently.

**Impact**: Leaked pending commands; blocks remain under-replicated despite healer running.

---

### LOW Bugs

### BUG-031: Linearizability checker off-by-one in read-write overlap

**File**: `dfs/client/src/checker.rs:345-363`

**Problem**: The overwrite check uses `writes[i+1].ts <= invoke` (before read **invokes**) instead of `writes[i+1].ts <= ret` (before read **returns**). This is too strict — a write between invoke and return should invalidate the old value but doesn't.

**Impact**: False negatives in linearizability testing (valid histories rejected as non-linearizable). No production impact.

---

### BUG-032: Erasure coding accepts invalid shard counts

**File**: `dfs/common/src/erasure.rs:7-23`

**Problem**: No check that `data_shards + parity_shards <= 255` (Reed-Solomon's GF(2^8) limit). The error from `ReedSolomon::new()` is generic and unhelpful.

**Impact**: Confusing error messages when misconfigured. No data corruption.

---

---

## Additional Findings (Round 3)

### CRITICAL Bugs (continued)

### BUG-033: `install_snapshot` corrupts state on deserialization failure

**File**: `dfs/metaserver/src/simple_raft.rs:1099-1158`

```rust
// Tries AppState, then MasterState — if BOTH fail, only logs error
Err(e) => tracing::error!("Failed to deserialize snapshot data: {}", e),

// But then UNCONDITIONALLY updates indices:
self.last_included_index = snapshot.last_included_index;
self.last_included_term = snapshot.last_included_term;
self.log = vec![LogEntry { term: self.last_included_term, command: Command::NoOp }];
self.commit_index = snapshot.last_included_index;
self.last_applied = snapshot.last_included_index;
```

**Problem**: If snapshot data is corrupted and deserialization fails, the log indices and commit_index are still advanced. The app_state is stale but `last_applied` claims it's up to date. On crash recovery, the node has a gap between persisted state and logical position.

**Impact**: Permanent state machine divergence after receiving corrupted snapshot. Node silently serves stale data.

---

### BUG-034: Credentials leaked in plaintext audit logs

**File**: `dfs/s3_server/src/auth_middleware.rs:346, 512`

**Problem**: Audit log receives `query_params` directly, which for presigned requests contains `X-Amz-Credential`, `X-Amz-Security-Token`, and other authentication material. Only `X-Amz-Signature` is filtered out (line 565). Audit logs written to `/tmp/s3_audit_log` with no restricted file permissions.

**Impact**: Any local user can read the audit log and extract valid AWS credentials. Critical credential leakage.

---

### BUG-035: STS signing key accepts arbitrarily short secrets

**File**: `dfs/s3_server/src/main.rs:154-164`

```rust
let mut key = [0u8; 32];
let key_bytes = key_str.as_bytes();
let len = key_bytes.len().min(32);
key[..len].copy_from_slice(&key_bytes[..len]);
```

**Problem**: A 1-byte `STS_SIGNING_KEY` env var produces a 32-byte key with 31 null bytes. This reduces effective entropy to 8 bits, making STS tokens trivially forgeable via brute force. No minimum length validation.

**Impact**: Token forgery → full authentication bypass.

---

### HIGH Bugs (continued)

### BUG-036: ListObjectsV2 continuation token is unauthenticated path traversal vector

**File**: `dfs/s3_server/src/handlers.rs:1549-1564`

```rust
let marker = params.start_after.clone()
    .or(params.continuation_token.clone())
    .unwrap_or_default();
if !marker.is_empty() {
    let marker_path = format!("/{}/{}", bucket, marker);
    if let Some(idx) = files.iter().position(|f| *f > marker_path) {
        start_index = idx;
    }
}
```

**Problem**: Continuation token is treated as a raw path component, not an opaque token. A crafted `continuation-token=/../other-bucket/secret` can enumerate files across bucket boundaries.

**Impact**: Cross-bucket file enumeration.

---

### BUG-037: `max_keys` accepts negative values → empty responses

**File**: `dfs/s3_server/src/handlers.rs:39, 1570, 1576`

```rust
pub max_keys: Option<i32>,  // Can be negative
let max_keys = params.max_keys.unwrap_or(1000);
if key_count >= max_keys { ... }  // key_count (0) >= max_keys (-1) → true immediately
```

**Problem**: No validation that `max_keys > 0`. Negative values cause the loop to exit immediately, returning 0 objects regardless of bucket contents.

**Impact**: DoS — clients receive empty listings for valid buckets.

---

### BUG-038: WriteBatch not atomic with in-memory log state

**File**: `dfs/metaserver/src/simple_raft.rs:938-952` (save_log_entries_batch) + callers

**Problem**: The AppendEntries handler pushes entries to `self.log` (in-memory) BEFORE calling `save_log_entries_batch()` (RocksDB). If the process crashes after the in-memory push but before the DB write, crash recovery loads from DB without those entries — but other in-memory state (like `commit_index`) may reference them.

**Impact**: Log entries lost on crash, commit_index points to nonexistent entries → state machine corruption on recovery.

---

### BUG-039: Background tasks silently die on panic (no JoinHandle tracking)

**File**: `dfs/metaserver/src/master.rs:1940-1974`

```rust
tokio::spawn(run_liveness_checker(...));
tokio::spawn(run_periodic_healer(...));
tokio::spawn(run_block_balancer(...));
tokio::spawn(run_transaction_cleanup(...));
tokio::spawn(run_transaction_recovery(...));
// ... 5 more background tasks
```

**Problem**: 10 critical background tasks are spawned without capturing `JoinHandle`. If any panics (e.g., from `.unwrap()` in hot path), the panic is silently swallowed. No monitoring, no restart, no alerting.

**Impact**: Silent degradation — healer stops, dead servers aren't detected, transactions aren't cleaned up. System appears healthy but gradually fails.

---

### BUG-040: `Ordering::Relaxed` used for Raft term in master.rs

**File**: `dfs/metaserver/src/master.rs:733, 2529, 2765`

```rust
current_term.store(info.current_term, Ordering::Relaxed);
current_term.load(Ordering::Relaxed);
```

**Problem**: The Raft current_term is shared across threads (main Raft loop, RPC handlers, background tasks) using `Relaxed` ordering. Thread B may read a stale term after Thread A updates it. Combined with BUG-005 (chunkserver also uses Relaxed for `known_term`), the entire term-based fencing system has weak memory ordering guarantees.

**Impact**: Term-based safety properties (leader uniqueness, fencing) may be violated on ARM/weak-memory architectures.

---

### BUG-041: HeadObject returns `Content-Length: 0` for multipart objects

**File**: `dfs/s3_server/src/handlers.rs:1393-1450`

```rust
.header(CONTENT_LENGTH, "0")  // Always 0 for MPU objects
```

**Problem**: `HeadObject` and `GetObject` return different `Content-Length` for multipart-uploaded objects. `HeadObject` always says 0 bytes; `GetObject` returns actual data.

**Impact**: Clients using `HEAD` to check object size before `GET` (e.g., for range downloads) will believe the object is empty. Breaks S3 SDK download managers.

---

### BUG-042: TOCTOU races in all file operation RPCs

**Files**: `dfs/metaserver/src/master.rs` (create_file, delete_file, allocate_block, rename)

**Problem**: Every file operation follows the same pattern:
1. Lock state mutex, check preconditions (file exists / doesn't exist)
2. Release lock
3. Send command to Raft
4. Between steps 2-3, another request can change the precondition

This affects: `create_file` (duplicate creation), `delete_file` (double delete), `allocate_block` (blocks allocated for deleted file), `rename` (source deleted before rename applies).

**Impact**: While Raft application should detect conflicts, the RPC response may be incorrect (success returned when command will fail at application time, or vice versa).

---

### BUG-043: Safe mode block count double-counted on ChunkServer restart

**File**: `dfs/metaserver/src/master.rs:2732-2735`

```rust
let is_new_chunkserver = !state.chunk_servers.contains_key(&req.chunk_server_address);
if state.is_in_safe_mode() && is_new_chunkserver {
    state.update_reported_blocks(req.chunk_count as usize);
}
```

**Problem**: If a ChunkServer restarts (same address), it's not in `chunk_servers` anymore (was removed by liveness checker), so `is_new_chunkserver = true`. Its block count is added again, double-counting. This inflates `reported_block_count`, causing premature safe mode exit.

**Impact**: Safe mode exits before all blocks are verified → writes allowed before data integrity confirmed.

---

### MEDIUM Bugs (continued)

### BUG-044: Partial snapshot recovery silently accepted

**File**: `dfs/metaserver/src/simple_raft.rs:835-871`

**Problem**: During crash recovery, if `snapshot_meta` exists in RocksDB but `snapshot_data` is missing or corrupted, the meta (including `last_included_index`, `last_included_term`) is still returned and used. The app_state is never restored but log recovery starts from `last_included_index + 1`. If those log entries were compacted away, there's a gap.

**Impact**: Corrupted state after crash — node has no state machine data but thinks it's at a valid index.

---

### BUG-045: `list_files` returns unbounded results (no pagination)

**File**: `dfs/metaserver/src/master.rs` (list_files handler)

```rust
let files: Vec<String> = state.files.keys()
    .filter(|k| k.starts_with(&prefix))
    .cloned()
    .collect();  // ALL matching files
```

**Problem**: No limit on result size. A directory with millions of files causes OOM or gRPC message size exceeded.

**Impact**: DoS via listing large directories. OOM crash on master server.

---

### BUG-046: Safe mode timeout underflows on clock skew

**File**: `dfs/metaserver/src/master.rs` (should_exit_safe_mode)

```rust
if now - self.safe_mode_entered_at > 60_000 {  // unsigned subtraction
```

**Problem**: If system clock jumps backward (NTP correction), `now < safe_mode_entered_at`, and unsigned subtraction wraps to `u64::MAX`. The condition becomes true, exiting safe mode prematurely.

**Impact**: Premature safe mode exit on clock skew → data corruption risk.

---

### BUG-047: DeleteObject silently fails for multipart upload directories

**File**: `dfs/s3_server/src/handlers.rs:1365-1391`

**Problem**: `delete_file(&path)` result is captured but never checked. If the object was created via multipart upload (stored as directory with parts), the direct delete may fail (it's a directory). Part files are cleaned up in a loop, but the result of the loop deletions is also ignored.

**Impact**: Objects appear deleted but persist on storage, leaking disk space.

---

### BUG-048: EC parameters from client not validated

**File**: `dfs/metaserver/src/master.rs` (create_file handler)

```rust
command: Command::Master(MasterCommand::CreateFile {
    ec_data_shards: req.ec_data_shards,    // No validation
    ec_parity_shards: req.ec_parity_shards, // No validation
}),
```

**Problem**: Client-supplied EC shard counts (e.g., `ec_data_shards=0, ec_parity_shards=300`) are passed directly to Raft without validation. Invalid combinations are persisted in metadata.

**Impact**: Files with impossible EC policies — encoding/decoding panics or produces garbage.

---

### LOW Bugs (continued)

### BUG-049: Signature debug log exposes canonical request at WARN level

**File**: `dfs/s3_server/src/auth_middleware.rs:355-359`

```rust
tracing::warn!("Signature mismatch. CR: {}, S2S: {}", canonical_request, string_to_sign);
```

**Problem**: On signature mismatch, the full canonical request (including headers and payload hash) is logged at WARN level. In production, this reveals request details to anyone with log access.

**Impact**: Information leakage in production logs.

---

### BUG-050: NamedTempFile::new().unwrap() panics if /tmp is full

**File**: `dfs/s3_server/src/handlers.rs` (multiple locations: initiate_multipart_upload, upload_part, put_object)

**Problem**: Multiple `.unwrap()` calls on `NamedTempFile::new()`. If `/tmp` is full or the filesystem is read-only, the S3 server panics and crashes.

**Impact**: DoS — filling `/tmp` crashes the S3 server.

---

## Additional Findings (Round 4)

### HIGH Bugs (continued)

### BUG-051: `delete_log_entries_from` leaves orphaned entries on index gaps

**File**: `dfs/metaserver/src/simple_raft.rs:1013-1031`

```rust
fn delete_log_entries_from(&self, start_index: usize) -> Result<()> {
    let mut index = start_index;
    loop {
        let key = format!("log:{}", index);
        if self.db.get(key.as_bytes())?.is_none() {
            break;  // Stops at first missing key
        }
        self.db.delete(key.as_bytes())?;
        index += 1;
    }
}
```

**Problem**: Sequential point lookups stop at the first missing key. If log indices have a gap (entries at 1,2,3,5 — gap at 4 due to partial crash), deletion stops at 4, leaving entry 5 orphaned in RocksDB. If the gap is later filled, the stale orphaned entry at index 5 could conflict with a new entry from a different term.

**Impact**: Stale log entries persist across restarts, potentially resurrecting old commands.

---

### BUG-052: Graceful shutdown doesn't flush Raft state or drain RPCs

**File**: `dfs/metaserver/src/bin/master.rs:290-299`

```rust
tokio::signal::ctrl_c().await.ok();
cancel.cancel();
tokio::time::sleep(std::time::Duration::from_secs(3)).await;
```

**Problem**: Only handles SIGINT (not SIGTERM). No Raft state flush to RocksDB. No gRPC connection draining. Docker's `docker stop` sends SIGTERM first, which is completely unhandled.

**Impact**: Raft state loss on `docker stop`. In-flight transactions half-written.

---

### MEDIUM Bugs (continued)

### BUG-053: Snapshot upload creates new HTTP client per invocation

**File**: `dfs/metaserver/src/simple_raft.rs:1245-1250`

**Problem**: `reqwest::Client::new()` called per snapshot instead of reusing `self.http_client`. Creates fresh TLS state and connection pool each time.

**Impact**: Connection pool exhaustion under heavy write load.

---

### BUG-054: Election retries lack jitter (thundering herd)

**File**: `dfs/metaserver/src/simple_raft.rs:1319-1362`

**Problem**: Exponential backoff (50/100/200ms) without jitter. After partition heals, all nodes retry with identical timing → repeated vote splits.

**Impact**: Extended leader election time after network recovery.

---

### BUG-055: Metrics endpoint exposes Raft internals without auth

**File**: `dfs/metaserver/src/bin/master.rs:174, 325-395`

**Problem**: `/metrics` exposes `raft_current_term`, `raft_commit_index`, `raft_log_len`, safe mode status without authentication.

**Impact**: Information disclosure — attacker learns cluster state and term numbers.

---

### LOW Bugs (continued)

---

## Additional Findings (Round 5)

### CRITICAL Bugs (continued)

### BUG-058: `get_shard()` off-by-one at range boundaries

**File**: `dfs/common/src/sharding.rs:171-174`

```rust
let entry = ranges.range(key.to_string()..).next();
entry.map(|(_, shard_id)| shard_id.clone())
```

**Problem**: `ranges` maps exclusive upper bounds to shard IDs. Entry `("/m", "s1")` means shard s1 covers keys `[start, "/m")`. The query `ranges.range(key..)` finds the first entry with key `>=` the query. When `key == "/m"`, it returns `("/m", "s1")`, mapping "/m" to s1. But "/m" is the **exclusive** upper bound — it should map to the **next** shard. The correct query is `ranges.range((Bound::Excluded(key), Bound::Unbounded))`.

**Impact**: Files at exact shard boundaries are routed to the wrong shard. Creates/reads/deletes go to the wrong master, causing phantom files and data loss.

---

### HIGH Bugs (continued)

### BUG-059: PutObject path traversal via unsanitized key

**File**: `dfs/s3_server/src/handlers.rs` (put_object)

```rust
let dest_path = format!("/{}/{}", bucket, key);  // No path validation
```

**Problem**: Object keys like `../../../other-bucket/secret` or keys with `\0` are not sanitized. Combined with BUG-013 (CopyObject same issue), this means both read and write paths allow cross-bucket access.

**Impact**: Arbitrary file write outside bucket namespace.

---

### BUG-060: S3 XML error responses don't escape special characters

**File**: `dfs/s3_server/src/handlers.rs` (multiple error response functions)

```rust
format!("<Error><Code>NoSuchBucketPolicy</Code><BucketName>{bucket}</BucketName></Error>")
```

**Problem**: Bucket names and keys containing `<`, `>`, `&`, `"`, `'` are interpolated directly into XML without escaping. A bucket named `my<script>` produces malformed XML.

**Impact**: XML injection. Clients parsing responses may execute unintended behavior.

---

### BUG-061: GetObject MPU reassembly doesn't validate part contiguity

**File**: `dfs/s3_server/src/handlers.rs` (get_object MPU path)

**Problem**: Parts are sorted by number but no validation that all expected parts exist. If parts [1, 3, 5] exist but [2, 4] are missing, the object is silently assembled with gaps. The data is corrupted but returned as success.

**Impact**: Silent data corruption on read for multipart-uploaded objects with missing parts.

---

### BUG-062: `add_shard()` hardcodes "/m" split point and uses "z-{id}" for subsequent shards

**File**: `dfs/common/src/sharding.rs:94-109`

```rust
// Second shard always splits at "/m"
ranges.insert("/m".to_string(), shard_id);
// Third+ shards get "z-{shard_id}" keys
ranges.insert(format!("z-{}", shard_id), shard_id);
```

**Problem**: The split point "/m" is hardcoded regardless of actual data distribution. Subsequent shards are assigned to keyspace starting with "z-", meaning nearly all file paths (which start with "/") are crammed into the first two shards. Dynamic shard addition doesn't distribute load.

**Impact**: Severe load imbalance — new shards receive almost no traffic.

---

### BUG-063: Config server linearizable read doesn't wait for state machine catch-up

**File**: `dfs/metaserver/src/config_server.rs:47-49`

```rust
self.ensure_linearizable_read().await?;
let state_lock = self.state.lock().unwrap();
// reads state immediately — may not reflect committed entries
```

**Problem**: `ensure_linearizable_read()` confirms the leader is current but doesn't wait for `last_applied >= read_index`. The state machine may lag behind the committed index. Reading state immediately after may return stale data.

**Impact**: Stale shard map returned despite linearizable read guarantee. Clients may route to wrong shards.

---

### BUG-064: DeleteBucket doesn't check for .meta files

**File**: `dfs/s3_server/src/handlers.rs` (delete_bucket)

```rust
if files.is_empty() || (files.len() == 1 && files[0].ends_with(".s3keep")) {
    // delete bucket — ignores .meta files
```

**Problem**: Orphaned `.meta` files (SSE metadata, etc.) are not counted as "bucket contents". A bucket with only `.meta` files is considered empty and deleted, losing the metadata.

**Impact**: Metadata loss on bucket deletion. Storage leak from orphaned `.meta` files.

---

### MEDIUM Bugs (continued)

### BUG-065: CreateBucket relies on error string matching for duplicate detection

**File**: `dfs/s3_server/src/handlers.rs` (create_bucket)

```rust
if e.to_string().contains("already exists") {
    empty_response(StatusCode::CONFLICT)
```

**Problem**: Duplicate bucket detection depends on the error message containing "already exists". If the DFS client changes its error message format, this silently breaks. Should use typed errors, not string matching.

**Impact**: Incorrect HTTP status codes returned for duplicate bucket creation.

---

### BUG-066: ListBuckets returns hardcoded creation date

**File**: `dfs/s3_server/src/handlers.rs` (list_buckets)

```rust
creation_date: "2025-01-01T00:00:00.000Z".to_string(),  // hardcoded
```

**Problem**: All buckets report the same creation date regardless of when they were actually created. S3 SDK clients sorting or filtering by date get incorrect results.

**Impact**: Incorrect metadata. Audit/compliance issues.

---

### BUG-067: `get_file()` passes length=0 instead of block.size

**File**: `dfs/client/src/mod.rs:720`

```rust
self.read_block_range(&block.locations, &block.block_id, 0, 0)
```

**Problem**: Length 0 means "read entire block" but the last block of a file may be partial. If the ChunkServer stores a full block (with padding), the client reads padding bytes as data. Should pass `block.size` to read only valid data.

**Impact**: Trailing garbage bytes appended to file contents for files whose size isn't a multiple of block size.

---

---

## Additional Findings (Round 6)

### CRITICAL Bugs (continued)

### BUG-068: Chunked payload decoder panics on malformed input

**File**: `dfs/s3_server/src/handlers.rs:291-320`

```rust
let chunk_header = &current[..line_end];
// ...
current = &current[line_end + 2..];  // No bounds check
// ...
current = &current[chunk_size + 2..];  // No bounds check
```

**Problem**: `line_end + 2` and `chunk_size + 2` are used to slice without checking that the indices are within bounds. A malformed chunked request (e.g., `chunk_size` larger than remaining data, or missing CRLF) causes an index-out-of-bounds panic.

**Impact**: DoS — any malformed chunked upload crashes the S3 server.

---

### BUG-069: Chunked encoding per-chunk signatures never verified

**File**: `dfs/s3_server/src/handlers.rs:931-935`

```rust
let body = if content_sha256 == "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" {
    decode_chunked_payload(body)  // Only extracts data, ignores per-chunk signatures
} else {
    body
};
```

**Problem**: AWS SigV4 chunked uploads include a signature per chunk in the format `chunk-size;chunk-signature=<sig>\r\n<data>\r\n`. The decoder extracts chunk data but never verifies the per-chunk signatures. An attacker can modify data in transit after the initial Authorization header is verified.

**Impact**: Data integrity bypass — uploaded data can be tampered with in transit.

---

### HIGH Bugs (continued)

### BUG-070: `merge_shards` corrupts shard map for non-adjacent shards

**File**: `dfs/common/src/sharding.rs:210-245`

```rust
pub fn merge_shards(&mut self, victim_shard_id: &ShardId, retained_shard_id: &ShardId) -> bool {
    // ... finds victim's key and removes it ...
    // No adjacency validation!
```

**Problem**: The function assumes victim and retained are adjacent but never validates this. If they're not adjacent, removing the victim's boundary causes the **wrong shard** (the one between them) to absorb the victim's key range. Example: with shards s1→[min,"/m"), s2→["/m","/t"), s0→["/t",max), calling `merge(victim="s1", retained="s0")` removes "/m", making s2 absorb s1's range instead of s0.

**Impact**: Shard map corruption — files routed to wrong shard after merge. Data inaccessible.

---

### BUG-071: Incomplete chunk data silently truncated

**File**: `dfs/s3_server/src/handlers.rs:312-317`

```rust
if current.len() < chunk_size + 2 {
    break;  // Silent break — no error returned
}
```

**Problem**: When a chunked upload stream is incomplete (connection dropped mid-chunk), the decoder silently breaks out of the loop. The partially-decoded data is returned as if it were the complete upload. The caller writes this truncated data as the full object.

**Impact**: Silent data truncation. Client's upload is partially stored, but server returns success.

---

### BUG-072: Audit log batch buffer grows without limit

**File**: `dfs/s3_server/src/audit.rs`

**Problem**: The `batch_buffer` Vec accumulates audit records until the flush interval. If RocksDB writes stall or the flush fails, records continue to accumulate without any backpressure or size limit.

**Impact**: OOM crash under sustained high load with slow disk I/O.

---

### BUG-073: Audit log uses `try_send` — records silently dropped

**File**: `dfs/s3_server/src/audit.rs`

```rust
let _ = sender.try_send(record);  // Non-blocking, drops on full channel
```

**Problem**: Audit records are sent via a bounded channel with `try_send`. If the channel is full (consumer lagging), records are silently dropped. No counter, no alert, no fallback.

**Impact**: Missing audit trail. Compliance violation — security events lost without detection.

---

### MEDIUM Bugs (continued)

### BUG-074: EC block read uses shard index from `locations` array index, not `shard_index` field

**File**: `dfs/client/src/mod.rs:1118-1138`

```rust
for (i, loc) in block.locations.iter().enumerate() {
    // Uses `i` (array index) as shard index
    fetch_futs.push(async move { (i, result) });
}
// ...
opt_shards[i] = Some(data);
```

**Problem**: The shard index is derived from the array position in `locations`, not from `block.shard_index` (proto field 5). If locations are returned in non-sequential order by the master, shard data is placed in wrong positions in `opt_shards`. RS decoding then produces garbage.

**Impact**: Silent data corruption on EC block reads when location order doesn't match shard order.

---

### BUG-075: `read_file_range` EC path doesn't bounds-check start index

**File**: `dfs/client/src/mod.rs:822-824`

```rust
let start = block_offset as usize;
let end = (block_offset + block_length) as usize;
full[start..end.min(full.len())].to_vec()
```

**Problem**: `end` is bounds-checked (`end.min(full.len())`) but `start` is not. If `block_offset` exceeds `full.len()`, `full[start..]` panics with index out of bounds.

**Impact**: Panic on corrupted block metadata where offset exceeds actual block size.

---

### BUG-076: `remove_shard` for Range sharding creates key range gaps

**File**: `dfs/common/src/sharding.rs:123-153`

```rust
ShardingStrategy::Range { ranges } => {
    let keys_to_remove: Vec<String> = ranges.iter()
        .filter(|(_, sid)| *sid == shard_id)
        .map(|(k, _)| k.clone())
        .collect();
    for k in keys_to_remove {
        ranges.remove(&k);  // Removes boundary without reassigning range
    }
}
```

**Problem**: Removing a shard's boundary entries without reassigning those ranges to another shard leaves orphaned key ranges. Queries for keys in the removed range may silently fall to the next shard (unrelated to the intended successor) or return `None` if the removed shard had the last boundary.

**Impact**: Key ranges become unmapped or map to wrong shard after shard removal.

---

---

## Additional Findings (Round 7 — Final)

### HIGH Bugs (continued)

### BUG-077: CopyObject `.unwrap()` on header access panics on malformed request

**File**: `dfs/s3_server/src/handlers.rs:210`

```rust
let source = headers.get("x-amz-copy-source").unwrap().to_str().unwrap();
```

**Problem**: Although the router checks `headers.contains_key("x-amz-copy-source")` on line 209, the `.to_str().unwrap()` panics if the header value contains non-ASCII bytes (valid in HTTP but not valid UTF-8). A crafted `x-amz-copy-source` header with non-UTF-8 bytes crashes the server.

**Impact**: DoS — single malformed CopyObject request crashes the S3 server.

---

### BUG-078: CopyObject `.unwrap()` on file I/O in critical path

**File**: `dfs/s3_server/src/handlers.rs:520-522`

```rust
let mut file = std::fs::File::open(&temp_path).unwrap();
let mut src_body = Vec::new();
file.read_to_end(&mut src_body).unwrap();
```

**Problem**: After downloading the source file to a temp path, the code opens and reads it with `.unwrap()`. If the temp file was cleaned up by the OS, or the disk is full, or permissions changed, this panics instead of returning a proper S3 error.

**Impact**: DoS — CopyObject for large files under disk pressure crashes the server.

---

### MEDIUM Bugs (continued)

### BUG-079: Range request underflow exists in 3 separate code paths

**File**: `dfs/s3_server/src/handlers.rs:1147, 1214, 1309`

**Problem**: BUG-015 identified the underflow at line 1147. The same `total_size - 1` underflow pattern appears in two additional code paths:
- Line 1214: MPU source range handling
- Line 1309: SSE-decrypted object range handling

All three paths have `total_size - 1` which underflows when the data is empty. BUG-015 only covered the first instance.

**Impact**: DoS in 3 separate code paths, not just 1.

---

### BUG-056: Test scripts missing `set -u` (11 files)

**Files**: `test_scripts/{network_partition,chaos,tls_e2e,cluster_membership,fault_recovery,oidc_sts,run_s3,dynamic_membership,transaction_abort,auto_scaling,simple_chaos}_test.sh`

**Problem**: Undefined variables silently expand to empty strings → tests can pass for wrong reasons.

---

### BUG-057: Docker Compose `depends_on` doesn't wait for service readiness

**File**: `docker-compose.yml:38-40`

**Problem**: `depends_on` waits for container start, not health. gRPC services may not respond for seconds after start → flaky tests.

---

## Recommendations

### Immediate (P0)

### Immediate (P0) — Data Loss / Security
1. **BUG-001/002**: Add empty-log guards everywhere `self.log.len() - 1` appears. Add helper `fn last_log_index(&self) -> usize`.
2. **BUG-003**: Audit all log index calculations for consistency with the NoOp padding entry at `log[0]`.
3. **BUG-004**: Return `success: false` when `replicas_written < min_replicas`.
4. **BUG-005/040**: Reject `master_term == 0`; upgrade all Raft term atomics to `Ordering::SeqCst`.
5. **BUG-006/029**: Audit all `let _ =` on Raft sends — propagate errors or add retry logic.
6. **BUG-021**: Update `config_change_state` after appending C-new entry.
7. **BUG-022/038**: Use `WriteBatch` for atomic config and log persistence.
8. **BUG-033**: Return error and abort snapshot install on deserialization failure.
9. **BUG-034**: Redact credentials from audit logs; restrict file permissions to 0600.
10. **BUG-035**: Require minimum 32-byte STS signing key.

### Short-term (P1) — Correctness / Security
11. **BUG-008**: Add transaction state machine with valid transition enforcement.
12. **BUG-007**: Two-phase shard split — delete source files only after ingestion ACK.
13. **BUG-013/036**: Sanitize paths in CopyObject and ListObjects continuation tokens.
14. **BUG-009**: Add transaction timeout/cleanup for orphaned Prepared records.
15. **BUG-012/014**: Fix presigned payload bypass; require auth for `Principal: "*"`.
16. **BUG-023**: Add per-object AAD to SSE encryption.
17. **BUG-024**: Cap STS session duration at 43200 seconds.
18. **BUG-037**: Validate `max_keys` range (1–1000).
19. **BUG-039**: Track background task JoinHandles; restart on panic.
20. **BUG-043/046**: Fix safe mode block double-counting; use saturating_sub for timeout.

### Medium-term (P2) — Robustness
21. Add linearizability tests that exercise snapshot + election edge cases.
22. Fuzz the S3 API (range headers, multipart, presigned URLs) with `cargo-fuzz`.
23. Add integration tests for shard split/merge under network partitions.
24. Add shard map versioning (BUG-025) and synchronous client refresh (BUG-011).
25. Audit all membership change paths for `config_change_state` consistency (BUG-027/028).
26. Add pagination to `list_files` RPC (BUG-045).
27. Replace `.unwrap()` in S3 handler I/O paths with proper error propagation (BUG-050).
28. Fix HeadObject Content-Length for multipart objects (BUG-041).
29. Use RocksDB `WriteBatch` or prefix-scan for log deletion to avoid orphaned entries (BUG-051).
30. Handle SIGTERM and flush Raft state on shutdown (BUG-052).
31. Add jitter to election retry backoff (BUG-054).
32. Fix `get_shard()` to use exclusive boundary comparison (BUG-058).
33. Add bounds checks to chunked payload decoder (BUG-068).
34. Implement per-chunk SigV4 signature verification (BUG-069).
35. Add adjacency validation to `merge_shards` (BUG-070).
36. Sanitize all user-supplied keys/paths in S3 handlers (BUG-059).
37. Escape XML special characters in all S3 error responses (BUG-060).
38. Add bounded audit log buffer with backpressure (BUG-072/073).
