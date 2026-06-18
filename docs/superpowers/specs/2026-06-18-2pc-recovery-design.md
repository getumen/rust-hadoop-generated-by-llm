# Cross-Shard 2PC Recovery Design

## Problem

The current cross-shard rename implementation has critical safety holes that can cause **data loss** and **orphaned files**. Specifically:

1. **Data loss**: Coordinator deletes source file before confirming participant committed destination file. If participant's Raft commit fails, the file disappears from both shards. (Failure window #9, #12)
2. **Missing abort notification**: Coordinator never sends `AbortTransaction` RPC to participant. Participant relies solely on 10-second timeout cleanup. (Failure window #15)
3. **Silent Raft failures**: All Raft state transitions use `let _ =` — errors are silently ignored, allowing the protocol to continue in an inconsistent state.
4. **No idempotency**: Participant `commit_transaction` applies file creation without checking if already applied, risking duplicate operations on retry.
5. **No recovery protocol**: After crash, neither coordinator nor participant attempts to resume or resolve in-flight transactions.

## Design

### Phase 1: Fix Safety Holes (A+C Hybrid)

Fix the critical bugs while preserving the existing protocol structure. Raft log format and existing proto messages unchanged (except one new RPC).

#### 1.1 Operation Order Fix

**Current (unsafe)**:
```
Coordinator: Prepare → UpdatePrepared → DeleteSource → UpdateCommitted → CommitRPC
```

**Fixed**:
```
Coordinator: Prepare → UpdatePrepared → CommitRPC → confirm OK → DeleteSource → UpdateCommitted
```

Source deletion moves AFTER participant commit confirmation. This is the single most important change — it eliminates the data loss window (#9).

If `CommitRPC` fails after retries, coordinator keeps the transaction in `Prepared` state (no new state introduced). The recovery task (1.6) will retry the commit RPC periodically. Source file remains safe throughout.

#### 1.2 Raft Error Handling

Replace all `let _ = raft_tx.send(...)` in the rename flow with proper error checking:

```rust
// Before (unsafe)
let _ = self.raft_tx.send(Event::ClientRequest { ... }).await;
let _ = rx.await;

// After
if self.raft_tx.send(Event::ClientRequest { ... }).await.is_err() {
    return abort_transaction(...);
}
match rx.await {
    Ok(Ok(())) => { /* continue */ }
    _ => { return abort_transaction(...); }
}
```

If any Raft operation fails, the transaction aborts immediately. No silent continuation.

#### 1.3 Abort RPC to Participant

When coordinator aborts (prepare failure, Raft error, or any other reason), it sends `AbortTransaction` RPC to the participant:

```rust
// In the abort branch of rename()
self.send_abort_to_dest_shard(&tx_id, &dest_peers).await;
// Best-effort: if this fails, participant's cleanup task handles it via inquiry
```

Add `send_abort_to_dest_shard` helper following the same pattern as `send_commit_to_dest_shard`: iterate peers, try until one succeeds or all fail.

#### 1.4 Idempotency Guards

These guards are essential for safe recovery. When a participant crashes mid-commit and restarts, the inquiry protocol (1.5) may re-apply the commit. Without idempotency, this could create duplicate files or panic. Each guard makes the corresponding operation safe to retry.

**Participant prepare**: Check if transaction record already exists before creating:

```rust
// In prepare_transaction handler
if state.transaction_records.contains_key(&tx_id) {
    // Already prepared — return success (idempotent)
    return Ok(Response::new(PrepareTransactionResponse { success: true, .. }));
}
```

**Participant commit**: Check if file already exists before applying (critical for failure window #12 — participant crash during commit, Raft replayed the create on restart, then inquiry triggers re-commit):

```rust
// In ApplyTransactionOperation (simple_raft.rs)
TxOpType::Create { path, metadata } => {
    if !master_state.files.contains_key(&path) {
        master_state.files.insert(path.clone(), metadata.clone());
    }
}
```

**Participant commit_transaction handler**: Check if already committed:

```rust
// In commit_transaction handler
if record.state == TxState::Committed {
    return Ok(Response::new(CommitTransactionResponse { success: true, .. }));
}
```

#### 1.5 Presumed Abort + Inquiry Protocol

**New proto RPC** — added to `MasterService`:

```protobuf
// In MasterService
rpc InquireTransaction(InquireTransactionRequest) returns (InquireTransactionResponse);

message InquireTransactionRequest {
    string tx_id = 1;
}

message InquireTransactionResponse {
    // Standard Presumed Abort response set: COMMITTED, ABORTED, UNKNOWN.
    // UNKNOWN means "no record found" — participant should presume abort.
    // Note: we do NOT return PENDING or PREPARED. If the coordinator has not
    // yet committed, the participant should wait and retry (the RPC will fail
    // or return UNKNOWN if the coordinator crashed before committing).
    string status = 1;  // "COMMITTED", "ABORTED", "UNKNOWN"
}
```

**Coordinator handler** (`inquire_transaction`):

```rust
async fn inquire_transaction(&self, req) -> Response {
    // Linearizable read to ensure we see the latest state
    self.ensure_linearizable_read().await?;

    let state = self.state.lock();
    if let Some(record) = state.transaction_records.get(&tx_id) {
        match record.state {
            TxState::Committed => Response { status: "COMMITTED" },
            TxState::Aborted => Response { status: "ABORTED" },
            // Pending or Prepared — coordinator has not decided yet.
            // Return UNKNOWN so participant waits and retries.
            _ => Response { status: "UNKNOWN" },
        }
    } else {
        // Record not found (never created, or GC'd) → Presumed Abort
        Response { status: "UNKNOWN" }
    }
}
```

**Design note**: We return UNKNOWN (not PENDING) for in-progress transactions. This is intentional: under Presumed Abort, only COMMITTED and ABORTED are terminal decisions. UNKNOWN tells the participant "I don't have a commit decision for you — wait and retry." This follows the standard Presumed Abort inquiry response set. The participant distinguishes "coordinator still in progress" from "coordinator aborted" by retrying: if the coordinator eventually commits, the next inquiry will return COMMITTED; if it aborts or crashes, the record disappears and UNKNOWN persists until the participant's max inquiry lifetime expires.

**Participant recovery logic** — added to `run_transaction_cleanup`:

When the cleanup task finds a `Prepared` transaction that has timed out (>10s), instead of immediately aborting, it first inquires the coordinator:

```rust
// In run_transaction_cleanup, for timed-out Prepared transactions on participant side
for (tx_id, record) in timed_out_prepared_participant_records {
    let coordinator_peers = record.participants.iter()
        .filter(|p| p != &self_shard_id)
        .collect();

    match inquire_coordinator(tx_id, coordinator_peers).await {
        "COMMITTED" => {
            // Coordinator committed — apply our operation and mark committed
            apply_and_commit(tx_id, record);
        }
        "ABORTED" => {
            // Explicitly aborted — abort locally
            abort_locally(tx_id);
        }
        "UNKNOWN" => {
            // Could be: coordinator crashed, coordinator hasn't decided yet,
            // or coordinator committed and GC'd the record (prevented by 1.5 GC guard).
            // Increment inquiry_count on this record.
            increment_inquiry_count(tx_id);
            if inquiry_count > MAX_INQUIRY_RETRIES {  // e.g., 60 retries * 5s = 5 minutes
                // Max lifetime exceeded — presume abort
                abort_locally(tx_id);
            }
            // Otherwise: wait, retry on next cleanup cycle
        }
        Err(rpc_error) => {
            // Network failure or coordinator running old code without InquireTransaction.
            // Do NOT treat as UNKNOWN (which could trigger premature abort).
            // Simply wait and retry on next cleanup cycle.
            tracing::warn!("Inquiry RPC failed for tx {}: {}", tx_id, rpc_error);
        }
    }
}
```

**Max inquiry lifetime**: Participant retries inquiry up to `MAX_INQUIRY_RETRIES` (default: 60, i.e., 5 minutes at 5-second intervals). After that, presume abort. This prevents permanently stuck transactions when the coordinator is permanently unreachable.

**Rolling upgrade safety**: If the coordinator is running old code without `InquireTransaction`, the RPC returns gRPC `Unimplemented` error. This is handled as `Err(rpc_error)` — the participant waits and retries, same as a network failure. It does NOT presume abort. Eventually the old 10-second timeout behavior takes over (the `MAX_INQUIRY_RETRIES` limit).

**Coordinator vs. Participant record distinction**: The cleanup task must distinguish coordinator-owned records from participant-owned records. A record where `coordinator_shard == self.shard_id` is a coordinator record; otherwise it's a participant record.

- **Participant records** (Prepared, timed out): Inquire coordinator as described above.
- **Coordinator records** (Prepared, timed out): The recovery task (1.6) handles these — retries the commit RPC. The cleanup task should NOT abort coordinator records in Prepared state. Coordinator Prepared means "prepare succeeded, commit in progress or pending retry." Aborting it would discard a valid in-progress transaction.
- **Coordinator records** (Pending, timed out): Safe to abort — prepare was never confirmed. Send abort RPC to participant as best-effort.

**Presumed Abort GC guard**: Coordinator must NOT garbage collect `Committed` records until participant confirms. Add `participant_acked` field to `TransactionRecord`:

```rust
pub struct TransactionRecord {
    // ... existing fields ...
    #[serde(default)]  // Backward compatible with existing Raft snapshots
    pub participant_acked: bool,  // NEW: participant confirmed commit applied
    #[serde(default)]
    pub inquiry_count: u32,       // NEW: number of inquiry retries (participant-side)
}
```

GC rule changes from "Committed + 1 hour" to "Committed + `participant_acked` + 1 hour". The `CommitTransaction` RPC success response from participant sets this flag via a Raft command.

**Recovery timeline**:

| Coordinator state | Participant state | Recovery action |
|---|---|---|
| No record | Prepared | Participant inquires → UNKNOWN → eventually presume abort (safe: source was never deleted) |
| Pending | Prepared | Coordinator cleanup aborts Pending record + sends abort RPC → participant inquires → ABORTED → abort |
| Prepared | Prepared | Coordinator recovery task (1.6) retries commit RPC → participant commits. If coordinator unreachable, participant eventually presumes abort (safe: source never deleted because commit never confirmed) |
| Committed | Prepared | Participant inquires → COMMITTED → apply + ack |
| Committed | No record | Coordinator recovery task retries commit RPC → participant creates record + applies |
| Aborted | Prepared | Participant inquires → ABORTED → abort |

Every cell has a deterministic resolution. No data loss possible because source deletion only happens after participant commit confirmation (1.1).

#### 1.6 Coordinator Recovery Task

Add a new background task `run_transaction_recovery` that runs periodically:

```rust
async fn run_transaction_recovery(state, raft_tx, shard_map, ...) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;

        // 1. Retry commit for Committed but unacked transactions
        let committed_unacked = find_transactions(state, |r|
            r.state == TxState::Committed
            && !r.participant_acked
            && r.coordinator_shard == self_shard_id
        );
        for tx in committed_unacked {
            let dest_peers = resolve_dest_peers(tx, shard_map);
            if send_commit_to_dest_shard(tx.tx_id, dest_peers).await {
                // Mark participant_acked via Raft
                set_participant_acked(tx.tx_id);
            }
        }

        // 2. Retry commit for Prepared coordinator records (commit RPC failed earlier)
        let prepared_coordinator = find_transactions(state, |r|
            r.state == TxState::Prepared
            && r.coordinator_shard == self_shard_id
            && r.is_timed_out()  // >10s since last attempt
        );
        for tx in prepared_coordinator {
            let dest_peers = resolve_dest_peers(tx, shard_map);
            if send_commit_to_dest_shard(tx.tx_id, dest_peers).await {
                // Commit RPC succeeded — now safe to delete source and update state
                apply_delete_source_and_commit(tx);
            }
            // If failed, will retry on next cycle
        }
    }
}
```

This ensures that:
- If coordinator crashes after committing but before participant receives commit → commit is eventually delivered
- If coordinator's commit RPC failed (network issue) → retried until successful

### Phase 2: Performance Optimization

After Phase 1 is stable and tested, optimize the critical path latency.

#### 2.1 Eliminate Coordinator Pending/Prepared Raft Writes

With Presumed Abort, coordinator does not need to write `Pending` or `Prepared` states to Raft. The absence of a `Committed` record implies abort.

**Before (Phase 1)**: 4 coordinator Raft commits on critical path
```
CreateTxRecord(Pending) → UpdateState(Prepared) → CommitRPC OK → ApplyDelete → UpdateState(Committed)
```

**After**: 1 coordinator Raft commit on critical path
```
(no write) → PrepareRPC → CommitAndDeleteSource (single Raft entry) → return to client
```

New Raft command:
```rust
MasterCommand::CommitCrossShard {
    tx_id: String,
    source_path: String,
    dest_shard_id: String,
    dest_peers: Vec<String>,
}
```

This command atomically: (a) records commit decision, (b) deletes source file, (c) stores enough info for recovery to retry commit RPC.

#### 2.2 Async Commit Notification

Return success to client immediately after coordinator Raft commit. Send `CommitTransaction` RPC to participant asynchronously:

```rust
// Critical path ends here
let result = raft_commit(CommitCrossShard { ... }).await?;
// Return to client
response = RenameResponse { success: true };

// Async: notify participant (recovery task will retry if this fails)
tokio::spawn(async move {
    send_commit_to_dest_shard(tx_id, dest_peers).await;
});
```

#### 2.3 Batched Participant Operations

Participant combines `ApplyCreate` + `UpdateState(Committed)` into a single Raft entry:

```rust
MasterCommand::CommitTransactionWithOperation {
    tx_id: String,
    operation: TxOperation,
}
```

One Raft commit instead of two.

#### 2.4 Performance Comparison

| Metric | Current | Phase 1 | Phase 2 |
|---|---|---|---|
| Coordinator Raft (critical path) | 4 | 4 | 1 |
| Participant Raft (critical path) | 2 | 2 | 1 |
| Cross-shard RPC (critical path) | 2 | 2 | 1 |
| Client-visible latency | ~6 Raft + 2 RPC | ~6 Raft + 2 RPC | ~1 Raft + 1 RPC |
| Abort path Raft writes | 1-2 | 1-2 | 0 |

Phase 1 does not improve latency — it fixes safety. Phase 2 is approximately **5x faster** on the critical path compared to Phase 1.

## Proto Changes

### Phase 1

```protobuf
// Add to MasterService
rpc InquireTransaction(InquireTransactionRequest) returns (InquireTransactionResponse);

message InquireTransactionRequest {
    string tx_id = 1;
}

message InquireTransactionResponse {
    string status = 1;  // "COMMITTED", "ABORTED", "UNKNOWN"
}
```

### Phase 2

No additional proto changes. New Raft commands are internal (serialized via serde in Raft log, not gRPC).

## Raft Log Compatibility

### Phase 1

- `TransactionRecord` struct gains `participant_acked: bool` and `inquiry_count: u32` fields, both with `#[serde(default)]` for backward compatibility with existing snapshots
- No new `MasterCommand` variants (reuses existing commands)
- Existing Raft logs and snapshots remain readable

### Phase 2

- New `MasterCommand::CommitCrossShard` variant added. Old logs don't contain this variant, so no compatibility issue.
- New `MasterCommand::CommitTransactionWithOperation` variant added. Same reasoning.

## Testing Strategy

### Phase 1

1. **Unit tests**: 
   - Transaction state machine transitions with error injection
   - Idempotency guards (duplicate prepare, duplicate commit, create-already-exists)
   - Inquiry handler: record in each terminal state → correct response; no record → UNKNOWN
   - Inquiry handler: Committed + `participant_acked=false` → record must exist (GC must not have removed it)
   - Coordinator vs. participant record classification
2. **Existing integration tests**: `cross_shard_test.sh`, `transaction_abort_test.sh`, `fault_recovery_test.sh` must continue passing
3. **New: `transaction_recovery_test.sh`** — Coordinator crash recovery
   - Start cluster, begin cross-shard rename
   - Kill coordinator after Prepare but before Commit
   - Restart coordinator
   - Verify: file appears at destination, source deleted
   - Verify: no orphaned transaction records after recovery
4. **New: `transaction_inquiry_test.sh`** — Participant crash recovery
   - Start cluster, begin cross-shard rename
   - Kill participant after Prepare
   - Restart participant
   - Verify: participant inquires coordinator and resolves correctly
5. **New: `transaction_ordering_test.sh`** — Source deletion ordering
   - Inject failure/delay in CommitRPC (e.g., via Toxiproxy)
   - Verify source file still exists while CommitRPC is pending
   - Remove failure, verify rename completes
6. **New: `transaction_gc_guard_test.sh`** — Participant acked GC guard
   - Coordinator commits, kill participant before it receives commit
   - Wait longer than old GC interval (>1 hour in test with shortened config)
   - Restart participant
   - Verify participant inquires coordinator → gets COMMITTED (not UNKNOWN)
7. **Concurrent rename stress test**: Multiple concurrent cross-shard renames to overlapping paths, verify no data loss or duplicates

### Phase 2

8. **Benchmark**: Compare rename latency before/after with `dfs_cli benchmark`
9. **Stress test**: Concurrent cross-shard renames under load, verify no data loss

## Files Changed

### Phase 1

| File | Changes |
|---|---|
| `proto/dfs.proto` | Add `InquireTransaction` RPC + messages |
| `dfs/metaserver/src/master.rs` | Fix operation order, error handling, abort RPC, idempotency, inquiry handler, recovery task, `participant_acked`/`inquiry_count` fields, coordinator vs participant record distinction in cleanup |
| `dfs/metaserver/src/simple_raft.rs` | Idempotency guard in `ApplyTransactionOperation` |
| `test_scripts/transaction_recovery_test.sh` | New test |
| `test_scripts/transaction_inquiry_test.sh` | New test |
| `test_scripts/transaction_ordering_test.sh` | New test |
| `test_scripts/transaction_gc_guard_test.sh` | New test |

### Phase 2

| File | Changes |
|---|---|
| `dfs/metaserver/src/master.rs` | New rename flow, async commit, coordinator recovery simplification |
| `dfs/metaserver/src/simple_raft.rs` | New `CommitCrossShard` and `CommitTransactionWithOperation` commands |

## Limitations

- **Single-participant only**: `participant_acked` is a single `bool`, not per-participant. If the protocol extends to multi-participant transactions, this field must become a set. This is acceptable because rename is the only cross-shard operation, and it involves exactly two shards.
- **No cross-shard copy**: Copy is not a consistency concern (source unmodified), so it is excluded.
- **Raft batching/pipelining**: Separate optimization, orthogonal to this design.
- **Shard split/merge protocol**: Not affected by this change.
