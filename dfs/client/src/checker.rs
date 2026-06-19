//! Linearizability checker for DFS operations.
//!
//! Parses JSONL history logs and verifies that the observed history is
//! linearizable using a Wing-Gong-Leung (WGL) style approach, extended
//! to handle multi-register rename operations.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;

// ---------------------------------------------------------------------------
// 1. Data types
// ---------------------------------------------------------------------------

/// A single line from the JSONL history log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: u64,
    #[serde(default)]
    pub client: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub src: String,
    #[serde(default)]
    pub dst: String,
    #[serde(default)]
    pub data_hash: String,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub ts_ns: u64,
}

/// The type of operation together with its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpType {
    Put { path: String, data_hash: String },
    Get { path: String },
    Delete { path: String },
    Rename { src: String, dst: String },
}

/// Result of an operation as observed in the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpResult {
    Ok,
    NotFound,
    Error,
    PutOk { data_hash: String },
    GetOk { data_hash: String },
    Unknown,
}

/// A paired invoke/return operation.
#[derive(Debug, Clone)]
pub struct Operation {
    pub id: u64,
    pub client: String,
    pub op_type: OpType,
    pub invoke_ts: u64,
    pub return_ts: u64, // 0 means crashed (no return observed)
    pub result: OpResult,
}

// ---------------------------------------------------------------------------
// 2. JSONL Parser
// ---------------------------------------------------------------------------

/// Parse a JSONL history into paired `Operation`s.
///
/// Each operation should have an "invoke" entry and optionally a "return"
/// entry with the same `id`. Crashed ops (invoke without return) get
/// `return_ts = 0` and `result = Unknown`.
pub fn parse_history(reader: impl BufRead) -> Result<Vec<Operation>, String> {
    let mut invokes: HashMap<u64, HistoryEntry> = HashMap::new();
    let mut ops: BTreeMap<u64, Operation> = BTreeMap::new();

    for (line_no, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("line {}: {}", line_no + 1, e))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry: HistoryEntry =
            serde_json::from_str(trimmed).map_err(|e| format!("line {}: {}", line_no + 1, e))?;

        if entry.entry_type == "invoke" {
            invokes.insert(entry.id, entry);
        } else if entry.entry_type == "return" {
            if let Some(inv) = invokes.remove(&entry.id) {
                let op_type = parse_op_type(&inv)?;
                let result = parse_result(&entry);
                ops.insert(
                    inv.id,
                    Operation {
                        id: inv.id,
                        client: inv.client.clone(),
                        op_type,
                        invoke_ts: inv.ts_ns,
                        return_ts: entry.ts_ns,
                        result,
                    },
                );
            } else {
                return Err(format!(
                    "return without matching invoke for id {}",
                    entry.id
                ));
            }
        } else {
            return Err(format!(
                "unknown entry type '{}' at line {}",
                entry.entry_type,
                line_no + 1
            ));
        }
    }

    // Crashed ops: invokes without a matching return
    for (id, inv) in invokes {
        let op_type = parse_op_type(&inv)?;
        ops.insert(
            id,
            Operation {
                id,
                client: inv.client.clone(),
                op_type,
                invoke_ts: inv.ts_ns,
                return_ts: 0,
                result: OpResult::Unknown,
            },
        );
    }

    Ok(ops.into_values().collect())
}

fn parse_op_type(entry: &HistoryEntry) -> Result<OpType, String> {
    match entry.op.as_str() {
        "put" => Ok(OpType::Put {
            path: entry.path.clone(),
            data_hash: entry.data_hash.clone(),
        }),
        "get" => Ok(OpType::Get {
            path: entry.path.clone(),
        }),
        "delete" => Ok(OpType::Delete {
            path: entry.path.clone(),
        }),
        "rename" => Ok(OpType::Rename {
            src: entry.src.clone(),
            dst: entry.dst.clone(),
        }),
        other => Err(format!("unknown op '{}'", other)),
    }
}

fn parse_result(entry: &HistoryEntry) -> OpResult {
    match entry.result.as_str() {
        "ok" => OpResult::Ok,
        "not_found" => OpResult::NotFound,
        "error" => OpResult::Error,
        s if s.starts_with("put_ok:") => OpResult::PutOk {
            data_hash: s[7..].to_string(),
        },
        s if s.starts_with("get_ok:") => OpResult::GetOk {
            data_hash: s[7..].to_string(),
        },
        _ => OpResult::Unknown,
    }
}

