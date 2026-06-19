# Linearizability Testing Framework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a linearizability testing framework that verifies the DFS maintains consistency under real network faults and node crashes.

**Architecture:** Three components: (1) workload generator as `dfs_cli workload` subcommand runs concurrent ops and records JSONL history, (2) linearizability checker as `dfs_cli check-history` subcommand verifies history against Register+Rename model, (3) shell test orchestrator runs scenarios with Docker+Toxiproxy fault injection.

**Tech Stack:** Rust (tokio, serde_json, clap), shell (Docker compose, Toxiproxy)

**Spec:** `docs/superpowers/specs/2026-06-19-linearizability-testing-design.md`

**Baseline commands:**
```bash
cargo build --release 2>&1 | tail -3
cargo clippy -- -D warnings 2>&1 | tail -3
cargo test --lib -p dfs-client 2>&1 | tail -5
```

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `dfs/client/src/checker.rs` | Create | JSONL parser, operation model, WGL linearizability checker with rename extension, self-test suite |
| `dfs/client/src/workload.rs` | Create | Concurrent workload generator, JSONL history recorder |
| `dfs/client/src/mod.rs` | Modify | Add `pub mod checker; pub mod workload;` |
| `dfs/client/src/bin/dfs_cli.rs` | Modify | Add `Workload` and `CheckHistory` subcommands |
| `dfs/client/Cargo.toml` | Modify | Add `chrono` dependency for timestamp formatting |
| `test_scripts/linearizability_test.sh` | Create | Test orchestrator with 7 fault scenarios |

---

### Task 1: Checker — JSONL parser and operation model

**Files:**
- Create: `dfs/client/src/checker.rs`
- Modify: `dfs/client/src/mod.rs`

- [ ] **Step 1: Create checker.rs with data types and JSONL parser**

