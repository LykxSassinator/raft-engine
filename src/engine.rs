// Copyright (c) 2017-present, PingCAP, Inc. Licensed under Apache-2.0.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{Builder as ThreadBuilder, JoinHandle};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use protobuf::{parse_from_bytes, Message};

use crate::config::{Config, RecoveryMode};
use crate::consistency::ConsistencyChecker;
use crate::env::{DefaultFileSystem, FileSystem};
use crate::event_listener::EventListener;
use crate::file_pipe_log::debug::LogItemReader;
use crate::file_pipe_log::{DefaultMachineFactory, FilePipeLog, FilePipeLogBuilder};
use crate::log_batch::{Command, LogBatch, MessageExt};
use crate::memtable::{EntryIndex, MemTableRecoverContextFactory, MemTables};
use crate::metrics::*;
use crate::pipe_log::{FileBlockHandle, FileId, LogQueue, PipeLog};
use crate::purge::{PurgeHook, PurgeManager};
use crate::write_barrier::{WriteBarrier, Writer};
use crate::{perf_context, Error, GlobalStats, Result};

const METRICS_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

pub struct Engine<F = DefaultFileSystem, P = FilePipeLog<F>>
where
    F: FileSystem,
    P: PipeLog,
{
    cfg: Arc<Config>,
    listeners: Vec<Arc<dyn EventListener>>,

    #[allow(dead_code)]
    stats: Arc<GlobalStats>,
    memtables: MemTables,
    pipe_log: Arc<P>,
    purge_manager: PurgeManager<P>,

    write_barrier: WriteBarrier<LogBatch, Result<FileBlockHandle>>,

    tx: Mutex<mpsc::Sender<()>>,
    metrics_flusher: Option<JoinHandle<()>>,

    _phantom: PhantomData<F>,
}

impl Engine<DefaultFileSystem, FilePipeLog<DefaultFileSystem>> {
    pub fn open(cfg: Config) -> Result<Engine<DefaultFileSystem, FilePipeLog<DefaultFileSystem>>> {
        Self::open_with_listeners(cfg, vec![])
    }

    pub fn open_with_listeners(
        cfg: Config,
        listeners: Vec<Arc<dyn EventListener>>,
    ) -> Result<Engine<DefaultFileSystem, FilePipeLog<DefaultFileSystem>>> {
        Self::open_with(cfg, Arc::new(DefaultFileSystem), listeners)
    }
}

impl<F> Engine<F, FilePipeLog<F>>
where
    F: FileSystem,
{
    pub fn open_with_file_system(
        cfg: Config,
        file_system: Arc<F>,
    ) -> Result<Engine<F, FilePipeLog<F>>> {
        Self::open_with(cfg, file_system, vec![])
    }

    pub fn open_with(
        mut cfg: Config,
        file_system: Arc<F>,
        mut listeners: Vec<Arc<dyn EventListener>>,
    ) -> Result<Engine<F, FilePipeLog<F>>> {
        cfg.sanitize()?;
        listeners.push(Arc::new(PurgeHook::new()) as Arc<dyn EventListener>);

        let start = Instant::now();
        let mut builder = FilePipeLogBuilder::new(cfg.clone(), file_system, listeners.clone());
        builder.scan()?;
        let factory = MemTableRecoverContextFactory::new(&cfg);
        let (append, rewrite) = builder.recover(&factory)?;
        let pipe_log = Arc::new(builder.finish()?);
        rewrite.merge_append_context(append);
        let (memtables, stats) = rewrite.finish();
        info!("Recovering raft logs takes {:?}", start.elapsed());

        let cfg = Arc::new(cfg);
        let purge_manager = PurgeManager::new(
            cfg.clone(),
            memtables.clone(),
            pipe_log.clone(),
            stats.clone(),
            listeners.clone(),
        );

        let (tx, rx) = mpsc::channel();
        let stats_clone = stats.clone();
        let memtables_clone = memtables.clone();
        let metrics_flusher = ThreadBuilder::new()
            .name("re-metrics".into())
            .spawn(move || loop {
                stats_clone.flush_metrics();
                memtables_clone.flush_metrics();
                if rx.recv_timeout(METRICS_FLUSH_INTERVAL).is_ok() {
                    break;
                }
            })?;

        Ok(Self {
            cfg,
            listeners,
            stats,
            memtables,
            pipe_log,
            purge_manager,
            write_barrier: Default::default(),
            tx: Mutex::new(tx),
            metrics_flusher: Some(metrics_flusher),
            _phantom: PhantomData,
        })
    }
}

