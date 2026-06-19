//! Concurrent workload generator for linearizability testing.
//!
//! Spawns multiple tokio tasks that perform random file operations
//! (put, get, delete, rename) and records every invoke/return to a
//! JSONL history file that can later be checked by `checker`.

use crate::checker::HistoryEntry;
use crate::Client;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::Mutex;

/// Configuration for a workload run.
pub struct WorkloadConfig {
    /// Number of operations each client performs.
    pub ops_per_client: usize,
    /// Number of concurrent client tasks.
    pub num_clients: usize,
    /// Number of distinct keys (files) in the key space.
    pub key_space: usize,
    /// Fraction of operations that are renames (0.0 to 1.0).
    pub rename_ratio: f64,
    /// Path to the output JSONL history file.
    pub history_path: String,
}

/// Global monotonic operation-ID counter.
static OP_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// Generate a file path for the given key index.
/// Even indices go to shard-1 prefix `/a/`, odd indices to shard-2 prefix `/z/`.
fn key_path(index: usize) -> String {
    if index.is_multiple_of(2) {
        format!("/a/lin_{}", index)
    } else {
        format!("/z/lin_{}", index)
    }
}

/// Run the concurrent workload and write history to `config.history_path`.
pub async fn run_workload(client: Arc<Client>, config: WorkloadConfig) -> anyhow::Result<()> {
    let file = std::fs::File::create(&config.history_path)?;
    let writer = Arc::new(Mutex::new(std::io::BufWriter::new(file)));

    let mut handles = Vec::new();

    for client_id in 0..config.num_clients {
        let client = Arc::clone(&client);
        let writer = Arc::clone(&writer);
        let ops = config.ops_per_client;
        let key_space = config.key_space;
        let rename_ratio = config.rename_ratio;
        let client_name = format!("c{}", client_id);

        handles.push(tokio::spawn(async move {
            let mut rng = StdRng::from_entropy();
            for _ in 0..ops {
                let is_rename = rng.gen::<f64>() < rename_ratio;
                if is_rename {
                    do_rename(&client, &writer, &client_name, key_space, &mut rng).await;
                } else {
                    // Split remaining ops evenly among put, get, delete
                    let choice: u8 = rng.gen_range(0..3);
                    match choice {
                        0 => do_put(&client, &writer, &client_name, key_space, &mut rng).await,
                        1 => do_get(&client, &writer, &client_name, key_space, &mut rng).await,
                        _ => do_delete(&client, &writer, &client_name, key_space, &mut rng).await,
                    }
                }
            }
        }));
    }

    for h in handles {
        h.await?;
    }

    // Final flush
    let mut w = writer.lock().await;
    w.flush()?;

    Ok(())
}

async fn write_entry(writer: &Mutex<std::io::BufWriter<std::fs::File>>, entry: &HistoryEntry) {
    let line = serde_json::to_string(entry).expect("serialize HistoryEntry");
    let mut w = writer.lock().await;
    let _ = writeln!(w, "{}", line);
    let _ = w.flush();
}

async fn do_put(
    client: &Client,
    writer: &Mutex<std::io::BufWriter<std::fs::File>>,
    client_name: &str,
    key_space: usize,
    rng: &mut impl Rng,
) {
    let idx = rng.gen_range(0..key_space);
    let path = key_path(idx);
    let op_id = OP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let data = format!("data_{}_{}", op_id, now_ns());
    let data_hash = format!("{:x}", md5::compute(data.as_bytes()));

    // Invoke
    let invoke = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "invoke".to_string(),
        op: "put".to_string(),
        path: path.clone(),
        src: String::new(),
        dst: String::new(),
        data_hash: data_hash.clone(),
        result: String::new(),
        ts_ns: now_ns(),
    };
    write_entry(writer, &invoke).await;

    // Execute
    let result = client
        .create_file_from_buffer(data.into_bytes(), &path)
        .await;

    // Return
    let ret = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "return".to_string(),
        op: "put".to_string(),
        path: path.clone(),
        src: String::new(),
        dst: String::new(),
        data_hash: data_hash.clone(),
        result: match &result {
            Ok(()) => format!("put_ok:{}", data_hash),
            Err(_) => "error".to_string(),
        },
        ts_ns: now_ns(),
    };
    write_entry(writer, &ret).await;
}

