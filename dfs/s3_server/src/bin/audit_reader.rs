use chrono::{DateTime, Utc};
use clap::Parser;
use dfs_common::auth::audit::AuditRecord;
use rocksdb::DB;

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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let opts = rocksdb::Options::default();
    let db = DB::open_for_read_only(&opts, &args.db_path, false)?;

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
        // Optimized path: Use secondary index: index:user:<user_id>:<ts_be>:<req_id>
        let mut user_prefix = Vec::with_capacity(32);
        user_prefix.extend_from_slice(b"index:user:");
        user_prefix.extend_from_slice(user_id.as_bytes());
        user_prefix.extend_from_slice(b":");

        let mut start_key = user_prefix.clone();
        if start_ms > 0 {
            start_key.extend_from_slice(&start_ms.to_be_bytes());
        }

        let iter = db.iterator(rocksdb::IteratorMode::From(
            &start_key,
            rocksdb::Direction::Forward,
        ));

        for result in iter {
            let (key, primary_key) = result?;
            if !key.starts_with(&user_prefix) {
                break;
            }

            // Extract timestamp from index key to check end_ms filter Before reading primary
            let ts_pos = key.len() - 45; // 36 (uuid) + 1 (:) + 8 (ts) = 45
            let mut ts_bytes = [0u8; 8];
            ts_bytes.copy_from_slice(&key[ts_pos..ts_pos + 8]);
            let ts = u64::from_be_bytes(ts_bytes);
            if ts > end_ms {
                break;
            }

            // Fetch actual record from primary key
            if let Some(val) = db.get(&primary_key)? {
                let record: AuditRecord = serde_json::from_slice(&val)?;
                if filter_record(&record, &args) {
                    print_record(&record, &args)?;
                    count += 1;
                }
            }

            if count >= args.limit {
                break;
            }
        }
    } else {
        // Standard path: Scan by time from primary index: audit:<ts_be>:<req_id>
        let mut start_key = Vec::with_capacity(16);
        start_key.extend_from_slice(b"audit:");
        start_key.extend_from_slice(&start_ms.to_be_bytes());

        let iter = db.iterator(rocksdb::IteratorMode::From(
            &start_key,
            rocksdb::Direction::Forward,
        ));

        for result in iter {
            let (key, val) = result?;
            if !key.starts_with(b"audit:") {
                break;
            }

            let record: AuditRecord = serde_json::from_slice(&val)?;
            if record.timestamp_ms > end_ms {
                break;
            }

            if filter_record(&record, &args) {
                print_record(&record, &args)?;
                count += 1;
            }

            if count >= args.limit {
                break;
            }
        }
    }

    if !args.json && count == 0 {
        println!("No audit records found matching criteria.");
    }

    Ok(())
}

fn filter_record(record: &AuditRecord, args: &Args) -> bool {
    // User filter is already handled by index if provided, but we check again for safety
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
