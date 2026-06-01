# Background Healer Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Automatically detect and repair under-replicated or corrupted blocks so the cluster self-heals after ChunkServer failures.

**Architecture:** Two complementary triggers — (1) Master detects dead ChunkServers and immediately schedules re-replication; (2) Master runs a periodic 5-minute scan for any remaining under-replicated blocks. ChunkServer's existing background scrubber (already runs every 60s) reports corrupted blocks back to Master via a new `bad_blocks` field in `HeartbeatRequest`, and Master marks those locations as bad before running the healer.

**Tech Stack:** Rust/Tokio async tasks, existing `ChunkServerCommand` / `pending_commands` mechanism, tonic gRPC proto extension, `Arc<Mutex<Vec<String>>>` for scrubber→heartbeat state sharing.

---

### Task 1: Proto — add `bad_blocks` to `HeartbeatRequest`

**Files:**
- Modify: `proto/dfs.proto:48-53`

**Step 1: Add field**

```protobuf
message HeartbeatRequest {
  string chunk_server_address = 1;
  uint64 used_space = 2;
  uint64 available_space = 3;
  uint64 chunk_count = 4;
  repeated string bad_blocks = 5;  // Block IDs whose local copy is corrupted
}
```

**Step 2: Rebuild to verify proto compiles**

```bash
cargo build -p dfs-metaserver 2>&1 | head -30
```

Expected: compiles (new field is backward-compatible).

**Step 3: Commit**

```bash
git add proto/dfs.proto
git commit -m "feat: add bad_blocks field to HeartbeatRequest"
```

---

### Task 2: Master — add `bad_block_locations` to `MasterState` and `heal_under_replicated_blocks`

**Files:**
- Modify: `dfs/metaserver/src/master.rs:188-233` (MasterState struct)
- Modify: `dfs/metaserver/src/master.rs` (add function after MasterState impl block, around line 340)

**Step 1: Add `bad_block_locations` field to `MasterState`**

In the `MasterState` struct, after `safe_mode_manual`:

```rust
/// Blocks known to be corrupted on specific ChunkServers (not Raft-persisted).
/// Maps block_id -> set of ChunkServer addresses with a bad copy.
#[serde(skip)]
pub bad_block_locations: HashMap<String, HashSet<String>>,
```

**Step 2: Add `heal_under_replicated_blocks` function**

Add this as a free function after the `impl MasterState` block (around line 340):

```rust
/// Schedule replication commands for all blocks with fewer live replicas than REPLICATION_FACTOR.
/// Considers both dead chunk servers and known-bad block locations.
fn heal_under_replicated_blocks(state: &mut MasterState) {
    const REPLICATION_FACTOR: usize = 3;

    let live_servers: Vec<String> = state.chunk_servers.keys().cloned().collect();
    if live_servers.is_empty() {
        return;
    }

    for file in state.files.values() {
        for block in &file.blocks {
            // Effective live locations: in metadata AND the server is alive AND not known-bad
            let bad_on = state
                .bad_block_locations
                .get(&block.block_id)
                .cloned()
                .unwrap_or_default();

            let live_locs: Vec<String> = block
                .locations
                .iter()
                .filter(|loc| state.chunk_servers.contains_key(*loc) && !bad_on.contains(*loc))
                .cloned()
                .collect();

            let needed = REPLICATION_FACTOR.saturating_sub(live_locs.len());
            if needed == 0 {
                continue;
            }

            if live_locs.is_empty() {
                tracing::error!(
                    "Healer: block {} has NO live replicas — data may be lost",
                    block.block_id
                );
                continue;
            }

            let source = &live_locs[0];

            // Targets: live servers not already in the effective replica set
            let targets: Vec<String> = live_servers
                .iter()
                .filter(|s| !live_locs.contains(s))
                .take(needed)
                .cloned()
                .collect();

            for target in &targets {
                state
                    .pending_commands
                    .entry(source.clone())
                    .or_default()
                    .push(ChunkServerCommand {
                        r#type: 1, // REPLICATE
                        block_id: block.block_id.clone(),
                        target_chunk_server_address: target.clone(),
                    });
                tracing::info!(
                    "Healer: scheduled replication of block {} from {} to {}",
                    block.block_id,
                    source,
                    target
                );
            }
        }
    }
}
```

