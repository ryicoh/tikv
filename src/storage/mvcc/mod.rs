// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

mod lock;
mod metrics;
mod reader;
mod txn;
mod write;

pub use self::lock::{Lock, LockType};
pub use self::reader::MvccReader;
pub use self::reader::{Scanner, ScannerBuilder};
pub use self::txn::{MvccTxn, MAX_TXN_WRITE_SIZE};
pub use self::write::{Write, WriteType};

use std::error;
use std::io;
use tikv_util::escape;
use tikv_util::metrics::CRITICAL_ERROR;
use tikv_util::{panic_when_unexpected_key_or_data, set_panic_mark};

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Engine(err: crate::storage::kv::Error) {
            from()
            cause(err)
            description(err.description())
        }
        Io(err: io::Error) {
            from()
            cause(err)
            description(err.description())
        }
        Codec(err: tikv_util::codec::Error) {
            from()
            cause(err)
            description(err.description())
        }
        KeyIsLocked { key: Vec<u8>, primary: Vec<u8>, ts: u64, ttl: u64, txn_size: u64 } {
            description("key is locked (backoff or cleanup)")
            display("key is locked (backoff or cleanup) {:?}-{:?}@{} ttl {} txn_size {}",
                        escape(key),
                        escape(primary),
                        ts,
                        ttl,
                        txn_size)
        }
        BadFormatLock { description("bad format lock data") }
        BadFormatWrite { description("bad format write data") }
        Committed { commit_ts: u64 } {
            description("txn already committed")
            display("txn already committed @{}", commit_ts)
        }
        PessimisticLockRollbacked { start_ts: u64, key: Vec<u8> } {
            description("pessimistic lock already rollbacked")
            display("pessimistic lock already rollbacked, start_ts:{}, key:{:?}", start_ts, escape(key))
        }
        TxnLockNotFound { start_ts: u64, commit_ts: u64, key: Vec<u8> } {
            description("txn lock not found")
            display("txn lock not found {}-{} key:{:?}", start_ts, commit_ts, escape(key))
        }
        LockTypeNotMatch { start_ts: u64, key: Vec<u8>, pessimistic: bool } {
            description("lock type not match")
            display("lock type not match, start_ts:{}, key:{:?}, pessimistic:{}", start_ts, escape(key), pessimistic)
        }
        WriteConflict { start_ts: u64, conflict_start_ts: u64, conflict_commit_ts: u64, key: Vec<u8>, primary: Vec<u8> } {
            description("write conflict")
            display("write conflict, start_ts:{}, conflict_start_ts:{}, conflict_commit_ts:{}, key:{:?}, primary:{:?}",
                    start_ts, conflict_start_ts, conflict_commit_ts, escape(key), escape(primary))
        }
        Deadlock { start_ts: u64, lock_ts: u64, key_hash: u64, deadlock_key_hash: u64 } {
            description("deadlock")
            display("deadlock occurs between txn:{} and txn:{}, key_hash:{}, deadlock_key_hash:{}",
                    start_ts, lock_ts, key_hash, deadlock_key_hash)
        }
        AlreadyExist { key: Vec<u8> } {
            description("already exists")
            display("key {:?} already exists", escape(key))
        }
        DefaultNotFound { key: Vec<u8>, write: Write } {
            description("write cf corresponding value not found in default cf")
            display("default not found: key:{:?}, write:{:?}, maybe read truncated/dropped table data?", escape(key), write)
        }
        KeyVersion { description("bad format key(version)") }
        PessimisticLockNotFound { start_ts: u64, key: Vec<u8> } {
            description("pessimistic lock not found when prewrite")
            display("pessimistic lock not found, start_ts:{}, key:{:?}", start_ts, escape(key))
        }
        Other(err: Box<dyn error::Error + Sync + Send>) {
            from()
            cause(err.as_ref())
            description(err.description())
            display("{:?}", err)
        }
    }
}

