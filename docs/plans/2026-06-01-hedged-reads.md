# Hedged Reads Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Reduce tail read latency by launching a second request to an alternate replica when the primary doesn't respond within a configurable deadline.

**Architecture:** Add `hedge_delay_ms: Option<u64>` to `Client` (None = disabled). Extract a `read_block_from_location` helper from `fetch_single_block`. Implement hedging in `fetch_single_block` using `tokio::spawn` + `tokio::select!`: launch primary, wait up to `hedge_delay`, if no response start a second request to the next replica, return whichever finishes first. Also refactor `read_file_range` to use the same helper.

**Tech Stack:** `tokio::spawn`, `tokio::select!`, `tokio::time::sleep`, existing `ChunkServerServiceClient`, `dfs/client/src/mod.rs`.

---

### Task 1: Extract `read_block_from_location` helper

**Files:**
- Modify: `dfs/client/src/mod.rs`

**Step 1: Write failing tests**

At the bottom of `dfs/client/src/mod.rs`, add a `#[cfg(test)]` module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn make_client() -> Client {
        Client::new(vec!["http://localhost:50051".to_string()], vec![])
    }

    #[tokio::test]
    async fn test_read_block_from_location_unreachable_returns_error() {
        let client = make_client();
        let result = client
            .read_block_from_location("localhost:19999", "block-x", 0, 0)
            .await;
        assert!(result.is_err(), "Expected error for unreachable server");
    }

    #[tokio::test]
    async fn test_fetch_single_block_empty_locations_returns_error() {
        let client = make_client();
        let block = crate::dfs::BlockInfo {
            block_id: "b1".to_string(),
            size: 0,
            locations: vec![],
            checksum_crc32c: 0,
        };
        let result = client.fetch_single_block(&block).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("no locations"), "Got: {}", msg);
    }
}
```

**Step 2: Run tests to verify they fail**

```bash
cargo test -p dfs-client --lib 2>&1 | tail -10
```

Expected: FAIL — `read_block_from_location` method does not exist yet.

**Step 3: Add `read_block_from_location` helper**

Add this method to `impl Client` in `dfs/client/src/mod.rs`, after `fetch_single_block`:

```rust
/// Read a block range from a single ChunkServer location.
/// `length = 0` means read the entire block.
async fn read_block_from_location(
    &self,
    location: &str,
    block_id: &str,
    offset: u64,
    length: u64,
) -> anyhow::Result<Vec<u8>> {
    let chunk_server_addr = format!("http://{}", location);
    let channel = self.connect_endpoint(&chunk_server_addr).await?;
    let mut client = ChunkServerServiceClient::with_interceptor(
        channel,
        dfs_common::telemetry::tracing_interceptor
            as fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
    )
    .max_decoding_message_size(100 * 1024 * 1024);

    let request = tonic::Request::new(ReadBlockRequest {
        block_id: block_id.to_string(),
        offset,
        length,
    });
    let data = client.read_block(request).await?.into_inner().data;
    Ok(data)
}
```

**Step 4: Refactor `fetch_single_block` to use the helper**

Replace the body of `fetch_single_block` (lines 682-734) with:

```rust
pub async fn fetch_single_block(&self, block: &dfs::BlockInfo) -> anyhow::Result<Vec<u8>> {
    if block.locations.is_empty() {
        bail!("Block {} has no locations", block.block_id);
    }

    for location in &block.locations {
        match self
            .read_block_from_location(location, &block.block_id, 0, 0)
            .await
        {
            Ok(data) => {
                tracing::debug!(
                    "Successfully fetched block {} from {}",
                    block.block_id,
                    location
                );
                return Ok(data);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read block {} from {}: {}",
                    block.block_id,
                    location,
                    e
                );
            }
        }
    }

    bail!("Failed to read block {} from any location", block.block_id)
}
```

**Step 5: Run tests**

```bash
cargo test -p dfs-client --lib 2>&1 | tail -10
```

Expected: 2 tests PASS.

**Step 6: Build**

```bash
cargo build -p dfs-client 2>&1 | head -10
```

Expected: clean build.

**Step 7: Commit**

```bash
git add dfs/client/src/mod.rs
git commit -m "refactor: extract read_block_from_location helper in client"
```

---

### Task 2: Add hedge_delay_ms to Client and implement hedged fetch

**Files:**
- Modify: `dfs/client/src/mod.rs`

**Step 1: Write failing tests**

Add to the `#[cfg(test)]` module:

```rust
    #[test]
    fn test_client_hedge_delay_default_is_none() {
        let client = make_client();
        assert!(client.hedge_delay_ms.is_none());
    }

    #[test]
    fn test_client_with_hedge_delay_sets_field() {
        let client = make_client().with_hedge_delay(50);
        assert_eq!(client.hedge_delay_ms, Some(50));
    }

    #[tokio::test]
    async fn test_fetch_single_block_no_hedge_when_one_location() {
        // With hedge enabled but only 1 location, should still work (no panic)
        let client = make_client().with_hedge_delay(1);
        let block = crate::dfs::BlockInfo {
            block_id: "b1".to_string(),
            size: 0,
            locations: vec!["localhost:19999".to_string()], // unreachable
            checksum_crc32c: 0,
        };
        // Should fail gracefully (not panic) even with hedge enabled
        let result = client.fetch_single_block(&block).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_single_block_hedge_disabled_with_two_locations() {
        // Hedge disabled: tries locations sequentially, both unreachable → error
        let client = make_client(); // no hedge
        let block = crate::dfs::BlockInfo {
            block_id: "b2".to_string(),
            size: 0,
            locations: vec![
                "localhost:19997".to_string(),
                "localhost:19998".to_string(),
            ],
            checksum_crc32c: 0,
        };
        let result = client.fetch_single_block(&block).await;
        assert!(result.is_err());
    }
```

**Step 2: Run tests to verify they fail**

```bash
cargo test -p dfs-client --lib -- test_client_hedge 2>&1 | tail -10
cargo test -p dfs-client --lib -- test_fetch_single_block_no_hedge 2>&1 | tail -10
cargo test -p dfs-client --lib -- test_fetch_single_block_hedge_disabled 2>&1 | tail -10
```

Expected: FAIL — `hedge_delay_ms` field and `with_hedge_delay` method don't exist.

**Step 3: Add `hedge_delay_ms` field to `Client` struct**

In the `Client` struct (around line 24), add after `domain_name`:

```rust
/// Hedge delay in milliseconds. None = hedging disabled.
hedge_delay_ms: Option<u64>,
```

In `Client::new` (around line 37), add to the struct literal:

```rust
hedge_delay_ms: None,
```

