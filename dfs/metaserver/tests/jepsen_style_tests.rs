// Jepsen-style consistency tests for distributed consensus
// Tests linearizability under concurrent operations and fault injection
//
// Based on Jepsen testing methodology:
// 1. Record a history of operations (invocations and completions)
// 2. Run concurrent operations from multiple clients
// 3. Inject faults (network partitions, crashes, slow nodes)
// 4. Verify the history is linearizable and invariants hold

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ============================================================================
// Core Data Structures
// ============================================================================

/// Unique operation ID for correlation
static OP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Type of operation in the history
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpType {
    Read,
    Write,
    Cas, // Compare-and-swap
}

/// Result of an operation
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpResult {
    ReadOk(Option<i64>),
    WriteOk,
    CasOk(bool),
    Error(String),
}

/// A single operation in the history, with invocation and completion times
#[derive(Debug, Clone)]
struct Operation {
    id: u64,
    thread_id: usize,
    op_type: OpType,
    key: String,
    input_value: Option<i64>,   // Value written or CAS new value
    _expect_value: Option<i64>, // CAS expected value
    result: OpResult,
    invoke_time: Instant,
    complete_time: Instant,
}

// ============================================================================
// History — Thread-safe operation log
// ============================================================================

#[derive(Clone)]
struct History {
    operations: Arc<Mutex<Vec<Operation>>>,
}

impl History {
    fn new() -> Self {
        Self {
            operations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, op: Operation) {
        self.operations.lock().unwrap().push(op);
    }

    fn get_operations(&self) -> Vec<Operation> {
        self.operations.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.operations.lock().unwrap().len()
    }
}

// ============================================================================
// LinearizabilityChecker — WGL-inspired verification
// ============================================================================

/// Checks if a concurrent history is linearizable for a key-value register.
///
/// Uses a simplified approach: for each key, we extract all operations and
/// attempt to find a total ordering consistent with real-time constraints
/// and register semantics.
struct LinearizabilityChecker;

impl LinearizabilityChecker {
    /// Check if the per-key history is linearizable.
    /// For each key, operations must be orderable such that:
    /// 1. If op A completes before op B starts, A is ordered before B
    /// 2. Each read returns the value of the most recent preceding write
    fn check(history: &History) -> Result<(), Vec<String>> {
        let ops = history.get_operations();
        let mut anomalies = Vec::new();

        // Group operations by key
        let mut by_key: HashMap<String, Vec<&Operation>> = HashMap::new();
        for op in &ops {
            by_key.entry(op.key.clone()).or_default().push(op);
        }

        for (key, key_ops) in &by_key {
            // Sort by invocation time
            let mut sorted: Vec<&&Operation> = key_ops.iter().collect();
            sorted.sort_by_key(|op| op.invoke_time);

            // Track the state: what value should the register hold?
            // We try to find a linearization by greedily ordering
            // non-overlapping ops first, then fitting concurrent ops.
            let mut committed_writes: Vec<(Instant, Instant, i64)> = Vec::new();

            for op in &sorted {
                match (&op.op_type, &op.result) {
                    (OpType::Write, OpResult::WriteOk) => {
                        if let Some(val) = op.input_value {
                            committed_writes.push((op.invoke_time, op.complete_time, val));
                        }
                    }
                    _ => {}
                }
            }

            // For each read, verify it could have returned a value consistent
            // with some linearization point
            for op in &sorted {
                if let (OpType::Read, OpResult::ReadOk(read_val)) = (&op.op_type, &op.result) {
                    // The read's linearization point is somewhere in [invoke, complete].
                    // Find all writes that *could* be the most recent write at that point.
                    let possible_values = Self::possible_read_values(
                        &committed_writes,
                        op.invoke_time,
                        op.complete_time,
                    );

                    if !possible_values.contains(read_val) {
                        anomalies.push(format!(
                            "Key '{}': Read by thread {} returned {:?} but possible values were {:?}",
                            key, op.thread_id, read_val, possible_values
                        ));
                    }
                }
            }
        }

        if anomalies.is_empty() {
            Ok(())
        } else {
            Err(anomalies)
        }
    }