impl Error {
    pub fn maybe_clone(&self) -> Option<Error> {
        match *self {
            Error::Engine(ref e) => e.maybe_clone().map(Error::Engine),
            Error::Codec(ref e) => e.maybe_clone().map(Error::Codec),
            Error::KeyIsLocked {
                ref key,
                ref primary,
                ts,
                ttl,
                txn_size,
            } => Some(Error::KeyIsLocked {
                key: key.clone(),
                primary: primary.clone(),
                ts,
                ttl,
                txn_size,
            }),
            Error::BadFormatLock => Some(Error::BadFormatLock),
            Error::BadFormatWrite => Some(Error::BadFormatWrite),
            Error::TxnLockNotFound {
                start_ts,
                commit_ts,
                ref key,
            } => Some(Error::TxnLockNotFound {
                start_ts,
                commit_ts,
                key: key.to_owned(),
            }),
            Error::LockTypeNotMatch {
                start_ts,
                ref key,
                pessimistic,
            } => Some(Error::LockTypeNotMatch {
                start_ts,
                key: key.to_owned(),
                pessimistic,
            }),
            Error::WriteConflict {
                start_ts,
                conflict_start_ts,
                conflict_commit_ts,
                ref key,
                ref primary,
            } => Some(Error::WriteConflict {
                start_ts,
                conflict_start_ts,
                conflict_commit_ts,
                key: key.to_owned(),
                primary: primary.to_owned(),
            }),
            Error::Deadlock {
                start_ts,
                lock_ts,
                key_hash,
                deadlock_key_hash,
            } => Some(Error::Deadlock {
                start_ts,
                lock_ts,
                key_hash,
                deadlock_key_hash,
            }),
            Error::AlreadyExist { ref key } => Some(Error::AlreadyExist { key: key.clone() }),
            Error::DefaultNotFound { ref key, ref write } => Some(Error::DefaultNotFound {
                key: key.to_owned(),
                write: write.clone(),
            }),
            Error::KeyVersion => Some(Error::KeyVersion),
            Error::Committed { commit_ts } => Some(Error::Committed { commit_ts }),
            Error::PessimisticLockRollbacked { start_ts, ref key } => {
                Some(Error::PessimisticLockRollbacked {
                    start_ts,
                    key: key.to_owned(),
                })
            }
            Error::PessimisticLockNotFound { start_ts, ref key } => {
                Some(Error::PessimisticLockNotFound {
                    start_ts,
                    key: key.to_owned(),
                })
            }
            Error::Io(_) | Error::Other(_) => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Generates `DefaultNotFound` error or panic directly based on config.
pub fn default_not_found_error(key: Vec<u8>, write: Write, hint: &str) -> Error {
    CRITICAL_ERROR
        .with_label_values(&["default value not found"])
        .inc();
    if panic_when_unexpected_key_or_data() {
        set_panic_mark();
        panic!(
            "default value not found for key {:?}, write: {:?} when {}",
            hex::encode_upper(&key),
            write,
            hint,
        );
    } else {
        error!(
            "default value not found";
            "key" => log_wrappers::Key(&key),
            "write" => ?write,
            "hint" => hint,
        );
        Error::DefaultNotFound { key, write }
    }
}

#[cfg(test)]
pub mod tests {
    use kvproto::kvrpcpb::{Context, IsolationLevel};

    use crate::storage::{Engine, Key, Modify, Mutation, Options, ScanMode, Snapshot};
    use engine::CF_DEFAULT;

    use super::*;

    fn write<E: Engine>(engine: &E, ctx: &Context, modifies: Vec<Modify>) {
        if !modifies.is_empty() {
            engine.write(ctx, modifies).unwrap();
        }
    }

    pub fn must_get<E: Engine>(engine: &E, key: &[u8], ts: u64, expect: &[u8]) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        assert_eq!(
            reader.get(&Key::from_raw(key), ts).unwrap().unwrap(),
            expect
        );
    }

    pub fn must_get_rc<E: Engine>(engine: &E, key: &[u8], ts: u64, expect: &[u8]) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::RC);
        assert_eq!(
            reader.get(&Key::from_raw(key), ts).unwrap().unwrap(),
            expect
        );
    }

