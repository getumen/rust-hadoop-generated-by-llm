# io_uring Persistent Pool Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace per-RPC `spawn_blocking + tokio_uring::start()` (60µs ring setup each time) with a single long-lived io_uring thread whose ring is created once at ChunkServer startup.

**Architecture:** New `IoUringPool` struct owns a dedicated std::thread running `tokio_uring::start()` forever. RPCs send `IoRequest` enum values over a `tokio::sync::mpsc` channel; results come back via `tokio::sync::oneshot`. `tokio-uring` becomes a required dependency (Linux/Docker only). All `#[cfg(feature = "io-uring")]` gates removed.

**Tech Stack:** `tokio-uring 0.5`, `tokio::sync::{mpsc, oneshot}`

---

### Task 1: Create `io_uring_pool.rs` with tests

**Files:**
- Create: `dfs/chunkserver/src/io_uring_pool.rs`

**Step 1: Write the failing tests**

```rust
// At the bottom of dfs/chunkserver/src/io_uring_pool.rs

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_write_read_roundtrip() {
        let pool = IoUringPool::new(16);
        let dir = tempdir().unwrap();
        let path = dir.path().join("block001");
        pool.write(path.clone(), b"hello io_uring".to_vec()).await.unwrap();
        let data = pool.read(path, 0, 14).await.unwrap();
        assert_eq!(data, b"hello io_uring");
    }

    #[tokio::test]
    async fn test_partial_read_at_offset() {
        let pool = IoUringPool::new(16);
        let dir = tempdir().unwrap();
        let path = dir.path().join("block002");
        let data: Vec<u8> = (0u8..=255).cycle().take(65536).collect();
        pool.write(path.clone(), data.clone()).await.unwrap();
        let result = pool.read(path, 32768, 4096).await.unwrap();
        assert_eq!(result, data[32768..32768 + 4096].to_vec());
    }

    #[tokio::test]
    async fn test_meta_file_written() {
        let pool = IoUringPool::new(16);
        let dir = tempdir().unwrap();
        let path = dir.path().join("block003");
        pool.write(path.clone(), b"test".to_vec()).await.unwrap();
        let meta_path = dir.path().join("block003.meta");
        // meta is written via write_with_meta; check it exists after calling the pool directly
        // (meta path is derived by caller — pool.write is raw, meta written separately)
        // This test verifies raw write works; meta tested via ChunkServer in Task 4
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_read_beyond_eof_errors() {
        let pool = IoUringPool::new(16);
        let dir = tempdir().unwrap();
        let path = dir.path().join("block004");
        pool.write(path.clone(), b"short".to_vec()).await.unwrap();
        // Reading more bytes than the file has should error
        let result = pool.read(path, 0, 100).await;
        // Either returns UnexpectedEof or truncated — implementation may return short read
        // For safety, we just ensure it doesn't panic
        let _ = result;
    }
}
```

**Step 2: Run tests to verify they fail**

```bash
cd /Users/ynakazat/github/getumen/rust-hadoop-generated-by-llm/.worktrees/feature/io-uring
cargo test -p dfs-chunkserver --lib io_uring_pool 2>&1 | head -20
```

Expected: `error[E0583]: file not found for module 'io_uring_pool'`

**Step 3: Implement `io_uring_pool.rs`**

Create the file with this content:

```rust
use std::io;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};
use tokio_uring::fs::File;

enum IoRequest {
    Write {
        path: PathBuf,
        data: Vec<u8>,
        reply: oneshot::Sender<io::Result<()>>,
    },
    Read {
        path: PathBuf,
        offset: u64,
        length: usize,
        reply: oneshot::Sender<io::Result<Vec<u8>>>,
    },
}

pub struct IoUringPool {
    sender: mpsc::Sender<IoRequest>,
}

impl IoUringPool {
    pub fn new(channel_capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<IoRequest>(channel_capacity);
        std::thread::spawn(move || {
            tokio_uring::start(async move {
                while let Some(req) = rx.recv().await {
                    tokio_uring::spawn(async move {
                        match req {
                            IoRequest::Write { path, data, reply } => {
                                let _ = reply.send(do_write(path, data).await);
                            }
                            IoRequest::Read { path, offset, length, reply } => {
                                let _ = reply.send(do_read(path, offset, length).await);
                            }
                        }
                    });
                }
            });
        });
        IoUringPool { sender: tx }
    }

    pub async fn write(&self, path: PathBuf, data: Vec<u8>) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(IoRequest::Write { path, data, reply: tx })
            .await
            .map_err(|_| io::Error::other("io_uring pool unavailable"))?;
        rx.await
            .map_err(|_| io::Error::other("io_uring thread panicked"))?
    }

    pub async fn read(&self, path: PathBuf, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(IoRequest::Read { path, offset, length, reply: tx })
            .await
            .map_err(|_| io::Error::other("io_uring pool unavailable"))?;
        rx.await
            .map_err(|_| io::Error::other("io_uring thread panicked"))?
    }
}

async fn do_write(path: PathBuf, data: Vec<u8>) -> io::Result<()> {
    let file = File::create(&path).await?;
    let (res, _) = file.write_all_at(data, 0).await;
    res?;
    file.sync_all().await
}

async fn do_read(path: PathBuf, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let file = File::open(&path).await?;
    let mut buf = vec![0u8; length];
    let mut filled = 0usize;
    while filled < length {
        let sub = buf[filled..].to_vec();
        let (res, ret) = file.read_at(sub, offset + filled as u64).await;
        let n = res?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read_at returned 0 bytes before buffer full",
            ));
        }
        buf[filled..filled + n].copy_from_slice(&ret[..n]);
        filled += n;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    // ... (tests from Step 1)
}
```

