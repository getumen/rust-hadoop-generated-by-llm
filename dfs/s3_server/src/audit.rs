use dfs_common::auth::audit::AuditRecord;
use rocksdb::{WriteBatch, DB};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tracing::{error, info, warn};

/// Maximum number of keys to delete in a single WriteBatch during cleanup
/// to avoid unbounded memory growth.
const CLEANUP_BATCH_LIMIT: usize = 10_000;

pub struct AuditLogger {
    tx: mpsc::Sender<AuditRecord>,
    _db: Arc<DB>,
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
        let (tx, mut rx) = mpsc::channel::<AuditRecord>(1000);

        let db_clone = db.clone();
        tokio::spawn(async move {
            let mut batch_buffer = Vec::with_capacity(batch_size);
            let mut interval = time::interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    Some(record) = rx.recv() => {
                        batch_buffer.push(record);
                        if batch_buffer.len() >= batch_size {
                            let db_ref = db_clone.clone();
                            let records = std::mem::replace(
                                &mut batch_buffer,
                                Vec::with_capacity(batch_size),
                            );
                            match tokio::task::spawn_blocking(move || {
                                Self::flush_batch(&db_ref, records)
                            })
                            .await
                            {
                                Ok(Err(e)) => error!("Failed to flush audit logs: {}", e),
                                Err(e) => error!("Flush task panicked: {}", e),
                                _ => {}
                            }
                        }
                    }
                    _ = interval.tick() => {
                        if !batch_buffer.is_empty() {
                            let db_ref = db_clone.clone();
                            let records = std::mem::replace(
                                &mut batch_buffer,
                                Vec::with_capacity(batch_size),
                            );
                            match tokio::task::spawn_blocking(move || {
                                Self::flush_batch(&db_ref, records)
                            })
                            .await
                            {
                                Ok(Err(e)) => error!("Failed to flush audit logs (interval): {}", e),
                                Err(e) => error!("Flush task panicked: {}", e),
                                _ => {}
                            }
                        }
                    }
                    else => break,
                }
            }
        });

        // TTL cleanup task
        let db_cleanup = db.clone();
        tokio::spawn(async move {
            let mut cleanup_interval = time::interval(Duration::from_secs(3600)); // Every hour
            loop {
                cleanup_interval.tick().await;
                let db_ref = db_cleanup.clone();
                let days = retention_days;
                match tokio::task::spawn_blocking(move || Self::cleanup_old_logs(&db_ref, days))
                    .await
                {
                    Ok(Err(e)) => error!("Failed to cleanup old audit logs: {}", e),
                    Err(e) => error!("Cleanup task panicked: {}", e),
                    _ => {}
                }
            }
        });

        Ok(Self { tx, _db: db })
    }

    /// Queue an audit record for async persistence.
    /// Uses `try_send` to avoid spawning tasks or blocking the caller.
    /// If the channel is full, the record is dropped and a warning is emitted.
    pub fn log(&self, record: AuditRecord) {
        if let Err(e) = self.tx.try_send(record) {
            warn!("Audit log channel full or closed, dropping record: {}", e);
        }
    }

    /// Write a batch of records to RocksDB. On failure the records are lost
    /// (they have already been taken from the buffer). The caller should log
    /// the error so operators are aware.
    fn flush_batch(db: &DB, records: Vec<AuditRecord>) -> anyhow::Result<()> {
        let mut batch = WriteBatch::default();
        for record in &records {
            let ts = chrono::DateTime::parse_from_rfc3339(&record.timestamp)
                .map(|dt| dt.timestamp_millis())
                .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());

            let mut key = Vec::with_capacity(32);
            key.extend_from_slice(b"audit:");
            key.extend_from_slice(&ts.to_be_bytes());
            key.extend_from_slice(b":");
            key.extend_from_slice(record.request_id.as_bytes());

            let val = serde_json::to_vec(record)?;
            batch.put(key, val);
        }
        db.write(batch)?;
        Ok(())
    }

    /// Delete audit logs older than `retention_days`.
    /// Deletes in chunks of `CLEANUP_BATCH_LIMIT` to avoid unbounded memory usage.
    fn cleanup_old_logs(db: &DB, retention_days: u32) -> anyhow::Result<()> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
        let cutoff_ms = cutoff.timestamp_millis();

        let mut end_key = Vec::with_capacity(32);
        end_key.extend_from_slice(b"audit:");
        end_key.extend_from_slice(&cutoff_ms.to_be_bytes());

        let iter = db.prefix_iterator(b"audit:");
        let mut batch = WriteBatch::default();
        let mut count: usize = 0;
        let mut total: usize = 0;
        for result in iter {
            let (key, _) = result?;
            if key[..] >= end_key[..] {
                break;
            }
            batch.delete(&key);
            count += 1;

            // Flush in chunks to bound memory usage
            if count >= CLEANUP_BATCH_LIMIT {
                db.write(batch)?;
                total += count;
                batch = WriteBatch::default();
                count = 0;
            }
        }
        if count > 0 {
            db.write(batch)?;
            total += count;
        }
        info!(
            "Cleaned up {} audit logs older than {} days",
            total, retention_days
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_record(timestamp: &str, request_id: &str) -> AuditRecord {
        AuditRecord {
            timestamp: timestamp.to_string(),
            request_id: request_id.to_string(),
            remote_ip: "127.0.0.1".to_string(),
            user_id: "test-user".to_string(),
            role_arn: None,
            action: "s3:GetObject".to_string(),
            resource: "arn:dfs:s3:::test-bucket/key".to_string(),
            status_code: 200,
            error_code: None,
            user_agent: Some("test-agent".to_string()),
        }
    }

    #[test]
    fn test_flush_batch_key_ordering() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, tmp.path()).unwrap();

        let records = vec![
            make_record("2026-03-13T10:00:00+00:00", "aaa"),
            make_record("2026-03-13T09:00:00+00:00", "bbb"),
            make_record("2026-03-13T11:00:00+00:00", "ccc"),
        ];

        AuditLogger::flush_batch(&db, records).unwrap();

        // Verify records are stored and ordered by timestamp (key prefix)
        let iter = db.prefix_iterator(b"audit:");
        let mut request_ids = Vec::new();
        for result in iter {
            let (_key, val) = result.unwrap();
            let record: AuditRecord = serde_json::from_slice(&val).unwrap();
            request_ids.push(record.request_id);
        }
        assert_eq!(request_ids, vec!["bbb", "aaa", "ccc"]); // 09:00, 10:00, 11:00
    }

    #[test]
    fn test_cleanup_old_logs_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, tmp.path()).unwrap();

        // Insert an old record (100 days ago) and a recent record (now)
        let old_ts = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
        let recent_ts = Utc::now().to_rfc3339();

        let records = vec![
            make_record(&old_ts, "old-record"),
            make_record(&recent_ts, "new-record"),
        ];
        AuditLogger::flush_batch(&db, records).unwrap();

        // Cleanup with 30-day retention
        AuditLogger::cleanup_old_logs(&db, 30).unwrap();

        // Only the recent record should remain
        let iter = db.prefix_iterator(b"audit:");
        let mut remaining = Vec::new();
        for result in iter {
            let (_key, val) = result.unwrap();
            let record: AuditRecord = serde_json::from_slice(&val).unwrap();
            remaining.push(record.request_id);
        }
        assert_eq!(remaining, vec!["new-record"]);
    }
}