async fn do_get(
    client: &Client,
    writer: &Mutex<std::io::BufWriter<std::fs::File>>,
    client_name: &str,
    key_space: usize,
    rng: &mut impl Rng,
) {
    let idx = rng.gen_range(0..key_space);
    let path = key_path(idx);
    let op_id = OP_COUNTER.fetch_add(1, Ordering::Relaxed);

    let invoke = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "invoke".to_string(),
        op: "get".to_string(),
        path: path.clone(),
        src: String::new(),
        dst: String::new(),
        data_hash: String::new(),
        result: String::new(),
        ts_ns: now_ns(),
    };
    write_entry(writer, &invoke).await;

    let result = client.get_file_content(&path).await;

    let ret = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "return".to_string(),
        op: "get".to_string(),
        path: path.clone(),
        src: String::new(),
        dst: String::new(),
        data_hash: String::new(),
        result: match &result {
            Ok(data) => {
                let hash = format!("{:x}", md5::compute(data));
                format!("get_ok:{}", hash)
            }
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("not found")
                    || msg.contains("Not found")
                    || msg.contains("File not found")
                {
                    "not_found".to_string()
                } else {
                    "error".to_string()
                }
            }
        },
        ts_ns: now_ns(),
    };
    write_entry(writer, &ret).await;
}

async fn do_delete(
    client: &Client,
    writer: &Mutex<std::io::BufWriter<std::fs::File>>,
    client_name: &str,
    key_space: usize,
    rng: &mut impl Rng,
) {
    let idx = rng.gen_range(0..key_space);
    let path = key_path(idx);
    let op_id = OP_COUNTER.fetch_add(1, Ordering::Relaxed);

    let invoke = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "invoke".to_string(),
        op: "delete".to_string(),
        path: path.clone(),
        src: String::new(),
        dst: String::new(),
        data_hash: String::new(),
        result: String::new(),
        ts_ns: now_ns(),
    };
    write_entry(writer, &invoke).await;

    let result = client.delete_file(&path).await;

    let ret = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "return".to_string(),
        op: "delete".to_string(),
        path: path.clone(),
        src: String::new(),
        dst: String::new(),
        data_hash: String::new(),
        result: match &result {
            Ok(()) => "ok".to_string(),
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("not found") || msg.contains("Not found") {
                    "not_found".to_string()
                } else {
                    "error".to_string()
                }
            }
        },
        ts_ns: now_ns(),
    };
    write_entry(writer, &ret).await;
}

async fn do_rename(
    client: &Client,
    writer: &Mutex<std::io::BufWriter<std::fs::File>>,
    client_name: &str,
    key_space: usize,
    rng: &mut impl Rng,
) {
    let src_idx = rng.gen_range(0..key_space);
    let mut dst_idx = rng.gen_range(0..key_space);
    if dst_idx == src_idx {
        dst_idx = (src_idx + 1) % key_space;
    }
    let src = key_path(src_idx);
    let dst = key_path(dst_idx);
    let op_id = OP_COUNTER.fetch_add(1, Ordering::Relaxed);

    let invoke = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "invoke".to_string(),
        op: "rename".to_string(),
        path: String::new(),
        src: src.clone(),
        dst: dst.clone(),
        data_hash: String::new(),
        result: String::new(),
        ts_ns: now_ns(),
    };
    write_entry(writer, &invoke).await;

    let result = client.rename_file(&src, &dst).await;

    let ret = HistoryEntry {
        id: op_id,
        client: client_name.to_string(),
        entry_type: "return".to_string(),
        op: "rename".to_string(),
        path: String::new(),
        src: src.clone(),
        dst: dst.clone(),
        data_hash: String::new(),
        result: match &result {
            Ok(()) => "ok".to_string(),
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("not found") || msg.contains("Not found") {
                    "not_found".to_string()
                } else {
                    "error".to_string()
                }
            }
        },
        ts_ns: now_ns(),
    };
    write_entry(writer, &ret).await;
}