Create `dfs/client/src/checker.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;

/// A single line in the JSONL history file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub client: u64,
    #[serde(rename = "type")]
    pub entry_type: String, // "invoke" or "return"
    pub op: String,         // "put", "get", "delete", "rename"
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub dst: Option<String>,
    #[serde(default)]
    pub data_hash: Option<String>,
    #[serde(default)]
    pub result: Option<String>, // "ok", "not_found", "error"
    pub ts_ns: u64,
}

/// A paired operation with invoke and return timestamps.
#[derive(Debug, Clone)]
pub struct Operation {
    pub id: u64,
    pub client: u64,
    pub op_type: OpType,
    pub invoke_ts: u64,
    pub return_ts: Option<u64>, // None = crashed (no return)
    pub result: OpResult,
}

#[derive(Debug, Clone)]
pub enum OpType {
    Put { path: String, data_hash: String },
    Get { path: String },
    Delete { path: String },
    Rename { src: String, dst: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpResult {
    Ok,
    NotFound,
    Error,
    PutOk { data_hash: String },
    GetOk { data_hash: String },
    Unknown, // No return yet (crashed)
}

/// Parse JSONL history file into paired operations.
pub fn parse_history(reader: impl BufRead) -> Result<Vec<Operation>, String> {
    let mut entries: Vec<HistoryEntry> = Vec::new();
    for (line_num, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("Line {}: {}", line_num + 1, e))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: HistoryEntry =
            serde_json::from_str(line).map_err(|e| format!("Line {}: {}", line_num + 1, e))?;
        entries.push(entry);
    }

    // Group by operation ID: pair invoke with return
    let mut invokes: HashMap<u64, HistoryEntry> = HashMap::new();
    let mut returns: HashMap<u64, HistoryEntry> = HashMap::new();

    for entry in entries {
        match entry.entry_type.as_str() {
            "invoke" => { invokes.insert(entry.id, entry); }
            "return" => { returns.insert(entry.id, entry); }
            other => return Err(format!("Unknown entry type: {}", other)),
        }
    }

    let mut ops = Vec::new();
    for (id, inv) in &invokes {
        let op_type = match inv.op.as_str() {
            "put" => OpType::Put {
                path: inv.path.clone().unwrap_or_default(),
                data_hash: inv.data_hash.clone().unwrap_or_default(),
            },
            "get" => OpType::Get {
                path: inv.path.clone().unwrap_or_default(),
            },
            "delete" => OpType::Delete {
                path: inv.path.clone().unwrap_or_default(),
            },
            "rename" => OpType::Rename {
                src: inv.src.clone().unwrap_or_default(),
                dst: inv.dst.clone().unwrap_or_default(),
            },
            other => return Err(format!("Unknown op: {}", other)),
        };

        let (return_ts, result) = if let Some(ret) = returns.get(id) {
            let result = match (inv.op.as_str(), ret.result.as_deref()) {
                ("get", Some("ok")) => OpResult::GetOk {
                    data_hash: ret.data_hash.clone().unwrap_or_default(),
                },
                ("get", Some("not_found")) => OpResult::NotFound,
                ("put", Some("ok")) => OpResult::PutOk {
                    data_hash: inv.data_hash.clone().unwrap_or_default(),
                },
                (_, Some("ok")) => OpResult::Ok,
                (_, Some("not_found")) => OpResult::NotFound,
                (_, Some("error")) => OpResult::Error,
                _ => OpResult::Unknown,
            };
            (Some(ret.ts_ns), result)
        } else {
            (None, OpResult::Unknown)
        };

        ops.push(Operation {
            id: *id,
            client: inv.client,
            op_type,
            invoke_ts: inv.ts_ns,
            return_ts,
            result,
        });
    }

    ops.sort_by_key(|op| op.invoke_ts);
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_put_get_history() {
        let jsonl = r#"{"id":1,"client":0,"type":"invoke","op":"put","path":"/a/f1","data_hash":"abc","ts_ns":100}
{"id":1,"client":0,"type":"return","op":"put","path":"/a/f1","result":"ok","ts_ns":200}
{"id":2,"client":1,"type":"invoke","op":"get","path":"/a/f1","ts_ns":150}
{"id":2,"client":1,"type":"return","op":"get","path":"/a/f1","result":"ok","data_hash":"abc","ts_ns":250}
"#;
        let ops = parse_history(Cursor::new(jsonl)).unwrap();
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn test_parse_rename_history() {
        let jsonl = r#"{"id":1,"client":0,"type":"invoke","op":"rename","src":"/a/f1","dst":"/z/f1","ts_ns":100}
{"id":1,"client":0,"type":"return","op":"rename","src":"/a/f1","dst":"/z/f1","result":"ok","ts_ns":200}
"#;
        let ops = parse_history(Cursor::new(jsonl)).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0].op_type, OpType::Rename { .. }));
    }

    #[test]
    fn test_parse_crashed_operation() {
        let jsonl = r#"{"id":1,"client":0,"type":"invoke","op":"put","path":"/a/f1","data_hash":"abc","ts_ns":100}
"#;
        let ops = parse_history(Cursor::new(jsonl)).unwrap();
        assert_eq!(ops.len(), 1);
        assert!(ops[0].return_ts.is_none());
        assert_eq!(ops[0].result, OpResult::Unknown);
    }
}
```

- [ ] **Step 2: Add module declaration**

In `dfs/client/src/mod.rs`, add at the top (after the `dfs` module):

```rust
pub mod checker;
```

- [ ] **Step 3: Build and test**

```bash
cargo build -p dfs-client 2>&1 | tail -3
cargo test --lib -p dfs-client 2>&1 | tail -10
```

Expected: 3 parser tests pass.

- [ ] **Step 4: Commit**

```bash
git add dfs/client/src/checker.rs dfs/client/src/mod.rs
git commit -m "feat(lincheck): add JSONL history parser and operation model"
```

---

### Task 2: Checker — WGL linearizability verification for simple registers

**Files:**
- Modify: `dfs/client/src/checker.rs`

- [ ] **Step 1: Write failing tests for simple register linearizability**

Add to `checker.rs` tests module:

