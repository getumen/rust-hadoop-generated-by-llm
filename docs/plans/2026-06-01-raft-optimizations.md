# Raft Optimizations Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Reduce write latency and Disk I/O in the Raft implementation via two targeted changes.

**Architecture:**
1. **Event-driven triggering**: After `ClientRequest` appends to the Leader's log, immediately call `send_heartbeats()` instead of waiting up to 100ms for the next tick.
2. **Follower-side WriteBatch**: When a Follower receives AppendEntries with N entries, write them all to RocksDB in a single `WriteBatch` instead of N individual `put()` calls.

**Tech Stack:** RocksDB `WriteBatch` (already in rocksdb crate), existing `send_heartbeats()` method.

---

### Task 1: Add `save_log_entries_batch` and use it in AppendEntries handler

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs`

**Step 1: Read the file to find exact line numbers for:**
- `save_log_entry` function (around line 896)
- The AppendEntries follower handler loop where entries are written one-by-one (around lines 1880-1893 and 1930-1940)

**Step 2: Add `save_log_entries_batch` method** after `save_log_entry` (around line 903):

```rust
/// Write multiple log entries to RocksDB in a single atomic WriteBatch.
fn save_log_entries_batch(&self, entries: &[(usize, &LogEntry)]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut batch = rocksdb::WriteBatch::default();
    for (index, entry) in entries {
        let key = format!("log:{}", index);
        let val = serde_json::to_vec(entry).context("Failed to serialize log entry")?;
        batch.put(key.as_bytes(), val);
    }
    self.db
        .write(batch)
        .context("Failed to write log entries batch to DB")?;
    Ok(())
}
```

**Step 3: Replace per-entry writes in the AppendEntries handler**

The follower AppendEntries handler pushes entries to `self.log` one by one and calls `save_log_entry` per entry. There are two such loops. Find them by searching for `self.save_log_entry(absolute_index, entry)?;` — there are multiple occurrences in the AppendEntries handler.

The pattern to find and replace is in `handle_rpc` inside the AppendEntries branch. After the loop that pushes entries to `self.log`, replace the individual `save_log_entry` calls with a batch call.

The cleanest way: collect `(absolute_index, &entry)` pairs during the loop into a `Vec`, then call `save_log_entries_batch` once after the loop.

The existing code in the AppendEntries handler (around lines 1880-1893) looks like:
```rust
for entry in &args.entries {
    // ... conflict detection ...
    self.log.push(entry.clone());
    self.save_log_entry(absolute_index, entry)?;
    // ... absolute_index increments ...
}
```

Replace with pattern:
```rust
let mut batch_entries: Vec<(usize, &LogEntry)> = Vec::new();
for entry in &args.entries {
    // ... same conflict detection and self.log.push(entry.clone()) ...
    batch_entries.push((absolute_index, entry));
    // ... absolute_index increments ... (remove save_log_entry call)
}
self.save_log_entries_batch(&batch_entries)?;
```

**IMPORTANT**: Read the actual code carefully before editing. There are two code paths in the AppendEntries handler:
1. When `args.prev_log_index == self.last_included_index` (snapshot boundary case, around line 1870)
2. Normal case (around line 1902)

Both need the same treatment.

**Step 4: Build and test**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```

Expected: clean build, all 19 tests pass.

**Step 5: Commit**

```bash
git add dfs/metaserver/src/simple_raft.rs
git commit -m "perf: batch RocksDB writes in AppendEntries handler (WriteBatch)"
```

---

### Task 2: Immediate replication after ClientRequest (event-driven pipelining)

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs`

**Step 1: Find the `ClientRequest` handler**

In `handle_event`, find the `Event::ClientRequest` branch (around line 1606). After `self.save_log_entry(absolute_index, &entry)?;` (line 1628) and the single-node commit check, there is a comment saying "The original code..." and then `let _ = reply_tx.send(Ok(()));`.

**Step 2: Add immediate `send_heartbeats()` call for multi-node clusters**

After `self.save_log_entry(absolute_index, &entry)?;` and the single-node check, add — BEFORE `reply_tx.send(Ok(()))`:

```rust
// Pipeline: immediately replicate to followers without waiting for next tick.
// For multi-node clusters this reduces write latency from up to 100ms to ~0ms.
if !self.peers.is_empty() {
    if let Err(e) = self.send_heartbeats().await {
        tracing::warn!("Immediate replication after ClientRequest failed: {}", e);
    }
}
```

Place this AFTER the single-node commit block and BEFORE `let _ = reply_tx.send(Ok(()));`.

**Step 3: Build and test**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```

Expected: clean build, all 19 tests pass.

**Step 4: Commit**

```bash
git add dfs/metaserver/src/simple_raft.rs
git commit -m "perf: immediately replicate to followers after ClientRequest (pipeline)"
```

---

### Task 3: Update TODO.md

**Step 1: Mark Raft Optimizations complete**

In `TODO.md`, replace:
```
- [ ] **Raft Optimizations (Batching & Pipelining)**
    - [ ] `simple_raft.rs` の `AppendEntries` をバッチ化し、1回のDisk I/Oで複数ログを処理。
    - [ ] コミット応答を待たずに次のログを先行送信するパイプライニングの実装。
```

With:
```
- [x] **Raft Optimizations (Batching & Pipelining)** ✅
    - [x] `simple_raft.rs` の `AppendEntries` をバッチ化し、1回のDisk I/Oで複数ログを処理。
    - [x] コミット応答を待たずに次のログを先行送信するパイプライニングの実装。
```

**Step 2: Build + full unit tests**

```bash
cargo build 2>&1 | tail -5
cargo test --lib 2>&1 | tail -5
```

**Step 3: Commit**

```bash
git add TODO.md
git commit -m "docs: mark Raft Optimizations as complete in TODO.md"
```
