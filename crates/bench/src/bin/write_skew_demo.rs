//! Two concurrent transactions, two keys, one invariant.
//!
//! T1 and T2 run in separate threads. Both read alice and bob. Both barrier.
//! T1 writes alice, T2 writes bob. Both barrier. Both commit. If the database
//! is actually serializable across read-sets, one aborts.

use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;

fn main() {
    println!("write skew demo (concurrent)");
    println!("----------------------------");
    println!("initial: alice=on, bob=on");
    println!("T1 (thread A): read both, write alice=off");
    println!("T2 (thread B): read both, write bob=off");
    println!("invariant: at least one stays on");
    println!();

    test_crackeddb();
    test_rocksdb();
}

fn check(name: &str, alice: &str, bob: &str) {
    let ok = alice == "on" || bob == "on";
    println!("{:10}  alice={:3}  bob={:3}  {}",
        name, alice, bob, if ok { "held" } else { "VIOLATED" });
}

fn s(v: Option<&[u8]>) -> String {
    v.and_then(|x| std::str::from_utf8(x).ok()).unwrap_or("?").to_string()
}

fn test_crackeddb() {
    use kv::{Db, Options};
    use runtime::{Path, RealEnv};

    let path = PathBuf::from("/tmp/ws-cd");
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();

    let db = Arc::new(Db::open(
        RealEnv::new(),
        Path::new(path.to_str().unwrap()),
        Options::default(),
    ).unwrap());

    let mut setup = db.begin();
    setup.put(b"alice", b"on").unwrap();
    setup.put(b"bob", b"on").unwrap();
    setup.commit().unwrap();

    let after_reads = Arc::new(Barrier::new(2));
    let after_writes = Arc::new(Barrier::new(2));

    let db_a = Arc::clone(&db);
    let br_a = Arc::clone(&after_reads);
    let bw_a = Arc::clone(&after_writes);
    let h1 = thread::spawn(move || {
        let mut t = db_a.begin();
        t.get(b"alice").unwrap();
        t.get(b"bob").unwrap();
        br_a.wait();
        t.put(b"alice", b"off").unwrap();
        bw_a.wait();
        t.commit().unwrap()
    });

    let db_b = Arc::clone(&db);
    let br_b = Arc::clone(&after_reads);
    let bw_b = Arc::clone(&after_writes);
    let h2 = thread::spawn(move || {
        let mut t = db_b.begin();
        t.get(b"alice").unwrap();
        t.get(b"bob").unwrap();
        br_b.wait();
        t.put(b"bob", b"off").unwrap();
        bw_b.wait();
        t.commit().unwrap()
    });

    let o1 = h1.join().unwrap();
    let o2 = h2.join().unwrap();

    let mut r = db.begin();
    let a = r.get(b"alice").unwrap();
    let b = r.get(b"bob").unwrap();
    r.rollback();

    println!("crackeddb   T1 ssi_abort={}  T2 ssi_abort={}",
        o1.aborted_for_ssi, o2.aborted_for_ssi);
    check("crackeddb", &s(a.as_deref()), &s(b.as_deref()));

    drop(db);
    let _ = std::fs::remove_dir_all(&path);
}

fn test_rocksdb() {
    use rocksdb::{Options, TransactionDB, TransactionDBOptions};

    let path = "/tmp/ws-rocks";
    let _ = std::fs::remove_dir_all(path);

    let mut opts = Options::default();
    opts.create_if_missing(true);
    let db: Arc<TransactionDB<rocksdb::MultiThreaded>> = Arc::new(TransactionDB::<rocksdb::MultiThreaded>::open(&opts, &TransactionDBOptions::default(), path).unwrap());

    let s_txn = db.transaction();
    s_txn.put(b"alice", b"on").unwrap();
    s_txn.put(b"bob", b"on").unwrap();
    s_txn.commit().unwrap();

    let after_reads = Arc::new(Barrier::new(2));
    let after_writes = Arc::new(Barrier::new(2));

    let db_a = Arc::clone(&db);
    let br_a = Arc::clone(&after_reads);
    let bw_a = Arc::clone(&after_writes);
    let h1 = thread::spawn(move || {
        let t = db_a.transaction();
        let _ = t.get(b"alice").unwrap();
        let _ = t.get(b"bob").unwrap();
        br_a.wait();
        let put_res = t.put(b"alice", b"off");
        bw_a.wait();
        match put_res {
            Ok(_) => match t.commit() {
                Ok(_) => "ok",
                Err(e) => Box::leak(format!("commit_err({:?})", e).into_boxed_str()) as &str,
            },
            Err(e) => Box::leak(format!("put_err({:?})", e).into_boxed_str()) as &str,
        }
    });

    let db_b = Arc::clone(&db);
    let br_b = Arc::clone(&after_reads);
    let bw_b = Arc::clone(&after_writes);
    let h2 = thread::spawn(move || {
        let t = db_b.transaction();
        let _ = t.get(b"alice").unwrap();
        let _ = t.get(b"bob").unwrap();
        br_b.wait();
        let put_res = t.put(b"bob", b"off");
        bw_b.wait();
        match put_res {
            Ok(_) => match t.commit() {
                Ok(_) => "ok",
                Err(e) => Box::leak(format!("commit_err({:?})", e).into_boxed_str()) as &str,
            },
            Err(e) => Box::leak(format!("put_err({:?})", e).into_boxed_str()) as &str,
        }
    });

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    let a = db.get(b"alice").unwrap();
    let b = db.get(b"bob").unwrap();

    println!("rocksdb     T1={}  T2={}", r1, r2);
    check("rocksdb", &s(a.as_deref()), &s(b.as_deref()));

    drop(db);
    let _ = std::fs::remove_dir_all(path);
}

fn test_sled() {
    use sled::transaction::TransactionError;

    let path = "/tmp/ws-sled";
    let _ = std::fs::remove_dir_all(path);
    let db = Arc::new(sled::open(path).unwrap());

    db.insert(b"alice", b"on").unwrap();
    db.insert(b"bob", b"on").unwrap();


    let after_reads = Arc::new(Barrier::new(2));
    let after_writes = Arc::new(Barrier::new(2));

    let db_a = Arc::clone(&db);
    let br_a = Arc::clone(&after_reads);
    let bw_a = Arc::clone(&after_writes);
    let h1 = thread::spawn(move || -> Result<(), TransactionError> {
        let br = br_a.clone();
        let bw = bw_a.clone();
        db_a.transaction(|tx| {
            tx.get(b"alice")?;
            tx.get(b"bob")?;
            br.wait();
            tx.insert(b"alice", b"off")?;
            bw.wait();
            Ok(())
        })
    });

    let db_b = Arc::clone(&db);
    let br_b = Arc::clone(&after_reads);
    let bw_b = Arc::clone(&after_writes);
    let h2 = thread::spawn(move || -> Result<(), TransactionError> {
        let br = br_b.clone();
        let bw = bw_b.clone();
        db_b.transaction(|tx| {
            tx.get(b"alice")?;
            tx.get(b"bob")?;
            br.wait();
            tx.insert(b"bob", b"off")?;
            bw.wait();
            Ok(())
        })
    });

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    let a = db.get(b"alice").unwrap().map(|v| v.to_vec());
    let b = db.get(b"bob").unwrap().map(|v| v.to_vec());

    println!("sled        T1={}  T2={}",
        if r1.is_ok() { "ok" } else { "abort" },
        if r2.is_ok() { "ok" } else { "abort" });
    check("sled", &s(a.as_deref()), &s(b.as_deref()));

    drop(db);
    let _ = std::fs::remove_dir_all(path);
}
