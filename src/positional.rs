//! Positional sources: archive bytes read at an offset, with no cursor shared
//! between the readers.
//!
//! An [`ArchiveReader`](crate::ArchiveReader) over a plain [`Read`] + [`Seek`]
//! has one cursor, so it can only ever be reading one folder. A [`ReadAt`]
//! source has none: each read names its offset, so several folders of a
//! non-solid archive can be decoded at once, each on its own thread, from the
//! one open source. That is what
//! [`ArchiveReader::set_positional_source`](crate::ArchiveReader::set_positional_source)
//! hands the reader, and what [`ArchiveReader::from_read_at`](crate::ArchiveReader::from_read_at)
//! builds a reader over.
//!
//! Three kinds of source are provided:
//!
//! - [`std::fs::File`], through `pread` on Unix and `seek_read` on Windows;
//! - bytes in memory: `[u8]`, `Vec<u8>`, and anything behind a `&`, a [`Box`]
//!   or an [`Arc`];
//! - [`SerialReadAt`], the fallback for any other [`Read`] + [`Seek`]: one lock
//!   around the reader, held for a seek and a read. The folders still decode in
//!   parallel; only the reading of packed bytes takes turns.

use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex, PoisonError},
};

/// A source of archive bytes that is read at an offset.
///
/// Every method takes `&self`, and a call must not depend on, or move, any
/// position another call sees: two threads reading two offsets of the same
/// source each get the bytes at their own offset. That is the whole contract,
/// and it is what lets the folders of a non-solid archive decode concurrently.
pub trait ReadAt: Send + Sync {
    /// Reads up to `buf.len()` bytes starting at `offset`, returning how many
    /// were read. Zero at or past the end of the source.
    ///
    /// # Errors
    ///
    /// Whatever the underlying source raises.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// The length of the source in bytes.
    ///
    /// # Errors
    ///
    /// Whatever the underlying source raises.
    fn size(&self) -> io::Result<u64>;
}

impl ReadAt for [u8] {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        let Some(rest) = self.get(start..) else {
            return Ok(0);
        };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }
}

impl ReadAt for Vec<u8> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.as_slice().read_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        (**self).read_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Box<T> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        (**self).read_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Arc<T> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        (**self).read_at(buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        (**self).size()
    }
}

/// `pread`: the file's own offset is neither read nor moved.
#[cfg(unix)]
impl ReadAt for std::fs::File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}

/// `ReadFile` at an offset. Windows moves the handle's own cursor as it does
/// so, which nothing here relies on: every sequential read the reader makes
/// seeks first.
#[cfg(windows)]
impl ReadAt for std::fs::File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}

/// Any [`Read`] + [`Seek`] as a [`ReadAt`], by taking turns: a lock around the
/// reader is held for one seek and one read.
///
/// This is the fallback for a source that has no positional read of its own.
/// The folders it feeds still decode in parallel; only fetching their packed
/// bytes is serialised, which costs little when decoding, not reading, is the
/// work.
#[derive(Debug)]
pub struct SerialReadAt<R> {
    inner: Mutex<R>,
}

impl<R> SerialReadAt<R> {
    /// Wraps `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            inner: Mutex::new(reader),
        }
    }

    /// Takes the reader back.
    pub fn into_inner(self) -> R {
        self.inner
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<R: Read + Seek + Send> ReadAt for SerialReadAt<R> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.seek(SeekFrom::Start(offset))?;
        inner.read(buf)
    }

    fn size(&self) -> io::Result<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.seek(SeekFrom::End(0))
    }
}

/// A [`Read`] + [`Seek`] over a [`ReadAt`], with a cursor of its own.
///
/// Each one is independent: cloning it, or making another over the same
/// source, gives a second cursor that never moves the first. This is the
/// source type of a reader built by
/// [`ArchiveReader::from_read_at`](crate::ArchiveReader::from_read_at).
#[derive(Debug)]
pub struct ReadAtCursor<S: ?Sized> {
    source: Arc<S>,
    pos: u64,
}

impl<S: ?Sized> Clone for ReadAtCursor<S> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            pos: self.pos,
        }
    }
}

impl<S: ReadAt + ?Sized> ReadAtCursor<S> {
    /// A cursor at offset zero of `source`.
    pub fn new(source: Arc<S>) -> Self {
        Self { source, pos: 0 }
    }

    /// The source this cursor reads.
    pub fn source(&self) -> &Arc<S> {
        &self.source
    }
}

impl<S: ReadAt + ?Sized> Read for ReadAtCursor<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.source.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<S: ReadAt + ?Sized> Seek for ReadAtCursor<S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            SeekFrom::End(delta) => self.source.size()?.checked_add_signed(delta),
        };
        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start or past u64::MAX",
            ));
        };
        self.pos = target;
        Ok(target)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn a_slice_reads_at_its_offset_and_ends_cleanly() {
        let bytes: &[u8] = b"0123456789";
        let mut buf = [0u8; 4];
        assert_eq!(bytes.read_at(&mut buf, 3).unwrap(), 4);
        assert_eq!(&buf, b"3456");
        assert_eq!(bytes.read_at(&mut buf, 8).unwrap(), 2);
        assert_eq!(&buf[..2], b"89");
        assert_eq!(bytes.read_at(&mut buf, 10).unwrap(), 0);
        assert_eq!(bytes.read_at(&mut buf, u64::MAX).unwrap(), 0);
    }

    #[test]
    fn two_cursors_over_one_source_do_not_share_a_position() {
        let source: Arc<Vec<u8>> = Arc::new((0u8..=255).collect());
        let mut a = ReadAtCursor::new(Arc::clone(&source));
        let mut b = ReadAtCursor::new(source);
        a.seek(SeekFrom::Start(10)).unwrap();
        b.seek(SeekFrom::End(-6)).unwrap();
        let (mut x, mut y) = ([0u8; 3], [0u8; 3]);
        a.read_exact(&mut x).unwrap();
        b.read_exact(&mut y).unwrap();
        assert_eq!(x, [10, 11, 12]);
        assert_eq!(y, [250, 251, 252]);
        assert_eq!(a.stream_position().unwrap(), 13);
        assert!(a.seek(SeekFrom::Current(-14)).is_err());
    }

    #[test]
    fn the_serial_fallback_reads_what_the_reader_holds() {
        let serial = SerialReadAt::new(Cursor::new(b"abcdefgh".to_vec()));
        let mut buf = [0u8; 3];
        assert_eq!(serial.read_at(&mut buf, 5).unwrap(), 3);
        assert_eq!(&buf, b"fgh");
        assert_eq!(serial.read_at(&mut buf, 1).unwrap(), 3);
        assert_eq!(&buf, b"bcd");
        assert_eq!(serial.size().unwrap(), 8);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_file_reads_at_an_offset() {
        let mut tmp = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut tmp, b"positional bytes").unwrap();
        let mut buf = [0u8; 5];
        assert_eq!(tmp.read_at(&mut buf, 11).unwrap(), 5);
        assert_eq!(&buf, b"bytes");
        assert_eq!(ReadAt::size(&tmp).unwrap(), 16);
    }
}