**Step 3: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -30
```

Expected: compiles.

**Step 4: Write unit test**

Add to the existing `#[cfg(test)]` module in `master.rs`:

```rust
#[test]
fn test_heal_under_replicated_blocks_schedules_replication() {
    use crate::dfs::BlockInfo;
    let mut state = MasterState::default();

    // Register 3 live chunk servers
    let now = 9_999_999_999_999u64;
    for addr in ["cs1:50055", "cs2:50056", "cs3:50057"] {
        state.chunk_servers.insert(
            addr.to_string(),
            ChunkServerStatus { last_heartbeat: now, used_space: 0, available_space: 1_000_000, chunk_count: 1 },
        );
    }

    // A file with one block that only has 1 replica
    state.files.insert(
        "/test/file".to_string(),
        FileMetadata {
            path: "/test/file".to_string(),
            size: 100,
            blocks: vec![BlockInfo {
                block_id: "block-1".to_string(),
                size: 100,
                locations: vec!["cs1:50055".to_string()],
                checksum_crc32c: 0,
            }],
            etag_md5: String::new(),
            created_at_ms: 0,
        },
    );

    heal_under_replicated_blocks(&mut state);

    // Should schedule 2 replication commands from cs1 to cs2 and cs3
    let cmds = state.pending_commands.get("cs1:50055").expect("commands expected");
    assert_eq!(cmds.len(), 2);
    let targets: Vec<_> = cmds.iter().map(|c| c.target_chunk_server_address.as_str()).collect();
    assert!(targets.contains(&"cs2:50056") || targets.contains(&"cs3:50057"));
}

#[test]
fn test_heal_skips_fully_replicated_blocks() {
    use crate::dfs::BlockInfo;
    let mut state = MasterState::default();

    let now = 9_999_999_999_999u64;
    for addr in ["cs1:50055", "cs2:50056", "cs3:50057"] {
        state.chunk_servers.insert(
            addr.to_string(),
            ChunkServerStatus { last_heartbeat: now, used_space: 0, available_space: 1_000_000, chunk_count: 1 },
        );
    }

    state.files.insert(
        "/test/full".to_string(),
        FileMetadata {
            path: "/test/full".to_string(),
            size: 100,
            blocks: vec![BlockInfo {
                block_id: "block-full".to_string(),
                size: 100,
                locations: vec!["cs1:50055".to_string(), "cs2:50056".to_string(), "cs3:50057".to_string()],
                checksum_crc32c: 0,
            }],
            etag_md5: String::new(),
            created_at_ms: 0,
        },
    );

    heal_under_replicated_blocks(&mut state);

    assert!(state.pending_commands.is_empty());
}

#[test]
fn test_heal_treats_bad_block_location_as_missing() {
    use crate::dfs::BlockInfo;
    let mut state = MasterState::default();

    let now = 9_999_999_999_999u64;
    for addr in ["cs1:50055", "cs2:50056", "cs3:50057"] {
        state.chunk_servers.insert(
            addr.to_string(),
            ChunkServerStatus { last_heartbeat: now, used_space: 0, available_space: 1_000_000, chunk_count: 1 },
        );
    }

    // Block has 3 locations but one is corrupt
    state.files.insert(
        "/test/bad".to_string(),
        FileMetadata {
            path: "/test/bad".to_string(),
            size: 100,
            blocks: vec![BlockInfo {
                block_id: "block-bad".to_string(),
                size: 100,
                locations: vec!["cs1:50055".to_string(), "cs2:50056".to_string(), "cs3:50057".to_string()],
                checksum_crc32c: 0,
            }],
            etag_md5: String::new(),
            created_at_ms: 0,
        },
    );

    // Mark cs2 as having a corrupt copy
    state.bad_block_locations
        .entry("block-bad".to_string())
        .or_default()
        .insert("cs2:50056".to_string());

    // With only 2 effective replicas we can't re-replicate (no unused server available)
    // — but no panic should occur
    heal_under_replicated_blocks(&mut state);
    // No new commands since all 3 servers already have the block in metadata
    // (we can't put a new replica somewhere that doesn't exist)
    assert!(state.pending_commands.is_empty());
}
```

**Step 5: Run tests**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | tail -20
```

Expected: new tests pass.

**Step 6: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat: add heal_under_replicated_blocks and bad_block_locations to MasterState"
```

