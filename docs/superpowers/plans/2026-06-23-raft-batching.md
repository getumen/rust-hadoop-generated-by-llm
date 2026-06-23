# Raft Batching & Commit-Wait Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the safety bug where ClientRequest replies before Raft commit, and optimize throughput via leader-side log batching.

**Architecture:** Add `pending_replies` HashMap to defer client responses until commit. Extract `step_down_to_follower()` helper for all 8 role transition sites. Change event loop from one-at-a-time to batch drain with `try_recv()`. All changes in `simple_raft.rs` only.

**Tech Stack:** Rust, tokio, RocksDB, oneshot channels

**Spec:** `docs/superpowers/specs/2026-06-23-raft-batching-design.md`

**Baseline commands:**
```bash
cargo build --release 2>&1 | tail -3
cargo clippy -- -D warnings 2>&1 | tail -3
cargo test --lib -p dfs-metaserver 2>&1 | tail -5
```

**Baseline benchmark (before changes):**
```
Stress Write (30s, 10KB, concurrency=5): 470 ops/s, 10ms avg latency
```

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `dfs/metaserver/src/simple_raft.rs` | Modify | All changes: pending_replies, step_down_to_follower, event batch drain, handle_event_batch, commit-wait in apply_logs |

---

### Task 1: Add `pending_replies` field and `step_down_to_follower` helper

This task extracts the stepdown logic into a single helper method that drains both `pending_replies` and `pending_read_indices`. This is the foundation for commit-wait.

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs`

- [ ] **Step 1: Add `pending_replies` field to RaftNode**

In `RaftNode` struct (line 568), add after `pending_read_indices` (line 624):

```rust
pub pending_replies: HashMap<usize, tokio::sync::oneshot::Sender<Result<(), Option<String>>>>,
```

Add `HashMap` import if not present (it is — `std::collections::HashMap` is already imported).

Initialize in `RaftNode::new()` (around line 764, near `pending_read_indices: vec![]`):

```rust
pending_replies: HashMap::new(),
```

- [ ] **Step 2: Create `step_down_to_follower` method**

Add a new method to `RaftNode` (before `handle_event`):

```rust
/// Centralized leader-to-follower transition. Drains all pending client
/// replies and read-index requests so callers get a redirect error.
fn step_down_to_follower(&mut self, term: u64, leader_hint: Option<String>) {
    let was_leader = self.role == Role::Leader;
    self.role = Role::Follower;
    if term > self.current_term {
        self.current_term = term;
        self.voted_for = None;
        if let Err(e) = self.save_term() {
            tracing::error!("Failed to save term on stepdown: {}", e);
        }
        if let Err(e) = self.save_vote() {
            tracing::error!("Failed to save vote on stepdown: {}", e);
        }
    }
    if let Some(hint) = &leader_hint {
        self.current_leader_address = Some(hint.clone());
    }
    // Drain pending write replies (only exist if we were leader)
    if was_leader {
        for (_, reply_tx) in self.pending_replies.drain() {
            let _ = reply_tx.send(Err(leader_hint.clone()));
        }
        // Drain pending read-index replies
        for req in self.pending_read_indices.drain(..) {
            let _ = req.reply_tx.send(Err(leader_hint.clone()));
        }
    }
}
```

- [ ] **Step 3: Replace all 8 stepdown sites**

Replace each `self.role = Role::Follower;` + surrounding term/vote logic with a call to `self.step_down_to_follower(term, leader_hint)`.

**Site 1 (line ~1785)**: RequestVote handler — higher term
Read the surrounding code. It currently does:
```rust
self.current_term = args.term;
self.role = Role::Follower;
self.voted_for = None;
self.save_term()?;
self.save_vote()?;
```
Replace with:
```rust
self.step_down_to_follower(args.term, None);
```

**Site 2 (line ~1867)**: RequestVoteResponse — higher term
Similar pattern. Replace role+term+vote block with:
```rust
self.step_down_to_follower(reply.term, None);
```

**Site 3 (line ~1893)**: AppendEntries handler — valid leader
This one also sets `self.current_leader` and `self.last_election_time`. Keep those AFTER the helper call:
```rust
self.step_down_to_follower(args.term, Some(/* leader address from peers */));
self.current_leader = Some(args.leader_id);
self.last_election_time = Instant::now();
```

**Site 4 (line ~2171)**: AppendEntriesResponse — higher term
```rust
self.step_down_to_follower(reply.term, None);
```

**Site 5 (line ~2193)**: InstallSnapshot handler — higher term
Also sets `current_leader` and `last_election_time`:
```rust
self.step_down_to_follower(args.term, Some(/* leader address */));
self.current_leader = Some(args.leader_id);
self.last_election_time = Instant::now();
```

**Site 6 (line ~2269)**: InstallSnapshotResponse — higher term
```rust
self.step_down_to_follower(reply.term, None);
```

**Site 7 (line ~2297)**: TimeoutNow handler — higher term
```rust
self.step_down_to_follower(args.term, None);
```

**Site 8 (line ~2715)**: initiate_leader_transfer — voluntary
```rust
self.step_down_to_follower(self.current_term, None);
```

For each site, carefully read the surrounding code to preserve any logic that should happen AFTER the stepdown (like setting `current_leader`, `last_election_time`, or proceeding with vote grant).

- [ ] **Step 4: Build and test**

```bash
cargo build --release 2>&1 | tail -5
cargo clippy -- -D warnings 2>&1 | tail -5
cargo fmt --all
cargo test --lib -p dfs-metaserver 2>&1 | tail -10
```

All 33 tests must pass. The behavior hasn't changed yet — replies still fire immediately.

- [ ] **Step 5: Commit**

```bash
git add dfs/metaserver/src/simple_raft.rs
git commit -m "refactor(raft): extract step_down_to_follower helper for all 8 role transitions

