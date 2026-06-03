# io_uring / Zero-Copy Data Path Design

**Goal:** Replace blocking `std::fs` I/O in ChunkServer with `tokio-uring` (io_uring) for async, low-syscall file operations on the write and read paths.

**Architecture:** Use `tokio_uring::start()` inside `tokio::task::spawn_blocking` to bridge the existing tokio gRPC runtime with io_uring's separate runtime model. LRU read cache is preserved as a fast path above io_uring. No cfg platform branches — Linux/Docker only.

**Tech Stack:** `tokio-uring 0.5`, `bytes 1`, `criterion 0.5` (benchmarks), Debian bookworm (kernel 6.1, io_uring 5.6+ required)

---

## Current I/O (baseline)

- `write_block_local`: synchronous `std::fs::File::create` + `write_all` called directly from async context (blocks tokio thread)
- `read_block`: synchronous `std::fs::File::open` + `seek(SeekFrom::Start(offset))` + `read_exact` (blocks tokio thread)
- `.meta` files (CRC32C): same synchronous pattern

## Target I/O

```
WriteBlock RPC
  └─ CRC32C verification (unchanged)
  └─ spawn_blocking → tokio_uring::start
       tokio_uring::fs::File::create → write_all_at(data, 0)
       tokio_uring::fs::File::create → write_all_at(checksum, 0)  [.meta]
       file.sync_all()

ReadBlock RPC
  └─ LRU cache check → hit: return immediately (unchanged)
  └─ cache miss → spawn_blocking → tokio_uring::start
       tokio_uring::fs::File::open → read_at(buf, offset)  [no seek syscall]
  └─ LRU cache store (unchanged)
```

## Runtime Integration

`tokio-uring` requires its own `tokio_uring::start()` runtime and cannot be called from a standard tokio thread. Bridge pattern:

```rust
tokio::task::spawn_blocking(move || {
    tokio_uring::start(async {
        // io_uring operations here
    })
}).await?
```

This keeps the tonic gRPC runtime (`#[tokio::main]`) unchanged and uses `spawn_blocking`'s thread pool to host io_uring runtimes.

## Benchmarks

Benchmarks run with `criterion` in `dfs/chunkserver/benches/io_bench.rs`.

**Measure before and after implementation:**

| Benchmark | Block sizes | Metric |
|-----------|-------------|--------|
| `write_block` | 4KB, 64KB, 1MB | throughput (MB/s), p50/p99 latency |
| `read_block_full` (cache miss) | 4KB, 64KB, 1MB | throughput (MB/s), p50/p99 latency |
| `read_block_partial` (offset) | 64KB block, read 4KB at offset 32KB | latency |

Baseline is recorded first (on `std::fs`), then re-run after io_uring implementation to measure improvement.

## Error Handling

- Write failure → `WriteBlockResponse { success: false, error_message }` (unchanged)
- `spawn_blocking` JoinError → `Status::internal`
- `tokio_uring` errors propagate as `std::io::Error`

## Testing

- Unit tests inside `tokio_uring::start` for write/read correctness and CRC32C metadata
- Existing integration tests (`fault_recovery_test.sh`, `chaos_test.sh`, `erasure_coding_test.sh`) validate end-to-end correctness
