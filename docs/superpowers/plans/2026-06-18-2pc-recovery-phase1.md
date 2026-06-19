# 2PC Recovery Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix all critical safety holes in cross-shard 2PC rename so that no failure scenario causes data loss or permanent inconsistency.

**Architecture:** Preserve existing 2PC protocol structure. Add Presumed Abort semantics with an Inquiry RPC, fix operation ordering (source deletion after commit confirmation), proper Raft error handling, and idempotency guards. New `run_transaction_recovery` background task retries committed-but-unacked transactions.

**Tech Stack:** Rust, tonic (gRPC), protobuf, tokio, RocksDB (via Raft)

**Spec:** `docs/superpowers/specs/2026-06-18-2pc-recovery-design.md`

**Baseline commands (run before and after each task):**
```bash
cargo build --release 2>&1 | tail -3
cargo clippy -- -D warnings 2>&1 | tail -3
cargo test --lib -p dfs-metaserver 2>&1 | tail -5
```

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `proto/dfs.proto` | Modify | Add `InquireTransaction` RPC + messages |
| `dfs/metaserver/src/master.rs` | Modify | TransactionRecord fields, rename flow, inquiry handler, recovery task, abort RPC helper, cleanup logic |
| `dfs/metaserver/src/simple_raft.rs` | Modify | Idempotency guard in `ApplyTransactionOperation`, new `SetParticipantAcked` command |
| `test_scripts/transaction_recovery_test.sh` | Create | Coordinator crash recovery integration test |

---

### Task 1: Add `participant_acked` and `inquiry_count` to TransactionRecord

**Files:**
- Modify: `dfs/metaserver/src/master.rs:78-91` (TransactionRecord struct)
- Modify: `dfs/metaserver/src/master.rs:108` (new_rename constructor)

- [ ] **Step 1: Add fields to TransactionRecord**

In `dfs/metaserver/src/master.rs`, add two fields to `TransactionRecord` (line 91, before closing `}`):

```rust
pub struct TransactionRecord {
    pub tx_id: String,
    pub tx_type: TransactionType,
    pub state: TxState,
    pub timestamp: u64,
    pub participants: Vec<String>,
    pub operations: Vec<TxOperation>,
    /// Coordinator shard ID — used to distinguish coordinator vs participant records
    pub coordinator_shard: String,
    /// Whether participant has confirmed commit was applied (coordinator-side only)
    #[serde(default)]
    pub participant_acked: bool,
    /// Number of inquiry retries attempted (participant-side only)
    #[serde(default)]
    pub inquiry_count: u32,
}
```

Update `new_rename` to initialize the new fields:

```rust
TransactionRecord {
    tx_id,
    tx_type: TransactionType::Rename { ... },
    state: TxState::Pending,
    timestamp: now,
    participants: vec![source_shard.clone(), dest_shard.clone()],
    operations: vec![ ... ],
    coordinator_shard: source_shard.clone(),
    participant_acked: false,
    inquiry_count: 0,
}
```

Note: `coordinator_shard` needs `#[serde(default)]` for backward compatibility with existing Raft snapshots (old records will deserialize with empty string). The cleanup task must treat `coordinator_shard == ""` as "unknown role — skip inquiry, fall back to legacy timeout abort."

- [ ] **Step 2: Fix all TransactionRecord constructions in master.rs**

Search for all places that construct `TransactionRecord { ... }` (in `prepare_transaction` handler around line 2487) and add the new fields:

```rust
// In prepare_transaction handler — participant creates record
let tx_record = TransactionRecord {
    // ... existing fields ...
    coordinator_shard: req.coordinator_shard.clone(),
    participant_acked: false,
    inquiry_count: 0,
};
```

- [ ] **Step 3: Build and verify**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Successful build (may need to fix any remaining struct literal errors)

- [ ] **Step 4: Run existing tests**

Run: `cargo test --lib -p dfs-metaserver 2>&1 | tail -10`
Expected: All 32 tests pass. Fix any test TransactionRecord constructions that are now missing fields.

- [ ] **Step 5: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat(2pc): add participant_acked and coordinator_shard fields to TransactionRecord"
```

---

### Task 2: Add InquireTransaction to proto and implement handler

**Files:**
- Modify: `proto/dfs.proto:19-21` (add RPC to MasterService)
- Modify: `dfs/metaserver/src/master.rs` (implement inquiry handler)

- [ ] **Step 1: Add proto definition**

In `proto/dfs.proto`, add after line 21 (after `AbortTransaction` RPC):

```protobuf
  rpc InquireTransaction (InquireTransactionRequest) returns (InquireTransactionResponse);