impl<F, P> Engine<F, P>
where
    F: FileSystem,
    P: PipeLog,
{
    /// Writes the content of `log_batch` into the engine and returns written
    /// bytes. If `sync` is true, the write will be followed by a call to
    /// `fdatasync` on the log file.
    pub fn write(&self, log_batch: &mut LogBatch, mut sync: bool) -> Result<usize> {
        if log_batch.is_empty() {
            return Ok(0);
        }
        let start = Instant::now();
        let len = log_batch.finish_populate(self.cfg.batch_compression_threshold.0 as usize)?;
        debug_assert!(len > 0);
        let block_handle = {
            // Max retry count is limited to `2`. If the first `append` retry because of
            // `NOSPC` error, the next `append` should success, unless there exists
            // several abnormal cases in the IO device. In that case,
            // `Engine::write` must return `Err`.
            let mut retry_count = 2;
            let mut handle = Ok(FileBlockHandle {
                id: FileId::new(LogQueue::Append, 0),
                offset: 0,
                len: 0,
            });
            while retry_count > 0 {
                let mut writer = Writer::new(log_batch, sync);
                // Snapshot and clear the current perf context temporarily, so the write group
                // leader will collect the perf context diff later.
                let mut perf_context = take_perf_context();
                let before_enter = Instant::now();
                if let Some(mut group) = self.write_barrier.enter(&mut writer) {
                    let now = Instant::now();
                    let _t = StopWatch::new_with(&*ENGINE_WRITE_LEADER_DURATION_HISTOGRAM, now);
                    // Flag on whether force to rotate the current active file or not.
                    let mut force_rotate = false;
                    for writer in group.iter_mut() {
                        writer.entered_time = Some(now);
                        sync |= writer.sync;
                        // To avoid redundant `write` check when `force_rotate` == `true`, we will
                        // directly assign a special `Error::Other(...)` to the following followers.
                        if force_rotate {
                            writer.set_output(Err(Error::Other(box_err!(
                                "Failed to append logbatch, try to dump it to other dir"
                            ))));
                            continue;
                        }
                        let log_batch = writer.mut_payload();
                        let res = self.pipe_log.append(LogQueue::Append, log_batch);
                        // If we found that there is no spare space for the next LogBatch in the
                        // current active file, we will mark the `force_rotate` with `true` to
                        // notify the leader do `rotate` immediately.
                        if let Err(Error::Other(e)) = res {
                            warn!(
                                "Cannot append, err: {}, try to re-append this log_batch into next log",
                                e
                            );
                            force_rotate = true;
                            writer.set_output(Err(Error::Other(box_err!(
                                "Failed to append logbatch, try to dump it to other dir"
                            ))));
                            continue;
                        }
                        writer.set_output(res);
                    }
                    perf_context!(log_write_duration).observe_since(now);
                    if force_rotate {
                        // If the leader failed to `rotate` a new log for the un-synced LogBatches,
                        // it means that it encounters unexpected errors, and should panic.
                        if let Err(e) = self.pipe_log.rotate(LogQueue::Append) {
                            panic!(
                                "Cannot rotate {:?} queue due to IO error: {}",
                                LogQueue::Append,
                                e
                            );
                        }
                    } else if sync {
                        // As per trait protocol, this error should be retriable. But we panic
                        // anyway to save the trouble of propagating it to
                        // other group members.
                        self.pipe_log.sync(LogQueue::Append).expect("pipe::sync()");
                    }
                    // Pass the perf context diff to all the writers.
                    let diff = get_perf_context();
                    for writer in group.iter_mut() {
                        writer.perf_context_diff = diff.clone();
                    }
                }
                let entered_time = writer.entered_time.unwrap();
                ENGINE_WRITE_PREPROCESS_DURATION_HISTOGRAM
                    .observe(entered_time.saturating_duration_since(start).as_secs_f64());
                perf_context.write_wait_duration +=
                    entered_time.saturating_duration_since(before_enter);
                debug_assert_eq!(writer.perf_context_diff.write_wait_duration, Duration::ZERO);
                perf_context += &writer.perf_context_diff;
                set_perf_context(perf_context);
                // Retry if `writer.finish()` returns a special 'Error::Other', remarking that
                // there still exists free space for this `LogBatch`.
                handle = writer.finish();
                if let Err(Error::Other(_)) = handle {
                    retry_count -= 1;
                    log_batch.prepare_rewrite()?;
                    // Here, we will retry this LogBatch `append` by appending this writer
                    // to the next write group, and the current write leader will not hang
                    // on this write and will return timely.
                    continue;
                } else {
                    break;
                }
            }
            handle?
        };
        let mut now = Instant::now();
        log_batch.finish_write(block_handle);
        self.memtables.apply_append_writes(log_batch.drain());
        for listener in &self.listeners {
            listener.post_apply_memtables(block_handle.id);
        }
        let end = Instant::now();
        let apply_duration = end.saturating_duration_since(now);
        ENGINE_WRITE_APPLY_DURATION_HISTOGRAM.observe(apply_duration.as_secs_f64());
        perf_context!(apply_duration).observe(apply_duration);
        now = end;
        ENGINE_WRITE_DURATION_HISTOGRAM.observe(now.saturating_duration_since(start).as_secs_f64());
        ENGINE_WRITE_SIZE_HISTOGRAM.observe(len as f64);
        Ok(len)
    }

    /// Synchronizes the Raft engine.
    pub fn sync(&self) -> Result<()> {
        self.write(&mut LogBatch::default(), true)?;
        Ok(())
    }

    pub fn get_message<S: Message>(&self, region_id: u64, key: &[u8]) -> Result<Option<S>> {
        let _t = StopWatch::new(&*ENGINE_READ_MESSAGE_DURATION_HISTOGRAM);
        if let Some(memtable) = self.memtables.get(region_id) {
            if let Some(value) = memtable.read().get(key) {
                return Ok(Some(parse_from_bytes(&value)?));
            }
        }
        Ok(None)
    }

    pub fn get(&self, region_id: u64, key: &[u8]) -> Option<Vec<u8>> {
        let _t = StopWatch::new(&*ENGINE_READ_MESSAGE_DURATION_HISTOGRAM);
        if let Some(memtable) = self.memtables.get(region_id) {
            return memtable.read().get(key);
        }
        None
    }

    /// Iterates over [start_key, end_key) range of Raft Group key-values and
    /// yields messages of the required type. Unparsable items are skipped.
    pub fn scan_messages<S, C>(
        &self,
        region_id: u64,
        start_key: Option<&[u8]>,
        end_key: Option<&[u8]>,
        reverse: bool,
        mut callback: C,
    ) -> Result<()>
    where
        S: Message,
        C: FnMut(&[u8], S) -> bool,
    {
        self.scan_raw_messages(region_id, start_key, end_key, reverse, move |k, raw_v| {
            if let Ok(v) = parse_from_bytes(raw_v) {
                callback(k, v)
            } else {
                true
            }
        })
    }

    /// Iterates over [start_key, end_key) range of Raft Group key-values and
    /// yields all key value pairs as bytes.
    pub fn scan_raw_messages<C>(
        &self,
        region_id: u64,
        start_key: Option<&[u8]>,
        end_key: Option<&[u8]>,
        reverse: bool,
        callback: C,
    ) -> Result<()>
    where
        C: FnMut(&[u8], &[u8]) -> bool,
    {
        let _t = StopWatch::new(&*ENGINE_READ_MESSAGE_DURATION_HISTOGRAM);
        if let Some(memtable) = self.memtables.get(region_id) {
            memtable
                .read()
                .scan(start_key, end_key, reverse, callback)?;
        }
        Ok(())
    }

    pub fn get_entry<M: MessageExt>(
        &self,
        region_id: u64,
        log_idx: u64,
    ) -> Result<Option<M::Entry>> {
        let _t = StopWatch::new(&*ENGINE_READ_ENTRY_DURATION_HISTOGRAM);
        if let Some(memtable) = self.memtables.get(region_id) {
            if let Some(idx) = memtable.read().get_entry(log_idx) {
                ENGINE_READ_ENTRY_COUNT_HISTOGRAM.observe(1.0);
                return Ok(Some(read_entry_from_file::<M, _>(
                    self.pipe_log.as_ref(),
                    &idx,
                )?));
            }
        }
        Ok(None)
    }

    /// Purges expired logs files and returns a set of Raft group ids that need
    /// to be compacted.
    pub fn purge_expired_files(&self) -> Result<Vec<u64>> {
        self.purge_manager.purge_expired_files()
    }

    /// Returns count of fetched entries.
    pub fn fetch_entries_to<M: MessageExt>(
        &self,
        region_id: u64,
        begin: u64,
        end: u64,
        max_size: Option<usize>,
        vec: &mut Vec<M::Entry>,
    ) -> Result<usize> {
        let _t = StopWatch::new(&*ENGINE_READ_ENTRY_DURATION_HISTOGRAM);
        if let Some(memtable) = self.memtables.get(region_id) {
            let mut ents_idx: Vec<EntryIndex> = Vec::with_capacity((end - begin) as usize);
            memtable
                .read()
                .fetch_entries_to(begin, end, max_size, &mut ents_idx)?;
            for i in ents_idx.iter() {
                vec.push(read_entry_from_file::<M, _>(self.pipe_log.as_ref(), i)?);
            }
            ENGINE_READ_ENTRY_COUNT_HISTOGRAM.observe(ents_idx.len() as f64);
            return Ok(ents_idx.len());
        }
        Ok(0)
    }

    pub fn first_index(&self, region_id: u64) -> Option<u64> {
        if let Some(memtable) = self.memtables.get(region_id) {
            return memtable.read().first_index();
        }
        None
    }

    pub fn last_index(&self, region_id: u64) -> Option<u64> {
        if let Some(memtable) = self.memtables.get(region_id) {
            return memtable.read().last_index();
        }
        None
    }

    /// Deletes log entries before `index` in the specified Raft group. Returns
    /// the number of deleted entries.
    pub fn compact_to(&self, region_id: u64, index: u64) -> u64 {
        let first_index = match self.first_index(region_id) {
            Some(index) => index,
            None => return 0,
        };

        let mut log_batch = LogBatch::default();
        log_batch.add_command(region_id, Command::Compact { index });
        if let Err(e) = self.write(&mut log_batch, false) {
            error!("Failed to write Compact command: {}", e);
        }

        self.first_index(region_id).unwrap_or(index) - first_index
    }

    pub fn raft_groups(&self) -> Vec<u64> {
        self.memtables.fold(vec![], |mut v, m| {
            v.push(m.region_id());
            v
        })
    }

    /// Returns `true` if the engine contains no Raft Group. Empty Raft Group
    /// that isn't cleaned is counted as well.
    pub fn is_empty(&self) -> bool {
        self.memtables.is_empty()
    }

    /// Returns the sequence number range of active log files in the specific
    /// log queue.
    /// For testing only.
    pub fn file_span(&self, queue: LogQueue) -> (u64, u64) {
        self.pipe_log.file_span(queue)
    }

    pub fn get_used_size(&self) -> usize {
        self.pipe_log.total_size(LogQueue::Append) + self.pipe_log.total_size(LogQueue::Rewrite)
    }
}

impl<F, P> Drop for Engine<F, P>
where
    F: FileSystem,
    P: PipeLog,
{
    fn drop(&mut self) {
        self.tx.lock().unwrap().send(()).unwrap();
        if let Some(t) = self.metrics_flusher.take() {
            t.join().unwrap();
        }
    }
}

impl Engine<DefaultFileSystem, FilePipeLog<DefaultFileSystem>> {
    pub fn consistency_check(path: &Path) -> Result<Vec<(u64, u64)>> {
        Self::consistency_check_with_file_system(path, Arc::new(DefaultFileSystem))
    }

    #[cfg(feature = "scripting")]
    pub fn unsafe_repair(path: &Path, queue: Option<LogQueue>, script: String) -> Result<()> {
        Self::unsafe_repair_with_file_system(path, queue, script, Arc::new(DefaultFileSystem))
    }

    pub fn dump(path: &Path) -> Result<LogItemReader<DefaultFileSystem>> {
        Self::dump_with_file_system(path, Arc::new(DefaultFileSystem))
    }
}

