# Rack Awareness Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** When allocating block replicas, spread them across different server racks to tolerate rack-level failures.

**Architecture:** ChunkServers advertise their rack via `--rack-id` CLI flag; the Master stores it in `ChunkServerStatus` and uses a rack-aware greedy algorithm in `AllocateBlock`: pick one server per rack, then fall back to any available server when replicas > racks.

**Tech Stack:** tonic/protobuf (dfs.proto), existing `ChunkServerStatus` struct, clap Args in chunkserver binary.

---

### Task 1: Add `rack_id` to proto and regenerate

**Files:**
- Modify: `proto/dfs.proto:146-149` (`RegisterChunkServerRequest`)

**Step 1: Add `rack_id` field to `RegisterChunkServerRequest`**

Current message (lines 146-149):
```proto
message RegisterChunkServerRequest {
  string address = 1;
  uint64 capacity = 2;
}
```

Replace with:
```proto
message RegisterChunkServerRequest {
  string address = 1;
  uint64 capacity = 2;
  string rack_id = 3;
}
```

**Step 2: Build to regenerate proto bindings**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

Expected: clean build (new `rack_id` field has default empty string, so all existing callers compile without changes).

**Step 3: Commit**

```bash
git add proto/dfs.proto
git commit -m "feat: add rack_id field to RegisterChunkServerRequest proto"
```

---

### Task 2: Add `rack_id` to `ChunkServerStatus` and master handlers

**Files:**
- Modify: `dfs/metaserver/src/master.rs:181-186` (`ChunkServerStatus` struct)
- Modify: `dfs/metaserver/src/master.rs:1670-1680` (`register_chunk_server` handler)
- Modify: `dfs/metaserver/src/master.rs:1707-1715` (`heartbeat` handler)

**Step 1: Add `rack_id` to `ChunkServerStatus`**

Current struct (lines 180-186):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkServerStatus {
    pub last_heartbeat: u64,
    pub used_space: u64,
    pub available_space: u64,
    pub chunk_count: u64,
}
```

Replace with:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkServerStatus {
    pub last_heartbeat: u64,
    pub used_space: u64,
    pub available_space: u64,
    pub chunk_count: u64,
    #[serde(default)]
    pub rack_id: String,
}
```

**Step 2: Update `register_chunk_server` handler to store `rack_id`**

Current code (lines 1672-1680):
```rust
state.chunk_servers.insert(
    req.address,
    ChunkServerStatus {
        last_heartbeat: now,
        used_space: 0,
        available_space: req.capacity,
        chunk_count: 0,
    },
);
```

Replace with:
```rust
state.chunk_servers.insert(
    req.address,
    ChunkServerStatus {
        last_heartbeat: now,
        used_space: 0,
        available_space: req.capacity,
        chunk_count: 0,
        rack_id: req.rack_id.clone(),
    },
);
```

**Step 3: Update `heartbeat` handler to preserve `rack_id`**

Current code (lines 1707-1715):
```rust
state.chunk_servers.insert(
    req.chunk_server_address.clone(),
    ChunkServerStatus {
        last_heartbeat: now,
        used_space: req.used_space,
        available_space: req.available_space,
        chunk_count: req.chunk_count,
    },
);
```

Replace with:
```rust
// Preserve existing rack_id (heartbeat doesn't re-send it)
let existing_rack_id = state
    .chunk_servers
    .get(&req.chunk_server_address)
    .map(|s| s.rack_id.clone())
    .unwrap_or_default();

state.chunk_servers.insert(
    req.chunk_server_address.clone(),
    ChunkServerStatus {
        last_heartbeat: now,
        used_space: req.used_space,
        available_space: req.available_space,
        chunk_count: req.chunk_count,
        rack_id: existing_rack_id,
    },
);
```

**Step 4: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

Expected: clean build. Existing unit tests using `ChunkServerStatus { ... }` struct literals will fail to compile if they don't include `rack_id` — fix them by adding `rack_id: String::new()` or `rack_id: "rack1".to_string()` as appropriate.

**Step 5: Run tests**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```

Expected: all existing tests pass.

**Step 6: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat: add rack_id to ChunkServerStatus and master handlers"
```

---

### Task 3: Implement rack-aware server selection in `AllocateBlock`

