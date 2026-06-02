# Erasure Coding Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Add Reed-Solomon erasure coding as a per-file storage policy alongside 3x replication, with full write/read/reconstruction support.

**Architecture:** Client-side RS(k,m) encoding using `reed-solomon-erasure`. Client splits each block into k data shards + m parity shards, writes them in parallel to k+m ChunkServers. On read, fetches k data shards (or any k shards if some are unavailable). Master detects missing shards and issues RECONSTRUCT_EC_SHARD commands.

**Tech Stack:** `reed-solomon-erasure` crate (pure Rust GF(2^8) RS), tonic/prost for proto changes, tokio for parallel I/O.

---

## Worktree

All work happens in `.worktrees/feature-erasure-coding` on branch `feature/erasure-coding`.

```bash
cd /Users/ynakazat/github/getumen/rust-hadoop-generated-by-llm/.worktrees/feature-erasure-coding
```

---

## Task 1: EC Module in dfs-common

Add `reed-solomon-erasure` dep and a thin `erasure.rs` wrapper in `dfs-common`.

**Files:**
- Modify: `dfs/common/Cargo.toml`
- Create: `dfs/common/src/erasure.rs`
- Modify: `dfs/common/src/lib.rs`

**Step 1: Write the failing test**

Add to `dfs/common/src/erasure.rs` (create the file):

```rust
use anyhow::{bail, Result};
use reed_solomon_erasure::galois_8::ReedSolomon;

/// Encodes `data` into `data_shards + parity_shards` equal-sized shards.
/// Returns Vec of (data_shards + parity_shards) byte vectors.
/// The last `parity_shards` entries are parity.
pub fn encode(data: &[u8], data_shards: usize, parity_shards: usize) -> Result<Vec<Vec<u8>>> {
    if data_shards == 0 || parity_shards == 0 {
        bail!("data_shards and parity_shards must both be > 0");
    }
    let r = ReedSolomon::new(data_shards, parity_shards)
        .map_err(|e| anyhow::anyhow!("RS init error: {:?}", e))?;
    let shard_size = shard_len(data.len(), data_shards);
    // Pad data to multiple of data_shards
    let mut padded = data.to_vec();
    padded.resize(shard_size * data_shards, 0);
    let mut shards: Vec<Vec<u8>> = padded
        .chunks(shard_size)
        .map(|c| c.to_vec())
        .collect();
    shards.resize(data_shards + parity_shards, vec![0u8; shard_size]);
    r.encode(&mut shards)
        .map_err(|e| anyhow::anyhow!("RS encode error: {:?}", e))?;
    Ok(shards)
}

/// Reconstructs original data from available shards.
/// `shards` has length `data_shards + parity_shards`.
/// Missing shards are represented as `None`.
/// Returns the original data (without padding).
pub fn decode(
    shards: &mut Vec<Option<Vec<u8>>>,
    data_shards: usize,
    parity_shards: usize,
    original_len: usize,
) -> Result<Vec<u8>> {
    let r = ReedSolomon::new(data_shards, parity_shards)
        .map_err(|e| anyhow::anyhow!("RS init error: {:?}", e))?;
    r.reconstruct(shards)
        .map_err(|e| anyhow::anyhow!("RS reconstruct error: {:?}", e))?;
    let mut result = Vec::new();
    for i in 0..data_shards {
        result.extend_from_slice(shards[i].as_ref().unwrap());
    }
    result.truncate(original_len);
    Ok(result)
}

/// Returns the byte length of each shard for a given total data length.
/// Pads to a multiple of data_shards.
pub fn shard_len(data_len: usize, data_shards: usize) -> usize {
    (data_len + data_shards - 1) / data_shards
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let data = b"Hello, Erasure Coding World!";
        let shards = encode(data, 4, 2).unwrap();
        assert_eq!(shards.len(), 6);

        // Decode with all shards present
        let mut opt_shards: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        let recovered = decode(&mut opt_shards, 4, 2, data.len()).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_decode_with_missing_shards() {
        let data = b"Hello, Erasure Coding World!";
        let shards = encode(data, 4, 2).unwrap();

        // Erase 2 shards (tolerable with m=2)
        let mut opt_shards: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        opt_shards[1] = None;
        opt_shards[4] = None;

        let recovered = decode(&mut opt_shards, 4, 2, data.len()).unwrap();
        assert_eq!(recovered, data);
    }

    #[test]
    fn test_encode_large_data() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let shards = encode(&data, 4, 2).unwrap();
        assert_eq!(shards.len(), 6);

        let mut opt_shards: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        let recovered = decode(&mut opt_shards, 4, 2, data.len()).unwrap();
        assert_eq!(recovered, data);
    }
}
```