Centralizes leader-to-follower transitions through a single method that
drains pending_replies and pending_read_indices. Foundation for commit-wait.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: Implement commit-wait (defer ClientRequest replies)

Change `ClientRequest` handling to store `reply_tx` in `pending_replies` instead of sending immediately. Replies are sent when `apply_logs()` processes the committed entry.

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs`

- [ ] **Step 1: Modify ClientRequest handler to defer reply**

Find `Event::ClientRequest` handler (line ~1653). Currently it does:
```rust
// ... append to log, save, send_heartbeats ...
let _ = reply_tx.send(Ok(()));  // line ~1701
```

Replace the immediate `reply_tx.send(Ok(()))` with storing in pending_replies:

```rust
// Store reply for when entry is committed and applied
let absolute_index = self.last_included_index + self.log.len() - 1;
self.pending_replies.insert(absolute_index, reply_tx);
```

**Keep the single-node immediate commit path** (around line 1683-1686). After `self.apply_logs()`, the pending reply will be sent by apply_logs (see Step 2). So no change needed for single-node — it just works.

**Remove the immediate `reply_tx.send(Ok(()))` call** that currently exists after `save_log_entry` / `send_heartbeats`.

Also remove or update the existing `reply_tx.send(Err(...))` for the not-leader case — that should stay as-is (immediate rejection).

- [ ] **Step 2: Modify apply_logs to send pending replies**

Find `apply_logs()` (line ~2329). After applying each entry, drain the corresponding pending reply:

```rust
fn apply_logs(&mut self) {
    while self.commit_index > self.last_applied {
        self.last_applied += 1;
        let log_index = self.last_applied - self.last_included_index;
        if log_index < self.log.len() {
            let command = self.log[log_index].command.clone();
            // Apply the command (existing logic)
            // ...
        }
        // Send pending reply if we have one for this index
        if let Some(reply_tx) = self.pending_replies.remove(&self.last_applied) {
            let _ = reply_tx.send(Ok(()));
        }
    }
}
```

The key: use `self.last_applied` (absolute index) as the HashMap key, matching what was stored in Step 1.

- [ ] **Step 3: Build and test**

```bash
cargo build --release 2>&1 | tail -5
cargo clippy -- -D warnings 2>&1 | tail -5
cargo fmt --all
cargo test --lib -p dfs-metaserver 2>&1 | tail -10
```

All tests must pass. Single-node tests should still work because single-node immediately commits.

- [ ] **Step 4: Commit**

```bash
git add dfs/metaserver/src/simple_raft.rs
git commit -m "fix(raft): defer ClientRequest reply until after Raft commit

ClientRequest reply_tx is now stored in pending_replies and sent only
after the entry is committed and applied via apply_logs(). This fixes
the safety bug where clients received success before majority replication.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: Implement event loop batch drain