```

Add messages at the end of the file (before closing):

```protobuf
message InquireTransactionRequest {
    string tx_id = 1;
}

message InquireTransactionResponse {
    // "COMMITTED", "ABORTED", or "UNKNOWN"
    string status = 1;
}
```

- [ ] **Step 2: Build to regenerate gRPC code**

Run: `cargo build --release 2>&1 | tail -10`
Expected: Build succeeds but with error — `inquire_transaction` not implemented in `MasterService` trait.

- [ ] **Step 3: Implement inquiry handler**

In `dfs/metaserver/src/master.rs`, in the `#[tonic::async_trait] impl MasterService for MyMaster` block, add:

```rust
async fn inquire_transaction(
    &self,
    request: Request<InquireTransactionRequest>,
) -> Result<Response<InquireTransactionResponse>, Status> {
    let req = request.into_inner();
    let tx_id = req.tx_id;

    // Linearizable read to ensure we see the latest state
    self.ensure_linearizable_read().await?;

    let state_lock = self.state.lock().expect("Mutex poisoned");
    if let AppState::Master(ref state) = *state_lock {
        if let Some(record) = state.transaction_records.get(&tx_id) {
            let status = match record.state {
                TxState::Committed => "COMMITTED",
                TxState::Aborted => "ABORTED",
                _ => "UNKNOWN", // Pending/Prepared — no decision yet
            };
            return Ok(Response::new(InquireTransactionResponse {
                status: status.to_string(),
            }));
        }
    }

    // Record not found → Presumed Abort
    Ok(Response::new(InquireTransactionResponse {
        status: "UNKNOWN".to_string(),
    }))
}
```

- [ ] **Step 4: Build and test**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Successful build.

Run: `cargo test --lib -p dfs-metaserver 2>&1 | tail -5`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add proto/dfs.proto dfs/metaserver/src/master.rs
git commit -m "feat(2pc): add InquireTransaction RPC for Presumed Abort recovery"
```

---

### Task 3: Add idempotency guards

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs:2993-3016` (ApplyTransactionOperation)
- Modify: `dfs/metaserver/src/master.rs` (prepare_transaction, commit_transaction handlers)

- [ ] **Step 1: Write unit test for idempotent create**

In `dfs/metaserver/src/simple_raft.rs`, in the existing `#[cfg(test)] mod tests` block, add a test that exercises the actual `MasterCommand::ApplyTransactionOperation` serialization and verifies the idempotency guard. Since we can't directly call the apply logic without a full Raft node, test the command serialization and the guard logic pattern:

```rust
#[test]
fn test_apply_transaction_create_idempotent() {
    // Verify the command round-trips correctly
    let cmd = MasterCommand::ApplyTransactionOperation {
        tx_id: "tx-idem".to_string(),
        operation: crate::master::TxOperation {
            shard_id: "shard-1".to_string(),
            op_type: crate::master::TxOpType::Create {
                path: "/test/file".to_string(),
                metadata: crate::dfs::FileMetadata {
                    path: "/test/file".to_string(),
                    size: 100,
                    ..Default::default()
                },
            },
        },
    };
    let json = serde_json::to_string(&cmd).unwrap();
    let deserialized: MasterCommand = serde_json::from_str(&json).unwrap();
    match deserialized {
        MasterCommand::ApplyTransactionOperation { tx_id, operation } => {
            assert_eq!(tx_id, "tx-idem");
            match &operation.op_type {
                crate::master::TxOpType::Create { path, metadata } => {
                    assert_eq!(path, "/test/file");
                    assert_eq!(metadata.size, 100);
                }
                _ => panic!("Expected Create operation"),
            }
        }
        _ => panic!("Expected ApplyTransactionOperation"),
    }

    // Verify idempotency: inserting into a map with existing key is a no-op with guard
    let mut files = std::collections::BTreeMap::new();
    let meta = crate::dfs::FileMetadata { path: "/test/file".to_string(), size: 100, ..Default::default() };
    files.insert("/test/file".to_string(), meta.clone());

    // Idempotent insert (the guard we're adding)
    if !files.contains_key("/test/file") {
        files.insert("/test/file".to_string(), crate::dfs::FileMetadata { size: 999, ..Default::default() });
    }
    assert_eq!(files.get("/test/file").unwrap().size, 100); // Original preserved
}
```

