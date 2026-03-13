use dfs_common::auth::audit::AuditRecord;
use rocksdb::{WriteBatch, DB};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tracing::{error, info};

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
                            if let Err(e) = Self::flush_batch(&db_clone, &mut batch_buffer) {
                                error!("Failed to flush audit logs: {}", e);
                            }
                        }
                    }
                    _ = interval.tick() => {
                        if !batch_buffer.is_empty() {
                            if let Err(e) = Self::flush_batch(&db_clone, &mut batch_buffer) {
                                error!("Failed to flush audit logs (interval): {}", e);
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
                if let Err(e) = Self::cleanup_old_logs(&db_cleanup, retention_days) {
                    error!("Failed to cleanup old audit logs: {}", e);
                }
            }
        });

        Ok(Self { tx, _db: db })
    }

    pub fn log(&self, record: AuditRecord) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = tx.send(record).await {
                error!("Failed to queue audit log: {}", e);
            }
        });
    }

    fn flush_batch(db: &DB, buffer: &mut Vec<AuditRecord>) -> anyhow::Result<()> {
        let mut batch = WriteBatch::default();
        for record in buffer.drain(..) {
            let ts = chrono::DateTime::parse_from_rfc3339(&record.timestamp)
                .map(|dt| dt.timestamp_millis())
                .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());

            let mut key = Vec::with_capacity(32);
            key.extend_from_slice(b"audit:");
            key.extend_from_slice(&ts.to_be_bytes());
            key.extend_from_slice(b":");
            key.extend_from_slice(record.request_id.as_bytes());

            let val = serde_json::to_vec(&record)?;
            batch.put(key, val);
        }
        db.write(batch)?;
        Ok(())
    }

    fn cleanup_old_logs(db: &DB, retention_days: u32) -> anyhow::Result<()> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
        let cutoff_ms = cutoff.timestamp_millis();

        let mut end_key = Vec::with_capacity(32);
        end_key.extend_from_slice(b"audit:");
        end_key.extend_from_slice(&cutoff_ms.to_be_bytes());

        // delete_range might not be available in this environment's rocksdb version
        // fallback to iterator-based deletion
        let iter = db.prefix_iterator(b"audit:");
        let mut batch = WriteBatch::default();
        let mut count = 0;
        for result in iter {
            let (key, _) = result?;
            if key[..] >= end_key[..] {
                break;
            }
            batch.delete(key);
            count += 1;
        }
        db.write(batch)?;
        info!(
            "Cleaned up {} audit logs older than {} days",
            count, retention_days
        );
        Ok(())
    }
}