```rust
#[test]
fn test_linearizable_simple_put_get() {
    // put(v1) completes, then get returns v1 — linearizable
    let ops = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 1, op_type: OpType::Get { path: "/a".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::GetOk { data_hash: "v1".into() } },
    ];
    assert!(check_linearizability(&ops).is_ok());
}

#[test]
fn test_non_linearizable_stale_read() {
    // put(v1) completes, put(v2) completes, then get returns v1 — NOT linearizable
    let ops = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v2".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::PutOk { data_hash: "v2".into() } },
        Operation { id: 3, client: 1, op_type: OpType::Get { path: "/a".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::GetOk { data_hash: "v1".into() } },
    ];
    assert!(check_linearizability(&ops).is_err());
}

#[test]
fn test_linearizable_concurrent_reads() {
    // put(v1), concurrent get could return v1 or not_found (depending on linearization)
    let ops = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(300), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 1, op_type: OpType::Get { path: "/a".into() }, invoke_ts: 150, return_ts: Some(250), result: OpResult::NotFound },
    ];
    // get overlaps put — not_found is valid (get linearized before put)
    assert!(check_linearizability(&ops).is_ok());
}
```

- [ ] **Step 2: Implement check_linearizability for simple registers**

Add to `checker.rs`:

```rust
/// Check linearizability of a history. Returns Ok(()) if linearizable,
/// Err(violations) if violations found.
pub fn check_linearizability(ops: &[Operation]) -> Result<(), Vec<String>> {
    let mut violations = Vec::new();

    // Partition into simple and rename-linked registers
    let mut rename_paths: HashSet<String> = HashSet::new();
    for op in ops {
        if let OpType::Rename { src, dst } = &op.op_type {
            rename_paths.insert(src.clone());
            rename_paths.insert(dst.clone());
        }
    }

    // Group ops by path for simple registers
    let mut per_key: BTreeMap<String, Vec<&Operation>> = BTreeMap::new();
    let mut rename_linked_ops: Vec<&Operation> = Vec::new();

    for op in ops {
        match &op.op_type {
            OpType::Put { path, .. } | OpType::Get { path } | OpType::Delete { path } => {
                if rename_paths.contains(path) {
                    rename_linked_ops.push(op);
                } else {
                    per_key.entry(path.clone()).or_default().push(op);
                }
            }
            OpType::Rename { .. } => {
                rename_linked_ops.push(op);
            }
        }
    }

    // Check simple registers (per-key WGL)
    for (key, key_ops) in &per_key {
        if let Err(v) = check_single_register(key, key_ops) {
            violations.extend(v);
        }
    }

    // Check rename-linked registers (Task 3)
    if !rename_linked_ops.is_empty() {
        if let Err(v) = check_rename_linked(&rename_linked_ops) {
            violations.extend(v);
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// WGL check for a single register (one file path).
fn check_single_register(key: &str, ops: &[&Operation]) -> Result<(), Vec<String>> {
    let mut violations = Vec::new();

    // Collect writes: put sets value, delete sets to None
    struct Write {
        invoke_ts: u64,
        return_ts: u64,
        value: Option<String>, // None = deleted/not_found
    }

    let mut writes: Vec<Write> = Vec::new();
    // Initial state: register is empty
    writes.push(Write { invoke_ts: 0, return_ts: 0, value: None });

    for op in ops {
        match &op.op_type {
            OpType::Put { data_hash, .. } => {
                if op.result != OpResult::Error {
                    writes.push(Write {
                        invoke_ts: op.invoke_ts,
                        return_ts: op.return_ts.unwrap_or(u64::MAX),
                        value: Some(data_hash.clone()),
                    });
                }
            }
            OpType::Delete { .. } => {
                if op.result == OpResult::Ok {
                    writes.push(Write {
                        invoke_ts: op.invoke_ts,
                        return_ts: op.return_ts.unwrap_or(u64::MAX),
                        value: None,
                    });
                }
            }
            _ => {}
        }
    }

    // For each read, check if the returned value is possible
    for op in ops {
        if let OpType::Get { .. } = &op.op_type {
            let read_ts_start = op.invoke_ts;
            let read_ts_end = op.return_ts.unwrap_or(u64::MAX);

            let read_value = match &op.result {
                OpResult::GetOk { data_hash } => Some(data_hash.clone()),
                OpResult::NotFound => None,
                OpResult::Error => continue, // Ambiguous, skip
                _ => continue,
            };

            // Find all writes that could be the "last write" at some point
            // in [read_ts_start, read_ts_end]
            let mut possible_values: HashSet<Option<String>> = HashSet::new();
            for w in &writes {
                // Write could be linearized at any point in [w.invoke_ts, w.return_ts]
                // Read could be linearized at any point in [read_ts_start, read_ts_end]
                // If a write's interval overlaps or precedes the read's interval,
                // it could be the most recent write at some linearization point of the read
                if w.return_ts <= read_ts_end {
                    possible_values.insert(w.value.clone());
                }
            }

            if !possible_values.contains(&read_value) {
                violations.push(format!(
                    "Stale read on {}: op {} read {:?} but possible values are {:?}",
                    key, op.id, read_value, possible_values
                ));
            }
        }
    }

    if violations.is_empty() { Ok(()) } else { Err(violations) }
}

/// Placeholder for rename-linked register check (Task 3).
fn check_rename_linked(_ops: &[&Operation]) -> Result<(), Vec<String>> {
    Ok(()) // Will be implemented in Task 3
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test --lib -p dfs-client 2>&1 | tail -10
```

