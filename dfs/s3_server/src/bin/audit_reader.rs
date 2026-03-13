use dfs_common::auth::audit::AuditRecord;
use rocksdb::DB;
use std::env;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: audit_reader <db_path>");
        std::process::exit(1);
    }
    let db_path = &args[1];
    let opts = rocksdb::Options::default();
    let db = DB::open_for_read_only(&opts, db_path, false)?;

    let iter = db.prefix_iterator(b"audit:");
    for result in iter {
        let (_key, val) = result?;
        let record: AuditRecord = serde_json::from_slice(&val)?;
        println!("{:?}", record);
    }
    Ok(())
}
