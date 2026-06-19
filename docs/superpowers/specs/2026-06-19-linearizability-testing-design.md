# Linearizability Testing Framework Design

## Problem

The project has Jepsen-style tests (`jepsen_style_tests.rs`) that verify linearizability on an in-memory key-value store, and Docker chaos tests that verify "system works/doesn't work" after failures. These two are not connected — there is no verification that the actual distributed file system maintains linearizability under real network faults and node crashes.

The recent 2PC recovery fix (PR #50) needs proof that cross-shard renames are atomic under coordinator/participant failures. The existing chaos tests only check "file exists at destination" — they don't verify that concurrent operations see a consistent view.

## Design

### Components

| Component | Form | Responsibility |
|---|---|---|
| **History Recorder** | `dfs_cli workload` subcommand | Run concurrent file operations (put, get, rename, delete) against real cluster, record operation history to JSONL file |
| **Linearizability Checker** | `dfs_cli check-history` subcommand | Read JSONL history, verify all operations are linearizable under Register + Rename model |
| **Test Orchestrator** | `test_scripts/linearizability_test.sh` | Docker compose + Toxiproxy cluster management, workload execution, fault injection, checker invocation |

### Operation Model (Register + Rename)

Each file path is an independent register. The register holds a value (data hash) or is empty (file doesn't exist).

```
Operations:
  put(path, data)     → ok | error
  get(path)           → data_hash | not_found | error
  delete(path)        → ok | error
  rename(src, dst)    → ok | error

Atomicity rules:
  put(path, data):    register[path] = hash(data)
  get(path):          return register[path] or not_found
  delete(path):       register[path] = empty
  rename(src, dst):   atomic { register[dst] = register[src]; register[src] = empty }
```

Linearizability means: for every completed operation, there exists a point between its invocation and return where the operation takes effect atomically. All operations' effects are consistent with some total order that respects these linearization points.

### History Format (JSONL)

Each operation produces two lines — invoke and return:

```jsonl
{"id":1,"client":0,"type":"invoke","op":"put","path":"/a/file1","data_hash":"abc123","ts_ns":1234567890}
{"id":1,"client":0,"type":"return","op":"put","path":"/a/file1","result":"ok","ts_ns":1234567900}
{"id":2,"client":1,"type":"invoke","op":"get","path":"/a/file1","ts_ns":1234567895}
{"id":2,"client":1,"type":"return","op":"get","path":"/a/file1","result":"ok","data_hash":"abc123","ts_ns":1234567910}
{"id":3,"client":2,"type":"invoke","op":"rename","src":"/a/file1","dst":"/z/file1","ts_ns":1234567905}
{"id":3,"client":2,"type":"return","op":"rename","src":"/a/file1","dst":"/z/file1","result":"ok","ts_ns":1234567930}
```

Fields:
- `id`: unique operation ID (monotonic per client)
- `client`: client task index (unique within the process)
- `type`: "invoke" or "return"
- `op`: "put", "get", "delete", "rename"
- `path`: target file path (for put/get/delete)
- `src`, `dst`: source and destination paths (for rename)
- `data_hash`: MD5 hash of file content (for put invoke and get return)
- `result`: "ok", "not_found", or "error"
- `ts_ns`: nanosecond wall-clock timestamp (`SystemTime::now()`)

**Timestamp design**: All workload tasks run in a single process, so they share the same `SystemTime` clock. Wall-clock is used instead of `Instant` because the checker needs comparable timestamps. Within a single host, wall-clock skew between tasks in the same process is zero.

For operations that return error, the operation is treated as "maybe happened" — the checker must consider both the applied and not-applied cases (see Step 5 below for limits).

### Checker Algorithm

Based on WGL (Wing-Gong Linearizability), extended for multi-register rename.

**Step 1: Parse history** — group invoke/return pairs by operation ID. Mark operations as completed (have return) or crashed (invoke only, no return).

**Step 2: Build operation intervals** — each operation has [invoke_ts, return_ts]. Crashed operations have [invoke_ts, +infinity] (could have taken effect at any point after invoke).

**Step 3: Identify register groups** — partition all registers (paths) into two sets:
- **Simple registers**: paths only touched by put/get/delete (no rename involvement)
- **Rename-linked registers**: all paths that appear as `src` or `dst` in any rename operation, plus all operations on those paths

Simple registers can be verified independently per-key (fast). Rename-linked registers must be verified together in a single linearization search.

**Step 4: Per-key verification (simple registers)** — for each simple register, verify linearizability using the existing WGL approach from `jepsen_style_tests.rs`:
- Track committed writes as (invoke_ts, return_ts, value) intervals
- For each read, compute possible values at any point in the read's interval
- Verify actual read result is among possible values

**Step 5: Multi-register verification (rename-linked registers)** — this is the core extension. Rename is modeled as producing two writes that share a single linearization point:

1. Collect all operations on rename-linked registers
2. Sort operations by invoke timestamp
3. Build a search tree: try placing each operation's linearization point within its interval, maintaining a consistent state for all affected registers
4. For rename operations: the linearization point is shared — at that instant, `src` becomes empty and `dst` gets `src`'s previous value. Both effects are atomic.
5. For reads on rename-linked registers: the read value must match the register state at the read's linearization point, considering all prior writes and renames

**Search strategy**: The full problem is NP-hard. For practical histories (< 500 operations on rename-linked registers), use backtracking search with pruning:
- Order candidate linearization points by midpoint of interval (heuristic)
- Prune branches where a read value is inconsistent with current state
- If search space exceeds 10M nodes, fall back to timestamp-ordered greedy linearization and report "inconclusive" instead of "pass" (never false-pass, may false-fail)

**Step 6: Error handling** — operations that returned "error" are ambiguous (may or may not have taken effect). For each error operation, the checker tries both possibilities. To prevent combinatorial explosion: if more than 15 error operations exist in a single register group, the checker reports "too many ambiguous operations, scenario inconclusive" and the test script treats this as a non-blocking warning (not a failure).

### Checker Self-Test

The checker binary includes a `--self-test` flag that runs built-in validation:

1. **Known-good history**: a small hand-crafted history (5 put, 3 get, 1 rename) that is linearizable. Checker must return pass.
2. **Known-bad history (value lost)**: rename completes, concurrent get(src) and get(dst) both return not_found. Checker must return violation.
3. **Known-bad history (value duplicated)**: rename completes, get(src) returns data AND get(dst) returns data. Checker must return violation.
4. **Known-bad history (stale read)**: put(path, v2) completes, then get(path) returns v1. Checker must return violation.

The test orchestrator runs `dfs_cli check-history --self-test` before any scenario to validate the checker itself.

### Workload Generator (`dfs_cli workload`)

```
dfs_cli workload \
  --config-servers http://config-server:50050 \
  --ops 50 \
  --clients 5 \
  --key-space 4 \
  --rename-ratio 0.3 \
  --history /tmp/history.jsonl
```

Parameters:
- `--ops`: number of operations per client task
- `--clients`: number of concurrent tokio tasks within this process (default: 5)
- `--key-space`: number of distinct file paths (default: 4, intentionally small to force contention)
- `--rename-ratio`: fraction of operations that are rename (rest are evenly split among put/get/delete)
- `--history`: output JSONL file path

**Contention design**: With 5 concurrent tasks operating on 4 paths with 30% rename ratio, the expected number of concurrent renames targeting the same file is high. Each task runs 50 ops, so 250 ops total with ~75 renames over 4 keys. At any given moment, 2-3 renames are likely in-flight, and with only 4 keys, conflicts are frequent.

The workload generator:
1. Creates small files (< 1KB, content = random bytes, tracked by MD5 hash)
2. Randomly selects operations weighted by rename-ratio
3. Records invoke/return timestamps using `SystemTime::now()` (wall-clock, comparable across tasks)
4. Writes JSONL to a shared file protected by `tokio::sync::Mutex` for serialized writes
5. On error, records result="error" and continues

Path selection strategy: paths are drawn from `/a/lin_0`, `/a/lin_1` (shard 1, paths < /m) and `/z/lin_0`, `/z/lin_1` (shard 2, paths >= /m) to ensure cross-shard renames occur naturally.

### Fault Injection Scenarios

| # | Scenario | Fault injection method | Duration | Target |
|---|----------|----------------------|----------|--------|
| 1 | No fault (baseline) | None | N/A | Baseline linearizability |
| 2 | Coordinator kill | `docker kill master1-shard1`, restart after 30s | 30s | 2PC recovery |
| 3 | Participant kill | `docker kill master1-shard2`, restart after 30s | 30s | Inquiry + Presumed Abort |
| 4 | Network partition (cross-shard) | Toxiproxy timeout on master1-shard1 proxy, heal after 30s | 30s | Partition tolerance |
| 5 | ChunkServer kill | `docker kill chunkserver1-shard1`, restart after 15s | 15s | Write integrity |
| 6 | Master leader failover | `docker stop master1-shard1` (graceful), Raft re-election, restart | 20s | Leader transfer |
| 7 | Delayed commit RPC | Toxiproxy 10s latency on master1-shard2, then kill master1-shard1 | 15s | Commit-before-delete window |

Scenario 7 specifically targets the bug that PR #50 fixed: coordinator sends commit RPC, RPC is delayed, coordinator is killed before source deletion. On restart, the recovery task must complete the transaction.

Each scenario:
- Single workload process with 5 concurrent tasks
- 50 operations per task, 30% rename ratio, 4 key paths
- Fault injected 5 seconds after workload starts
- Fault healed after specified duration
- Checker runs after workload completes

### Test Orchestrator (`linearizability_test.sh`)

```bash
#!/bin/bash
set -e

COMPOSE_FILE="docker-compose.toxiproxy-sharded.yml"

cleanup() {
    docker compose -f $COMPOSE_FILE down -v 2>/dev/null || true
}
trap cleanup EXIT

docker compose -f $COMPOSE_FILE up -d --no-build
sleep 20

# Self-test the checker first
echo "=== Checker self-test ==="
docker exec dfs-chunkserver-extra /app/dfs_cli check-history --self-test
echo "=== Checker self-test PASSED ==="

SCENARIOS=("baseline" "coordinator_kill" "participant_kill" "network_partition" "chunkserver_kill" "leader_failover" "delayed_commit")

for scenario in "${SCENARIOS[@]}"; do
    echo "=== Scenario: $scenario ==="
    
    # Cleanup test files from previous scenario
    cleanup_test_files
    
    # Run workload from a neutral container (not one that gets killed)
    docker exec dfs-chunkserver-extra /app/dfs_cli workload \
        --config-servers http://config-server:50050 \
        --ops 50 --clients 5 --key-space 4 --rename-ratio 0.3 \
        --history /tmp/hist_${scenario}.jsonl &
    WORKLOAD_PID=$!
    
    sleep 5  # Let workload run normally first
    
    # Inject fault (scenario-specific function)
    inject_fault_${scenario}
    
    # Wait for fault duration
    sleep_for_scenario_${scenario}
    
    # Heal fault
    heal_fault_${scenario}
    
    # Wait for workload to finish
    wait $WORKLOAD_PID || true
    
    # Run checker
    docker exec dfs-chunkserver-extra /app/dfs_cli check-history /tmp/hist_${scenario}.jsonl
    
    echo "=== $scenario: PASSED ==="
done

echo "All linearizability tests passed!"
```

**Key design decision**: workload runs from `dfs-chunkserver-extra`, a container that is never killed in any scenario. This avoids the problem of the workload process dying with the container under test.

**Cleanup between scenarios**: `cleanup_test_files` deletes all `/a/lin_*` and `/z/lin_*` files via `dfs_cli` and waits for confirmation. This ensures each scenario starts from a clean state.

### File Changes

| File | Action | Content |
|---|---|---|
| `dfs/client/src/bin/dfs_cli.rs` | Modify | Add `Workload` and `CheckHistory` subcommands |
| `dfs/client/src/workload.rs` | Create | Workload generator: concurrent tokio tasks, JSONL history recording with SystemTime |
| `dfs/client/src/checker.rs` | Create | Linearizability checker: JSONL parser, WGL for simple registers, multi-register search for rename, self-test suite |
| `dfs/client/src/lib.rs` or `mod.rs` | Modify | Add `pub mod workload; pub mod checker;` |
| `test_scripts/linearizability_test.sh` | Create | Test orchestrator with 7 fault scenarios |

### Success Criteria

1. Checker self-test passes (known-good passes, known-bad detected)
2. Baseline scenario passes linearizability check
3. All fault scenarios pass (or report "inconclusive" for high-error-rate scenarios, which is acceptable)
4. If a scenario fails, the checker outputs the specific violation: which operations are inconsistent, what values were observed, and what values were possible
5. Test completes in < 15 minutes total (all 7 scenarios)

### Out of Scope

- Full filesystem model (ls, mkdir) — Register + Rename is sufficient
- Byzantine faults (corrupted messages) — crash-stop model only
- Multi-key transactions beyond rename — only rename is a cross-shard operation
- Formal TLA+ specification — empirical testing, not formal verification
- Performance benchmarking — this is correctness testing only
- Porcupine/Knossos integration — custom checker is simpler and handles our rename model directly
