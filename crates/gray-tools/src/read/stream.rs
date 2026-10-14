use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio_util::sync::CancellationToken;

use super::hygiene;

pub const READ_CHUNK_BYTES: usize = 64 * 1024;

pub const LINE_BYTE_CAP: usize = super::window::MAX_LINE_CHARS * 4;

pub const COUNT_SKIP_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

pub const MAX_COUNT_LINES: u64 = 100_000;

pub fn should_count_exact(file_size: u64) -> bool {
    file_size <= COUNT_SKIP_LIMIT_BYTES
}

fn count_chars(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u64
}

struct HashRead<R> {
    inner: R,
    hash: Option<DefaultHasher>,
}

impl<R: AsyncRead + Unpin> AsyncRead for HashRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let n = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if let Some(h) = self.hash.as_mut() {
                    h.write(&buf.filled()[n..]);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

pub struct RawLine {
    pub line_no: usize,

    pub bytes: Vec<u8>,

    pub overflow_chars: u64,
}

impl RawLine {
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

pub struct LineStream {
    reader: BufReader<HashRead<File>>,
    cancel: CancellationToken,
    line_no: usize,
    file_size: u64,
    scratch: Vec<u8>,
    binary_note: Option<String>,
    cancelled: bool,
    done: bool,
}

impl LineStream {
    pub async fn open(
        path: &Path,
        display: &str,
        cancel: CancellationToken,
    ) -> std::io::Result<Self> {
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            let std = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)?;
            tokio::fs::File::from_std(std)
        };
        #[cfg(not(unix))]
        let file = File::open(path).await?;
        let meta = file.metadata().await?;
        if !meta.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("read refused: {display} is not a regular file"),
            ));
        }
        let file_size = meta.len();

        let hash = (file_size <= crate::ledger::MAX_HASH_BYTES).then(DefaultHasher::new);
        let mut reader = BufReader::with_capacity(READ_CHUNK_BYTES, HashRead { inner: file, hash });
        let binary_note = {
            let chunk = reader.fill_buf().await?;
            let sample = &chunk[..chunk.len().min(hygiene::SNIFF_SAMPLE_BYTES)];
            hygiene::sniff(sample, display).err()
        };
        let done = binary_note.is_some();
        Ok(Self {
            reader,
            cancel,
            line_no: 0,
            file_size,
            scratch: Vec::new(),
            binary_note,
            cancelled: false,
            done,
        })
    }

    pub async fn next_line(&mut self) -> std::io::Result<Option<RawLine>> {
        if self.done {
            return Ok(None);
        }
        if self.cancel.is_cancelled() {
            self.abandon();
            return Ok(None);
        }
        self.scratch.clear();
        let mut overflow_bytes: u64 = 0;
        let mut overflow_chars: u64 = 0;
        let mut had_newline = false;
        loop {
            if self.cancel.is_cancelled() {
                self.abandon();
                return Ok(None);
            }
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                break;
            }
            let room = (LINE_BYTE_CAP + 1).saturating_sub(self.scratch.len());
            if room == 0 {
                match chunk.iter().position(|&b| b == b'\n') {
                    Some(p) => {
                        overflow_bytes += p as u64;
                        overflow_chars += count_chars(&chunk[..p]);
                        self.reader.consume(p + 1);
                        had_newline = true;
                        break;
                    }
                    None => {
                        overflow_bytes += chunk.len() as u64;
                        overflow_chars += count_chars(chunk);
                        let len = chunk.len();
                        self.reader.consume(len);
                    }
                }
            } else {
                let n = chunk.len().min(room);
                match chunk[..n].iter().position(|&b| b == b'\n') {
                    Some(p) => {
                        self.scratch.extend_from_slice(&chunk[..p + 1]);
                        self.reader.consume(p + 1);
                        had_newline = true;
                        break;
                    }
                    None => {
                        self.scratch.extend_from_slice(&chunk[..n]);
                        self.reader.consume(n);
                    }
                }
            }
        }
        if self.scratch.is_empty() && overflow_bytes == 0 && !had_newline {
            self.done = true;
            return Ok(None);
        }

        if self.scratch.ends_with(b"\n") {
            let len = self.scratch.len();
            self.scratch.truncate(len - 1);
        }
        if self.scratch.ends_with(b"\r") {
            let len = self.scratch.len();
            self.scratch.truncate(len - 1);
        }
        if self.line_no == 0 {
            let bom = self.scratch.len() - hygiene::strip_bom(&self.scratch).len();
            if bom > 0 {
                self.scratch.drain(..bom);
            }
        }
        self.line_no += 1;
        let bytes = std::mem::take(&mut self.scratch);
        Ok(Some(RawLine {
            line_no: self.line_no,
            bytes,
            overflow_chars,
        }))
    }

    async fn drain_rest_capped(
        &mut self,
        cap: Option<u64>,
    ) -> std::io::Result<(u64, bool, bool, bool)> {
        let mut newlines: u64 = 0;
        let mut saw_any = false;
        let mut ended_newline = true;
        loop {
            if self.cancel.is_cancelled() {
                self.abandon();
                break;
            }
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                self.done = true;
                break;
            }
            saw_any = true;
            let chunk_newlines = chunk.iter().filter(|&&b| b == b'\n').count() as u64;
            if let Some(max) = cap
                && newlines + chunk_newlines >= max
            {
                let mut need = max - newlines;
                let mut pos = chunk.len();
                if need == 0 {
                    pos = 0;
                } else {
                    for (i, &b) in chunk.iter().enumerate() {
                        if b == b'\n' {
                            need -= 1;
                            if need == 0 {
                                pos = i + 1;
                                break;
                            }
                        }
                    }
                }
                self.reader.consume(pos);
                return Ok((max, saw_any, false, true));
            }
            newlines += chunk_newlines;
            ended_newline = chunk.last() == Some(&b'\n');
            let len = chunk.len();
            self.reader.consume(len);
        }
        Ok((newlines, saw_any, ended_newline, false))
    }

    pub async fn count_rest_lines_capped(
        &mut self,
        max_lines: u64,
    ) -> std::io::Result<(u64, bool)> {
        let (newlines, saw_any, ended_newline, capped) =
            self.drain_rest_capped(Some(max_lines)).await?;
        if capped {
            return Ok((newlines, true));
        }
        Ok((newlines + u64::from(saw_any && !ended_newline), false))
    }

    pub fn discard_hash(&mut self) {
        self.reader.get_mut().hash = None;
    }

    pub fn content_hash(&mut self) -> Option<u64> {
        self.reader.get_mut().hash.take().map(|h| h.finish())
    }

    fn abandon(&mut self) {
        self.cancelled = true;
        self.done = true;
        self.scratch.clear();
    }

    pub fn line_no(&self) -> usize {
        self.line_no
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn binary_note(&self) -> Option<&str> {
        self.binary_note.as_deref()
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled
    }
}

#[path = "stream_tests.rs"]
#[cfg(test)]
mod tests;