Expected: All 6 tests pass (3 parser + 3 linearizability).

- [ ] **Step 4: Commit**

```bash
git add dfs/client/src/checker.rs
git commit -m "feat(lincheck): add WGL linearizability checker for simple registers"
```

---

### Task 3: Checker — multi-register rename verification

**Files:**
- Modify: `dfs/client/src/checker.rs`

- [ ] **Step 1: Write failing tests for rename linearizability**

Add tests:

```rust
#[test]
fn test_linearizable_rename() {
    // put /a, rename /a→/z, get /z returns data — linearizable
    let ops = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 0, op_type: OpType::Rename { src: "/a".into(), dst: "/z".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::Ok },
        Operation { id: 3, client: 1, op_type: OpType::Get { path: "/z".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::GetOk { data_hash: "v1".into() } },
    ];
    assert!(check_linearizability(&ops).is_ok());
}

#[test]
fn test_non_linearizable_value_lost() {
    // rename completes, but both src and dst return not_found — value lost
    let ops = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 0, op_type: OpType::Rename { src: "/a".into(), dst: "/z".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::Ok },
        Operation { id: 3, client: 1, op_type: OpType::Get { path: "/a".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::NotFound },
        Operation { id: 4, client: 2, op_type: OpType::Get { path: "/z".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::NotFound },
    ];
    assert!(check_linearizability(&ops).is_err());
}

#[test]
fn test_non_linearizable_value_duplicated() {
    // rename completes, but both src and dst have the value — duplicated
    let ops = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 0, op_type: OpType::Rename { src: "/a".into(), dst: "/z".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::Ok },
        Operation { id: 3, client: 1, op_type: OpType::Get { path: "/a".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::GetOk { data_hash: "v1".into() } },
        Operation { id: 4, client: 2, op_type: OpType::Get { path: "/z".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::GetOk { data_hash: "v1".into() } },
    ];
    assert!(check_linearizability(&ops).is_err());
}
```

- [ ] **Step 2: Implement check_rename_linked**

Replace the placeholder `check_rename_linked` with the multi-register verification. The algorithm:

1. Collect all registers touched by renames
2. Build a timeline-ordered sequence of operations
3. Try to construct a valid linearization using backtracking search
4. Rename operations constrain two registers atomically

The implementation should use a state-machine approach: maintain current value of each register, try linearizing operations in timestamp order, backtrack when inconsistency found.

