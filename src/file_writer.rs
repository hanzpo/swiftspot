//! Durable atomic file writes off the UI/engine loop thread.
//!
//! Replacing a file whole and syncing it to disk can stall under heavy I/O.
//! The caller encodes the bytes (cheap) and hands them to [`FileWriter`],
//! which writes them on a dedicated background thread. Writes to the same
//! path stay ordered, and a newer write to a path whose previous write has
//! not started yet replaces it so only the latest contents reach the disk.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long shutdown waits for queued writes to finish.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(4);

thread_local! {
    static UI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as one that must not wait on synchronous disk I/O.
pub fn mark_ui_thread() {
    UI_THREAD.with(|ui| ui.set(true));
}

/// Replaces `path` atomically with `contents` and syncs the file before the
/// rename. Warns in release builds and panics in debug builds if called on a
/// thread marked with [`mark_ui_thread`].
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if UI_THREAD.with(Cell::get) {
        let message = format!(
            "{} was written on the UI thread; hand it to FileWriter instead",
            path.display()
        );
        if cfg!(debug_assertions) {
            panic!("{message}");
        }
        log::error!("{message}");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = temp_path(path);
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(contents)?;
    let _ = file.sync_data();
    drop(file);
    crate::util::replace_file(&temporary, path)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|value| value.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

type WriteFn = dyn Fn(&Path, &[u8]) -> std::io::Result<()> + Send + Sync;

struct Job {
    path: PathBuf,
    contents: Vec<u8>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    writing: bool,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    write: Box<WriteFn>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn run(&self, job: Job) {
        if let Err(error) = (self.write)(&job.path, &job.contents) {
            log::warn!("unable to write {}: {error}", job.path.display());
        }
    }
}

/// Writes files whole and atomically on a dedicated background thread.
pub struct FileWriter {
    shared: Arc<Shared>,
    threaded: bool,
}

impl Default for FileWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FileWriter {
    pub fn new() -> Self {
        Self::with_write(write_atomic)
    }

    /// A writer backed by a custom write closure, for tests.
    pub fn with_write(
        write: impl Fn(&Path, &[u8]) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            write: Box::new(write),
        });
        let worker = Arc::clone(&shared);
        let threaded = std::thread::Builder::new()
            .name("file-writer".into())
            .spawn(move || work(&worker))
            .map_err(|error| log::error!("could not start the file writer: {error}"))
            .is_ok();
        Self { shared, threaded }
    }

    /// Queues `contents` to replace `path`. Coalesces any older queued write
    /// to the same path that has not started yet.
    pub fn write(&self, path: PathBuf, contents: Vec<u8>) {
        if !self.threaded {
            self.shared.run(Job { path, contents });
            return;
        }
        let mut state = self.shared.lock();
        if state.closed {
            drop(state);
            self.shared.run(Job { path, contents });
            return;
        }
        if let Some(existing) = state.queue.iter_mut().find(|job| job.path == path) {
            existing.contents = contents;
        } else {
            state.queue.push_back(Job { path, contents });
        }
        self.shared.changed.notify_one();
    }

    /// Waits up to `timeout` for all queued writes to reach disk.
    pub fn flush(&self, timeout: Duration) -> bool {
        if !self.threaded {
            return true;
        }
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        while !state.queue.is_empty() || state.writing {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, wait) = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = guard;
            if wait.timed_out() && (!state.queue.is_empty() || state.writing) {
                return false;
            }
        }
        true
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        self.flush(FLUSH_TIMEOUT);
        let mut state = self.shared.lock();
        state.closed = true;
        self.shared.changed.notify_all();
    }
}

fn work(shared: &Shared) {
    loop {
        let job = {
            let mut state = shared.lock();
            loop {
                if let Some(job) = state.queue.pop_front() {
                    state.writing = true;
                    break job;
                }
                if state.closed {
                    return;
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };
        shared.run(job);
        let mut state = shared.lock();
        state.writing = false;
        if state.queue.is_empty() {
            shared.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn coalesces_queued_writes_to_the_same_path() {
        let dir = std::env::temp_dir().join(format!("swiftspot-fw-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let calls = Arc::new(AtomicUsize::new(0));
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(Some(hold_rx)));
        let calls_ref = Arc::clone(&calls);
        let writer = FileWriter::with_write(move |p, data| {
            calls_ref.fetch_add(1, Ordering::SeqCst);
            if let Some(rx) = gate.lock().unwrap().take() {
                let _ = rx.recv_timeout(Duration::from_secs(2));
            }
            write_atomic(p, data)
        });
        writer.write(dir.join("blocker.json"), b"0".to_vec());
        std::thread::sleep(Duration::from_millis(20));
        writer.write(path.clone(), b"first".to_vec());
        writer.write(path.clone(), b"second".to_vec());
        writer.write(path.clone(), b"final".to_vec());
        let _ = hold_tx.send(());
        assert!(writer.flush(Duration::from_secs(2)));
        assert_eq!(std::fs::read(&path).unwrap(), b"final");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
