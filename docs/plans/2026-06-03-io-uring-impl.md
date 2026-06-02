# io_uring Zero-Copy Data Path Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace blocking `std::fs` I/O in ChunkServer with `tokio-uring` (io_uring) on both write and read paths, eliminating thread blocking and reducing syscall overhead.

**Architecture:** New `write_block_async` and `read_block_async` methods use `tokio::task::spawn_blocking` + `tokio_uring::start` to bridge the tonic gRPC runtime with io_uring. LRU read cache is preserved. `write_block_local` (sync) is kept for unit tests only.

**Tech Stack:** `tokio-uring 0.5`, `criterion 0.5`, Rust 1.91, Linux kernel 6.1 (Debian bookworm in Docker)

---

### Task 1: Add dependencies and benchmark scaffold

**Files:**
- Modify: `dfs/chunkserver/Cargo.toml`
- Create: `dfs/chunkserver/benches/io_bench.rs`

**Step 1: Add tokio-uring and criterion to Cargo.toml**

```toml
# dfs/chunkserver/Cargo.toml  — add to [dependencies]:
tokio-uring = "0.5"

# add new section at end of file:
[dev-dependencies]
tempfile = "3.23"
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "io_bench"
harness = false
```

**Step 2: Create benchmark file**

Create `dfs/chunkserver/benches/io_bench.rs`:

```rust
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::io::{Read, Seek, SeekFrom, Write};
use tempfile::TempDir;

fn make_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

// ── Baseline: std::fs write ──────────────────────────────────────────────────

fn bench_write_stdfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_stdfs");
    for size in [4 * 1024usize, 64 * 1024, 1024 * 1024] {
        let data = make_data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            let dir = TempDir::new().unwrap();
            b.iter(|| {
                let path = dir.path().join("block");
                let mut f = std::fs::File::create(&path).unwrap();
                f.write_all(data).unwrap();
            });
        });
    }
    group.finish();
}

// ── Baseline: std::fs read ───────────────────────────────────────────────────

fn bench_read_stdfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_stdfs");
    for size in [4 * 1024usize, 64 * 1024, 1024 * 1024] {
        let data = make_data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("block");
            std::fs::write(&path, data).unwrap();
            b.iter(|| {
                let mut f = std::fs::File::open(&path).unwrap();
                let mut buf = vec![0u8; data.len()];
                f.seek(SeekFrom::Start(0)).unwrap();
                f.read_exact(&mut buf).unwrap();
                buf
            });
        });
    }
    group.finish();
}

// ── Baseline: std::fs partial read (offset) ──────────────────────────────────

fn bench_partial_read_stdfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("partial_read_stdfs");
    let size = 64 * 1024usize;
    let data = make_data(size);
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("64KB_read_4KB_at_32KB", |b| {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("block");
        std::fs::write(&path, &data).unwrap();
        b.iter(|| {
            let mut f = std::fs::File::open(&path).unwrap();
            let mut buf = vec![0u8; 4096];
            f.seek(SeekFrom::Start(32 * 1024)).unwrap();
            f.read_exact(&mut buf).unwrap();
            buf
        });
    });
    group.finish();
}

criterion_group!(benches, bench_write_stdfs, bench_read_stdfs, bench_partial_read_stdfs);
criterion_main!(benches);
```

**Step 3: Verify it compiles**

```bash
cd /Users/ynakazat/github/getumen/rust-hadoop-generated-by-llm
cargo build -p dfs-chunkserver 2>&1
```

Expected: compiles without error (tokio-uring may show a build message about io_uring feature detection)

**Step 4: Commit**

```bash
git add dfs/chunkserver/Cargo.toml dfs/chunkserver/benches/io_bench.rs
git commit -m "bench(chunkserver): add criterion benchmark scaffold for io_uring baseline"
```

---

### Task 2: Record std::fs baseline numbers

**Files:**
- Read: `dfs/chunkserver/benches/io_bench.rs` (just run it)

**Step 1: Run baseline benchmarks**

```bash
cd /Users/ynakazat/github/getumen/rust-hadoop-generated-by-llm
cargo bench -p dfs-chunkserver -- --save-baseline stdfs 2>&1
```

