// WalImpl<T> — high-level WAL with background writer thread, supporting both
// synchronous and asynchronous write modes.
//
// All writes, truncations, and close operations are serialized through a
// single background thread (main_loop) via crossbeam channels. This avoids
// contention on the underlying WalLog and makes the design straightforward
// to reason about.
//
// T2.3: async write mode, batched writes, and auto-pruning.

use crate::log::{WalLog, WalLogOptions};
use crossbeam_channel::{bounded, select, Receiver, Sender};
use mptdb_common::{
    config::WalConfig,
    error::{MptDbError, Result},
};
use mptdb_traits::wal::Wal;
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// ---------------------------------------------------------------------------
// Type aliases for clippy::type_complexity
// ---------------------------------------------------------------------------

type MarshalFn<T> = Arc<dyn Fn(&T) -> Result<Vec<u8>> + Send + Sync>;
type UnmarshalFn<T> = Arc<dyn Fn(&[u8]) -> Result<T> + Send + Sync>;

// ---------------------------------------------------------------------------
// Internal request types
// ---------------------------------------------------------------------------

struct WriteRequest<T> {
    entry: T,
    /// Some for sync mode (caller blocks on this), None for async fire-and-forget.
    err_tx: Option<Sender<Result<()>>>,
}

struct TruncateRequest {
    /// true = truncate_front (before), false = truncate_back (after).
    before: bool,
    index: u64,
    err_tx: Sender<Result<()>>,
}

// ---------------------------------------------------------------------------
// WalImpl
// ---------------------------------------------------------------------------

/// High-level write-ahead log wrapping [`WalLog`] with a background writer
/// thread and marshal/unmarshal callbacks.
pub struct WalImpl<T: Send + 'static> {
    log: Arc<Mutex<WalLog>>,
    #[allow(dead_code)]
    config: WalConfig,
    /// Marshal callback — kept alive so the background thread's Arc clone stays valid.
    #[allow(dead_code)]
    marshal: MarshalFn<T>,
    unmarshal: UnmarshalFn<T>,

    async_writes: bool,
    #[allow(dead_code)]
    write_batch_size: usize,

    write_tx: Sender<WriteRequest<T>>,
    truncate_tx: Sender<TruncateRequest>,
    close_tx: Sender<()>,

    close_result: Arc<Mutex<Option<Result<()>>>>,
    async_error: Arc<AtomicBool>,
    async_error_detail: Arc<Mutex<Option<MptDbError>>>,
    worker_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl<T: Send + 'static> WalImpl<T> {
    /// Create a new WAL.
    ///
    /// - `marshal`: serializes `T` to bytes.
    /// - `unmarshal`: deserializes bytes to `T`.
    /// - `config`: WAL configuration (buffer sizes, fsync, etc.).
    /// - `dir`: directory for segment files.
    pub fn new(
        marshal: impl Fn(&T) -> Result<Vec<u8>> + Send + Sync + 'static,
        unmarshal: impl Fn(&[u8]) -> Result<T> + Send + Sync + 'static,
        config: WalConfig,
        dir: impl AsRef<std::path::Path>,
    ) -> Result<Self> {
        let opts = WalLogOptions {
            no_sync: !config.fsync_enabled,
            segment_size: 0, // use default 20 MB
        };
        let wal_log = WalLog::open(dir.as_ref(), opts)?;
        let log = Arc::new(Mutex::new(wal_log));

        let async_writes = config.write_buffer_size > 0;
        let write_batch_size = config.write_batch_size;
        let chan_size = if config.write_buffer_size > 0 { config.write_buffer_size } else { 1 };

        let (write_tx, write_rx) = bounded::<WriteRequest<T>>(chan_size);
        let (truncate_tx, truncate_rx) = bounded::<TruncateRequest>(1);
        let (close_tx, close_rx) = bounded::<()>(1);

        let async_error = Arc::new(AtomicBool::new(false));
        let async_error_detail: Arc<Mutex<Option<MptDbError>>> = Arc::new(Mutex::new(None));

        let marshal = Arc::new(marshal);
        let unmarshal = Arc::new(unmarshal);

        // Clone Arcs for the background thread.
        let bg_log = Arc::clone(&log);
        let bg_marshal = Arc::clone(&marshal);
        let bg_async_error = Arc::clone(&async_error);
        let bg_async_error_detail = Arc::clone(&async_error_detail);
        let bg_config = config.clone();

        let worker_handle = std::thread::Builder::new()
            .name("mptdb-wal-writer".into())
            .spawn(move || {
                main_loop(
                    bg_log,
                    bg_marshal,
                    bg_config,
                    write_rx,
                    truncate_rx,
                    close_rx,
                    bg_async_error,
                    bg_async_error_detail,
                );
            })
            .map_err(|e| MptDbError::Other(format!("failed to spawn wal writer thread: {e}")))?;

        Ok(Self {
            log,
            config,
            marshal,
            unmarshal,
            async_writes,
            write_batch_size,
            write_tx,
            truncate_tx,
            close_tx,
            close_result: Arc::new(Mutex::new(None)),
            async_error,
            async_error_detail,
            worker_handle: Mutex::new(Some(worker_handle)),
        })
    }

    /// Returns an error if the background thread has recorded a fatal error.
    fn check_error(&self) -> Result<()> {
        if self.async_error.load(Ordering::Relaxed) {
            let detail = self.async_error_detail.lock();
            if let Some(ref e) = *detail {
                Err(MptDbError::Other(format!("wal async error: {e}")))
            } else {
                Err(MptDbError::Other("wal async error (unknown)".into()))
            }
        } else {
            Ok(())
        }
    }

    /// Send a truncation request through the channel and wait for the result.
    fn send_truncate(&self, before: bool, index: u64) -> Result<()> {
        let (err_tx, err_rx) = bounded(1);
        let req = TruncateRequest { before, index, err_tx };
        self.truncate_tx
            .send(req)
            .map_err(|_| MptDbError::Other("wal truncate channel closed".into()))?;
        err_rx
            .recv()
            .map_err(|_| MptDbError::Other("wal truncate response channel closed".into()))?
    }
}

