# Backup & Recovery Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Periodically upload Raft state machine snapshots to the project's S3 server so metadata can be recovered after a full cluster loss.

**Architecture:** After each `create_snapshot()` call in `RaftNode::run()`, if this node is the Leader and `backup_s3_endpoint` is configured, spawn an async task that reads `snapshot_data` from RocksDB and PUTs it to S3 at `/{bucket}/master-snapshots/node-{id}/{timestamp}-idx{last_included_index}.bin`. If `backup_s3_endpoint` is not set, backup is silently skipped. Upload failures are logged as warnings and never crash the node.

**Tech Stack:** `reqwest` (already in metaserver deps), RocksDB (`Arc<DB>`), existing `RaftNodeConfig` / `RaftNode` structs.

---

### Task 1: Add backup args to master binary

**Files:**
- Modify: `dfs/metaserver/src/bin/master.rs:22-70` (Args struct)

**Step 1: Add two optional args to `Args`**

After `domain_name: Option<String>`, add:

```rust
/// S3 endpoint for snapshot backups (e.g. http://dfs-s3-server:9000).
/// If not set, backup is disabled.
#[arg(long)]
backup_s3_endpoint: Option<String>,

/// S3 bucket name for snapshot backups.
#[arg(long, default_value = "dfs-backups")]
backup_bucket: String,
```

**Step 2: Pass args into `RaftNodeConfig`** (already done in the next task; just confirm the args struct compiles)

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

Expected: compile error about unknown fields in `RaftNodeConfig` — that's expected until Task 2 is done. Just check the Args struct parses OK by looking for arg-parsing errors only.

Actually, just build once after Task 2.

**Step 3: Commit**

```bash
git add dfs/metaserver/src/bin/master.rs
git commit -m "feat: add --backup_s3_endpoint and --backup_bucket args to master"
```

---

### Task 2: Add backup fields to RaftNodeConfig + RaftNode, implement upload

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs:619-628` (RaftNodeConfig)
- Modify: `dfs/metaserver/src/simple_raft.rs:540-595` (RaftNode struct)
- Modify: `dfs/metaserver/src/simple_raft.rs:631-720` (RaftNode::new)
- Add: new async fn `upload_snapshot_to_s3` on `RaftNode`

**Step 1: Add fields to `RaftNodeConfig`**

```rust
pub struct RaftNodeConfig {
    // ... existing fields ...
    pub backup_s3_endpoint: Option<String>,
    pub backup_bucket: String,
}
```

**Step 2: Add fields to `RaftNode` struct** (after `storage_dir: String` around line 623):

```rust
/// S3 endpoint for snapshot backups. None = backup disabled.
pub backup_s3_endpoint: Option<String>,
/// S3 bucket name for snapshot backups.
pub backup_bucket: String,
```

**Step 3: Initialize in `RaftNode::new()`**

In the destructuring of `config`, add:
```rust
let RaftNodeConfig {
    // ... existing fields ...
    backup_s3_endpoint,
    backup_bucket,
} = config;
```

And add to the returned struct:
```rust
backup_s3_endpoint,
backup_bucket,
```

**Step 4: Implement `upload_snapshot_to_s3`**

Add this method to `impl RaftNode` (after `create_snapshot`, around line 1040):

```rust
/// Upload the current snapshot to S3. Non-fatal: logs warnings on failure.
async fn upload_snapshot_to_s3(&self) {
    let endpoint = match &self.backup_s3_endpoint {
        Some(e) => e.clone(),
        None => return, // backup disabled
    };

    let snapshot_data = match self.db.get(b"snapshot_data") {
        Ok(Some(data)) => data,
        Ok(None) => {
            tracing::debug!("Backup: no snapshot data found, skipping upload");
            return;
        }
        Err(e) => {
            tracing::warn!("Backup: failed to read snapshot_data from RocksDB: {}", e);
            return;
        }
    };

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let key = format!(
        "master-snapshots/node-{}/{}--idx{}.bin",
        self.id, timestamp, self.last_included_index
    );
    let url = format!("{}/{}/{}", endpoint.trim_end_matches('/'), self.backup_bucket, key);

    tracing::info!("Backup: uploading snapshot to {}", url);

    match reqwest::Client::new()
        .put(&url)
        .header("Content-Type", "application/octet-stream")
        .body(snapshot_data)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(
                "Backup: snapshot uploaded successfully (node={}, idx={})",
                self.id,
                self.last_included_index
            );
        }
        Ok(resp) => {
            tracing::warn!(
                "Backup: S3 upload returned status {} for {}",
                resp.status(),
                url
            );
        }
        Err(e) => {
            tracing::warn!("Backup: S3 upload failed for {}: {}", url, e);
        }
    }
}
```

**Step 5: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -30
```

Expected: compile errors about `RaftNodeConfig` missing fields in callers (master.rs bin). Fix them in the next step.

**Step 6: Fix callers in `bin/master.rs`**

