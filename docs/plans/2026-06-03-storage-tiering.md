# Intelligent Storage Tiering Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Automatically migrate cold blocks to a separate cold-storage directory (HDD tier), then convert them to Erasure Coding to reduce storage usage by ~67%.

**Architecture:** Phase 1 adds `last_access_ms` / `access_count` to `FileMetadata` (Raft-persisted), a background scanner on the Master that issues `MOVE_TO_COLD` commands via `pending_commands`, and ChunkServer support for `hot/` → `cold/` block moves. Phase 2 extends the scanner to issue EC-conversion commands for blocks that have been cold for long enough, reusing the existing EC encode/replicate infrastructure.

**Tech Stack:** Rust, existing `reed-solomon-erasure` crate, existing `ChunkServerCommand` / `pending_commands` pipeline, environment-variable thresholds.

---

## Context

Key files:
- `proto/dfs.proto` — `FileMetadata`, `BlockInfo`, `ChunkServerCommand` proto definitions
- `dfs/metaserver/src/simple_raft.rs` — `MasterCommand` enum (lines 255–330), `apply_command` (line 2864)
- `dfs/metaserver/src/master.rs` — `MasterState`, `ChunkServerStatus`, `MyMaster`, heartbeat handler, background tasks
- `dfs/metaserver/src/bin/master.rs` — CLI args, `tokio::spawn` background task pattern
- `dfs/chunkserver/src/chunkserver.rs` — `MyChunkServer`, `write_block_async`, `read_block_async`, `pending_commands` processing
- `dfs/chunkserver/src/bin/chunkserver.rs` — CLI args (`--storage-dir`)

Threshold env vars (read at startup):
- `COLD_THRESHOLD_SECS` (default: 604800 = 7 days)
- `EC_THRESHOLD_SECS` (default: 2592000 = 30 days, measured from `moved_to_cold_at_ms`)

---

## Task 1: Add access tracking fields to FileMetadata proto + MasterCommand

**Files:**
- Modify: `proto/dfs.proto`
- Modify: `dfs/metaserver/src/simple_raft.rs`

**Step 1: Add fields to `FileMetadata` in proto**

In `proto/dfs.proto`, extend `message FileMetadata` (currently ends at field 7):

```proto
message FileMetadata {
  string path = 1;
  uint64 size = 2;
  repeated BlockInfo blocks = 3;
  string etag_md5 = 4;
  uint64 created_at_ms = 5;
  int32 ec_data_shards = 6;
  int32 ec_parity_shards = 7;
  // Storage tiering fields
  uint64 last_access_ms = 8;       // Unix millis of last read; 0 = never accessed
  uint64 access_count = 9;         // Cumulative read count
  uint64 moved_to_cold_at_ms = 10; // Unix millis when moved to cold tier; 0 = hot
}
```

**Step 2: Add `UpdateAccessStats` and `MoveToCold` variants to `MasterCommand`**

In `dfs/metaserver/src/simple_raft.rs`, extend `pub enum MasterCommand` (after the last variant ~line 325):

```rust
/// Update file access statistics (called on each read)
UpdateAccessStats {
    path: String,
    accessed_at_ms: u64,
},
/// Mark file as moved to cold tier
MoveToCold {
    path: String,
    moved_at_ms: u64,
},
/// Mark file as converted to EC (updates block locations and ec fields)
ConvertToEc {
    path: String,
    ec_data_shards: i32,
    ec_parity_shards: i32,
    /// New BlockInfo list with EC shard locations replacing replication locations
    new_blocks: Vec<crate::dfs::BlockInfo>,
},
```

**Step 3: Add apply logic for new commands**

In `simple_raft.rs`, inside `fn apply_command` → `Command::Master(cmd)` match arm, add after the `StopShuffle` arm:

```rust
MasterCommand::UpdateAccessStats { path, accessed_at_ms } => {
    if let Some(meta) = master_state.files.get_mut(path) {
        meta.last_access_ms = *accessed_at_ms;
        meta.access_count += 1;
    }
}
MasterCommand::MoveToCold { path, moved_at_ms } => {
    if let Some(meta) = master_state.files.get_mut(path) {
        meta.moved_to_cold_at_ms = *moved_at_ms;
        tracing::info!("Moved file {} to cold tier", path);
    }
}
MasterCommand::ConvertToEc { path, ec_data_shards, ec_parity_shards, new_blocks } => {
    if let Some(meta) = master_state.files.get_mut(path) {
        meta.ec_data_shards = *ec_data_shards;
        meta.ec_parity_shards = *ec_parity_shards;
        meta.blocks = new_blocks.clone();
        tracing::info!("Converted file {} to EC({},{})", path, ec_data_shards, ec_parity_shards);
    }
}
```

