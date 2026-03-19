use chrono::{DateTime, Utc};
use clap::Parser;
use dfs_common::auth::audit::AuditRecord;
use hmac::{Hmac, Mac};
use rocksdb::{ColumnFamilyDescriptor, DBCompressionType, Options, DB};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Parser, Debug)]
#[command(author, version, about = "CLI tool to read and filter S3 server audit logs", long_about = None)]
struct Args {
    #[arg(help = "Path to the RocksDB audit database")]
    db_path: String,

    #[arg(
        short,
        long,
        help = "Filter by User ID (Access Key or Sub). Using this flag enables optimized index lookup."
    )]
    user: Option<String>,

    #[arg(
        short,
        long,
        help = "Filter by Resource Bucket name. Using this flag enables optimized index lookup."
    )]
    resource: Option<String>,

    #[arg(short, long, help = "Filter by S3 Action (e.g. s3:GetObject)")]
    action: Option<String>,

    #[arg(short, long, help = "Filter by HTTP Status Code")]
    status: Option<u16>,

    #[arg(long, help = "Start time (ISO8601, e.g. 2026-03-19T00:00:00Z)")]
    start: Option<String>,

    #[arg(long, help = "End time (ISO8601)")]
    end: Option<String>,

    #[arg(long, default_value_t = false, help = "Output as raw JSON lines")]
    json: bool,

    #[arg(
        short,
        long,
        default_value_t = 100,
        help = "Limit the number of records"
    )]
    limit: usize,

    #[arg(long, help = "Verify Hash Chain using the provided HMAC secret")]
    verify_chain: Option<String>,
}

fn compute_hmac(record: &AuditRecord, secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");

    // Canonical JSON serialization excluding hashes for tamper-evidence
    let mut rec_clone = record.clone();
    rec_clone.record_hash = None; // record_hash must not be tied into its own computation
    let payload = serde_json::to_string(&rec_clone).unwrap_or_default();

    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let mut opts = Options::default();
    opts.set_compression_type(DBCompressionType::Zstd);

    let cf_logs = ColumnFamilyDescriptor::new("logs", opts.clone());
    let cf_idx_user = ColumnFamilyDescriptor::new("idx_user", opts.clone());
    let cf_idx_resource = ColumnFamilyDescriptor::new("idx_resource", opts.clone());

    let cfs = vec![
        ColumnFamilyDescriptor::new(rocksdb::DEFAULT_COLUMN_FAMILY_NAME, opts.clone()),
        cf_logs,
        cf_idx_user,
        cf_idx_resource,
    ];

    let db = DB::open_cf_descriptors_read_only(&opts, &args.db_path, cfs, false)?;

    if let Some(secret) = &args.verify_chain {
        return verify_chain(&db, secret);
    }

    let start_ms = if let Some(s) = &args.start {
        DateTime::parse_from_rfc3339(s)?
            .with_timezone(&Utc)
            .timestamp_millis() as u64
    } else {
        0
    };

    let end_ms = if let Some(e) = &args.end {
        DateTime::parse_from_rfc3339(e)?
            .with_timezone(&Utc)
            .timestamp_millis() as u64
    } else {
        u64::MAX
    };

    if !args.json {
        println!(
            "{:<25} {:<15} {:<15} {:<20} {:<10} {:<10}",
            "Timestamp", "User", "Action", "Resource", "Status", "Duration"
        );
        println!("{}", "-".repeat(95));
    }

    let mut count = 0;

    if let Some(user_id) = &args.user {
        // Optimized path: Use user secondary index
        let cf = db.cf_handle("idx_user").unwrap();
        let mut start_key = Vec::with_capacity(32);
        start_key.extend_from_slice(user_id.as_bytes());
        start_key.extend_from_slice(b":");

        let iter = db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Forward),
        );

        for result in iter {
            let (key, primary_key) = result?;
            if !key.starts_with(&start_key) {
                break;
            }
            if process_primary_key(&db, &primary_key, start_ms, end_ms, &args)? {
                count += 1;
                if count >= args.limit {
                    break;
                }
            }
        }
    } else if let Some(resource_id) = &args.resource {
        // Optimized path: Use resource secondary index
        let cf = db.cf_handle("idx_resource").unwrap();
        let mut start_key = Vec::with_capacity(32);
        start_key.extend_from_slice(resource_id.as_bytes());
        start_key.extend_from_slice(b":");

        let iter = db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Forward),
        );

        for result in iter {
            let (key, primary_key) = result?;
            if !key.starts_with(&start_key) {
                break;
            }
            if process_primary_key(&db, &primary_key, start_ms, end_ms, &args)? {
                count += 1;
                if count >= args.limit {
                    break;
                }
            }
        }
    } else {
        // Standard path: Scan by time from primary index in CF_LOGS
        let cf = db.cf_handle("logs").unwrap();
        let mut start_key = Vec::with_capacity(16);
        start_key.extend_from_slice(b":"); // Primary keys start with ":"
        if start_ms > 0 {
            start_key.extend_from_slice(&start_ms.to_be_bytes());
        }

        let iter = db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Forward),
        );

        for result in iter {
            let (key, val) = result?;
            if !key.starts_with(b":") {
                break; // Should always start with ":"
            }

            let record: AuditRecord = serde_json::from_slice(&val)?;
            if record.timestamp_ms > end_ms {
                break;
            }
            if record.timestamp_ms < start_ms {
                continue;
            }

            if filter_record(&record, &args) {
                print_record(&record, &args)?;
                count += 1;
                if count >= args.limit {
                    break;
                }
            }
        }
    }

    if !args.json && count == 0 {
        println!("No audit records found matching criteria.");
    }

    Ok(())
}

