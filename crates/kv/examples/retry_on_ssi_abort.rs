//! Example showing how to handle SSI conflicts with retry.
//!
//! SSI (Serializable Snapshot Isolation) may abort transactions to prevent
//! serialization anomalies. When this happens, `commit()` returns
//! `Ok(CommitOutcome { aborted_for_ssi: true })`, NOT an error.
//!
//! The caller should retry the transaction.
//!
//! Run with: cargo run --example retry_on_ssi_abort

use kv::{CommitOutcome, Db, Error, Options};
use runtime::{Path, RealEnv};

/// Transfer amount from one account to another.
///
/// This function demonstrates the retry pattern for SSI conflicts.
fn transfer(db: &Db<RealEnv>, from: &[u8], to: &[u8], amount: i64) -> Result<(), Error> {
    const MAX_RETRIES: u32 = 10;
    let mut retries = 0;

    loop {
        let mut txn = db.begin();

        // Read current balances
        let from_balance = txn.get(from)?.map(|v| parse_i64(&v)).unwrap_or(0);
        let to_balance = txn.get(to)?.map(|v| parse_i64(&v)).unwrap_or(0);

        // Check sufficient funds
        if from_balance < amount {
            txn.rollback();
            return Err(Error::InvalidArgument("insufficient funds".to_string()));
        }

        // Update balances
        txn.put(from, &(from_balance - amount).to_le_bytes())?;
        txn.put(to, &(to_balance + amount).to_le_bytes())?;

        // Try to commit
        match txn.commit()? {
            CommitOutcome {
                aborted_for_ssi: false,
                commit_ts,
            } => {
                println!("Transfer committed at ts={}", commit_ts);
                return Ok(());
            }
            CommitOutcome {
                aborted_for_ssi: true,
                ..
            } => {
                retries += 1;
                if retries >= MAX_RETRIES {
                    return Err(Error::InvalidArgument("too many SSI retries".to_string()));
                }
                println!("SSI conflict, retrying... (attempt {})", retries);
                // In a real application, add exponential backoff here
                continue;
            }
        }
    }
}

fn parse_i64(bytes: &[u8]) -> i64 {
    if bytes.len() == 8 {
        i64::from_le_bytes(bytes.try_into().unwrap())
    } else {
        0
    }
}

fn main() -> Result<(), Error> {
    let env = RealEnv::new();
    let db = Db::open(
        env,
        Path::new("/tmp/crackeddb_transfer"),
        Options::default(),
    )?;

    // Initialize accounts
    {
        let mut txn = db.begin();
        txn.put(b"account:alice", &1000i64.to_le_bytes())?;
        txn.put(b"account:bob", &500i64.to_le_bytes())?;
        txn.commit()?;
    }

    // Perform a transfer
    transfer(&db, b"account:alice", b"account:bob", 100)?;

    // Check final balances
    {
        let mut txn = db.begin();
        let alice = txn
            .get(b"account:alice")?
            .map(|v| parse_i64(&v))
            .unwrap_or(0);
        let bob = txn.get(b"account:bob")?.map(|v| parse_i64(&v)).unwrap_or(0);
        println!("Alice: {}, Bob: {}", alice, bob);
        txn.rollback();
    }

    println!("Done!");
    Ok(())
}