This is the most complex function. Key logic:
- State: `HashMap<String, Option<String>>` mapping each path to its current value
- For each operation (sorted by invoke_ts), try placing its linearization point at the earliest valid position
- For rename: atomically move value from src to dst
- For reads: verify the returned value matches state at the linearization point
- Error ops: try both applied and not-applied

If the number of ambiguous operations exceeds 15, return `Ok(())` with a warning (see spec).

- [ ] **Step 3: Run tests**

```bash
cargo test --lib -p dfs-client -- checker 2>&1 | tail -15
```

Expected: All 9 tests pass (3 parser + 3 simple + 3 rename).

- [ ] **Step 4: Commit**

```bash
git add dfs/client/src/checker.rs
git commit -m "feat(lincheck): add multi-register rename linearizability verification"
```

---

### Task 4: Checker — self-test suite and CLI subcommand

**Files:**
- Modify: `dfs/client/src/checker.rs` (add self-test function)
- Modify: `dfs/client/src/bin/dfs_cli.rs` (add check-history subcommand)

- [ ] **Step 1: Add self-test function to checker**

Add to `checker.rs`:

```rust
/// Run built-in self-tests to validate the checker itself.
/// Returns Ok(()) if all self-tests pass, Err with description if any fail.
pub fn run_self_tests() -> Result<(), String> {
    // Test 1: Known-good history (linearizable)
    let good = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 0, op_type: OpType::Rename { src: "/a".into(), dst: "/z".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::Ok },
        Operation { id: 3, client: 1, op_type: OpType::Get { path: "/z".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::GetOk { data_hash: "v1".into() } },
    ];
    if check_linearizability(&good).is_err() {
        return Err("Self-test FAIL: known-good history rejected".into());
    }

    // Test 2: Known-bad (value lost after rename)
    let bad_lost = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/a".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 0, op_type: OpType::Rename { src: "/a".into(), dst: "/z".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::Ok },
        Operation { id: 3, client: 1, op_type: OpType::Get { path: "/a".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::NotFound },
        Operation { id: 4, client: 2, op_type: OpType::Get { path: "/z".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::NotFound },
    ];
    if check_linearizability(&bad_lost).is_ok() {
        return Err("Self-test FAIL: value-lost violation not detected".into());
    }

    // Test 3: Known-bad (stale read)
    let bad_stale = vec![
        Operation { id: 1, client: 0, op_type: OpType::Put { path: "/x".into(), data_hash: "v1".into() }, invoke_ts: 100, return_ts: Some(200), result: OpResult::PutOk { data_hash: "v1".into() } },
        Operation { id: 2, client: 0, op_type: OpType::Put { path: "/x".into(), data_hash: "v2".into() }, invoke_ts: 300, return_ts: Some(400), result: OpResult::PutOk { data_hash: "v2".into() } },
        Operation { id: 3, client: 1, op_type: OpType::Get { path: "/x".into() }, invoke_ts: 500, return_ts: Some(600), result: OpResult::GetOk { data_hash: "v1".into() } },
    ];
    if check_linearizability(&bad_stale).is_ok() {
        return Err("Self-test FAIL: stale-read violation not detected".into());
    }

    Ok(())
}
```

- [ ] **Step 2: Add CheckHistory subcommand to dfs_cli**

In `dfs/client/src/bin/dfs_cli.rs`, add to the `Commands` enum:

```rust
/// Check a JSONL history file for linearizability violations
CheckHistory {
    /// Path to the JSONL history file
    path: String,
    /// Run built-in self-tests instead of checking a file
    #[arg(long)]
    self_test: bool,
},
```

Add the handler in the main match:

```rust
Commands::CheckHistory { path, self_test } => {
    if self_test {
        match dfs_client::checker::run_self_tests() {
            Ok(()) => {
                println!("All checker self-tests passed.");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("Checker self-test FAILED: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        let file = std::fs::File::open(&path)
            .unwrap_or_else(|e| { eprintln!("Cannot open {}: {}", path, e); std::process::exit(1); });
        let reader = std::io::BufReader::new(file);
        let ops = dfs_client::checker::parse_history(reader)
            .unwrap_or_else(|e| { eprintln!("Parse error: {}", e); std::process::exit(1); });
        println!("Parsed {} operations from {}", ops.len(), path);
        match dfs_client::checker::check_linearizability(&ops) {
            Ok(()) => {
                println!("Linearizability check PASSED ({} operations)", ops.len());
                std::process::exit(0);
            }
            Err(violations) => {
                eprintln!("Linearizability check FAILED:");
                for v in &violations {
                    eprintln!("  - {}", v);
                }
                std::process::exit(1);
            }
        }
    }
}
```

- [ ] **Step 3: Build and test**

```bash
cargo build --release 2>&1 | tail -3
cargo test --lib -p dfs-client -- checker 2>&1 | tail -10
# Test CLI
./target/release/dfs_cli check-history --self-test
```

Expected: All tests pass, self-test CLI returns 0.

- [ ] **Step 4: Commit**

```bash
git add dfs/client/src/checker.rs dfs/client/src/bin/dfs_cli.rs
git commit -m "feat(lincheck): add checker self-test suite and check-history CLI subcommand"
```

---

### Task 5: Workload generator

**Files:**
- Create: `dfs/client/src/workload.rs`
- Modify: `dfs/client/src/mod.rs` (add module)
- Modify: `dfs/client/src/bin/dfs_cli.rs` (add workload subcommand)

- [ ] **Step 1: Create workload.rs**

Create `dfs/client/src/workload.rs` with the concurrent workload generator. Key design:
- Uses `tokio::spawn` to run `--clients` concurrent tasks
- Each task runs `--ops` operations (put/get/delete/rename with configurable ratio)
- Records JSONL to a shared file via `tokio::sync::Mutex<std::fs::File>`
- Uses `SystemTime::now()` for timestamps
- Paths drawn from key-space spanning both shards (`/a/lin_N`, `/z/lin_N`)