---

### Task 3: Master — call healer after ChunkServer death detection

**Files:**
- Modify: `dfs/metaserver/src/master.rs:482-488` (liveness check loop, dead server removal)

**Step 1: Call healer after dead server removal**

Replace the dead server removal block (lines 482-486):

```rust
for addr in &dead_servers {
    tracing::warn!("ChunkServer {} is dead (no heartbeat), removing...", addr);
    state.chunk_servers.remove(addr);
    state.pending_commands.remove(addr);
}
if !dead_servers.is_empty() {
    tracing::info!(
        "Healer: {} ChunkServer(s) died, scanning for under-replicated blocks",
        dead_servers.len()
    );
    heal_under_replicated_blocks(state);
}
```

**Step 2: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

**Step 3: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat: trigger background healer after ChunkServer death detection"
```

---

### Task 4: Master — periodic healer task (5-minute interval)

**Files:**
- Modify: `dfs/metaserver/src/master.rs` — add new `tokio::spawn` after the balancer task spawn (around line 560)

**Step 1: Add periodic healer spawn**

After the balancer task's closing `});` (around line 559), add:

```rust
// Spawn periodic healer task
let state_clone_healer = state.clone();
tokio::spawn(async move {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // 5 minutes
    loop {
        interval.tick().await;
        let mut state_lock = state_clone_healer.lock().expect("Mutex poisoned");
        if let AppState::Master(ref mut state) = *state_lock {
            if !state.is_in_safe_mode() {
                tracing::info!("Periodic healer: scanning for under-replicated blocks");
                heal_under_replicated_blocks(state);
            }
        }
    }
});
```

**Step 2: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

**Step 3: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat: add periodic 5-minute background healer task"
```

---

### Task 5: Master — process `bad_blocks` in heartbeat handler

**Files:**
- Modify: `dfs/metaserver/src/master.rs:1606-1639` (heartbeat handler)

**Step 1: Process bad_blocks before retrieving commands**

In the heartbeat handler, after the safe mode exit check (around line 1629), before the pending_commands retrieval, add:

```rust
// Process bad block reports from the ChunkServer's scrubber
if !req.bad_blocks.is_empty() {
    tracing::warn!(
        "HeartBeat: {} bad block(s) reported by {}",
        req.bad_blocks.len(),
        req.chunk_server_address
    );
    for block_id in &req.bad_blocks {
        state
            .bad_block_locations
            .entry(block_id.clone())
            .or_default()
            .insert(req.chunk_server_address.clone());
    }
    heal_under_replicated_blocks(state);
}
```

**Step 2: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

**Step 3: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat: process bad_blocks from ChunkServer heartbeat and trigger healing"
```

---

### Task 6: ChunkServer — share corrupted blocks between scrubber and heartbeat

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs:22-31` (MyChunkServer struct)
- Modify: `dfs/chunkserver/src/chunkserver.rs:34-77` (new() constructor)
- Modify: `dfs/chunkserver/src/chunkserver.rs:374-445` (run_background_scrubber)
- Modify: `dfs/chunkserver/src/bin/chunkserver.rs:120-127` (scrubber spawn)
- Modify: `dfs/chunkserver/src/bin/chunkserver.rs:134+` (heartbeat loop)

**Step 1: Add `pending_bad_blocks` field to `MyChunkServer`**

In the `MyChunkServer` struct:

```rust
/// Corrupted block IDs detected by the scrubber, waiting to be reported to Master.
pub pending_bad_blocks: Arc<Mutex<Vec<String>>>,
```

**Step 2: Initialize in `new()`**

In `MyChunkServer::new()`, add to the returned struct:

```rust
pending_bad_blocks: Arc::new(Mutex::new(Vec::new())),
```

**Step 3: Scrubber populates `pending_bad_blocks` instead of calling `recover_block`**

Replace the `run_background_scrubber` function body. The scrubber's job is now *detection only*. Report to Master (via heartbeat) and let Master drive the fix. Still call `recover_block` locally for immediate self-healing, AND report to Master:

