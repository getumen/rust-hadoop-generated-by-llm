# io_uring Persistent Pool Design

**Goal:** Replace per-RPC `spawn_blocking + tokio_uring::start()` (60µs overhead per call) with a single long-lived io_uring thread whose ring is created once at startup.

**Architecture:** A dedicated thread runs `tokio_uring::start()` forever. RPCs send typed `IoRequest` values over an `mpsc` channel and receive results via `oneshot` channels. Multiple requests are dispatched concurrently on the same ring via `tokio_uring::spawn`. `tokio-uring` becomes a required dependency (Linux/Docker only — macOS not supported).

**Tech Stack:** `tokio-uring 0.5`, `tokio::sync::{mpsc, oneshot}`

---

## Problem with Current Design

```
RPC arrives → spawn_blocking → tokio_uring::start() → I/O → return
                               ^^^^^^^^^^^^^^^^^^^^
                               ~60µs ring setup per call
```

Benchmark on Docker Linux: `read_uring/4KB = 60µs` vs `read_stdfs/4KB = 1.8µs` (34× slower due to ring setup amortized nowhere).

## Target Design

```
Startup: IoUringPool::new()
  └─ std::thread::spawn → tokio_uring::start() [runs forever]
        receives IoRequest via mpsc channel
        dispatches each as tokio_uring::spawn (concurrent on same ring)

RPC arrives → pool.write(path, data).await  ← oneshot result
           → pool.read(path, offset, len).await
```

Ring is created once. All RPC I/O shares it. `io_uring_enter` syscall batches concurrent ops.

---

## Data Types

```rust
// dfs/chunkserver/src/io_uring_pool.rs

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
    sender: tokio::sync::mpsc::Sender<IoRequest>,
}
```

## IoUringPool Implementation

```rust
impl IoUringPool {
    pub fn new(channel_capacity: usize) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<IoRequest>(channel_capacity);
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
        self.sender.send(IoRequest::Write { path, data, reply: tx }).await
            .map_err(|_| io::Error::other("io_uring pool unavailable"))?;
        rx.await.map_err(|_| io::Error::other("io_uring thread panicked"))?
    }

    pub async fn read(&self, path: PathBuf, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.sender.send(IoRequest::Read { path, offset, length, reply: tx }).await
            .map_err(|_| io::Error::other("io_uring pool unavailable"))?;
        rx.await.map_err(|_| io::Error::other("io_uring thread panicked"))?
    }
}

async fn do_write(path: PathBuf, data: Vec<u8>) -> io::Result<()> {
    let file = tokio_uring::fs::File::create(&path).await?;
    let (res, _) = file.write_all_at(data, 0).await;
    res?;
    file.sync_all().await
}

async fn do_read(path: PathBuf, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let file = tokio_uring::fs::File::open(&path).await?;
    let mut buf = vec![0u8; length];
    let mut filled = 0usize;
    while filled < length {
        let sub = buf[filled..].to_vec();
        let (res, ret) = file.read_at(sub, offset + filled as u64).await;
        let n = res?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read_at returned 0"));
        }
        buf[filled..filled + n].copy_from_slice(&ret[..n]);
        filled += n;
    }
    Ok(buf)
}
```

## ChunkServer Integration

```rust
pub struct ChunkServer {
    // ...existing fields...
    io_uring_pool: Arc<IoUringPool>,  // no cfg gate — always present
}

impl ChunkServer {
    pub fn new(...) -> Self {
        ChunkServer {
            // ...
            io_uring_pool: Arc::new(IoUringPool::new(1024)),
        }
    }

    async fn write_block_async(&self, block_id: &str, data: &[u8]) -> io::Result<()> {
        let checksums = Self::calculate_checksums(data);
        let meta_bytes: Vec<u8> = checksums.iter()
            .flat_map(|c| c.to_be_bytes())
            .collect();
        let pool = &self.io_uring_pool;
        pool.write(self.storage_dir.join(block_id), data.to_vec()).await?;
        pool.write(self.storage_dir.join(format!("{}.meta", block_id)), meta_bytes).await
    }

    async fn read_block_async(&self, block_id: &str, offset: u64, length: u64) -> io::Result<Vec<u8>> {
        let path = self.storage_dir.join(block_id);
        let total_size = std::fs::metadata(&path)?.len();
        if offset >= total_size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                format!("offset {} >= file size {}", offset, total_size)));
        }
        let bytes = std::cmp::min(length, total_size - offset) as usize;
        self.io_uring_pool.read(path, offset, bytes).await
    }
}
```

## Error Handling

| Error | Behavior |
|-------|----------|
| Channel full (backpressure) | `mpsc::send` blocks until space available (bounded channel) |
| Pool thread panicked | `oneshot` recv returns `Err` → `io::Error::other(...)` → `WriteBlockResponse { success: false }` |
| io_uring I/O error | Propagated as `io::Error` through `oneshot` |

## Cargo.toml Changes

```toml
# tokio-uring becomes required (was optional)
[dependencies]
tokio-uring = "0.5"

# Remove [features] section entirely
# Remove io-uring feature flag
```

Dockerfile `--features` flag also removed (no longer needed).

## Testing

Unit tests in `io_uring_pool.rs`:

```rust
#[test]
fn test_pool_write_read_roundtrip() {
    tokio_uring::start(async {
        // Tests run directly on io_uring — Linux only
    });
}
```

`ChunkServer::new()` in existing unit tests updated to provide `IoUringPool::new(16)`.

Shell integration tests (`fault_recovery_test.sh`, `chaos_test.sh`, etc.) validate end-to-end correctness unchanged.