    pub fn must_get_none<E: Engine>(engine: &E, key: &[u8], ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        assert!(reader.get(&Key::from_raw(key), ts).unwrap().is_none());
    }

    pub fn must_get_err<E: Engine>(engine: &E, key: &[u8], ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        assert!(reader.get(&Key::from_raw(key), ts).is_err());
    }

    // Insert has a constraint that key should not exist
    pub fn try_prewrite_insert<E: Engine>(
        engine: &E,
        key: &[u8],
        value: &[u8],
        pk: &[u8],
        ts: u64,
    ) -> Result<()> {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        txn.prewrite(
            Mutation::Insert((Key::from_raw(key), value.to_vec())),
            pk,
            &Options::default(),
        )?;
        write(engine, &ctx, txn.into_modifies());
        Ok(())
    }

    fn must_prewrite_put_impl<E: Engine>(
        engine: &E,
        key: &[u8],
        value: &[u8],
        pk: &[u8],
        ts: u64,
        is_pessimistic_lock: bool,
    ) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        let options = Options::default();
        let mutation = Mutation::Put((Key::from_raw(key), value.to_vec()));
        if !is_pessimistic_lock {
            txn.prewrite(mutation, pk, &options).unwrap();
        } else {
            txn.pessimistic_prewrite(mutation, pk, true, &options)
                .unwrap();
        }
        write(engine, &ctx, txn.into_modifies());
    }

    pub fn must_prewrite_put<E: Engine>(engine: &E, key: &[u8], value: &[u8], pk: &[u8], ts: u64) {
        must_prewrite_put_impl(engine, key, value, pk, ts, false);
    }

    pub fn must_pessimistic_prewrite_put<E: Engine>(
        engine: &E,
        key: &[u8],
        value: &[u8],
        pk: &[u8],
        ts: u64,
    ) {
        must_prewrite_put_impl(engine, key, value, pk, ts, true);
    }

    fn must_prewrite_put_err_impl<E: Engine>(
        engine: &E,
        key: &[u8],
        value: &[u8],
        pk: &[u8],
        ts: u64,
        is_pessimistic_lock: bool,
    ) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        let options = Options::default();
        let mutation = Mutation::Put((Key::from_raw(key), value.to_vec()));
        if !is_pessimistic_lock {
            txn.prewrite(mutation, pk, &options).unwrap_err();
        } else {
            txn.pessimistic_prewrite(mutation, pk, true, &options)
                .unwrap_err();
        }
    }

    pub fn must_prewrite_put_err<E: Engine>(
        engine: &E,
        key: &[u8],
        value: &[u8],
        pk: &[u8],
        ts: u64,
    ) {
        must_prewrite_put_err_impl(engine, key, value, pk, ts, false);
    }

    pub fn must_pessimistic_prewrite_put_err<E: Engine>(
        engine: &E,
        key: &[u8],
        value: &[u8],
        pk: &[u8],
        ts: u64,
    ) {
        must_prewrite_put_err_impl(engine, key, value, pk, ts, true);
    }

    fn must_prewrite_delete_impl<E: Engine>(
        engine: &E,
        key: &[u8],
        pk: &[u8],
        ts: u64,
        is_pessimistic_lock: bool,
    ) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        let options = Options::default();
        let mutation = Mutation::Delete(Key::from_raw(key));
        if !is_pessimistic_lock {
            txn.prewrite(mutation, pk, &options).unwrap();
        } else {
            txn.pessimistic_prewrite(mutation, pk, true, &options)
                .unwrap();
        }
        engine.write(&ctx, txn.into_modifies()).unwrap();
    }

    pub fn must_prewrite_delete<E: Engine>(engine: &E, key: &[u8], pk: &[u8], ts: u64) {
        must_prewrite_delete_impl(engine, key, pk, ts, false);
    }

    pub fn must_pessimistic_prewrite_delete<E: Engine>(engine: &E, key: &[u8], pk: &[u8], ts: u64) {
        must_prewrite_delete_impl(engine, key, pk, ts, true);
    }

    pub fn must_prewrite_lock<E: Engine>(engine: &E, key: &[u8], pk: &[u8], ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        txn.prewrite(Mutation::Lock(Key::from_raw(key)), pk, &Options::default())
            .unwrap();
        engine.write(&ctx, txn.into_modifies()).unwrap();
    }

    pub fn must_prewrite_lock_err<E: Engine>(engine: &E, key: &[u8], pk: &[u8], ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        assert!(txn
            .prewrite(Mutation::Lock(Key::from_raw(key)), pk, &Options::default())
            .is_err());
    }

    pub fn must_acquire_pessimistic_lock<E: Engine>(
        engine: &E,
        key: &[u8],
        pk: &[u8],
        ts: u64,
        for_update_ts: u64,
    ) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        txn.acquire_pessimistic_lock(
            Key::from_raw(key),
            pk,
            for_update_ts,
            false,
            &Options::default(),
        )
        .unwrap();
        let modifies = txn.into_modifies();
        if !modifies.is_empty() {
            engine.write(&ctx, modifies).unwrap();
        }
    }

    pub fn must_acquire_pessimistic_lock_err<E: Engine>(
        engine: &E,
        key: &[u8],
        pk: &[u8],
        ts: u64,
        for_update_ts: u64,
    ) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, ts, true).unwrap();
        txn.acquire_pessimistic_lock(
            Key::from_raw(key),
            pk,
            for_update_ts,
            false,
            &Options::default(),
        )
        .unwrap_err();
    }

    pub fn must_commit<E: Engine>(engine: &E, key: &[u8], start_ts: u64, commit_ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, start_ts, true).unwrap();
        txn.commit(Key::from_raw(key), commit_ts).unwrap();
        write(engine, &ctx, txn.into_modifies());
    }

    pub fn must_commit_err<E: Engine>(engine: &E, key: &[u8], start_ts: u64, commit_ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, start_ts, true).unwrap();
        assert!(txn.commit(Key::from_raw(key), commit_ts).is_err());
    }

    pub fn must_rollback<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, start_ts, true).unwrap();
        txn.collapse_rollback(false);
        txn.rollback(Key::from_raw(key)).unwrap();
        write(engine, &ctx, txn.into_modifies());
    }

    pub fn must_rollback_collapsed<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, start_ts, true).unwrap();
        txn.rollback(Key::from_raw(key)).unwrap();
        write(engine, &ctx, txn.into_modifies());
    }

    pub fn must_rollback_err<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, start_ts, true).unwrap();
        assert!(txn.rollback(Key::from_raw(key)).is_err());
    }

    pub fn must_gc<E: Engine>(engine: &E, key: &[u8], safe_point: u64) {
        let ctx = Context::new();
        let snapshot = engine.snapshot(&ctx).unwrap();
        let mut txn = MvccTxn::new(snapshot, 0, true).unwrap();
        txn.gc(Key::from_raw(key), safe_point).unwrap();
        write(engine, &ctx, txn.into_modifies());
    }

    pub fn must_locked<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        let lock = reader.load_lock(&Key::from_raw(key)).unwrap().unwrap();
        assert_eq!(lock.ts, start_ts);
        assert_ne!(lock.lock_type, LockType::Pessimistic);
    }

    pub fn must_pessimistic_locked<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        let lock = reader.load_lock(&Key::from_raw(key)).unwrap().unwrap();
        assert_eq!(lock.ts, start_ts);
        assert_eq!(lock.lock_type, LockType::Pessimistic);
    }

    pub fn must_unlocked<E: Engine>(engine: &E, key: &[u8]) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        assert!(reader.load_lock(&Key::from_raw(key)).unwrap().is_none());
    }

    pub fn must_written<E: Engine>(
        engine: &E,
        key: &[u8],
        start_ts: u64,
        commit_ts: u64,
        tp: WriteType,
    ) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let k = Key::from_raw(key).append_ts(commit_ts);
        let v = snapshot.get_cf(CF_DEFAULT, &k).unwrap().unwrap();
        let write = Write::parse(&v).unwrap();
        assert_eq!(write.start_ts, start_ts);
        assert_eq!(write.write_type, tp);
    }

    pub fn must_seek_write_none<E: Engine>(engine: &E, key: &[u8], ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        assert!(reader
            .seek_write(&Key::from_raw(key), ts)
            .unwrap()
            .is_none());
    }

    pub fn must_seek_write<E: Engine>(
        engine: &E,
        key: &[u8],
        ts: u64,
        start_ts: u64,
        commit_ts: u64,
        write_type: WriteType,
    ) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        let (t, write) = reader.seek_write(&Key::from_raw(key), ts).unwrap().unwrap();
        assert_eq!(t, commit_ts);
        assert_eq!(write.start_ts, start_ts);
        assert_eq!(write.write_type, write_type);
    }

    pub fn must_reverse_seek_write_none<E: Engine>(engine: &E, key: &[u8], ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        assert!(reader
            .reverse_seek_write(&Key::from_raw(key), ts)
            .unwrap()
            .is_none());
    }

    pub fn must_reverse_seek_write<E: Engine>(
        engine: &E,
        key: &[u8],
        ts: u64,
        start_ts: u64,
        commit_ts: u64,
        write_type: WriteType,
    ) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        let (t, write) = reader
            .reverse_seek_write(&Key::from_raw(key), ts)
            .unwrap()
            .unwrap();
        assert_eq!(t, commit_ts);
        assert_eq!(write.start_ts, start_ts);
        assert_eq!(write.write_type, write_type);
    }

    pub fn must_get_commit_ts<E: Engine>(engine: &E, key: &[u8], start_ts: u64, commit_ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);
        let (ts, write_type) = reader
            .get_txn_commit_info(&Key::from_raw(key), start_ts)
            .unwrap()
            .unwrap();
        assert_ne!(write_type, WriteType::Rollback);
        assert_eq!(ts, commit_ts);
    }

    pub fn must_get_commit_ts_none<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);

        let ret = reader.get_txn_commit_info(&Key::from_raw(key), start_ts);
        assert!(ret.is_ok());
        match ret.unwrap() {
            None => {}
            Some((_, write_type)) => {
                assert_eq!(write_type, WriteType::Rollback);
            }
        }
    }

    pub fn must_get_rollback_ts<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);

        let (ts, write_type) = reader
            .get_txn_commit_info(&Key::from_raw(key), start_ts)
            .unwrap()
            .unwrap();
        assert_eq!(ts, start_ts);
        assert_eq!(write_type, WriteType::Rollback);
    }

    pub fn must_get_rollback_ts_none<E: Engine>(engine: &E, key: &[u8], start_ts: u64) {
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(snapshot, None, true, None, None, IsolationLevel::SI);

        let ret = reader
            .get_txn_commit_info(&Key::from_raw(key), start_ts)
            .unwrap();
        assert_eq!(ret, None);
    }

    pub fn must_scan_keys<E: Engine>(
        engine: &E,
        start: Option<&[u8]>,
        limit: usize,
        keys: Vec<&[u8]>,
        next_start: Option<&[u8]>,
    ) {
        let expect = (
            keys.into_iter().map(Key::from_raw).collect(),
            next_start.map(|x| Key::from_raw(x).append_ts(0)),
        );
        let snapshot = engine.snapshot(&Context::new()).unwrap();
        let mut reader = MvccReader::new(
            snapshot,
            Some(ScanMode::Mixed),
            false,
            None,
            None,
            IsolationLevel::SI,
        );
        assert_eq!(
            reader.scan_keys(start.map(Key::from_raw), limit).unwrap(),
            expect
        );
    }
}
