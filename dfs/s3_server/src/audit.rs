use dfs_common::auth::audit::AuditRecord;
use prometheus::{IntCounter, Registry};
use rocksdb::{WriteBatch, DB};
use std::path::Path;
use std::sync::{Arc, LazyLock};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{self, Duration, Instant};
use tracing::{error, info, warn};

// Prometheus metrics for Audit Logging
pub static AUDIT_LOG_TOTAL: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "audit_log_total",
        "Total number of audit logs sent to the logger",
    )
    .unwrap()
});
pub static AUDIT_LOG_DROPPED: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "audit_log_dropped_total",
        "Total number of audit logs dropped due to full buffer",
    )
    .unwrap()
});
pub static AUDIT_LOG_FLUSH_ERRORS: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "audit_log_flush_errors_total",
        "Total number of audit log flush failures",
    )
    .unwrap()
});

pub struct AuditLogger {
    tx: mpsc::Sender<AuditRecord>,
    #[allow(dead_code)]
    db: Arc<DB>,
}

impl AuditLogger {
    pub fn new<P: AsRef<Path>>(
        db_path: P,
        retention_days: u32,
        batch_size: usize,
    ) -> anyhow::Result<Self> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = Arc::new(DB::open(&opts, db_path)?);
        let (tx, mut rx) = mpsc::channel::<AuditRecord>(10000);

        // Limit concurrent flushes to avoid overwhelming RocksDB during heavy load
        let flush_semaphore = Arc::new(Semaphore::new(5));

        let db_clone = db.clone();
        tokio::spawn(async move {
            let mut batch_buffer = Vec::with_capacity(batch_size);
            let mut interval = time::interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    res = rx.recv() => {
                        match res {
                            Some(record) => {
                                batch_buffer.push(record);
                                if batch_buffer.len() >= batch_size {
                                    let records = std::mem::replace(
                                        &mut batch_buffer,
                                        Vec::with_capacity(batch_size),
                                    );
                                    let db_ref = db_clone.clone();
                                    let sem = flush_semaphore.clone();
                                    tokio::spawn(async move {
                                        let _permit = match sem.acquire().await {
                                            Ok(p) => p,
                                            Err(_) => return, // Runtime shutting down
                                        };

                                        // Retry flush up to 3 times to prevent data loss on transient DB issues
                                        let records_arc = Arc::new(records);
                                        let mut success = false;
                                        for attempt in 1..=3 {
                                            let db_inner = db_ref.clone();
                                            let recs_inner = records_arc.clone();
                                            match tokio::task::spawn_blocking(move || {
                                                Self::flush_batch(&db_inner, &recs_inner)
                                            }).await {
                                                Ok(Ok(_)) => {
                                                    success = true;
                                                    break;
                                                },
                                                Ok(Err(e)) => {
                                                    error!("Failed to flush audit logs (attempt {}): {}", attempt, e);
                                                    AUDIT_LOG_FLUSH_ERRORS.inc();
                                                }
                                                Err(e) => {
                                                    error!("Flush task panicked (attempt {}): {}", attempt, e);
                                                    AUDIT_LOG_FLUSH_ERRORS.inc();
                                                }
                                            }
                                            time::sleep(Duration::from_millis(500 * attempt)).await;
                                        }
                                        if !success {
                                            error!("Critical: Failed to flush audit logs after 3 attempts. Data lost.");
                                        }
                                    });
                                }
                            }
                            None => break, // Channel closed (Logger dropped)
                        }
                    }
                    _ = interval.tick() => {
                        if !batch_buffer.is_empty() {
                            let records = std::mem::replace(
                                &mut batch_buffer,
                                Vec::with_capacity(batch_size),
                            );
                            let db_ref = db_clone.clone();
                            let sem = flush_semaphore.clone();
                            tokio::spawn(async move {
                                let _permit = match sem.acquire().await {
                                    Ok(p) => p,
                                    Err(_) => return,
                                };
                                let records_arc = Arc::new(records);
                                let mut success = false;
                                for attempt in 1..=3 {
                                    let db_inner = db_ref.clone();
                                    let recs_inner = records_arc.clone();
                                    match tokio::task::spawn_blocking(move || {
                                        Self::flush_batch(&db_inner, &recs_inner)
                                    }).await {
                                        Ok(Ok(_)) => {
                                            success = true;
                                            break;
                                        },
                                        Ok(Err(e)) => {
                                            error!("Failed to flush audit logs (interval, attempt {}): {}", attempt, e);
                                            AUDIT_LOG_FLUSH_ERRORS.inc();
                                        }
                                        Err(e) => {
                                            error!("Flush task panicked (interval, attempt {}): {}", attempt, e);
                                            AUDIT_LOG_FLUSH_ERRORS.inc();
                                        }
                                    }
                                    time::sleep(Duration::from_millis(500 * attempt)).await;
                                }
                                if !success {
                                    error!("Critical: Failed to flush audit logs after 3 attempts (interval). Data lost.");
                                }
                            });
                        }
                    }
                }
            }