// ---------------------------------------------------------------------------
// Wal trait implementation
// ---------------------------------------------------------------------------

impl<T: Send + 'static> Wal<T> for WalImpl<T> {
    fn write(&self, entry: T) -> Result<()> {
        self.check_error()?;

        if self.async_writes {
            // Async fire-and-forget: no response channel.
            let req = WriteRequest { entry, err_tx: None };
            self.write_tx
                .send(req)
                .map_err(|_| MptDbError::Other("wal write channel closed".into()))?;
            Ok(())
        } else {
            // Synchronous: wait for the background thread to finish writing.
            let (err_tx, err_rx) = bounded(1);
            let req = WriteRequest { entry, err_tx: Some(err_tx) };
            self.write_tx
                .send(req)
                .map_err(|_| MptDbError::Other("wal write channel closed".into()))?;
            err_rx
                .recv()
                .map_err(|_| MptDbError::Other("wal write response channel closed".into()))?
        }
    }

    fn read_at(&self, offset: u64) -> Result<T> {
        self.check_error()?;
        let data = self.log.lock().read(offset)?;
        (self.unmarshal)(&data)
    }

    fn first_offset(&self) -> Result<u64> {
        self.check_error()?;
        Ok(self.log.lock().first_index())
    }

    fn last_offset(&self) -> Result<u64> {
        self.check_error()?;
        Ok(self.log.lock().last_index())
    }

    fn replay(&self, start: u64, end: u64, f: &mut dyn FnMut(u64, T) -> Result<()>) -> Result<()> {
        self.check_error()?;
        for idx in start..=end {
            let entry = self.read_at(idx)?;
            f(idx, entry)?;
        }
        Ok(())
    }

    fn truncate_before(&self, offset: u64) -> Result<()> {
        self.check_error()?;
        self.send_truncate(true, offset)
    }

    fn truncate_after(&self, offset: u64) -> Result<()> {
        self.check_error()?;
        self.send_truncate(false, offset)
    }

    fn close(&mut self) -> Result<()> {
        // If already closed, return cached result.
        {
            let cached = self.close_result.lock();
            if let Some(ref r) = *cached {
                return match r {
                    Ok(()) => Ok(()),
                    Err(e) => Err(MptDbError::Other(format!("{e}"))),
                };
            }
        }

        // Signal the background thread to stop.
        let _ = self.close_tx.send(());

        // Join the worker thread.
        if let Some(handle) = self.worker_handle.lock().take() {
            let _ = handle.join();
        }

        // Close the underlying log.
        let result = self.log.lock().close();

        // Cache and return the result.
        let mut cached = self.close_result.lock();
        match &result {
            Ok(()) => *cached = Some(Ok(())),
            Err(e) => *cached = Some(Err(MptDbError::Other(format!("{e}")))),
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Background thread
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn main_loop<T: Send + 'static>(
    log: Arc<Mutex<WalLog>>,
    marshal: MarshalFn<T>,
    config: WalConfig,
    write_rx: Receiver<WriteRequest<T>>,
    truncate_rx: Receiver<TruncateRequest>,
    close_rx: Receiver<()>,
    async_error: Arc<AtomicBool>,
    async_error_detail: Arc<Mutex<Option<MptDbError>>>,
) {
    let write_batch_size = config.write_batch_size;

    // Set up prune timer if configured.
    let prune_enabled = !config.prune_interval.is_zero() && config.keep_recent > 0;
    let prune_rx = if prune_enabled {
        crossbeam_channel::tick(config.prune_interval)
    } else {
        // Create a channel that never fires.
        crossbeam_channel::never()
    };
    let keep_recent = config.keep_recent;

    loop {
        if async_error.load(Ordering::Relaxed) {
            break;
        }
        select! {
            recv(write_rx) -> msg => {
                match msg {
                    Ok(req) => handle_write(
                        &log, &marshal, &write_rx, write_batch_size,
                        req, &async_error, &async_error_detail,
                    ),
                    Err(_) => break, // channel closed
                }
            }
            recv(truncate_rx) -> msg => {
                match msg {
                    Ok(req) => handle_truncate(
                        &log, req,
                        &async_error, &async_error_detail,
                    ),
                    Err(_) => break,
                }
            }
            recv(prune_rx) -> _ => {
                prune(&log, keep_recent, &async_error, &async_error_detail);
            }
            recv(close_rx) -> _ => {
                break;
            }
        }
    }

    // Drain remaining writes and truncations before exiting.
    drain(
        &log,
        &marshal,
        &write_rx,
        write_batch_size,
        &truncate_rx,
        &async_error,
        &async_error_detail,
    );
}

/// Process remaining requests from both channels before shutdown.
fn drain<T>(
    log: &Arc<Mutex<WalLog>>,
    marshal: &MarshalFn<T>,
    write_rx: &Receiver<WriteRequest<T>>,
    write_batch_size: usize,
    truncate_rx: &Receiver<TruncateRequest>,
    async_error: &Arc<AtomicBool>,
    async_error_detail: &Arc<Mutex<Option<MptDbError>>>,
) {
    // Drain writes, stopping if a fatal error occurs.
    while !async_error.load(Ordering::Relaxed) {
        match write_rx.try_recv() {
            Ok(req) => handle_write(
                log,
                marshal,
                write_rx,
                write_batch_size,
                req,
                async_error,
                async_error_detail,
            ),
            Err(_) => break,
        }
    }
    // Drain truncations.
    while !async_error.load(Ordering::Relaxed) {
        match truncate_rx.try_recv() {
            Ok(req) => handle_truncate(log, req, async_error, async_error_detail),
            Err(_) => break,
        }
    }
}

fn handle_write<T>(
    log: &Arc<Mutex<WalLog>>,
    marshal: &MarshalFn<T>,
    write_rx: &Receiver<WriteRequest<T>>,
    write_batch_size: usize,
    req: WriteRequest<T>,
    async_error: &Arc<AtomicBool>,
    async_error_detail: &Arc<Mutex<Option<MptDbError>>>,
) {
    if write_batch_size > 1 {
        handle_batched_write(
            log,
            marshal,
            write_rx,
            write_batch_size,
            req,
            async_error,
            async_error_detail,
        );
    } else {
        handle_unbatched_write(log, marshal, req, async_error, async_error_detail);
    }
}

/// Gather up to `batch_size` requests starting from `initial`, then marshal
/// and write them all in a single `write_batch` call with one fsync.
fn handle_batched_write<T>(
    log: &Arc<Mutex<WalLog>>,
    marshal: &MarshalFn<T>,
    write_rx: &Receiver<WriteRequest<T>>,
    batch_size: usize,
    initial_req: WriteRequest<T>,
    async_error: &Arc<AtomicBool>,
    async_error_detail: &Arc<Mutex<Option<MptDbError>>>,
) {
    let requests = gather_requests(write_rx, batch_size, initial_req);

    // Marshal all entries.
    let mut batch_data: Vec<Vec<u8>> = Vec::with_capacity(requests.len());
    for (i, req) in requests.iter().enumerate() {
        match (marshal)(&req.entry) {
            Ok(data) => batch_data.push(data),
            Err(e) => {
                report_fatal_error(async_error, async_error_detail, &e);
                // Reply error to all requests.
                for (j, r) in requests.into_iter().enumerate() {
                    if let Some(tx) = r.err_tx {
                        if i == j {
                            let _ = tx.send(Err(MptDbError::Other(format!("{e}"))));
                        } else {
                            let _ = tx.send(Err(MptDbError::Other(
                                "another request failed to marshal, WAL is shutting down"
                                    .to_string(),
                            )));
                        }
                    }
                }
                return;
            }
        }
    }

    // Build (index, data) pairs and write batch.
    let result = {
        let mut guard = log.lock();
        let mut next_index = if guard.last_index() == 0 { 1 } else { guard.last_index() + 1 };
        let entries: Vec<(u64, Vec<u8>)> = batch_data
            .into_iter()
            .map(|data| {
                let idx = next_index;
                next_index += 1;
                (idx, data)
            })
            .collect();
        guard.write_batch(&entries)
    };

    match result {
        Ok(()) => {
            for r in requests {
                if let Some(tx) = r.err_tx {
                    let _ = tx.send(Ok(()));
                }
            }
        }
        Err(e) => {
            report_fatal_error(async_error, async_error_detail, &e);
            for r in requests {
                if let Some(tx) = r.err_tx {
                    let _ = tx.send(Err(MptDbError::Other(format!("{e}"))));
                }
            }
        }
    }
}

/// Collect up to `batch_size` write requests, starting with `initial`.
/// Uses non-blocking try_recv to gather additional pending requests.
fn gather_requests<T>(
    write_rx: &Receiver<WriteRequest<T>>,
    batch_size: usize,
    initial: WriteRequest<T>,
) -> Vec<WriteRequest<T>> {
    let mut requests = Vec::with_capacity(batch_size);
    requests.push(initial);
    while requests.len() < batch_size {
        match write_rx.try_recv() {
            Ok(req) => requests.push(req),
            Err(_) => break,
        }
    }
    requests
}

fn handle_unbatched_write<T>(
    log: &Arc<Mutex<WalLog>>,
    marshal: &MarshalFn<T>,
    req: WriteRequest<T>,
    async_error: &Arc<AtomicBool>,
    async_error_detail: &Arc<Mutex<Option<MptDbError>>>,
) {
    let data = match (marshal)(&req.entry) {
        Ok(d) => d,
        Err(e) => {
            report_fatal_error(async_error, async_error_detail, &e);
            if let Some(tx) = req.err_tx {
                let _ = tx.send(Err(MptDbError::Other(format!("{e}"))));
            }
            return;
        }
    };

    let result = {
        let mut guard = log.lock();
        let index = if guard.last_index() == 0 { 1 } else { guard.last_index() + 1 };
        guard.write(index, &data)
    };

    match result {
        Ok(()) => {
            if let Some(tx) = req.err_tx {
                let _ = tx.send(Ok(()));
            }
        }
        Err(e) => {
            report_fatal_error(async_error, async_error_detail, &e);
            if let Some(tx) = req.err_tx {
                let _ = tx.send(Err(MptDbError::Other(format!("{e}"))));
            }
        }
    }
}

fn handle_truncate(
    log: &Arc<Mutex<WalLog>>,
    req: TruncateRequest,
    async_error: &Arc<AtomicBool>,
    async_error_detail: &Arc<Mutex<Option<MptDbError>>>,
) {
    let result = {
        let mut guard = log.lock();
        if req.before {
            guard.truncate_front(req.index)
        } else {
            guard.truncate_back(req.index)
        }
    };

    match &result {
        Ok(()) => {}
        Err(e) => {
            // Out-of-range / not-found truncations are non-fatal but still
            // reported to the caller.
            if !matches!(e, MptDbError::NotFound(_)) {
                report_fatal_error(async_error, async_error_detail, e);
            }
        }
    }

    let _ = req.err_tx.send(result);
}

/// Auto-prune old WAL entries, keeping only the most recent `keep_recent`.
fn prune(
    log: &Arc<Mutex<WalLog>>,
    keep_recent: u64,
    async_error: &Arc<AtomicBool>,
    async_error_detail: &Arc<Mutex<Option<MptDbError>>>,
) {
    if keep_recent == 0 {
        return;
    }

    let guard = log.lock();
    let last = guard.last_index();
    let first = guard.first_index();

    if last > keep_recent && (last - keep_recent) > first {
        let prune_pos = last - keep_recent;
        drop(guard);
        let mut guard = log.lock();
        if let Err(e) = guard.truncate_front(prune_pos) {
            report_fatal_error(async_error, async_error_detail, &e);
        }
    }
}

fn report_fatal_error(
    async_error: &Arc<AtomicBool>,
    async_error_detail: &Arc<Mutex<Option<MptDbError>>>,
    err: &MptDbError,
) {
    let mut detail = async_error_detail.lock();
    *detail = Some(MptDbError::Other(format!("{err}")));
    async_error.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_marshal() -> impl Fn(&String) -> Result<Vec<u8>> + Send + Sync + 'static {
        |s: &String| Ok(s.as_bytes().to_vec())
    }

    fn make_unmarshal() -> impl Fn(&[u8]) -> Result<String> + Send + Sync + 'static {
        |data: &[u8]| {
            String::from_utf8(data.to_vec())
                .map_err(|e| MptDbError::Other(format!("utf8 error: {e}")))
        }
    }

    fn default_config() -> WalConfig {
        WalConfig { fsync_enabled: false, ..Default::default() }
    }

    fn async_config() -> WalConfig {
        WalConfig {
            write_buffer_size: 256,
            write_batch_size: 8,
            fsync_enabled: false,
            ..Default::default()
        }
    }

    fn new_wal(dir: &std::path::Path) -> WalImpl<String> {
        WalImpl::new(make_marshal(), make_unmarshal(), default_config(), dir).unwrap()
    }

    fn new_async_wal(dir: &std::path::Path) -> WalImpl<String> {
        WalImpl::new(make_marshal(), make_unmarshal(), async_config(), dir).unwrap()
    }

    #[test]
    fn test_sync_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_wal(dir.path());

        wal.write("alpha".to_string()).unwrap();
        wal.write("beta".to_string()).unwrap();
        wal.write("gamma".to_string()).unwrap();

        assert_eq!(wal.read_at(1).unwrap(), "alpha");
        assert_eq!(wal.read_at(2).unwrap(), "beta");
        assert_eq!(wal.read_at(3).unwrap(), "gamma");
    }

    #[test]
    fn test_replay() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_wal(dir.path());

        wal.write("a".to_string()).unwrap();
        wal.write("b".to_string()).unwrap();
        wal.write("c".to_string()).unwrap();

        let mut collected = Vec::new();
        wal.replay(1, 2, &mut |idx, entry| {
            collected.push((idx, entry));
            Ok(())
        })
        .unwrap();

        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0], (1, "a".to_string()));
        assert_eq!(collected[1], (2, "b".to_string()));
    }

    #[test]
    fn test_replay_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_wal(dir.path());

        wal.write("x".to_string()).unwrap();
        wal.write("y".to_string()).unwrap();
        wal.write("z".to_string()).unwrap();

        let mut count = 0;
        let result = wal.replay(1, 3, &mut |_idx, _entry| {
            count += 1;
            if count == 2 {
                Err(MptDbError::Other("stop here".into()))
            } else {
                Ok(())
            }
        });

        assert!(result.is_err());
        assert_eq!(count, 2);
    }

    #[test]
    fn test_truncate_after() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_wal(dir.path());

        wal.write("a".to_string()).unwrap();
        wal.write("b".to_string()).unwrap();
        wal.write("c".to_string()).unwrap();

        wal.truncate_after(2).unwrap();
        assert_eq!(wal.last_offset().unwrap(), 2);

        // Write a new entry — it should get index 3.
        wal.write("d".to_string()).unwrap();
        assert_eq!(wal.last_offset().unwrap(), 3);
        assert_eq!(wal.read_at(3).unwrap(), "d");
    }

    #[test]
    fn test_truncate_before() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_wal(dir.path());

        wal.write("a".to_string()).unwrap();
        wal.write("b".to_string()).unwrap();
        wal.write("c".to_string()).unwrap();

        wal.truncate_before(2).unwrap();
        assert_eq!(wal.first_offset().unwrap(), 2);
        assert_eq!(wal.read_at(2).unwrap(), "b");
    }

    #[test]
    fn test_first_and_last_offset() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_wal(dir.path());

        // Empty WAL returns 0 for both.
        assert_eq!(wal.first_offset().unwrap(), 0);
        assert_eq!(wal.last_offset().unwrap(), 0);

        wal.write("one".to_string()).unwrap();
        wal.write("two".to_string()).unwrap();
        wal.write("three".to_string()).unwrap();

        assert_eq!(wal.first_offset().unwrap(), 1);
        assert_eq!(wal.last_offset().unwrap(), 3);
    }

    #[test]
    fn test_read_at_non_existent() {
        let dir = tempfile::tempdir().unwrap();
        let wal = new_wal(dir.path());

        let result = wal.read_at(999);
        assert!(result.is_err());
    }

    #[test]
    fn test_reopen_and_persist() {
        let dir = tempfile::tempdir().unwrap();

        {
            let mut wal = new_wal(dir.path());
            wal.write("first".to_string()).unwrap();
            wal.write("second".to_string()).unwrap();
            Wal::close(&mut wal).unwrap();
        }

        // Reopen from the same directory.
        let wal = new_wal(dir.path());
        assert_eq!(wal.first_offset().unwrap(), 1);
        assert_eq!(wal.last_offset().unwrap(), 2);
        assert_eq!(wal.read_at(1).unwrap(), "first");
        assert_eq!(wal.read_at(2).unwrap(), "second");

        // Write one more.
        wal.write("third".to_string()).unwrap();
        assert_eq!(wal.last_offset().unwrap(), 3);
        assert_eq!(wal.read_at(3).unwrap(), "third");
    }

    // -----------------------------------------------------------------------
    // T2.3 tests: async writes, batching, pruning
    // -----------------------------------------------------------------------

    #[test]
    fn test_async_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = new_async_wal(dir.path());

        wal.write("a".to_string()).unwrap();
        wal.write("b".to_string()).unwrap();
        wal.write("c".to_string()).unwrap();

        Wal::close(&mut wal).unwrap();

        // Reopen and verify all 3 entries persisted.
        let wal = new_async_wal(dir.path());
        assert_eq!(wal.last_offset().unwrap(), 3);
    }

    #[test]
    fn test_async_write_reopen() {
        let dir = tempfile::tempdir().unwrap();

        {
            let mut wal = new_async_wal(dir.path());
            wal.write("a".to_string()).unwrap();
            wal.write("b".to_string()).unwrap();
            wal.write("c".to_string()).unwrap();
            Wal::close(&mut wal).unwrap();
        }

        // Reopen and write 3 more.
        {
            let mut wal = new_async_wal(dir.path());
            wal.write("d".to_string()).unwrap();
            wal.write("e".to_string()).unwrap();
            wal.write("f".to_string()).unwrap();
            Wal::close(&mut wal).unwrap();
        }

        // Reopen and verify all 6.
        let wal = new_async_wal(dir.path());
        assert_eq!(wal.last_offset().unwrap(), 6);
        assert_eq!(wal.read_at(1).unwrap(), "a");
        assert_eq!(wal.read_at(4).unwrap(), "d");
        assert_eq!(wal.read_at(6).unwrap(), "f");
    }

    #[test]
    fn test_batch_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = new_async_wal(dir.path());

        for i in 0..32 {
            wal.write(format!("entry-{i}")).unwrap();
        }

        Wal::close(&mut wal).unwrap();

        // Reopen and verify all 32 entries via replay.
        let wal = new_async_wal(dir.path());
        assert_eq!(wal.last_offset().unwrap(), 32);

        let mut collected = Vec::new();
        wal.replay(1, 32, &mut |idx, entry| {
            collected.push((idx, entry));
            Ok(())
        })
        .unwrap();

        assert_eq!(collected.len(), 32);
        for (i, (idx, entry)) in collected.iter().enumerate() {
            assert_eq!(*idx, (i + 1) as u64);
            assert_eq!(*entry, format!("entry-{i}"));
        }
    }

    #[test]
    fn test_batch_write_marshal_failure() {
        let dir = tempfile::tempdir().unwrap();

        // Marshal that fails when the entry is "FAIL".
        let marshal = |s: &String| -> Result<Vec<u8>> {
            if s == "FAIL" {
                Err(MptDbError::Other("marshal failed".into()))
            } else {
                Ok(s.as_bytes().to_vec())
            }
        };

        let mut wal = WalImpl::new(marshal, make_unmarshal(), async_config(), dir.path()).unwrap();

        wal.write("ok1".to_string()).unwrap();
        wal.write("FAIL".to_string()).unwrap();

        // Give the background thread time to process the failing entry.
        std::thread::sleep(std::time::Duration::from_millis(100));

        // After a fatal marshal error, subsequent writes should return error.
        let result = wal.write("ok2".to_string());
        assert!(result.is_err(), "expected error after fatal marshal failure");

        // Close should also complete without panic.
        let _ = Wal::close(&mut wal);
    }

    #[test]
    fn test_async_error_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = new_async_wal(dir.path());

        wal.write("hello".to_string()).unwrap();
        wal.write("world".to_string()).unwrap();

        Wal::close(&mut wal).unwrap();

        // No error should have been set.
        assert!(!wal.async_error.load(Ordering::Relaxed));
    }

    #[test]
    fn test_concurrent_close_with_async_writes() {
        let dir = tempfile::tempdir().unwrap();
        let wal = Arc::new(Mutex::new(new_async_wal(dir.path())));

        let barrier = Arc::new(std::sync::Barrier::new(9)); // 8 writers + 1 closer

        let mut handles = Vec::new();

        // Spawn 8 writer threads.
        for t in 0..8 {
            let wal_clone = Arc::clone(&wal);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..10 {
                    let _ = wal_clone.lock().write(format!("t{t}-e{i}"));
                }
            }));
        }

        // Closer thread.
        {
            let wal_clone = Arc::clone(&wal);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                // Small delay to let writers start.
                std::thread::sleep(std::time::Duration::from_millis(10));
                let _ = Wal::close(&mut *wal_clone.lock());
            }));
        }

        for h in handles {
            h.join().expect("thread should not panic");
        }
    }

    #[test]
    fn test_concurrent_truncate_with_prune() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let config = WalConfig {
            write_buffer_size: 256,
            write_batch_size: 8,
            fsync_enabled: false,
            keep_recent: 10,
            prune_interval: Duration::from_millis(10),
            ..Default::default()
        };

        let mut wal = WalImpl::new(make_marshal(), make_unmarshal(), config, dir.path()).unwrap();

        for i in 0..50 {
            wal.write(format!("entry-{i}")).unwrap();
        }

        // Wait for prune timer to fire.
        std::thread::sleep(Duration::from_millis(100));

        // Prune should have advanced first_offset past 1.
        let first = wal.first_offset().unwrap();
        assert!(first > 1, "expected prune to advance first_offset, got {first}");

        Wal::close(&mut wal).unwrap();
    }

    #[test]
    fn test_multiple_close_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut wal = new_async_wal(dir.path());

        wal.write("data".to_string()).unwrap();

        // Close 10 times — should not panic and should return same result.
        for _ in 0..10 {
            let result = Wal::close(&mut wal);
            assert!(result.is_ok());
        }
    }
}