**Files:**
- Modify: `dfs/metaserver/src/master.rs` — add helper function + replace selection logic in `allocate_block`

**Step 1: Write failing tests first**

At the bottom of the `#[cfg(test)]` module in `master.rs`, add:

```rust
#[test]
fn test_rack_aware_selection_spreads_across_racks() {
    // 3 servers on 3 different racks — should pick one per rack
    let servers = vec![
        ("cs1:50051".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 1_000_000, chunk_count: 0,
            rack_id: "rack-a".to_string(),
        }),
        ("cs2:50052".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 900_000, chunk_count: 0,
            rack_id: "rack-b".to_string(),
        }),
        ("cs3:50053".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 800_000, chunk_count: 0,
            rack_id: "rack-c".to_string(),
        }),
    ];
    let selected = select_servers_rack_aware(&servers, 3);
    assert_eq!(selected.len(), 3);
    // All 3 racks represented
    let racks: std::collections::HashSet<String> = selected.iter()
        .map(|addr| {
            servers.iter().find(|(a, _)| a == addr).unwrap().1.rack_id.clone()
        })
        .collect();
    assert_eq!(racks.len(), 3, "All 3 racks should be represented");
}

#[test]
fn test_rack_aware_selection_fallback_to_same_rack() {
    // 3 servers on only 2 racks — need 3 replicas, must pick 2 from one rack
    let servers = vec![
        ("cs1:50051".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 1_000_000, chunk_count: 0,
            rack_id: "rack-a".to_string(),
        }),
        ("cs2:50052".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 900_000, chunk_count: 0,
            rack_id: "rack-a".to_string(),
        }),
        ("cs3:50053".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 800_000, chunk_count: 0,
            rack_id: "rack-b".to_string(),
        }),
    ];
    let selected = select_servers_rack_aware(&servers, 3);
    assert_eq!(selected.len(), 3);
    // All 3 servers picked (no duplicates)
    let unique: std::collections::HashSet<&String> = selected.iter().collect();
    assert_eq!(unique.len(), 3);
}

#[test]
fn test_rack_aware_selection_empty_rack_id_treated_as_distinct() {
    // Servers with empty rack_id should each be treated as their own rack
    let servers = vec![
        ("cs1:50051".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 1_000_000, chunk_count: 0,
            rack_id: "".to_string(),
        }),
        ("cs2:50052".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 900_000, chunk_count: 0,
            rack_id: "".to_string(),
        }),
    ];
    let selected = select_servers_rack_aware(&servers, 2);
    assert_eq!(selected.len(), 2);
}

#[test]
fn test_rack_aware_selection_fewer_servers_than_replicas() {
    // Only 2 servers but want 3 replicas — return all 2
    let servers = vec![
        ("cs1:50051".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 1_000_000, chunk_count: 0,
            rack_id: "rack-a".to_string(),
        }),
        ("cs2:50052".to_string(), ChunkServerStatus {
            last_heartbeat: 9_999_999_999_999,
            used_space: 0, available_space: 900_000, chunk_count: 0,
            rack_id: "rack-b".to_string(),
        }),
    ];
    let selected = select_servers_rack_aware(&servers, 3);
    assert_eq!(selected.len(), 2);
}
```

**Step 2: Run tests to confirm they fail**

```bash
cargo test -p dfs-metaserver --lib -- test_rack_aware 2>&1 | tail -10
```

Expected: FAIL with `cannot find function select_servers_rack_aware`.

**Step 3: Add `select_servers_rack_aware` free function**

Add this function near `heal_under_replicated_blocks` (around line 200, before the `impl MasterServer` block):

