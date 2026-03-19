use dfs_common::auth::audit::AuditRecord;
use hmac::{Hmac, Mac};
use prometheus::{IntCounter, Registry};
use rocksdb::{ColumnFamilyDescriptor, DBCompressionType, Options, WriteBatch, DB};
use sha2::Sha256;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, Instant};
use tracing::{error, info, warn};

// Use HMAC-SHA256
type HmacSha256 = Hmac<Sha256>;

pub const CF_LOGS: &str = "logs";
pub const CF_IDX_USER: &str = "idx_user";
pub const CF_IDX_RESOURCE: &str = "idx_resource";

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
        hmac_secret: String,
    ) -> anyhow::Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        // Use Zstd compression for high efficiency with JSON logs
        opts.set_compression_type(DBCompressionType::Zstd);

        let cf_logs = ColumnFamilyDescriptor::new(CF_LOGS, opts.clone());
        let cf_idx_user = ColumnFamilyDescriptor::new(CF_IDX_USER, opts.clone());
        let cf_idx_resource = ColumnFamilyDescriptor::new(CF_IDX_RESOURCE, opts.clone());

        let cfs = vec![
            ColumnFamilyDescriptor::new(rocksdb::DEFAULT_COLUMN_FAMILY_NAME, opts.clone()),
            cf_logs,
            cf_idx_user,
            cf_idx_resource,
        ];

        let db = Arc::new(DB::open_cf_descriptors(&opts, db_path, cfs)?);
        let (tx, mut rx) = mpsc::channel::<AuditRecord>(10000);

        let db_clone = db.clone();

        // Single Worker Task for sequential write and HMAC chaining
        tokio::spawn(async move {
            let mut batch_buffer = Vec::with_capacity(batch_size);
            let mut interval = time::interval(Duration::from_secs(5));
            let mut last_hash: Option<String> = None;

            // Recover the last hash from DB on startup
            if let Some(cf) = db_clone.cf_handle(CF_LOGS) {
                let mut iter = db_clone.iterator_cf(cf, rocksdb::IteratorMode::End);
                if let Some(Ok((_, val))) = iter.next() {
                    if let Ok(last_rec) = serde_json::from_slice::<AuditRecord>(&val) {
                        last_hash = last_rec.record_hash;
                        info!(
                            "Recovered last audit log hash for chaining ({}).",
                            last_hash.as_deref().unwrap_or("None")
                        );
                    }
                }
            }

            loop {
                tokio::select! {
                    res = rx.recv() => {
                        match res {
                            Some(mut record) => {
                                // Compute HMAC Chaining
                                record.previous_hash = last_hash.clone();
                                last_hash = Some(Self::compute_hmac(&record, &hmac_secret));
                                record.record_hash = last_hash.clone();

                                batch_buffer.push(record);
                                if batch_buffer.len() >= batch_size {
                                    let records = std::mem::replace(
                                        &mut batch_buffer,
                                        Vec::with_capacity(batch_size),
                                    );
                                    let db_ref = db_clone.clone();

                                    // Serialize DB writes in the same worker to guarantee order
                                    let mut success = false;
                                    for attempt in 1..=3 {
                                        let recs_inner = records.clone();
                                        match tokio::task::spawn_blocking({
                                            let db_inner = db_ref.clone();
                                            move || Self::flush_batch(&db_inner, &recs_inner)
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
                            let mut success = false;
                            for attempt in 1..=3 {
                                let recs_inner = records.clone();
                                match tokio::task::spawn_blocking({
                                    let db_inner = db_ref.clone();
                                    move || Self::flush_batch(&db_inner, &recs_inner)
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
                        }
                    }
                }
            }

            // --- Graceful Shutdown ---
            info!("S3 AuditLogger shutting down, draining remaining records...");
            while let Ok(mut record) = rx.try_recv() {
                record.previous_hash = last_hash.clone();
                last_hash = Some(Self::compute_hmac(&record, &hmac_secret));
                record.record_hash = last_hash.clone();
                batch_buffer.push(record);
            }

            if !batch_buffer.is_empty() {
                let db_ref = db_clone.clone();
                match tokio::task::spawn_blocking(move || Self::flush_batch(&db_ref, &batch_buffer))
                    .await
                {
                    Ok(Ok(_)) => info!("Final audit log flush completed."),
                    Ok(Err(e)) => error!("Failed to perform final audit log flush: {}", e),
                    Err(e) => error!("Final flush task panicked: {}", e),
                }
            }
        });

        // TTL cleanup task
        let db_cleanup = db.clone();
        tokio::spawn(async move {
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

    pub fn compute_hmac(record: &AuditRecord, secret: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");

        let payload = format!(
            "{}:{}:{}:{}:{}:{}:{}",
            record.previous_hash.as_deref().unwrap_or(""),
            record.timestamp_ms,
            record.request_id,
            record.user_id,
            record.action,
            record.resource,
            record.status_code
        );
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn flush_batch(db: &DB, records: &[AuditRecord]) -> anyhow::Result<()> {
        let mut batch = WriteBatch::default();
        let cf_logs = db
            .cf_handle(CF_LOGS)
            .ok_or_else(|| anyhow::anyhow!("Missing CF logs"))?;
        let cf_user = db
            .cf_handle(CF_IDX_USER)
            .ok_or_else(|| anyhow::anyhow!("Missing CF idx_user"))?;
        let cf_resource = db
            .cf_handle(CF_IDX_RESOURCE)
            .ok_or_else(|| anyhow::anyhow!("Missing CF idx_resource"))?;

        for record in records {
            let ts_bytes = record.timestamp_ms.to_be_bytes();
            let req_id_bytes = record.request_id.as_bytes();

            // 1. Primary Record Key (logs CF): <ts_be>:<req_id>
            let mut key = Vec::with_capacity(8 + 1 + req_id_bytes.len());
            key.extend_from_slice(&ts_bytes);
            key.extend_from_slice(b":");
            key.extend_from_slice(req_id_bytes);

            let val = serde_json::to_vec(record)?;
            batch.put_cf(cf_logs, &key, val);

            // 2. User Index: <user_id>:<ts_be>:<req_id> -> key
            let mut user_idx_key = Vec::with_capacity(32);
            user_idx_key.extend_from_slice(record.user_id.as_bytes());
            user_idx_key.extend_from_slice(b":");
            user_idx_key.extend_from_slice(&key);
            batch.put_cf(cf_user, user_idx_key, &key);

            // 3. Resource Index (bucket level): <bucket_name>:<ts_be>:<req_id> -> key
            let resource_parts: Vec<&str> = record.resource.split(':').collect();
            let bucket_id = if let Some(&part) = resource_parts.get(5) {
                part
            } else {
                record.resource.as_str()
            };
            let bucket_name = bucket_id.split('/').next().unwrap_or(bucket_id);

            let mut res_idx_key = Vec::with_capacity(64);
            res_idx_key.extend_from_slice(bucket_name.as_bytes());
            res_idx_key.extend_from_slice(b":");
            res_idx_key.extend_from_slice(&key);
            batch.put_cf(cf_resource, res_idx_key, &key);
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

        let mut end_key = Vec::with_capacity(16);
        end_key.extend_from_slice(&ts_cutoff_bytes);

        let cf_logs = db.cf_handle(CF_LOGS).unwrap();
        let mut batch = rocksdb::WriteBatch::default();

        // Clean logs CF (start to cutoff timestamp)
        batch.delete_range_cf(cf_logs, [0u8].as_slice(), end_key.as_slice());
        db.write(batch)?;

        // Manually iterate and clean secondary indexes
        for cf_name in &[CF_IDX_USER, CF_IDX_RESOURCE] {
            let cf = db.cf_handle(cf_name).unwrap();
            let iter = db.iterator_cf(cf, rocksdb::IteratorMode::Start);
            let mut del_batch = rocksdb::WriteBatch::default();
            for result in iter {
                let (key, _) = result?;
                if let Some(pos) = key.iter().rposition(|&b| b == b':') {
                    if pos >= 8 {
                        let ts_pos = pos - 8;
                        if &key[ts_pos..ts_pos] == b":" {
                            continue; // Safety check
                        }
                        let mut ts_bytes = [0u8; 8];
                        ts_bytes.copy_from_slice(&key[ts_pos..pos]);
                        let ts = u64::from_be_bytes(ts_bytes);
                        if ts < cutoff_ms as u64 {
                            del_batch.delete_cf(cf, key);
                        }
                    }
                }
            }
            db.write(del_batch)?;
        }

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
            previous_hash: None,
            record_hash: None,
        }
    }

    #[test]
    fn test_cf_write_and_secondary_index() {
        let tmp = tempfile::tempdir().unwrap();
        let db = AuditLogger::new(tmp.path(), 30, 10, "secret".to_string())
            .unwrap()
            .db;

        let records = vec![
            make_record("2026-03-13T10:00:00+00:00", "req1", "user-a"),
            make_record("2026-03-13T11:00:00+00:00", "req2", "user-b"),
            make_record("2026-03-13T12:00:00+00:00", "req3", "user-a"),
        ];

        AuditLogger::flush_batch(&db, &records).unwrap();

        // Search for user-a in CF_IDX_USER
        let cf_user = db.cf_handle(CF_IDX_USER).unwrap();
        let cf_logs = db.cf_handle(CF_LOGS).unwrap();

        let prefix = b"user-a:";
        let iter = db.prefix_iterator_cf(cf_user, prefix);
        let mut found = Vec::new();
        for result in iter {
            let (key, primary_key) = result.unwrap();
            if !key.starts_with(prefix) {
                break;
            }
            let val = db.get_cf(cf_logs, primary_key).unwrap().unwrap();
            let record: AuditRecord = serde_json::from_slice(&val).unwrap();
            found.push(record.request_id);
        }
        assert_eq!(found, vec!["req1", "req3"]);

        // Test resource index (bucket is 'test-bucket')
        let cf_resource = db.cf_handle(CF_IDX_RESOURCE).unwrap();
        let res_prefix = b"test-bucket:";
        let mut res_found = 0;
        let iter_res = db.prefix_iterator_cf(cf_resource, res_prefix);
        for result in iter_res {
            let (key, _) = result.unwrap();
            if key.starts_with(res_prefix) {
                res_found += 1;
            } else {
                break;
            }
        }
        assert_eq!(res_found, 3);
    }
}