impl<F> Engine<F, FilePipeLog<F>>
where
    F: FileSystem,
{
    /// Returns a list of corrupted Raft groups, including their ids and last
    /// valid log index. Head or tail corruption cannot be detected.
    pub fn consistency_check_with_file_system(
        path: &Path,
        file_system: Arc<F>,
    ) -> Result<Vec<(u64, u64)>> {
        if !path.exists() {
            return Err(Error::InvalidArgument(format!(
                "raft-engine directory '{}' does not exist.",
                path.to_str().unwrap()
            )));
        }

        let cfg = Config {
            dir: path.to_str().unwrap().to_owned(),
            recovery_mode: RecoveryMode::TolerateAnyCorruption,
            ..Default::default()
        };
        let mut builder = FilePipeLogBuilder::new(cfg, file_system, Vec::new());
        builder.scan()?;
        let (append, rewrite) =
            builder.recover(&DefaultMachineFactory::<ConsistencyChecker>::default())?;
        let mut map = rewrite.finish();
        for (id, index) in append.finish() {
            map.entry(id).or_insert(index);
        }
        let mut list: Vec<(u64, u64)> = map.into_iter().collect();
        list.sort_unstable();
        Ok(list)
    }

    #[cfg(feature = "scripting")]
    pub fn unsafe_repair_with_file_system(
        path: &Path,
        queue: Option<LogQueue>,
        script: String,
        file_system: Arc<F>,
    ) -> Result<()> {
        use crate::file_pipe_log::{RecoveryConfig, ReplayMachine};

        if !path.exists() {
            return Err(Error::InvalidArgument(format!(
                "raft-engine directory '{}' does not exist.",
                path.to_str().unwrap()
            )));
        }

        let cfg = Config {
            dir: path.to_str().unwrap().to_owned(),
            recovery_mode: RecoveryMode::TolerateAnyCorruption,
            ..Default::default()
        };
        let recovery_mode = cfg.recovery_mode;
        let read_block_size = cfg.recovery_read_block_size.0;
        let mut builder = FilePipeLogBuilder::new(cfg, file_system.clone(), Vec::new());
        builder.scan()?;
        let factory = crate::filter::RhaiFilterMachineFactory::from_script(script);
        let mut machine = None;
        if queue.is_none() || queue.unwrap() == LogQueue::Append {
            machine = Some(builder.recover_queue(
                file_system.clone(),
                RecoveryConfig {
                    queue: LogQueue::Append,
                    mode: recovery_mode,
                    concurrency: 1,
                    read_block_size,
                },
                &factory,
            )?);
        }
        if queue.is_none() || queue.unwrap() == LogQueue::Rewrite {
            let machine2 = builder.recover_queue(
                file_system.clone(),
                RecoveryConfig {
                    queue: LogQueue::Rewrite,
                    mode: recovery_mode,
                    concurrency: 1,
                    read_block_size,
                },
                &factory,
            )?;
            if let Some(machine) = &mut machine {
                machine.merge(machine2, LogQueue::Rewrite)?;
            }
        }
        if let Some(machine) = machine {
            machine.finish(file_system.as_ref(), path)?;
        }
        Ok(())
    }

    /// Dumps all operations.
    pub fn dump_with_file_system(path: &Path, file_system: Arc<F>) -> Result<LogItemReader<F>> {
        if !path.exists() {
            return Err(Error::InvalidArgument(format!(
                "raft-engine directory or file '{}' does not exist.",
                path.to_str().unwrap()
            )));
        }

        if path.is_dir() {
            LogItemReader::new_directory_reader(file_system, path)
        } else {
            LogItemReader::new_file_reader(file_system, path)
        }
    }
}

struct BlockCache {
    key: Cell<FileBlockHandle>,
    block: RefCell<Vec<u8>>,
}

impl BlockCache {
    fn new() -> Self {
        BlockCache {
            key: Cell::new(FileBlockHandle {
                id: FileId::new(LogQueue::Append, 0),
                offset: 0,
                len: 0,
            }),
            block: RefCell::new(Vec::new()),
        }
    }

    fn insert(&self, key: FileBlockHandle, block: Vec<u8>) {
        self.key.set(key);
        self.block.replace(block);
    }
}

thread_local! {
    static BLOCK_CACHE: BlockCache = BlockCache::new();
}

pub(crate) fn read_entry_from_file<M, P>(pipe_log: &P, idx: &EntryIndex) -> Result<M::Entry>
where
    M: MessageExt,
    P: PipeLog,
{
    BLOCK_CACHE.with(|cache| {
        if cache.key.get() != idx.entries.unwrap() {
            cache.insert(
                idx.entries.unwrap(),
                LogBatch::decode_entries_block(
                    &pipe_log.read_bytes(idx.entries.unwrap())?,
                    idx.entries.unwrap(),
                    idx.compression_type,
                )?,
            );
        }
        let e = parse_from_bytes(
            &cache.block.borrow()
                [idx.entry_offset as usize..(idx.entry_offset + idx.entry_len) as usize],
        )?;
        assert_eq!(M::index(&e), idx.index);
        Ok(e)
    })
}