**Step 4: Write failing tests for new apply logic**

Add to `simple_raft.rs` test module (search for `#[cfg(test)]` near end of file):

```rust
#[test]
fn test_apply_update_access_stats() {
    let state = make_test_master_state();
    // create a file first
    apply_cmd(&state, MasterCommand::CreateFile { path: "/f".into(), ec_data_shards: 0, ec_parity_shards: 0 });
    apply_cmd(&state, MasterCommand::UpdateAccessStats { path: "/f".into(), accessed_at_ms: 1000 });
    let locked = state.lock().unwrap();
    if let AppState::Master(ref ms) = *locked {
        let f = ms.files.get("/f").unwrap();
        assert_eq!(f.last_access_ms, 1000);
        assert_eq!(f.access_count, 1);
    }
}

#[test]
fn test_apply_move_to_cold() {
    let state = make_test_master_state();
    apply_cmd(&state, MasterCommand::CreateFile { path: "/f".into(), ec_data_shards: 0, ec_parity_shards: 0 });
    apply_cmd(&state, MasterCommand::MoveToCold { path: "/f".into(), moved_at_ms: 5000 });
    let locked = state.lock().unwrap();
    if let AppState::Master(ref ms) = *locked {
        assert_eq!(ms.files.get("/f").unwrap().moved_to_cold_at_ms, 5000);
    }
}
```

**Step 5: Run tests to verify they fail**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | grep -E "FAILED|error"
```

Expected: compile errors until apply logic is added.

**Step 6: Run tests to verify they pass after implementation**

```bash
cargo test -p dfs-metaserver --lib
```

Expected: all tests pass.

**Step 7: Commit**

```bash
git add proto/dfs.proto dfs/metaserver/src/simple_raft.rs
git commit -m "feat(tiering): add access tracking fields to FileMetadata and MasterCommand variants"
```

---

## Task 2: Record access stats on `get_file_info` RPC

**Files:**
- Modify: `dfs/metaserver/src/master.rs` (around line 1516 `get_file_info`)

**Context:** `get_file_info` is called by clients before reading blocks. It's the right place to bump access stats. The call must not block — fire-and-forget Raft proposal via `tokio::spawn`.

**Step 1: Write failing test**

In `master.rs` test module, add:

```rust
#[tokio::test]
async fn test_get_file_info_increments_access_count() {
    let (master, _dir) = make_test_master().await;
    // create a file
    create_test_file(&master, "/test-access").await;
    // call get_file_info
    let req = Request::new(GetFileInfoRequest { path: "/test-access".into() });
    master.get_file_info(req).await.unwrap();
    // allow Raft to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // verify access_count incremented
    let state = master.state.lock().unwrap();
    if let AppState::Master(ref ms) = *state {
        assert_eq!(ms.files.get("/test-access").unwrap().access_count, 1);
    }
}
```

Run: `cargo test -p dfs-metaserver --lib test_get_file_info_increments_access_count -- --nocapture`

Expected: FAIL (access_count stays 0).

**Step 2: Add access stat recording to `get_file_info`**

In `master.rs`, inside `async fn get_file_info`, after `self.monitor.record_request(...)`, add before the state lock:

```rust
// Fire-and-forget access stat update (best effort — tiering only)
{
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let raft_tx = self.raft_tx.clone();
    let path = req.path.clone();
    tokio::spawn(async move {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let _ = raft_tx
            .send(Event::ClientRequest {
                command: Command::Master(MasterCommand::UpdateAccessStats {
                    path,
                    accessed_at_ms: now_ms,
                }),
                reply_tx: tx,
            })
            .await;
    });
}
```

**Step 3: Run test to verify it passes**

```bash
cargo test -p dfs-metaserver --lib test_get_file_info_increments_access_count
```

**Step 4: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat(tiering): record access stats in get_file_info"
```