```rust
use crate::Client;
use crate::checker::HistoryEntry;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct WorkloadConfig {
    pub ops_per_client: usize,
    pub num_clients: usize,
    pub key_space: usize,
    pub rename_ratio: f64,
    pub history_path: String,
}

pub async fn run_workload(client: Client, config: WorkloadConfig) -> anyhow::Result<()> {
    let history_file = std::fs::File::create(&config.history_path)?;
    let writer = Arc::new(Mutex::new(std::io::BufWriter::new(history_file)));

    // Generate key paths spanning both shards
    let mut paths = Vec::new();
    for i in 0..config.key_space {
        if i % 2 == 0 {
            paths.push(format!("/a/lin_{}", i));
        } else {
            paths.push(format!("/z/lin_{}", i));
        }
    }
    let paths = Arc::new(paths);

    let mut handles = Vec::new();
    for client_id in 0..config.num_clients {
        let client = client.clone();
        let writer = writer.clone();
        let paths = paths.clone();
        let ops = config.ops_per_client;
        let rename_ratio = config.rename_ratio;

        handles.push(tokio::spawn(async move {
            run_client_workload(client_id as u64, client, writer, paths, ops, rename_ratio).await
        }));
    }

    for h in handles {
        h.await??;
    }

    Ok(())
}

async fn run_client_workload(
    client_id: u64,
    client: Client,
    writer: Arc<Mutex<std::io::BufWriter<std::fs::File>>>,
    paths: Arc<Vec<String>>,
    ops: usize,
    rename_ratio: f64,
) -> anyhow::Result<()> {
    use rand::Rng;
    use std::io::Write;
    use std::time::SystemTime;

    let mut rng = rand::thread_rng();
    let mut op_counter: u64 = client_id * 100_000; // Unique IDs per client

    for _ in 0..ops {
        op_counter += 1;
        let op_id = op_counter;
        let r: f64 = rng.gen();

        if r < rename_ratio {
            // Rename operation
            let src_idx = rng.gen_range(0..paths.len());
            let mut dst_idx = rng.gen_range(0..paths.len());
            while dst_idx == src_idx { dst_idx = rng.gen_range(0..paths.len()); }
            let src = &paths[src_idx];
            let dst = &paths[dst_idx];

            let invoke_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
            write_entry(&writer, &HistoryEntry {
                id: op_id, client: client_id, entry_type: "invoke".into(),
                op: "rename".into(), path: None, src: Some(src.clone()), dst: Some(dst.clone()),
                data_hash: None, result: None, ts_ns: invoke_ts,
            }).await;

            let result = match client.rename_file(src, dst).await {
                Ok(()) => "ok",
                Err(_) => "error",
            };

            let return_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
            write_entry(&writer, &HistoryEntry {
                id: op_id, client: client_id, entry_type: "return".into(),
                op: "rename".into(), path: None, src: Some(src.clone()), dst: Some(dst.clone()),
                data_hash: None, result: Some(result.into()), ts_ns: return_ts,
            }).await;
        } else {
            let op_r: f64 = rng.gen();
            let path = &paths[rng.gen_range(0..paths.len())];

            if op_r < 0.33 {
                // Put
                let data = format!("data_{}_{}", client_id, op_id);
                let data_hash = format!("{:x}", md5::compute(&data));

                let invoke_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
                write_entry(&writer, &HistoryEntry {
                    id: op_id, client: client_id, entry_type: "invoke".into(),
                    op: "put".into(), path: Some(path.clone()), src: None, dst: None,
                    data_hash: Some(data_hash.clone()), result: None, ts_ns: invoke_ts,
                }).await;

                let result = match client.create_file_from_buffer(data.into_bytes(), path).await {
                    Ok(()) => "ok",
                    Err(_) => "error",
                };

                let return_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
                write_entry(&writer, &HistoryEntry {
                    id: op_id, client: client_id, entry_type: "return".into(),
                    op: "put".into(), path: Some(path.clone()), src: None, dst: None,
                    data_hash: Some(data_hash), result: Some(result.into()), ts_ns: return_ts,
                }).await;
            } else if op_r < 0.66 {
                // Get
                let invoke_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
                write_entry(&writer, &HistoryEntry {
                    id: op_id, client: client_id, entry_type: "invoke".into(),
                    op: "get".into(), path: Some(path.clone()), src: None, dst: None,
                    data_hash: None, result: None, ts_ns: invoke_ts,
                }).await;

                let (result, data_hash) = match client.get_file_content(path).await {
                    Ok(content) => ("ok".to_string(), Some(format!("{:x}", md5::compute(&content)))),
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("not found") || msg.contains("Not found") {
                            ("not_found".to_string(), None)
                        } else {
                            ("error".to_string(), None)
                        }
                    }
                };

                let return_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
                write_entry(&writer, &HistoryEntry {
                    id: op_id, client: client_id, entry_type: "return".into(),
                    op: "get".into(), path: Some(path.clone()), src: None, dst: None,
                    data_hash: data_hash, result: Some(result), ts_ns: return_ts,
                }).await;
            } else {
                // Delete
                let invoke_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
                write_entry(&writer, &HistoryEntry {
                    id: op_id, client: client_id, entry_type: "invoke".into(),
                    op: "delete".into(), path: Some(path.clone()), src: None, dst: None,
                    data_hash: None, result: None, ts_ns: invoke_ts,
                }).await;

                let result = match client.delete_file(path).await {
                    Ok(()) => "ok",
                    Err(_) => "error",
                };

                let return_ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
                write_entry(&writer, &HistoryEntry {
                    id: op_id, client: client_id, entry_type: "return".into(),
                    op: "delete".into(), path: Some(path.clone()), src: None, dst: None,
                    data_hash: None, result: Some(result.into()), ts_ns: return_ts,
                }).await;
            }
        }
    }
    Ok(())
}

async fn write_entry(
    writer: &Arc<Mutex<std::io::BufWriter<std::fs::File>>>,
    entry: &HistoryEntry,
) {
    use std::io::Write;
    let json = serde_json::to_string(entry).unwrap();
    let mut w = writer.lock().await;
    writeln!(w, "{}", json).unwrap();
    w.flush().unwrap();
}
```