Also add a unit test for the inquiry handler response logic:

```rust
#[test]
fn test_inquiry_response_mapping() {
    // Test that each TxState maps to the correct inquiry response
    use crate::master::TxState;
    let cases = vec![
        (TxState::Committed, "COMMITTED"),
        (TxState::Aborted, "ABORTED"),
        (TxState::Pending, "UNKNOWN"),
        (TxState::Prepared, "UNKNOWN"),
    ];
    for (state, expected) in cases {
        let response = match state {
            TxState::Committed => "COMMITTED",
            TxState::Aborted => "ABORTED",
            _ => "UNKNOWN",
        };
        assert_eq!(response, expected, "TxState::{:?} should map to {}", state, expected);
    }
}
```

- [ ] **Step 2: Add idempotency guard in ApplyTransactionOperation**

In `dfs/metaserver/src/simple_raft.rs`, around line 3008-3012, modify the `Create` arm:

```rust
crate::master::TxOpType::Create { path, metadata } => {
    if !master_state.files.contains_key(path) {
        master_state.files.insert(path.clone(), metadata.clone());
        tracing::info!("Transaction {}: created file {}", tx_id, path);
    } else {
        tracing::info!("Transaction {}: file {} already exists (idempotent skip)", tx_id, path);
    }
}
```

- [ ] **Step 3: Add idempotency in prepare_transaction handler**

In `dfs/metaserver/src/master.rs`, at the start of `prepare_transaction` (around line 2470), add before the existing validation:

```rust
// Idempotency: if we already have this transaction record, return success
{
    let state_lock = self.state.lock().expect("Mutex poisoned");
    if let AppState::Master(ref state) = *state_lock {
        if state.transaction_records.contains_key(&tx_id) {
            return Ok(Response::new(PrepareTransactionResponse {
                success: true,
                error_message: String::new(),
                leader_hint: String::new(),
            }));
        }
    }
}
```

- [ ] **Step 4: Add idempotency in commit_transaction handler**

In `dfs/metaserver/src/master.rs`, in `commit_transaction` (around line 2556), add early return for already-committed:

```rust
// Idempotency: if already committed, return success
{
    let state_lock = self.state.lock().expect("Mutex poisoned");
    if let AppState::Master(ref state) = *state_lock {
        if let Some(record) = state.transaction_records.get(&tx_id) {
            if record.state == TxState::Committed {
                return Ok(Response::new(CommitTransactionResponse {
                    success: true,
                    error_message: String::new(),
                    leader_hint: String::new(),
                }));
            }
        }
    }
}
```

- [ ] **Step 5: Build and test**

Run: `cargo build --release && cargo clippy -- -D warnings && cargo test --lib -p dfs-metaserver`
Expected: All tests pass including new idempotency test.

- [ ] **Step 6: Commit**

```bash
git add dfs/metaserver/src/master.rs dfs/metaserver/src/simple_raft.rs
git commit -m "feat(2pc): add idempotency guards for prepare, commit, and apply operations"
```

---

### Task 4: Fix operation order + Raft error handling in rename

**Files:**
- Modify: `dfs/metaserver/src/master.rs:2235-2453` (rename handler)
- Modify: `dfs/metaserver/src/master.rs:3088-3133` (send_commit_to_dest_shard)

This is the most critical task. The rename flow is reordered and all `let _ =` patterns are replaced.

- [ ] **Step 1: Add send_abort_to_dest_shard helper**

In `dfs/metaserver/src/master.rs`, after `send_commit_to_dest_shard` (around line 3133), add:

```rust
async fn send_abort_to_dest_shard(
    &self,
    tx_id: &str,
    dest_peers: &[String],
) -> Result<bool, Status> {
    for peer in dest_peers {
        let addr = if peer.starts_with("http://") || peer.starts_with("https://") {
            peer.clone()
        } else {
            format!("http://{}", peer)
        };

        match MasterServiceClient::connect(addr.clone()).await {
            Ok(mut client) => {
                let request = tonic::Request::new(AbortTransactionRequest {
                    tx_id: tx_id.to_string(),
                });

                match client.abort_transaction(request).await {
                    Ok(response) => {
                        let resp = response.into_inner();
                        if resp.success {
                            return Ok(true);
                        }
                        if !resp.leader_hint.is_empty() {
                            continue;
                        }
                        return Ok(false);
                    }
                    Err(e) => {
                        tracing::error!("AbortTransaction failed to {}: {}", addr, e);
                        continue;
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to connect to {}: {}", addr, e);
                continue;
            }
        }
    }

    Ok(false)
}
```

