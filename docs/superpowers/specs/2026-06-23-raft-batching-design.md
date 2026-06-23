# Raft Batching & Commit-Wait Design

## Problem

Two issues in the current Raft implementation:

1. **Safety**: `ClientRequest` returns `Ok()` immediately after appending to the leader's log, before Raft commit (majority replication). If the leader crashes before commit, the write is lost — but the client thinks it succeeded.

2. **Performance**: The leader executes one `db.put()` per `ClientRequest`. Under high throughput, disk I/O becomes the bottleneck. Current benchmark: ~470 ops/s sustained.

## Design

### 1. Commit-Wait (Safety Fix)

Change `ClientRequest` handling so the response is sent only after the entry is committed and applied.

**Current flow (unsafe):**
```
ClientRequest arrives
  → append to in-memory log
  → db.put() (single entry)
  → return Ok() to caller               ← IMMEDIATE, before commit
  → (background) send_heartbeats()
```

**New flow (safe):**
```
ClientRequest arrives
  → append to in-memory log
  → save to RocksDB (batched)
  → store reply_tx in pending_replies[log_index]
  → send_heartbeats()
  → (wait for majority ack)
  → commit_index advances
  → apply_logs()
  → pending_replies.remove(index).send(Ok(()))   ← AFTER COMMIT
```

**Implementation:**

Add to `RaftNode`:
```rust
pending_replies: HashMap<usize, oneshot::Sender<Result<(), Option<String>>>>,
```

- On `ClientRequest`: insert `reply_tx` into `pending_replies` keyed by the log entry's absolute index. Do NOT send the reply.
- In `apply_logs()`: after applying each entry, check if `pending_replies` has a sender for that index. If so, send `Ok(())`. If the sender was already dropped (client disconnected), `send()` returns `Err` which is silently discarded via `let _ =`. This is expected and harmless.
- On leader stepdown: drain all `pending_replies` AND `pending_read_indices` with `Err(Some(new_leader_hint))` so clients can retry on the new leader.

**Disk write failure rollback:** If `save_log_entries_batch()` fails during batched append, the in-memory log must be truncated back to its pre-batch length, and all `reply_tx` senders for the failed entries must be drained with errors. Without this, the in-memory log would diverge from the persisted log.

```rust
let pre_batch_len = self.log.len();
// append entries to self.log ...
if let Err(e) = self.save_log_entries_batch(&batch_entries) {
    // Rollback: truncate in-memory log
    self.log.truncate(pre_batch_len);
    // Drain reply senders with error
    for (idx, reply_tx) in failed_reply_txs {
        let _ = reply_tx.send(Err(None));
    }
    return Err(e);
}
```

**Leader stepdown helper**: Extract a `fn step_down_to_follower(&mut self, term: u64, leader_hint: Option<String>)` method that:
1. Sets `self.role = Role::Follower`
2. Updates term/voted_for as needed
3. Drains ALL `pending_replies` with `Err(leader_hint)`
4. Drains ALL `pending_read_indices` with `Err(leader_hint)` (existing bug fix — ReadIndex requests were also orphaned on stepdown)

Route ALL existing `self.role = Role::Follower` assignments through this helper. Exhaustive list of call sites (8 total):

| # | Line | Context |
|---|------|---------|
| 1 | ~1785 | RequestVote handler — higher term |
| 2 | ~1867 | RequestVoteResponse handler — higher term in reply |
| 3 | ~1893 | AppendEntries handler — accepting AE from valid leader |
| 4 | ~2171 | AppendEntriesResponse handler — higher term in reply |
| 5 | ~2193 | InstallSnapshot handler — accepting snapshot from leader |
| 6 | ~2269 | InstallSnapshotResponse handler — higher term in reply |
| 7 | ~2297 | TimeoutNow handler — higher term |
| 8 | ~2715 | initiate_leader_transfer — voluntary stepdown |

**Single-node special case**: For single-node clusters (no peers), the current behavior of immediate commit + apply is preserved. The entry is committed immediately since the leader is the only voter, and `apply_logs()` triggers the pending reply.

### 2. Leader-Side Log Batching (Performance)

Batch multiple `ClientRequest` events into a single disk write and a single `send_heartbeats()` call.

**Current event loop:**
```rust
Some(event) = self.inbox.recv() => {
    self.handle_event(event).await?;
}
```

**New event loop:**
```rust
Some(event) = self.inbox.recv() => {
    let mut events = vec![event];
    // Drain queued events without blocking, cap at 256 to prevent starvation
    while events.len() < 256 {
        match self.inbox.try_recv() {
            Ok(more) => events.push(more),
            Err(_) => break,
        }
    }
    self.handle_event_batch(events).await?;
}
```

**`handle_event_batch` logic:**