// ---------------------------------------------------------------------------
// 3 & 4. Linearizability checkers
// ---------------------------------------------------------------------------

/// Check linearizability of a history of operations.
///
/// Returns `Ok(())` if the history is linearizable, or `Err(violations)` with
/// a list of violation descriptions.
pub fn check_linearizability(ops: &[Operation]) -> Result<(), Vec<String>> {
    // Identify keys touched by rename operations
    let mut rename_keys: HashSet<String> = HashSet::new();
    let mut rename_ops: Vec<&Operation> = Vec::new();

    for op in ops {
        if let OpType::Rename { src, dst } = &op.op_type {
            rename_keys.insert(src.clone());
            rename_keys.insert(dst.clone());
            rename_ops.push(op);
        }
    }

    // Collect ops that touch rename-linked keys
    let mut linked_ops: Vec<&Operation> = Vec::new();
    let mut simple_ops: Vec<&Operation> = Vec::new();

    for op in ops {
        let key = op_key(op);
        let touches_rename = match key {
            Some(k) => rename_keys.contains(k),
            None => true, // rename itself
        };
        if touches_rename {
            linked_ops.push(op);
        } else {
            simple_ops.push(op);
        }
    }

    let mut violations = Vec::new();

    // Check simple (per-key) registers
    let mut by_key: HashMap<&str, Vec<&Operation>> = HashMap::new();
    for op in &simple_ops {
        if let Some(k) = op_key(op) {
            by_key.entry(k).or_default().push(op);
        }
    }
    for (key, key_ops) in &by_key {
        if let Err(errs) = check_single_register(key, key_ops) {
            violations.extend(errs);
        }
    }

    // Check rename-linked registers
    if !linked_ops.is_empty() {
        if let Err(errs) = check_rename_linked(&linked_ops) {
            violations.extend(errs);
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Return the key (path) for non-rename ops.
fn op_key(op: &Operation) -> Option<&str> {
    match &op.op_type {
        OpType::Put { path, .. } | OpType::Get { path } | OpType::Delete { path } => {
            Some(path.as_str())
        }
        OpType::Rename { .. } => None,
    }
}

/// Per-key single-register linearizability check.
///
/// Tracks the sequence of writes and verifies that each read could have seen
/// the value that was current at some point during the read's [invoke, return]
/// interval.
fn check_single_register(key: &str, ops: &[&Operation]) -> Result<(), Vec<String>> {
    // Collect writes (puts/deletes) and reads (gets) separately
    #[derive(Debug, Clone)]
    struct WriteEvent {
        ts: u64,
        value: Option<String>, // None = deleted/not-yet-created, Some(hash) = put
    }

    let mut writes: Vec<WriteEvent> = Vec::new();
    let mut reads: Vec<&Operation> = Vec::new();

    // Initial state: empty
    writes.push(WriteEvent { ts: 0, value: None });

    // Sort all ops by invoke_ts to establish ordering
    let mut sorted: Vec<&Operation> = ops.to_vec();
    sorted.sort_by_key(|o| o.invoke_ts);

    for op in &sorted {
        match &op.op_type {
            OpType::Put { data_hash, .. } => {
                // Write takes effect at some point in [invoke_ts, return_ts]
                // We use return_ts as the linearization point (latest possible)
                let effect_ts = if op.return_ts > 0 {
                    op.return_ts
                } else {
                    op.invoke_ts
                };
                writes.push(WriteEvent {
                    ts: effect_ts,
                    value: Some(data_hash.clone()),
                });
            }
            OpType::Delete { .. } => {
                let effect_ts = if op.return_ts > 0 {
                    op.return_ts
                } else {
                    op.invoke_ts
                };
                writes.push(WriteEvent {
                    ts: effect_ts,
                    value: None,
                });
            }
            OpType::Get { .. } => {
                reads.push(op);
            }
            _ => {}
        }
    }

    writes.sort_by_key(|w| w.ts);

    let mut violations = Vec::new();

    for read in &reads {
        // Skip crashed or error reads
        if read.return_ts == 0 {
            continue;
        }
        match &read.result {
            OpResult::Error | OpResult::Unknown => continue,
            _ => {}
        }

        let read_value: Option<String> = match &read.result {
            OpResult::GetOk { data_hash } => Some(data_hash.clone()),
            OpResult::NotFound => None,
            OpResult::Ok => None, // treat as not-found for gets
            _ => continue,
        };

        // The read can be linearized at any point in [invoke_ts, return_ts].
        // We need to find at least one write whose value matches and whose
        // effect is visible (write.ts <= some point in read interval) and not
        // yet overwritten by a later write before the read interval ends.
        let invoke = read.invoke_ts;
        let ret = read.return_ts;

        let mut found_valid = false;

        // For each write, check if the read could see it:
        // A write w_i is visible if w_i.ts <= ret (the read could happen after it)
        // AND no later write w_{i+1} has w_{i+1}.ts <= invoke (which would have
        // overwritten it before the read started)
        for (i, w) in writes.iter().enumerate() {
            if w.ts > ret {
                break; // no point checking later writes
            }
            // Check if this write's value matches
            if w.value != read_value {
                continue;
            }
            // Check if a subsequent write overwrites before read starts
            let overwritten_before_read = if i + 1 < writes.len() {
                writes[i + 1].ts <= invoke
            } else {
                false
            };
            if !overwritten_before_read {
                found_valid = true;
                break;
            }
        }

        if !found_valid {
            violations.push(format!(
                "key '{}': read op {} returned {:?} but no valid write visible in [{}, {}]",
                key, read.id, read_value, invoke, ret,
            ));
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

// ---------------------------------------------------------------------------
// 4. Multi-register rename checker
// ---------------------------------------------------------------------------

/// State of all registers being tracked.
type RegisterState = HashMap<String, Option<String>>;

/// Check linearizability for operations that involve renames (multi-register).
///
/// Uses a backtracking search to find a valid linearization order.
fn check_rename_linked(ops: &[&Operation]) -> Result<(), Vec<String>> {
    // Sort by invoke_ts
    let mut sorted: Vec<&Operation> = ops.to_vec();
    sorted.sort_by_key(|o| o.invoke_ts);

    // Identify all keys
    let mut all_keys: HashSet<String> = HashSet::new();
    for op in &sorted {
        match &op.op_type {
            OpType::Put { path, .. } | OpType::Get { path } | OpType::Delete { path } => {
                all_keys.insert(path.clone());
            }
            OpType::Rename { src, dst } => {
                all_keys.insert(src.clone());
                all_keys.insert(dst.clone());
            }
        }
    }

    // Initial state: all keys empty
    let mut initial_state: RegisterState = HashMap::new();
    for k in &all_keys {
        initial_state.insert(k.clone(), None);
    }

    // Count ambiguous (error/crashed) ops for limiting backtracking
    let ambiguous_count = sorted
        .iter()
        .filter(|o| o.return_ts == 0 || matches!(o.result, OpResult::Error | OpResult::Unknown))
        .count();

    let limit_backtrack = ambiguous_count > 15;

    // Try to find a linearization using backtracking
    let mut linearized: Vec<usize> = Vec::new();
    let mut remaining: Vec<usize> = (0..sorted.len()).collect();

    if try_linearize(
        &sorted,
        &initial_state,
        &mut linearized,
        &mut remaining,
        limit_backtrack,
    ) {
        Ok(())
    } else {
        // Produce diagnostic violations
        let violations = diagnose_violations(&sorted, &initial_state);
        if violations.is_empty() {
            Err(vec![
                "history is not linearizable (no valid ordering found)".to_string(),
            ])
        } else {
            Err(violations)
        }
    }
}

/// Recursive backtracking linearizability search.
///
/// Tries to schedule one of the `remaining` ops next, checking that the
/// observed result is consistent with the current state.
fn try_linearize(
    ops: &[&Operation],
    state: &RegisterState,
    linearized: &mut Vec<usize>,
    remaining: &mut Vec<usize>,
    limit_backtrack: bool,
) -> bool {
    if remaining.is_empty() {
        return true;
    }

    // Find the earliest return_ts among linearized ops (or 0).
    // An op can be linearized only if its invoke_ts <= current_time,
    // where current_time is the latest return_ts we've committed to.
    // Actually, for correctness we need: the op's interval overlaps
    // the current linearization point.
    //
    // Simplification: an op is "available" if its invoke_ts <= min(return_ts)
    // of all not-yet-linearized ops that have a return before it invoked.
    // But the simplest correct approach: try all remaining ops whose invoke_ts
    // is <= the minimum return_ts of any remaining op (including itself).

    let min_return = remaining
        .iter()
        .filter_map(|&i| {
            let r = ops[i].return_ts;
            if r > 0 {
                Some(r)
            } else {
                None
            }
        })
        .min()
        .unwrap_or(u64::MAX);

    // Candidates: ops whose invoke_ts <= min_return
    let candidates: Vec<usize> = remaining
        .iter()
        .filter(|&&i| ops[i].invoke_ts <= min_return)
        .copied()
        .collect();

    if candidates.is_empty() {
        // Crashed ops only remain; try them all
        let candidates: Vec<usize> = remaining.clone();
        for idx in candidates {
            let pos = remaining.iter().position(|&x| x == idx).unwrap();
            remaining.remove(pos);
            linearized.push(idx);

            // Crashed/unknown ops: try both applied and not-applied
            if let Some(new_state) = apply_op(ops[idx], state, true) {
                if try_linearize(ops, &new_state, linearized, remaining, limit_backtrack) {
                    return true;
                }
            }
            if !limit_backtrack {
                // Try not-applied (state unchanged)
                if try_linearize(ops, state, linearized, remaining, limit_backtrack) {
                    return true;
                }
            }

            linearized.pop();
            remaining.insert(pos, idx);
        }
        return false;
    }

    for idx in candidates {
        let pos = remaining.iter().position(|&x| x == idx).unwrap();
        remaining.remove(pos);
        linearized.push(idx);

        let op = ops[idx];

        // For error/crashed ops, try both applied and not-applied
        if op.return_ts == 0 || matches!(op.result, OpResult::Error | OpResult::Unknown) {
            // Try applied
            if let Some(new_state) = apply_op(op, state, true) {
                if try_linearize(ops, &new_state, linearized, remaining, limit_backtrack) {
                    return true;
                }
            }
            if !limit_backtrack {
                // Try not-applied
                if try_linearize(ops, state, linearized, remaining, limit_backtrack) {
                    return true;
                }
            }
        } else {
            // Check if the observed result is consistent with state
            if let Some(new_state) = check_and_apply(op, state) {
                if try_linearize(ops, &new_state, linearized, remaining, limit_backtrack) {
                    return true;
                }
            }
        }

        linearized.pop();
        remaining.insert(pos, idx);
    }

    false
}

/// Check whether `op`'s observed result is consistent with `state`, and if so,
/// return the new state after applying the op.
fn check_and_apply(op: &Operation, state: &RegisterState) -> Option<RegisterState> {
    let mut new_state = state.clone();

    match &op.op_type {
        OpType::Put { path, data_hash } => {
            match &op.result {
                OpResult::Ok | OpResult::PutOk { .. } => {
                    new_state.insert(path.clone(), Some(data_hash.clone()));
                    Some(new_state)
                }
                OpResult::Error => {
                    // Could have applied or not
                    None // handled by caller as ambiguous
                }
                _ => Some(new_state), // treat unexpected results leniently
            }
        }
        OpType::Get { path } => {
            let current = new_state.get(path).cloned().unwrap_or(None);
            match &op.result {
                OpResult::GetOk { data_hash } => {
                    if current.as_deref() == Some(data_hash.as_str()) {
                        Some(new_state)
                    } else {
                        None // stale or wrong read
                    }
                }
                OpResult::NotFound => {
                    if current.is_none() {
                        Some(new_state)
                    } else {
                        None // should have found data
                    }
                }
                OpResult::Ok => {
                    // Ambiguous — treat as not-found
                    if current.is_none() {
                        Some(new_state)
                    } else {
                        None
                    }
                }
                _ => Some(new_state),
            }
        }
        OpType::Delete { path } => match &op.result {
            OpResult::Ok => {
                new_state.insert(path.clone(), None);
                Some(new_state)
            }
            OpResult::NotFound => {
                if new_state.get(path).cloned().unwrap_or(None).is_none() {
                    Some(new_state)
                } else {
                    None
                }
            }
            _ => {
                new_state.insert(path.clone(), None);
                Some(new_state)
            }
        },
        OpType::Rename { src, dst } => {
            let src_val = new_state.get(src).cloned().unwrap_or(None);
            match &op.result {
                OpResult::Ok => {
                    // Atomic rename: dst gets src's value, src becomes empty
                    if src_val.is_some() {
                        new_state.insert(dst.clone(), src_val);
                        new_state.insert(src.clone(), None);
                        Some(new_state)
                    } else {
                        None // can't rename if src doesn't exist
                    }
                }
                OpResult::NotFound => {
                    // Source didn't exist
                    if src_val.is_none() {
                        Some(new_state)
                    } else {
                        None
                    }
                }
                OpResult::Error => None, // ambiguous
                _ => Some(new_state),
            }
        }
    }
}

/// Apply an op's effect to state unconditionally (for crashed/error ops).
fn apply_op(op: &Operation, state: &RegisterState, applied: bool) -> Option<RegisterState> {
    if !applied {
        return Some(state.clone());
    }
    let mut new_state = state.clone();
    match &op.op_type {
        OpType::Put { path, data_hash } => {
            new_state.insert(path.clone(), Some(data_hash.clone()));
            Some(new_state)
        }
        OpType::Get { .. } => Some(new_state), // reads don't change state
        OpType::Delete { path } => {
            new_state.insert(path.clone(), None);
            Some(new_state)
        }
        OpType::Rename { src, dst } => {
            let src_val = new_state.get(src).cloned().unwrap_or(None);
            if src_val.is_some() {
                new_state.insert(dst.clone(), src_val);
                new_state.insert(src.clone(), None);
                Some(new_state)
            } else {
                // Can't rename non-existent source
                Some(new_state)
            }
        }
    }
}

/// Produce diagnostic messages for why a history is not linearizable.
fn diagnose_violations(ops: &[&Operation], initial_state: &RegisterState) -> Vec<String> {
    let mut violations = Vec::new();

    // Try a greedy forward pass and report the first conflict
    let mut state = initial_state.clone();
    let mut sorted: Vec<&Operation> = ops.to_vec();
    sorted.sort_by_key(|o| o.invoke_ts);

    let mut processed = vec![false; sorted.len()];
    let mut progress = true;

    while progress {
        progress = false;
        for (i, op) in sorted.iter().enumerate() {
            if processed[i] {
                continue;
            }
            if op.return_ts == 0 || matches!(op.result, OpResult::Error | OpResult::Unknown) {
                // Try applying
                if let Some(ns) = apply_op(op, &state, true) {
                    state = ns;
                    processed[i] = true;
                    progress = true;
                }
                continue;
            }
            if let Some(ns) = check_and_apply(op, &state) {
                state = ns;
                processed[i] = true;
                progress = true;
            }
        }
    }

    // Report unprocessed ops
    for (i, op) in sorted.iter().enumerate() {
        if !processed[i] {
            let current_val = match &op.op_type {
                OpType::Get { path } => state.get(path).cloned().unwrap_or(None),
                OpType::Rename { src, .. } => state.get(src).cloned().unwrap_or(None),
                _ => None,
            };
            match &op.op_type {
                OpType::Get { path } => {
                    let expected = match &op.result {
                        OpResult::GetOk { data_hash } => format!("Some({})", data_hash),
                        OpResult::NotFound => "None".to_string(),
                        _ => format!("{:?}", op.result),
                    };
                    violations.push(format!(
                        "stale read: op {} GET '{}' returned {} but state has {:?}",
                        op.id, path, expected, current_val,
                    ));
                }
                OpType::Rename { src, dst } => {
                    // Check for value lost or duplicated
                    let src_v = state.get(src).cloned().unwrap_or(None);
                    let dst_v = state.get(dst).cloned().unwrap_or(None);
                    if src_v.is_none() && dst_v.is_none() && op.result == OpResult::Ok {
                        violations.push(format!(
                            "value lost: rename op {} from '{}' to '{}' — both src and dst are empty",
                            op.id, src, dst,
                        ));
                    } else if src_v.is_some() && dst_v.is_some() && op.result == OpResult::Ok {
                        violations.push(format!(
                            "value duplicated: rename op {} from '{}' to '{}' — both src and dst have data",
                            op.id, src, dst,
                        ));
                    } else {
                        violations.push(format!(
                            "rename violation: op {} from '{}' to '{}' result={:?} but src={:?}, dst={:?}",
                            op.id, src, dst, op.result, src_v, dst_v,
                        ));
                    }
                }
                _ => {
                    violations.push(format!(
                        "linearization violation: op {} {:?} result={:?}",
                        op.id, op.op_type, op.result,
                    ));
                }
            }
        }
    }

    violations
}

// ---------------------------------------------------------------------------
// 5. Self-test function
// ---------------------------------------------------------------------------

/// Run built-in self-tests to verify the checker works correctly.
pub fn run_self_tests() -> Result<(), String> {
    // Test 1: known-good rename history should pass
    {
        let history = r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/a","data_hash":"h1","ts_ns":100}
{"id":1,"client":"c1","type":"return","op":"put","path":"/a","result":"ok","ts_ns":200}
{"id":2,"client":"c1","type":"invoke","op":"rename","src":"/a","dst":"/b","ts_ns":300}
{"id":2,"client":"c1","type":"return","op":"rename","src":"/a","dst":"/b","result":"ok","ts_ns":400}
{"id":3,"client":"c1","type":"invoke","op":"get","path":"/b","ts_ns":500}
{"id":3,"client":"c1","type":"return","op":"get","path":"/b","result":"get_ok:h1","ts_ns":600}
{"id":4,"client":"c1","type":"invoke","op":"get","path":"/a","ts_ns":500}
{"id":4,"client":"c1","type":"return","op":"get","path":"/a","result":"not_found","ts_ns":600}
"#;
        let ops =
            parse_history(history.as_bytes()).map_err(|e| format!("self-test 1 parse: {}", e))?;
        if let Err(v) = check_linearizability(&ops) {
            return Err(format!("self-test 1 (good rename) failed: {:?}", v));
        }
    }

    // Test 2: value-lost history should detect violation
    {
        let history = r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/a","data_hash":"h1","ts_ns":100}
{"id":1,"client":"c1","type":"return","op":"put","path":"/a","result":"ok","ts_ns":200}
{"id":2,"client":"c1","type":"invoke","op":"rename","src":"/a","dst":"/b","ts_ns":300}
{"id":2,"client":"c1","type":"return","op":"rename","src":"/a","dst":"/b","result":"ok","ts_ns":400}
{"id":3,"client":"c1","type":"invoke","op":"get","path":"/a","ts_ns":500}
{"id":3,"client":"c1","type":"return","op":"get","path":"/a","result":"not_found","ts_ns":600}
{"id":4,"client":"c1","type":"invoke","op":"get","path":"/b","ts_ns":500}
{"id":4,"client":"c1","type":"return","op":"get","path":"/b","result":"not_found","ts_ns":600}
"#;
        let ops =
            parse_history(history.as_bytes()).map_err(|e| format!("self-test 2 parse: {}", e))?;
        match check_linearizability(&ops) {
            Ok(()) => {
                return Err("self-test 2 (value lost) should have detected violation".to_string())
            }
            Err(v) => {
                let joined = v.join(" ");
                if !joined.contains("value lost")
                    && !joined.contains("not linearizable")
                    && !joined.contains("stale read")
                    && !joined.contains("violation")
                {
                    return Err(format!("self-test 2: violation message unclear: {:?}", v));
                }
            }
        }
    }

    // Test 3: stale-read history should detect violation
    {
        let history = r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/x","data_hash":"v1","ts_ns":100}
{"id":1,"client":"c1","type":"return","op":"put","path":"/x","result":"ok","ts_ns":200}
{"id":2,"client":"c1","type":"invoke","op":"put","path":"/x","data_hash":"v2","ts_ns":300}
{"id":2,"client":"c1","type":"return","op":"put","path":"/x","result":"ok","ts_ns":400}
{"id":3,"client":"c1","type":"invoke","op":"get","path":"/x","ts_ns":500}
{"id":3,"client":"c1","type":"return","op":"get","path":"/x","result":"get_ok:v1","ts_ns":600}
"#;
        let ops =
            parse_history(history.as_bytes()).map_err(|e| format!("self-test 3 parse: {}", e))?;
        match check_linearizability(&ops) {
            Ok(()) => {
                return Err("self-test 3 (stale read) should have detected violation".to_string())
            }
            Err(_) => { /* expected */ }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 6. Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_history(lines: &[&str]) -> Vec<Operation> {
        let input = lines.join("\n");
        parse_history(input.as_bytes()).expect("parse failed")
    }

    #[test]
    fn test_parse_put_get_history() {
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/f","data_hash":"abc","ts_ns":10}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"put","path":"/f","result":"ok","ts_ns":20}"#,
            r#"{"id":2,"client":"c1","type":"invoke","op":"get","path":"/f","ts_ns":30}"#,
            r#"{"id":2,"client":"c1","type":"return","op":"get","path":"/f","result":"get_ok:abc","ts_ns":40}"#,
        ]);
        assert_eq!(ops.len(), 2);
        assert!(
            matches!(&ops[0].op_type, OpType::Put { path, data_hash } if path == "/f" && data_hash == "abc")
        );
        assert!(matches!(&ops[1].result, OpResult::GetOk { data_hash } if data_hash == "abc"));
    }

    #[test]
    fn test_parse_rename_history() {
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"rename","src":"/a","dst":"/b","ts_ns":10}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"rename","src":"/a","dst":"/b","result":"ok","ts_ns":20}"#,
        ]);
        assert_eq!(ops.len(), 1);
        assert!(
            matches!(&ops[0].op_type, OpType::Rename { src, dst } if src == "/a" && dst == "/b")
        );
        assert_eq!(ops[0].result, OpResult::Ok);
    }

    #[test]
    fn test_parse_crashed_operation() {
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/f","data_hash":"x","ts_ns":10}"#,
        ]);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].return_ts, 0);
        assert_eq!(ops[0].result, OpResult::Unknown);
    }

    #[test]
    fn test_linearizable_simple_put_get() {
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/f","data_hash":"v1","ts_ns":100}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"put","path":"/f","result":"ok","ts_ns":200}"#,
            r#"{"id":2,"client":"c1","type":"invoke","op":"get","path":"/f","ts_ns":300}"#,
            r#"{"id":2,"client":"c1","type":"return","op":"get","path":"/f","result":"get_ok:v1","ts_ns":400}"#,
        ]);
        assert!(check_linearizability(&ops).is_ok());
    }

    #[test]
    fn test_non_linearizable_stale_read() {
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/f","data_hash":"v1","ts_ns":100}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"put","path":"/f","result":"ok","ts_ns":200}"#,
            r#"{"id":2,"client":"c1","type":"invoke","op":"put","path":"/f","data_hash":"v2","ts_ns":300}"#,
            r#"{"id":2,"client":"c1","type":"return","op":"put","path":"/f","result":"ok","ts_ns":400}"#,
            r#"{"id":3,"client":"c1","type":"invoke","op":"get","path":"/f","ts_ns":500}"#,
            r#"{"id":3,"client":"c1","type":"return","op":"get","path":"/f","result":"get_ok:v1","ts_ns":600}"#,
        ]);
        let result = check_linearizability(&ops);
        assert!(result.is_err(), "stale read should be detected");
    }

    #[test]
    fn test_linearizable_concurrent_reads() {
        // Two concurrent reads during a write — both can see either value
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/f","data_hash":"v1","ts_ns":100}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"put","path":"/f","result":"ok","ts_ns":200}"#,
            r#"{"id":2,"client":"c1","type":"invoke","op":"put","path":"/f","data_hash":"v2","ts_ns":250}"#,
            r#"{"id":3,"client":"c2","type":"invoke","op":"get","path":"/f","ts_ns":260}"#,
            r#"{"id":4,"client":"c3","type":"invoke","op":"get","path":"/f","ts_ns":270}"#,
            r#"{"id":2,"client":"c1","type":"return","op":"put","path":"/f","result":"ok","ts_ns":350}"#,
            r#"{"id":3,"client":"c2","type":"return","op":"get","path":"/f","result":"get_ok:v1","ts_ns":360}"#,
            r#"{"id":4,"client":"c3","type":"return","op":"get","path":"/f","result":"get_ok:v2","ts_ns":370}"#,
        ]);
        assert!(
            check_linearizability(&ops).is_ok(),
            "concurrent reads should be linearizable"
        );
    }

    #[test]
    fn test_linearizable_rename() {
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/a","data_hash":"h1","ts_ns":100}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"put","path":"/a","result":"ok","ts_ns":200}"#,
            r#"{"id":2,"client":"c1","type":"invoke","op":"rename","src":"/a","dst":"/b","ts_ns":300}"#,
            r#"{"id":2,"client":"c1","type":"return","op":"rename","src":"/a","dst":"/b","result":"ok","ts_ns":400}"#,
            r#"{"id":3,"client":"c1","type":"invoke","op":"get","path":"/b","ts_ns":500}"#,
            r#"{"id":3,"client":"c1","type":"return","op":"get","path":"/b","result":"get_ok:h1","ts_ns":600}"#,
            r#"{"id":4,"client":"c1","type":"invoke","op":"get","path":"/a","ts_ns":500}"#,
            r#"{"id":4,"client":"c1","type":"return","op":"get","path":"/a","result":"not_found","ts_ns":600}"#,
        ]);
        assert!(
            check_linearizability(&ops).is_ok(),
            "valid rename should be linearizable"
        );
    }

    #[test]
    fn test_non_linearizable_value_lost() {
        // After rename, both src and dst are empty — value was lost
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/a","data_hash":"h1","ts_ns":100}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"put","path":"/a","result":"ok","ts_ns":200}"#,
            r#"{"id":2,"client":"c1","type":"invoke","op":"rename","src":"/a","dst":"/b","ts_ns":300}"#,
            r#"{"id":2,"client":"c1","type":"return","op":"rename","src":"/a","dst":"/b","result":"ok","ts_ns":400}"#,
            r#"{"id":3,"client":"c1","type":"invoke","op":"get","path":"/a","ts_ns":500}"#,
            r#"{"id":3,"client":"c1","type":"return","op":"get","path":"/a","result":"not_found","ts_ns":600}"#,
            r#"{"id":4,"client":"c1","type":"invoke","op":"get","path":"/b","ts_ns":500}"#,
            r#"{"id":4,"client":"c1","type":"return","op":"get","path":"/b","result":"not_found","ts_ns":600}"#,
        ]);
        let result = check_linearizability(&ops);
        assert!(result.is_err(), "value lost should be detected");
    }

    #[test]
    fn test_non_linearizable_value_duplicated() {
        // After rename, both src and dst have data — value was duplicated
        let ops = make_history(&[
            r#"{"id":1,"client":"c1","type":"invoke","op":"put","path":"/a","data_hash":"h1","ts_ns":100}"#,
            r#"{"id":1,"client":"c1","type":"return","op":"put","path":"/a","result":"ok","ts_ns":200}"#,
            r#"{"id":2,"client":"c1","type":"invoke","op":"rename","src":"/a","dst":"/b","ts_ns":300}"#,
            r#"{"id":2,"client":"c1","type":"return","op":"rename","src":"/a","dst":"/b","result":"ok","ts_ns":400}"#,
            r#"{"id":3,"client":"c1","type":"invoke","op":"get","path":"/a","ts_ns":500}"#,
            r#"{"id":3,"client":"c1","type":"return","op":"get","path":"/a","result":"get_ok:h1","ts_ns":600}"#,
            r#"{"id":4,"client":"c1","type":"invoke","op":"get","path":"/b","ts_ns":500}"#,
            r#"{"id":4,"client":"c1","type":"return","op":"get","path":"/b","result":"get_ok:h1","ts_ns":600}"#,
        ]);
        let result = check_linearizability(&ops);
        assert!(result.is_err(), "value duplication should be detected");
    }

    #[test]
    fn test_self_tests_pass() {
        run_self_tests().expect("self-tests should pass");
    }
}