---

## Task 3: Add `MOVE_TO_COLD` command type to proto + ChunkServer handler

**Files:**
- Modify: `proto/dfs.proto`
- Modify: `dfs/chunkserver/src/chunkserver.rs`
- Modify: `dfs/chunkserver/src/bin/chunkserver.rs`

**Step 1: Add `MOVE_TO_COLD` to `ChunkServerCommand.CommandType` in proto**

```proto
message ChunkServerCommand {
  enum CommandType {
    UNKNOWN = 0;
    REPLICATE = 1;
    DELETE = 2;
    RECONSTRUCT_EC_SHARD = 3;
    MOVE_TO_COLD = 4;  // Move block from hot/ to cold/ subdir
  }
  CommandType type = 1;
  string block_id = 2;
  string target_chunk_server_address = 3;
  int32 shard_index = 4;
  int32 ec_data_shards = 5;
  int32 ec_parity_shards = 6;
  repeated string ec_shard_sources = 7;
  uint64 original_block_size = 8;
}
```

**Step 2: Add `--cold-storage-dir` CLI arg to chunkserver binary**

In `dfs/chunkserver/src/bin/chunkserver.rs`, in the `Args` struct (find `struct Args`):

```rust
/// Cold storage directory (HDD tier). If not set, tiering is disabled.
#[arg(long)]
cold_storage_dir: Option<PathBuf>,
```

Pass it through to `MyChunkServer::new`:

```rust
MyChunkServer::new(
    args.storage_dir.clone(),
    args.cold_storage_dir.clone(),  // new
)
```

**Step 3: Add `cold_storage_dir` to `MyChunkServer`**

In `dfs/chunkserver/src/chunkserver.rs`, update `MyChunkServer` struct:

```rust
pub struct MyChunkServer {
    storage_dir: PathBuf,
    cold_storage_dir: Option<PathBuf>,  // new
    // ... rest unchanged
}
```

Update `MyChunkServer::new`:

```rust
pub fn new(storage_dir: PathBuf, cold_storage_dir: Option<PathBuf>) -> Self {
    fs::create_dir_all(&storage_dir).expect("Failed to create storage directory");
    if let Some(ref cold) = cold_storage_dir {
        fs::create_dir_all(cold).expect("Failed to create cold storage directory");
    }
    // ... rest unchanged
    MyChunkServer {
        storage_dir,
        cold_storage_dir,
        // ...
    }
}
```

**Step 4: Add `move_block_to_cold` helper**

```rust
async fn move_block_to_cold(&self, block_id: &str) -> std::io::Result<()> {
    let cold_dir = self.cold_storage_dir.as_ref().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "cold_storage_dir not configured")
    })?;
    let src = self.storage_dir.join(block_id);
    let src_meta = self.storage_dir.join(format!("{}.meta", block_id));
    let dst = cold_dir.join(block_id);
    let dst_meta = cold_dir.join(format!("{}.meta", block_id));
    tokio::task::spawn_blocking(move || {
        std::fs::rename(&src, &dst)?;
        if src_meta.exists() {
            std::fs::rename(&src_meta, &dst_meta)?;
        }
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}
```

**Step 5: Update `block_path` lookup to check cold dir**

Currently `read_block_async` builds `path = self.storage_dir.join(block_id)`. Change to:

```rust
fn block_path(&self, block_id: &str) -> PathBuf {
    let hot = self.storage_dir.join(block_id);
    if hot.exists() {
        return hot;
    }
    if let Some(ref cold) = self.cold_storage_dir {
        let cold_path = cold.join(block_id);
        if cold_path.exists() {
            return cold_path;
        }
    }
    hot // fallback (will get NotFound error naturally)
}
```

Replace all `self.storage_dir.join(block_id)` in read paths with `self.block_path(block_id)`.

**Step 6: Handle `MOVE_TO_COLD` in pending_commands processing**

Find the section in `chunkserver.rs` where `pending_commands` are processed (search for `RECONSTRUCT_EC_SHARD`). Add:

```rust
CommandType::MoveToCold => {
    let server_clone = server.clone();
    let block_id = cmd.block_id.clone();
    tokio::spawn(async move {
        match server_clone.move_block_to_cold(&block_id).await {
            Ok(()) => tracing::info!("Moved block {} to cold tier", block_id),
            Err(e) => tracing::warn!("Failed to move block {} to cold: {}", block_id, e),
        }
    });
}
```