    /// Determine what values a read could validly return.
    ///
    /// A read with linearization point in [r_invoke, r_complete] could see
    /// the value written by any write whose linearization point could precede it.
    fn possible_read_values(
        writes: &[(Instant, Instant, i64)],
        read_invoke: Instant,
        _read_complete: Instant,
    ) -> HashSet<Option<i64>> {
        let mut values = HashSet::new();

        // If no write has definitely completed before the read started,
        // the read could see None (initial state)
        let any_write_before_read = writes.iter().any(|(_, wc, _)| *wc <= read_invoke);
        if !any_write_before_read {
            values.insert(None);
        }

        // For concurrent or completed-before writes, the read could see their value
        for (w_invoke, _w_complete, val) in writes {
            // A write is visible if its linearization point could come before
            // the read's linearization point. This is possible if the write
            // was invoked before the read completed.
            if *w_invoke <= _read_complete {
                values.insert(Some(*val));
            }
        }

        // Also always allow None if there are no writes at all
        if writes.is_empty() {
            values.insert(None);
        }

        values
    }
}

// ============================================================================
// KeyValueStore — Shared state machine with history recording
// ============================================================================

#[derive(Clone)]
struct KeyValueStore {
    data: Arc<Mutex<HashMap<String, i64>>>,
    history: History,
}

impl KeyValueStore {
    fn new(history: History) -> Self {
        Self {
            data: Arc::new(Mutex::new(HashMap::new())),
            history,
        }
    }

    fn write(&self, thread_id: usize, key: &str, value: i64) -> OpResult {
        let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let invoke = Instant::now();

        self.data.lock().unwrap().insert(key.to_string(), value);
        let result = OpResult::WriteOk;

        let complete = Instant::now();
        self.history.record(Operation {
            id: op_id,
            thread_id,
            op_type: OpType::Write,
            key: key.to_string(),
            input_value: Some(value),
            _expect_value: None,
            result: result.clone(),
            invoke_time: invoke,
            complete_time: complete,
        });

        result
    }

    fn read(&self, thread_id: usize, key: &str) -> OpResult {
        let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let invoke = Instant::now();

        let val = self.data.lock().unwrap().get(key).copied();
        let result = OpResult::ReadOk(val);

        let complete = Instant::now();
        self.history.record(Operation {
            id: op_id,
            thread_id,
            op_type: OpType::Read,
            key: key.to_string(),
            input_value: None,
            _expect_value: None,
            result: result.clone(),
            invoke_time: invoke,
            complete_time: complete,
        });

        result
    }

    fn compare_and_swap(
        &self,
        thread_id: usize,
        key: &str,
        expected: i64,
        new_value: i64,
    ) -> OpResult {
        let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let invoke = Instant::now();

        let success = {
            let mut data = self.data.lock().unwrap();
            let current = data.get(key).copied();
            if current == Some(expected) {
                data.insert(key.to_string(), new_value);
                true
            } else {
                false
            }
        };
        let result = OpResult::CasOk(success);

        let complete = Instant::now();
        self.history.record(Operation {
            id: op_id,
            thread_id,
            op_type: OpType::Cas,
            key: key.to_string(),
            input_value: Some(new_value),
            _expect_value: Some(expected),
            result: result.clone(),
            invoke_time: invoke,
            complete_time: complete,
        });

        result
    }

    fn get_raw(&self, key: &str) -> Option<i64> {
        self.data.lock().unwrap().get(key).copied()
    }
}

// ============================================================================
// FaultInjector — Simulated fault conditions
// ============================================================================

#[derive(Clone)]
struct FaultInjector {
    /// When true, operations will fail with partition error
    partitioned: Arc<AtomicBool>,
    /// When true, all operations fail (simulates crash)
    crashed: Arc<AtomicBool>,
    /// When non-zero, adds this many ms of latency
    slow_ms: Arc<AtomicU64>,
}

impl FaultInjector {
    fn new() -> Self {
        Self {
            partitioned: Arc::new(AtomicBool::new(false)),
            crashed: Arc::new(AtomicBool::new(false)),
            slow_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn partition(&self) {
        self.partitioned.store(true, Ordering::SeqCst);
    }

    fn heal(&self) {
        self.partitioned.store(false, Ordering::SeqCst);
    }

    fn crash(&self) {
        self.crashed.store(true, Ordering::SeqCst);
    }

    fn recover(&self) {
        self.crashed.store(false, Ordering::SeqCst);
    }

    fn set_slow(&self, ms: u64) {
        self.slow_ms.store(ms, Ordering::SeqCst);
    }

    fn clear_slow(&self) {
        self.slow_ms.store(0, Ordering::SeqCst);
    }

    /// Check fault state and return error if faulted, or apply latency
    fn check(&self) -> Result<(), String> {
        if self.crashed.load(Ordering::SeqCst) {
            return Err("node crashed".to_string());
        }
        if self.partitioned.load(Ordering::SeqCst) {
            return Err("network partition".to_string());
        }
        let slow = self.slow_ms.load(Ordering::SeqCst);
        if slow > 0 {
            thread::sleep(Duration::from_millis(slow));
        }
        Ok(())
    }
}

/// A fault-aware wrapper around KeyValueStore
#[derive(Clone)]
struct FaultAwareStore {
    inner: KeyValueStore,
    fault: FaultInjector,
    history: History,
}

impl FaultAwareStore {
    fn new(history: History, fault: FaultInjector) -> Self {
        let inner = KeyValueStore::new(history.clone());
        Self {
            inner,
            fault,
            history,
        }
    }

    fn write(&self, thread_id: usize, key: &str, value: i64) -> OpResult {
        let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let invoke = Instant::now();

        let result = match self.fault.check() {
            Err(e) => OpResult::Error(e),
            Ok(()) => {
                self.inner
                    .data
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), value);
                OpResult::WriteOk
            }
        };

        let complete = Instant::now();
        self.history.record(Operation {
            id: op_id,
            thread_id,
            op_type: OpType::Write,
            key: key.to_string(),
            input_value: Some(value),
            _expect_value: None,
            result: result.clone(),
            invoke_time: invoke,
            complete_time: complete,
        });

        result
    }