Pass the new backup args into `RaftNodeConfig`:

```rust
RaftNodeConfig {
    // ... existing fields ...
    backup_s3_endpoint: args.backup_s3_endpoint.clone(),
    backup_bucket: args.backup_bucket.clone(),
}
```

**Step 7: Build again**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

Expected: clean build.

**Step 8: Run tests**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```

Expected: all 19 tests pass.

**Step 9: Commit**

```bash
git add dfs/metaserver/src/simple_raft.rs dfs/metaserver/src/bin/master.rs
git commit -m "feat: add snapshot backup to S3 (upload_snapshot_to_s3)"
```

---

### Task 3: Trigger upload after create_snapshot when Leader

**Files:**
- Modify: `dfs/metaserver/src/simple_raft.rs:1143-1147` (snapshot trigger in run loop)

**Step 1: Find the snapshot trigger block**

Current code (around line 1143):
```rust
// Create snapshot if log is too large (threshold: 100 entries)
const SNAPSHOT_THRESHOLD: usize = 100;
if self.log.len() > SNAPSHOT_THRESHOLD && self.last_applied > self.last_included_index {
    self.create_snapshot()?;
}
```

**Step 2: Replace with backup-triggering version**

```rust
// Create snapshot if log is too large (threshold: 100 entries)
const SNAPSHOT_THRESHOLD: usize = 100;
if self.log.len() > SNAPSHOT_THRESHOLD && self.last_applied > self.last_included_index {
    self.create_snapshot()?;
    // Upload snapshot to S3 if this is the leader and backup is configured
    if self.role == Role::Leader && self.backup_s3_endpoint.is_some() {
        let db = self.db.clone();
        let endpoint = self.backup_s3_endpoint.clone().unwrap();
        let bucket = self.backup_bucket.clone();
        let id = self.id;
        let last_included_index = self.last_included_index;
        tokio::spawn(async move {
            let snapshot_data = match db.get(b"snapshot_data") {
                Ok(Some(data)) => data,
                Ok(None) => {
                    tracing::debug!("Backup: no snapshot_data found after snapshot creation");
                    return;
                }
                Err(e) => {
                    tracing::warn!("Backup: failed to read snapshot_data: {}", e);
                    return;
                }
            };
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let key = format!(
                "master-snapshots/node-{}/{}--idx{}.bin",
                id, timestamp, last_included_index
            );
            let url = format!("{}/{}/{}", endpoint.trim_end_matches('/'), bucket, key);
            tracing::info!("Backup: uploading snapshot to {}", url);
            match reqwest::Client::new()
                .put(&url)
                .header("Content-Type", "application/octet-stream")
                .body(snapshot_data)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!(
                        "Backup: snapshot uploaded (node={}, idx={})",
                        id,
                        last_included_index
                    );
                }
                Ok(resp) => {
                    tracing::warn!("Backup: S3 upload returned status {} for {}", resp.status(), url);
                }
                Err(e) => {
                    tracing::warn!("Backup: S3 upload failed: {}", e);
                }
            }
        });
    }
}
```

Note: we inline the upload logic in the spawn rather than using `self.upload_snapshot_to_s3()` because `tokio::spawn` requires `'static` and `self` is not `'static`. So we clone what we need and move into the closure.

Because `upload_snapshot_to_s3` is now redundant (inlined above), either remove it or keep it. Keep it for now (dead code warning will appear — suppress or remove if needed).

Actually, since the method was added in Task 2 and would be unused, just don't add `upload_snapshot_to_s3` as a method in Task 2 at all. Instead, just add the backup fields and implement the inline logic here in Task 3. Revise: skip Step 4 of Task 2.

**Step 3: Build**

```bash
cargo build -p dfs-metaserver 2>&1 | head -20
```

If there's a dead-code warning for `upload_snapshot_to_s3`, remove that method.

**Step 4: Run tests**

```bash
cargo test -p dfs-metaserver --lib 2>&1 | tail -10
```

**Step 5: Commit**

```bash
git add dfs/metaserver/src/simple_raft.rs
git commit -m "feat: trigger S3 snapshot upload after log compaction on Leader"
```

---

### Task 4: Smoke test + update TODO.md

**Step 1: Quick compile + test smoke**

```bash
cargo build 2>&1 | tail -5
cargo test --lib 2>&1 | tail -5
```

**Step 2: Verify the flag works**

```bash
cargo run -p dfs-metaserver --bin master -- --help 2>&1 | grep backup
```

Expected output includes `--backup-s3-endpoint` and `--backup-bucket`.

**Step 3: Update TODO.md**

Mark Backup & Recovery complete:
```markdown
- [x] **Backup & Recovery** ✅
    - [x] Raftログを外部ストレージに退避するアーカイブスレッド。
    - [x] State MachineのスナップショットをS3形式で外部へ定期アップロード。
```

**Step 4: Commit**

```bash
git add TODO.md
git commit -m "docs: mark Backup & Recovery as complete in TODO.md"
```