fn process_primary_key(
    db: &DB,
    primary_key: &[u8],
    start_ms: u64,
    end_ms: u64,
    args: &Args,
) -> anyhow::Result<bool> {
    // Key format is :ts_be:req_id
    if primary_key.len() > 9 && primary_key[0] == b':' {
        let mut ts_bytes = [0u8; 8];
        ts_bytes.copy_from_slice(&primary_key[1..9]);
        let ts = u64::from_be_bytes(ts_bytes);

        if ts < start_ms || ts > end_ms {
            return Ok(false);
        }
    } else {
        return Ok(false);
    }

    let cf_logs = db.cf_handle("logs").unwrap();
    if let Some(val) = db.get_cf(cf_logs, primary_key)? {
        let record: AuditRecord = serde_json::from_slice(&val)?;
        if filter_record(&record, args) {
            print_record(&record, args)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn verify_chain(db: &DB, secret: &str) -> anyhow::Result<()> {
    println!("Verifying Hash Chain...");
    let cf_logs = db.cf_handle("logs").unwrap();
    let iter = db.iterator_cf(cf_logs, rocksdb::IteratorMode::Start);

    let mut last_hash: Option<String> = None;
    let mut verified_count = 0;

    for result in iter {
        let (_, val) = result?;
        let record: AuditRecord = serde_json::from_slice(&val)?;

        if verified_count > 0 && record.previous_hash != last_hash {
            println!(
                "❌ Chain Mismatch Discovered at Request ID: {}",
                record.request_id
            );
            println!("  Expected previous_hash: {:?}", last_hash);
            println!("  Found previous_hash:    {:?}", record.previous_hash);
            return Err(anyhow::anyhow!("Audit log hash chain is broken!"));
        }

        // Verify current record hash
        let expected_hash = compute_hmac(&record, secret);
        if record.record_hash.as_deref() != Some(&expected_hash) {
            println!(
                "❌ Record Hash Mismatch Discovered at Request ID: {}",
                record.request_id
            );
            println!("  Computed hash: {:?}", expected_hash);
            println!("  Found hash:    {:?}", record.record_hash);
            return Err(anyhow::anyhow!("Audit log record is tampered!"));
        }

        last_hash = record.record_hash;
        verified_count += 1;
    }

    println!("✅ Hash Chain Verification Successful!");
    println!("✅ Total {} records verified.", verified_count);

    Ok(())
}

fn filter_record(record: &AuditRecord, args: &Args) -> bool {
    if let Some(u) = &args.user {
        if &record.user_id != u {
            return false;
        }
    }
    if let Some(a) = &args.action {
        if &record.action != a {
            return false;
        }
    }
    if let Some(s) = args.status {
        if record.status_code != s {
            return false;
        }
    }
    if let Some(r) = &args.resource {
        let resource_parts: Vec<&str> = record.resource.split(':').collect();
        let bucket_id = if let Some(&part) = resource_parts.get(5) {
            part
        } else {
            record.resource.as_str()
        };
        let bucket_name = bucket_id.split('/').next().unwrap_or(bucket_id);
        if bucket_name != r {
            return false;
        }
    }
    true
}

fn print_record(record: &AuditRecord, args: &Args) -> anyhow::Result<()> {
    if args.json {
        println!("{}", serde_json::to_string(record)?);
    } else {
        let user = if let Some(role) = &record.role_arn {
            role.split(':').next_back().unwrap_or(&record.user_id)
        } else {
            &record.user_id
        };

        let resource = record
            .resource
            .split(':')
            .next_back()
            .unwrap_or(&record.resource);
        let duration = record
            .duration_ms
            .map(|d| format!("{}ms", d))
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<25} {:<15} {:<15} {:<20} {:<10} {:<10}",
            record.timestamp, user, record.action, resource, record.status_code, duration
        );
    }
    Ok(())
}