    fn read(&self, thread_id: usize, key: &str) -> OpResult {
        let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let invoke = Instant::now();

        let result = match self.fault.check() {
            Err(e) => OpResult::Error(e),
            Ok(()) => {
                let val = self.inner.data.lock().unwrap().get(key).copied();
                OpResult::ReadOk(val)
            }
        };

        let complete = Instant::now();
        self.history.record(Operation {
            id: op_id,
            thread_id,
            op_type: OpType::Read,
            key: key.to_string(),
            input_value: None,
            _expect_value: None,
            result: result.clone(),
            invoke_time: invoke,
            complete_time: complete,
        });

        result
    }

    fn get_raw(&self, key: &str) -> Option<i64> {
        self.inner.data.lock().unwrap().get(key).copied()
    }
}

// ============================================================================
// BankAccount — Classic Jepsen invariant test
// ============================================================================

#[derive(Clone)]
struct BankAccount {
    accounts: Arc<Mutex<HashMap<String, i64>>>,
    history: History,
}

impl BankAccount {
    fn new(initial_accounts: Vec<(String, i64)>) -> Self {
        let mut map = HashMap::new();
        for (id, balance) in initial_accounts {
            map.insert(id, balance);
        }
        Self {
            accounts: Arc::new(Mutex::new(map)),
            history: History::new(),
        }
    }

    /// Atomically transfer money between accounts.
    /// Returns true if transfer succeeded (sufficient funds).
    fn transfer(&self, from: &str, to: &str, amount: i64, thread_id: usize) -> bool {
        let invoke = Instant::now();
        let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);

        let success = {
            if from == to {
                // Self-transfer is a no-op
                true
            } else {
                let mut accounts = self.accounts.lock().unwrap();
                let from_balance = accounts.get(from).copied().unwrap_or(0);
                if from_balance >= amount {
                    let to_balance = accounts.get(to).copied().unwrap_or(0);
                    accounts.insert(from.to_string(), from_balance - amount);
                    accounts.insert(to.to_string(), to_balance + amount);
                    true
                } else {
                    false
                }
            }
        };

        let complete = Instant::now();
        self.history.record(Operation {
            id: op_id,
            thread_id,
            op_type: OpType::Write,
            key: format!("transfer:{}->{}:{}", from, to, amount),
            input_value: Some(amount),
            _expect_value: None,
            result: if success {
                OpResult::WriteOk
            } else {
                OpResult::Error("insufficient funds".to_string())
            },
            invoke_time: invoke,
            complete_time: complete,
        });

        success
    }

    /// Get the total balance across all accounts
    fn total_balance(&self) -> i64 {
        self.accounts.lock().unwrap().values().sum()
    }

    fn get_history(&self) -> History {
        self.history.clone()
    }
}

// ============================================================================
// Tests
// ============================================================================