```rust
/// Select up to `n` chunk servers for replica placement, maximizing rack diversity.
///
/// Algorithm:
/// 1. Sort all candidates by available_space descending (best-first).
/// 2. Round-robin through racks: in each round, pick the best remaining server
///    from each rack that hasn't contributed yet in this round.
/// 3. Stop when `n` servers are selected or candidates exhausted.
///
/// Empty rack_id strings are each treated as a unique rack (address-keyed).
fn select_servers_rack_aware(
    servers: &[(String, ChunkServerStatus)],
    n: usize,
) -> Vec<String> {
    if n == 0 || servers.is_empty() {
        return vec![];
    }

    // Sort candidates by available_space descending
    let mut candidates: Vec<&(String, ChunkServerStatus)> =
        servers.iter().collect();
    candidates.sort_by(|a, b| b.1.available_space.cmp(&a.1.available_space));

    // Group by rack. Empty rack_id → use address as key to avoid grouping them.
    use std::collections::HashMap;
    let mut rack_buckets: HashMap<String, Vec<&(String, ChunkServerStatus)>> =
        HashMap::new();
    for s in &candidates {
        let rack_key = if s.1.rack_id.is_empty() {
            format!("__addr__{}", s.0)
        } else {
            s.1.rack_id.clone()
        };
        rack_buckets.entry(rack_key).or_default().push(s);
    }

    // Collect racks ordered by their best (first) server's available_space
    let mut racks: Vec<Vec<&(String, ChunkServerStatus)>> = {
        let mut r: Vec<Vec<&(String, ChunkServerStatus)>> =
            rack_buckets.into_values().collect();
        // Sort racks by best server descending
        r.sort_by(|a, b| {
            b[0].1.available_space.cmp(&a[0].1.available_space)
        });
        r
    };

    let mut selected: Vec<String> = Vec::with_capacity(n);
    let mut rack_positions: Vec<usize> = vec![0; racks.len()];

    // Round-robin: each round picks one server per rack
    'outer: loop {
        let mut picked_this_round = false;
        for (rack_idx, rack) in racks.iter().enumerate() {
            if selected.len() >= n {
                break 'outer;
            }
            let pos = rack_positions[rack_idx];
            if pos < rack.len() {
                selected.push(rack[pos].0.clone());
                rack_positions[rack_idx] += 1;
                picked_this_round = true;
            }
        }
        if !picked_this_round {
            break; // All candidates exhausted
        }
    }

    selected
}
```

**Step 4: Run tests to confirm they pass**

```bash
cargo test -p dfs-metaserver --lib -- test_rack_aware 2>&1 | tail -10
```

Expected: 4 tests PASS.

**Step 5: Replace selection logic in `allocate_block`**

In `allocate_block` (lines 1497-1529), replace the current selection block:

Current code:
```rust
let (chunk_servers, block_id) = {
    let state_lock = self.state.lock().expect("Mutex poisoned");
    if let AppState::Master(ref state) = *state_lock {
        if !state.files.contains_key(&req.path) {
            return Err(Status::not_found("File not found"));
        }

        // Load balancing: Select chunk servers with most available space
        let mut candidates: Vec<(String, u64)> = state
            .chunk_servers
            .iter()
            .map(|(addr, status)| (addr.clone(), status.available_space))
            .collect();

        if candidates.is_empty() {
            return Err(Status::unavailable("No chunk servers available"));
        }

        // Sort by available space descending
        candidates.sort_by(|a, b| b.1.cmp(&a.1));

        let chunk_servers: Vec<String> =
            candidates.into_iter().map(|(addr, _)| addr).collect();
        (chunk_servers, Uuid::new_v4().to_string())
    } else {
        return Err(Status::internal("Wrong state type"));
    }
};

// Select chunk servers
let num_replicas = std::cmp::min(REPLICATION_FACTOR, chunk_servers.len());
let selected_servers: Vec<String> =
    chunk_servers.iter().take(num_replicas).cloned().collect();
```

Replace with:
```rust
let (selected_servers, block_id) = {
    let state_lock = self.state.lock().expect("Mutex poisoned");
    if let AppState::Master(ref state) = *state_lock {
        if !state.files.contains_key(&req.path) {
            return Err(Status::not_found("File not found"));
        }

        if state.chunk_servers.is_empty() {
            return Err(Status::unavailable("No chunk servers available"));
        }

        let candidates: Vec<(String, ChunkServerStatus)> = state
            .chunk_servers
            .iter()
            .map(|(addr, status)| (addr.clone(), status.clone()))
            .collect();

        let selected = select_servers_rack_aware(&candidates, REPLICATION_FACTOR);
        (selected, Uuid::new_v4().to_string())
    } else {
        return Err(Status::internal("Wrong state type"));
    }
};
```

**Step 6: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

Expected: clean build.