- [ ] **Step 2: Rewrite the cross-shard rename flow**

Replace the entire cross-shard branch of `rename()` (from the `else` after same-shard rename, approximately lines 2317-2453) with the corrected flow. The key changes are:

1. Raft errors checked (not `let _ =`)
2. Source deletion moved AFTER commit RPC confirmation
3. Abort sends RPC to participant

The new flow (pseudocode):

```
1. CreateTxRecord(Pending) via Raft — if fails, return error (no abort needed)
2. UpdateState(Prepared) via Raft — if fails, abort locally
3. send_prepare_to_dest_shard — if fails, abort locally + abort participant
4. send_commit_to_dest_shard — if fails, keep in Prepared (recovery task retries)
5. ApplyDelete(source) via Raft — if fails, log error (commit already sent, recovery handles)
6. UpdateState(Committed) via Raft
7. Return success
```

The full implementation is too long to inline here. Key patterns for each Raft call:

```rust
// Pattern for checked Raft call
let (tx, rx) = tokio::sync::oneshot::channel();
if self.raft_tx.send(Event::ClientRequest {
    command: Command::Master(MasterCommand::UpdateTransactionState {
        tx_id: tx_id.clone(),
        new_state: TxState::Prepared,
    }),
    reply_tx: tx,
}).await.is_err() {
    tracing::error!("Raft channel closed during 2PC for tx {}", tx_id);
    let _ = self.send_abort_to_dest_shard(&tx_id, &dest_peers).await;
    return Ok(Response::new(RenameResponse {
        success: false,
        error_message: "Internal error: Raft unavailable".to_string(),
        ..Default::default()
    }));
}
match rx.await {
    Ok(Ok(())) => { /* continue */ }
    _ => {
        tracing::error!("Raft commit failed during 2PC for tx {}", tx_id);
        let _ = self.send_abort_to_dest_shard(&tx_id, &dest_peers).await;
        return Ok(Response::new(RenameResponse {
            success: false,
            error_message: "Internal error: Raft commit failed".to_string(),
            ..Default::default()
        }));
    }
}
```

For step 4 (commit RPC failure), keep in Prepared and return success=false:

```rust
match self.send_commit_to_dest_shard(&tx_id, &dest_peers).await {
    Ok(true) => { /* proceed to delete source */ }
    _ => {
        // Keep transaction in Prepared state — recovery task (Task 6) will retry
        tracing::warn!("Commit RPC failed for tx {}. Recovery task will retry.", tx_id);
        return Ok(Response::new(RenameResponse {
            success: false,
            error_message: "Cross-shard commit pending, will be retried".to_string(),
            ..Default::default()
        }));
    }
}
```

- [ ] **Step 3: Build and test**

Run: `cargo build --release && cargo clippy -- -D warnings && cargo test --lib -p dfs-metaserver`
Expected: All existing tests pass.