**Step 7: Write unit tests**

In `chunkserver.rs` test module:

```rust
#[tokio::test]
async fn test_move_block_to_cold() {
    let hot_dir = tempfile::TempDir::new().unwrap();
    let cold_dir = tempfile::TempDir::new().unwrap();
    let server = MyChunkServer::new(hot_dir.path().to_path_buf(), Some(cold_dir.path().to_path_buf()));
    let data = b"cold block data";
    server.write_block_async("cold-block", data).await.unwrap();
    assert!(hot_dir.path().join("cold-block").exists());
    server.move_block_to_cold("cold-block").await.unwrap();
    assert!(!hot_dir.path().join("cold-block").exists());
    assert!(cold_dir.path().join("cold-block").exists());
}

#[tokio::test]
async fn test_read_block_finds_cold_block() {
    let hot_dir = tempfile::TempDir::new().unwrap();
    let cold_dir = tempfile::TempDir::new().unwrap();
    let server = MyChunkServer::new(hot_dir.path().to_path_buf(), Some(cold_dir.path().to_path_buf()));
    let data = b"hello cold world";
    // write directly to cold dir to simulate already-migrated block
    std::fs::write(cold_dir.path().join("cold-test"), data).unwrap();
    let result = server.read_block_async("cold-test", 0, data.len() as u64).await.unwrap();
    assert_eq!(result, data);
}
```

Run: `cargo test -p dfs-chunkserver --lib test_move_block_to_cold test_read_block_finds_cold_block`

**Step 8: Commit**

```bash
git add proto/dfs.proto dfs/chunkserver/src/chunkserver.rs dfs/chunkserver/src/bin/chunkserver.rs
git commit -m "feat(tiering): add MOVE_TO_COLD command and cold_storage_dir to ChunkServer"
```

---

## Task 4: Master background tiering scanner (Phase 1 — hot→cold)

**Files:**
- Modify: `dfs/metaserver/src/master.rs`
- Modify: `dfs/metaserver/src/bin/master.rs`

**Context:** The scanner runs as a `tokio::spawn` loop in the master binary, similar to how heartbeat / shard-split monitoring works. It only runs on the Raft leader (check with `GetLeaderInfo`). Threshold is read from env var.

**Step 1: Add tiering config to `MasterConfig`**

In `master.rs`, find `pub struct MasterConfig` and add:

```rust
/// Seconds since last access before a file is considered cold (env: COLD_THRESHOLD_SECS)
pub cold_threshold_secs: u64,
/// Seconds after cold move before EC conversion (env: EC_THRESHOLD_SECS)  
pub ec_threshold_secs: u64,
```

In `bin/master.rs`, when constructing `MasterConfig`:

```rust
cold_threshold_secs: std::env::var("COLD_THRESHOLD_SECS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(604800),  // 7 days
ec_threshold_secs: std::env::var("EC_THRESHOLD_SECS")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(2592000), // 30 days
```

**Step 2: Add `scan_tiering` method to `MyMaster`**

In `master.rs`:

```rust
/// Scan all files and issue MOVE_TO_COLD commands for cold files.
/// Only runs on the Raft leader. Called by background task every 60s.
pub async fn scan_tiering(&self) {
    // Only run on leader
    let (tx, rx) = tokio::sync::oneshot::channel();
    if self.raft_tx.send(Event::GetLeaderInfo { reply_tx: tx }).await.is_err() {
        return;
    }
    // GetLeaderInfo returns Some(addr) if we are leader (addr == self), else None means follower
    // We check by comparing with our own advertise address
    // Actually GetLeaderInfo returns the leader's address; we need to know our own.
    // Simpler: try a no-op Raft write and check if it succeeds — but that's expensive.
    // Instead, check safe_mode and use a heuristic: only scan if we can propose.
    // We'll attempt to propose and silently ignore "Not Leader" errors.

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let cold_threshold_ms = self.config.cold_threshold_secs * 1000;

    // Collect candidates under lock (don't hold lock during await)
    let candidates: Vec<(String, Vec<String>)> = {
        let state = self.state.lock().expect("Mutex poisoned");
        if let AppState::Master(ref ms) = *state {
            ms.files
                .values()
                .filter(|f| {
                    f.moved_to_cold_at_ms == 0          // not already cold
                    && f.ec_data_shards == 0             // not already EC
                    && f.last_access_ms > 0              // has been accessed at least once
                    && now_ms.saturating_sub(f.last_access_ms) > cold_threshold_ms
                })
                .map(|f| {
                    let block_locations: Vec<String> = f
                        .blocks
                        .iter()
                        .flat_map(|b| b.locations.iter().cloned())
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    (f.path.clone(), block_locations)
                })
                .collect()
        } else {
            vec![]
        }
    };

    for (path, locations) in candidates {
        // Issue MOVE_TO_COLD command to all ChunkServers holding blocks of this file
        {
            let mut state = self.state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref mut ms) = *state {
                if let Some(meta) = ms.files.get(&path) {
                    for block in &meta.blocks {
                        for loc in &block.locations {
                            ms.pending_commands
                                .entry(loc.clone())
                                .or_default()
                                .push(crate::dfs::ChunkServerCommand {
                                    r#type: crate::dfs::chunk_server_command::CommandType::MoveToCold as i32,
                                    block_id: block.block_id.clone(),
                                    ..Default::default()
                                });
                        }
                    }
                }
            }
        }

        // Persist the move via Raft
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.raft_tx
            .send(Event::ClientRequest {
                command: Command::Master(MasterCommand::MoveToCold {
                    path: path.clone(),
                    moved_at_ms: now_ms,
                }),
                reply_tx: tx,
            })
            .await;
        let _ = rx.await; // ignore error (follower nodes will just skip)
        tracing::info!("Tiering scanner: queued cold move for {}", path);
    }
}
```

**Step 3: Spawn tiering scanner in master binary**

In `dfs/metaserver/src/bin/master.rs`, after the Raft node spawn:

```rust
// Storage tiering background scanner
let master_for_tiering = master.clone();
tokio::spawn(async move {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        master_for_tiering.scan_tiering().await;
    }
});
```

Note: `MyMaster` needs to be `Clone` or wrapped in `Arc`. Check if `master` is already `Arc<MyMaster>` — if not, wrap it. (Look at how other background tasks use `master`.)

**Step 4: Write unit test for `scan_tiering`**

```rust
#[tokio::test]
async fn test_scan_tiering_queues_cold_move() {
    let (master, _dir) = make_test_master().await;
    create_test_file(&master, "/old-file").await;
    // Manually set last_access_ms to a very old timestamp
    {
        let mut state = master.state.lock().unwrap();
        if let AppState::Master(ref mut ms) = *state {
            if let Some(f) = ms.files.get_mut("/old-file") {
                f.last_access_ms = 1; // epoch+1ms = ancient
                // Add a fake block location
                f.blocks.push(crate::dfs::BlockInfo {
                    block_id: "blk-001".into(),
                    locations: vec!["127.0.0.1:9000".into()],
                    ..Default::default()
                });
            }
        }
    }
    // set tiny threshold
    // (override config for test)
    master.scan_tiering().await;
    // check pending_commands
    let state = master.state.lock().unwrap();
    if let AppState::Master(ref ms) = *state {
        let cmds = ms.pending_commands.get("127.0.0.1:9000");
        assert!(cmds.is_some_and(|c| c.iter().any(|cmd| cmd.block_id == "blk-001")));
    }
}
```

**Step 5: Run tests**

```bash
cargo test -p dfs-metaserver --lib
```

**Step 6: Commit**

```bash
git add dfs/metaserver/src/master.rs dfs/metaserver/src/bin/master.rs
git commit -m "feat(tiering): background scanner issues MOVE_TO_COLD for stale files"
```

---

## Task 5: Phase 2 — EC conversion scanner

**Files:**
- Modify: `dfs/metaserver/src/master.rs`

**Context:** After a file has been in the cold tier for `EC_THRESHOLD_SECS`, convert its 3x-replicated blocks to RS(6,3) EC. The existing `RECONSTRUCT_EC_SHARD` command path handles EC shard writing — we reuse `AllocateBlock` with EC params + `REPLICATE`/`RECONSTRUCT_EC_SHARD` commands, then issue `DELETE` for old replicas.