            // --- Graceful Shutdown ---
            info!("S3 AuditLogger shutting down, draining remaining records...");
            while let Ok(record) = rx.try_recv() {
                batch_buffer.push(record);
            }

            if !batch_buffer.is_empty() {
                let db_ref = db_clone.clone();
                // Final flush
                match tokio::task::spawn_blocking(move || Self::flush_batch(&db_ref, &batch_buffer))
                    .await
                {
                    Ok(Ok(_)) => info!("Final audit log flush completed."),
                    Ok(Err(e)) => error!("Failed to perform final audit log flush: {}", e),
                    Err(e) => error!("Final flush task panicked: {}", e),
                }
            }
        });

        // TTL cleanup task with delayed startup to avoid I/O spike
        let db_cleanup = db.clone();
        tokio::spawn(async move {
            // First tick after 1 hour to allow server to warm up
            let start = Instant::now() + Duration::from_secs(3600);
            let mut cleanup_interval = time::interval_at(start, Duration::from_secs(3600));

            loop {
                cleanup_interval.tick().await;
                let db_ref = db_cleanup.clone();
                let days = retention_days;
                match tokio::task::spawn_blocking(move || Self::cleanup_old_logs(&db_ref, days))
                    .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        error!("Failed to cleanup old audit logs: {}", e);
                    }
                    Err(e) => {
                        error!("Cleanup task panicked: {}", e);
                    }
                }
            }
        });

        Ok(Self { tx, db })
    }

    /// Register audit log metrics with a Prometheus registry
    #[allow(dead_code)]
    pub fn register_metrics(registry: &Registry) -> anyhow::Result<()> {
        registry.register(Box::new(AUDIT_LOG_TOTAL.clone()))?;
        registry.register(Box::new(AUDIT_LOG_DROPPED.clone()))?;
        registry.register(Box::new(AUDIT_LOG_FLUSH_ERRORS.clone()))?;
        Ok(())
    }

    pub fn log(&self, record: AuditRecord) {
        AUDIT_LOG_TOTAL.inc();
        if let Err(e) = self.tx.try_send(record) {
            AUDIT_LOG_DROPPED.inc();
            warn!("Audit log channel full or closed, dropping record: {}", e);
        }
    }

    #[allow(dead_code)]
    pub fn db(&self) -> &DB {
        &self.db
    }

    /// Write a batch of records to RocksDB including secondary indexes.
    /// Index pattern: index:user:<user_id>:<timestamp_ms>:<request_id>
    fn flush_batch(db: &DB, records: &[AuditRecord]) -> anyhow::Result<()> {
        let mut batch = WriteBatch::default();
        for record in records {
            let ts_bytes = record.timestamp_ms.to_be_bytes();
            let req_id_bytes = record.request_id.as_bytes();

            // 1. Primary Record: audit:<ts_be>:<req_id>
            let mut key = Vec::with_capacity(32);
            key.extend_from_slice(b"audit:");
            key.extend_from_slice(&ts_bytes);
            key.extend_from_slice(b":");
            key.extend_from_slice(req_id_bytes);

            let val = serde_json::to_vec(record)?;
            batch.put(&key, val);

            // 2. Secondary Index (User): index:user:<user_id>:<ts_be>:<req_id>
            let mut user_idx_key = Vec::with_capacity(64);
            user_idx_key.extend_from_slice(b"index:user:");
            user_idx_key.extend_from_slice(record.user_id.as_bytes());
            user_idx_key.extend_from_slice(b":");
            user_idx_key.extend_from_slice(&ts_bytes);
            user_idx_key.extend_from_slice(b":");
            user_idx_key.extend_from_slice(req_id_bytes);

            batch.put(user_idx_key, key);
        }

        if let Err(e) = db.write(batch) {
            AUDIT_LOG_FLUSH_ERRORS.inc();
            return Err(e.into());
        }
        Ok(())
    }

    fn cleanup_old_logs(db: &DB, retention_days: u32) -> anyhow::Result<()> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
        let cutoff_ms = cutoff.timestamp_millis();
        let ts_cutoff_bytes = cutoff_ms.to_be_bytes();

        let mut batch = rocksdb::WriteBatch::default();

        // 1. Cleanup Primary Records
        let start_key = b"audit:";
        let mut end_key = Vec::with_capacity(16);
        end_key.extend_from_slice(b"audit:");
        end_key.extend_from_slice(&ts_cutoff_bytes);

        batch.delete_range(start_key.as_slice(), end_key.as_slice());

        // 2. Cleanup User Secondary Indexes
        let idx_start = b"index:user:";
        let idx_end = b"index:user;"; // ';' is ':' + 1 in ASCII

        let mut iter_opts = rocksdb::ReadOptions::default();
        iter_opts.set_iterate_upper_bound(idx_end.to_vec());
        let iter = db.iterator_opt(
            rocksdb::IteratorMode::From(idx_start, rocksdb::Direction::Forward),
            iter_opts,
        );

        for result in iter {
            let (key, _) = result?;
            // Robust parsing of combined key: index:user:<user_id>:<ts_be>:<req_id>
            if let Some(last_colon_pos) = key.iter().rposition(|&b| b == b':') {
                if last_colon_pos >= 8 {
                    let ts_pos = last_colon_pos - 8;
                    let mut ts_bytes = [0u8; 8];
                    ts_bytes.copy_from_slice(&key[ts_pos..last_colon_pos]);
                    let ts = u64::from_be_bytes(ts_bytes);
                    if ts < cutoff_ms as u64 {
                        batch.delete(key);
                    }
                }
            }
        }

        db.write(batch)?;
        info!(
            "Cleaned up audit logs and indexes older than {} days (cutoff: {})",
            retention_days, cutoff
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(timestamp: &str, request_id: &str, user_id: &str) -> AuditRecord {
        let ts_ms = chrono::DateTime::parse_from_rfc3339(timestamp)
            .map(|dt| dt.timestamp_millis() as u64)
            .unwrap_or(0);
        AuditRecord {
            timestamp: timestamp.to_string(),
            timestamp_ms: ts_ms,
            request_id: request_id.to_string(),
            remote_ip: "127.0.0.1".to_string(),
            user_id: user_id.to_string(),
            role_arn: None,
            action: "s3:GetObject".to_string(),
            resource: "arn:dfs:s3:::test-bucket/key".to_string(),
            status_code: 200,
            error_code: None,
            user_agent: Some("test-agent".to_string()),
            duration_ms: Some(10),
        }
    }

    #[test]
    fn test_secondary_index_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, tmp.path()).unwrap();

        let records = vec![
            make_record("2026-03-13T10:00:00+00:00", "req1", "user-a"),
            make_record("2026-03-13T11:00:00+00:00", "req2", "user-b"),
            make_record("2026-03-13T12:00:00+00:00", "req3", "user-a"),
        ];

        AuditLogger::flush_batch(&db, &records).unwrap();

        // Search for user-a
        let prefix = b"index:user:user-a:";
        let iter = db.prefix_iterator(prefix);
        let mut found = Vec::new();
        for result in iter {
            let (key, primary_key) = result.unwrap();
            if !key.starts_with(prefix) {
                break;
            }
            let val = db.get(primary_key).unwrap().unwrap();
            let record: AuditRecord = serde_json::from_slice(&val).unwrap();
            found.push(record.request_id);
        }
        assert_eq!(found, vec!["req1", "req3"]);
    }
}
