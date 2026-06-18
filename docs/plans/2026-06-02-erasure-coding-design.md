# Erasure Coding Design

## Goal

Add Reed-Solomon erasure coding as an alternative storage policy to 3x replication, following the HDFS-EC approach. Files can use either replication or EC on a per-file basis.

## Architecture

**Approach**: Client-side encoding (same as HDFS). The client splits data into k data shards, computes m parity shards using `reed-solomon-erasure`, and writes each shard to a separate ChunkServer in parallel.

**Library**: `reed-solomon-erasure` crate — pure Rust, GF(2^8), production-proven.

**EC scheme**: Configurable k (data shards) and m (parity shards). Common configurations:
- RS(4, 2): 1.5x storage overhead, tolerates 2 CS failures
- RS(6, 3): 2x storage overhead, tolerates 3 CS failures (same overhead as 3x replication but better tolerance)

---

## Data Model

### StoragePolicy (new enum in `master.rs`)

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StoragePolicy {
    Replicated { factor: u8 },             // default: factor=3
    ErasureCoded { data: u8, parity: u8 }, // e.g. data=4, parity=2
}

impl Default for StoragePolicy {
    fn default() -> Self {
        StoragePolicy::Replicated { factor: 3 }
    }
}
```

### FileMetadata changes

```rust
pub struct FileMetadata {
    // existing fields...
    pub storage_policy: StoragePolicy,  // added
}
```

### Block Group

For EC files, one logical block becomes a **block group** of `data + parity` shards. Each shard is stored on a different ChunkServer.

```
Block group "uuid-bg-0" with RS(4,2):
  shard 0 → CS1  (data)
  shard 1 → CS2  (data)
  shard 2 → CS3  (data)
  shard 3 → CS4  (data)
  shard 4 → CS5  (parity)
  shard 5 → CS6  (parity)
```

---

## Proto Changes (`proto/dfs.proto`)

### CreateFileRequest — add storage policy

```protobuf
message CreateFileRequest {
  string path = 1;
  // new:
  bool erasure_coded = 2;
  int32 ec_data_shards = 3;    // 0 = use server default
  int32 ec_parity_shards = 4;
}
```

### AllocateBlockResponse — return EC info

```protobuf
message AllocateBlockResponse {
  string block_id = 1;
  repeated string chunk_servers = 2;  // k+m servers for EC, 3 for replication
  // new:
  bool is_erasure_coded = 3;
  int32 data_shards = 4;
  int32 parity_shards = 5;
}
```

### WriteBlockRequest — add shard index

```protobuf
message WriteBlockRequest {
  string block_id = 1;
  bytes data = 2;
  repeated string next_servers = 3;  // empty for EC (parallel, not pipeline)
  int32 shard_index = 4;             // new: 0..k+m-1; -1 for replicated
}
```

### BlockLocation — track shard index

```protobuf
message BlockLocation {
  string block_id = 1;
  repeated string chunk_servers = 2;
  // new:
  bool is_erasure_coded = 3;
  int32 data_shards = 4;
  int32 parity_shards = 5;
  repeated int32 shard_indices = 6;  // which shard each CS holds
}
```

---

## Write Path

```
Client                          Master                    ChunkServers
  │                               │                            │
  ├─ CreateFile(path, EC(4,2)) ──>│                            │
  ├─ AllocateBlock() ────────────>│                            │
  │<─ [CS1,CS2,CS3,CS4,CS5,CS6] ─┤                            │
  │                               │                            │
  │  Split data → 4 shards        │                            │
  │  Compute 2 parity shards      │                            │
  │  (reed-solomon-erasure)       │                            │
  │                               │                            │
  ├─ WriteBlock(shard=0) ─────────┼──────────────────────────>│ CS1
  ├─ WriteBlock(shard=1) ─────────┼──────────────────────────>│ CS2
  ├─ WriteBlock(shard=2) ─────────┼──────────────────────────>│ CS3
  ├─ WriteBlock(shard=3) ─────────┼──────────────────────────>│ CS4
  ├─ WriteBlock(shard=4) ─────────┼──────────────────────────>│ CS5
  ├─ WriteBlock(shard=5) ─────────┼──────────────────────────>│ CS6
  │  (all parallel)               │                            │
  │                               │                            │
  ├─ CompleteFile() ─────────────>│                            │
```

Key difference from replication: shards are written **in parallel** (not pipeline).

---

## Read Path

### Normal read (all shards available)

Read only the k data shards — parity shards are not needed. No decode overhead.

```
Client → CS1 (shard 0), CS2 (shard 1), CS3 (shard 2), CS4 (shard 3)
       ← concatenate shards → full data
```

### Degraded read (some shards unavailable)

Fetch any k available shards (data or parity), then decode with RS.

```
Condition: available_shards >= k  → decode and return
Condition: available_shards < k   → error (unrecoverable)
```

---

## Reconstruction (Auto-Repair)

The Master runs a background task that detects missing shards and triggers repair.

```
1. Master monitors CS health via heartbeats
2. On CS failure: identify all block groups with shards on that CS
3. For each affected block group:
   a. Fetch k available shards from surviving CSes
   b. RS decode → reconstruct missing shards
   c. Write reconstructed shards to new healthy CSes
   d. Update BlockLocation metadata in Raft log
```

The reconstruction logic reuses the client-side encode/decode code (shared via `dfs-common` or the client crate).

---

## Component Summary

| Component | Change |
|-----------|--------|
| `proto/dfs.proto` | Add EC fields to CreateFile, AllocateBlock, WriteBlock, BlockLocation |
| `dfs/metaserver/src/master.rs` | StoragePolicy in FileMetadata, allocate k+m CSes for EC, background reconstruction task |
| `dfs/client/src/mod.rs` | EC write path (shard + parallel write), EC read path (fetch + decode) |
| `dfs/common` | New `erasure` module: RS encode/decode wrapper around `reed-solomon-erasure` |
| `Cargo.toml` | Add `reed-solomon-erasure` dependency |
| `vendor/` | Run `cargo vendor` after adding dependency |