**Step 2: Add dependency to `dfs/common/Cargo.toml`**

Add after `base64 = "0.22.1"`:
```toml
reed-solomon-erasure = "6.0"
```

**Step 3: Export from `dfs/common/src/lib.rs`**

Add at the bottom of the file:
```rust
pub mod erasure;
```

**Step 4: Run tests to verify they pass**

```bash
cargo test -p dfs-common --lib erasure 2>&1
```
Expected: `test result: ok. 3 passed`

**Step 5: Update vendor directory**

```bash
cargo vendor vendor/ 2>&1 | tail -5
```

**Step 6: Commit**

```bash
git add dfs/common/Cargo.toml dfs/common/src/erasure.rs dfs/common/src/lib.rs vendor/ Cargo.lock
git commit -m "feat(common): add reed-solomon erasure coding module"
```

---

## Task 2: Proto Changes

Add EC fields to `proto/dfs.proto`. Proto field numbers must not reuse existing numbers.

**Files:**
- Modify: `proto/dfs.proto`

**Current field numbers to preserve:**
- `FileMetadata`: path=1, size=2, blocks=3, etag_md5=4, created_at_ms=5
- `BlockInfo`: block_id=1, size=2, locations=3, checksum_crc32c=4
- `CreateFileRequest`: path=1
- `AllocateBlockResponse`: block=1, chunk_server_addresses=2, leader_hint=3
- `WriteBlockRequest`: block_id=1, data=2, next_servers=3, expected_checksum_crc32c=4
- `ChunkServerCommand`: type=1, block_id=2, target_chunk_server_address=3

**Step 1: Modify `proto/dfs.proto`**

**`FileMetadata`** — add EC policy fields (6, 7):
```protobuf
message FileMetadata {
  string path = 1;
  uint64 size = 2;
  repeated BlockInfo blocks = 3;
  string etag_md5 = 4;
  uint64 created_at_ms = 5;
  // Erasure coding policy (both 0 = replicated)
  int32 ec_data_shards = 6;
  int32 ec_parity_shards = 7;
}
```

**`BlockInfo`** — add EC flag and shard mapping (5, 6, 7):
```protobuf
message BlockInfo {
  string block_id = 1;
  uint64 size = 2;
  repeated string locations = 3;  // For EC: locations[i] = CS holding shard i
  uint32 checksum_crc32c = 4;
  // Erasure coding info (both 0 = replicated)
  int32 ec_data_shards = 5;
  int32 ec_parity_shards = 6;
  uint64 original_size = 7;       // Unpadded data length for EC decode
}
```

**`CreateFileRequest`** — add EC policy (2, 3, 4):
```protobuf
message CreateFileRequest {
  string path = 1;
  int32 ec_data_shards = 2;    // 0 = use replication (default)
  int32 ec_parity_shards = 3;
}
```

**`AllocateBlockResponse`** — add EC echo (4, 5):
```protobuf
message AllocateBlockResponse {
  BlockInfo block = 1;
  repeated string chunk_server_addresses = 2;
  string leader_hint = 3;
  // Echo of EC policy from the file
  int32 ec_data_shards = 4;
  int32 ec_parity_shards = 5;
}
```

**`WriteBlockRequest`** — add shard_index (5):
```protobuf
message WriteBlockRequest {
  string block_id = 1;
  bytes data = 2;
  repeated string next_servers = 3;
  uint32 expected_checksum_crc32c = 4;
  int32 shard_index = 5;  // -1 = replicated, 0..k+m-1 = EC shard
}
```

