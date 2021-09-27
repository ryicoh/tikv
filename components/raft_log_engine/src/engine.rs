// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::fs;
use std::io::{Read, Result as IoResult, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use encryption::{CrypterReader, CrypterWriter, DataKeyManager};
use engine_traits::{
    CacheStats, RaftEngine, RaftEngineReadOnly, RaftLogBatch as RaftLogBatchTrait, Result,
};
use file_system::{IOOp, IORateLimiter, IOType};
use kvproto::raft_serverpb::RaftLocalState;
use raft::eraftpb::Entry;
use raft_engine::{
    Command, Engine as RawRaftEngine, Error as RaftEngineError, FileBuilder, LogBatch, MessageExt,
};

pub use raft_engine::{Config as RaftEngineConfig, RecoveryMode};

#[derive(Clone)]
pub struct MessageExtTyped;

impl MessageExt for MessageExtTyped {
    type Entry = Entry;

    fn index(e: &Entry) -> u64 {
        e.index
    }
}

struct ManagedReader<R: Seek + Read> {
    raw: Option<R>,
    decrypter: Option<CrypterReader<R>>,
    rate_limiter: Option<Arc<IORateLimiter>>,
}

impl<R: Seek + Read> Seek for ManagedReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        match (&mut self.raw, &mut self.decrypter) {
            (Some(ref mut reader), None) => reader.seek(pos),
            (None, Some(ref mut reader)) => reader.seek(pos),
            _ => unreachable!(),
        }
    }
}

impl<R: Seek + Read> Read for ManagedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let mut size = buf.len();
        if let Some(ref mut limiter) = self.rate_limiter {
            size = limiter.request(IOType::ForegroundRead, IOOp::Read, size);
        }
        match (&mut self.raw, &mut self.decrypter) {
            (Some(ref mut reader), None) => reader.read(&mut buf[..size]),
            (None, Some(ref mut reader)) => reader.read(&mut buf[..size]),
            _ => unreachable!(),
        }
    }
}

struct ManagedWriter<W: Seek + Write> {
    raw: Option<W>,
    encrypter: Option<CrypterWriter<W>>,
    rate_limiter: Option<Arc<IORateLimiter>>,
}

impl<W: Seek + Write> Seek for ManagedWriter<W> {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        match (&mut self.raw, &mut self.encrypter) {
            (Some(ref mut writer), None) => writer.seek(pos),
            (None, Some(ref mut writer)) => writer.seek(pos),
            _ => unreachable!(),
        }
    }
}

impl<W: Seek + Write> Write for ManagedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        let mut size = buf.len();
        if let Some(ref mut limiter) = self.rate_limiter {
            size = limiter.request(IOType::ForegroundWrite, IOOp::Write, size);
        }
        match (&mut self.raw, &mut self.encrypter) {
            (Some(ref mut writer), None) => writer.write(&buf[..size]),
            (None, Some(ref mut writer)) => writer.write(&buf[..size]),
            _ => unreachable!(),
        }
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

struct ManagedFileBuilder {
    key_manager: Option<Arc<DataKeyManager>>,
    rate_limiter: Option<Arc<IORateLimiter>>,
}

impl ManagedFileBuilder {
    fn new(
        key_manager: Option<Arc<DataKeyManager>>,
        rate_limiter: Option<Arc<IORateLimiter>>,
    ) -> Self {
        Self {
            key_manager,
            rate_limiter,
        }
    }
}

impl FileBuilder for ManagedFileBuilder {
    type Reader<R: Seek + Read + Send> = ManagedReader<R>;
    type Writer<W: Seek + Write + Send> = ManagedWriter<W>;

    fn build_reader<R>(&self, path: &Path, reader: R) -> IoResult<Self::Reader<R>>
    where
        R: Seek + Read + Send,
    {
        if let Some(ref key_manager) = self.key_manager {
            Ok(ManagedReader {
                raw: None,
                decrypter: Some(key_manager.open_file_with_reader(path, reader)?),
                rate_limiter: self.rate_limiter.clone(),
            })
        } else {
            Ok(ManagedReader {
                raw: Some(reader),
                decrypter: None,
                rate_limiter: self.rate_limiter.clone(),
            })
        }
    }

    fn build_writer<W>(&self, path: &Path, writer: W, create: bool) -> IoResult<Self::Writer<W>>
    where
        W: Seek + Write + Send,
    {
        if let Some(ref key_manager) = self.key_manager {
            Ok(ManagedWriter {
                raw: None,
                encrypter: Some(key_manager.open_file_with_writer(path, writer, create)?),
                rate_limiter: self.rate_limiter.clone(),
            })
        } else {
            Ok(ManagedWriter {
                raw: Some(writer),
                encrypter: None,
                rate_limiter: self.rate_limiter.clone(),
            })
        }
    }
}

#[derive(Clone)]
pub struct RaftLogEngine(Arc<RawRaftEngine<MessageExtTyped, ManagedFileBuilder>>);

impl RaftLogEngine {
    pub fn new(
        config: RaftEngineConfig,
        key_manager: Option<Arc<DataKeyManager>>,
        rate_limiter: Option<Arc<IORateLimiter>>,
    ) -> Result<Self> {
        let file_builder = Arc::new(ManagedFileBuilder::new(key_manager, rate_limiter));
        Ok(RaftLogEngine(Arc::new(
            RawRaftEngine::open_with_file_builder(config, file_builder).map_err(transfer_error)?,
        )))
    }