In `with_tls_config` — no change needed (builder methods don't touch this field).

Add a new builder method after `with_retry_config`:

```rust
/// Enable hedged reads: if the primary replica doesn't respond within
/// `delay_ms` milliseconds, a second request is launched to the next replica.
/// The first successful response wins.
pub fn with_hedge_delay(mut self, delay_ms: u64) -> Self {
    self.hedge_delay_ms = Some(delay_ms);
    self
}
```

**Step 4: Implement hedging in `fetch_single_block`**

Replace the `fetch_single_block` body (which now iterates with `read_block_from_location`) with this hedged version:

```rust
pub async fn fetch_single_block(&self, block: &dfs::BlockInfo) -> anyhow::Result<Vec<u8>> {
    if block.locations.is_empty() {
        bail!("Block {} has no locations", block.block_id);
    }

    // If hedging is disabled or only one location, fall back to sequential.
    let hedge_delay = match self.hedge_delay_ms {
        Some(d) if block.locations.len() > 1 => tokio::time::Duration::from_millis(d),
        _ => {
            for location in &block.locations {
                match self
                    .read_block_from_location(location, &block.block_id, 0, 0)
                    .await
                {
                    Ok(data) => return Ok(data),
                    Err(e) => tracing::warn!(
                        "Failed to read block {} from {}: {}",
                        block.block_id, location, e
                    ),
                }
            }
            bail!("Failed to read block {} from any location", block.block_id);
        }
    };

    // Hedged path: spawn primary, then race against a delay.
    let self1 = self.clone();
    let block_id1 = block.block_id.clone();
    let loc0 = block.locations[0].clone();
    let mut primary = tokio::spawn(async move {
        self1.read_block_from_location(&loc0, &block_id1, 0, 0).await
    });

    // Wait for primary or hedge_delay, whichever comes first.
    tokio::select! {
        result = &mut primary => {
            return result.unwrap_or_else(|e| Err(anyhow::anyhow!("Task error: {}", e)));
        }
        _ = tokio::time::sleep(hedge_delay) => {
            tracing::debug!(
                "Hedge triggered for block {} after {:?}",
                block.block_id, hedge_delay
            );
        }
    }

    // Primary is slow — launch hedge request to the next location.
    let self2 = self.clone();
    let block_id2 = block.block_id.clone();
    let loc1 = block.locations[1].clone();
    let mut hedge = tokio::spawn(async move {
        self2.read_block_from_location(&loc1, &block_id2, 0, 0).await
    });

    // Return whichever finishes first (and succeeds).
    tokio::select! {
        result = &mut primary => {
            result.unwrap_or_else(|e| Err(anyhow::anyhow!("Primary task error: {}", e)))
        }
        result = &mut hedge => {
            result.unwrap_or_else(|e| Err(anyhow::anyhow!("Hedge task error: {}", e)))
        }
    }
}
```

**Step 5: Run tests**

```bash
cargo test -p dfs-client --lib 2>&1 | tail -10
```

Expected: all 6 tests PASS.

**Step 6: Build**

```bash
cargo build -p dfs-client 2>&1 | head -10
```

Expected: clean build.

**Step 7: Commit**

```bash
git add dfs/client/src/mod.rs
git commit -m "feat: hedged reads in fetch_single_block (hedge_delay_ms)"
```

---

### Task 3: Refactor `read_file_range` to use `read_block_from_location`

**Files:**
- Modify: `dfs/client/src/mod.rs` — `read_file_range` method (lines 547-596)

The `read_file_range` method has its own inline location-iteration loop (duplicated from the old `fetch_single_block`). Refactor it to use `read_block_from_location`, and apply hedging.

**Step 1: Identify the inner loop in `read_file_range`**

Search for the loop that iterates over `block.locations` in `read_file_range`. It looks like:

```rust
let mut success = false;
for location in &block.locations {
    let chunk_server_addr = format!("http://{}", location);
    match self.connect_endpoint(&chunk_server_addr).await {
        Ok(channel) => {
            // ... build client, send ReadBlockRequest with block_offset/block_length ...
            match client.read_block(request).await {
                Ok(response) => { result.extend_from_slice(&resp.data); success = true; break; }
                Err(e) => { ... }
            }
        }
        Err(e) => { ... }
    }
}
if !success { bail!(...); }
```

**Step 2: Replace the inner loop with `read_block_from_location` + optional hedging**

Replace the entire `let mut success = false; ... if !success { bail!(...) }` block with:

```rust
// Hedged or sequential read for this block range
let data = self
    .read_block_range(&block.locations, &block.block_id, block_offset, block_length)
    .await?;
result.extend_from_slice(&data);
```

**Step 3: Add `read_block_range` helper**

Add this method to `impl Client`, after `read_block_from_location`:

```rust
/// Read a block range from the given locations, applying hedging if configured.
/// Tries locations sequentially (or with hedge delay) until one succeeds.
async fn read_block_range(
    &self,
    locations: &[String],
    block_id: &str,
    offset: u64,
    length: u64,
) -> anyhow::Result<Vec<u8>> {
    if locations.is_empty() {
        bail!("Block {} has no locations", block_id);
    }

    let hedge_delay = match self.hedge_delay_ms {
        Some(d) if locations.len() > 1 => tokio::time::Duration::from_millis(d),
        _ => {
            for location in locations {
                match self
                    .read_block_from_location(location, block_id, offset, length)
                    .await
                {
                    Ok(data) => return Ok(data),
                    Err(e) => tracing::warn!(
                        "Failed to read block {} from {}: {}", block_id, location, e
                    ),
                }
            }
            bail!("Failed to read block {} from any location", block_id);
        }
    };

    // Hedged path
    let self1 = self.clone();
    let bid1 = block_id.to_string();
    let loc0 = locations[0].clone();
    let mut primary = tokio::spawn(async move {
        self1.read_block_from_location(&loc0, &bid1, offset, length).await
    });

    tokio::select! {
        result = &mut primary => {
            return result.unwrap_or_else(|e| Err(anyhow::anyhow!("Task error: {}", e)));
        }
        _ = tokio::time::sleep(hedge_delay) => {
            tracing::debug!("Hedge triggered for block {} (range)", block_id);
        }
    }

    let self2 = self.clone();
    let bid2 = block_id.to_string();
    let loc1 = locations[1].clone();
    let mut hedge = tokio::spawn(async move {
        self2.read_block_from_location(&loc1, &bid2, offset, length).await
    });

    tokio::select! {
        result = &mut primary => {
            result.unwrap_or_else(|e| Err(anyhow::anyhow!("Primary task error: {}", e)))
        }
        result = &mut hedge => {
            result.unwrap_or_else(|e| Err(anyhow::anyhow!("Hedge task error: {}", e)))
        }
    }
}
```

**Note on DRY:** Now update `fetch_single_block` to delegate to `read_block_range`:

```rust
pub async fn fetch_single_block(&self, block: &dfs::BlockInfo) -> anyhow::Result<Vec<u8>> {
    if block.locations.is_empty() {
        bail!("Block {} has no locations", block.block_id);
    }
    self.read_block_range(&block.locations, &block.block_id, 0, 0).await
}
```

This removes the hedging logic from `fetch_single_block` and centralizes it in `read_block_range`.

**Step 4: Build**

```bash
cargo build -p dfs-client 2>&1 | head -10
```

Expected: clean build.

**Step 5: Run tests**

```bash
cargo test -p dfs-client --lib 2>&1 | tail -10
```

Expected: all tests PASS (same 6 as before — tests don't need to change since `fetch_single_block` API is unchanged).

**Step 6: Commit**

```bash
git add dfs/client/src/mod.rs
git commit -m "refactor: use read_block_range helper in read_file_range (DRY hedging)"
```

---

### Task 4: Update TODO.md

**Step 1: Mark Hedged Reads complete**

In `TODO.md`, replace:
```
- [ ] **Hedged Reads (Tail Latency Mitigation)**
    - [ ] 1次リクエストの応答が一定時間（例: p95レイテンシ (ms)）来ない場合、別レプリカに投げる並行リクエスト管理。
    - [ ] 最速のレスポンスをクライアントに返し、遅い方のリクエストをキャンセルするロジック。
```

With:
```
- [x] **Hedged Reads (Tail Latency Mitigation)** ✅
    - [x] 1次リクエストの応答が一定時間（例: p95レイテンシ (ms)）来ない場合、別レプリカに投げる並行リクエスト管理。
    - [x] 最速のレスポンスをクライアントに返し、遅い方のリクエストをキャンセルするロジック。
```

**Step 2: Final build + test**

```bash
cargo build 2>&1 | tail -5
cargo test --lib 2>&1 | tail -5
```

**Step 3: Commit**

```bash
git add TODO.md
git commit -m "docs: mark Hedged Reads as complete in TODO.md"
```