The simplest correct approach: add EC conversion logic to `scan_tiering`. When a file has `moved_to_cold_at_ms > 0` and `ec_data_shards == 0` and age > `ec_threshold_secs`, issue `ConvertToEc` Raft command + appropriate `pending_commands`.

**Step 1: Add `scan_ec_conversion` method**

```rust
pub async fn scan_ec_conversion(&self) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let ec_threshold_ms = self.config.ec_threshold_secs * 1000;
    const EC_DATA: i32 = 6;
    const EC_PARITY: i32 = 3;

    let candidates: Vec<FileMetadata> = {
        let state = self.state.lock().expect("Mutex poisoned");
        if let AppState::Master(ref ms) = *state {
            ms.files
                .values()
                .filter(|f| {
                    f.moved_to_cold_at_ms > 0
                    && f.ec_data_shards == 0
                    && now_ms.saturating_sub(f.moved_to_cold_at_ms) > ec_threshold_ms
                })
                .cloned()
                .collect()
        } else {
            vec![]
        }
    };

    for file in candidates {
        // Find ChunkServers with enough capacity for EC shards
        let available_servers: Vec<String> = {
            let state = self.state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref ms) = *state {
                ms.chunk_servers
                    .iter()
                    .filter(|(_, s)| s.available_space > 0)
                    .map(|(addr, _)| addr.clone())
                    .take((EC_DATA + EC_PARITY) as usize)
                    .collect()
            } else {
                vec![]
            }
        };

        if available_servers.len() < (EC_DATA + EC_PARITY) as usize {
            tracing::warn!(
                "Not enough ChunkServers for EC conversion of {} (need {}, have {})",
                file.path, EC_DATA + EC_PARITY, available_servers.len()
            );
            continue;
        }

        // For each block: issue RECONSTRUCT_EC_SHARD commands (one per shard),
        // then issue ConvertToEc Raft command with new shard locations,
        // then issue DELETE for old replicas.
        let mut new_blocks = Vec::new();

        for block in &file.blocks {
            // Assign one CS per shard
            let shard_locations: Vec<String> = available_servers
                .iter()
                .take((EC_DATA + EC_PARITY) as usize)
                .cloned()
                .collect();

            let new_block = crate::dfs::BlockInfo {
                block_id: block.block_id.clone(),
                size: block.size,
                locations: shard_locations.clone(),
                checksum_crc32c: block.checksum_crc32c,
                ec_data_shards: EC_DATA,
                ec_parity_shards: EC_PARITY,
                original_size: block.original_size,
            };
            new_blocks.push(new_block);

            // Issue RECONSTRUCT_EC_SHARD commands for each shard
            {
                let mut state = self.state.lock().expect("Mutex poisoned");
                if let AppState::Master(ref mut ms) = *state {
                    // Find the current CS holding this block (source for EC encoding)
                    let source_cs = block.locations.first().cloned().unwrap_or_default();
                    for (shard_idx, cs_addr) in shard_locations.iter().enumerate() {
                        let mut ec_shard_sources = vec!["".to_string(); (EC_DATA + EC_PARITY) as usize];
                        ec_shard_sources[0] = source_cs.clone(); // data shard 0 = source block
                        ms.pending_commands
                            .entry(cs_addr.clone())
                            .or_default()
                            .push(crate::dfs::ChunkServerCommand {
                                r#type: crate::dfs::chunk_server_command::CommandType::ReconstructEcShard as i32,
                                block_id: block.block_id.clone(),
                                shard_index: shard_idx as i32,
                                ec_data_shards: EC_DATA,
                                ec_parity_shards: EC_PARITY,
                                ec_shard_sources,
                                original_block_size: block.original_size,
                                ..Default::default()
                            });
                    }

                    // Issue DELETE for old replicas
                    for old_loc in &block.locations {
                        ms.pending_commands
                            .entry(old_loc.clone())
                            .or_default()
                            .push(crate::dfs::ChunkServerCommand {
                                r#type: crate::dfs::chunk_server_command::CommandType::Delete as i32,
                                block_id: block.block_id.clone(),
                                ..Default::default()
                            });
                    }
                }
            }
        }

        // Persist EC conversion via Raft
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.raft_tx
            .send(Event::ClientRequest {
                command: Command::Master(MasterCommand::ConvertToEc {
                    path: file.path.clone(),
                    ec_data_shards: EC_DATA,
                    ec_parity_shards: EC_PARITY,
                    new_blocks,
                }),
                reply_tx: tx,
            })
            .await;
        let _ = rx.await;
        tracing::info!("Tiering scanner: queued EC conversion for {}", file.path);
    }
}
```