**Step 7: Run all tests**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```

Expected: all tests pass (at least 19 + 4 new = 23+).

**Step 8: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat: rack-aware replica placement in AllocateBlock"
```

---

### Task 4: Add `--rack-id` arg to ChunkServer binary and send it on registration

**Files:**
- Modify: `dfs/chunkserver/src/bin/chunkserver.rs:33-63` (`Args` struct)
- Modify: `dfs/chunkserver/src/bin/chunkserver.rs` (registration logic)

**Step 1: Add `rack_id` to `Args`**

After `domain_name: Option<String>` (line 62), add:

```rust
/// Rack identifier for rack-aware replica placement.
/// Defaults to empty string (rack unknown).
#[arg(long, default_value = "")]
rack_id: String,
```

**Step 2: Find and update the registration call**

Search for `RegisterChunkServerRequest` in `chunkserver.rs`:

```bash
grep -n "RegisterChunkServerRequest" dfs/chunkserver/src/bin/chunkserver.rs
```

The chunkserver binary currently registers via heartbeat (not a separate RegisterChunkServer call). In the heartbeat loop, the first registration happens implicitly. Actually, looking at the code, the ChunkServer only sends `HeartbeatRequest` — there is no explicit `RegisterChunkServerRequest` call in the binary.

**IMPORTANT: Read `dfs/chunkserver/src/chunkserver.rs` to find where registration happens** (search for `register_chunk_server` calls).

If there is no separate register call (registration happens via first heartbeat), then `rack_id` must travel via `HeartbeatRequest` instead. In that case:

**Alternative step 2a: Add `rack_id` to `HeartbeatRequest` proto instead**

In `proto/dfs.proto`, add to `HeartbeatRequest`:
```proto
message HeartbeatRequest {
  string chunk_server_address = 1;
  uint64 used_space = 2;
  uint64 available_space = 3;
  uint64 chunk_count = 4;
  repeated string bad_blocks = 5;
  string rack_id = 6;
}
```

**Alternative step 2b: Pass rack_id in HeartbeatRequest**

In `chunkserver.rs` binary, find the `HeartbeatRequest { ... }` construction (around line 231) and add `rack_id: args.rack_id.clone()`.

**Alternative step 2c: Update heartbeat handler in master to store rack_id from heartbeat**

In `master.rs` `heartbeat` handler, replace the `existing_rack_id` logic added in Task 2 with:

```rust
// Use rack_id from heartbeat if provided, else preserve existing
let rack_id = if !req.rack_id.is_empty() {
    req.rack_id.clone()
} else {
    state
        .chunk_servers
        .get(&req.chunk_server_address)
        .map(|s| s.rack_id.clone())
        .unwrap_or_default()
};
```

And use `rack_id` instead of `existing_rack_id` in the `ChunkServerStatus` insertion.

**IMPORTANT:** After reading the actual code, pick whichever path applies (HeartbeatRequest or RegisterChunkServerRequest). Both proto messages may need `rack_id` if both are used.

**Step 3: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
cargo build -p dfs-chunkserver 2>&1 | head -20
```

Expected: clean build.

**Step 4: Run all tests**

```bash
cargo test --lib 2>&1 | tail -10
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add proto/dfs.proto dfs/chunkserver/src/bin/chunkserver.rs dfs/metaserver/src/master.rs
git commit -m "feat: propagate rack_id from ChunkServer to Master via heartbeat"
```

---

### Task 5: Update TODO.md

**Step 1: Mark Rack Awareness complete**

In `TODO.md`, replace:
```
- [ ] **Rack Awareness (Tier 2)**
    - [ ] ChunkServerのラック情報をMasterに登録し、ブロックレプリカが異なるラックに配置されるよう `AllocateBlock` を修正。
```

With:
```
- [x] **Rack Awareness (Tier 2)** ✅
    - [x] ChunkServerのラック情報をMasterに登録し、ブロックレプリカが異なるラックに配置されるよう `AllocateBlock` を修正。
```

**Step 2: Final build + test**

```bash
cargo build 2>&1 | tail -5
cargo test --lib 2>&1 | tail -5
```

**Step 3: Commit**

```bash
git add TODO.md
git commit -m "docs: mark Rack Awareness as complete in TODO.md"
```