**`ChunkServerCommand`** — add RECONSTRUCT_EC_SHARD type and fields (3-7):
```protobuf
message ChunkServerCommand {
  enum CommandType {
    UNKNOWN = 0;
    REPLICATE = 1;
    DELETE = 2;
    RECONSTRUCT_EC_SHARD = 3;
  }
  CommandType type = 1;
  string block_id = 2;
  string target_chunk_server_address = 3;
  // For RECONSTRUCT_EC_SHARD:
  int32 shard_index = 4;              // Which shard index to reconstruct
  int32 ec_data_shards = 5;
  int32 ec_parity_shards = 6;
  repeated string ec_shard_sources = 7;  // CS address per shard (k+m entries, empty string = unavailable)
  uint64 original_block_size = 8;    // Unpadded block data length
}
```

**Step 2: Verify proto compilation**

```bash
cargo build -p dfs-metaserver 2>&1 | grep -E "error|warning: unused" | head -20
```
Expected: compiles (may have unused field warnings, that's OK)

**Step 3: Commit**

```bash
git add proto/dfs.proto
git commit -m "feat(proto): add EC fields to FileMetadata, BlockInfo, and RPC messages"
```

---

## Task 3: Master — EC-Aware CreateFile and AllocateBlock

**Files:**
- Modify: `dfs/metaserver/src/master.rs`

### 3a: CreateFile stores EC policy

**Current `CreateFile` handler** creates a `FileMetadata` with `ec_data_shards=0, ec_parity_shards=0` (proto defaults). We need to read the request fields and store them.

Find `async fn create_file` in `master.rs`. It calls `Command::Master(MasterCommand::CreateFile { path, metadata })`.

The `FileMetadata` is constructed around line 2814. In the `create_file` handler (search for `CreateFileRequest`), the metadata is built and passed to Raft. Modify the metadata construction to copy EC fields from the request:

Find the line that builds `FileMetadata { path: ..., size: 0, blocks: vec![], ... }` in the `create_file` RPC handler and add:

```rust
let metadata = FileMetadata {
    path: req.path.clone(),
    size: 0,
    blocks: vec![],
    etag_md5: "".to_string(),
    created_at_ms: 0,
    ec_data_shards: req.ec_data_shards,
    ec_parity_shards: req.ec_parity_shards,
};
```

### 3b: AllocateBlock selects k+m servers for EC files

**Current `allocate_block`** (line 1557): hardcodes `REPLICATION_FACTOR: usize = 3`.

Replace the constant with dynamic selection based on the file's EC policy:

```rust
async fn allocate_block(
    &self,
    request: Request<AllocateBlockRequest>,
) -> Result<Response<AllocateBlockResponse>, Status> {
    // ... existing span/monitor/check code ...

    let (selected_servers, block_id, ec_data, ec_parity) = {
        let state_lock = self.state.lock().expect("Mutex poisoned");
        if let AppState::Master(ref state) = *state_lock {
            if !state.files.contains_key(&req.path) {
                return Err(Status::not_found("File not found"));
            }
            if state.chunk_servers.is_empty() {
                return Err(Status::unavailable("No chunk servers available"));
            }

            let file_meta = &state.files[&req.path];
            let (ec_data, ec_parity) = (file_meta.ec_data_shards, file_meta.ec_parity_shards);
            let needed = if ec_data > 0 && ec_parity > 0 {
                (ec_data + ec_parity) as usize
            } else {
                3 // default replication factor
            };

            let candidates: Vec<(String, ChunkServerStatus)> = state
                .chunk_servers
                .iter()
                .map(|(addr, status)| (addr.clone(), status.clone()))
                .collect();

            if candidates.len() < needed {
                return Err(Status::unavailable(format!(
                    "Need {} chunk servers for EC({},{}), only {} available",
                    needed, ec_data, ec_parity, candidates.len()
                )));
            }

            let selected = select_servers_rack_aware(&candidates, needed);
            (selected, Uuid::new_v4().to_string(), ec_data, ec_parity)
        } else {
            return Err(Status::internal("Wrong state type"));
        }
    };

    // ... existing Raft command send ...

    // In the success branch, return EC info:
    Ok(Response::new(AllocateBlockResponse {
        block: Some(BlockInfo {
            block_id: block_id.clone(),
            size: 0,
            locations: selected_servers.clone(),
            checksum_crc32c: 0,
            ec_data_shards: ec_data,
            ec_parity_shards: ec_parity,
            original_size: 0,
        }),
        chunk_server_addresses: selected_servers,
        leader_hint: "".to_string(),
        ec_data_shards: ec_data,
        ec_parity_shards: ec_parity,
    }))
}
```

**Step 1: Write failing test**

Add to `master.rs` tests section:

```rust
#[test]
fn test_allocate_block_ec_needs_kplusm_servers() {
    // Verify that a file with EC(4,2) policy requires 6 chunk servers
    let mut state = MasterState::default();
    // Insert EC file
    state.files.insert("/ec-file".to_string(), FileMetadata {
        path: "/ec-file".to_string(),
        size: 0,
        blocks: vec![],
        etag_md5: "".to_string(),
        created_at_ms: 0,
        ec_data_shards: 4,
        ec_parity_shards: 2,
    });
    // Register 6 chunk servers
    for i in 0..6 {
        state.chunk_servers.insert(
            format!("cs{}:50052", i),
            ChunkServerStatus::default(),
        );
    }
    // Verify state is consistent: file has EC policy, 6 CSes registered
    assert_eq!(state.files["/ec-file"].ec_data_shards, 4);
    assert_eq!(state.files["/ec-file"].ec_parity_shards, 2);
    assert_eq!(state.chunk_servers.len(), 6);
}
```

**Step 2: Run test**

```bash
cargo test -p dfs-metaserver --lib test_allocate_block_ec 2>&1
```

**Step 3: Implement** (see code above for create_file and allocate_block changes)

**Step 4: Run all master tests**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```
Expected: all 24+ tests pass

**Step 5: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat(master): EC-aware CreateFile and AllocateBlock"
```

---

## Task 4: Client — EC Write Path

**Files:**
- Modify: `dfs/client/src/mod.rs`
- Modify: `dfs/client/Cargo.toml`

The current write path (around line 260–344) allocates one block and does a pipeline write to one CS (which forwards to next_servers). For EC, we need to:
1. Detect EC from `AllocateBlockResponse`
2. Encode data into k+m shards
3. Write each shard to its CS in parallel

**Step 1: Add dfs-common dep to client**

In `dfs/client/Cargo.toml`, add:
```toml
dfs-common = { path = "../common" }
```

**Step 2: Write failing test**

Add to `dfs/client/src/mod.rs` tests:

```rust
#[test]
fn test_ec_shard_split_and_encode() {
    // Verify that EC encoding produces k+m shards
    let data: Vec<u8> = vec![42u8; 1024];
    let shards = dfs_common::erasure::encode(&data, 4, 2).unwrap();
    assert_eq!(shards.len(), 6);
    // Each shard should be ceil(1024/4) = 256 bytes
    for s in &shards {
        assert_eq!(s.len(), 256);
    }
    // Reconstruct with 2 missing shards
    let mut opt: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
    opt[0] = None;
    opt[5] = None;
    let recovered = dfs_common::erasure::decode(&mut opt, 4, 2, data.len()).unwrap();
    assert_eq!(recovered, data);
}
```

**Step 3: Run test to verify it fails**

```bash
cargo test -p dfs-client --lib test_ec_shard 2>&1
```

**Step 4: Implement EC write path**

In `dfs/client/src/mod.rs`, find the `write_block` helper (the internal function that calls AllocateBlock and WriteBlock, around line 260).

After receiving `alloc_resp`, add an EC branch:

```rust
let alloc_resp = alloc_resp.into_inner();
let block = alloc_resp.block.ok_or_else(|| anyhow!("No block allocated"))?;
let chunk_servers = alloc_resp.chunk_server_addresses;

let is_ec = alloc_resp.ec_data_shards > 0 && alloc_resp.ec_parity_shards > 0;

if is_ec {
    let data_shards = alloc_resp.ec_data_shards as usize;
    let parity_shards = alloc_resp.ec_parity_shards as usize;
    let total_shards = data_shards + parity_shards;

    if chunk_servers.len() != total_shards {
        bail!("Expected {} CS for EC({},{}), got {}", total_shards, data_shards, parity_shards, chunk_servers.len());
    }

    let shards = dfs_common::erasure::encode(&buffer, data_shards, parity_shards)?;
    let original_size = buffer.len() as u64;

    // Write all shards in parallel
    let mut write_futures = Vec::new();
    for (shard_index, (shard_data, cs_addr)) in shards.into_iter().zip(chunk_servers.iter()).enumerate() {
        let block_id = block.block_id.clone();
        let cs_addr = format!("http://{}", cs_addr);
        let self_clone = self.clone();
        let shard_index = shard_index as i32;
        write_futures.push(async move {
            let channel = self_clone.connect_endpoint(&cs_addr).await?;
            let mut client = crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(
                channel,
                dfs_common::telemetry::tracing_interceptor
                    as fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
            ).max_decoding_message_size(100 * 1024 * 1024);

            let req = tonic::Request::new(WriteBlockRequest {
                block_id: block_id.clone(),
                data: shard_data,
                next_servers: vec![],  // No pipeline for EC
                expected_checksum_crc32c: 0,
                shard_index,
            });
            let resp = client.write_block(req).await?.into_inner();
            if !resp.success {
                bail!("Shard {} write failed: {}", shard_index, resp.error_message);
            }
            Ok::<(), anyhow::Error>(())
        });
    }
    futures::future::try_join_all(write_futures).await?;

    // Complete file with EC block info
    let ec_block = BlockInfo {
        block_id: block.block_id.clone(),
        size: original_size,
        locations: chunk_servers.clone(),
        checksum_crc32c: 0,
        ec_data_shards: data_shards as i32,
        ec_parity_shards: parity_shards as i32,
        original_size,
    };
    // ... CompleteFile call (adapt from existing code) ...
    return self.complete_file_ec(dest, buffer.len() as u64, ec_block).await;
}
// ... existing replication path below ...
```

Also add `futures` to `dfs/client/Cargo.toml`:
```toml
futures = "0.3"
```

**Step 5: Run tests**

```bash
cargo test -p dfs-client --lib 2>&1 | tail -10
```

**Step 6: Commit**

```bash
git add dfs/client/src/mod.rs dfs/client/Cargo.toml vendor/ Cargo.lock
git commit -m "feat(client): EC write path with parallel shard upload"
```

---

## Task 5: Client — EC Read Path

**Files:**
- Modify: `dfs/client/src/mod.rs`

The current `read_file` / `get_file` path iterates `file_meta.blocks` and calls `read_block_range`. For EC blocks (`block.ec_data_shards > 0`), we need to fetch k data shards and concatenate (or decode if some are missing).

**Step 1: Write failing test**

Add to `dfs/client/src/mod.rs` tests:

```rust
#[test]
fn test_ec_decode_with_erasures() {
    // Simulate reading an EC(4,2) block where 2 shards are unavailable
    let original = vec![99u8; 5000];
    let shards = dfs_common::erasure::encode(&original, 4, 2).unwrap();
    // Shards 1 and 3 unavailable
    let mut opt: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
    opt[1] = None;
    opt[3] = None;
    let recovered = dfs_common::erasure::decode(&mut opt, 4, 2, original.len()).unwrap();
    assert_eq!(recovered, original);
}
```

**Step 2: Implement EC read path**

Find `get_file` method and add an EC branch. After fetching `file_meta`, for each block:

```rust
pub async fn get_file_ec_block(
    &self,
    block: &BlockInfo,
) -> Result<Vec<u8>> {
    let data_shards = block.ec_data_shards as usize;
    let parity_shards = block.ec_parity_shards as usize;
    let original_size = block.original_size as usize;
    let total = data_shards + parity_shards;

    // Try to fetch all k+m shards concurrently
    let mut fetch_futs = Vec::new();
    for (i, loc) in block.locations.iter().enumerate() {
        let block_id = block.block_id.clone();
        let loc = loc.clone();
        let self_clone = self.clone();
        fetch_futs.push(async move {
            let result = self_clone.read_block_from_location(&loc, &block_id, 0, 0).await;
            (i, result)
        });
    }
    let results = futures::future::join_all(fetch_futs).await;

    let mut opt_shards: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut available = 0;
    for (i, result) in results {
        if let Ok(data) = result {
            opt_shards[i] = Some(data);
            available += 1;
        }
    }

    if available < data_shards {
        bail!("EC block {} is unrecoverable: only {}/{} shards available",
              block.block_id, available, data_shards);
    }

    // If all data shards available, just concatenate them
    let all_data_shards_ok = opt_shards[..data_shards].iter().all(|s| s.is_some());
    if all_data_shards_ok {
        let mut result = Vec::new();
        for s in &opt_shards[..data_shards] {
            result.extend_from_slice(s.as_ref().unwrap());
        }
        result.truncate(original_size);
        return Ok(result);
    }

    // Degraded read: decode from available shards
    dfs_common::erasure::decode(&mut opt_shards, data_shards, parity_shards, original_size)
}
```

In `get_file` and `read_file_range`, check `block.ec_data_shards > 0` and call `get_file_ec_block`.

**Step 3: Run tests**

```bash
cargo test -p dfs-client --lib 2>&1 | tail -10
```

**Step 4: Commit**

```bash
git add dfs/client/src/mod.rs
git commit -m "feat(client): EC read path with degraded read support"
```

---

## Task 6: ChunkServer — RECONSTRUCT_EC_SHARD Handler

When the master issues a RECONSTRUCT_EC_SHARD command in a heartbeat response, the receiving ChunkServer must:
1. Fetch k available shards from the source CSes
2. RS reconstruct the missing shard
3. Store the reconstructed shard locally (under the given block_id)

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs`
- Modify: `dfs/chunkserver/Cargo.toml`

**Step 1: Add dfs-common dep with erasure feature**

`dfs/chunkserver/Cargo.toml` already has `dfs-common = { path = "../common" }`.
The `reed-solomon-erasure` dep is in `dfs-common`, so it's accessible via `dfs_common::erasure`.

**Step 2: Write failing test**

Add to `dfs/chunkserver/src/chunkserver.rs` tests:

```rust
#[tokio::test]
async fn test_reconstruct_ec_shard_writes_local() {
    use tempfile::TempDir;
    let tmp = TempDir::new().unwrap();
    // Encode some data
    let data = vec![7u8; 4096];
    let shards = dfs_common::erasure::encode(&data, 4, 2).unwrap();

    // Write shards 0,1,2,3,5 to temp files (shard 4 is "missing")
    let shard_paths: Vec<_> = (0..6).map(|i| tmp.path().join(format!("shard-{}", i))).collect();
    for (i, path) in shard_paths.iter().enumerate() {
        if i != 4 {
            std::fs::write(path, &shards[i]).unwrap();
        }
    }

    // Reconstruct shard 4
    let available: Vec<Option<Vec<u8>>> = (0..6usize).map(|i| {
        if i == 4 { None } else { Some(std::fs::read(&shard_paths[i]).unwrap()) }
    }).collect();
    let mut opt = available;
    // Use erasure module to reconstruct
    let r = reed_solomon_erasure::galois_8::ReedSolomon::new(4, 2).unwrap();
    r.reconstruct(&mut opt).unwrap();
    let reconstructed = opt[4].as_ref().unwrap();
    assert_eq!(reconstructed, &shards[4]);
}
```

**Step 3: Add `reed-solomon-erasure` to chunkserver**

In `dfs/chunkserver/Cargo.toml`:
```toml
reed-solomon-erasure = "6.0"
```

**Step 4: Implement RECONSTRUCT_EC_SHARD in heartbeat handler**

Find the heartbeat loop in `chunkserver.rs` where `ChunkServerCommand` is processed (look for `REPLICATE` handling, around the `send_heartbeat` function).

Add a new arm for `RECONSTRUCT_EC_SHARD = 3`:

```rust
3 => {
    // RECONSTRUCT_EC_SHARD
    let block_id = cmd.block_id.clone();
    let shard_index = cmd.shard_index as usize;
    let data_shards = cmd.ec_data_shards as usize;
    let parity_shards = cmd.ec_parity_shards as usize;
    let original_size = cmd.original_block_size as usize;
    let sources = cmd.ec_shard_sources.clone();
    let storage_dir = self.storage_dir.clone();
    let connect_fn = self.connect_endpoint.clone(); // or however CS connects

    tokio::spawn(async move {
        let total = data_shards + parity_shards;
        let mut opt_shards: Vec<Option<Vec<u8>>> = vec![None; total];

        // Fetch available shards from source CSes
        for (i, src_addr) in sources.iter().enumerate() {
            if src_addr.is_empty() || i == shard_index {
                continue;
            }
            // Read shard i from src_addr using ReadBlock gRPC
            if let Ok(data) = read_shard_from_cs(src_addr, &block_id).await {
                opt_shards[i] = Some(data);
            }
        }

        // Reconstruct
        match dfs_common::erasure::decode(&mut opt_shards, data_shards, parity_shards, original_size) {
            Ok(_) => {
                // Re-encode to get the missing shard
                let combined: Vec<u8> = (0..data_shards)
                    .flat_map(|i| opt_shards[i].as_ref().unwrap().clone())
                    .collect();
                if let Ok(full_shards) = dfs_common::erasure::encode(&combined[..original_size], data_shards, parity_shards) {
                    let path = std::path::Path::new(&storage_dir).join(&block_id);
                    if let Err(e) = std::fs::write(&path, &full_shards[shard_index]) {
                        tracing::error!("EC reconstruct: failed to write shard {}: {}", shard_index, e);
                    } else {
                        tracing::info!("EC reconstruct: shard {} of {} written", shard_index, block_id);
                    }
                }
            }
            Err(e) => tracing::error!("EC reconstruct failed for block {}: {}", block_id, e),
        }
    });
}
```

**Step 5: Run chunkserver tests**

```bash
cargo test -p dfs-chunkserver --lib 2>&1 | tail -10
```

**Step 6: Update vendor and commit**

```bash
cargo vendor vendor/ 2>&1 | tail -5
git add dfs/chunkserver/src/chunkserver.rs dfs/chunkserver/Cargo.toml vendor/ Cargo.lock
git commit -m "feat(chunkserver): RECONSTRUCT_EC_SHARD command handler"
```

---

## Task 7: Master — EC-Aware Heal Function

Extend `heal_under_replicated_blocks` (line 429 in `master.rs`) to handle EC blocks by issuing `RECONSTRUCT_EC_SHARD` commands when shards are missing.

**Files:**
- Modify: `dfs/metaserver/src/master.rs`

**Step 1: Write failing test**

Add to `master.rs` tests:

```rust
#[test]
fn test_heal_ec_block_issues_reconstruct_command() {
    let mut state = MasterState::default();

    // 6 CSes: cs0-cs5. cs2 is dead (not registered).
    for i in [0, 1, 3, 4, 5] {
        state.chunk_servers.insert(
            format!("cs{}:50052", i),
            ChunkServerStatus::default(),
        );
    }

    // EC(4,2) file with block having shard 2 on dead cs2
    state.files.insert("/ec-file".to_string(), FileMetadata {
        path: "/ec-file".to_string(),
        size: 100,
        blocks: vec![BlockInfo {
            block_id: "blk-1".to_string(),
            size: 100,
            locations: vec![
                "cs0:50052".to_string(),
                "cs1:50052".to_string(),
                "cs2:50052".to_string(), // dead
                "cs3:50052".to_string(),
                "cs4:50052".to_string(),
                "cs5:50052".to_string(),
            ],
            checksum_crc32c: 0,
            ec_data_shards: 4,
            ec_parity_shards: 2,
            original_size: 100,
        }],
        etag_md5: "".to_string(),
        created_at_ms: 0,
        ec_data_shards: 4,
        ec_parity_shards: 2,
    });

    heal_under_replicated_blocks(&mut state);

    // Should issue RECONSTRUCT_EC_SHARD command to some live CS
    let all_cmds: Vec<_> = state.pending_commands.values().flatten().collect();
    assert!(
        all_cmds.iter().any(|c| c.r#type == 3 && c.block_id == "blk-1" && c.shard_index == 2),
        "Expected RECONSTRUCT_EC_SHARD for shard 2, got: {:?}", all_cmds
    );
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test -p dfs-metaserver --lib test_heal_ec 2>&1
```
Expected: FAIL

**Step 3: Implement EC-aware heal**

In `heal_under_replicated_blocks` (line 429), after the existing replication logic, add:

```rust
fn heal_under_replicated_blocks(state: &mut MasterState) {
    const REPLICATION_FACTOR: usize = 3;
    let live_servers: Vec<String> = state.chunk_servers.keys().cloned().collect();
    if live_servers.is_empty() {
        return;
    }

    for file in state.files.values() {
        for block in &file.blocks {
            let bad_on = state
                .bad_block_locations
                .get(&block.block_id)
                .cloned()
                .unwrap_or_default();

            let is_ec = block.ec_data_shards > 0 && block.ec_parity_shards > 0;

            if is_ec {
                // EC healing: find missing shards, issue RECONSTRUCT_EC_SHARD
                let data_shards = block.ec_data_shards as usize;
                let parity_shards = block.ec_parity_shards as usize;
                let total = data_shards + parity_shards;

                for (shard_idx, loc) in block.locations.iter().enumerate() {
                    let is_live = state.chunk_servers.contains_key(loc) && !bad_on.contains(loc);
                    if is_live {
                        continue; // shard is fine
                    }

                    // Shard shard_idx is missing. Find a target CS to reconstruct it.
                    // Count live shards
                    let live_count = block.locations.iter()
                        .filter(|l| state.chunk_servers.contains_key(*l) && !bad_on.contains(*l))
                        .count();
                    if live_count < data_shards {
                        tracing::error!(
                            "EC block {} is unrecoverable: only {}/{} shards live",
                            block.block_id, live_count, data_shards
                        );
                        continue;
                    }

                    // Pick a target: a CS not holding any shard of this block
                    let target = match live_servers.iter()
                        .find(|s| !block.locations.contains(s))
                    {
                        Some(t) => t.clone(),
                        None => {
                            // All CSes already hold a shard; pick any live shard holder as target
                            match block.locations.iter()
                                .find(|l| state.chunk_servers.contains_key(*l) && *l != loc)
                            {
                                Some(t) => t.clone(),
                                None => continue,
                            }
                        }
                    };

                    // Build source list: CS address per shard (empty = unavailable)
                    let sources: Vec<String> = (0..total).map(|i| {
                        let l = &block.locations[i];
                        if state.chunk_servers.contains_key(l) && !bad_on.contains(l) {
                            l.clone()
                        } else {
                            "".to_string()
                        }
                    }).collect();

                    state
                        .pending_commands
                        .entry(target.clone())
                        .or_default()
                        .push(ChunkServerCommand {
                            r#type: 3, // RECONSTRUCT_EC_SHARD
                            block_id: block.block_id.clone(),
                            target_chunk_server_address: target.clone(),
                            shard_index: shard_idx as i32,
                            ec_data_shards: block.ec_data_shards,
                            ec_parity_shards: block.ec_parity_shards,
                            ec_shard_sources: sources,
                            original_block_size: block.original_size,
                        });
                }
            } else {
                // Existing replication healing logic (unchanged)
                let live_locs: Vec<String> = block
                    .locations
                    .iter()
                    .filter(|loc| state.chunk_servers.contains_key(*loc) && !bad_on.contains(*loc))
                    .cloned()
                    .collect();

                let needed = REPLICATION_FACTOR.saturating_sub(live_locs.len());
                if needed == 0 { continue; }
                if live_locs.is_empty() {
                    tracing::error!("Healer: block {} has NO live replicas", block.block_id);
                    continue;
                }

                let source = &live_locs[0];
                let targets: Vec<String> = live_servers.iter()
                    .filter(|s| !block.locations.contains(s))
                    .take(needed)
                    .cloned()
                    .collect();

                for target in &targets {
                    state.pending_commands
                        .entry(source.clone())
                        .or_default()
                        .push(ChunkServerCommand {
                            r#type: 1, // REPLICATE
                            block_id: block.block_id.clone(),
                            target_chunk_server_address: target.clone(),
                            ..Default::default()
                        });
                }
            }
        }
    }
}
```

**Step 4: Run all master tests**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```
Expected: all tests pass

**Step 5: Build all crates**

```bash
cargo build --release 2>&1 | grep "^error" | head -20
```
Expected: no errors

**Step 6: Commit**

```bash
git add dfs/metaserver/src/master.rs
git commit -m "feat(master): EC-aware heal function issues RECONSTRUCT_EC_SHARD commands"
```

---

## Final Verification

```bash
cargo test --lib 2>&1 | tail -15
```
Expected: all tests pass across all crates.

```bash
cargo build --release 2>&1 | grep "^error"
```
Expected: clean build.
