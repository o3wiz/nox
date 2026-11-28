//! Streaming standard-input spooling backed by a temporary file so piped logs
//! work with the same byte-offset indexing, search, and live-follow pipeline as
//! regular files.
use anyhow::{Context, Result};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Returns `true` when standard input is connected to a pipe, redirected file,
/// or socket rather than an interactive terminal or null device.
pub fn has_piped_input() -> bool {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::{fd::AsFd, unix::fs::FileTypeExt};
        let Ok(owned) = stdin.as_fd().try_clone_to_owned() else {
            return false;
        };
        let Ok(meta) = std::fs::File::from(owned).metadata() else {
            return false;
        };
        let file_type = meta.file_type();
        file_type.is_fifo() || file_type.is_file() || file_type.is_socket()
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        const FILE_TYPE_DISK: u32 = 0x0001;
        const FILE_TYPE_PIPE: u32 = 0x0003;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFileType(h_file: *mut std::ffi::c_void) -> u32;
        }
        let handle = stdin.as_handle().as_raw_handle();
        if handle.is_null() || handle as isize == -1 {
            return false;
        }
        // SAFETY: `handle` is the valid borrowed standard input handle from `io::stdin()`.
        let file_type = unsafe { GetFileType(handle) };
        matches!(file_type, FILE_TYPE_DISK | FILE_TYPE_PIPE)
    }

    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

#[derive(Debug, Default)]
struct StdinState {
    bytes_written: AtomicU64,
    finished: AtomicBool,
    stop: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Handle to a background reader spooling piped input into a temporary file.
/// Dropping the handle signals the reader to stop and removes the temporary file.
#[derive(Debug)]
pub struct StdinSpool {
    path: PathBuf,
    state: Arc<StdinState>,
    _temp_file: tempfile::NamedTempFile,
}

impl StdinSpool {
    /// Starts spooling the process's standard input in a background thread.
    pub fn spawn_stdin() -> Result<Self> {
        Self::spawn_reader(io::stdin())
    }

    /// Starts spooling an arbitrary `Read` source in a background thread.
    pub fn spawn_reader<R: Read + Send + 'static>(mut reader: R) -> Result<Self> {
        let temp_file = tempfile::Builder::new()
            .prefix("nox-stdin-")
            .suffix(".log")
            .tempfile()
            .context("Cannot create temporary file for stdin")?;
        let path = temp_file.path().to_path_buf();
        let mut writer = temp_file
            .as_file()
            .try_clone()
            .context("Cannot duplicate temporary file handle for stdin")?;
        let state = Arc::new(StdinState::default());
        let thread_state = Arc::clone(&state);

        std::thread::Builder::new()
            .name("nox-stdin".into())
            .spawn(move || {
                let mut buffer = vec![0u8; 64 * 1024];
                let mut total = 0u64;
                loop {
                    if thread_state.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if thread_state.stop.load(Ordering::Relaxed) {
                                break;
                            }
                            if let Err(error) = writer.write_all(&buffer[..n]) {
                                *thread_state.error.lock().unwrap() =
                                    Some(format!("Writing stdin buffer: {error}"));
                                break;
                            }
                            total += n as u64;
                            thread_state.bytes_written.store(total, Ordering::Release);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            *thread_state.error.lock().unwrap() =
                                Some(format!("Reading stdin: {error}"));
                            break;
                        }
                    }
                }
                let _ = writer.flush();
                drop(writer);
                thread_state.finished.store(true, Ordering::Release);
            })
            .context("Cannot start stdin reader thread")?;

        Ok(Self {
            path,
            state,
            _temp_file: temp_file,
        })
    }

    /// Path to the backing temporary file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of bytes flushed to the backing file so far.
    pub fn bytes_written(&self) -> u64 {
        self.state.bytes_written.load(Ordering::Acquire)
    }

    /// Returns `true` once the input stream has reached EOF, stopped, or errored.
    pub fn is_finished(&self) -> bool {
        self.state.finished.load(Ordering::Acquire)
    }

    /// Returns the first read or write error encountered while spooling, if any.
    pub fn error(&self) -> Option<String> {
        self.state.error.lock().unwrap().clone()
    }
}

impl Drop for StdinSpool {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logfile::LogFile;
    use std::time::{Duration, Instant};

    fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + timeout;
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for condition");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn spools_finite_input_indexes_with_logfile_and_cleans_up_on_drop() {
        let input = b"first line\nsecond line\nthird line";
        let spool = StdinSpool::spawn_reader(io::Cursor::new(input.to_vec())).unwrap();
        wait_until(Duration::from_secs(2), || spool.is_finished());

        assert_eq!(spool.bytes_written(), input.len() as u64);
        assert_eq!(spool.error(), None);

        let path = spool.path().to_path_buf();
        let log = LogFile::open(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log.line(0, 100).unwrap(), "first line");
        assert_eq!(log.line(1, 100).unwrap(), "second line");
        assert_eq!(log.line(2, 100).unwrap(), "third line");

        drop(log);
        drop(spool);
        assert!(!path.exists(), "temp spool file should be deleted on drop");
    }

    #[test]
    fn streams_incrementally_from_live_pipe_until_eof() {
        let (reader, mut writer) = io::pipe().unwrap();
        let spool = StdinSpool::spawn_reader(reader).unwrap();

        assert!(!spool.is_finished());

        writer.write_all(b"alpha\nbeta\n").unwrap();
        writer.flush().unwrap();
        wait_until(Duration::from_secs(2), || spool.bytes_written() >= 11);
        assert!(!spool.is_finished());

        let first = LogFile::open(spool.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first.line(0, 100).unwrap(), "alpha");
        assert_eq!(first.line(1, 100).unwrap(), "beta");

        writer.write_all(b"gamma\n").unwrap();
        writer.flush().unwrap();
        drop(writer);

        wait_until(Duration::from_secs(2), || spool.is_finished());
        assert_eq!(spool.bytes_written(), 17);
        assert_eq!(spool.error(), None);

        let second = LogFile::open(spool.path(), &AtomicBool::new(false)).unwrap();
        assert!(!first.replaced_by(&second));
        assert_eq!(second.len(), 3);
        assert_eq!(second.line(2, 100).unwrap(), "gamma");
    }

    #[test]
    fn records_reader_errors_and_marks_stream_finished() {
        struct FailingReader {
            sent: bool,
        }
        impl Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.sent {
                    self.sent = true;
                    let chunk = b"partial\n";
                    buf[..chunk.len()].copy_from_slice(chunk);
                    Ok(chunk.len())
                } else {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "boom"))
                }
            }
        }

        let spool = StdinSpool::spawn_reader(FailingReader { sent: false }).unwrap();
        wait_until(Duration::from_secs(2), || spool.is_finished());

        assert_eq!(spool.bytes_written(), 8);
        assert!(
            spool
                .error()
                .as_deref()
                .is_some_and(|err| err.contains("Reading stdin") && err.contains("boom"))
        );
    }
}