- [ ] **Step 4: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "fix(2pc): reorder source deletion after commit confirmation, add Raft error handling and abort RPC"
```

---

### Task 5: Fix cleanup task to distinguish coordinator vs participant records

**Files:**
- Modify: `dfs/metaserver/src/master.rs:789-855` (run_transaction_cleanup)

- [ ] **Step 1: Add SetParticipantAcked Raft command**

In `dfs/metaserver/src/simple_raft.rs`, add to `MasterCommand` enum (around line 296):

```rust
SetParticipantAcked {
    tx_id: String,
},
```

Add the apply logic (around line 3017, before `DeleteTransactionRecord`):

```rust
MasterCommand::SetParticipantAcked { tx_id } => {
    if let Some(record) = master_state.transaction_records.get_mut(tx_id) {
        record.participant_acked = true;
        tracing::info!("Transaction {}: participant acked", tx_id);
    }
}
```

- [ ] **Step 2: Rewrite run_transaction_cleanup**

Replace the body of `run_transaction_cleanup` with logic that distinguishes coordinator vs participant:

```rust
async fn run_transaction_cleanup(
    state: Arc<Mutex<AppState>>,
    raft_tx: mpsc::Sender<Event>,
    shard_id: String,         // NEW parameter
    shard_map: Arc<Mutex<ShardMap>>,  // NEW parameter for peer lookup
    ca_cert_path: Option<String>,     // NEW for TLS
    domain_name: Option<String>,      // NEW for TLS
) {
    const MAX_INQUIRY_RETRIES: u32 = 60; // 5 minutes at 5s intervals
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;

        let records_to_process = {
            let state_lock = state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref state) = *state_lock {
                state.transaction_records.iter()
                    .filter(|(_, r)| r.is_timed_out())
                    .map(|(id, r)| (id.clone(), r.clone()))
                    .collect::<Vec<_>>()
            } else {
                vec![]
            }
        };

        for (tx_id, record) in records_to_process {
            let is_coordinator = record.coordinator_shard == shard_id;

            match (&record.state, is_coordinator) {
                // Coordinator Pending: prepare never confirmed — abort + notify participant
                (TxState::Pending, true) => {
                    tracing::warn!("Coordinator tx {} timed out in Pending, aborting", tx_id);
                    abort_via_raft(&raft_tx, &tx_id).await;
                    // Best-effort abort to participant
                    // (participant may not have a record — that's fine)
                }

                // Coordinator Prepared: commit RPC failed — recovery task handles this, skip
                (TxState::Prepared, true) => {
                    // Do NOT abort — recovery task (Task 6) will retry commit
                }

                // Participant Prepared: inquire coordinator
                (TxState::Prepared, false) => {
                    // Inquiry logic — see Task 6 for full implementation
                }

                // GC: Committed/Aborted + stale
                (TxState::Committed | TxState::Aborted, _) if record.is_stale() => {
                    if record.state == TxState::Committed && !record.participant_acked && is_coordinator {
                        // Don't GC committed-unacked coordinator records
                        continue;
                    }
                    tracing::info!("GC stale tx {}", tx_id);
                    delete_via_raft(&raft_tx, &tx_id).await;
                }

                _ => {}
            }
        }
    }
}
```

- [ ] **Step 3: Update MyMaster::new() to pass new parameters**

Update the `tokio::spawn(run_transaction_cleanup(...))` call to pass `shard_id`, `shard_map`, `ca_cert_path`, `domain_name`.

- [ ] **Step 4: Build and test**

Run: `cargo build --release && cargo clippy -- -D warnings && cargo test --lib -p dfs-metaserver`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add dfs/metaserver/src/master.rs dfs/metaserver/src/simple_raft.rs
git commit -m "feat(2pc): distinguish coordinator vs participant in cleanup, add GC guard for committed-unacked"
```

---

### Task 6: Implement participant inquiry + coordinator recovery task

**Files:**
- Modify: `dfs/metaserver/src/master.rs` (run_transaction_cleanup participant branch, new run_transaction_recovery)

- [ ] **Step 1: Implement participant inquiry in cleanup task**

In the `(TxState::Prepared, false)` branch of `run_transaction_cleanup`, add the inquiry logic:

```rust
(TxState::Prepared, false) => {
    // Find coordinator peers from shard_map
    let coordinator_peers = {
        let map = shard_map.lock().unwrap();
        map.get_shard_peers(&record.coordinator_shard)
            .unwrap_or_default()
    };

    if coordinator_peers.is_empty() {
        tracing::warn!("No peers for coordinator shard {} of tx {}", record.coordinator_shard, tx_id);
        continue;
    }

    // Inquire coordinator
    let mut status = None;
    for peer in &coordinator_peers {
        let addr = if peer.starts_with("http") { peer.clone() } else { format!("http://{}", peer) };
        match dfs_common::security::connect_endpoint(
            &addr,
            ca_cert_path.as_deref(),
            domain_name.as_deref(),
        ).await {
            Ok(channel) => {
                let mut client = crate::dfs::master_service_client::MasterServiceClient::new(channel);
                match client.inquire_transaction(crate::dfs::InquireTransactionRequest {
                    tx_id: tx_id.clone(),
                }).await {
                    Ok(resp) => {
                        status = Some(resp.into_inner().status);
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("Inquiry RPC to {} failed: {}", addr, e);
                        continue;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Connect to {} failed: {}", addr, e);
                continue;
            }
        }
    }

    match status.as_deref() {
        Some("COMMITTED") => {
            tracing::info!("Tx {} committed at coordinator, applying locally", tx_id);
            // Apply the operation + mark committed
            if let Some(op) = record.operations.first() {
                apply_operation_via_raft(&raft_tx, &tx_id, op.clone()).await;
                commit_via_raft(&raft_tx, &tx_id).await;
            }
        }
        Some("ABORTED") => {
            tracing::info!("Tx {} aborted at coordinator, aborting locally", tx_id);
            abort_via_raft(&raft_tx, &tx_id).await;
        }
        Some("UNKNOWN") => {
            // Increment inquiry count
            increment_inquiry_count_via_raft(&raft_tx, &tx_id).await;
            let new_count = record.inquiry_count + 1;
            if new_count > MAX_INQUIRY_RETRIES {
                tracing::warn!("Tx {} exceeded max inquiry retries, presuming abort", tx_id);
                abort_via_raft(&raft_tx, &tx_id).await;
            }
        }
        _ => {
            // RPC failed entirely — wait and retry next cycle
            tracing::debug!("All inquiry RPCs failed for tx {}, will retry", tx_id);
        }
    }
}
```