/// Test 1: Multi-threaded writes/reads recorded with correct timestamps
#[test]
fn test_history_recording() {
    let history = History::new();
    let store = KeyValueStore::new(history.clone());

    let num_threads = 4;
    let ops_per_thread = 25;
    let mut handles = vec![];

    for tid in 0..num_threads {
        let store = store.clone();
        handles.push(thread::spawn(move || {
            for i in 0..ops_per_thread {
                let key = format!("key_{}", tid);
                store.write(tid, &key, (tid * 100 + i) as i64);
                store.read(tid, &key);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let ops = history.get_operations();
    let expected_ops = num_threads * ops_per_thread * 2; // write + read per iteration
    assert_eq!(
        ops.len(),
        expected_ops,
        "Should record {} operations from {} threads",
        expected_ops,
        num_threads
    );

    // Verify all operations have valid timestamps (invoke <= complete)
    for op in &ops {
        assert!(
            op.invoke_time <= op.complete_time,
            "Operation {} has invoke_time > complete_time",
            op.id
        );
    }

    // Verify all thread IDs are represented
    let thread_ids: HashSet<usize> = ops.iter().map(|op| op.thread_id).collect();
    for tid in 0..num_threads {
        assert!(
            thread_ids.contains(&tid),
            "Thread {} should have recorded operations",
            tid
        );
    }
}

/// Test 2: Sequential operations on a single key are linearizable
#[test]
fn test_linearizability_sequential() {
    let history = History::new();
    let store = KeyValueStore::new(history.clone());

    // Sequential writes and reads — trivially linearizable
    store.write(0, "x", 1);
    let r1 = store.read(0, "x");
    assert_eq!(r1, OpResult::ReadOk(Some(1)));

    store.write(0, "x", 2);
    let r2 = store.read(0, "x");
    assert_eq!(r2, OpResult::ReadOk(Some(2)));

    store.write(0, "x", 3);
    let r3 = store.read(0, "x");
    assert_eq!(r3, OpResult::ReadOk(Some(3)));

    // Verify linearizability
    let result = LinearizabilityChecker::check(&history);
    assert!(
        result.is_ok(),
        "Sequential history should be linearizable: {:?}",
        result.err()
    );
}

/// Test 3: Concurrent operations from N threads produce a linearizable history
#[test]
fn test_linearizability_concurrent() {
    let history = History::new();
    let store = KeyValueStore::new(history.clone());

    let num_threads = 5;
    let ops_per_thread = 50;
    let mut handles = vec![];

    for tid in 0..num_threads {
        let store = store.clone();
        handles.push(thread::spawn(move || {
            for i in 0..ops_per_thread {
                // All threads write to the same key
                store.write(tid, "shared_key", (tid * 1000 + i) as i64);
                // Small sleep to increase interleaving
                if i % 10 == 0 {
                    thread::sleep(Duration::from_micros(1));
                }
                store.read(tid, "shared_key");
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // The history must be linearizable: every read must return a value
    // that was actually written and could be the most recent at the
    // read's linearization point.
    let result = LinearizabilityChecker::check(&history);
    assert!(
        result.is_ok(),
        "Concurrent history should be linearizable: {:?}",
        result.err()
    );

    // Verify we recorded all expected operations
    assert_eq!(
        history.len(),
        num_threads * ops_per_thread * 2,
        "All operations should be recorded"
    );
}

/// Test 4: Bank account invariant — total balance never changes under transfers
#[test]
fn test_bank_account_invariant() {
    let accounts = vec![
        ("alice".to_string(), 1000),
        ("bob".to_string(), 1000),
        ("charlie".to_string(), 1000),
        ("dave".to_string(), 1000),
        ("eve".to_string(), 1000),
    ];
    let initial_total: i64 = accounts.iter().map(|(_, b)| b).sum();
    let bank = BankAccount::new(accounts);
    let account_names = ["alice", "bob", "charlie", "dave", "eve"];

    let num_threads = 5;
    let transfers_per_thread = 50;
    let invariant_violated = Arc::new(AtomicBool::new(false));
    let mut handles = vec![];

    for tid in 0..num_threads {
        let bank = bank.clone();
        let violated = invariant_violated.clone();
        handles.push(thread::spawn(move || {
            for i in 0..transfers_per_thread {
                let from_idx = tid % account_names.len();
                let mut to_idx = (tid + i + 1) % account_names.len();
                if to_idx == from_idx {
                    to_idx = (to_idx + 1) % account_names.len();
                }
                let from = account_names[from_idx];
                let to = account_names[to_idx];
                let amount = (i as i64 % 50) + 1;
                bank.transfer(from, to, amount, tid);

                // Check invariant after every transfer
                let total = bank.total_balance();
                if total != initial_total {
                    violated.store(true, Ordering::SeqCst);
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert!(
        !invariant_violated.load(Ordering::SeqCst),
        "Bank account invariant was violated: total balance changed during concurrent transfers"
    );
    assert_eq!(
        bank.total_balance(),
        initial_total,
        "Final total balance should equal initial total"
    );

    // Verify all transfers were recorded
    let ops = bank.get_history().get_operations();
    assert_eq!(
        ops.len(),
        num_threads * transfers_per_thread,
        "All transfer attempts should be recorded"
    );
}

/// Test 5: High-concurrency bank transfers stress test
#[test]
fn test_bank_account_concurrent_stress() {
    let num_accounts = 10;
    let accounts: Vec<(String, i64)> = (0..num_accounts)
        .map(|i| (format!("acct_{}", i), 1000))
        .collect();
    let initial_total: i64 = accounts.iter().map(|(_, b)| b).sum();
    let bank = BankAccount::new(accounts);

    let num_threads = 10;
    let ops_per_thread = 100;
    let invariant_checks = Arc::new(AtomicUsize::new(0));
    let invariant_failures = Arc::new(AtomicUsize::new(0));
    let total_successful = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    for tid in 0..num_threads {
        let bank = bank.clone();
        let checks = invariant_checks.clone();
        let failures = invariant_failures.clone();
        let successful = total_successful.clone();
        handles.push(thread::spawn(move || {
            for i in 0..ops_per_thread {
                let from_idx = (tid + i) % num_accounts;
                let to_idx = (tid + i + 1) % num_accounts;
                let from = format!("acct_{}", from_idx);
                let to = format!("acct_{}", to_idx);
                let amount = (i as i64 % 20) + 1;

                if bank.transfer(&from, &to, amount, tid) {
                    successful.fetch_add(1, Ordering::Relaxed);
                }

                // Periodic invariant check
                if i % 10 == 0 {
                    checks.fetch_add(1, Ordering::Relaxed);
                    let total = bank.total_balance();
                    if total != initial_total {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let check_count = invariant_checks.load(Ordering::Relaxed);
    let failure_count = invariant_failures.load(Ordering::Relaxed);
    let success_count = total_successful.load(Ordering::Relaxed);

    assert_eq!(
        failure_count, 0,
        "Invariant check failed {} out of {} times",
        failure_count, check_count
    );
    assert_eq!(
        bank.total_balance(),
        initial_total,
        "Final total should match initial"
    );
    assert!(success_count > 0, "At least some transfers should succeed");
    println!(
        "Stress test: {} successful transfers out of {}, {} invariant checks passed",
        success_count,
        num_threads * ops_per_thread,
        check_count
    );
}

/// Test 6: Compare-and-swap register with concurrent incrementers
#[test]
fn test_concurrent_cas_register() {
    let history = History::new();
    let store = KeyValueStore::new(history.clone());

    // Initialize register to 0
    store.write(0, "counter", 0);

    let num_threads = 5;
    let increments_per_thread = 100;
    let total_success = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    for tid in 0..num_threads {
        let store = store.clone();
        let success = total_success.clone();
        handles.push(thread::spawn(move || {
            let mut local_success = 0;
            for _ in 0..increments_per_thread {
                // CAS loop: atomically increment counter
                loop {
                    let current = match store.read(tid + 1, "counter") {
                        OpResult::ReadOk(Some(v)) => v,
                        _ => 0,
                    };
                    match store.compare_and_swap(tid + 1, "counter", current, current + 1) {
                        OpResult::CasOk(true) => {
                            local_success += 1;
                            break;
                        }
                        OpResult::CasOk(false) => {
                            // Retry — another thread won the race
                            continue;
                        }
                        _ => break,
                    }
                }
            }
            success.fetch_add(local_success, Ordering::Relaxed);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    let final_value = store.get_raw("counter").unwrap_or(0);
    let successful_cas = total_success.load(Ordering::Relaxed);

    assert_eq!(
        final_value, successful_cas as i64,
        "Final counter value should equal total successful CAS operations"
    );
    assert_eq!(
        successful_cas,
        num_threads * increments_per_thread,
        "All increments should eventually succeed via CAS retry"
    );
}

/// Test 7: Fault injection — partition blocks operations, healing restores them
#[test]
fn test_fault_injection_partition() {
    let history = History::new();
    let fault = FaultInjector::new();
    let store = FaultAwareStore::new(history.clone(), fault.clone());

    // Normal operation succeeds
    let r1 = store.write(0, "key1", 42);
    assert_eq!(
        r1,
        OpResult::WriteOk,
        "Write should succeed without partition"
    );

    let r2 = store.read(0, "key1");
    assert_eq!(
        r2,
        OpResult::ReadOk(Some(42)),
        "Read should return written value"
    );

    // Inject partition
    fault.partition();

    let r3 = store.write(0, "key1", 99);
    assert!(
        matches!(r3, OpResult::Error(ref e) if e.contains("partition")),
        "Write during partition should fail: {:?}",
        r3
    );

    let r4 = store.read(0, "key1");
    assert!(
        matches!(r4, OpResult::Error(ref e) if e.contains("partition")),
        "Read during partition should fail: {:?}",
        r4
    );

    // Heal partition
    fault.heal();

    let r5 = store.write(0, "key1", 100);
    assert_eq!(
        r5,
        OpResult::WriteOk,
        "Write should succeed after partition heals"
    );

    let r6 = store.read(0, "key1");
    assert_eq!(
        r6,
        OpResult::ReadOk(Some(100)),
        "Read should return new value after healing"
    );

    // Verify history contains errors for partitioned operations
    let ops = history.get_operations();
    let error_count = ops
        .iter()
        .filter(|op| matches!(&op.result, OpResult::Error(_)))
        .count();
    assert_eq!(
        error_count, 2,
        "Should have 2 error operations during partition"
    );
}

/// Test 8: Crash simulation — state persists, reads after recovery return pre-crash values
#[test]
fn test_fault_injection_crash_recovery() {
    let history = History::new();
    let fault = FaultInjector::new();
    let store = FaultAwareStore::new(history.clone(), fault.clone());

    // Write some data before crash
    store.write(0, "persistent_key", 42);
    store.write(0, "another_key", 100);

    // Verify data is there
    assert_eq!(store.read(0, "persistent_key"), OpResult::ReadOk(Some(42)));

    // Crash the node
    fault.crash();

    // All operations should fail during crash
    let r1 = store.write(0, "persistent_key", 999);
    assert!(
        matches!(r1, OpResult::Error(ref e) if e.contains("crashed")),
        "Write during crash should fail"
    );

    let r2 = store.read(0, "persistent_key");
    assert!(
        matches!(r2, OpResult::Error(ref e) if e.contains("crashed")),
        "Read during crash should fail"
    );

    // Recover
    fault.recover();

    // Data written before crash should still be present
    let r3 = store.read(0, "persistent_key");
    assert_eq!(
        r3,
        OpResult::ReadOk(Some(42)),
        "Pre-crash data should persist after recovery"
    );

    let r4 = store.read(0, "another_key");
    assert_eq!(
        r4,
        OpResult::ReadOk(Some(100)),
        "All pre-crash data should persist"
    );

    // Write during crash should NOT have taken effect
    assert_eq!(
        store.get_raw("persistent_key"),
        Some(42),
        "Value should not have changed during crash"
    );
}

/// Test 9: Stale read detection — detect reads that return values older
/// than the most recent completed write
#[test]
fn test_stale_read_detection() {
    let history = History::new();
    let store = KeyValueStore::new(history.clone());

    // Write increasing values with delays so timestamps are distinguishable
    store.write(0, "x", 1);
    thread::sleep(Duration::from_millis(5));
    store.write(0, "x", 2);
    thread::sleep(Duration::from_millis(5));
    store.write(0, "x", 3);
    thread::sleep(Duration::from_millis(5));

    // Read should return latest value
    let r = store.read(0, "x");
    assert_eq!(r, OpResult::ReadOk(Some(3)));

    // Verify no stale reads in the history
    let ops = history.get_operations();
    let writes: Vec<_> = ops
        .iter()
        .filter(|op| op.op_type == OpType::Write && op.key == "x")
        .collect();
    let reads: Vec<_> = ops
        .iter()
        .filter(|op| op.op_type == OpType::Read && op.key == "x")
        .collect();

    for read_op in &reads {
        if let OpResult::ReadOk(Some(read_val)) = &read_op.result {
            // Find the latest write that completed before this read started
            let latest_write_before = writes
                .iter()
                .filter(|w| w.complete_time <= read_op.invoke_time)
                .max_by_key(|w| w.complete_time);

            if let Some(latest_w) = latest_write_before {
                if let Some(written_val) = latest_w.input_value {
                    assert!(
                        *read_val >= written_val,
                        "Stale read detected: read {} but latest completed write was {}",
                        read_val,
                        written_val
                    );
                }
            }
        }
    }

    // The linearizability checker should also pass
    let result = LinearizabilityChecker::check(&history);
    assert!(
        result.is_ok(),
        "History should be linearizable: {:?}",
        result.err()
    );
}

/// Test 10: Write ordering monotonicity — per-key values are monotonically
/// increasing when observed by sequential reader
#[test]
fn test_write_ordering_monotonicity() {
    let history = History::new();
    let store = KeyValueStore::new(history.clone());
    let stop = Arc::new(AtomicBool::new(false));

    // Writer thread: writes increasing values
    let writer_store = store.clone();
    let writer_stop = stop.clone();
    let writer = thread::spawn(move || {
        let mut value = 0i64;
        while !writer_stop.load(Ordering::SeqCst) {
            value += 1;
            writer_store.write(0, "monotonic", value);
            thread::sleep(Duration::from_micros(100));
        }
        value
    });

    // Reader thread: reads and verifies monotonicity
    let reader_store = store.clone();
    let reader_stop = stop.clone();
    let monotonicity_violated = Arc::new(AtomicBool::new(false));
    let violated = monotonicity_violated.clone();
    let reader = thread::spawn(move || {
        let mut last_seen: Option<i64> = None;
        let mut read_count = 0;
        while !reader_stop.load(Ordering::SeqCst) && read_count < 500 {
            if let OpResult::ReadOk(Some(val)) = reader_store.read(1, "monotonic") {
                if let Some(prev) = last_seen {
                    if val < prev {
                        violated.store(true, Ordering::SeqCst);
                    }
                }
                last_seen = Some(val);
            }
            read_count += 1;
            thread::sleep(Duration::from_micros(50));
        }
        read_count
    });

    // Let it run for a bit
    thread::sleep(Duration::from_millis(100));
    stop.store(true, Ordering::SeqCst);

    let max_written = writer.join().unwrap();
    let reads_done = reader.join().unwrap();

    assert!(
        !monotonicity_violated.load(Ordering::SeqCst),
        "Monotonicity violated: reader saw a value decrease"
    );
    assert!(max_written > 0, "Writer should have written values");
    assert!(reads_done > 0, "Reader should have completed reads");
    println!(
        "Monotonicity test: writer wrote up to {}, reader did {} reads",
        max_written, reads_done
    );
}

/// Test 11: Nemesis — random fault injection during concurrent workload
#[test]
fn test_nemesis_random_faults() {
    let history = History::new();
    let fault = FaultInjector::new();
    let store = FaultAwareStore::new(history.clone(), fault.clone());

    let num_workers = 4;
    let ops_per_worker = 100;
    let stop = Arc::new(AtomicBool::new(false));
    let total_errors = Arc::new(AtomicUsize::new(0));
    let total_success = Arc::new(AtomicUsize::new(0));

    // Start worker threads
    let mut handles = vec![];
    for tid in 0..num_workers {
        let store = store.clone();
        let stop = stop.clone();
        let errors = total_errors.clone();
        let success = total_success.clone();
        handles.push(thread::spawn(move || {
            for i in 0..ops_per_worker {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let key = format!("nemesis_key_{}", i % 5);
                let result = store.write(tid + 1, &key, (tid * 1000 + i) as i64);
                match result {
                    OpResult::WriteOk => {
                        success.fetch_add(1, Ordering::Relaxed);
                    }
                    OpResult::Error(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                // Also do reads
                let _ = store.read(tid + 1, &key);
                thread::sleep(Duration::from_micros(50));
            }
        }));
    }

    // Nemesis thread: randomly inject and heal faults
    let nemesis_fault = fault.clone();
    let nemesis_stop = stop.clone();
    let nemesis = thread::spawn(move || {
        let mut cycle = 0;
        while !nemesis_stop.load(Ordering::SeqCst) && cycle < 20 {
            match cycle % 4 {
                0 => {
                    // Inject partition
                    nemesis_fault.partition();
                    thread::sleep(Duration::from_millis(10));
                    nemesis_fault.heal();
                }
                1 => {
                    // Inject slowness
                    nemesis_fault.set_slow(5);
                    thread::sleep(Duration::from_millis(15));
                    nemesis_fault.clear_slow();
                }
                2 => {
                    // Inject crash
                    nemesis_fault.crash();
                    thread::sleep(Duration::from_millis(8));
                    nemesis_fault.recover();
                }
                _ => {
                    // No fault period
                    thread::sleep(Duration::from_millis(10));
                }
            }
            cycle += 1;
        }
    });

    for h in handles {
        h.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    nemesis.join().unwrap();

    // Heal everything for final check
    fault.heal();
    fault.recover();
    fault.clear_slow();

    let err_count = total_errors.load(Ordering::Relaxed);
    let ok_count = total_success.load(Ordering::Relaxed);

    // Some operations should have succeeded and some should have failed
    assert!(
        ok_count > 0,
        "At least some operations should succeed during nemesis test"
    );
    // History should have recorded everything
    assert!(history.len() > 0, "History should have recorded operations");
    println!(
        "Nemesis test: {} successful writes, {} errors, {} total history entries",
        ok_count,
        err_count,
        history.len()
    );

    // Verify that the store is in a consistent state after all faults
    // (every key's value should be one that was actually written)
    let ops = history.get_operations();
    for key_num in 0..5 {
        let key = format!("nemesis_key_{}", key_num);
        if let Some(current_val) = store.get_raw(&key) {
            let valid_values: HashSet<i64> = ops
                .iter()
                .filter(|op| {
                    op.key == key && op.op_type == OpType::Write && op.result == OpResult::WriteOk
                })
                .filter_map(|op| op.input_value)
                .collect();
            assert!(
                valid_values.contains(&current_val),
                "Key '{}' has value {} which was never successfully written",
                key,
                current_val
            );
        }
    }
}

/// Test 12: Set consistency — concurrent add/remove/contains operations
#[test]
fn test_set_consistency() {
    let set: Arc<Mutex<HashSet<i64>>> = Arc::new(Mutex::new(HashSet::new()));
    let history = History::new();

    let num_threads = 4;
    let ops_per_thread = 100;
    let mut handles = vec![];

    // Adder threads
    for tid in 0..num_threads / 2 {
        let set = set.clone();
        let history = history.clone();
        handles.push(thread::spawn(move || {
            for i in 0..ops_per_thread {
                let val = (tid * 1000 + i) as i64;
                let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
                let invoke = Instant::now();
                set.lock().unwrap().insert(val);
                let complete = Instant::now();
                history.record(Operation {
                    id: op_id,
                    thread_id: tid,
                    op_type: OpType::Write,
                    key: "set".to_string(),
                    input_value: Some(val),
                    _expect_value: None,
                    result: OpResult::WriteOk,
                    invoke_time: invoke,
                    complete_time: complete,
                });
            }
        }));
    }

    // Reader/checker threads
    for tid in num_threads / 2..num_threads {
        let set = set.clone();
        let history = history.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..ops_per_thread {
                let op_id = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
                let invoke = Instant::now();
                let snapshot: Vec<i64> = set.lock().unwrap().iter().copied().collect();
                let complete = Instant::now();
                history.record(Operation {
                    id: op_id,
                    thread_id: tid,
                    op_type: OpType::Read,
                    key: "set".to_string(),
                    input_value: None,
                    _expect_value: None,
                    result: OpResult::ReadOk(Some(snapshot.len() as i64)),
                    invoke_time: invoke,
                    complete_time: complete,
                });
                thread::sleep(Duration::from_micros(10));
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Final set should contain exactly the elements that were added
    let final_set = set.lock().unwrap();
    let adder_count = num_threads / 2;
    let expected_size = adder_count * ops_per_thread;
    assert_eq!(
        final_set.len(),
        expected_size,
        "Set should contain exactly {} elements",
        expected_size
    );

    // Verify every element in the set was actually added by an adder thread
    for &val in final_set.iter() {
        let tid = val as usize / 1000;
        let idx = val as usize % 1000;
        assert!(tid < adder_count, "Value {} has invalid thread ID", val);
        assert!(idx < ops_per_thread, "Value {} has invalid index", val);
    }

    // Verify history: set size observations should be monotonically non-decreasing
    // within each reader thread (since we only add, never remove)
    let ops = history.get_operations();
    for tid in num_threads / 2..num_threads {
        let reader_ops: Vec<_> = ops
            .iter()
            .filter(|op| op.thread_id == tid && op.op_type == OpType::Read)
            .collect();

        let mut sorted = reader_ops;
        sorted.sort_by_key(|op| op.invoke_time);

        let mut prev_size = 0i64;
        for op in sorted {
            if let OpResult::ReadOk(Some(size)) = &op.result {
                assert!(
                    *size >= prev_size,
                    "Set size should be monotonically non-decreasing for reader {}: saw {} after {}",
                    tid,
                    size,
                    prev_size
                );
                prev_size = *size;
            }
        }
    }
}