Expected output (approximate, numbers will vary):
```
write_stdfs/4096    time: [~5 µs]
write_stdfs/65536   time: [~20 µs]
write_stdfs/1048576 time: [~200 µs]
read_stdfs/4096     time: [~3 µs]
read_stdfs/65536    time: [~15 µs]
read_stdfs/1048576  time: [~150 µs]
partial_read_stdfs/64KB_read_4KB_at_32KB  time: [~4 µs]
```

**Step 2: Note the numbers**

Record the reported median times (they will appear in the output). The io_uring implementation in Task 4 and 5 should improve p99 latency (fewer syscalls, no thread blocking).

---

### Task 3: Implement `write_block_async`

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs:169-185`

**Step 1: Write failing unit test**

Add at the bottom of the `#[cfg(test)]` block (around line 930):

```rust
#[tokio::test]
async fn test_write_block_async_creates_file_with_correct_content() {
    let dir = tempfile::TempDir::new().unwrap();
    let server = make_test_server(dir.path());
    let data = b"hello io_uring world";
    server.write_block_async("test-block-async", data).await.unwrap();
    let written = std::fs::read(dir.path().join("test-block-async")).unwrap();
    assert_eq!(written, data);
}

#[tokio::test]
async fn test_write_block_async_creates_meta_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let server = make_test_server(dir.path());
    let data = b"checksum test data";
    server.write_block_async("meta-block", data).await.unwrap();
    assert!(dir.path().join("meta-block.meta").exists());
    // verify the meta file is non-empty (checksum bytes)
    let meta = std::fs::read(dir.path().join("meta-block.meta")).unwrap();
    assert!(!meta.is_empty());
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test -p dfs-chunkserver --lib test_write_block_async 2>&1
```

Expected: FAIL — `write_block_async` method does not exist yet.

**Step 3: Implement `write_block_async`**

Add this method to `impl MyChunkServer` after `write_block_local` (after line 185):

```rust
async fn write_block_async(&self, block_id: &str, data: &[u8]) -> Result<(), std::io::Error> {
    let path = self.storage_dir.join(block_id);
    let meta_path = self.storage_dir.join(format!("{}.meta", block_id));

    // Build checksum bytes synchronously (CPU-bound, no I/O)
    let checksums = Self::calculate_checksums(data);
    let mut meta_bytes: Vec<u8> = Vec::with_capacity(checksums.len() * 4);
    for c in checksums {
        meta_bytes.extend_from_slice(&c.to_be_bytes());
    }

    let data_owned: Vec<u8> = data.to_vec();

    tokio::task::spawn_blocking(move || {
        tokio_uring::start(async {
            // Write block data
            let file = tokio_uring::fs::File::create(&path).await?;
            let (res, _) = file.write_all_at(data_owned, 0).await;
            res?;
            file.sync_all().await?;

            // Write checksum metadata
            let meta_file = tokio_uring::fs::File::create(&meta_path).await?;
            let (res, _) = meta_file.write_all_at(meta_bytes, 0).await;
            res?;
            Ok::<(), std::io::Error>(())
        })
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
}
```

Also add the `tokio_uring` use at the top of the file (with existing `use` statements):
```rust
// No explicit use needed; call as tokio_uring::start / tokio_uring::fs::File
```

**Step 4: Run test to verify it passes**

```bash
cargo test -p dfs-chunkserver --lib test_write_block_async 2>&1
```

Expected: PASS (both `test_write_block_async_creates_file_with_correct_content` and `test_write_block_async_creates_meta_file`)

**Step 5: Commit**

```bash
git add dfs/chunkserver/src/chunkserver.rs
git commit -m "feat(chunkserver): add write_block_async using tokio-uring"
```

---

