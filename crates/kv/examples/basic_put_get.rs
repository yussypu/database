//! Basic put/get example.
//!
//! Run with: cargo run --example basic_put_get

use kv::{Db, Options};
use runtime::{Path, RealEnv};

fn main() -> Result<(), kv::Error> {
    // Create the runtime environment. RealEnv uses actual filesystem and time.
    // For deterministic testing, use SimEnv instead.
    let env = RealEnv::new();

    // Open (or create) the database
    let db = Db::open(env, Path::new("/tmp/crackeddb_example"), Options::default())?;

    // Write some data in a transaction
    {
        let mut txn = db.begin();
        txn.put(b"user:1", b"alice")?;
        txn.put(b"user:2", b"bob")?;
        txn.put(b"counter", b"42")?;

        let outcome = txn.commit()?;
        println!("Committed at timestamp {}", outcome.commit_ts);
    }

    // Read the data back
    {
        let mut txn = db.begin();

        if let Some(value) = txn.get(b"user:1")? {
            println!("user:1 = {}", String::from_utf8_lossy(&value));
        }

        if let Some(value) = txn.get(b"user:2")? {
            println!("user:2 = {}", String::from_utf8_lossy(&value));
        }

        if let Some(value) = txn.get(b"counter")? {
            println!("counter = {}", String::from_utf8_lossy(&value));
        }

        // Read-only transaction, just rollback
        txn.rollback();
    }

    // Delete a key
    {
        let mut txn = db.begin();
        txn.delete(b"user:2")?;
        txn.commit()?;
    }

    // Verify deletion
    {
        let mut txn = db.begin();
        match txn.get(b"user:2")? {
            Some(_) => println!("user:2 still exists (unexpected)"),
            None => println!("user:2 deleted successfully"),
        }
        txn.rollback();
    }

    // Demonstrate scan
    {
        let mut txn = db.begin();
        txn.put(b"a", b"1")?;
        txn.put(b"b", b"2")?;
        txn.put(b"c", b"3")?;
        txn.commit()?;
    }

    {
        let mut txn = db.begin();
        println!("Scanning a..z:");
        for entry in txn.scan(b"a".to_vec()..b"z".to_vec())? {
            let (k, v) = entry?;
            println!(
                "  {} = {}",
                std::str::from_utf8(&k).unwrap_or("<binary>"),
                std::str::from_utf8(&v).unwrap_or("<binary>")
            );
        }
        txn.rollback();
    }

    println!("Done!");
    Ok(())
}
