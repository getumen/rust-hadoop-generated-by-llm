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
            let mut committed_last_hash: Option<String> = None;
            let mut global_last_ts_ms: u64 = 0;

            // Recover the last hash & timestamp from DB on startup
            if let Some(cf) = db_clone.cf_handle(CF_LOGS) {
                let mut iter = db_clone.iterator_cf(cf, rocksdb::IteratorMode::End);
                if let Some(Ok((_, val))) = iter.next() {
                    if let Ok(last_rec) = serde_json::from_slice::<AuditRecord>(&val) {
                        committed_last_hash = last_rec.record_hash;
                        global_last_ts_ms = last_rec.timestamp_ms;
                        info!("Recovered last audit log hash for chaining ({}). Sequence starting at TS: {}", committed_last_hash.as_deref().unwrap_or("None"), global_last_ts_ms);
                    }
                }
            }

            loop {
                tokio::select! {
                    res = rx.recv() => {
                        match res {
                            Some(record) => {
                                batch_buffer.push(record);
                                if batch_buffer.len() >= batch_size {
                                    let mut records = std::mem::replace(
                                        &mut batch_buffer,
                                        Vec::with_capacity(batch_size),
                                    );

                                    // Keep original timestamp immutable, compute monotonic key_ts for DB sorting
                                    records.sort_by(|a, b| {
                                        a.timestamp_ms.cmp(&b.timestamp_ms).then_with(|| a.request_id.cmp(&b.request_id))
                                    });
                                    let mut keyed_records = Vec::with_capacity(records.len());

                                    // Compute HMAC chaining correctly across the batch
                                    let mut temp_hash = committed_last_hash.clone();
                                    for mut rec in records.into_iter() {
                                        let key_ts = std::cmp::max(rec.timestamp_ms, global_last_ts_ms);
                                        global_last_ts_ms = key_ts;
                                        rec.previous_hash = temp_hash.clone();
                                        temp_hash = Some(Self::compute_hmac(&rec, &hmac_secret));
                                        rec.record_hash = temp_hash.clone();
                                        keyed_records.push((key_ts, rec));
                                    }

                                    let db_ref = db_clone.clone();

                                    // Serialize DB writes in the same worker to guarantee order
                                    let mut success = false;
                                    for attempt in 1..=3 {
                                        let recs_inner = keyed_records.clone();
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
                                    if success {
                                        committed_last_hash = temp_hash; // Advance only on persistance
                                    } else {
                                        error!("Critical: Failed to flush audit logs after 3 attempts. Data lost.");
                                    }
                                }
                            }
                            None => break, // Channel closed (Logger dropped)
                        }
                    }
                    _ = interval.tick() => {
                        if !batch_buffer.is_empty() {
                            let mut records = std::mem::replace(
                                &mut batch_buffer,
                                Vec::with_capacity(batch_size),
                            );

                            // Keep original timestamp immutable, compute monotonic key_ts for DB sorting
                            records.sort_by(|a, b| {
                                a.timestamp_ms.cmp(&b.timestamp_ms).then_with(|| a.request_id.cmp(&b.request_id))
                            });
                            let mut keyed_records = Vec::with_capacity(records.len());

                            // Compute HMAC chaining correctly across the batch
                            let mut temp_hash = committed_last_hash.clone();
                            for mut rec in records.into_iter() {
                                let key_ts = std::cmp::max(rec.timestamp_ms, global_last_ts_ms);
                                global_last_ts_ms = key_ts;
                                rec.previous_hash = temp_hash.clone();
                                temp_hash = Some(Self::compute_hmac(&rec, &hmac_secret));
                                rec.record_hash = temp_hash.clone();
                                keyed_records.push((key_ts, rec));
                            }

                            let db_ref = db_clone.clone();
                            let mut success = false;
                            for attempt in 1..=3 {
                                let recs_inner = keyed_records.clone();
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
                            if success {
                                committed_last_hash = temp_hash;
                            } else {
                                error!("Critical: Failed to flush audit logs after 3 attempts (interval). Data lost.");
                            }
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
                // Keep original timestamp immutable, compute monotonic key_ts
                batch_buffer.sort_by(|a, b| {
                    a.timestamp_ms
                        .cmp(&b.timestamp_ms)
                        .then_with(|| a.request_id.cmp(&b.request_id))
                });

                let mut keyed_records = Vec::with_capacity(batch_buffer.len());
                // Determine final chain
                let mut temp_hash = committed_last_hash.clone();
                for mut rec in batch_buffer.into_iter() {
                    let key_ts = std::cmp::max(rec.timestamp_ms, global_last_ts_ms);
                    global_last_ts_ms = key_ts;
                    rec.previous_hash = temp_hash.clone();
                    temp_hash = Some(Self::compute_hmac(&rec, &hmac_secret));
                    rec.record_hash = temp_hash.clone();
                    keyed_records.push((key_ts, rec));
                }

                let db_ref = db_clone.clone();
                match tokio::task::spawn_blocking(move || {
                    Self::flush_batch(&db_ref, &keyed_records)
                })
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

        // Canonical JSON serialization excluding hashes for tamper-evidence
        let mut rec_clone = record.clone();
        rec_clone.record_hash = None; // record_hash must not be tied into its own computation
        let payload = serde_json::to_string(&rec_clone).unwrap_or_default();

        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn extract_bucket_name(resource: &str) -> &str {
        let parts: Vec<&str> = resource.split(':').collect();
        let bucket_id = if let Some(&part) = parts.get(5) {
            part
        } else {
            resource
        };
        bucket_id.split('/').next().unwrap_or(bucket_id)
    }

    fn flush_batch(db: &DB, records: &[(u64, AuditRecord)]) -> anyhow::Result<()> {
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

        for (key_ts, record) in records {
            let ts_bytes = key_ts.to_be_bytes();
            let req_id_bytes = record.request_id.as_bytes();

            // 1. Primary Record Key (logs CF): :<ts_be>:<req_id>
            let mut key = Vec::with_capacity(1 + 8 + 1 + req_id_bytes.len());
            key.extend_from_slice(b":");
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
            let bucket_name = Self::extract_bucket_name(&record.resource);

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
        end_key.extend_from_slice(b":");
        end_key.extend_from_slice(&ts_cutoff_bytes);

        let cf_logs = db.cf_handle(CF_LOGS).unwrap();
        let cf_user = db.cf_handle(CF_IDX_USER).unwrap();
        let cf_resource = db.cf_handle(CF_IDX_RESOURCE).unwrap();

        let mut batch = rocksdb::WriteBatch::default();

        let mut iter_opts = rocksdb::ReadOptions::default();
        iter_opts.set_iterate_upper_bound(end_key.as_slice());
        let iter = db.iterator_cf_opt(cf_logs, iter_opts, rocksdb::IteratorMode::Start);

        let mut deleted_count = 0;
        for result in iter {
            let (key, val) = result?;

            if let Ok(record) = serde_json::from_slice::<AuditRecord>(&val) {
                // Delete user index
                let mut user_idx_key = Vec::with_capacity(32);
                user_idx_key.extend_from_slice(record.user_id.as_bytes());
                user_idx_key.extend_from_slice(b":");
                user_idx_key.extend_from_slice(&key);
                batch.delete_cf(cf_user, user_idx_key);

                // Delete resource index
                let bucket_name = Self::extract_bucket_name(&record.resource);

                let mut res_idx_key = Vec::with_capacity(64);
                res_idx_key.extend_from_slice(bucket_name.as_bytes());
                res_idx_key.extend_from_slice(b":");
                res_idx_key.extend_from_slice(&key);
                batch.delete_cf(cf_resource, res_idx_key);

                deleted_count += 1;

                // Avoid too large batches: Execute intermediate chunks if huge
                if deleted_count % 10000 == 0 {
                    db.write(std::mem::take(&mut batch))?;
                }
            }
        }

        // Finally range delete from the primary CF
        batch.delete_range_cf(cf_logs, b":".as_slice(), end_key.as_slice());
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
            previous_hash: None,
            record_hash: None,
        }
    }

    #[tokio::test]
    async fn test_cf_write_and_secondary_index() {
        let tmp = tempfile::tempdir().unwrap();
        let db = AuditLogger::new(tmp.path(), 30, 10, "secret".to_string())
            .unwrap()
            .db;

        let record_1 = make_record("2026-03-13T10:00:00+00:00", "req1", "user-a");
        let record_2 = make_record("2026-03-13T11:00:00+00:00", "req2", "user-b");
        let record_3 = make_record("2026-03-13T12:00:00+00:00", "req3", "user-a");

        let records = vec![
            (record_1.timestamp_ms, record_1),
            (record_2.timestamp_ms, record_2),
            (record_3.timestamp_ms, record_3),
        ];

        AuditLogger::flush_batch(&db, &records).unwrap();

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