```rust
pub async fn run_background_scrubber(server: MyChunkServer, interval: std::time::Duration) {
    let storage_dir = server.storage_dir.clone();

    loop {
        tokio::time::sleep(interval).await;
        tracing::info!("Starting background block scrubber...");

        let storage_dir_clone = storage_dir.clone();
        let server_clone = server.clone();

        let corrupted_blocks = tokio::task::spawn_blocking(move || {
            let mut corrupted = Vec::new();
            match std::fs::read_dir(&storage_dir_clone) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() || path.extension().is_some_and(|ext| ext == "meta") {
                            continue;
                        }
                        if let Some(block_id) = path.file_name().and_then(|n| n.to_str()) {
                            match std::fs::read(&path) {
                                Ok(data) => {
                                    if server_clone.verify_block(block_id, &data).is_err() {
                                        tracing::error!(
                                            "Scrubber: corruption detected in block {}",
                                            block_id
                                        );
                                        corrupted.push(block_id.to_string());
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Scrubber: failed to read block {}: {}", block_id, e)
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::error!("Scrubber: failed to read storage dir: {}", e),
            }
            corrupted
        })
        .await
        .unwrap_or_default();

        if !corrupted_blocks.is_empty() {
            // Queue for reporting to Master via next heartbeat
            {
                let mut pending = server.pending_bad_blocks.lock().expect("Mutex poisoned");
                pending.extend(corrupted_blocks.iter().cloned());
            }

            // Also attempt local self-healing immediately
            for block_id in &corrupted_blocks {
                tracing::info!("Scrubber: attempting local recovery for block {}", block_id);
                if let Err(e) = server.recover_block(block_id).await {
                    tracing::error!("Scrubber: local recovery failed for {}: {}", block_id, e);
                }
            }
        }

        tracing::info!("Background block scrubber finished.");
    }
}
```

**Step 4: Drain `pending_bad_blocks` in the heartbeat loop**

In `dfs/chunkserver/src/bin/chunkserver.rs`, in the heartbeat sending loop (where `HeartbeatRequest` is constructed, around line 200+), add:

```rust
// Drain any bad blocks detected by the scrubber
let bad_blocks = {
    let mut pending = chunk_server_heartbeat.pending_bad_blocks.lock().expect("Mutex poisoned");
    std::mem::take(&mut *pending)
};
```

Then pass `bad_blocks` to `HeartbeatRequest`:

```rust
let req = tonic::Request::new(crate::dfs::HeartbeatRequest {
    chunk_server_address: my_addr.clone(),
    used_space,
    available_space,
    chunk_count,
    bad_blocks,
});
```

**Step 5: Build**

```bash
cargo build 2>&1 | head -40
```

Expected: full build passes.

**Step 6: Run unit tests**

```bash
cargo test --lib 2>&1 | tail -20
```

Expected: all pass.

**Step 7: Commit**

```bash
git add dfs/chunkserver/src/chunkserver.rs dfs/chunkserver/src/bin/chunkserver.rs
git commit -m "feat: ChunkServer scrubber reports bad blocks to Master via heartbeat"
```

---

### Task 7: Smoke test with Docker Compose

**Step 1: Build Docker image**

```bash
docker compose build 2>&1 | tail -10
```

**Step 2: Start cluster**

```bash
docker compose up -d
sleep 10
```

**Step 3: Write a file, then kill a ChunkServer, verify healing log**

```bash
# Write test file
docker exec dfs-master1-shard1 dfs_cli --master http://localhost:50051 put /etc/hostname /test/heal_test

# Kill one chunkserver
docker stop dfs-chunkserver1-shard1

# Wait for death detection + healing (15s detection + a few seconds)
sleep 20

# Check master logs for healer activity
docker logs dfs-master1-shard1 2>&1 | grep -i "healer\|under-replicated\|replicate"
```

Expected: log lines like `"Healer: scheduled replication of block ... from ... to ..."`.

**Step 4: Bring down cluster**

```bash
docker compose down -v
```

**Step 5: Update TODO.md**

In `TODO.md`, mark Background Healer as complete:

```markdown
- [x] **Background Healer (Auto-Repair)** ✅
    - [x] レプリカ数が不足している、またはチェックサムが不一致なブロックを抽出するスキャナー。
    - [x] Metaserverによる不足レプリカの自動再配置命令の送出。
```

**Step 6: Commit**

```bash
git add TODO.md
git commit -m "docs: mark Background Healer as complete in TODO.md"
```