    /// If path is not an empty directory, we say db exists.
    pub fn exists(path: &str) -> bool {
        let path = Path::new(path);
        if !path.exists() || !path.is_dir() {
            return false;
        }
        fs::read_dir(&path).unwrap().next().is_some()
    }

    pub fn raft_groups(&self) -> Vec<u64> {
        self.0.raft_groups()
    }

    pub fn first_index(&self, raft_id: u64) -> Option<u64> {
        self.0.first_index(raft_id)
    }

    pub fn last_index(&self, raft_id: u64) -> Option<u64> {
        self.0.last_index(raft_id)
    }
}

#[derive(Default)]
pub struct RaftLogBatch(LogBatch);

const RAFT_LOG_STATE_KEY: &[u8] = b"R";

impl RaftLogBatchTrait for RaftLogBatch {
    fn append(&mut self, raft_group_id: u64, entries: Vec<Entry>) -> Result<()> {
        self.0
            .add_entries::<MessageExtTyped>(raft_group_id, &entries)
            .map_err(transfer_error)
    }

    fn cut_logs(&mut self, _: u64, _: u64, _: u64) {
        // It's unnecessary because overlapped entries can be handled in `append`.
    }

    fn put_raft_state(&mut self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        self.0
            .put_message(raft_group_id, RAFT_LOG_STATE_KEY.to_vec(), state)
            .map_err(transfer_error)
    }

    fn persist_size(&self) -> usize {
        panic!("persist_size is not implemented for raft engine");
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn merge(&mut self, _: Self) {
        panic!("merge is not implemented for raft engine");
    }
}

impl RaftEngineReadOnly for RaftLogEngine {
    fn get_raft_state(&self, raft_group_id: u64) -> Result<Option<RaftLocalState>> {
        self.0
            .get_message(raft_group_id, RAFT_LOG_STATE_KEY)
            .map_err(transfer_error)
    }

    fn get_entry(&self, raft_group_id: u64, index: u64) -> Result<Option<Entry>> {
        self.0
            .get_entry(raft_group_id, index)
            .map_err(transfer_error)
    }

    fn fetch_entries_to(
        &self,
        raft_group_id: u64,
        begin: u64,
        end: u64,
        max_size: Option<usize>,
        to: &mut Vec<Entry>,
    ) -> Result<usize> {
        self.0
            .fetch_entries_to(raft_group_id, begin, end, max_size, to)
            .map_err(transfer_error)
    }
}

impl RaftEngine for RaftLogEngine {
    type LogBatch = RaftLogBatch;

    fn log_batch(&self, _capacity: usize) -> Self::LogBatch {
        RaftLogBatch::default()
    }

    fn sync(&self) -> Result<()> {
        self.0.sync().map_err(transfer_error)
    }

    fn consume(&self, batch: &mut Self::LogBatch, sync: bool) -> Result<usize> {
        self.0.write(&mut batch.0, sync).map_err(transfer_error)
    }

    fn consume_and_shrink(
        &self,
        batch: &mut Self::LogBatch,
        sync: bool,
        _: usize,
        _: usize,
    ) -> Result<usize> {
        self.0.write(&mut batch.0, sync).map_err(transfer_error)
    }

    fn clean(
        &self,
        raft_group_id: u64,
        _: &RaftLocalState,
        batch: &mut RaftLogBatch,
    ) -> Result<()> {
        batch.0.add_command(raft_group_id, Command::Clean);
        Ok(())
    }

    fn append(&self, raft_group_id: u64, entries: Vec<Entry>) -> Result<usize> {
        let mut batch = Self::LogBatch::default();
        batch
            .0
            .add_entries::<MessageExtTyped>(raft_group_id, &entries)
            .map_err(transfer_error)?;
        self.0.write(&mut batch.0, false).map_err(transfer_error)
    }

    fn put_raft_state(&self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        self.0
            .put_message(raft_group_id, RAFT_LOG_STATE_KEY, state)
            .map_err(transfer_error)
    }

    fn gc(&self, raft_group_id: u64, _from: u64, to: u64) -> Result<usize> {
        Ok(self.0.compact_to(raft_group_id, to) as usize)
    }

    fn purge_expired_files(&self) -> Result<Vec<u64>> {
        self.0.purge_expired_files().map_err(transfer_error)
    }

    fn has_builtin_entry_cache(&self) -> bool {
        false
    }

    fn gc_entry_cache(&self, _raft_group_id: u64, _to: u64) {}

    /// Flush current cache stats.
    fn flush_stats(&self) -> Option<CacheStats> {
        None
    }

    fn stop(&self) {}

    fn dump_stats(&self) -> Result<String> {
        // Raft engine won't dump anything.
        Ok("".to_owned())
    }

    fn get_engine_size(&self) -> Result<u64> {
        //TODO impl this when RaftLogEngine is ready to go online.
        Ok(0)
    }
}

fn transfer_error(e: RaftEngineError) -> engine_traits::Error {
    match e {
        RaftEngineError::StorageCompacted => engine_traits::Error::EntriesCompacted,
        RaftEngineError::StorageUnavailable => engine_traits::Error::EntriesUnavailable,
        e => {
            let e = box_err!(e);
            engine_traits::Error::Other(e)
        }
    }
}