1. Separate events into ClientRequests and non-ClientRequests
2. **Process non-ClientRequests first**, one-by-one using existing `handle_event()`. This is critical: if an RPC response triggers a term bump (stepdown to follower), subsequent ClientRequests in the same batch must be rejected, not appended as leader. Processing RPCs first ensures the node's role is up-to-date before handling client writes.
3. For ClientRequests (if still leader after step 2):
   a. Record `pre_batch_len = self.log.len()`
   b. Append all entries to in-memory log
   c. Write all entries in one `save_log_entries_batch()` call — if fails, rollback (truncate log, drain reply senders)
   d. Store all `reply_tx` in `pending_replies`
   e. Call `send_heartbeats()` once (sends all new entries to followers in one RPC per peer)
   f. For single-node: immediately commit + apply (triggers pending replies)
4. For ClientRequests (if not leader): reject each with leader hint, same as current behavior

Note: If a batch contains both `GetReadIndex` and `ClientRequest` events, the ReadIndex is processed first (step 2) which may trigger its own `send_heartbeats()`. The subsequent `send_heartbeats()` in step 3e is redundant but harmless (followers handle duplicate AppendEntries idempotently).

This means N concurrent client requests that arrive within one event loop iteration share:
- 1 disk write (instead of N)
- 1 round of AppendEntries RPCs (instead of N)

### 3. Leader Stepdown Handling

When the leader loses leadership, all pending replies and read indices must be drained:

```rust
fn step_down_to_follower(&mut self, term: u64, leader_hint: Option<String>) {
    self.role = Role::Follower;
    if term > self.current_term {
        self.current_term = term;
        self.voted_for = None;
        self.save_state();
    }
    // Drain pending write replies
    for (_, reply_tx) in self.pending_replies.drain() {
        let _ = reply_tx.send(Err(leader_hint.clone()));
    }
    // Drain pending read index replies (reply_tx type: Sender<Result<usize, Option<String>>>)
    for req in self.pending_read_indices.drain(..) {
        let _ = req.reply_tx.send(Err(leader_hint.clone()));
    }
    if let Some(hint) = &leader_hint {
        self.current_leader_address = Some(hint.clone());
    }
    // Note: callers at AppendEntries (site 3) and InstallSnapshot (site 5) must also
    // set self.current_leader and reset self.last_election_time after calling this helper.
    // save_state() should be implemented as self.save_term() + self.save_vote(), or
    // errors should be logged (these are best-effort persistence of volatile state).
}
```

### 4. Performance Expectations

| Metric | Before (unsafe, no commit-wait) | After (safe, commit-wait + batch) |
|---|---|---|
| Write ops/s (concurrency=5) | 470 | 1500-3000 |
| Write ops/s (concurrency=1) | 470 (fake) | ~100-125 (real Raft round-trip) |
| Avg latency | 10ms (fake: no commit-wait) | 7-12ms (real: with commit-wait) |
| P99 latency | 26ms | 20-30ms |
| Disk writes/s | 470 (1 per request) | 50-100 (batched) |

**Important**: The "before" numbers are not comparable with "after" — "before" latency excluded Raft commit time (unsafe), "after" includes it (safe and correct). Single-client throughput will regress because the old implementation returned before commit. This is the correct tradeoff: safety over artificial performance.

The real improvement is in **throughput under concurrent load**: batching reduces disk I/O by 5-10x when multiple requests are in-flight.

## Files Changed

| File | Changes |
|---|---|
| `dfs/metaserver/src/simple_raft.rs` | `pending_replies` field, `step_down_to_follower` helper (replaces 8 `role = Follower` sites), event loop batch drain, `handle_event_batch`, commit-wait in `apply_logs`, disk write rollback |

No changes to `master.rs`, `config_server.rs`, `client/`, or `proto/`. This is purely a Raft-internal change.

## Testing

1. All existing Raft unit tests (33) must pass
2. All integration tests (22) must pass
3. Linearizability tests (7 scenarios) must pass — critical since we're changing when replies are sent
4. Benchmark before/after comparison using `dfs_cli benchmark stress-write`
5. Single-client benchmark to verify latency is reasonable (~7-12ms per write)

## Risks

- **Latency increase for single-request workloads**: A single request with no concurrent traffic incurs full Raft round-trip latency (~7-12ms). This is correct behavior — the "before" latency was artificially low because it skipped commit-wait.
- **Pending replies memory**: If the leader has many in-flight requests and commit is slow (e.g., follower is down), `pending_replies` can grow. Mitigated by Raft's existing leader lease timeout — if commit is stuck, the leader steps down and drains all replies with errors.
- **Batch size bound**: The `try_recv()` drain is capped at 256 events per batch to prevent event loop starvation and oversized AppendEntries RPCs.

## Out of Scope

- Raft pipelining (sending next batch before previous batch is acked) — can be added later as a separate optimization
- Read batching (batching ReadIndex requests) — reads already return from local state after commit verification
- WAL separate from RocksDB — RocksDB's internal WAL is sufficient
- Prometheus metrics for batch size / commit-wait latency — useful but not required for correctness
