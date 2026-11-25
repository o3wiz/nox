//! Byte-offset indexing: memory grows with line count, not file contents.
use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, Metadata},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub len: u64,
    modified: Option<SystemTime>,
    identity: u64,
}
impl Stamp {
    fn from_metadata(meta: &Metadata) -> Self {
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            meta.ino()
        };
        #[cfg(not(unix))]
        let identity = 0;
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            identity,
        }
    }
    pub fn read(path: &Path) -> Result<Self> {
        Ok(Self::from_metadata(&std::fs::metadata(path)?))
    }
}
#[derive(Debug)]
pub struct LogFile {
    pub path: PathBuf,
    pub stamp: Stamp,
    offsets: Vec<u64>,
    file: Arc<File>,
    pub widest_line: usize,
}
struct PositionedReader {
    file: Arc<File>,
    position: u64,
    end: u64,
}
impl Read for PositionedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = (self.end - self.position).min(buffer.len() as u64) as usize;
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            self.file.read_at(&mut buffer[..available], self.position)?
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            self.file
                .seek_read(&mut buffer[..available], self.position)?
        };
        self.position += n as u64;
        Ok(n)
    }
}
impl LogFile {
    pub fn open(path: &Path, cancel: &AtomicBool) -> Result<Self> {
        let file =
            Arc::new(File::open(path).with_context(|| format!("Cannot open {}", path.display()))?);
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "{} is not a regular file", path.display());
        let stamp = Stamp::from_metadata(&meta);
        let mut reader = BufReader::with_capacity(
            256 * 1024,
            PositionedReader {
                file: file.clone(),
                position: 0,
                end: stamp.len,
            },
        );
        let mut offsets = Vec::new();
        if stamp.len > 0 {
            offsets.push(0);
        }
        let mut position = 0;
        loop {
            ensure!(!cancel.load(Ordering::Relaxed), "Cancelled");
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                break;
            }
            for (index, byte) in buffer.iter().enumerate() {
                if *byte == b'\n' && position + index as u64 + 1 < stamp.len {
                    offsets.push(position + index as u64 + 1);
                }
            }
            let consumed = buffer.len();
            position += consumed as u64;
            reader.consume(consumed);
        }
        ensure!(
            position == stamp.len,
            "File changed while indexing; reload to retry"
        );
        let widest_line = offsets
            .iter()
            .enumerate()
            .max_by_key(|(index, start)| {
                offsets.get(index + 1).copied().unwrap_or(stamp.len) - **start
            })
            .map_or(0, |(index, _)| index);
        Ok(Self {
            widest_line,
            path: path.to_owned(),
            stamp,
            offsets,
            file,
        })
    }
    pub fn line_bytes(&self, index: usize) -> u64 {
        self.offsets.get(index).map_or(0, |start| {
            self.offsets
                .get(index + 1)
                .copied()
                .unwrap_or(self.stamp.len)
                - start
        })
    }
    pub fn replaced_by(&self, new: &Self) -> bool {
        self.stamp.identity != new.stamp.identity
            || self.stamp.len > new.stamp.len
            || (self.stamp.len == new.stamp.len && self.stamp != new.stamp)
    }
    pub fn len(&self) -> usize {
        self.offsets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
    pub fn line(&self, index: usize, max_bytes: usize) -> Result<String> {
        let start = *self.offsets.get(index).context("Line out of range")?;
        let end = self
            .offsets
            .get(index + 1)
            .copied()
            .unwrap_or(self.stamp.len);
        let mut bytes = Vec::new();
        PositionedReader {
            file: self.file.clone(),
            position: start,
            end,
        }
        .take(max_bytes as u64)
        .read_to_end(&mut bytes)?;
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if end - start > max_bytes as u64 {
            text.push_str(" … [display truncated]");
        }
        Ok(text)
    }
    pub fn lines(&self) -> impl BufRead {
        BufReader::with_capacity(
            256 * 1024,
            PositionedReader {
                file: self.file.clone(),
                position: 0,
                end: self.stamp.len,
            },
        )
    }
    pub fn export(&self, indices: &[usize], destination: &Path) -> Result<()> {
        // create_new protects both source logs and existing exports, including hard links.
        let mut output = io::BufWriter::new(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?,
        );
        for &index in indices {
            let start = *self.offsets.get(index).context("Line out of range")?;
            let end = self
                .offsets
                .get(index + 1)
                .copied()
                .unwrap_or(self.stamp.len);
            io::copy(
                &mut PositionedReader {
                    file: self.file.clone(),
                    position: start,
                    end,
                },
                &mut output,
            )?;
            if index + 1 == self.len() && end > start {
                let mut last = [0];
                PositionedReader {
                    file: self.file.clone(),
                    position: end - 1,
                    end,
                }
                .read_exact(&mut last)?;
                if last[0] != b'\n' {
                    output.write_all(b"\n")?;
                }
            }
        }
        output.flush()?;
        Ok(())
    }
}