pub(crate) fn read_entry_bytes_from_file<P>(pipe_log: &P, idx: &EntryIndex) -> Result<Vec<u8>>
where
    P: PipeLog,
{
    BLOCK_CACHE.with(|cache| {
        if cache.key.get() != idx.entries.unwrap() {
            cache.insert(
                idx.entries.unwrap(),
                LogBatch::decode_entries_block(
                    &pipe_log.read_bytes(idx.entries.unwrap())?,
                    idx.entries.unwrap(),
                    idx.compression_type,
                )?,
            );
        }
        Ok(cache.block.borrow()
            [idx.entry_offset as usize..(idx.entry_offset + idx.entry_len) as usize]
            .to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ObfuscatedFileSystem;
    use crate::file_pipe_log::FileNameExt;
    use crate::pipe_log::Version;
    use crate::test_util::{generate_entries, PanicGuard};
    use crate::util::ReadableSize;
    use kvproto::raft_serverpb::RaftLocalState;
    use raft::eraftpb::Entry;
    use std::collections::BTreeSet;
    use std::fs::OpenOptions;
    use std::path::PathBuf;

    type RaftLogEngine<F = DefaultFileSystem> = Engine<F>;
    impl<F: FileSystem> RaftLogEngine<F> {
        fn append(&self, rid: u64, start_index: u64, end_index: u64, data: Option<&[u8]>) {
            let entries = generate_entries(start_index, end_index, data);
            if !entries.is_empty() {
                let mut batch = LogBatch::default();
                batch.add_entries::<Entry>(rid, &entries).unwrap();
                batch
                    .put_message(
                        rid,
                        b"last_index".to_vec(),
                        &RaftLocalState {
                            last_index: entries[entries.len() - 1].index,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                self.write(&mut batch, true).unwrap();
            }
        }

        fn clean(&self, rid: u64) {
            let mut log_batch = LogBatch::default();
            log_batch.add_command(rid, Command::Clean);
            self.write(&mut log_batch, true).unwrap();
        }

        fn decode_last_index(&self, rid: u64) -> Option<u64> {
            self.get_message::<RaftLocalState>(rid, b"last_index")
                .unwrap()
                .map(|s| s.last_index)
        }

        fn reopen(self) -> Self {
            let cfg: Config = self.cfg.as_ref().clone();
            let file_system = self.pipe_log.file_system();
            let mut listeners = self.listeners.clone();
            listeners.pop();
            drop(self);
            RaftLogEngine::open_with(cfg, file_system, listeners).unwrap()
        }

        fn scan_entries<FR: Fn(u64, LogQueue, &[u8])>(
            &self,
            rid: u64,
            start: u64,
            end: u64,
            reader: FR,
        ) {
            let mut entries = Vec::new();
            self.fetch_entries_to::<Entry>(
                rid,
                self.first_index(rid).unwrap(),
                self.last_index(rid).unwrap() + 1,
                None,
                &mut entries,
            )
            .unwrap();
            assert_eq!(entries.len(), (end - start) as usize);
            assert_eq!(entries.first().unwrap().index, start);
            assert_eq!(
                entries.last().unwrap().index,
                self.decode_last_index(rid).unwrap()
            );
            assert_eq!(entries.last().unwrap().index + 1, end);
            for e in entries.iter() {
                let entry_index = self
                    .memtables
                    .get(rid)
                    .unwrap()
                    .read()
                    .get_entry(e.index)
                    .unwrap();
                assert_eq!(&self.get_entry::<Entry>(rid, e.index).unwrap().unwrap(), e);
                reader(e.index, entry_index.entries.unwrap().id.queue, &e.data);
            }
        }

        fn file_count(&self, queue: Option<LogQueue>) -> usize {
            if let Some(queue) = queue {
                let (a, b) = self.file_span(queue);
                (b - a + 1) as usize
            } else {
                self.file_count(Some(LogQueue::Append)) + self.file_count(Some(LogQueue::Rewrite))
            }
        }
    }

    #[test]
    fn test_empty_engine() {
        let dir = tempfile::Builder::new()
            .prefix("test_empty_engine")
            .tempdir()
            .unwrap();
        let mut sub_dir = PathBuf::from(dir.as_ref());
        sub_dir.push("raft-engine");
        let cfg = Config {
            dir: sub_dir.to_str().unwrap().to_owned(),
            ..Default::default()
        };
        RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
            .unwrap();
    }

    #[test]
    fn test_get_entry() {
        let normal_batch_size = 10;
        let compressed_batch_size = 5120;
        for &entry_size in &[normal_batch_size, compressed_batch_size] {
            let dir = tempfile::Builder::new()
                .prefix("test_get_entry")
                .tempdir()
                .unwrap();
            let cfg = Config {
                dir: dir.path().to_str().unwrap().to_owned(),
                target_file_size: ReadableSize(1),
                ..Default::default()
            };

            let engine = RaftLogEngine::open_with_file_system(
                cfg.clone(),
                Arc::new(ObfuscatedFileSystem::default()),
            )
            .unwrap();
            let data = vec![b'x'; entry_size];
            for i in 10..20 {
                let rid = i;
                let index = i;
                engine.append(rid, index, index + 2, Some(&data));
            }
            for i in 10..20 {
                let rid = i;
                let index = i;
                engine.scan_entries(rid, index, index + 2, |_, q, d| {
                    assert_eq!(q, LogQueue::Append);
                    assert_eq!(d, &data);
                });
            }

            // Recover the engine.
            let engine = engine.reopen();
            for i in 10..20 {
                let rid = i;
                let index = i;
                engine.scan_entries(rid, index, index + 2, |_, q, d| {
                    assert_eq!(q, LogQueue::Append);
                    assert_eq!(d, &data);
                });
            }
        }
    }

    #[test]
    fn test_clean_raft_group() {
        fn run_steps(steps: &[Option<(u64, u64)>]) {
            let rid = 1;
            let data = vec![b'x'; 1024];

            for rewrite_step in 1..=steps.len() {
                for exit_purge in [None, Some(1), Some(2)] {
                    let _guard = PanicGuard::with_prompt(format!(
                        "case: [{:?}, {}, {:?}]",
                        steps, rewrite_step, exit_purge
                    ));
                    let dir = tempfile::Builder::new()
                        .prefix("test_clean_raft_group")
                        .tempdir()
                        .unwrap();
                    let cfg = Config {
                        dir: dir.path().to_str().unwrap().to_owned(),
                        target_file_size: ReadableSize(1),
                        ..Default::default()
                    };
                    let engine = RaftLogEngine::open_with_file_system(
                        cfg.clone(),
                        Arc::new(ObfuscatedFileSystem::default()),
                    )
                    .unwrap();

                    for (i, step) in steps.iter().enumerate() {
                        if let Some((start, end)) = *step {
                            engine.append(rid, start, end, Some(&data));
                        } else {
                            engine.clean(rid);
                        }
                        if i + 1 == rewrite_step {
                            engine
                                .purge_manager
                                .must_rewrite_append_queue(None, exit_purge);
                        }
                    }

                    let engine = engine.reopen();
                    if let Some((start, end)) = *steps.last().unwrap() {
                        engine.scan_entries(rid, start, end, |_, _, d| {
                            assert_eq!(d, &data);
                        });
                    } else {
                        assert!(engine.raft_groups().is_empty());
                    }

                    engine.purge_manager.must_rewrite_append_queue(None, None);
                    let engine = engine.reopen();
                    if let Some((start, end)) = *steps.last().unwrap() {
                        engine.scan_entries(rid, start, end, |_, _, d| {
                            assert_eq!(d, &data);
                        });
                    } else {
                        assert!(engine.raft_groups().is_empty());
                    }
                }
            }
        }

        run_steps(&[Some((1, 5)), None, Some((2, 6)), None, Some((3, 7)), None]);
        run_steps(&[Some((1, 5)), None, Some((2, 6)), None, Some((3, 7))]);
        run_steps(&[Some((1, 5)), None, Some((2, 6)), None]);
        run_steps(&[Some((1, 5)), None, Some((2, 6))]);
        run_steps(&[Some((1, 5)), None]);
    }

    #[test]
    fn test_key_value_scan() {
        fn key(i: u64) -> Vec<u8> {
            format!("k{}", i).as_bytes().to_vec()
        }
        fn value(i: u64) -> Vec<u8> {
            format!("v{}", i).as_bytes().to_vec()
        }
        fn rich_value(i: u64) -> RaftLocalState {
            RaftLocalState {
                last_index: i,
                ..Default::default()
            }
        }

        let dir = tempfile::Builder::new()
            .prefix("test_key_value_scan")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            ..Default::default()
        };
        let rid = 1;
        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();

        engine
            .scan_messages::<RaftLocalState, _>(rid, None, None, false, |_, _| {
                panic!("unexpected message.");
            })
            .unwrap();

        let mut batch = LogBatch::default();
        let mut res = Vec::new();
        let mut rich_res = Vec::new();
        batch.put(rid, key(1), value(1));
        batch.put(rid, key(2), value(2));
        batch.put(rid, key(3), value(3));
        engine.write(&mut batch, false).unwrap();

        engine
            .scan_raw_messages(rid, None, None, false, |k, v| {
                res.push((k.to_vec(), v.to_vec()));
                true
            })
            .unwrap();
        assert_eq!(
            res,
            vec![(key(1), value(1)), (key(2), value(2)), (key(3), value(3))]
        );
        res.clear();
        engine
            .scan_raw_messages(rid, None, None, true, |k, v| {
                res.push((k.to_vec(), v.to_vec()));
                true
            })
            .unwrap();
        assert_eq!(
            res,
            vec![(key(3), value(3)), (key(2), value(2)), (key(1), value(1))]
        );
        res.clear();
        engine
            .scan_messages::<RaftLocalState, _>(rid, None, None, false, |_, _| {
                panic!("unexpected message.")
            })
            .unwrap();

        batch.put_message(rid, key(22), &rich_value(22)).unwrap();
        batch.put_message(rid, key(33), &rich_value(33)).unwrap();
        engine.write(&mut batch, false).unwrap();

        engine
            .scan_messages(rid, None, None, false, |k, v| {
                rich_res.push((k.to_vec(), v));
                false
            })
            .unwrap();
        assert_eq!(rich_res, vec![(key(22), rich_value(22))]);
        rich_res.clear();
        engine
            .scan_messages(rid, None, None, true, |k, v| {
                rich_res.push((k.to_vec(), v));
                false
            })
            .unwrap();
        assert_eq!(rich_res, vec![(key(33), rich_value(33))]);
        rich_res.clear();
    }

    #[test]
    fn test_delete_key_value() {
        let dir = tempfile::Builder::new()
            .prefix("test_delete_key_value")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            ..Default::default()
        };
        let rid = 1;
        let key = b"key".to_vec();
        let (v1, v2) = (b"v1".to_vec(), b"v2".to_vec());
        let mut batch_1 = LogBatch::default();
        batch_1.put(rid, key.clone(), v1);
        let mut batch_2 = LogBatch::default();
        batch_2.put(rid, key.clone(), v2.clone());
        let mut delete_batch = LogBatch::default();
        delete_batch.delete(rid, key.clone());

        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        assert_eq!(
            engine.get_message::<RaftLocalState>(rid, &key).unwrap(),
            None
        );
        assert_eq!(engine.get(rid, &key), None);

        // put | delete
        //     ^ rewrite
        engine.write(&mut batch_1.clone(), true).unwrap();
        assert!(engine.get_message::<RaftLocalState>(rid, &key).is_err());
        engine.purge_manager.must_rewrite_append_queue(None, None);
        engine.write(&mut delete_batch.clone(), true).unwrap();
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key), None);
        assert_eq!(
            engine.get_message::<RaftLocalState>(rid, &key).unwrap(),
            None
        );

        // Incomplete purge.
        engine.write(&mut batch_1.clone(), true).unwrap();
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        engine.write(&mut delete_batch.clone(), true).unwrap();
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key), None);

        // TODO: Preserve kv tombstone during rewrite and activate this test case.
        // put | delete |
        //              ^ rewrite
        // let engine = engine.reopen();
        // engine.write(&mut batch_1.clone(), true).unwrap();
        // engine.write(&mut delete_batch.clone(), true).unwrap();
        // engine.purge_manager.must_rewrite_append_queue(None, None);
        // let engine = engine.reopen();
        // assert_eq!(engine.get(rid, &key), None);

        // put | delete | put
        //     ^ rewrite
        let engine = engine.reopen();
        engine.write(&mut batch_1.clone(), true).unwrap();
        engine.purge_manager.must_rewrite_append_queue(None, None);
        engine.write(&mut delete_batch.clone(), true).unwrap();
        engine.write(&mut batch_2.clone(), true).unwrap();
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key).unwrap(), v2);
        // Incomplete purge.
        engine.write(&mut batch_1.clone(), true).unwrap();
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        engine.write(&mut delete_batch.clone(), true).unwrap();
        engine.write(&mut batch_2.clone(), true).unwrap();
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key).unwrap(), v2);

        // put | delete | put
        //              ^ rewrite
        let engine = engine.reopen();
        engine.write(&mut batch_1.clone(), true).unwrap();
        engine.write(&mut delete_batch.clone(), true).unwrap();
        engine.purge_manager.must_rewrite_append_queue(None, None);
        engine.write(&mut batch_2.clone(), true).unwrap();
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key).unwrap(), v2);
        // Incomplete purge.
        engine.write(&mut batch_1.clone(), true).unwrap();
        engine.write(&mut delete_batch.clone(), true).unwrap();
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        engine.write(&mut batch_2.clone(), true).unwrap();
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key).unwrap(), v2);

        // put | delete | put |
        //                    ^ rewrite
        let engine = engine.reopen();
        engine.write(&mut batch_1.clone(), true).unwrap();
        engine.write(&mut delete_batch.clone(), true).unwrap();
        engine.write(&mut batch_2.clone(), true).unwrap();
        engine.purge_manager.must_rewrite_append_queue(None, None);
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key).unwrap(), v2);
        // Incomplete purge.
        let engine = engine.reopen();
        engine.write(&mut batch_1.clone(), true).unwrap();
        engine.write(&mut delete_batch.clone(), true).unwrap();
        engine.write(&mut batch_2.clone(), true).unwrap();
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        let engine = engine.reopen();
        assert_eq!(engine.get(rid, &key).unwrap(), v2);
    }

    #[test]
    fn test_compact_raft_group() {
        let dir = tempfile::Builder::new()
            .prefix("test_compact_raft_group")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            ..Default::default()
        };
        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        let data = vec![b'x'; 1024];

        // rewrite:[1  ..10]
        // append:   [5..10]
        let mut rid = 7;
        engine.append(rid, 1, 10, Some(&data));
        // Files are not purged.
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        let mut compact_log = LogBatch::default();
        compact_log.add_command(rid, Command::Compact { index: 5 });
        engine.write(&mut compact_log, true).unwrap();
        let engine = engine.reopen();
        engine.scan_entries(rid, 5, 10, |_, q, d| {
            assert_eq!(q, LogQueue::Append);
            assert_eq!(d, &data);
        });
        assert_eq!(engine.stats.live_entries(LogQueue::Append), 6); // 5 entries + 1 kv

        // rewrite:   [20..25]
        // append: [10   ..25]
        rid += 1;
        engine.append(rid, 5, 15, Some(&data));
        let mut compact_log = LogBatch::default();
        compact_log.add_command(rid, Command::Compact { index: 10 });
        engine.write(&mut compact_log, true).unwrap();
        engine.append(rid, 15, 25, Some(&data));
        // Files are not purged.
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        // Simulate loss of buffered write.
        let mut compact_log = LogBatch::default();
        compact_log.add_command(rid, Command::Compact { index: 20 });
        engine.memtables.apply_append_writes(compact_log.drain());
        engine.purge_manager.must_rewrite_rewrite_queue();
        let engine = engine.reopen();
        engine.scan_entries(rid, 10, 25, |_, q, d| {
            assert_eq!(q, LogQueue::Append);
            assert_eq!(d, &data);
        });
        assert_eq!(engine.stats.live_entries(LogQueue::Append), 22); // 20 entries + 2 kv
        engine.clean(rid - 1);
        assert_eq!(engine.stats.live_entries(LogQueue::Append), 16);
        // rewrite: [20..25][10..25]
        // append: [10..25]
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        let engine = engine.reopen();
        engine.scan_entries(rid, 10, 25, |_, q, d| {
            assert_eq!(q, LogQueue::Append);
            assert_eq!(d, &data);
        });

        // rewrite:[10..15][15  ..25]
        // append:           [20..25]
        rid += 1;
        engine.append(rid, 5, 15, Some(&data));
        let mut compact_log = LogBatch::default();
        compact_log.add_command(rid, Command::Compact { index: 10 });
        engine.write(&mut compact_log, true).unwrap();
        engine.purge_manager.must_rewrite_append_queue(None, None);
        engine.append(rid, 15, 25, Some(&data));
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        let mut compact_log = LogBatch::default();
        compact_log.add_command(rid, Command::Compact { index: 20 });
        engine.write(&mut compact_log, true).unwrap();
        let engine = engine.reopen();
        engine.scan_entries(rid, 20, 25, |_, q, d| {
            assert_eq!(q, LogQueue::Append);
            assert_eq!(d, &data);
        });

        // rewrite:[1..5] [10..15]
        // append:        [10..15]
        rid += 1;
        engine.append(rid, 1, 5, Some(&data));
        engine.purge_manager.must_rewrite_append_queue(None, None);
        engine.append(rid, 5, 15, Some(&data));
        let mut compact_log = LogBatch::default();
        compact_log.add_command(rid, Command::Compact { index: 10 });
        engine.write(&mut compact_log, true).unwrap();
        // Files are not purged.
        engine
            .purge_manager
            .must_rewrite_append_queue(None, Some(2));
        let engine = engine.reopen();
        engine.scan_entries(rid, 10, 15, |_, q, d| {
            assert_eq!(q, LogQueue::Append);
            assert_eq!(d, &data);
        });
    }

    #[test]
    fn test_purge_triggered_by_compact() {
        let dir = tempfile::Builder::new()
            .prefix("test_purge_triggered_by_compact")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize::kb(5),
            purge_threshold: ReadableSize::kb(150),
            ..Default::default()
        };

        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        let data = vec![b'x'; 1024];
        for index in 0..100 {
            engine.append(1, index, index + 1, Some(&data));
        }

        // GC all log entries. Won't trigger purge because total size is not enough.
        let count = engine.compact_to(1, 100);
        assert_eq!(count, 100);
        assert!(!engine
            .purge_manager
            .needs_rewrite_log_files(LogQueue::Append));

        // Append more logs to make total size greater than `purge_threshold`.
        for index in 100..250 {
            engine.append(1, index, index + 1, Some(&data));
        }

        // GC first 101 log entries.
        assert_eq!(engine.compact_to(1, 101), 1);
        // Needs to purge because the total size is greater than `purge_threshold`.
        assert!(engine
            .purge_manager
            .needs_rewrite_log_files(LogQueue::Append));

        let old_min_file_seq = engine.file_span(LogQueue::Append).0;
        let will_force_compact = engine.purge_expired_files().unwrap();
        let new_min_file_seq = engine.file_span(LogQueue::Append).0;
        // Some entries are rewritten.
        assert!(new_min_file_seq > old_min_file_seq);
        // No regions need to be force compacted because the threshold is not reached.
        assert!(will_force_compact.is_empty());
        // After purge, entries and raft state are still available.
        assert!(engine.get_entry::<Entry>(1, 101).unwrap().is_some());

        assert_eq!(engine.compact_to(1, 102), 1);
        // Needs to purge because the total size is greater than `purge_threshold`.
        assert!(engine
            .purge_manager
            .needs_rewrite_log_files(LogQueue::Append));
        let will_force_compact = engine.purge_expired_files().unwrap();
        // The region needs to be force compacted because the threshold is reached.
        assert!(!will_force_compact.is_empty());
        assert_eq!(will_force_compact[0], 1);
    }

    #[test]
    fn test_purge_trigger_force_rewrite() {
        let dir = tempfile::Builder::new()
            .prefix("test_purge_trigger_force_write")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize::kb(1),
            purge_threshold: ReadableSize::kb(10),
            ..Default::default()
        };

        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        let data = vec![b'x'; 1024];
        // write 50 small entries into region 1~3, it should trigger force compact.
        for rid in 1..=3 {
            for index in 0..50 {
                engine.append(rid, index, index + 1, Some(&data[..10]));
            }
        }
        // write some small entries to trigger purge.
        for rid in 4..=50 {
            engine.append(rid, 1, 2, Some(&data));
        }

        let check_purge = |pending_regions: Vec<u64>| {
            let mut compact_regions = engine.purge_expired_files().unwrap();
            // sort key in order.
            compact_regions.sort_unstable();
            assert_eq!(compact_regions, pending_regions);
        };

        for _ in 0..9 {
            check_purge(vec![1, 2, 3]);
        }

        // 10th, rewritten, but still needs to be compacted.
        check_purge(vec![1, 2, 3]);
        for rid in 1..=3 {
            let memtable = engine.memtables.get(rid).unwrap();
            assert_eq!(memtable.read().rewrite_count(), 50);
        }

        // compact and write some new data to trigger compact again.
        for rid in 2..=50 {
            let last_idx = engine.last_index(rid).unwrap();
            engine.compact_to(rid, last_idx);
            engine.append(rid, last_idx, last_idx + 1, Some(&data));
        }
        // after write, region 1 can trigger compact again.
        check_purge(vec![1]);
    }

    #[test]
    fn test_rewrite_and_recover() {
        let dir = tempfile::Builder::new()
            .prefix("test_rewrite_and_recover")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize::kb(5),
            purge_threshold: ReadableSize::kb(80),
            ..Default::default()
        };
        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        let data = vec![b'x'; 1024];

        // Put 100 entries into 10 regions.
        for index in 1..=10 {
            for rid in 1..=10 {
                engine.append(rid, index, index + 1, Some(&data));
            }
        }

        // The engine needs purge, and all old entries should be rewritten.
        assert!(engine
            .purge_manager
            .needs_rewrite_log_files(LogQueue::Append));
        assert!(engine.purge_expired_files().unwrap().is_empty());
        assert!(engine.file_span(LogQueue::Append).0 > 1);

        let rewrite_file_size = engine.pipe_log.total_size(LogQueue::Rewrite);
        assert!(rewrite_file_size > 59); // The rewrite queue isn't empty.

        // All entries should be available.
        for rid in 1..=10 {
            engine.scan_entries(rid, 1, 11, |_, _, d| {
                assert_eq!(d, &data);
            });
        }

        // Recover with rewrite queue and append queue.
        let cleaned_region_ids = engine.memtables.cleaned_region_ids();

        let engine = engine.reopen();
        assert_eq!(engine.memtables.cleaned_region_ids(), cleaned_region_ids);

        for rid in 1..=10 {
            engine.scan_entries(rid, 1, 11, |_, _, d| {
                assert_eq!(d, &data);
            });
        }

        // Rewrite again to check the rewrite queue is healthy.
        for index in 11..=20 {
            for rid in 1..=10 {
                engine.append(rid, index, index + 1, Some(&data));
            }
        }

        assert!(engine
            .purge_manager
            .needs_rewrite_log_files(LogQueue::Append));
        assert!(engine.purge_expired_files().unwrap().is_empty());
    }

    #[test]
    fn test_empty_protobuf_message() {
        let dir = tempfile::Builder::new()
            .prefix("test_empty_protobuf_message")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();

        let mut log_batch = LogBatch::default();
        let empty_entry = Entry::new();
        assert_eq!(empty_entry.compute_size(), 0);
        log_batch
            .add_entries::<Entry>(0, &[empty_entry.clone()])
            .unwrap();
        engine.write(&mut log_batch, false).unwrap();
        let empty_state = RaftLocalState::new();
        assert_eq!(empty_state.compute_size(), 0);
        log_batch
            .put_message(1, b"key".to_vec(), &empty_state)
            .unwrap();
        engine.write(&mut log_batch, false).unwrap();
        log_batch
            .add_entries::<Entry>(2, &[empty_entry.clone()])
            .unwrap();
        log_batch
            .put_message(2, b"key".to_vec(), &empty_state)
            .unwrap();
        engine.write(&mut log_batch, true).unwrap();

        let engine = engine.reopen();
        assert_eq!(
            engine.get_entry::<Entry>(0, 0).unwrap().unwrap(),
            empty_entry
        );
        assert_eq!(
            engine.get_entry::<Entry>(2, 0).unwrap().unwrap(),
            empty_entry
        );
        assert_eq!(
            engine
                .get_message::<RaftLocalState>(1, b"key")
                .unwrap()
                .unwrap(),
            empty_state
        );
        assert_eq!(
            engine
                .get_message::<RaftLocalState>(2, b"key")
                .unwrap()
                .unwrap(),
            empty_state
        );
    }

    #[test]
    fn test_empty_batch() {
        let dir = tempfile::Builder::new()
            .prefix("test_empty_batch")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        let data = vec![b'x'; 16];
        let cases = [[false, false], [false, true], [true, true]];
        for (i, writes) in cases.iter().enumerate() {
            let rid = i as u64;
            let mut batch = LogBatch::default();
            for &has_data in writes {
                if has_data {
                    batch.put(rid, b"key".to_vec(), data.clone());
                }
                engine.write(&mut batch, true).unwrap();
                assert!(batch.is_empty());
            }
        }
    }

    #[test]
    fn test_dirty_recovery() {
        let dir = tempfile::Builder::new()
            .prefix("test_dirty_recovery")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        let data = vec![b'x'; 1024];

        for rid in 1..21 {
            engine.append(rid, 1, 21, Some(&data));
        }

        // Create an unrelated sub-directory.
        std::fs::create_dir(dir.path().join(Path::new("random_dir"))).unwrap();
        // Create an unrelated file.
        let _f = std::fs::File::create(dir.path().join(Path::new("random_file"))).unwrap();

        let engine = engine.reopen();
        for rid in 1..21 {
            engine.scan_entries(rid, 1, 21, |_, _, d| {
                assert_eq!(d, &data);
            });
        }
    }

    #[test]
    fn test_large_rewrite_batch() {
        let dir = tempfile::Builder::new()
            .prefix("test_large_rewrite_batch")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            ..Default::default()
        };
        let engine =
            RaftLogEngine::open_with_file_system(cfg, Arc::new(ObfuscatedFileSystem::default()))
                .unwrap();
        let data = vec![b'x'; 2 * 1024 * 1024];

        for rid in 1..=3 {
            engine.append(rid, 1, 11, Some(&data));
        }

        let old_active_file = engine.file_span(LogQueue::Append).1;
        engine.purge_manager.must_rewrite_append_queue(None, None);
        assert_eq!(engine.file_span(LogQueue::Append).0, old_active_file + 1);
        let old_active_file = engine.file_span(LogQueue::Rewrite).1;
        engine.purge_manager.must_rewrite_rewrite_queue();
        assert!(engine.file_span(LogQueue::Rewrite).0 > old_active_file);

        let engine = engine.reopen();
        for rid in 1..=3 {
            engine.scan_entries(rid, 1, 11, |_, _, d| {
                assert_eq!(d, &data);
            });
        }
    }

    #[test]
    fn test_combination_of_version_and_recycle() {
        fn test_engine_ops(cfg_v1: &Config, cfg_v2: &Config) {
            let rid = 1;
            let data = vec![b'7'; 1024];
            {
                // open engine with format_version - Version::V1
                let engine = RaftLogEngine::open(cfg_v1.clone()).unwrap();
                engine.append(rid, 0, 20, Some(&data));
                let append_first = engine.file_span(LogQueue::Append).0;
                engine.compact_to(rid, 18);
                engine.purge_expired_files().unwrap();
                assert!(engine.file_span(LogQueue::Append).0 > append_first);
                assert_eq!(engine.first_index(rid).unwrap(), 18);
                assert_eq!(engine.last_index(rid).unwrap(), 19);
            }
            {
                // open engine with format_version - Version::V2
                let engine = RaftLogEngine::open(cfg_v2.clone()).unwrap();
                assert_eq!(engine.first_index(rid).unwrap(), 18);
                assert_eq!(engine.last_index(rid).unwrap(), 19);
                engine.append(rid, 20, 40, Some(&data));
                let append_first = engine.file_span(LogQueue::Append).0;
                engine.compact_to(rid, 38);
                engine.purge_expired_files().unwrap();
                assert!(engine.file_span(LogQueue::Append).0 > append_first);
                assert_eq!(engine.first_index(rid).unwrap(), 38);
                assert_eq!(engine.last_index(rid).unwrap(), 39);
            }
            {
                // reopen engine with format_version - Version::V1
                let engine = RaftLogEngine::open(cfg_v1.clone()).unwrap();
                assert_eq!(engine.first_index(rid).unwrap(), 38);
                assert_eq!(engine.last_index(rid).unwrap(), 39);
            }
        }
        // test engine on mutable versions
        {
            let dir = tempfile::Builder::new()
                .prefix("test_mutable_format_version")
                .tempdir()
                .unwrap();
            // config with v1
            let cfg_v1 = Config {
                dir: dir.path().to_str().unwrap().to_owned(),
                target_file_size: ReadableSize(1),
                purge_threshold: ReadableSize(1),
                ..Default::default()
            };
            // config with v2
            let cfg_v2 = Config {
                dir: dir.path().to_str().unwrap().to_owned(),
                target_file_size: ReadableSize(1),
                purge_threshold: ReadableSize(1),
                format_version: Version::V2,
                ..Default::default()
            };
            test_engine_ops(&cfg_v1, &cfg_v2);
        }
        // test engine when enable_log_recycle == true
        {
            let dir = tempfile::Builder::new()
                .prefix("test_enable_log_recycle")
                .tempdir()
                .unwrap();
            // config with v1
            let cfg_v1 = Config {
                dir: dir.path().to_str().unwrap().to_owned(),
                target_file_size: ReadableSize(1),
                purge_threshold: ReadableSize(1),
                ..Default::default()
            };
            // config with v2
            let cfg_v2 = Config {
                dir: dir.path().to_str().unwrap().to_owned(),
                target_file_size: ReadableSize(1),
                purge_threshold: ReadableSize(1),
                format_version: Version::V2,
                enable_log_recycle: true,
                ..Default::default()
            };
            test_engine_ops(&cfg_v1, &cfg_v2);
        }
    }

    /// Test cases related to tools ///

    #[test]
    fn test_dump_file_or_directory() {
        let dir = tempfile::Builder::new()
            .prefix("test_dump_file_or_directory")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 1024];
        let fs = Arc::new(ObfuscatedFileSystem::default());

        let mut batches = vec![vec![LogBatch::default()]];
        let mut batch = LogBatch::default();
        batch
            .add_entries::<Entry>(7, &generate_entries(1, 11, Some(&entry_data)))
            .unwrap();
        batch.add_command(7, Command::Clean);
        batch.put(7, b"key".to_vec(), b"value".to_vec());
        batch.delete(7, b"key2".to_vec());
        batches.push(vec![batch.clone()]);
        let mut batch2 = LogBatch::default();
        batch2.put(8, b"key3".to_vec(), b"value".to_vec());
        batch2
            .add_entries::<Entry>(8, &generate_entries(5, 15, Some(&entry_data)))
            .unwrap();
        batches.push(vec![batch, batch2]);

        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            ..Default::default()
        };

        let engine = RaftLogEngine::open_with_file_system(cfg, fs.clone()).unwrap();
        for bs in batches.iter_mut() {
            for batch in bs.iter_mut() {
                engine.write(batch, false).unwrap();
            }

            engine.sync().unwrap();
        }

        drop(engine);
        //dump dir with raft groups. 8 element in raft groups 7 and 2 elements in raft
        // groups 8
        let dump_it = Engine::dump_with_file_system(dir.path(), fs.clone()).unwrap();
        let total = dump_it
            .inspect(|i| {
                i.as_ref().unwrap();
            })
            .count();
        assert!(total == 10);

        //dump file
        let file_id = FileId {
            queue: LogQueue::Rewrite,
            seq: 1,
        };
        let dump_it = Engine::dump_with_file_system(
            file_id.build_file_path(dir.path()).as_path(),
            fs.clone(),
        )
        .unwrap();
        let total = dump_it
            .inspect(|i| {
                i.as_ref().unwrap();
            })
            .count();
        assert!(0 == total);

        //dump dir that does not exists
        assert!(Engine::dump_with_file_system(Path::new("/not_exists_dir"), fs.clone()).is_err());

        //dump file that does not exists
        let mut not_exists_file = PathBuf::from(dir.as_ref());
        not_exists_file.push("not_exists_file");
        assert!(Engine::dump_with_file_system(not_exists_file.as_path(), fs).is_err());
    }

    #[cfg(feature = "scripting")]
    #[test]
    fn test_repair_default() {
        let dir = tempfile::Builder::new()
            .prefix("test_repair_default")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 128];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1), // Create lots of files.
            ..Default::default()
        };
        let fs = Arc::new(ObfuscatedFileSystem::default());

        let engine = RaftLogEngine::open_with_file_system(cfg.clone(), fs.clone()).unwrap();
        for rid in 1..=50 {
            engine.append(rid, 1, 6, Some(&entry_data));
        }
        for rid in 25..=50 {
            engine.append(rid, 6, 11, Some(&entry_data));
        }
        drop(engine);

        let script1 = "".to_owned();
        RaftLogEngine::unsafe_repair_with_file_system(
            dir.path(),
            None, /* queue */
            script1,
            fs.clone(),
        )
        .unwrap();
        let script2 = "
            fn filter_append(id, first, count, rewrite_count, queue, ifirst, ilast) {
                0
            }
            fn filter_compact(id, first, count, rewrite_count, queue, compact_to) {
                0
            }
            fn filter_clean(id, first, count, rewrite_count, queue) {
                0
            }
        "
        .to_owned();
        RaftLogEngine::unsafe_repair_with_file_system(
            dir.path(),
            None, /* queue */
            script2,
            fs.clone(),
        )
        .unwrap();

        let engine = RaftLogEngine::open_with_file_system(cfg, fs).unwrap();
        for rid in 1..25 {
            engine.scan_entries(rid, 1, 6, |_, _, d| {
                assert_eq!(d, &entry_data);
            });
        }
        for rid in 25..=50 {
            engine.scan_entries(rid, 1, 11, |_, _, d| {
                assert_eq!(d, &entry_data);
            });
        }
    }

    #[cfg(feature = "scripting")]
    #[test]
    fn test_repair_discard_entries() {
        let dir = tempfile::Builder::new()
            .prefix("test_repair_discard")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 128];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1), // Create lots of files.
            ..Default::default()
        };
        let fs = Arc::new(ObfuscatedFileSystem::default());

        let engine = RaftLogEngine::open_with_file_system(cfg.clone(), fs.clone()).unwrap();
        for rid in 1..=50 {
            engine.append(rid, 1, 6, Some(&entry_data));
        }
        for rid in 25..=50 {
            engine.append(rid, 6, 11, Some(&entry_data));
        }
        drop(engine);

        let incoming_emptied = [1, 25];
        let existing_emptied = [2, 26];
        let script = "
            fn filter_append(id, first, count, rewrite_count, queue, ifirst, ilast) {
                if id == 1 {
                    return 1;
                } else if id == 2 {
                    return 2;
                } else if id == 25 {
                    return 1;
                } else if id == 26 {
                    return 2;
                }
                0 // default
            }
        "
        .to_owned();
        RaftLogEngine::unsafe_repair_with_file_system(
            dir.path(),
            None, /* queue */
            script,
            fs.clone(),
        )
        .unwrap();

        let engine = RaftLogEngine::open_with_file_system(cfg, fs).unwrap();
        for rid in 1..25 {
            if existing_emptied.contains(&rid) || incoming_emptied.contains(&rid) {
                continue;
            }
            engine.scan_entries(rid, 1, 6, |_, _, d| {
                assert_eq!(d, &entry_data);
            });
        }
        for rid in 25..=50 {
            if existing_emptied.contains(&rid) || incoming_emptied.contains(&rid) {
                continue;
            }
            engine.scan_entries(rid, 1, 11, |_, _, d| {
                assert_eq!(d, &entry_data);
            });
        }
        for rid in existing_emptied {
            let first_index = if rid < 25 { 1 } else { 6 };
            let last_index = if rid < 25 { 5 } else { 10 };
            engine.scan_entries(rid, first_index, last_index + 1, |_, _, d| {
                assert_eq!(d, &entry_data);
            });
        }
        for rid in incoming_emptied {
            let last_index = if rid < 25 { 5 } else { 10 };
            assert_eq!(engine.first_index(rid), None);
            assert_eq!(engine.last_index(rid), None);
            assert_eq!(engine.decode_last_index(rid), Some(last_index));
        }
    }

    #[test]
    fn test_tail_corruption() {
        let dir = tempfile::Builder::new()
            .prefix("test_tail_corruption")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 16];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            // One big file.
            target_file_size: ReadableSize::gb(10),
            ..Default::default()
        };
        let fs = Arc::new(ObfuscatedFileSystem::default());

        let engine = RaftLogEngine::open_with_file_system(cfg.clone(), fs.clone()).unwrap();
        for rid in 1..=50 {
            engine.append(rid, 1, 6, Some(&entry_data));
        }
        for rid in 25..=50 {
            engine.append(rid, 6, 11, Some(&entry_data));
        }
        let (_, last_file_seq) = engine.file_span(LogQueue::Append);
        drop(engine);

        let last_file = FileId {
            queue: LogQueue::Append,
            seq: last_file_seq,
        };
        let f = OpenOptions::new()
            .write(true)
            .open(last_file.build_file_path(dir.path()))
            .unwrap();

        // Corrupt a log batch.
        f.set_len(f.metadata().unwrap().len() - 1).unwrap();
        RaftLogEngine::open_with_file_system(cfg.clone(), fs.clone()).unwrap();

        // Corrupt the file header.
        f.set_len(1).unwrap();
        RaftLogEngine::open_with_file_system(cfg, fs).unwrap();
    }

    #[test]
    fn test_reopen_with_wrong_file_system() {
        let dir = tempfile::Builder::new()
            .prefix("test_reopen_with_wrong_file_system")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 128];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            ..Default::default()
        };
        let fs = Arc::new(ObfuscatedFileSystem::default());

        let engine = RaftLogEngine::open_with_file_system(cfg.clone(), fs.clone()).unwrap();
        for rid in 1..=10 {
            engine.append(rid, 1, 11, Some(&entry_data));
        }
        drop(engine);

        assert!(RaftLogEngine::open(cfg.clone()).is_err());

        let engine = RaftLogEngine::open_with_file_system(cfg, fs).unwrap();
        for rid in 1..10 {
            engine.scan_entries(rid, 1, 11, |_, _, d| {
                assert_eq!(d, &entry_data);
            });
        }
    }

    #[cfg(feature = "nightly")]
    #[bench]
    fn bench_engine_fetch_entries(b: &mut test::Bencher) {
        use rand::{thread_rng, Rng};

        let dir = tempfile::Builder::new()
            .prefix("bench_engine_fetch_entries")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 1024];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let engine = RaftLogEngine::open(cfg).unwrap();
        for i in 0..10 {
            for rid in 1..=100 {
                engine.append(rid, 1 + i * 10, 1 + i * 10 + 10, Some(&entry_data));
            }
        }
        let mut vec: Vec<Entry> = Vec::new();
        b.iter(move || {
            let region_id = thread_rng().gen_range(1..=100);
            engine
                .fetch_entries_to::<Entry>(region_id, 1, 101, None, &mut vec)
                .unwrap();
            vec.clear();
        });
    }

    #[test]
    fn test_engine_is_empty() {
        let dir = tempfile::Builder::new()
            .prefix("test_engine_is_empty")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 128];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let fs = Arc::new(ObfuscatedFileSystem::default());
        let rid = 1;

        let engine = RaftLogEngine::open_with_file_system(cfg, fs).unwrap();
        assert!(engine.is_empty());
        engine.append(rid, 1, 11, Some(&entry_data));
        assert!(!engine.is_empty());

        let mut log_batch = LogBatch::default();
        log_batch.add_command(rid, Command::Compact { index: 11 });
        log_batch.delete(rid, b"last_index".to_vec());
        engine.write(&mut log_batch, true).unwrap();
        assert!(!engine.is_empty());

        engine.clean(rid);
        assert!(engine.is_empty());
    }

    pub struct DeleteMonitoredFileSystem {
        inner: ObfuscatedFileSystem,
        append_metadata: Mutex<BTreeSet<u64>>,
    }

    impl DeleteMonitoredFileSystem {
        fn new() -> Self {
            Self {
                inner: ObfuscatedFileSystem::default(),
                append_metadata: Mutex::new(BTreeSet::new()),
            }
        }

        fn update_metadata(&self, path: &Path, delete: bool) -> bool {
            let id = FileId::parse_file_name(path.file_name().unwrap().to_str().unwrap()).unwrap();
            if id.queue == LogQueue::Append {
                if delete {
                    self.append_metadata.lock().unwrap().remove(&id.seq)
                } else {
                    self.append_metadata.lock().unwrap().insert(id.seq)
                }
            } else {
                false
            }
        }
    }

    impl FileSystem for DeleteMonitoredFileSystem {
        type Handle = <ObfuscatedFileSystem as FileSystem>::Handle;
        type Reader = <ObfuscatedFileSystem as FileSystem>::Reader;
        type Writer = <ObfuscatedFileSystem as FileSystem>::Writer;

        fn create<P: AsRef<Path>>(&self, path: P) -> std::io::Result<Self::Handle> {
            let handle = self.inner.create(&path)?;
            self.update_metadata(path.as_ref(), false);
            Ok(handle)
        }

        fn open<P: AsRef<Path>>(&self, path: P) -> std::io::Result<Self::Handle> {
            let handle = self.inner.open(&path)?;
            self.update_metadata(path.as_ref(), false);
            Ok(handle)
        }

        fn delete<P: AsRef<Path>>(&self, path: P) -> std::io::Result<()> {
            self.inner.delete(&path)?;
            self.update_metadata(path.as_ref(), true);
            Ok(())
        }

        fn rename<P: AsRef<Path>>(&self, src_path: P, dst_path: P) -> std::io::Result<()> {
            self.inner.rename(src_path.as_ref(), dst_path.as_ref())?;
            self.update_metadata(src_path.as_ref(), true);
            self.update_metadata(dst_path.as_ref(), false);
            Ok(())
        }

        fn reuse<P: AsRef<Path>>(&self, src_path: P, dst_path: P) -> std::io::Result<()> {
            self.inner.reuse(src_path.as_ref(), dst_path.as_ref())?;
            self.update_metadata(src_path.as_ref(), true);
            self.update_metadata(dst_path.as_ref(), false);
            Ok(())
        }

        fn delete_metadata<P: AsRef<Path>>(&self, path: P) -> std::io::Result<()> {
            self.inner.delete_metadata(&path)?;
            self.update_metadata(path.as_ref(), true);
            Ok(())
        }

        fn exists_metadata<P: AsRef<Path>>(&self, path: P) -> bool {
            if self.inner.exists_metadata(&path) {
                return true;
            }
            let id = FileId::parse_file_name(path.as_ref().file_name().unwrap().to_str().unwrap())
                .unwrap();
            if id.queue == LogQueue::Append {
                self.append_metadata.lock().unwrap().contains(&id.seq)
            } else {
                false
            }
        }

        fn new_reader(&self, h: Arc<Self::Handle>) -> std::io::Result<Self::Reader> {
            self.inner.new_reader(h)
        }

        fn new_writer(&self, h: Arc<Self::Handle>) -> std::io::Result<Self::Writer> {
            self.inner.new_writer(h)
        }
    }

    #[test]
    fn test_managed_file_deletion() {
        let dir = tempfile::Builder::new()
            .prefix("test_managed_file_deletion")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 128];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            purge_threshold: ReadableSize(1),
            ..Default::default()
        };
        let fs = Arc::new(DeleteMonitoredFileSystem::new());
        let engine = RaftLogEngine::open_with_file_system(cfg, fs.clone()).unwrap();
        for rid in 1..=10 {
            engine.append(rid, 1, 11, Some(&entry_data));
        }
        for rid in 1..=5 {
            engine.clean(rid);
        }
        let (start, _) = engine.file_span(LogQueue::Append);
        engine.purge_expired_files().unwrap();
        assert!(start < engine.file_span(LogQueue::Append).0);
        assert_eq!(engine.file_count(None), fs.inner.file_count());
        let start = engine.file_span(LogQueue::Append).0;
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );

        let engine = engine.reopen();
        assert_eq!(engine.file_count(None), fs.inner.file_count());
        let (start, _) = engine.file_span(LogQueue::Append);
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );

        // Simulate stale metadata.
        for i in start / 2..start {
            fs.append_metadata.lock().unwrap().insert(i);
        }
        let engine = engine.reopen();
        let (start, _) = engine.file_span(LogQueue::Append);
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );
    }

    #[test]
    fn test_managed_file_reuse() {
        let dir = tempfile::Builder::new()
            .prefix("test_managed_file_reuse")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 128];
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            purge_threshold: ReadableSize(2),
            format_version: Version::V2,
            enable_log_recycle: true,
            ..Default::default()
        };
        let fs = Arc::new(DeleteMonitoredFileSystem::new());
        let engine = RaftLogEngine::open_with_file_system(cfg, fs.clone()).unwrap();
        for rid in 1..=10 {
            engine.append(rid, 1, 11, Some(&entry_data));
        }
        for rid in 1..=5 {
            engine.clean(rid);
        }
        let (start, _) = engine.file_span(LogQueue::Append);
        // the [start - 1] files are recycled
        engine.purge_expired_files().unwrap();
        assert!(start < engine.file_span(LogQueue::Append).0);
        let file_count = fs.inner.file_count();
        assert_eq!(engine.file_count(None) + 1, file_count);
        let start = engine.file_span(LogQueue::Append).0 - 1;
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );
        // reopen the engine and validate the stale files are removed
        let engine = engine.reopen();
        assert_eq!(fs.inner.file_count(), file_count - 1);
        assert_eq!(engine.file_span(LogQueue::Append).0, start + 1);
        let start = engine.file_span(LogQueue::Append).0;
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );
        // rewrite the recycled files
        for rid in 1..=2 {
            engine.clean(rid);
        }
        engine.purge_expired_files().unwrap();
        for rid in 1..=2 {
            engine.append(rid, 1, 11, Some(&entry_data));
        }
        assert_eq!(engine.file_count(None), fs.inner.file_count());
        let start = engine.file_span(LogQueue::Append).0;
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );

        let engine = engine.reopen();
        assert_eq!(engine.file_count(None), fs.inner.file_count());
        let (start, _) = engine.file_span(LogQueue::Append);
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );

        // Simulate stale metadata.
        for i in start / 2..start {
            fs.append_metadata.lock().unwrap().insert(i);
        }
        let engine = engine.reopen();
        let (start, _) = engine.file_span(LogQueue::Append);
        assert_eq!(
            fs.append_metadata.lock().unwrap().iter().next().unwrap(),
            &start
        );
    }

    #[test]
    fn test_simple_write_perf_context() {
        let dir = tempfile::Builder::new()
            .prefix("test_simple_write_perf_context")
            .tempdir()
            .unwrap();
        let cfg = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let rid = 1;
        let entry_size = 5120;
        let engine = RaftLogEngine::open(cfg).unwrap();
        let data = vec![b'x'; entry_size];
        let old_perf_context = get_perf_context();
        engine.append(rid, 1, 5, Some(&data));
        let new_perf_context = get_perf_context();
        assert_ne!(
            old_perf_context.log_populating_duration,
            new_perf_context.log_populating_duration
        );
        assert_ne!(
            old_perf_context.log_write_duration,
            new_perf_context.log_write_duration
        );
        assert_ne!(
            old_perf_context.apply_duration,
            new_perf_context.apply_duration
        );
    }

    #[test]
    fn test_recycle_no_signing_files() {
        let dir = tempfile::Builder::new()
            .prefix("test_recycle_no_signing_files")
            .tempdir()
            .unwrap();
        let entry_data = vec![b'x'; 128];
        let fs = Arc::new(DeleteMonitoredFileSystem::new());
        let cfg_v1 = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            purge_threshold: ReadableSize(1024),
            format_version: Version::V1,
            enable_log_recycle: false,
            ..Default::default()
        };
        let cfg_v2 = Config {
            dir: dir.path().to_str().unwrap().to_owned(),
            target_file_size: ReadableSize(1),
            purge_threshold: ReadableSize(15),
            format_version: Version::V2,
            enable_log_recycle: true,
            ..Default::default()
        };
        assert_eq!(cfg_v2.recycle_capacity(), 15);
        // Prepare files with format_version V1
        {
            let engine = RaftLogEngine::open_with_file_system(cfg_v1.clone(), fs.clone()).unwrap();
            for rid in 1..=10 {
                engine.append(rid, 1, 11, Some(&entry_data));
            }
        }
        // Reopen the Engine with V2 and purge
        {
            let engine = RaftLogEngine::open_with_file_system(cfg_v2.clone(), fs.clone()).unwrap();
            let (start, _) = engine.file_span(LogQueue::Append);
            for rid in 6..=10 {
                engine.append(rid, 11, 20, Some(&entry_data));
            }
            // Mark region_id -> 6 obsolete.
            engine.clean(6);
            // the [1, 12] files are recycled
            engine.purge_expired_files().unwrap();
            assert_eq!(engine.file_count(Some(LogQueue::Append)), 5);
            assert!(start < engine.file_span(LogQueue::Append).0);
        }
        // Reopen the Engine with V1 -> V2 and purge
        {
            let engine = RaftLogEngine::open_with_file_system(cfg_v1, fs.clone()).unwrap();
            let (start, _) = engine.file_span(LogQueue::Append);
            for rid in 6..=10 {
                engine.append(rid, 20, 30, Some(&entry_data));
            }
            for rid in 6..=10 {
                engine.append(rid, 30, 40, Some(&entry_data));
            }
            for rid in 1..=5 {
                engine.append(rid, 11, 20, Some(&entry_data));
            }
            assert_eq!(engine.file_span(LogQueue::Append).0, start);
            let file_count = engine.file_count(Some(LogQueue::Append));
            drop(engine);
            let engine = RaftLogEngine::open_with_file_system(cfg_v2, fs).unwrap();
            assert_eq!(engine.file_span(LogQueue::Append).0, start);
            assert_eq!(engine.file_count(Some(LogQueue::Append)), file_count);
            // Mark all regions obsolete.
            for rid in 1..=10 {
                engine.clean(rid);
            }
            let (start, _) = engine.file_span(LogQueue::Append);
            // the [13, 32] files are purged
            engine.purge_expired_files().unwrap();
            assert_eq!(engine.file_count(Some(LogQueue::Append)), 1);
            assert!(engine.file_span(LogQueue::Append).0 > start);
        }
    }
}