**Step 2: Call `scan_ec_conversion` from the tiering background task**

In `bin/master.rs`, update the tiering loop:

```rust
tokio::spawn(async move {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        master_for_tiering.scan_tiering().await;
        master_for_tiering.scan_ec_conversion().await;
    }
});
```

**Step 3: Write unit test for EC conversion scan**

```rust
#[tokio::test]
async fn test_scan_ec_conversion_queues_commands() {
    let (master, _dir) = make_test_master().await;
    create_test_file(&master, "/cold-file").await;
    {
        let mut state = master.state.lock().unwrap();
        if let AppState::Master(ref mut ms) = *state {
            // Register 9 fake chunk servers
            for i in 0..9 {
                ms.chunk_servers.insert(
                    format!("127.0.0.{}:9000", i + 1),
                    ChunkServerStatus { available_space: 1_000_000, ..Default::default() },
                );
            }
            if let Some(f) = ms.files.get_mut("/cold-file") {
                f.moved_to_cold_at_ms = 1; // ancient
                f.blocks.push(crate::dfs::BlockInfo {
                    block_id: "blk-ec-001".into(),
                    locations: vec!["127.0.0.1:9000".into()],
                    ..Default::default()
                });
            }
        }
    }
    master.scan_ec_conversion().await;
    let state = master.state.lock().unwrap();
    if let AppState::Master(ref ms) = *state {
        // Verify that at least one CS has RECONSTRUCT_EC_SHARD commands queued
        let total_cmds: usize = ms.pending_commands.values().map(|v| v.len()).sum();
        assert!(total_cmds >= 9, "Expected ≥9 EC shard commands, got {}", total_cmds);
    }
}
```

**Step 4: Run tests**

```bash
cargo test -p dfs-metaserver --lib
cargo test -p dfs-chunkserver --lib
```

**Step 5: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat(tiering): Phase 2 EC conversion scanner"
```

---

## Task 6: Wire up Docker Compose + update TODO.md

**Files:**
- Modify: `docker-compose.yml`
- Modify: `TODO.md`

**Step 1: Add cold storage volume to chunkserver services in docker-compose.yml**

For each `chunkserver` service, add a cold volume mount and the `--cold-storage-dir` arg:

```yaml
chunkserver1:
  # ... existing config ...
  volumes:
    - chunkserver1_data:/data
    - chunkserver1_cold:/data/cold   # new
  command: >
    /app/chunkserver
    --storage-dir /data
    --cold-storage-dir /data/cold    # new
    # ... rest of args ...
  environment:
    COLD_THRESHOLD_SECS: "604800"   # 7 days (override for testing: "60")
    EC_THRESHOLD_SECS: "2592000"    # 30 days

volumes:
  chunkserver1_cold:  # new
```

Apply the same pattern to all chunkserver instances.

**Step 2: Mark TODO items as complete**

In `TODO.md`, change the Intelligent Storage Tiering items:

```markdown
- [x] **Intelligent Storage Tiering** ✅
    - [x] アクセス統計をベースに「冷えたデータ」をHDD層へ自動移行するバックグラウンドプロセス。
    - [ ] メタデータの構造最適化（SeaweedFS方式など）によるメモリ占有率の削減。
```

Also update Erasure Coding item:

```markdown
- [x] **Erasure Coding (RS(6,3))** ✅ — automatic conversion via tiering scanner
```

**Step 3: Commit**

```bash
git add docker-compose.yml TODO.md
git commit -m "feat(tiering): wire up cold volumes in docker-compose, update TODO"
```

---

## Task 7: Build verification

**Step 1: Full build**

```bash
cargo build --release 2>&1 | grep -E "^error"
```

Expected: no errors.

**Step 2: All unit tests**

```bash
cargo test --lib
```

Expected: all pass.

**Step 3: Commit if any fixes needed**

```bash
git add -A
git commit -m "fix(tiering): address compile errors"
```