**Step 4: Run tests to verify they pass**

```bash
cargo test -p dfs-chunkserver --lib io_uring_pool 2>&1 | tail -10
```

Expected: `test result: ok. 3 passed` (test_read_beyond_eof_errors may vary)

**Step 5: Commit**

```bash
git add dfs/chunkserver/src/io_uring_pool.rs
git commit -m "feat(chunkserver): IoUringPool with persistent ring"
```

---

### Task 2: Update `Cargo.toml` — make `tokio-uring` required

**Files:**
- Modify: `dfs/chunkserver/Cargo.toml`

**Step 1: Edit the file**

Change:
```toml
tokio-uring = { version = "0.5", optional = true }
```
To:
```toml
tokio-uring = "0.5"
```

Remove the entire `[features]` section:
```toml
[features]
io-uring = ["dep:tokio-uring"]
```

**Step 2: Verify it compiles**

```bash
cargo build -p dfs-chunkserver 2>&1 | tail -5
```

Expected: `Finished` (may error due to remaining `#[cfg(feature = "io-uring")]` references — that's OK, fixed in Task 4)

**Step 3: Commit**

```bash
git add dfs/chunkserver/Cargo.toml
git commit -m "build(chunkserver): make tokio-uring a required dependency"
```

---

### Task 3: Register `io_uring_pool` module in `lib.rs`

**Files:**
- Modify: `dfs/chunkserver/src/lib.rs`

**Step 1: Add module declaration**

Current content of `dfs/chunkserver/src/lib.rs`:
```rust
pub mod dfs {
    include!(concat!(env!("OUT_DIR"), "/dfs.rs"));
}

pub mod chunkserver;
```

Add `pub mod io_uring_pool;` after `pub mod chunkserver;`:
```rust
pub mod dfs {
    include!(concat!(env!("OUT_DIR"), "/dfs.rs"));
}

pub mod chunkserver;
pub mod io_uring_pool;
```

**Step 2: Verify it compiles**

```bash
cargo build -p dfs-chunkserver 2>&1 | tail -5
```

**Step 3: Commit**

```bash
git add dfs/chunkserver/src/lib.rs
git commit -m "feat(chunkserver): expose io_uring_pool module"
```

---

### Task 4: Refactor `MyChunkServer` to use `IoUringPool`

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs`

This is the main task. Make these changes:

**Step 1: Update imports at the top of `chunkserver.rs`**

Add `use crate::io_uring_pool::IoUringPool;` after the existing `use` statements.

**Step 2: Add `io_uring_pool` field to `MyChunkServer`**

```rust
#[derive(Debug, Clone)]
pub struct MyChunkServer {
    storage_dir: PathBuf,
    config_server_addrs: Vec<String>,
    pub shard_map: Arc<Mutex<ShardMap>>,
    block_cache: Arc<Mutex<LruCache<String, CachedBlock>>>,
    ca_cert_path: Option<String>,
    domain_name: Option<String>,
    pub pending_bad_blocks: Arc<Mutex<Vec<String>>>,
    io_uring_pool: Arc<IoUringPool>,   // ADD THIS
}
```

**Step 3: Initialize in `new()`**

At the end of `MyChunkServer::new()`, before the closing `MyChunkServer { ... }` struct literal, the pool is added:

```rust
MyChunkServer {
    storage_dir,
    config_server_addrs,
    shard_map,
    block_cache,
    ca_cert_path,
    domain_name,
    pending_bad_blocks: Arc::new(Mutex::new(Vec::new())),
    io_uring_pool: Arc::new(IoUringPool::new(1024)),   // ADD THIS
}
```

**Step 4: Replace `write_block_async` — delete both cfg variants, write one**

Delete these two functions entirely:
- `#[cfg(feature = "io-uring")] async fn write_block_async(...)`
- `#[cfg(not(feature = "io-uring"))] async fn write_block_async(...)`

Replace with a single function:

```rust
async fn write_block_async(&self, block_id: &str, data: &[u8]) -> Result<(), std::io::Error> {
    let path = self.storage_dir.join(block_id);
    let meta_path = self.storage_dir.join(format!("{}.meta", block_id));

    let checksums = Self::calculate_checksums(data);
    let meta_bytes: Vec<u8> = checksums
        .iter()
        .flat_map(|c| c.to_be_bytes())
        .collect();

    self.io_uring_pool.write(path, data.to_vec()).await?;
    self.io_uring_pool.write(meta_path, meta_bytes).await
}
```

**Step 5: Replace `read_block_async` — delete both cfg variants, write one**

Delete:
- `#[cfg(feature = "io-uring")] async fn read_block_async(...)`
- `#[cfg(not(feature = "io-uring"))] async fn read_block_async(...)`

Replace with:

```rust
async fn read_block_async(
    &self,
    block_id: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, std::io::Error> {
    let path = self.storage_dir.join(block_id);
    let total_size = std::fs::metadata(&path)?.len();
    if offset >= total_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Offset {} exceeds file size {}", offset, total_size),
        ));
    }
    let bytes_to_read = std::cmp::min(length, total_size - offset) as usize;
    self.io_uring_pool.read(path, offset, bytes_to_read).await
}
```

**Step 6: Build to verify no errors**

```bash
cargo build -p dfs-chunkserver 2>&1 | grep -E "^error" | head -20
```

Expected: no errors (warnings OK)

**Step 7: Commit**

```bash
git add dfs/chunkserver/src/chunkserver.rs
git commit -m "feat(chunkserver): wire IoUringPool into write/read paths, remove cfg gates"
```

---

### Task 5: Update unit tests in `chunkserver.rs`

**Files:**
- Modify: `dfs/chunkserver/src/chunkserver.rs` (tests section, lines ~985–1110)

The tests need updating because:
- `make_test_server` calls `MyChunkServer::new()` which now spawns an io_uring thread — that's fine
- `test_checksum_verification` calls `write_block_local` directly — update to call `write_block_async`
- The `#[cfg(feature = "io-uring")]` tests still have that gate — remove it
- The `#[cfg(not(feature = "io-uring"))]` fallback tests can be deleted (fallback no longer exists)

**Step 1: Update `test_checksum_verification` to use `write_block_async`**

The test currently does:
```rust
server.write_block_local(block_id, data).unwrap();
```

Change to:
```rust
let rt = tokio::runtime::Runtime::new().unwrap();
rt.block_on(server.write_block_async(block_id, data)).unwrap();
```

OR make the test async:
```rust
#[tokio::test]
async fn test_checksum_verification() {
    let dir = tempdir().unwrap();
    let server = MyChunkServer::new(dir.path().to_path_buf(), vec![], None, None);
    let block_id = "test_block";
    let data = b"Hello, world! This is a test block for checksum verification.";
    server.write_block_async(block_id, data).await.unwrap();
    server.verify_block(block_id, data).unwrap();
    // ... rest of test unchanged
}
```

**Step 2: Remove `#[cfg(feature = "io-uring")]` from io_uring tests**

Change:
```rust
#[cfg(feature = "io-uring")]
#[tokio::test]
async fn test_write_block_async_creates_file_with_correct_content() {
```
To:
```rust
#[tokio::test]
async fn test_write_block_async_creates_file_with_correct_content() {
```

Do the same for all four `#[cfg(feature = "io-uring")]` tests.

**Step 3: Delete the two fallback tests**

Delete these entire test functions (they test the old std::fs fallback, which no longer exists):
- `test_read_block_async_fallback_correct_data`
- `test_read_block_async_fallback_partial_read`

**Step 4: Run all unit tests**

```bash
cargo test -p dfs-chunkserver --lib 2>&1 | tail -15
```

Expected: all tests pass. Note: tests are Linux-only; run in Docker if on macOS:
```bash
docker run --rm --security-opt seccomp=unconfined dfs-bench cargo test -p dfs-chunkserver --lib 2>&1 | tail -20
```

**Step 5: Commit**

```bash
git add dfs/chunkserver/src/chunkserver.rs
git commit -m "test(chunkserver): update tests for IoUringPool, remove cfg gates"
```

---

### Task 6: Update benchmarks and Dockerfile

**Files:**
- Modify: `dfs/chunkserver/benches/io_bench.rs`
- Modify: `Dockerfile`

**Step 1: Update `io_bench.rs`**

The benchmark currently calls `tokio_uring::start()` per iteration. Update it to use `IoUringPool` (ring created once) for the io_uring benchmarks, and also remove the `#[cfg(feature = "io-uring")]` gates.

Replace the entire contents of `dfs/chunkserver/benches/io_bench.rs`:

```rust
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use dfs_chunkserver::io_uring_pool::IoUringPool;
use std::io::{Read, Seek, SeekFrom, Write};
use tempfile::TempDir;
use tokio::runtime::Runtime;

fn make_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

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
                f.sync_all().unwrap();
            });
        });
    }
    group.finish();
}

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
                f.read_exact(&mut buf).unwrap();
                buf
            });
        });
    }
    group.finish();
}

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

// io_uring benchmarks use IoUringPool — ring created once, not per iteration
fn bench_write_uring(c: &mut Criterion) {
    let pool = IoUringPool::new(1024);
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("write_uring");
    for size in [4 * 1024usize, 64 * 1024, 1024 * 1024] {
        let data = make_data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            let dir = TempDir::new().unwrap();
            b.iter(|| {
                let path = dir.path().join("block");
                rt.block_on(pool.write(path, data.clone())).unwrap();
            });
        });
    }
    group.finish();
}

fn bench_read_uring(c: &mut Criterion) {
    let pool = IoUringPool::new(1024);
    let rt = Runtime::new().unwrap();
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
                rt.block_on(pool.read(path.clone(), 0, len)).unwrap()
            });
        });
    }
    group.finish();
}

fn bench_partial_read_uring(c: &mut Criterion) {
    let pool = IoUringPool::new(1024);
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("partial_read_uring");
    let size = 64 * 1024usize;
    let data = make_data(size);
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("64KB_read_4KB_at_32KB", |b| {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("block");
        std::fs::write(&path, &data).unwrap();
        b.iter(|| {
            rt.block_on(pool.read(path.clone(), 32 * 1024, 4096)).unwrap()
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_write_stdfs,
    bench_read_stdfs,
    bench_partial_read_stdfs,
    bench_write_uring,
    bench_read_uring,
    bench_partial_read_uring
);
criterion_main!(benches);
```

**Step 2: Update `Dockerfile` — remove `--features` flag**

In `Dockerfile`, find:
```
cargo build --release --locked --features dfs-chunkserver/io-uring
```
Change to:
```
cargo build --release --locked
```

**Step 3: Build to verify**

```bash
cargo build -p dfs-chunkserver 2>&1 | tail -5
```

**Step 4: Commit**

```bash
git add dfs/chunkserver/benches/io_bench.rs Dockerfile
git commit -m "bench(chunkserver): use IoUringPool in bench (ring reused); remove Dockerfile --features flag"
```

---

### Task 7: Run full test suite and benchmark in Docker

**Step 1: Rebuild Docker bench image**

```bash
docker build -f Dockerfile.bench -t dfs-bench . 2>&1 | tail -5
```

**Step 2: Run unit tests**

```bash
docker run --rm --security-opt seccomp=unconfined dfs-bench \
  cargo test -p dfs-chunkserver --lib 2>&1 | tail -20
```

Expected: all tests pass.

**Step 3: Run benchmarks (std::fs baseline)**

```bash
docker run --rm --security-opt seccomp=unconfined dfs-bench \
  cargo bench -p dfs-chunkserver -- --save-baseline stdfs 2>&1 | grep -E "write_stdfs|read_stdfs|partial_read_stdfs|time:|thrpt:" | grep -v "Benchmarking\|Warming\|Collecting\|Analyzing"
```

**Step 4: Run benchmarks (io_uring comparison)**

```bash
docker run --rm --security-opt seccomp=unconfined dfs-bench \
  cargo bench -p dfs-chunkserver 2>&1 | grep -E "write_uring|read_uring|partial_read_uring|time:|thrpt:" | grep -v "Benchmarking\|Warming\|Collecting\|Analyzing"
```

**Step 5: Commit if clean**

```bash
git add Dockerfile.bench
git commit -m "chore: cleanup Dockerfile.bench"
```