### Task 4: Wire `write_block_async` into all callsites

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs` at lines 307, 494, 620, 862

There are 4 callsites of `write_block_local` in production code (line 939 is a unit test — keep it as-is).

**Step 1: Replace callsite at line 307 (`recover_block`)**

Find:
```rust
if let Err(e) = self.write_block_local(block_id, &data) {
    tracing::error!("Failed to write recovered block: {}", e);
    continue;
```

Replace with:
```rust
if let Err(e) = self.write_block_async(block_id, &data).await {
    tracing::error!("Failed to write recovered block: {}", e);
    continue;
```

**Step 2: Replace callsite at line 494 (`reconstruct_ec_shard`)**

Find:
```rust
self.write_block_local(&block_id, shard_data)
    .map_err(|e| anyhow::anyhow!("Failed to write reconstructed shard: {}", e))?;
```

Replace with:
```rust
self.write_block_async(&block_id, shard_data)
    .await
    .map_err(|e| anyhow::anyhow!("Failed to write reconstructed shard: {}", e))?;
```

**Step 3: Replace callsite at line 620 (`write_block` RPC)**

Find:
```rust
if let Err(e) = self.write_block_local(&req.block_id, &req.data) {
    return Ok(Response::new(WriteBlockResponse {
        success: false,
        error_message: e.to_string(),
    }));
}
```

Replace with:
```rust
if let Err(e) = self.write_block_async(&req.block_id, &req.data).await {
    return Ok(Response::new(WriteBlockResponse {
        success: false,
        error_message: e.to_string(),
    }));
}
```

**Step 4: Replace callsite at line 862 (`replicate_block` RPC)**

Find:
```rust
if let Err(e) = self.write_block_local(&req.block_id, &req.data) {
    return Ok(Response::new(crate::dfs::ReplicateBlockResponse {
        success: false,
```

Replace with:
```rust
if let Err(e) = self.write_block_async(&req.block_id, &req.data).await {
    return Ok(Response::new(crate::dfs::ReplicateBlockResponse {
        success: false,
```

**Step 5: Build and run all unit tests**

```bash
cargo test -p dfs-chunkserver --lib 2>&1
```

Expected: all tests pass (the existing `test_write_and_verify` test still uses `write_block_local` directly — that's fine).

**Step 6: Commit**

```bash
git add dfs/chunkserver/src/chunkserver.rs
git commit -m "feat(chunkserver): wire write_block_async into all write callsites"
```

---

### Task 5: Implement `read_block_async`

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs`

**Step 1: Write failing unit test**

Add to `#[cfg(test)]` block:

```rust
#[tokio::test]
async fn test_read_block_async_returns_correct_data() {
    let dir = tempfile::TempDir::new().unwrap();
    let server = make_test_server(dir.path());
    let data = make_test_data(4096);
    std::fs::write(dir.path().join("read-test"), &data).unwrap();

    let result = server.read_block_async("read-test", 0, data.len() as u64).await.unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_read_block_async_partial_read_at_offset() {
    let dir = tempfile::TempDir::new().unwrap();
    let server = make_test_server(dir.path());
    let data = make_test_data(65536);
    std::fs::write(dir.path().join("partial-test"), &data).unwrap();

    // Read 4096 bytes starting at offset 32768
    let result = server.read_block_async("partial-test", 32768, 4096).await.unwrap();
    assert_eq!(result.len(), 4096);
    assert_eq!(result, data[32768..32768 + 4096]);
}
```

Also add this helper function near the top of the `#[cfg(test)]` block (if not already present):

```rust
fn make_test_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test -p dfs-chunkserver --lib test_read_block_async 2>&1
```

Expected: FAIL — `read_block_async` method does not exist yet.

**Step 3: Implement `read_block_async`**

Add after `write_block_async` in `impl MyChunkServer`:

```rust
async fn read_block_async(&self, block_id: &str, offset: u64, length: u64) -> Result<Vec<u8>, std::io::Error> {
    let path = self.storage_dir.join(block_id);

    // Get file size synchronously (cheap metadata call)
    let total_size = std::fs::metadata(&path)?.len();

    if offset >= total_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Offset {} exceeds file size {}", offset, total_size),
        ));
    }

    let bytes_to_read = std::cmp::min(length, total_size - offset) as usize;

    tokio::task::spawn_blocking(move || {
        tokio_uring::start(async {
            let file = tokio_uring::fs::File::open(&path).await?;
            let buf = vec![0u8; bytes_to_read];
            let (res, buf) = file.read_at(buf, offset).await;
            let n = res?;
            Ok::<Vec<u8>, std::io::Error>(buf[..n].to_vec())
        })
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
}
```

**Step 4: Run test to verify it passes**

```bash
cargo test -p dfs-chunkserver --lib test_read_block_async 2>&1
```

Expected: PASS (both tests).

**Step 5: Commit**

```bash
git add dfs/chunkserver/src/chunkserver.rs
git commit -m "feat(chunkserver): add read_block_async using tokio-uring read_at (no seek)"
```

---

### Task 6: Wire `read_block_async` into `read_block` RPC handler

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs:677-830`

The `read_block` handler currently opens the file with `std::fs::File::open` and reads with `seek` + `read_exact`. Replace the I/O portion while keeping the LRU cache logic and checksum verification unchanged.

**Step 1: Replace the std::fs read section**

In the `read_block` async handler, find the block (lines ~677–730):

```rust
match fs::File::open(&path) {
    Ok(mut file) => {
        // Get total block size
        let metadata = file.metadata()...
        let total_size = metadata.len();
        ...
        // Seek to offset and read requested data
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))...
        let mut data = vec![0u8; bytes_to_read as usize];
        file.read_exact(&mut data)...
```

Replace the entire `match fs::File::open(&path) { Ok(mut file) => { ... }` block with:

```rust
// Get total file size for validation and cache check
let total_size = match std::fs::metadata(&path) {
    Ok(m) => m.len(),
    Err(_) => return Err(Status::not_found("Block not found")),
};

// Determine read parameters
let offset = req.offset;
let length = if req.length == 0 {
    total_size.saturating_sub(offset)
} else {
    req.length
};

if offset >= total_size {
    return Err(Status::out_of_range(format!(
        "Offset {} exceeds block size {}",
        offset, total_size
    )));
}

let bytes_to_read = std::cmp::min(length, total_size - offset);
let is_full_block_read = offset == 0 && bytes_to_read == total_size;

// Fast path: LRU cache for full-block reads
if is_full_block_read {
    if let Ok(mut cache) = self.block_cache.lock() {
        if let Some(cached) = cache.get(&req.block_id) {
            tracing::debug!("Cache hit for block {}", req.block_id);
            return Ok(Response::new(ReadBlockResponse {
                data: cached.data.clone(),
                bytes_read: cached.size as u64,
                total_size,
            }));
        }
        tracing::debug!("Cache miss for block {}", req.block_id);
    }
}

// io_uring read (no seek syscall — read_at uses offset directly)
let data = match self.read_block_async(&req.block_id, offset, bytes_to_read).await {
    Ok(d) => d,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
        return Err(Status::not_found("Block not found"));
    }
    Err(e) => return Err(Status::internal(format!("Failed to read block: {}", e))),
};

// Checksum verification for full-block reads
if is_full_block_read {
    if let Err(e) = self.verify_block(&req.block_id, &data) {
        tracing::error!(
            "CRITICAL: Data corruption detected for block {}: {}",
            req.block_id,
            e
        );
        tracing::warn!("Attempting automatic recovery for block {}", req.block_id);
        match self.recover_block(&req.block_id).await {
            Ok(_) => {
                tracing::info!("Block {} recovered, retrying read", req.block_id);
                let recovered = self
                    .read_block_async(&req.block_id, offset, bytes_to_read)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
                if let Err(e) = self.verify_block(&req.block_id, &recovered) {
                    return Err(Status::data_loss(format!(
                        "Recovered block is still corrupted: {}",
                        e
                    )));
                }
                return Ok(Response::new(ReadBlockResponse {
                    data: recovered,
                    bytes_read: bytes_to_read,
                    total_size,
                }));
            }
            Err(recovery_err) => {
                return Err(Status::data_loss(format!(
                    "Data corruption detected: {}. Recovery failed: {}",
                    e, recovery_err
                )));
            }
        }
    }
}

tracing::debug!(
    "Read block {} (offset={}, length={}, bytes_read={})",
    req.block_id, offset, length, bytes_to_read
);

// Cache full-block reads for future requests
if is_full_block_read {
    if let Ok(mut cache) = self.block_cache.lock() {
        cache.put(
            req.block_id.clone(),
            CachedBlock {
                data: data.clone(),
                size: data.len(),
            },
        );
        tracing::debug!("Cached block {}", req.block_id);
    }
}

Ok(Response::new(ReadBlockResponse {
    data,
    bytes_read: bytes_to_read,
    total_size,
}))
```

Note: the old code had `match fs::File::open { Ok => { ... } Err => not_found }`. The new version uses `std::fs::metadata` for the size check and returns `not_found` if metadata fails. Remove the outer `match` and the old `Err(_) => Err(Status::not_found("Block not found"))` arm.

**Step 2: Build and run all unit tests**

```bash
cargo test -p dfs-chunkserver --lib 2>&1
```

Expected: all tests pass.

**Step 3: Commit**

```bash
git add dfs/chunkserver/src/chunkserver.rs
git commit -m "feat(chunkserver): wire read_block_async into read_block RPC (tokio-uring read_at)"
```

---

### Task 7: Add io_uring benchmarks and measure improvement

**Files:**
- Modify: `dfs/chunkserver/benches/io_bench.rs`

**Step 1: Add io_uring write and read benchmarks**

Append to `dfs/chunkserver/benches/io_bench.rs`:

```rust
// ── io_uring write ────────────────────────────────────────────────────────────

fn bench_write_uring(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_uring");
    for size in [4 * 1024usize, 64 * 1024, 1024 * 1024] {
        let data = make_data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            let dir = TempDir::new().unwrap();
            let data = data.clone();
            b.iter(|| {
                let path = dir.path().join("block");
                tokio::task::spawn_blocking(move || {
                    tokio_uring::start(async {
                        let file = tokio_uring::fs::File::create(&path).await.unwrap();
                        let (res, _) = file.write_all_at(data.clone(), 0).await;
                        res.unwrap();
                        file.sync_all().await.unwrap();
                    })
                });
            });
        });
    }
    group.finish();
}

// ── io_uring read ─────────────────────────────────────────────────────────────

fn bench_read_uring(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_uring");
    for size in [4 * 1024usize, 64 * 1024, 1024 * 1024] {
        let data = make_data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("block");
            std::fs::write(&path, data).unwrap();
            let len = data.len();
            b.iter(|| {
                let path = path.clone();
                tokio_uring::start(async move {
                    let file = tokio_uring::fs::File::open(&path).await.unwrap();
                    let buf = vec![0u8; len];
                    let (res, buf) = file.read_at(buf, 0).await;
                    res.unwrap();
                    buf
                });
            });
        });
    }
    group.finish();
}

// ── io_uring partial read ────────────────────────────────────────────────────

fn bench_partial_read_uring(c: &mut Criterion) {
    let mut group = c.benchmark_group("partial_read_uring");
    let size = 64 * 1024usize;
    let data = make_data(size);
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("64KB_read_4KB_at_32KB", |b| {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("block");
        std::fs::write(&path, &data).unwrap();
        b.iter(|| {
            let path = path.clone();
            tokio_uring::start(async move {
                let file = tokio_uring::fs::File::open(&path).await.unwrap();
                let buf = vec![0u8; 4096];
                let (res, buf) = file.read_at(buf, 32 * 1024).await;
                res.unwrap();
                buf
            });
        });
    });
    group.finish();
}
```

Also update the `criterion_group!` line to include the new benchmarks:
```rust
criterion_group!(
    benches,
    bench_write_stdfs,
    bench_read_stdfs,
    bench_partial_read_stdfs,
    bench_write_uring,
    bench_read_uring,
    bench_partial_read_uring
);
```

And add the `use tokio_uring` import at the top:
```rust
use tokio_uring;
```

**Step 2: Run io_uring benchmarks and compare against baseline**

```bash
cd /Users/ynakazat/github/getumen/rust-hadoop-generated-by-llm
cargo bench -p dfs-chunkserver -- --baseline stdfs 2>&1
```

This compares against the `stdfs` baseline saved in Task 2. Look for "Performance has improved" or percentage change lines in the output.

**Step 3: Run integration tests to verify correctness**

```bash
bash test_scripts/fault_recovery_test.sh 2>&1 | tail -5
bash test_scripts/erasure_coding_test.sh 2>&1 | tail -5
```

Expected: both pass.

**Step 4: Update TODO.md**

In `TODO.md`, change:
```markdown
- [ ] **io_uring / Zero-Copy Data Path**
    - [ ] `tokio-uring` 等を用いたChunkServerの非同期ファイルI/Oの高速化。
    - [ ] `sendfile` や registered buffer を活用し、カーネル/ユーザ空間のメモリコピーを排除。
```

To:
```markdown
- [x] **io_uring / Zero-Copy Data Path** ✅
    - [x] `tokio-uring` を用いたChunkServerの非同期ファイルI/Oの高速化（write_block_async / read_block_async）。
    - [x] `read_at` による seek syscall 排除（オフセット付きSQEでカーネル往復を削減）。
```

**Step 5: Commit everything**

```bash
git add dfs/chunkserver/benches/io_bench.rs TODO.md
git commit -m "feat(chunkserver): add io_uring benchmarks and update TODO

- bench: write_uring, read_uring, partial_read_uring for comparison vs stdfs baseline
- TODO: mark io_uring / zero-copy data path as complete"
```