Change the event loop from processing one event at a time to draining a batch, then processing non-ClientRequests first and ClientRequests as a batched group.

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs`

- [ ] **Step 1: Create `handle_event_batch` method**

Add a new method:

```rust
async fn handle_event_batch(&mut self, events: Vec<Event>) -> Result<()> {
    // Separate into client requests and other events
    let mut client_requests = Vec::new();
    let mut other_events = Vec::new();
    
    for event in events {
        match event {
            Event::ClientRequest { .. } => client_requests.push(event),
            other => other_events.push(other),
        }
    }
    
    // Process non-ClientRequests first (may trigger stepdown)
    for event in other_events {
        self.handle_event(event).await?;
    }
    
    // Process ClientRequests as a batch (if still leader)
    if client_requests.is_empty() {
        return Ok(());
    }
    
    if self.role != Role::Leader {
        // Not leader — reject all with leader hint
        let hint = self.current_leader_address.clone();
        for event in client_requests {
            if let Event::ClientRequest { reply_tx, .. } = event {
                let _ = reply_tx.send(Err(hint.clone()));
            }
        }
        return Ok(());
    }
    
    // Leader: batch append all entries
    let pre_batch_len = self.log.len();
    let mut batch_entries = Vec::new();
    let mut batch_reply_txs = Vec::new();
    
    for event in client_requests {
        if let Event::ClientRequest { command, reply_tx } = event {
            let entry = LogEntry {
                term: self.current_term,
                command: command.clone(),
            };
            self.log.push(entry.clone());
            let absolute_index = self.last_included_index + self.log.len() - 1;
            batch_entries.push((absolute_index, entry));
            batch_reply_txs.push((absolute_index, reply_tx));
        }
    }
    
    // Batch disk write
    let entries_ref: Vec<(usize, &LogEntry)> = batch_entries.iter()
        .map(|(idx, entry)| (*idx, entry))
        .collect();
    
    if let Err(e) = self.save_log_entries_batch(&entries_ref) {
        // Rollback: truncate in-memory log
        tracing::error!("Failed to persist batch of {} entries: {}", batch_entries.len(), e);
        self.log.truncate(pre_batch_len);
        for (_, reply_tx) in batch_reply_txs {
            let _ = reply_tx.send(Err(None));
        }
        return Err(e);
    }
    
    // Store reply senders
    for (idx, reply_tx) in batch_reply_txs {
        self.pending_replies.insert(idx, reply_tx);
    }
    
    // Single-node: immediate commit
    if self.peers.is_empty() {
        let last_index = self.last_included_index + self.log.len() - 1;
        if last_index > self.commit_index {
            self.commit_index = last_index;
            self.apply_logs();
        }
    }
    
    // Send heartbeats once for the whole batch
    self.send_heartbeats().await?;
    
    Ok(())
}
```

- [ ] **Step 2: Change the event loop to use batch drain**

Find the event loop in `run()` (line ~1156). Change:

```rust
// Before
Some(event) = self.inbox.recv() => {
    if let Err(e) = self.handle_event(event).await {
        // ...
    }
}

// After
Some(event) = self.inbox.recv() => {
    let mut events = vec![event];
    while events.len() < 256 {
        match self.inbox.try_recv() {
            Ok(more) => events.push(more),
            Err(_) => break,
        }
    }
    if let Err(e) = self.handle_event_batch(events).await {
        // ... same error handling as before
    }
}
```

- [ ] **Step 3: Remove duplicate ClientRequest logic from handle_event**

The existing `handle_event` still has the old `Event::ClientRequest` handler. It should now delegate single ClientRequests through the batch path. The simplest approach: keep `handle_event` for non-ClientRequest events only, and have the batch path handle all ClientRequests. Add a guard in `handle_event`:

```rust
Event::ClientRequest { .. } => {
    // Should not be called directly — routed through handle_event_batch
    unreachable!("ClientRequest should be handled via handle_event_batch");
}
```

Or simpler: just handle it the old way (for any edge case where handle_event is called directly outside the batch path). Leave the existing code but change it to use pending_replies (already done in Task 2).

Actually, the safest approach: leave `handle_event`'s ClientRequest handler intact (it already uses pending_replies from Task 2). The `handle_event_batch` method only handles the BATCHED case. If a single ClientRequest comes through outside a batch (shouldn't happen, but defensive), it still works.

- [ ] **Step 4: Build and test**

```bash
cargo build --release 2>&1 | tail -5
cargo clippy -- -D warnings 2>&1 | tail -5
cargo fmt --all
cargo test --lib -p dfs-metaserver 2>&1 | tail -10
```

All tests must pass.

- [ ] **Step 5: Commit**

```bash
git add dfs/metaserver/src/simple_raft.rs
git commit -m "feat(raft): implement event loop batch drain and batched log persistence

Multiple ClientRequests arriving within one event loop iteration now share
a single RocksDB WriteBatch and a single round of AppendEntries RPCs.
Non-ClientRequest events are processed first to handle potential stepdowns.
Batch size capped at 256 to prevent event loop starvation.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: Run full test suite and benchmark

- [ ] **Step 1: Run full unit tests**

```bash
cargo test 2>&1 | grep "^test result"
```

Expected: All test suites pass.

- [ ] **Step 2: Run clippy and format**

```bash
cargo clippy -- -D warnings && cargo fmt --all -- --check
```

- [ ] **Step 3: Docker integration tests**

```bash
docker compose build && bash run_all_tests.sh
```

All 22+ tests must pass, including linearizability tests.

- [ ] **Step 4: Benchmark (after)**

```bash
docker compose up -d && sleep 20
docker exec dfs-master1-shard1 /app/dfs_cli --config-servers http://config-server:50050 benchmark stress-write --duration 30 --size 10240 --concurrency 5
docker compose down -v
```

Compare with baseline: expect throughput increase from ~470 to 1000+ ops/s under concurrency=5.

- [ ] **Step 5: Push and create PR**

```bash
git push -u origin feature/raft-batching
gh pr create --title "feat: Raft commit-wait + leader-side log batching" --body "..."
```