- [ ] **Step 2: Add module and CLI subcommand**

In `dfs/client/src/mod.rs`, add: `pub mod workload;`

In `dfs/client/src/bin/dfs_cli.rs`, add to Commands enum:

```rust
/// Run concurrent workload and record operation history
Workload {
    #[arg(long, default_value = "50")]
    ops: usize,
    #[arg(long, default_value = "5")]
    clients: usize,
    #[arg(long, default_value = "4")]
    key_space: usize,
    #[arg(long, default_value = "0.3")]
    rename_ratio: f64,
    #[arg(long)]
    history: String,
},
```

Add handler:

```rust
Commands::Workload { ops, clients, key_space, rename_ratio, history } => {
    let config = dfs_client::workload::WorkloadConfig {
        ops_per_client: ops,
        num_clients: clients,
        key_space,
        rename_ratio,
        history_path: history,
    };
    if let Err(e) = dfs_client::workload::run_workload(client, config).await {
        eprintln!("Workload failed: {}", e);
        std::process::exit(1);
    }
    println!("Workload completed.");
}
```

- [ ] **Step 3: Build**

```bash
cargo build --release 2>&1 | tail -5
cargo clippy -- -D warnings 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git add dfs/client/src/workload.rs dfs/client/src/mod.rs dfs/client/src/bin/dfs_cli.rs
git commit -m "feat(lincheck): add concurrent workload generator with JSONL history recording"
```

---

### Task 6: Test orchestrator shell script

**Files:**
- Create: `test_scripts/linearizability_test.sh`

- [ ] **Step 1: Create the orchestrator script**

Create `test_scripts/linearizability_test.sh` with 7 fault scenarios. The script:
1. Starts the toxiproxy-sharded cluster
2. Runs checker self-test
3. For each scenario: run workload from `dfs-chunkserver-extra`, inject fault, heal, check history
4. Cleanup

Each scenario uses `docker exec dfs-chunkserver-extra /app/dfs_cli workload ...` (neutral container that is never killed).

Fault injection functions:
- `inject_fault_baseline`: no-op
- `inject_fault_coordinator_kill`: `docker kill dfs-master1-shard1`
- `inject_fault_participant_kill`: `docker kill dfs-master1-shard2`
- `inject_fault_network_partition`: Toxiproxy timeout on master1-shard1 proxy
- `inject_fault_chunkserver_kill`: `docker kill dfs-chunkserver1-shard1`
- `inject_fault_leader_failover`: `docker stop dfs-master1-shard1`
- `inject_fault_delayed_commit`: Toxiproxy 10s latency + kill coordinator

Cleanup function removes `/a/lin_*` and `/z/lin_*` files between scenarios.

- [ ] **Step 2: Make executable**

```bash
chmod +x test_scripts/linearizability_test.sh
```

- [ ] **Step 3: Commit**

```bash
git add test_scripts/linearizability_test.sh
git commit -m "feat(lincheck): add linearizability test orchestrator with 7 fault scenarios"
```

---

### Task 7: Final verification

- [ ] **Step 1: Run full unit tests**

```bash
cargo test 2>&1 | grep "^test result"
```

Expected: All test suites pass (including new checker tests).

- [ ] **Step 2: Clippy + format**

```bash
cargo clippy -- -D warnings && cargo fmt --all -- --check
```

- [ ] **Step 3: Run checker self-test via CLI**

```bash
./target/release/dfs_cli check-history --self-test
```

Expected: "All checker self-tests passed."

- [ ] **Step 4: Push and create PR**

```bash
git push -u origin feature/linearizability-testing
gh pr create --title "feat: linearizability testing framework with WGL checker" --body "..."
```