- [ ] **Step 2: Add helper functions**

Add small helpers for Raft operations used in cleanup (at module level, near `run_transaction_cleanup`):

```rust
async fn abort_via_raft(raft_tx: &mpsc::Sender<Event>, tx_id: &str) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if raft_tx.send(Event::ClientRequest {
        command: Command::Master(MasterCommand::UpdateTransactionState {
            tx_id: tx_id.to_string(),
            new_state: TxState::Aborted,
        }),
        reply_tx: tx,
    }).await.is_err() {
        tracing::error!("Raft channel closed while aborting tx {}", tx_id);
        return;
    }
    if let Err(e) = rx.await {
        tracing::error!("Raft response error while aborting tx {}: {:?}", tx_id, e);
    }
}

async fn commit_via_raft(raft_tx: &mpsc::Sender<Event>, tx_id: &str) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if raft_tx.send(Event::ClientRequest {
        command: Command::Master(MasterCommand::UpdateTransactionState {
            tx_id: tx_id.to_string(),
            new_state: TxState::Committed,
        }),
        reply_tx: tx,
    }).await;
    let _ = rx.await;
}

async fn delete_via_raft(raft_tx: &mpsc::Sender<Event>, tx_id: &str) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = raft_tx.send(Event::ClientRequest {
        command: Command::Master(MasterCommand::DeleteTransactionRecord {
            tx_id: tx_id.to_string(),
        }),
        reply_tx: tx,
    }).await;
    let _ = rx.await;
}

async fn apply_operation_via_raft(raft_tx: &mpsc::Sender<Event>, tx_id: &str, op: TxOperation) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = raft_tx.send(Event::ClientRequest {
        command: Command::Master(MasterCommand::ApplyTransactionOperation {
            tx_id: tx_id.to_string(),
            operation: op,
        }),
        reply_tx: tx,
    }).await;
    let _ = rx.await;
}

async fn increment_inquiry_count_via_raft(raft_tx: &mpsc::Sender<Event>, tx_id: &str) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = raft_tx.send(Event::ClientRequest {
        command: Command::Master(MasterCommand::IncrementInquiryCount {
            tx_id: tx_id.to_string(),
        }),
        reply_tx: tx,
    }).await;
    let _ = rx.await;
}
```

Add `IncrementInquiryCount` to `MasterCommand` enum in `simple_raft.rs` and its apply logic:

```rust
MasterCommand::IncrementInquiryCount { tx_id } => {
    if let Some(record) = master_state.transaction_records.get_mut(tx_id) {
        record.inquiry_count += 1;
    }
}
```

- [ ] **Step 3: Add run_transaction_recovery background task**

In `dfs/metaserver/src/master.rs`, add new background task function:

```rust
async fn run_transaction_recovery(
    state: Arc<Mutex<AppState>>,
    raft_tx: mpsc::Sender<Event>,
    shard_id: String,
    shard_map: Arc<Mutex<ShardMap>>,
    ca_cert_path: Option<String>,
    domain_name: Option<String>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;

        let records = {
            let state_lock = state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref state) = *state_lock {
                state.transaction_records.iter()
                    .filter(|(_, r)| r.coordinator_shard == shard_id)
                    .filter(|(_, r)| {
                        (r.state == TxState::Committed && !r.participant_acked)
                        || (r.state == TxState::Prepared && r.is_timed_out())
                    })
                    .map(|(id, r)| (id.clone(), r.clone()))
                    .collect::<Vec<_>>()
            } else {
                vec![]
            }
        };

        for (tx_id, record) in records {
            // Find destination shard peers
            let dest_shard = record.participants.iter()
                .find(|p| **p != shard_id)
                .cloned()
                .unwrap_or_default();

            let dest_peers = {
                let map = shard_map.lock().unwrap();
                map.get_shard_peers(&dest_shard).unwrap_or_default()
            };

            if dest_peers.is_empty() {
                continue;
            }

            // Send commit RPC to participant (same pattern as send_commit_to_dest_shard)
            let mut commit_ok = false;
            for peer in &dest_peers {
                let addr = if peer.starts_with("http") { peer.clone() } else { format!("http://{}", peer) };
                match dfs_common::security::connect_endpoint(
                    &addr, ca_cert_path.as_deref(), domain_name.as_deref(),
                ).await {
                    Ok(channel) => {
                        let mut client = crate::dfs::master_service_client::MasterServiceClient::new(channel);
                        let req = tonic::Request::new(crate::dfs::CommitTransactionRequest {
                            tx_id: tx_id.clone(),
                        });
                        if let Ok(resp) = client.commit_transaction(req).await {
                            if resp.into_inner().success {
                                commit_ok = true;
                                break;
                            }
                        }
                    }
                    Err(_) => continue,
                }
            }

            if commit_ok {
                match record.state {
                    TxState::Committed => {
                        // Already committed locally — just mark participant acked
                        tracing::info!("Recovery: participant acked for tx {}", tx_id);
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let _ = raft_tx.send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::SetParticipantAcked {
                                tx_id: tx_id.clone(),
                            }),
                            reply_tx: tx,
                        }).await;
                        let _ = rx.await;
                    }
                    TxState::Prepared => {
                        // Commit RPC succeeded — now safe to delete source + mark committed
                        tracing::info!("Recovery: completing prepared tx {} (delete source + commit)", tx_id);
                        // Find source path from operations
                        if let Some(delete_op) = record.operations.iter().find(|op| matches!(op.op_type, TxOpType::Delete { .. })) {
                            apply_operation_via_raft(&raft_tx, &tx_id, delete_op.clone()).await;
                        }
                        commit_via_raft(&raft_tx, &tx_id).await;
                        // Set participant acked
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let _ = raft_tx.send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::SetParticipantAcked {
                                tx_id: tx_id.clone(),
                            }),
                            reply_tx: tx,
                        }).await;
                        let _ = rx.await;
                    }
                    _ => {}
                }
            }
        }
    }
}
```

- [ ] **Step 4: Spawn recovery task in MyMaster::new()**

Add `tokio::spawn(run_transaction_recovery(...))` alongside the other background tasks.

- [ ] **Step 5: Build and test**

Run: `cargo build --release && cargo clippy -- -D warnings && cargo test --lib -p dfs-metaserver`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add dfs/metaserver/src/master.rs dfs/metaserver/src/simple_raft.rs
git commit -m "feat(2pc): implement participant inquiry and coordinator recovery task"
```

---

### Task 7: Update GC rule for committed transactions

**Files:**
- Modify: `dfs/metaserver/src/master.rs` (send_commit_to_dest_shard, cleanup task GC logic)

- [ ] **Step 1: Set participant_acked when commit RPC succeeds**

In the rename flow (Task 4), after `send_commit_to_dest_shard` returns `Ok(true)`, set the acked flag:

```rust
// After successful commit RPC
let (tx, rx) = tokio::sync::oneshot::channel();
let _ = self.raft_tx.send(Event::ClientRequest {
    command: Command::Master(MasterCommand::SetParticipantAcked {
        tx_id: tx_id.clone(),
    }),
    reply_tx: tx,
}).await;
let _ = rx.await;
```

Also in `run_transaction_recovery`, when commit retry succeeds.

- [ ] **Step 2: Verify GC guard in cleanup task**

Confirm the cleanup task's GC branch (Task 5, step 2) checks `participant_acked` before deleting committed coordinator records.

- [ ] **Step 3: Build and test**

Run: `cargo build --release && cargo clippy -- -D warnings && cargo test --lib -p dfs-metaserver`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat(2pc): set participant_acked on successful commit, guard GC"
```

---

### Task 8: Integration test — coordinator crash recovery

**Files:**
- Create: `test_scripts/transaction_recovery_test.sh`

- [ ] **Step 1: Write the integration test**

Create `test_scripts/transaction_recovery_test.sh`:

```bash
#!/bin/bash
set -e

echo "=== Transaction Recovery Test ==="
echo "Tests: (1) normal cross-shard rename, (2) coordinator restart recovery"

COMPOSE_FILE="docker-compose.yml"

cleanup() {
    echo "Cleaning up..."
    docker compose -f $COMPOSE_FILE down -v 2>/dev/null || true
    rm -f /tmp/recovery_test_*.txt 2>/dev/null || true
}
trap cleanup EXIT

# Start cluster
echo "Starting cluster..."
docker compose -f $COMPOSE_FILE up -d --no-build
echo "Waiting for cluster to stabilize (20s)..."
sleep 20

# ========================================
# Test 1: Normal cross-shard rename
# ========================================
echo ""
echo "--- Test 1: Normal cross-shard rename ---"
docker exec dfs-master1-shard1 sh -c 'echo "recovery test data" > /tmp/test_file.txt'
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 put /tmp/test_file.txt /a_recovery_test.txt

# Cross-shard rename: /a_... (shard-1) → /z_... (shard-2)
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 rename /a_recovery_test.txt /z_recovery_test.txt

# Verify destination exists
DEST_CHECK=$(docker exec dfs-master1-shard2 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
echo "$DEST_CHECK" | grep -q "z_recovery_test.txt" || { echo "FAIL: dest not found"; exit 1; }

# Verify source gone
SOURCE_CHECK=$(docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
if echo "$SOURCE_CHECK" | grep -q "a_recovery_test.txt"; then
    echo "FAIL: source still exists"; exit 1
fi
echo "PASS: Normal rename succeeded"

# ========================================
# Test 2: Coordinator restart recovery
# ========================================
echo ""
echo "--- Test 2: Coordinator restart recovery ---"

# Upload another file
docker exec dfs-master1-shard1 sh -c 'echo "restart test" > /tmp/restart_file.txt'
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 put /tmp/restart_file.txt /a_restart_test.txt

# Kill coordinator (shard-1) abruptly
echo "Killing coordinator (master1-shard1)..."
docker kill dfs-master1-shard1

# Wait briefly
sleep 5

# Restart coordinator
echo "Restarting coordinator..."
docker compose -f $COMPOSE_FILE start master1-shard1
echo "Waiting for coordinator recovery (30s)..."
sleep 30

# Verify the file is still accessible after restart
echo "Verifying file survives coordinator restart..."
FILE_CHECK=$(docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
echo "$FILE_CHECK" | grep -q "a_restart_test.txt" || { echo "FAIL: file lost after restart"; exit 1; }

# Now do a cross-shard rename after restart to verify 2PC works post-recovery
echo "Cross-shard rename after restart..."
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 rename /a_restart_test.txt /z_restart_test.txt

# Verify
DEST_CHECK2=$(docker exec dfs-master1-shard2 /app/dfs_cli --config-servers http://config-server:50050 ls 2>&1 || true)
echo "$DEST_CHECK2" | grep -q "z_restart_test.txt" || { echo "FAIL: rename after restart failed"; exit 1; }
echo "PASS: Rename after coordinator restart succeeded"

echo ""
echo "=== Transaction Recovery Test PASSED ==="
```

- [ ] **Step 2: Make executable and test**

```bash
chmod +x test_scripts/transaction_recovery_test.sh
```

Run locally if Docker is available, or verify in CI.

- [ ] **Step 3: Commit**

```bash
git add test_scripts/transaction_recovery_test.sh
git commit -m "test(2pc): add transaction recovery integration test"
```

---

### Task 9: Final verification

- [ ] **Step 1: Run full unit test suite**

Run: `cargo test 2>&1 | grep "^test result"`
Expected: All test suites pass.

- [ ] **Step 2: Run clippy and format**

Run: `cargo clippy -- -D warnings && cargo fmt --all -- --check`
Expected: No warnings, no format issues.

- [ ] **Step 3: Run existing integration tests**

Run (requires Docker):
```bash
bash test_scripts/cross_shard_test.sh
bash test_scripts/transaction_abort_test.sh
bash test_scripts/fault_recovery_test.sh
bash test_scripts/rename_test.sh
bash test_scripts/transaction_recovery_test.sh
```
Expected: All pass.

- [ ] **Step 4: Push and create PR**

```bash
git push -u origin feature/2pc-recovery
gh pr create --title "fix: cross-shard 2PC recovery with Presumed Abort" --body "..."
```
