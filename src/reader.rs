use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    fs::File,
    io,
    io::{Read, Seek, SeekFrom, Write},
    rc::Rc,
    sync::{Arc, OnceLock},
};

use lzma_turbo::crc::{Crc32, crc32};

mod positional_block;
pub use positional_block::PositionalBlockDecoder;
#[cfg(all(feature = "util", not(target_arch = "wasm32")))]
mod extract;
#[cfg(not(target_arch = "wasm32"))]
mod store_overlap;

use crate::{
    ByteReader, Password,
    archive::*,
    bitset::BitSet,
    block::*,
    codec::filter::bcj2::Bcj2Reader,
    codec::lzma_turbo::{Lzma2Control, Lzma2Handle, Lzma2Progress},
    container::{ArchiveLimits, BlockCompletion, SubStreamCompletion},
    decoder::{
        DecodeOptions, DecodeRead, add_decoder, check_chain_memory, unsized_coders_memory_kb,
    },
    error::{Error, Limit},
    pipeline::{Chain, Stage},
    positional::{ReadAt, ReadAtCursor},
};

/// Upper bound for eagerly pre-allocating an output buffer from an archive-declared
/// (untrusted) uncompressed size. The buffer still grows to the real size as data is
/// read; this only stops a tiny archive from forcing a huge up-front allocation.
const MAX_PREALLOC_BYTES: usize = 4 << 20;

pub struct BoundedReader<R: Read> {
    inner: R,
    remain: usize,
}

impl<R: Read> BoundedReader<R> {
    pub fn new(inner: R, max_size: usize) -> Self {
        Self {
            inner,
            remain: max_size,
        }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remain == 0 || buf.is_empty() {
            return Ok(0);
        }
        let bound = buf.len().min(self.remain);
        let size = self.inner.read(&mut buf[..bound])?;
        if size == 0 {
            return Err(ended_early());
        }
        self.remain -= size;
        Ok(size)
    }
}

impl<R: DecodeRead> DecodeRead for BoundedReader<R> {
    fn supports_borrowed(&self) -> bool {
        self.inner.supports_borrowed()
    }

    fn read_borrowed(
        &mut self,
        max: usize,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Option<io::Result<usize>> {
        if self.remain == 0 || max == 0 {
            return Some(Ok(0));
        }
        let limit = max.min(self.remain);
        let remain = &mut self.remain;
        let read = self.inner.read_borrowed(limit, &mut |bytes| {
            *remain -= bytes.len();
            sink(bytes)
        })?;
        Some(read.and_then(|size| {
            if size == 0 {
                Err(ended_early())
            } else {
                Ok(size)
            }
        }))
    }

    fn push(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) -> Option<io::Result<usize>> {
        if self.remain == 0 || max == 0 {
            return Some(Ok(0));
        }
        let size = match self.inner.push(max.min(self.remain), sink)? {
            Ok(0) => return Some(Err(ended_early())),
            Ok(size) => size,
            Err(e) => return Some(Err(e)),
        };
        self.remain -= size;
        Some(Ok(size))
    }
}

/// The error of a stream that ended while bytes its header declared were
/// still owed.
///
/// A coder that keeps its own count, LZMA or LZMA2, reports a short stream
/// itself. Copy cannot, and neither can a filter over it: they hand on what
/// their input gives them. So a source cut short, or a header that declares
/// more than was packed, used to end such a stream with `Ok(0)`, and its
/// files came out short with nothing said: a file's CRC is compared once
/// every byte it declares has been read, and that read never came. It is the
/// same error a truncated LZMA stream gives.
fn ended_early() -> io::Error {
    io::ErrorKind::UnexpectedEof.into()
}

/// Fails the read that finds a block's decoded stream ended before the bytes
/// its header declares have come out of it.
///
/// Reads beyond the declared size are passed on as they are: the read that
/// lets an LZMA2 coder reach its end marker is one of them.
struct DeclaredLength<R> {
    inner: R,
    owed: u64,
}

impl<R: Read> Read for DeclaredLength<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let size = self.inner.read(buf)?;
        if size == 0 && self.owed > 0 {
            return Err(ended_early());
        }
        self.owed = self.owed.saturating_sub(size as u64);
        Ok(size)
    }
}

impl<R: DecodeRead> DecodeRead for DeclaredLength<R> {
    fn supports_borrowed(&self) -> bool {
        self.inner.supports_borrowed()
    }

    fn read_borrowed(
        &mut self,
        max: usize,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Option<io::Result<usize>> {
        if max == 0 {
            return Some(Ok(0));
        }
        let owed = &mut self.owed;
        let read = self.inner.read_borrowed(max, &mut |bytes| {
            *owed = owed.saturating_sub(bytes.len() as u64);
            sink(bytes)
        })?;
        Some(read.and_then(|size| {
            if size == 0 && self.owed > 0 {
                Err(ended_early())
            } else {
                Ok(size)
            }
        }))
    }

    fn push(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) -> Option<io::Result<usize>> {
        if max == 0 {
            return Some(Ok(0));
        }
        let size = match self.inner.push(max, sink)? {
            Ok(size) => size,
            Err(e) => return Some(Err(e)),
        };
        if size == 0 && self.owed > 0 {
            return Some(Err(ended_early()));
        }
        self.owed = self.owed.saturating_sub(size as u64);
        Some(Ok(size))
    }
}

/// A special reader that shares it's inner reader with other instances and
/// needs to re-seek every read operation.
#[derive(Debug)]
pub(crate) struct SharedBoundedReader<'a, R> {
    inner: Rc<RefCell<&'a mut R>>,
    cur: u64,
    bounds: (u64, u64),
}

impl<'a, R> Clone for SharedBoundedReader<'a, R> {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
            cur: self.cur,
            bounds: self.bounds,
        }
    }
}

impl<'a, R: Read + Seek> Seek for SharedBoundedReader<'a, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(pos) => self.bounds.0 as i64 + pos as i64,
            SeekFrom::End(pos) => self.bounds.1 as i64 + pos,
            SeekFrom::Current(pos) => self.cur as i64 + pos,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start",
            ));
        }
        self.cur = new_pos as u64;
        self.inner.borrow_mut().seek(SeekFrom::Start(self.cur))
    }
}

impl<'a, R: Read + Seek> Read for SharedBoundedReader<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cur >= self.bounds.1 || buf.is_empty() {
            return Ok(0);
        }

        let mut inner = self.inner.borrow_mut();

        inner.seek(SeekFrom::Start(self.cur))?;

        let bound = buf.len().min((self.bounds.1 - self.cur) as usize);
        let size = inner.read(&mut buf[..bound])?;
        if size == 0 {
            return Err(ended_early());
        }
        self.cur += size as u64;
        Ok(size)
    }
}

impl<'a, R: Read + Seek> SharedBoundedReader<'a, R> {
    fn new(inner: Rc<RefCell<&'a mut R>>, bounds: (u64, u64)) -> Self {
        Self {
            inner,
            cur: bounds.0,
            bounds,
        }
    }
}

/// Wraps the decode chain so that a failure can be told apart from a failure
/// in the caller's own callback.
///
/// An entry is handed to the caller as a `&mut dyn Read`; when the caller then
/// returns an error there is otherwise no way to know whether the archive was
/// damaged or the caller's own sink failed, and only the first of those may be
/// reported as a block failure.
struct FaultRecordingReader<R> {
    inner: R,
    faulted: Rc<Cell<bool>>,
}

impl<R: Read> Read for FaultRecordingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf).inspect_err(|_| self.faulted.set(true))
    }
}

impl<R: DecodeRead> DecodeRead for FaultRecordingReader<R> {
    fn supports_borrowed(&self) -> bool {
        self.inner.supports_borrowed()
    }

    fn read_borrowed(
        &mut self,
        max: usize,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Option<io::Result<usize>> {
        let read = self.inner.read_borrowed(max, sink)?;
        Some(read.inspect_err(|_| self.faulted.set(true)))
    }

    fn push(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) -> Option<io::Result<usize>> {
        let pushed = self.inner.push(max, sink)?;
        Some(pushed.inspect_err(|_| self.faulted.set(true)))
    }
}

/// An entry's bytes, as a [`ArchiveReader::for_each_entries_direct`] or
/// [`BlockDecoder::for_each_entries_direct`] callback is handed them.
///
/// It is a [`Read`], and [`EntryRead::write_rest`] is the other way to take
/// the bytes: it writes them on from the decoder's own output wherever the
/// decoder keeps one, so a consumer that only writes an entry somewhere needs
/// no buffer of its own and copies nothing into one.
pub trait EntryRead: Read {
    /// Writes the rest of the entry to `sink`, and returns how many bytes
    /// that was.
    ///
    /// The bytes are handed to `sink` in the pieces the decoder made them in,
    /// as large as a whole block of its output, so `sink` is best unbuffered:
    /// a `BufWriter` passes a write at least as large as its buffer straight
    /// through when it holds nothing, and is only a copy otherwise.
    ///
    /// # Errors
    ///
    /// What reading the entry fails with, as [`Read::read`] reports it, or
    /// what `sink` does.
    fn write_rest(&mut self, sink: &mut dyn Write) -> io::Result<u64> {
        copy_rest(self, sink)
    }
}

/// Bytes [`copy_rest`] reads at a time, on the stack.
const COPY_REST_BYTES: usize = 64 << 10;

/// [`EntryRead::write_rest`] for a reader that keeps no output to write from:
/// read, then written, a piece at a time.
fn copy_rest<R: Read + ?Sized>(reader: &mut R, sink: &mut dyn Write) -> io::Result<u64> {
    let mut buf = [0u8; COPY_REST_BYTES];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        sink.write_all(&buf[..n])?;
        total += n as u64;
    }
}

/// Crosses one dictionary boundary with exactly the ordinary read request.
/// Its complete successful chunk is verified before it reaches the sink.
fn copy_once<R: Read + ?Sized>(reader: &mut R, sink: &mut dyn Write) -> io::Result<usize> {
    let mut buf = [0u8; COPY_REST_BYTES];
    let n = loop {
        match reader.read(&mut buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => break result?,
        }
    };
    if n == 0 {
        return Ok(0);
    }
    sink.write_all(&buf[..n])?;
    Ok(n)
}

/// Bytes one [`DecodeRead::push`] of [`write_pushed`] asks for: a block of
/// the parallel decoder's output, after which the coder gets to read ahead
/// again before the next.
const PUSH_STEP_BYTES: usize = 1 << 20;

/// [`EntryRead::write_rest`] for a decode chain: each block of output written
/// to `sink` from the coder that made it, or read and copied when the chain
/// keeps no output of its own.
///
/// `sink` is not told about the decoder's errors, nor the decoder about
/// `sink`'s: a write that fails stops the walk with that error, after the
/// push it failed in, and the chain's own recording of its failures never
/// sees it.
fn write_pushed<R: DecodeRead + ?Sized>(reader: &mut R, sink: &mut dyn Write) -> io::Result<u64> {
    let mut total = 0u64;
    let borrowed_supported = reader.supports_borrowed();
    loop {
        let mut failed = None;
        let mut deliver = |bytes: &[u8]| {
            if failed.is_none()
                && let Err(e) = sink.write_all(bytes)
            {
                failed = Some(e);
            }
        };
        let borrowed = if borrowed_supported {
            reader.read_borrowed(COPY_REST_BYTES, &mut |bytes| {
                deliver(bytes);
                Ok(())
            })
        } else {
            None
        };
        let pushed = match borrowed {
            Some(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Some(result) => Some(result),
            None if borrowed_supported => None,
            None => reader.push(PUSH_STEP_BYTES, &mut deliver),
        };
        let Some(pushed) = pushed else {
            if borrowed_supported {
                // A transient dictionary boundary is not a permanent barrier.
                // Preserve this entire Read chunk, then try the next span.
                match copy_once(reader, sink)? {
                    0 => return Ok(total),
                    n => total += n as u64,
                }
                continue;
            }
            return copy_rest(reader, sink).map(|rest| total + rest);
        };
        if let Some(e) = failed {
            return Err(e);
        }
        match pushed? {
            0 => return Ok(total),
            n => total += n as u64,
        }
    }
}

impl<R: DecodeRead> EntryRead for FaultRecordingReader<R> {
    fn write_rest(&mut self, sink: &mut dyn Write) -> io::Result<u64> {
        write_pushed(self, sink)
    }
}

impl EntryRead for &[u8] {
    fn write_rest(&mut self, sink: &mut dyn Write) -> io::Result<u64> {
        sink.write_all(self)?;
        let n = self.len() as u64;
        *self = &self[self.len()..];
        Ok(n)
    }
}

struct Crc32VerifyingReader<R> {
    inner: R,
    crc_digest: Crc32,
    expected_value: u64,
    remaining: i64,
    /// Where to leave the checksum once it is final, for a caller that wants
    /// the value and not only the verdict. See [`SubStreamCompletion`].
    report: Option<Rc<Cell<Option<u32>>>>,
}

impl<R: Read> Crc32VerifyingReader<R> {
    fn new(inner: R, remaining: usize, expected_value: u64) -> Self {
        Self {
            inner,
            crc_digest: Crc32::new(),
            expected_value,
            remaining: remaining as i64,
            report: None,
        }
    }

    /// Same, and hands the computed checksum to `report` when it matches.
    fn reporting(
        inner: R,
        remaining: usize,
        expected_value: u64,
        report: Rc<Cell<Option<u32>>>,
    ) -> Self {
        Self {
            report: Some(report),
            ..Self::new(inner, remaining, expected_value)
        }
    }
}

impl<R: Read> Read for Crc32VerifyingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining <= 0 {
            return Ok(0);
        }
        let size = self.inner.read(buf)?;
        if size > 0 {
            self.remaining -= size as i64;
            self.crc_digest.update(&buf[..size]);
        }
        self.check_at_end()?;
        Ok(size)
    }
}

impl<R: DecodeRead> DecodeRead for Crc32VerifyingReader<R> {
    fn supports_borrowed(&self) -> bool {
        self.inner.supports_borrowed()
    }

    fn read_borrowed(
        &mut self,
        max: usize,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Option<io::Result<usize>> {
        if self.remaining <= 0 {
            return Some(Ok(0));
        }
        let digest = &mut self.crc_digest;
        let remaining = &mut self.remaining;
        let expected = self.expected_value;
        let report = self.report.as_ref();
        self.inner.read_borrowed(max, &mut |bytes| {
            *remaining -= bytes.len() as i64;
            digest.update(bytes);
            Self::check_parts(digest, *remaining, expected, report)?;
            sink(bytes)
        })
    }

    fn push(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) -> Option<io::Result<usize>> {
        if self.remaining <= 0 {
            return Some(Ok(0));
        }
        let digest = &mut self.crc_digest;
        let pushed = self.inner.push(max, &mut |bytes| {
            digest.update(bytes);
            sink(bytes);
        })?;
        let size = match pushed {
            Ok(size) => size,
            Err(e) => return Some(Err(e)),
        };
        self.remaining -= size as i64;
        Some(self.check_at_end().map(|()| size))
    }
}

impl<R> Crc32VerifyingReader<R> {
    /// Compares the checksum once every byte it covers has been through.
    fn check_at_end(&mut self) -> io::Result<()> {
        Self::check_parts(
            &mut self.crc_digest,
            self.remaining,
            self.expected_value,
            self.report.as_ref(),
        )
    }

    fn check_parts(
        digest: &mut Crc32,
        remaining: i64,
        expected: u64,
        report: Option<&Rc<Cell<Option<u32>>>>,
    ) -> io::Result<()> {
        if remaining <= 0 {
            let d = std::mem::replace(digest, Crc32::new()).finalize();
            if d as u64 != expected {
                return Err(std::io::Error::other(Error::ChecksumVerificationFailed));
            }
            if let Some(report) = report {
                report.set(Some(d));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod borrowed_delivery_tests {
    use super::*;
    use crate::codec::lzma_turbo::{Lzma2Plan, lzma2_decoder};

    fn packed(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, chunk) in data.chunks(65536).enumerate() {
            out.push(if index == 0 { 1 } else { 2 });
            out.extend_from_slice(&((chunk.len() - 1) as u16).to_be_bytes());
            out.extend_from_slice(chunk);
        }
        out.push(0);
        out
    }

    fn chain(
        input: Vec<u8>,
        len: usize,
        expected: u32,
        prop: u8,
        fault: Rc<Cell<bool>>,
        report: Rc<Cell<Option<u32>>>,
    ) -> impl DecodeRead {
        let decoder =
            lzma2_decoder(io::Cursor::new(input), prop, Lzma2Plan::SingleThreaded).unwrap();
        let inner: Box<dyn DecodeRead> = Box::new(DeclaredLength {
            inner: decoder,
            owed: len as u64,
        });
        FaultRecordingReader {
            inner: Crc32VerifyingReader::reporting(
                BoundedReader::new(inner, len),
                len,
                u64::from(expected),
                report,
            ),
            faulted: fault,
        }
    }

    #[derive(Default)]
    struct Sink {
        bytes: Vec<u8>,
        fail_after: Option<usize>,
        interrupted: bool,
    }

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let left = self
                .fail_after
                .map_or(usize::MAX, |n| n.saturating_sub(self.bytes.len()));
            if left == 0 {
                return Err(io::Error::other("output marker"));
            }
            let n = bytes.len().min(997).min(left);
            self.bytes.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Counted<R> {
        inner: R,
        borrowed_calls: usize,
        declined: usize,
        resumed: usize,
        reads: usize,
    }

    impl<R: Read> Read for Counted<R> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            self.inner.read(out)
        }
    }

    impl<R: DecodeRead> DecodeRead for Counted<R> {
        fn supports_borrowed(&self) -> bool {
            self.inner.supports_borrowed()
        }
        fn read_borrowed(
            &mut self,
            max: usize,
            sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
        ) -> Option<io::Result<usize>> {
            self.borrowed_calls += 1;
            let result = self.inner.read_borrowed(max, sink);
            if result.is_none() {
                self.declined += 1;
            }
            if matches!(result, Some(Ok(n)) if n > 0) && self.declined > 0 {
                self.resumed += 1;
            }
            result
        }
    }

    struct RecordedSink {
        inner: Sink,
        requests: Vec<usize>,
    }

    impl Write for RecordedSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.requests.push(bytes.len());
            self.inner.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn borrowed_delivery_resumes_after_wrap_with_exact_read_visibility() {
        let data: Vec<u8> = (0..600003).map(|i| (i * 31) as u8).collect();
        let valid = packed(&data);
        let mut damaged = valid.clone();
        damaged[5 * 65539] = 3;
        for (case, input) in [valid.clone(), damaged, valid[..350000].to_vec()]
            .into_iter()
            .enumerate()
        {
            for prop in [0, 12] {
                for bad_crc in [false, true] {
                    for fail_after in [None, Some(0), Some(300001)] {
                        for prefix in [0, 7] {
                            let mut observed = Vec::new();
                            for direct in [false, true] {
                                let fault = Rc::new(Cell::new(false));
                                let report = Rc::new(Cell::new(None));
                                let inner = chain(
                                    input.clone(),
                                    data.len(),
                                    crc32(&data) ^ u32::from(bad_crc),
                                    prop,
                                    Rc::clone(&fault),
                                    Rc::clone(&report),
                                );
                                let mut reader = Counted {
                                    inner,
                                    borrowed_calls: 0,
                                    declined: 0,
                                    resumed: 0,
                                    reads: 0,
                                };
                                reader.read_exact(&mut vec![0; prefix]).unwrap();
                                let mut sink = RecordedSink {
                                    inner: Sink {
                                        fail_after,
                                        ..Sink::default()
                                    },
                                    requests: Vec::new(),
                                };
                                let result = if direct {
                                    write_pushed(&mut reader, &mut sink)
                                } else {
                                    copy_rest(&mut reader, &mut sink)
                                };
                                if direct
                                    && case == 0
                                    && prop == 12
                                    && !bad_crc
                                    && fail_after.is_none()
                                {
                                    assert!(
                                        reader.declined > 1 && reader.resumed > 1,
                                        "the real decoder must resume across repeated wraps"
                                    );
                                }
                                observed.push((
                                    sink.inner.bytes,
                                    sink.requests,
                                    result.map_err(|e| (e.kind(), e.to_string())),
                                    fault.get(),
                                    report.get(),
                                ));
                            }
                            assert_eq!(
                                observed[0], observed[1],
                                "case={case} prop={prop} bad_crc={bad_crc} fail={fail_after:?} prefix={prefix}"
                            );
                        }
                    }
                }
            }
        }
        let inner = crate::decoder::ReadOnly(io::Cursor::new(data.clone()));
        let mut reader = Counted {
            inner,
            borrowed_calls: 0,
            declined: 0,
            resumed: 0,
            reads: 0,
        };
        let mut out = Vec::new();
        assert_eq!(
            write_pushed(&mut reader, &mut out).unwrap(),
            data.len() as u64
        );
        assert_eq!(out, data);
        assert_eq!(
            reader.borrowed_calls, 0,
            "a static barrier is never probed for a borrowed span"
        );

        struct Eof;
        impl Read for Eof {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        impl DecodeRead for Eof {
            fn supports_borrowed(&self) -> bool {
                true
            }
        }
        struct RejectEmptyWrite;
        impl Write for RejectEmptyWrite {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                panic!("EOF must not write")
            }
            fn write_all(&mut self, _: &[u8]) -> io::Result<()> {
                panic!("EOF must not invoke overridden write_all")
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(write_pushed(&mut Eof, &mut RejectEmptyWrite).unwrap(), 0);
        assert_eq!(copy_rest(&mut Eof, &mut RejectEmptyWrite).unwrap(), 0);
    }

    #[test]
    fn borrowed_delivery_preserves_exact_crc_error_prefix_and_sink_precedence() {
        for len in [0, 17003, 65536, 140003] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31) as u8).collect();
            for prop in [0, 16] {
                for bad_crc in [false, true] {
                    for fail_after in [None, Some(0), Some(70001)] {
                        for prefix in [0, len.min(7)] {
                            let checksum = crc32(&data) ^ u32::from(bad_crc);
                            let mut observed = Vec::new();
                            for direct in [false, true] {
                                let fault = Rc::new(Cell::new(false));
                                let report = Rc::new(Cell::new(None));
                                let mut reader = chain(
                                    packed(&data),
                                    len,
                                    checksum,
                                    prop,
                                    Rc::clone(&fault),
                                    Rc::clone(&report),
                                );
                                let mut head = vec![0; prefix];
                                reader.read_exact(&mut head).unwrap();
                                let mut sink = Sink {
                                    fail_after,
                                    ..Sink::default()
                                };
                                let result = if direct {
                                    write_pushed(&mut reader, &mut sink)
                                } else {
                                    copy_rest(&mut reader, &mut sink)
                                };
                                observed.push((
                                    sink.bytes,
                                    result.map_err(|e| (e.kind(), e.to_string())),
                                    fault.get(),
                                    report.get(),
                                ));
                            }
                            assert_eq!(
                                observed[0], observed[1],
                                "len={len} prop={prop} bad_crc={bad_crc} prefix={prefix} fail={fail_after:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn borrowed_delivery_withholds_failed_decode_and_unwinds_borrowed_sink() {
        let data = vec![42; 140003];
        let mut damaged = packed(&data);
        damaged[65539] = 3; // Invalid control after the first stored chunk.
        let truncated = packed(&data)[..70000].to_vec();
        for input in [damaged, truncated] {
            let mut observed = Vec::new();
            for direct in [false, true] {
                let fault = Rc::new(Cell::new(false));
                let mut reader = chain(
                    input.clone(),
                    data.len(),
                    crc32(&data),
                    16,
                    Rc::clone(&fault),
                    Rc::new(Cell::new(None)),
                );
                let mut sink = Vec::new();
                let result = if direct {
                    write_pushed(&mut reader, &mut sink)
                } else {
                    copy_rest(&mut reader, &mut sink)
                };
                observed.push((
                    sink,
                    result.map_err(|e| (e.kind(), e.to_string())),
                    fault.get(),
                ));
            }
            assert_eq!(observed[0], observed[1]);
        }
        struct Panics;
        impl Write for Panics {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                panic!("output panic marker")
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let bytes = b"borrowed output";
        let fault = Rc::new(Cell::new(false));
        let mut reader = chain(
            packed(bytes),
            bytes.len(),
            crc32(bytes),
            0,
            Rc::clone(&fault),
            Rc::new(Cell::new(None)),
        );
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_pushed(&mut reader, &mut Panics)
        }));
        assert!(panic.is_err());
        assert!(!fault.get());
    }

    #[cfg(feature = "compress")]
    #[test]
    fn borrowed_archive_delivery_preserves_filters_crc_hooks_and_partial_stop() {
        use crate::encoder_options::{DeltaOptions, Lzma2Options};
        use crate::{ArchiveEntry, ArchiveWriter, EncoderConfiguration, SourceReader};
        use std::sync::Mutex;
        let data = [
            Vec::new(),
            (0..17003)
                .map(|i| ((i / 97 + i % 19) % 251) as u8)
                .collect(),
            vec![91; 70003],
        ];
        let mut options = Lzma2Options::from_level(1);
        options.set_dictionary_size(1 << 16);
        let plain: Vec<EncoderConfiguration> = vec![options.into()];
        let mut filtered = plain.clone();
        filtered.push(DeltaOptions::from_distance(1).into());
        let modes = vec![
            (plain.clone(), Password::empty()),
            (filtered, Password::empty()),
        ];
        #[cfg(feature = "aes256")]
        let modes = {
            let mut modes = modes;
            let password = Password::new("fixture-key");
            let mut encrypted = plain;
            encrypted.insert(
                0,
                crate::encoder_options::AesEncoderOptions::new(password.clone()).into(),
            );
            modes.push((encrypted, password));
            modes
        };
        for (methods, password) in modes {
            for solid in [false, true] {
                let mut writer = ArchiveWriter::new(io::Cursor::new(Vec::new())).unwrap();
                writer.set_content_methods(methods.clone());
                writer.set_encrypt_header(false);
                let entries: Vec<_> = (0..data.len())
                    .map(|i| ArchiveEntry::new_file(&format!("member-{i}")))
                    .collect();
                if solid {
                    writer
                        .push_archive_entries(
                            entries,
                            data.iter()
                                .map(|bytes| SourceReader::new(bytes.as_slice()))
                                .collect(),
                        )
                        .unwrap();
                } else {
                    for (entry, bytes) in entries.into_iter().zip(&data) {
                        writer
                            .push_archive_entry(entry, Some(bytes.as_slice()))
                            .unwrap();
                    }
                }
                let bytes = writer.finish().unwrap().into_inner();
                for bad_crc in [false, true] {
                    for stop_early in [false, true] {
                        let mut outcomes = Vec::new();
                        for direct in [false, true] {
                            let mut source = io::Cursor::new(bytes.clone());
                            let mut archive = Archive::read(&mut source, &password).unwrap();
                            if bad_crc {
                                archive
                                    .files
                                    .iter_mut()
                                    .find(|file| file.size > 0)
                                    .unwrap()
                                    .crc ^= 1;
                            }
                            let mut reader =
                                ArchiveReader::from_archive(archive, source, password.clone());
                            reader.set_threads(1);
                            let reports = Arc::new(Mutex::new(Vec::new()));
                            let hook = Arc::clone(&reports);
                            reader.set_sub_stream_complete_hook(move |event| {
                                hook.lock().unwrap().push((event.file_index, event.crc32));
                            });
                            let mut output = Vec::new();
                            let result = reader.for_each_entries_direct(|file, input| {
                                if file.size == 0 {
                                    return Ok(true);
                                }
                                let mut prefix = [0; 7];
                                input.read_exact(&mut prefix)?;
                                output.extend_from_slice(&prefix);
                                if stop_early {
                                    return Ok(false);
                                }
                                if direct {
                                    input.write_rest(&mut output)?;
                                } else {
                                    copy_rest(input, &mut output)?;
                                }
                                Ok(true)
                            });
                            let checks = reports.lock().unwrap().clone();
                            outcomes.push((
                                output,
                                result.map_err(|error| error.to_string()),
                                checks,
                            ));
                        }
                        assert_eq!(outcomes[0], outcomes[1]);
                        if !bad_crc && !stop_early {
                            assert_eq!(outcomes[1].0, data.concat());
                        }
                    }
                }
            }
        }
    }
}

impl Archive {
    /// Open 7z file under specified `path`.
    #[inline]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Archive, Error> {
        Self::open_with_password(path, &Password::empty())
    }

    /// Open an encrypted 7z file under specified `path` with `password`.
    ///
    /// # Parameters
    /// - `reader`   - the path to the 7z file
    /// - `password` - archive password encoded in utf16 little endian
    #[inline]
    pub fn open_with_password(
        path: impl AsRef<std::path::Path>,
        password: &Password,
    ) -> Result<Archive, Error> {
        let mut file = File::open(path)?;
        Self::read(&mut file, password)
    }

    /// Read 7z file archive info use the specified `reader`.
    ///
    /// # Parameters
    /// - `reader`   - the reader of the 7z filr archive
    /// - `password` - archive password encoded in utf16 little endian
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::{
    ///     fs::File,
    ///     io::{Read, Seek},
    /// };
    ///
    /// use sevenz_turbo::*;
    ///
    /// let mut reader = File::open("example.7z").unwrap();
    ///
    /// let password = Password::from("the password");
    /// let archive = Archive::read(&mut reader, &password).unwrap();
    ///
    /// for entry in &archive.files {
    ///     println!("{}", entry.name());
    /// }
    /// ```
    pub fn read<R: Read + Seek>(reader: &mut R, password: &Password) -> Result<Archive, Error> {
        Self::read_with_limits(reader, password, &ArchiveLimits::default())
    }

    /// Reads the archive's header under `limits`.
    ///
    /// The one limit that has to be applied here rather than by the caller is
    /// [`ArchiveLimits::max_end_header_bytes`]: the declared end-header size is
    /// read out of the archive's first 32 bytes and the header is buffered in
    /// full in order to parse it, so checking the size afterwards is checking
    /// nothing. An archive that declares more is refused with
    /// [`Error::EndHeaderTooLarge`] before the allocation.
    ///
    /// A compressed header is decoded under the rest of `limits`: its declared
    /// size against [`ArchiveLimits::max_header_unpacked_bytes`], and its coder
    /// chain, summed, against [`ArchiveLimits::memory_limit_bytes`], which is
    /// refused with [`Error::MaxMemLimited`] before any coder is built.
    ///
    /// # Errors
    ///
    /// Anything [`Archive::read`] raises, plus [`Error::EndHeaderTooLarge`].
    pub fn read_with_limits<R: Read + Seek>(
        reader: &mut R,
        password: &Password,
        limits: &ArchiveLimits,
    ) -> Result<Archive, Error> {
        let reader_len = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;

        let mut signature = [0; 6];
        reader.read_exact(&mut signature)?;
        if signature != SEVEN_Z_SIGNATURE {
            return Err(Error::BadSignature(signature));
        }
        let mut versions = [0; 2];
        reader.read_exact(&mut versions)?;
        let version_major = versions[0];
        let version_minor = versions[1];
        if version_major != 0 {
            return Err(Error::UnsupportedVersion {
                major: version_major,
                minor: version_minor,
            });
        }

        let start_header_crc = reader.read_u32()?;

        let header_valid = if start_header_crc == 0 {
            let current_position = reader.stream_position()?;
            let mut buf = [0; 20];
            reader.read_exact(&mut buf)?;
            reader.seek(SeekFrom::Start(current_position))?;
            buf.iter().any(|a| *a != 0)
        } else {
            true
        };
        if header_valid {
            let start_header = Self::read_start_header(reader, start_header_crc)?;
            Self::init_archive(
                reader,
                start_header,
                password,
                true,
                &DecodeOptions::header(limits),
            )
        } else {
            Self::try_to_locale_end_header(
                reader,
                reader_len,
                password,
                &DecodeOptions::header(limits),
            )
        }
    }

    fn read_start_header<R: Read>(
        reader: &mut R,
        start_header_crc: u32,
    ) -> Result<StartHeader, Error> {
        let mut buf = [0; 20];
        reader.read_exact(&mut buf)?;
        let crc32 = crc32(&buf);
        if crc32 != start_header_crc {
            return Err(Error::ChecksumVerificationFailed);
        }
        let mut buf_read = buf.as_slice();
        let offset = buf_read.read_u64()?;

        let size = buf_read.read_u64()?;
        let crc = buf_read.read_u32()?;
        Ok(StartHeader {
            next_header_offset: offset,
            next_header_size: size,
            next_header_crc: crc as u64,
        })
    }

    fn read_header<R: Read + Seek>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let mut nid = header.read_u8()?;
        if nid == K_ARCHIVE_PROPERTIES {
            Self::read_archive_properties(header, bounds)?;
            nid = header.read_u8()?;
        }

        if nid == K_ADDITIONAL_STREAMS_INFO {
            return Err(Error::unsupported("Additional streams unsupported"));
        }
        if nid == K_MAIN_STREAMS_INFO {
            Self::read_streams_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        }
        if nid == K_FILES_INFO {
            Self::read_files_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        }
        if nid != K_END {
            return Err(Error::BadTerminatedHeader(nid));
        }

        Ok(())
    }

    fn read_archive_properties<R: Read + Seek>(
        header: &mut R,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let mut nid = header.read_u8()?;
        while nid != K_END {
            // Bound the skip length against the buffer: an unbounded value cast to `i64`
            // could go negative and seek backwards, re-reading the same bytes forever.
            let property_size = bounds.size(read_variable_u64(header)?)?;
            header.seek(SeekFrom::Current(property_size as i64))?;
            nid = header.read_u8()?;
        }
        Ok(())
    }

    fn try_to_locale_end_header<R: Read + Seek>(
        reader: &mut R,
        reader_len: u64,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<Self, Error> {
        let search_limit = 1024 * 1024;
        let prev_data_size = reader.stream_position()? + 20;
        let size = reader_len;
        let min_pos = if reader.stream_position()? + search_limit > size {
            reader.stream_position()?
        } else {
            size - search_limit
        };
        let mut pos = reader_len - 1;
        while pos > min_pos {
            pos -= 1;

            reader.seek(SeekFrom::Start(pos))?;
            let nid = reader.read_u8()?;
            if nid == K_ENCODED_HEADER || nid == K_HEADER {
                // `pos` scans down and can fall below `prev_data_size`; skip such candidates
                // instead of underflowing the subtraction.
                let Some(next_header_offset) = pos.checked_sub(prev_data_size) else {
                    continue;
                };
                let start_header = StartHeader {
                    next_header_offset,
                    next_header_size: reader_len - pos,
                    next_header_crc: 0,
                };
                let result = Self::init_archive(reader, start_header, password, false, opts)?;

                if !result.files.is_empty() {
                    return Ok(result);
                }
            }
        }
        Err(Error::corrupted(
            "Start header corrupt and unable to guess end header",
        ))
    }

    fn init_archive<R: Read + Seek>(
        reader: &mut R,
        start_header: StartHeader,
        password: &Password,
        verify_crc: bool,
        opts: &DecodeOptions<'_>,
    ) -> Result<Self, Error> {
        // The caller's own bound comes first: this number is read out of the
        // archive's first 32 bytes and the header below is buffered in full.
        if start_header.next_header_size > opts.limits.max_end_header_bytes {
            return Err(Error::EndHeaderTooLarge {
                limit_bytes: opts.limits.max_end_header_bytes,
                declared_bytes: start_header.next_header_size,
            });
        }

        // Bound the declared next-header size against the actual file length before allocating.
        let reader_len = reader.seek(SeekFrom::End(0))?;
        if start_header.next_header_size > usize::MAX as u64
            || start_header.next_header_size > reader_len
        {
            return Err(Error::corrupted(format!(
                "Cannot handle next_header_size {}",
                start_header.next_header_size
            )));
        }

        let next_header_size_int = start_header.next_header_size as usize;

        // Bound the header position too: `next_header_offset` is an unbounded `u64`, so the
        // addition can overflow (a panic under overflow checks) and any value past the file
        // end is invalid anyway.
        let header_pos = SIGNATURE_HEADER_SIZE
            .checked_add(start_header.next_header_offset)
            .filter(|pos| *pos <= reader_len)
            .ok_or_else(|| Error::corrupted("next header offset out of range"))?;
        reader.seek(SeekFrom::Start(header_pos))?;

        let mut buf = vec![0; next_header_size_int];
        reader.read_exact(&mut buf)?;
        if verify_crc && u64::from(crc32(&buf)) != start_header.next_header_crc {
            return Err(Error::NextHeaderCrcMismatch);
        }

        // C/7zArcIn.c: SzArEx_Open2 accepts an empty archive without a next
        // header only when the header is where an empty archive puts it:
        // `nextHeaderOffset == 0 && nextHeaderCRC == 0`. The CRC half is the
        // check above, since crc32("") is zero; an empty header anywhere else
        // says there are packed bytes nothing describes.
        if buf.is_empty() {
            if start_header.next_header_offset != 0 {
                return Err(Error::corrupted(
                    "empty next header at a non-zero offset: the archive is corrupt",
                ));
            }
            return Ok(Archive::default());
        }

        let mut archive = Archive::default();
        let mut buf_reader = buf.as_slice();
        let mut nid = buf_reader.read_u8()?;
        let mut header = if nid == K_ENCODED_HEADER {
            // A compressed header is one level of nesting: this header, plus the
            // one it decodes to. A caller that allows fewer refuses it outright.
            if opts.limits.max_header_depth < 2 {
                return Err(Error::limit(
                    Limit::HeaderDepth,
                    u64::from(opts.limits.max_header_depth),
                    2,
                ));
            }
            let (mut out_reader, buf_size) = Self::read_encoded_header(
                &mut buf_reader,
                reader,
                &mut archive,
                password,
                HeaderBounds::new(next_header_size_int, opts.limits),
                opts,
            )?;
            // The header's own chain says whether it decrypts, as a block's
            // does: a damaged plain header is damage under any password.
            let encrypted = archive.blocks.first().is_some_and(Block::is_encrypted);
            buf = read_decoded_header(&mut out_reader, buf_size, encrypted)?;
            archive = Archive::default();
            buf_reader = buf.as_slice();
            nid = buf_reader.read_u8()?;
            buf_reader
        } else {
            buf_reader
        };
        // Upper bound for any header-declared count/size: it can never exceed the number
        // of bytes in the header buffer, since every counted element consumes at least
        // one header byte. This kills the "tiny file declares a huge count" OOM vector
        // without rejecting any legitimate archive.
        let header_len_bound = header.len();
        let mut header = std::io::Cursor::new(&mut header);
        if nid == K_HEADER {
            Self::read_header(
                &mut header,
                &mut archive,
                HeaderBounds::new(header_len_bound, opts.limits),
            )?;
        } else if nid == K_ENCODED_HEADER {
            // A compressed header that decodes to another one. Nothing writes
            // this, and following it would be a recursion driven by a few bytes
            // of input, so it is refused as the depth limit it is.
            return Err(Error::limit(
                Limit::HeaderDepth,
                u64::from(opts.limits.max_header_depth),
                3,
            ));
        } else {
            return Err(Error::corrupted("Broken or unsupported archive: no Header"));
        }

        // Like CDbEx::FillLinks, stream maps depend only on parsed metadata.
        // Release the decoded or plain header before allocating those final
        // maps, rather than keeping both complete representations live.
        // This also validates blocks in headers without a FilesInfo section.
        drop(buf);
        Self::calculate_stream_map(&mut archive)?;

        // Every packed byte the header describes has to be inside the file. The
        // pack streams are laid out end to end from `pack_pos`, so they cannot
        // overlap or run backwards by construction; what is not implied is that
        // the last one ends inside the archive. Without this, a header can send a
        // decode reading at an arbitrary offset, and a coder can be handed a
        // length that is only ever going to end in a short read part-way through.
        let mut packed_total: u64 = 0;
        for size in &archive.pack_sizes {
            packed_total = packed_total
                .checked_add(*size)
                .ok_or_else(|| Error::corrupted("pack sizes overflow"))?;
        }
        let packed_end = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .and_then(|start| start.checked_add(packed_total))
            .ok_or_else(|| Error::corrupted("pack stream range out of range"))?;
        if packed_end > reader_len {
            return Err(Error::corrupted(format!(
                "pack streams end at {packed_end}, past the end of a {reader_len}-byte archive"
            )));
        }

        // The decompression-bomb bounds. Both are about what the archive says it
        // will produce, which is the only thing that can be known before any of
        // it is produced; the decode itself never writes more than these sizes,
        // because every read is driven by them.
        if opts.limits.max_unpack_bytes < u64::MAX || opts.limits.max_unpack_ratio < u64::MAX {
            let mut unpacked_total: u64 = 0;
            for block in &archive.blocks {
                unpacked_total = unpacked_total
                    .checked_add(block.get_unpack_size())
                    .ok_or_else(|| Error::corrupted("unpacked sizes overflow"))?;
            }
            if unpacked_total > opts.limits.max_unpack_bytes {
                return Err(Error::limit(
                    Limit::UnpackBytes,
                    opts.limits.max_unpack_bytes,
                    unpacked_total,
                ));
            }
            // A ratio needs something to divide by: an archive of nothing but
            // empty files packs to nothing and is not a bomb.
            if opts.limits.max_unpack_ratio < u64::MAX && packed_total > 0 {
                let ratio = unpacked_total / packed_total;
                if ratio > opts.limits.max_unpack_ratio {
                    return Err(Error::limit(
                        Limit::UnpackRatio,
                        opts.limits.max_unpack_ratio,
                        ratio,
                    ));
                }
            }
        }

        // The caller extracts to a directory, so a name that would not stay
        // inside one makes the archive unreadable rather than being reported
        // per entry.
        if opts.limits.reject_unsafe_paths {
            for file in &archive.files {
                if let Some(reason) = crate::archive::unsafe_path_reason(&file.name) {
                    return Err(Error::UnsafeEntryName {
                        name: file.name.clone(),
                        reason,
                    });
                }
            }
        }

        archive.check_aes_work(opts.limits)?;
        archive.is_solid = archive
            .blocks
            .iter()
            .any(|block| block.num_unpack_sub_streams > 1);

        Ok(archive)
    }

    fn read_encoded_header<'r, R: Read, RI: 'r + Read + Seek>(
        header: &mut R,
        reader: &'r mut RI,
        archive: &mut Archive,
        password: &Password,
        bounds: HeaderBounds<'_>,
        opts: &DecodeOptions<'_>,
    ) -> Result<(Box<dyn Read + 'r>, usize), Error> {
        Self::read_streams_info(header, archive, bounds)?;
        archive.check_aes_work(opts.limits)?;
        let block = archive
            .blocks
            .first()
            .ok_or(Error::corrupted("no blocks, can't read encoded header"))?;
        let first_pack_stream_index = 0;
        let block_offset = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .ok_or_else(|| Error::corrupted("pack position out of range"))?;
        if archive.pack_sizes.is_empty() {
            return Err(Error::corrupted(
                "no packed streams, can't read encoded header",
            ));
        }

        reader.seek(SeekFrom::Start(block_offset))?;
        let coder_len = block.coders.len();
        // The decoded header is buffered whole before it can be parsed, and this
        // size is a header-declared number multiplied by whatever the coder can
        // amplify: a kilobyte of packed input may claim to unpack to a terabyte.
        let declared_unpack = block.get_unpack_size();
        if declared_unpack > bounds.limits.max_header_unpacked_bytes {
            return Err(Error::limit(
                Limit::HeaderUnpackedBytes,
                bounds.limits.max_header_unpacked_bytes,
                declared_unpack,
            ));
        }
        let unpack_size = usize::try_from(declared_unpack)
            .map_err(|_| Error::corrupted("encoded header unpack size out of range"))?;
        // Each coder is checked against the memory limit as it is built, which
        // alone lets a chain of them hold the limit several times over; the
        // chain is refused here, as a whole, before any of them allocates.
        let coder_sizes = || {
            block
                .ordered_coder_iter()
                .map(|(index, coder)| (coder, block.get_unpack_size_at_index(index)))
        };
        let reserved_kb = check_chain_memory(coder_sizes(), opts.limits, 0)?;
        let unsized_kb = unsized_coders_memory_kb(coder_sizes());
        let opts = &opts
            .reserving(reserved_kb, unsized_kb)
            .decrypting(block.is_encrypted());
        let pack_size = archive.pack_sizes[first_pack_stream_index] as usize;
        let input_reader = BoundedReader::new(reader, pack_size);
        let mut decoder: Box<dyn Read> = Box::new(input_reader);
        let mut decoder = if coder_len > 0 {
            for (index, coder) in block.ordered_coder_iter() {
                if coder.num_in_streams() != 1 || coder.num_out_streams() != 1 {
                    return Err(Error::unsupported(
                        "Multi input/output stream coders are not yet supported",
                    ));
                }
                let next = add_decoder(
                    decoder,
                    block.get_unpack_size_at_index(index) as usize,
                    coder,
                    password,
                    opts,
                )?;
                decoder = Box::new(next);
            }
            decoder
        } else {
            decoder
        };
        // The block checksum is verified by folding the workers' own segments
        // when the parallel coder computed them; wrapping the stream here as
        // well would put a CRC-32 back on the consuming thread, which is the
        // one place the multi-threaded path must not spend time. See
        // `BlockDecoder::for_each_entries`.
        if block.has_crc && opts.verify_checksums && !opts.folding_checksums() {
            decoder = Box::new(Crc32VerifyingReader::new(decoder, unpack_size, block.crc));
        }

        Ok((decoder, unpack_size))
    }

    fn read_streams_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let mut nid = header.read_u8()?;
        if nid == K_PACK_INFO {
            Self::read_pack_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        }

        if nid == K_UNPACK_INFO {
            Self::read_unpack_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        } else {
            archive.blocks.clear();
        }
        let explicit_sub_streams = nid == K_SUB_STREAMS_INFO;
        if explicit_sub_streams {
            Self::read_sub_streams_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        }
        if nid != K_END {
            return Err(Error::BadTerminatedStreamsInfo(nid));
        }
        if !explicit_sub_streams {
            // C: SzReadStreamsInfo defaults to one sub-stream per folder;
            // SzReadHeader2 derives its size and inherits its folder CRC.
            let count = bounds.count(archive.blocks.len() as u64, Limit::Entries)?;
            let mut info = SubStreamsInfo {
                unpack_sizes: Vec::with_capacity(count),
                has_crc: BitSet::with_capacity(count),
                crcs: Vec::with_capacity(count),
            };
            for (index, block) in archive.blocks.iter_mut().enumerate() {
                block.num_unpack_sub_streams = 1;
                info.unpack_sizes.push(block.get_unpack_size());
                if block.has_crc {
                    info.has_crc.insert(index);
                }
                // The parser read the folder CRC from a 32-bit wire field.
                info.crcs
                    .push(if block.has_crc { block.crc as u32 } else { 0 });
            }
            archive.sub_streams_info = Some(info);
        }

        Ok(())
    }

    fn read_files_info<R: Read + Seek>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let num_files = bounds.count(read_variable_u64(header)?, Limit::Entries)?;
        let mut files: Vec<ArchiveEntry> = vec![Default::default(); num_files];

        let mut is_empty_stream: Option<BitSet> = None;
        let mut is_empty_file: Option<BitSet> = None;
        let mut is_anti: Option<BitSet> = None;
        loop {
            let prop_type = header.read_u8()?;
            if prop_type == 0 {
                break;
            }
            let size = read_variable_u64(header)?;
            match prop_type {
                K_EMPTY_STREAM => {
                    is_empty_stream = Some(read_bits(header, num_files)?);
                }
                K_EMPTY_FILE => {
                    let n = if let Some(s) = &is_empty_stream {
                        s.len()
                    } else {
                        return Err(Error::corrupted(
                            "Header format error: kEmptyStream must appear before kEmptyFile",
                        ));
                    };
                    is_empty_file = Some(read_bits(header, n)?);
                }
                K_ANTI => {
                    let n = if let Some(s) = is_empty_stream.as_ref() {
                        s.len()
                    } else {
                        return Err(Error::corrupted(
                            "Header format error: kEmptyStream must appear before kEmptyFile",
                        ));
                    };
                    is_anti = Some(read_bits(header, n)?);
                }
                K_NAME => {
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::unsupported("Not implemented:external != 0"));
                    }
                    // A zero `size` would underflow `size - 1`; reject it explicitly.
                    if size == 0 || (size - 1) & 1 != 0 {
                        return Err(Error::corrupted("file names length invalid"));
                    }

                    let size = bounds.count(size, Limit::TotalNameBytes)?;
                    // let mut names = vec![0u8; size - 1];
                    // header.read_exact(&mut names)?;
                    let names_reader = NamesReader::new(
                        header,
                        size - 1,
                        usize::try_from(bounds.limits.max_name_bytes).unwrap_or(usize::MAX),
                    );

                    let mut next_file = 0;
                    for s in names_reader {
                        // The names blob is an independent length, so it can yield more
                        // names than `num_files`. Bail with an error instead of letting
                        // `files[next_file]` panic with an out-of-bounds index.
                        if next_file >= files.len() {
                            return Err(Error::corrupted("Error parsing file names"));
                        }
                        files[next_file].name = s?;
                        next_file += 1;
                    }

                    if next_file != files.len() {
                        return Err(Error::corrupted("Error parsing file names"));
                    }
                }
                K_C_TIME => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::unsupported(format!(
                            "kCTime Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_creation_date = times_defined.contains(i);
                        if file.has_creation_date {
                            file.creation_date = header.read_u64()?.into();
                        }
                    }
                }
                K_A_TIME => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::unsupported(format!(
                            "kATime Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_access_date = times_defined.contains(i);
                        if file.has_access_date {
                            file.access_date = header.read_u64()?.into();
                        }
                    }
                }
                K_M_TIME => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::unsupported(format!(
                            "kMTime Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_last_modified_date = times_defined.contains(i);
                        if file.has_last_modified_date {
                            file.last_modified_date = header.read_u64()?.into();
                        }
                    }
                }
                K_WIN_ATTRIBUTES => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::unsupported(format!(
                            "kWinAttributes Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_windows_attributes = times_defined.contains(i);
                        if file.has_windows_attributes {
                            file.windows_attributes = header.read_u32()?;
                        }
                    }
                }
                K_START_POS => {
                    return Err(Error::unsupported(
                        "kStartPos is unsupported, please report",
                    ));
                }
                K_DUMMY => {
                    // Bound the skip against the buffer: an unbounded value cast to `i64`
                    // could go negative and seek backwards, re-reading the same bytes forever.
                    let skip = bounds.size(size)?;
                    header.seek(SeekFrom::Current(skip as i64))?;
                }
                _ => {
                    let skip = bounds.size(size)?;
                    header.seek(SeekFrom::Current(skip as i64))?;
                }
            };
        }

        let mut non_empty_file_counter = 0;
        let mut empty_file_counter = 0;
        for (i, file) in files.iter_mut().enumerate() {
            file.has_stream = is_empty_stream
                .as_ref()
                .map(|s| !s.contains(i))
                .unwrap_or(true);
            if file.has_stream {
                let sub_stream_info = if let Some(s) = archive.sub_streams_info.as_ref() {
                    s
                } else {
                    return Err(Error::corrupted(
                        "Archive contains file with streams but no subStreamsInfo",
                    ));
                };
                file.is_directory = false;
                file.is_anti_item = false;
                // The count of streamed files and `total_unpack_streams` are independent
                // header quantities. Reject a mismatch instead of indexing out of bounds.
                let (Some(&crc), Some(&size)) = (
                    sub_stream_info.crcs.get(non_empty_file_counter),
                    sub_stream_info.unpack_sizes.get(non_empty_file_counter),
                ) else {
                    return Err(Error::corrupted(
                        "Archive declares more streamed files than sub-streams",
                    ));
                };
                file.has_crc = sub_stream_info.has_crc.contains(non_empty_file_counter);
                file.crc = u64::from(crc);
                file.size = size;
                non_empty_file_counter += 1;
            } else {
                file.is_directory = if let Some(s) = &is_empty_file {
                    !s.contains(empty_file_counter)
                } else {
                    true
                };
                file.is_anti_item = is_anti
                    .as_ref()
                    .map(|s| s.contains(empty_file_counter))
                    .unwrap_or(false);
                file.has_crc = false;
                file.size = 0;
                empty_file_counter += 1;
            }
        }
        // C/7zArcIn.c: `numFiles - numEmptyStreams != ssi.NumTotalSubStreams`
        // is SZ_ERROR_ARCHIVE. A sub-stream no file claims would leave its
        // block mapped to files that are not its own.
        let sub_streams = archive
            .sub_streams_info
            .as_ref()
            .map_or(0, |s| s.unpack_sizes.len());
        if non_empty_file_counter != sub_streams {
            return Err(Error::corrupted(
                "Archive declares fewer streamed files than sub-streams",
            ));
        }
        archive.files = files;

        Ok(())
    }

    fn calculate_stream_map(archive: &mut Archive) -> Result<(), Error> {
        let mut stream_map = StreamMap::default();

        let mut next_block_pack_stream_index = 0;
        let num_blocks = archive.blocks.len();
        stream_map.block_first_pack_stream_index = vec![0; num_blocks];
        for i in 0..num_blocks {
            stream_map.block_first_pack_stream_index[i] = next_block_pack_stream_index;
            // A block's pack-stream span `[first .. first + packed_streams.len())` is later
            // used to slice `pack_stream_offsets`/`pack_sizes` in `build_decode_stack{,2}`.
            // Reject a block whose span runs past the available pack streams instead of
            // panicking there.
            next_block_pack_stream_index = next_block_pack_stream_index
                .checked_add(archive.blocks[i].packed_streams().len())
                .ok_or_else(|| Error::corrupted("pack stream index overflow"))?;
            if next_block_pack_stream_index > archive.pack_sizes.len() {
                return Err(Error::corrupted(
                    "block references pack streams beyond the available pack sizes",
                ));
            }
        }

        // `SubStreamsInfo` is indexed by the running sub-stream count over all
        // blocks. Record each block's first index here so `build_decode_stack`
        // does not re-sum the preceding blocks every time it opens one, which
        // made extracting an archive of many non-solid blocks quadratic.
        stream_map.block_first_sub_stream_index = SubStreamOffsets::from_counts(
            archive
                .blocks
                .iter()
                .map(|block| block.num_unpack_sub_streams),
        )?;

        let mut next_pack_stream_offset: u64 = 0;
        let num_pack_sizes = archive.pack_sizes.len();
        stream_map.pack_stream_offsets = vec![0; num_pack_sizes];
        for i in 0..num_pack_sizes {
            stream_map.pack_stream_offsets[i] = next_pack_stream_offset;
            next_pack_stream_offset = next_pack_stream_offset
                .checked_add(archive.pack_sizes[i])
                .ok_or_else(|| Error::corrupted("pack stream offset overflow"))?;
        }

        stream_map.block_first_file_index = vec![0; num_blocks];
        stream_map.file_block_index = vec![None; archive.files.len()];
        let mut next_block_index = 0;
        let mut next_block_unpack_stream_index = 0;
        for i in 0..archive.files.len() {
            if !archive.files[i].has_stream && next_block_unpack_stream_index == 0 {
                stream_map.file_block_index[i] = None;
                continue;
            }
            if next_block_unpack_stream_index == 0 {
                while next_block_index < archive.blocks.len() {
                    stream_map.block_first_file_index[next_block_index] = i;
                    if archive.blocks[next_block_index].num_unpack_sub_streams > 0 {
                        break;
                    }
                    next_block_index += 1;
                }
                if next_block_index >= archive.blocks.len() {
                    return Err(Error::corrupted("Too few blocks in archive"));
                }
            }
            stream_map.file_block_index[i] = Some(next_block_index);
            if !archive.files[i].has_stream {
                continue;
            }

            // The first file in a block carries the block's compressed size:
            // every pack stream it reads, which for BCJ2 is all four, as the
            // writer reports it when the entry is pushed.
            if stream_map.block_first_file_index[next_block_index] == i {
                let first_pack_stream_index =
                    stream_map.block_first_pack_stream_index[next_block_index];
                let pack_sizes = first_pack_stream_index
                    .checked_add(archive.blocks[next_block_index].packed_streams().len())
                    .and_then(|end| archive.pack_sizes.get(first_pack_stream_index..end))
                    .ok_or_else(|| {
                        Error::corrupted("block references a pack stream index beyond pack_sizes")
                    })?;
                let pack_size = pack_sizes
                    .iter()
                    .try_fold(0u64, |total, &size| total.checked_add(size))
                    .ok_or_else(|| Error::corrupted("block pack size overflow"))?;

                archive.files[i].compressed_size = pack_size;
            }

            next_block_unpack_stream_index += 1;
            if next_block_unpack_stream_index
                >= archive.blocks[next_block_index].num_unpack_sub_streams
            {
                next_block_index += 1;
                next_block_unpack_stream_index = 0;
            }
        }
        // CPP/7zip/Archive/7z/7zIn.cpp: a folder no file reached starts after
        // the last file, not at the first. A block the loop stopped inside of
        // keeps the start it was given.
        let unreached = next_block_index + usize::from(next_block_unpack_stream_index != 0);
        for first in stream_map.block_first_file_index.iter_mut().skip(unreached) {
            *first = archive.files.len();
        }

        // Each block's file span `[first_file .. first_file + num_unpack_sub_streams)` is
        // later iterated directly against `archive.files` (in `BlockDecoder`). The
        // sub-stream count is an independent header quantity, so reject a block that
        // declares more sub-streams than there are files instead of indexing out of bounds.
        for (b, block) in archive.blocks.iter().enumerate() {
            let start = stream_map.block_first_file_index[b];
            let end = start
                .checked_add(block.num_unpack_sub_streams)
                .ok_or_else(|| Error::corrupted("block file span overflow"))?;
            if end > archive.files.len() {
                return Err(Error::corrupted(
                    "block declares more sub-streams than the archive has files",
                ));
            }
        }

        archive.stream_map = stream_map;
        Ok(())
    }

    fn read_pack_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        archive.pack_pos = read_variable_u64(header)?;
        let num_pack_streams = bounds.count(read_variable_u64(header)?, Limit::Entries)?;
        let mut nid = header.read_u8()?;
        if nid == K_SIZE {
            archive.pack_sizes = vec![0u64; num_pack_streams];
            for i in 0..archive.pack_sizes.len() {
                archive.pack_sizes[i] = read_variable_u64(header)?;
            }
            nid = header.read_u8()?;
        }

        if nid == K_CRC {
            archive.pack_crcs_defined = read_all_or_bits(header, num_pack_streams)?;
            archive.pack_crcs = vec![0; num_pack_streams];
            for i in 0..num_pack_streams {
                if archive.pack_crcs_defined.contains(i) {
                    archive.pack_crcs[i] = header.read_u32()? as u64;
                }
            }
            nid = header.read_u8()?;
        }

        if nid != K_END {
            return Err(Error::BadTerminatedPackInfo(nid));
        }

        Ok(())
    }
    fn read_unpack_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let nid = header.read_u8()?;
        if nid != K_FOLDER {
            return Err(Error::corrupted(format!("Expected kFolder, got {nid}")));
        }
        let num_blocks = bounds.count(read_variable_u64(header)?, Limit::Entries)?;

        // Grow into the count rather than reserving it: a `Block` is a hundred-odd
        // bytes and the count is one, so the reservation is the amplification the
        // byte bound alone does not cover.
        archive.blocks.reserve(num_blocks.min(1024));
        let external = header.read_u8()?;
        if external != 0 {
            return Err(Error::ExternalUnsupported);
        }

        // A block is cheap to declare and a coder is not, so the coders are counted
        // across the whole archive as well as per block.
        let mut total_coders: u64 = 0;
        for _ in 0..num_blocks {
            let block = Self::read_block(header, bounds)?;
            total_coders = total_coders.saturating_add(block.coders.len() as u64);
            if total_coders > bounds.limits.max_total_coders {
                return Err(Error::limit(
                    Limit::TotalCoders,
                    bounds.limits.max_total_coders,
                    total_coders,
                ));
            }
            archive.blocks.push(block);
        }

        let nid = header.read_u8()?;
        if nid != K_CODERS_UNPACK_SIZE {
            return Err(Error::corrupted(format!(
                "Expected kCodersUnpackSize, got {nid}"
            )));
        }

        for block in archive.blocks.iter_mut() {
            // The stream count was checked against header and policy bounds
            // in read_block before sizing this table.
            block.prepare_unpack_sizes();
            for size in block.unpack_sizes_mut() {
                *size = read_variable_u64(header)?;
            }
        }

        let mut nid = header.read_u8()?;
        if nid == K_CRC {
            let crcs_defined = read_all_or_bits(header, num_blocks)?;
            for i in 0..num_blocks {
                if crcs_defined.contains(i) {
                    archive.blocks[i].has_crc = true;
                    archive.blocks[i].crc = header.read_u32()? as u64;
                } else {
                    archive.blocks[i].has_crc = false;
                }
            }
            nid = header.read_u8()?;
        }
        if nid != K_END {
            return Err(Error::BadTerminatedUnpackInfo);
        }

        Ok(())
    }

    fn read_sub_streams_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        for block in archive.blocks.iter_mut() {
            block.num_unpack_sub_streams = 1;
        }
        let mut total_unpack_streams = archive.blocks.len();

        let mut nid = header.read_u8()?;
        if nid == K_NUM_UNPACK_STREAM {
            total_unpack_streams = 0;
            for block in archive.blocks.iter_mut() {
                let num_streams = bounds.count(read_variable_u64(header)?, Limit::Entries)?;
                block.num_unpack_sub_streams = num_streams;
                // Each sub-stream still consumes header bytes downstream, so the running
                // total stays bounded by `limit`; reject anything larger up front.
                // C: CInArchive::ReadSubStreamsInfo checks the running sum.
                total_unpack_streams = bounds.add_count(total_unpack_streams, num_streams)?;
            }
            nid = header.read_u8()?;
        }

        let mut sub_streams_info = SubStreamsInfo::default();
        sub_streams_info
            .unpack_sizes
            .resize(total_unpack_streams, Default::default());
        sub_streams_info
            .has_crc
            .reserve_len_exact(total_unpack_streams);
        sub_streams_info.crcs = vec![0; total_unpack_streams];

        let mut next_unpack_stream = 0;
        for block in archive.blocks.iter() {
            if block.num_unpack_sub_streams == 0 {
                continue;
            }
            let mut sum: u64 = 0;
            if nid == K_SIZE {
                for _i in 0..block.num_unpack_sub_streams - 1 {
                    let size = read_variable_u64(header)?;
                    sub_streams_info.unpack_sizes[next_unpack_stream] = size;
                    next_unpack_stream += 1;
                    sum = sum
                        .checked_add(size)
                        .ok_or_else(|| Error::corrupted("sub-stream size sum overflow"))?;
                }
            }
            if sum > block.get_unpack_size() {
                return Err(Error::corrupted(
                    "sum of unpack sizes of block exceeds total unpack size",
                ));
            }
            // Calculate the last size from the total minus the sum of N-1 sizes.
            sub_streams_info.unpack_sizes[next_unpack_stream] = block.get_unpack_size() - sum;
            next_unpack_stream += 1;
        }
        if nid == K_SIZE {
            nid = header.read_u8()?;
        }

        let mut num_digests = 0usize;
        for block in archive.blocks.iter() {
            if block.num_unpack_sub_streams != 1 || !block.has_crc {
                num_digests = num_digests
                    .checked_add(block.num_unpack_sub_streams)
                    .filter(|total| *total <= total_unpack_streams)
                    .ok_or_else(|| Error::corrupted("sub-stream digest count out of range"))?;
            }
        }

        if nid == K_CRC {
            let has_missing_crc = read_all_or_bits(header, num_digests)?;
            let mut missing_crcs = vec![0; num_digests];
            for (i, missing_crc) in missing_crcs.iter_mut().enumerate() {
                if has_missing_crc.contains(i) {
                    *missing_crc = header.read_u32()?;
                }
            }
            let mut next_crc = 0;
            let mut next_missing_crc = 0;
            for block in archive.blocks.iter() {
                if block.num_unpack_sub_streams == 1 && block.has_crc {
                    next_crc += 1;
                } else {
                    for _i in 0..block.num_unpack_sub_streams {
                        if has_missing_crc.contains(next_missing_crc) {
                            sub_streams_info.has_crc.insert(next_crc);
                        } else {
                            sub_streams_info.has_crc.remove(next_crc);
                        }
                        sub_streams_info.crcs[next_crc] = missing_crcs[next_missing_crc];
                        next_crc += 1;
                        next_missing_crc += 1;
                    }
                }
            }

            nid = header.read_u8()?;
        }

        if nid != K_END {
            return Err(Error::BadTerminatedSubStreamsInfo);
        }

        // C: SzReadHeader2 inherits the folder CRC independently of the
        // optional sub-stream CRC property, for exactly one sub-stream.
        let mut first_sub_stream = 0;
        for block in &archive.blocks {
            if block.num_unpack_sub_streams == 1 && block.has_crc {
                sub_streams_info.has_crc.insert(first_sub_stream);
                // Parsing established that this is a 32-bit wire CRC.
                sub_streams_info.crcs[first_sub_stream] = block.crc as u32;
            }
            first_sub_stream += block.num_unpack_sub_streams;
        }

        archive.sub_streams_info = Some(sub_streams_info);
        Ok(())
    }

    fn read_block<R: Read>(header: &mut R, bounds: HeaderBounds<'_>) -> Result<Block, Error> {
        let num_coders = bounds.count(read_variable_u64(header)?, Limit::CodersPerBlock)?;
        let mut coders = Vec::with_capacity(num_coders);
        let mut total_in_streams: u64 = 0;
        let mut total_out_streams: u64 = 0;
        for _i in 0..num_coders {
            let mut coder = Coder::single_stream();
            let bits = header.read_u8()?;
            let id_size = bits & 0xF;
            let is_simple = (bits & 0x10) == 0;
            let has_attributes = (bits & 0x20) != 0;
            let more_alternative_methods = (bits & 0x80) != 0;

            coder.set_id_size(id_size);

            header.read_exact(coder.decompression_method_id_mut())?;
            let (input_streams, output_streams) = if is_simple {
                (1, 1)
            } else {
                (read_variable_u64(header)?, read_variable_u64(header)?)
            };
            // Each stream is referenced by a bind-pair/packed-stream entry that consumes
            // header bytes, so the totals cannot legitimately exceed `limit`. The counts are
            // unbounded attacker-controlled varints, so the sums are checked: an overflowing
            // addition must be rejected rather than wrap past the bound below.
            total_in_streams = total_in_streams
                .checked_add(input_streams)
                .ok_or_else(|| Error::corrupted("coder stream counts exceed available input"))?;
            total_out_streams = total_out_streams
                .checked_add(output_streams)
                .ok_or_else(|| Error::corrupted("coder stream counts exceed available input"))?;
            if input_streams > bounds.limits.max_streams_per_coder
                || output_streams > bounds.limits.max_streams_per_coder
            {
                return Err(Error::limit(
                    Limit::StreamsPerCoder,
                    bounds.limits.max_streams_per_coder,
                    input_streams.max(output_streams),
                ));
            }
            bounds.count(total_in_streams, Limit::Entries)?;
            bounds.count(total_out_streams, Limit::Entries)?;
            coder.set_stream_counts(input_streams, output_streams);
            if has_attributes {
                let properties_size = bounds.size(read_variable_u64(header)?)?;
                let mut props = SmallSlice::with_len(properties_size);
                header.read_exact(&mut props)?;
                coder.set_properties(props);
            }
            coders.push(coder);
            // would need to keep looping as above:
            if more_alternative_methods {
                return Err(Error::unsupported(
                    "Alternative methods are unsupported, please report. The reference implementation doesn't support them either.",
                ));
            }
        }
        let mut block = Block::with_coders(coders);
        let total_in_streams = total_in_streams as usize;
        let total_out_streams = total_out_streams as usize;

        if total_out_streams == 0 {
            return Err(Error::corrupted("Total output streams can't be 0"));
        }
        let num_bind_pairs = total_out_streams - 1;
        let mut bind_pairs = Vec::with_capacity(num_bind_pairs);
        for _ in 0..num_bind_pairs {
            let bp = BindPair {
                in_index: read_variable_u64(header)?,
                out_index: read_variable_u64(header)?,
            };
            // Bind-pair indices are later used to index fixed-size coder arrays and the
            // coder graph. Validate them at parse time so decoding cannot panic on an
            // out-of-range index.
            if bp.in_index >= total_in_streams as u64 || bp.out_index >= total_out_streams as u64 {
                return Err(Error::corrupted(
                    "bind pair references an out-of-range stream",
                ));
            }
            bind_pairs.push(bp);
        }

        if total_in_streams < num_bind_pairs {
            return Err(Error::corrupted(
                "Total input streams can't be less than the number of bind pairs",
            ));
        }
        let num_packed_streams = total_in_streams - num_bind_pairs;
        let mut packed_streams = SmallSlice::with_len(num_packed_streams);
        if num_packed_streams == 1 {
            let mut index = u64::MAX;
            for i in 0..total_in_streams {
                if !bind_pairs.iter().any(|pair| pair.in_index == i as u64) {
                    index = i as u64;
                    break;
                }
            }
            if index == u64::MAX {
                return Err(Error::corrupted("Couldn't find stream's bind pair index"));
            }
            packed_streams[0] = index;
        } else {
            for packed_stream in packed_streams.iter_mut() {
                *packed_stream = read_variable_u64(header)?;
            }
        }
        block.set_streams(
            total_in_streams,
            total_out_streams,
            bind_pairs.into_boxed_slice(),
            packed_streams,
            SmallSlice::default(),
        );

        Self::validate_coder_graph(&block)?;

        Ok(block)
    }

    /// Checks that a block's coders form a decodable graph before anything is
    /// built out of it.
    ///
    /// The bind pairs and packed-stream indices are attacker-controlled numbers
    /// that the decode stack walks as if they were a chain. Three things make
    /// that walk safe, and none of them are implied by the counts alone:
    ///
    /// - every stream index is in range, and no stream is bound or packed twice
    ///   (a stream bound twice leaves another one unbound, which is a second
    ///   "final" output);
    /// - exactly one output stream is unbound — the block's actual output;
    /// - the graph is acyclic. A cycle makes the ordered coder walk revisit
    ///   coders forever, which is an unbounded stack of decoders built from a
    ///   handful of header bytes.
    fn validate_coder_graph(block: &Block) -> Result<(), Error> {
        let total_in = block.total_input_streams();
        let total_out = block.total_output_streams();

        // ReadUnpackInfo's simple-folder case: one packed input, one final
        // output and no edges satisfy every graph invariant directly.
        if total_in == 1
            && total_out == 1
            && block.bind_pairs().is_empty()
            && block.packed_streams()[..] == [0]
            && let [coder] = block.coders.as_slice()
            && coder.num_in_streams() == 1
            && coder.num_out_streams() == 1
        {
            return Ok(());
        }

        let mut in_bound = vec![false; total_in];
        let mut out_bound = vec![false; total_out];
        for bp in block.bind_pairs() {
            // Ranges were checked as the pairs were read; this is the duplicate check.
            let (i, o) = (bp.in_index as usize, bp.out_index as usize);
            if in_bound[i] || out_bound[o] {
                return Err(Error::corrupted("bind pairs bind a stream twice"));
            }
            in_bound[i] = true;
            out_bound[o] = true;
        }

        for &ps in block.packed_streams() {
            let i = usize::try_from(ps)
                .ok()
                .filter(|i| *i < total_in)
                .ok_or_else(|| {
                    Error::corrupted("packed stream references an out-of-range stream")
                })?;
            if in_bound[i] {
                return Err(Error::corrupted(
                    "packed stream is also bound by a bind pair",
                ));
            }
            in_bound[i] = true;
        }
        if in_bound.iter().any(|bound| !bound) {
            return Err(Error::corrupted(
                "block leaves an input stream unaccounted for",
            ));
        }

        let unbound_outputs = out_bound.iter().filter(|bound| !**bound).count();
        if unbound_outputs != 1 {
            return Err(Error::corrupted(
                "block must have exactly one unbound output stream",
            ));
        }

        // Which coder owns each stream index. Both totals are the sums that were
        // bounded as the coders were read.
        let mut coder_of_in = Vec::with_capacity(total_in);
        let mut coder_of_out = Vec::with_capacity(total_out);
        for (ci, coder) in block.coders.iter().enumerate() {
            for _ in 0..coder.num_in_streams() {
                coder_of_in.push(ci);
            }
            for _ in 0..coder.num_out_streams() {
                coder_of_out.push(ci);
            }
        }
        debug_assert_eq!(coder_of_in.len(), total_in);
        debug_assert_eq!(coder_of_out.len(), total_out);

        // Depth-first from the block's output back through the bind pairs, with
        // the coders on the current path marked: meeting one of them again is a
        // cycle.
        const UNSEEN: u8 = 0;
        const ON_PATH: u8 = 1;
        const DONE: u8 = 2;
        let mut state = vec![UNSEEN; block.coders.len()];
        let final_out = out_bound
            .iter()
            .position(|bound| !*bound)
            .expect("exactly one unbound output");
        let mut stack = vec![(coder_of_out[final_out], 0usize)];
        while let Some((coder_index, step)) = stack.pop() {
            if step == 0 {
                match state[coder_index] {
                    ON_PATH => return Err(Error::corrupted("block's coders form a cycle")),
                    DONE => continue,
                    _ => state[coder_index] = ON_PATH,
                }
            }
            // This coder's input streams, in order; `step` is how many have been walked.
            let first_in = block.coders[..coder_index]
                .iter()
                .map(|c| c.num_in_streams() as usize)
                .sum::<usize>();
            let num_in = block.coders[coder_index].num_in_streams() as usize;
            if step < num_in {
                stack.push((coder_index, step + 1));
                if let Some(bp) = block.find_bind_pair_for_in_stream((first_in + step) as u64) {
                    stack.push((coder_of_out[bp.out_index as usize], 0));
                }
            } else {
                state[coder_index] = DONE;
            }
        }

        Ok(())
    }
}

/// What the header is allowed to claim, and what it is being read out of.
///
/// Two independent bounds, and every count is checked against both.
///
/// - `bytes` is the header buffer's length. Everything a header describes — a
///   file, a coder, a pack stream, a name — costs at least one byte to
///   describe, so no legitimate count can exceed it. This bound comes from the
///   archive itself and needs no caller.
/// - [`ArchiveLimits`] is what the caller will believe. The byte bound alone
///   is not enough: a count is one byte and the entry it reserves is tens or
///   hundreds, so a 64 MiB header could still ask for gigabytes of `Vec`.
#[derive(Clone, Copy)]
struct HeaderBounds<'a> {
    bytes: usize,
    limits: &'a ArchiveLimits,
}

impl<'a> HeaderBounds<'a> {
    fn new(bytes: usize, limits: &'a ArchiveLimits) -> Self {
        Self { bytes, limits }
    }

    /// Checks a count of things the header goes on to describe.
    fn count(self, value: u64, what: Limit) -> Result<usize, Error> {
        if value > self.bytes as u64 {
            return Err(Error::limit(Limit::ArchiveBytes, self.bytes as u64, value));
        }
        let cap = match what {
            Limit::CodersPerBlock => self.limits.max_coders_per_block,
            Limit::TotalCoders => self.limits.max_total_coders,
            Limit::TotalNameBytes => self.limits.max_total_name_bytes,
            _ => self.limits.max_entries,
        };
        if value > cap {
            return Err(Error::limit(what, cap, value));
        }
        // Bounded by `self.bytes`, which is a `usize`.
        Ok(value as usize)
    }

    fn add_count(self, total: usize, additional: usize) -> Result<usize, Error> {
        let total = total
            .checked_add(additional)
            .ok_or_else(|| Error::corrupted("total unpack streams overflow"))?;
        self.count(total as u64, Limit::Entries)
    }

    /// Checks a size in bytes that the header goes on to spend on itself.
    fn size(self, value: u64) -> Result<usize, Error> {
        if value > self.bytes as u64 {
            return Err(Error::limit(Limit::ArchiveBytes, self.bytes as u64, value));
        }
        Ok(value as usize)
    }
}

fn read_variable_u64<R: Read>(reader: &mut R) -> io::Result<u64> {
    let first = reader.read_u8()? as u64;
    let mut mask = 0x80_u64;
    let mut value = 0;
    for i in 0..8 {
        if (first & mask) == 0 {
            return Ok(value | ((first & (mask - 1)) << (8 * i)));
        }
        let b = reader.read_u8()? as u64;
        value |= b << (8 * i);
        mask >>= 1;
    }
    Ok(value)
}

fn read_all_or_bits<R: Read>(header: &mut R, size: usize) -> io::Result<BitSet> {
    let all = header.read_u8()?;
    if all != 0 {
        let mut bits = BitSet::with_capacity(size);
        for i in 0..size {
            bits.insert(i);
        }
        Ok(bits)
    } else {
        read_bits(header, size)
    }
}

fn read_bits<R: Read>(header: &mut R, size: usize) -> io::Result<BitSet> {
    let mut bits = BitSet::with_capacity(size);
    let mut mask = 0u32;
    let mut cache = 0u32;
    for i in 0..size {
        if mask == 0 {
            mask = 0x80;
            cache = header.read_u8()? as u32;
        }
        if (cache & mask) != 0 {
            bits.insert(i);
        }
        mask >>= 1;
    }
    Ok(bits)
}

struct NamesReader<'a, R: Read> {
    max_bytes: usize,
    read_bytes: usize,
    /// Longest one name may be, in bytes of UTF-16. A names blob is one
    /// length for all of the names in it, so without this a single name may
    /// be the whole blob — which is not an allocation problem here, but is one
    /// for everything downstream that treats a name as a path.
    max_name_bytes: usize,
    cache: Vec<u16>,
    reader: &'a mut R,
}

impl<'a, R: Read> NamesReader<'a, R> {
    fn new(reader: &'a mut R, max_bytes: usize, max_name_bytes: usize) -> Self {
        Self {
            max_bytes,
            max_name_bytes,
            reader,
            read_bytes: 0,
            cache: Vec::with_capacity(16),
        }
    }
}

impl<R: Read> Iterator for NamesReader<'_, R> {
    type Item = Result<String, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.max_bytes <= self.read_bytes {
            return None;
        }
        self.cache.clear();
        let mut buf = [0; 2];
        while self.read_bytes < self.max_bytes {
            let r = self.reader.read_exact(&mut buf);
            self.read_bytes += 2;
            if let Err(e) = r {
                return Some(Err(e.into()));
            }
            let u = u16::from_le_bytes(buf);
            if u == 0 {
                break;
            }
            if self.cache.len() * 2 >= self.max_name_bytes {
                return Some(Err(Error::limit(
                    Limit::NameBytes,
                    self.max_name_bytes as u64,
                    self.max_name_bytes as u64 + 2,
                )));
            }
            self.cache.push(u);
        }

        Some(String::from_utf16(&self.cache).map_err(|e| Error::corrupted(e.to_string())))
    }
}

#[derive(Copy, Clone)]
struct IndexEntry {
    block_index: Option<usize>,
    file_index: usize,
}

/// Reads a 7z archive file.
pub struct ArchiveReader<R: Read + Seek> {
    source: R,
    archive: Archive,
    password: Password,
    thread_count: u32,
    adaptive_lzma2: bool,
    input_complete: bool,
    verify_checksums: bool,
    lzma2: Arc<Lzma2Control>,
    /// Where each name's entry is, built by the first call that looks a name
    /// up. A consumer that walks the entries never asks, and building it at
    /// open cost every reader a second copy of every name.
    index: OnceLock<HashMap<String, IndexEntry>>,
    limits: ArchiveLimits,
    #[allow(clippy::type_complexity)]
    on_block_complete: Option<Box<dyn FnMut(BlockCompletion) + Send>>,
    #[allow(clippy::type_complexity)]
    on_sub_stream_complete: Option<Box<dyn FnMut(SubStreamCompletion) + Send>>,
    /// The same bytes as `source`, read at an offset, for decoding several
    /// folders at once. See [`ArchiveReader::set_positional_source`].
    positional: Option<Arc<dyn ReadAt>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ArchiveReader<File> {
    /// Opens a 7z archive file at the given `path` and creates a [`ArchiveReader`] to read it.
    ///
    /// The reader keeps a second handle on the file as its positional source
    /// (see [`ArchiveReader::set_positional_source`]), so with more than one
    /// thread the folders of a non-solid archive decode in parallel. With the
    /// default of one thread that handle is never read.
    #[inline]
    pub fn open(path: impl AsRef<std::path::Path>, password: Password) -> Result<Self, Error> {
        let file = File::open(path.as_ref())
            .map_err(|e| Error::file_open(e, path.as_ref().to_string_lossy().to_string()))?;
        let positional = file.try_clone().ok();
        let mut reader = Self::new(file, password)?;
        reader.set_input_complete(true);
        #[cfg(any(unix, windows))]
        if let Some(positional) = positional {
            reader.set_positional_source(positional);
        }
        #[cfg(not(any(unix, windows)))]
        drop(positional);
        Ok(reader)
    }
}

impl<S: ReadAt + 'static> ArchiveReader<ReadAtCursor<S>> {
    /// A reader over a [`ReadAt`] source: a file, bytes in memory, or any
    /// `Read + Seek` behind [`crate::SerialReadAt`].
    ///
    /// The header is parsed through a cursor of the reader's own, and the same
    /// source is the reader's positional source, so with more than one thread
    /// ([`ArchiveReader::set_threads`]) the folders of a non-solid archive
    /// decode in parallel. See [`ArchiveReader::set_positional_source`] for
    /// what that changes and what it does not.
    ///
    /// # Errors
    ///
    /// As [`ArchiveReader::with_limits`].
    pub fn from_read_at(
        source: S,
        password: Password,
        limits: ArchiveLimits,
    ) -> Result<Self, Error> {
        let source = Arc::new(source);
        let mut reader =
            Self::with_limits(ReadAtCursor::new(Arc::clone(&source)), password, limits)?;
        reader.positional = Some(source);
        Ok(reader)
    }
}

impl<R: Read + Seek> ArchiveReader<R> {
    /// Creates a [`ArchiveReader`] to read a 7z archive file from the given `source` reader.
    #[inline]
    pub fn new(source: R, password: Password) -> Result<Self, Error> {
        Self::with_limits(source, password, ArchiveLimits::default())
    }

    /// Creates an [`ArchiveReader`] that refuses an archive exceeding `limits`
    /// *before* allocating for it.
    ///
    /// Two checks, both of which have to happen before an allocation rather
    /// than after one, because both numbers come out of the archive:
    ///
    /// - the declared end-header size, against
    ///   [`ArchiveLimits::max_end_header_bytes`], before the header is
    ///   buffered ([`Error::EndHeaderTooLarge`]);
    /// - [`Archive::decoder_memory_estimate`], against
    ///   [`ArchiveLimits::memory_limit_bytes`], before any decoder is built
    ///   ([`Error::MemoryLimited`]). The same limit then bounds each block's
    ///   coder chain, summed, before it is constructed.
    ///
    /// An archive whose coder chain has no memory model is refused too when a
    /// memory limit is set: a budget that cannot be computed has not been met.
    ///
    /// # Errors
    ///
    /// Anything [`ArchiveReader::new`] raises, plus the two above.
    pub fn with_limits(
        mut source: R,
        password: Password,
        limits: ArchiveLimits,
    ) -> Result<Self, Error> {
        let archive = Archive::read_with_limits(&mut source, &password, &limits)?;

        if limits.memory_limit_bytes < u64::MAX {
            match archive.decoder_memory_estimate() {
                Ok(required_bytes) if required_bytes > limits.memory_limit_bytes => {
                    return Err(Error::MemoryLimited {
                        limit_bytes: limits.memory_limit_bytes,
                        required_bytes,
                    });
                }
                Ok(_) => {}
                Err(unsized_coder) => {
                    return Err(Error::UnsupportedCompressionMethod(
                        unsized_coder.to_string(),
                    ));
                }
            }
        }

        Ok(Self {
            source,
            archive,
            password,
            // One thread unless the caller asks for more. A reader that is
            // handed to something with its own idea of how many threads it
            // may use — which is every consumer this fork exists for — must
            // not quietly spawn `available_parallelism()` of them, and must
            // not quietly hold the memory that decoding in parallel needs.
            thread_count: 1,
            adaptive_lzma2: false,
            input_complete: false,
            verify_checksums: true,
            lzma2: Arc::new(Lzma2Control::new(1)),
            index: OnceLock::new(),
            limits,
            on_block_complete: None,
            on_sub_stream_complete: None,
            positional: None,
        })
    }

    /// Allows bounded framing inspection anywhere in a complete input.
    ///
    /// This can reduce LZMA2 buffering, enable positional ring extraction,
    /// and let graph workers read complete positional inputs directly.
    /// It defaults to false for custom sources: a seekable source may still
    /// be downloading, and inspection could wait for bytes beyond its ready
    /// prefix. [`Self::open`] enables it for its local file. Set it only when
    /// reads throughout the archive can proceed without waiting for delivery;
    /// it does not bypass payload or checksum verification.
    pub fn set_input_complete(&mut self, complete: bool) {
        self.input_complete = complete;
    }

    /// Gives the reader a positional view of its source, so that the folders
    /// of a non-solid archive can be decoded concurrently.
    ///
    /// `source` must hold the same bytes as the reader's own source; the reader
    /// cannot check that, and decodes whatever it reads. [`ArchiveReader::open`]
    /// and [`ArchiveReader::from_read_at`] set it themselves.
    ///
    /// It changes nothing until the thread count is above one. Then
    /// [`ArchiveReader::for_each_entries`] decodes each run of consecutive
    /// small folders — none larger than 8 MiB unpacked — on worker threads,
    /// as many at once as the thread count allows, each folder with its share
    /// of the threads, while a folder too large to stage is decoded alone on
    /// the calling thread with the whole count, exactly as without a
    /// positional source. Nothing else changes:
    ///
    /// - **Order.** The callback, the sub-stream hook and the block hook are
    ///   all called on the calling thread, in archive order, as on the
    ///   sequential path; only the decoding happens elsewhere.
    /// - **Memory.** At most two staged folders per worker, each at most
    ///   8 MiB, are held between the workers and the callback. Under a memory
    ///   limit the worker count is cut until what the workers hold fits the
    ///   limit together with the 1 MiB piece the callback is reading — for
    ///   each worker its decoder and input buffer, its two stages and the
    ///   1 MiB piece it is filling — and each folder then decodes on one
    ///   thread. A run with room for one worker only is decoded on the
    ///   calling thread.
    /// - **Threads.** A run starts at the reader's thread count, as a block's
    ///   coder does, and [`Lzma2Handle::set_threads`] is followed while it
    ///   decodes: the ceiling bounds how many of the run's folders decode at
    ///   once, and the threads of each folder's own coder, from the next
    ///   folder to start. A run never has more workers than it was planned
    ///   with, so a ceiling raised past the reader's thread count widens
    ///   nothing here. [`Lzma2Handle::progress`] is `None` for as long as
    ///   the run lasts: no one coder is decoding for the reader then.
    /// - **Checks.** Every member's CRC is verified on the worker that decoded
    ///   it, and a failure is the same located [`Error::BlockDecode`] naming
    ///   the same folder. A worker reads each member in full, so a damaged
    ///   member is reported even if the callback returned without reading it.
    pub fn set_positional_source(&mut self, source: impl ReadAt + 'static) {
        self.positional = Some(Arc::new(source));
    }

    /// [`ArchiveReader::set_positional_source`], for a builder chain.
    #[must_use]
    pub fn with_positional_source(mut self, source: impl ReadAt + 'static) -> Self {
        self.set_positional_source(source);
        self
    }

    /// Drops the positional source: every folder decodes on the calling
    /// thread again, one at a time.
    pub fn clear_positional_source(&mut self) {
        self.positional = None;
    }

    /// Calls `hook` each time a block has been decoded in full and its
    /// checksum, if the archive records one, has been verified.
    ///
    /// A consumer that is streaming output somewhere else uses this to learn
    /// that a region is settled without waiting for the whole archive, and
    /// without inferring it from entry callbacks (an entry boundary is not a
    /// block boundary, and in a solid archive a block's last entry is checked
    /// only when the block's own CRC is).
    ///
    /// The hook never fires for a block that failed: a failure is an error
    /// from the call that was decoding it.
    pub fn set_block_complete_hook(&mut self, hook: impl FnMut(BlockCompletion) + Send + 'static) {
        self.on_block_complete = Some(Box::new(hook));
    }

    /// Drops any hook set by [`ArchiveReader::set_block_complete_hook`].
    pub fn clear_block_complete_hook(&mut self) {
        self.on_block_complete = None;
    }

    /// Calls `hook` as each file's CRC-32 becomes final, with the value.
    ///
    /// The reader already checksums every file it decodes, to check it against
    /// the header; this hands the number over rather than discarding it, so a
    /// consumer can report per-file integrity without reading the bytes a
    /// second time. The hook fires after the entry callback returns for that
    /// file, once per file the archive records a checksum for, in order.
    ///
    /// A failed check never reaches the hook: it is [`Error::BlockDecode`] of
    /// kind [`crate::BlockErrorKind::ChecksumMismatch`] out of
    /// [`ArchiveReader::for_each_entries`].
    pub fn set_sub_stream_complete_hook(
        &mut self,
        hook: impl FnMut(SubStreamCompletion) + Send + 'static,
    ) {
        self.on_sub_stream_complete = Some(Box::new(hook));
    }

    /// Drops any hook set by [`ArchiveReader::set_sub_stream_complete_hook`].
    pub fn clear_sub_stream_complete_hook(&mut self) {
        self.on_sub_stream_complete = None;
    }

    /// The limits this reader was built with.
    #[must_use]
    pub fn limits(&self) -> ArchiveLimits {
        self.limits
    }

    /// A decoder for one block, borrowing this reader rather than consuming it.
    ///
    /// This is what lets a consumer parse the header once and then decode
    /// blocks from the same source: the header pass and the decode pass share
    /// one [`Read`] + [`Seek`], instead of the caller having to hand out a
    /// freshly opened reader per pass because the constructor took ownership.
    /// The decoder carries this reader's thread count and limits.
    ///
    /// # Errors
    ///
    /// [`Error::FileNotFound`] if `block_index` is past the last block.
    pub fn block_decoder(&mut self, block_index: usize) -> Result<BlockDecoder<'_, R>, Error> {
        if block_index >= self.archive.blocks.len() {
            return Err(Error::FileNotFound);
        }
        Ok(BlockDecoder {
            thread_count: self.thread_count,
            adaptive_lzma2: self.adaptive_lzma2,
            input_complete: self.input_complete,
            positional: self.positional.as_ref(),
            verify_checksums: self.verify_checksums,
            lzma2: Arc::clone(&self.lzma2),
            block_index,
            archive: &self.archive,
            password: &self.password,
            source: &mut self.source,
            limits: self.limits,
            on_sub_stream_complete: self
                .on_sub_stream_complete
                .as_mut()
                .map(|hook| &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send + '_)),
        })
    }

    /// The source this reader is reading from.
    ///
    /// Seeking it moves the reader's own cursor; every decode seeks to the
    /// block it wants first, so that is safe between calls, and is how a
    /// caller shares one open file between the header pass and the decode
    /// pass.
    pub fn source_mut(&mut self) -> &mut R {
        &mut self.source
    }

    /// Takes the source back, dropping the reader.
    pub fn into_source(self) -> R {
        self.source
    }

    /// Creates an [`ArchiveReader`] from an existing [`Archive`] instance.
    ///
    /// This is useful when you already have a parsed archive and want to create a reader
    /// without re-parsing the archive structure.
    ///
    /// # Arguments
    /// * `archive` - An existing parsed archive instance
    /// * `source` - The reader providing access to the archive data
    /// * `password` - Password for encrypted archives
    #[inline]
    pub fn from_archive(archive: Archive, source: R, password: Password) -> Self {
        Self {
            source,
            archive,
            password,
            thread_count: 1,
            adaptive_lzma2: false,
            input_complete: false,
            verify_checksums: true,
            lzma2: Arc::new(Lzma2Control::new(1)),
            index: OnceLock::new(),
            limits: ArchiveLimits::default(),
            on_block_complete: None,
            on_sub_stream_complete: None,
            positional: None,
        }
    }

    /// Sets how many threads one block's LZMA2 coder may decode on, clamped
    /// to `1..=256`.
    ///
    /// **The default is one**, which is the behaviour of every other coder in
    /// this crate and the behaviour this fork had before the parallel path
    /// existed: nothing is spawned, nothing is buffered, and the memory a
    /// decode needs is the dictionary. Upstream `sevenz-rust2` defaults this
    /// to `available_parallelism()`; a library that decides on its own to
    /// occupy every core and to hold a gigabyte while doing it is not a
    /// default a consumer can build a memory budget on, so the fork asks.
    ///
    /// The count takes effect at the next LZMA2 **run boundary**, so changing
    /// it during a decode is allowed and lossless — a run begins with a
    /// dictionary reset, which is exactly where one decoder can hand over to
    /// another. A count of `1` decodes one run at a time: on the calling
    /// thread for a coder built single-threaded, and on one worker for a
    /// coder that can widen, so that a widening is not held up behind a run
    /// decoded inline. Already-spawned workers park on their channel and cost
    /// nothing until it goes back up.
    ///
    /// A block only decodes in parallel at all if its coder was built for it:
    /// see [`ArchiveReader::set_adaptive_lzma2`] for starting at one thread
    /// and widening later.
    pub fn set_threads(&mut self, threads: u32) {
        self.thread_count = threads.clamp(1, 256);
        self.lzma2.set_threads(self.thread_count);
    }

    /// [`ArchiveReader::set_threads`], for a builder chain.
    #[must_use]
    pub fn with_threads(mut self, threads: u32) -> Self {
        self.set_threads(threads);
        self
    }

    /// The thread ceiling currently in force.
    pub fn threads(&self) -> u32 {
        self.thread_count
    }

    /// Upstream's name for [`ArchiveReader::set_threads`].
    pub fn set_thread_count(&mut self, thread_count: u32) {
        self.set_threads(thread_count);
    }

    /// Builds each block's LZMA2 coder so that it *can* widen later, even
    /// while the thread count is one.
    ///
    /// The choice between the plain decoder and the adaptive one is made when
    /// a block's coder is built, because the plain one is what costs nothing
    /// and a caller who never asked for threads must keep paying nothing. A
    /// consumer that intends to chase a download — decode with one thread
    /// while the tail is arriving, widen once a backlog of complete runs has
    /// built up, narrow again — asks for this once, leaves the count at one,
    /// and then drives [`Lzma2Handle::set_threads`] as it goes.
    pub fn set_adaptive_lzma2(&mut self, adaptive: bool) {
        self.adaptive_lzma2 = adaptive;
    }

    /// [`ArchiveReader::set_adaptive_lzma2`], for a builder chain.
    #[must_use]
    pub fn with_adaptive_lzma2(mut self) -> Self {
        self.set_adaptive_lzma2(true);
        self
    }

    /// Whether the header's CRC-32s are checked as the archive is decoded.
    /// On by default, which is what almost every caller wants.
    ///
    /// Turning it off is for a consumer that verifies the bytes by other
    /// means — a PAR2 set over the extracted files, say — and does not want to
    /// pay for the same assurance twice. With it off, a corrupt archive is
    /// decoded into corrupt bytes without complaint: this crate then reports
    /// only what the decoder itself notices, which is far less than a
    /// checksum notices. It exists to be measured against, too: a decode with
    /// it off is the floor the checked decode is compared with.
    pub fn set_verify_checksums(&mut self, verify: bool) {
        self.verify_checksums = verify;
    }

    /// [`ArchiveReader::set_verify_checksums`], for a builder chain.
    #[must_use]
    pub fn with_verify_checksums(mut self, verify: bool) -> Self {
        self.verify_checksums = verify;
        self
    }

    /// A handle on the LZMA2 coder of whichever block is decoding, which can
    /// be held and used while this reader is borrowed by a decode.
    pub fn lzma2_handle(&self) -> Lzma2Handle {
        Lzma2Handle {
            control: Arc::clone(&self.lzma2),
        }
    }

    /// What the LZMA2 coder of the block being decoded is doing right now, or
    /// `None` when no block is decoding through the adaptive path.
    pub fn lzma2_progress(&self) -> Option<Lzma2Progress> {
        self.lzma2.progress()
    }

    /// The name index, built by the first call.
    ///
    /// Of two entries with one name the later is the one found, as it was
    /// when the index was built at open.
    fn index(&self) -> &HashMap<String, IndexEntry> {
        self.index.get_or_init(|| {
            let files = &self.archive.files;
            let mut index = HashMap::with_capacity(files.len());
            for (file_index, file) in files.iter().enumerate() {
                let block_index = self
                    .archive
                    .stream_map
                    .file_block_index
                    .get(file_index)
                    .copied()
                    .flatten();
                index.insert(
                    file.name.clone(),
                    IndexEntry {
                        block_index,
                        file_index,
                    },
                );
            }
            index
        })
    }

    /// Returns a reference to the underlying [`Archive`] structure.
    ///
    /// This provides access to the archive metadata including files, blocks,
    /// and compression information.
    #[inline]
    pub fn archive(&self) -> &Archive {
        &self.archive
    }

    fn build_decode_stack<'r>(
        source: &'r mut R,
        archive: &Archive,
        block_index: usize,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<(Box<dyn DecodeRead + 'r>, usize), Error> {
        let block = &archive.blocks[block_index];
        crate::container::check_aes_coders(block.coders.iter(), opts.limits)?;
        // The chain as a whole against the memory limit, before any coder is
        // built; see `read_encoded_header`. Under `ArchiveReader::with_limits`
        // this never refuses anything, because `decoder_memory_estimate` has
        // already bounded the same sum from above; it is what bounds a
        // `BlockDecoder` built with limits of its own.
        let (reserved_kb, unsized_kb) = if block.total_input_streams()
            > block.total_output_streams()
        {
            // Every coder of a multi-stream graph is built.
            let graph = || {
                block
                    .coders
                    .iter()
                    .enumerate()
                    .map(|(index, coder)| (coder, block.get_unpack_size_at_index(index)))
            };
            (
                check_chain_memory(graph(), opts.limits, 0)?,
                unsized_coders_memory_kb(graph()),
            )
        } else {
            // Under the chain, the pack stream's read buffer (below).
            let chain = || {
                block
                    .ordered_coder_iter()
                    .map(|(index, coder)| (coder, block.get_unpack_size_at_index(index)))
            };
            (
                check_chain_memory(chain(), opts.limits, crate::decoder::INPUT_BUF_SIZE / 1024)?,
                unsized_coders_memory_kb(chain()),
            )
        };
        let mut opts = opts.reserving(reserved_kb, unsized_kb);
        if block.total_input_streams() > block.total_output_streams() {
            return Self::build_decode_stack2(source, archive, block_index, password, &opts);
        }
        let first_pack_stream_index = archive.stream_map.block_first_pack_stream_index[block_index];
        let block_offset = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .and_then(|v| {
                v.checked_add(archive.stream_map.pack_stream_offsets[first_pack_stream_index])
            })
            .ok_or_else(|| Error::corrupted("block offset out of range"))?;

        // A large stored folder, with threads to spare and a cursor of its
        // own on the archive, is read ahead on a thread that checksums as it
        // reads (and decrypts on another): see `store_overlap`.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(decoder) =
            Self::stored_overlap(archive, block_index, block_offset, password, &opts)?
        {
            return Ok((
                decoder,
                archive.pack_sizes[first_pack_stream_index] as usize,
            ));
        }

        let (mut has_crc, mut crc) = (block.has_crc, block.crc);

        // Single stream blocks might have it's CRC stored in the single substream information.
        // A caller that checks the file itself already checks exactly that
        // CRC over exactly these bytes, so it is borrowed only for the callers
        // whose one check this is.
        if !has_crc
            && !opts.files_verified
            && block.num_unpack_sub_streams == 1
            && let Some(sub_streams_info) = archive.sub_streams_info.as_ref()
        {
            let substream_index = archive
                .stream_map
                .block_first_sub_stream_index
                .get(block_index)
                .expect("validated sub-stream mapping");

            // Only when there is a single stream, we can use it's CRC to verify the compressed block data.
            // Multiple streams would contain the CRC of the compressed data for each file in the block.
            if sub_streams_info.has_crc.contains(substream_index) {
                has_crc = true;
                crc = u64::from(sub_streams_info.crcs[substream_index]);
            }
        }

        // A complete lone run has no parallel work to discover. Prove it
        // from framing before allocating the read-ahead queue, then use the
        // controlled decoder's ring immediately. Keep progress, ledger and
        // checksum folding; adaptive sources retain their ordinary scanner.
        // At most 4 MiB of header snippets (65,536 reads): a GiB-scale lone
        // run can contain over 16,000 compressed chunks. Packed payload is
        // skipped; smaller BCJ2 side-stream probes keep their separate cap.
        opts.single_run_lzma2 = opts.input_complete
            && opts.threads > 1
            && !opts.adaptive_lzma2
            && block.coders.len() == 1
            && block.coders[0].encoder_method_id() == EncoderMethod::ID_LZMA2
            && block.coders[0].properties().len() == 1
            && block.coders[0].properties()[0] <= 40
            && block.get_unpack_size() > crate::codec::lzma_turbo::MT_MIN_BLOCK_BYTES
            && crate::codec::lzma_turbo::is_single_run_with_limit(
                source,
                block_offset,
                archive.pack_sizes[first_pack_stream_index],
                block.get_unpack_size(),
                65_536,
            );
        // Always restore the cursor, including a declined or failed probe.
        source.seek(SeekFrom::Start(block_offset))?;
        let pack_size = archive.pack_sizes[first_pack_stream_index] as usize;

        // Buffered at the bottom too, for a chain whose first coder reads
        // whatever its caller asks for: Copy, or delta straight off the pack
        // stream, would otherwise make one read call per caller read, however
        // small. The block re-seeks the source before this, and the bounded
        // reader stops the read-ahead at the pack stream's end, so nothing
        // beyond the block is consumed.
        let block = &archive.blocks[block_index];
        // Before any coder is built: the plan's pipes are reserved with the
        // coders, so one that fits itself to what the limit leaves sees them.
        let mut chain = Chain::new(block, &mut opts);
        let opts = &opts;
        let mut stage = Stage::Leaf(Box::new(BoundedReader::new(source, pack_size)));
        for (index, coder) in block.ordered_coder_iter() {
            if coder.num_in_streams() != 1 || coder.num_out_streams() != 1 {
                return Err(Error::unsupported(
                    "Multi input/output stream coders are not supported",
                ));
            }
            stage = chain.add(block, stage, index, password, opts)?;
        }
        let mut decoder: Box<dyn DecodeRead + 'r> = Box::new(DeclaredLength {
            inner: chain.pipeline.top(stage),
            owed: block.get_unpack_size(),
        });
        // Read after the coders are built: the LZMA2 coder decides there
        // whether its workers fold the checksums, and when they do, the
        // block's is folded with them rather than taken again here, on the
        // thread delivering the bytes. A caller that said not to verify gets
        // no verifying reader at all.
        //
        // A chain with stages on threads of their own is checked here too.
        // Its top coder runs on this thread whatever the plan, so this is
        // the thread the bytes are made on, and the checksum is taken where
        // they are made: handing them to another thread for it would cost a
        // pipe and a copy, more than the checksum does.
        if has_crc && opts.verify_checksums && !opts.folding_checksums() {
            decoder = Box::new(Crc32VerifyingReader::new(
                decoder,
                block.get_unpack_size() as usize,
                crc,
            ));
        }

        Ok((decoder, pack_size))
    }

    /// The decode chain of a stored folder (COPY, AES, or AES under COPY)
    /// with its reading, checksumming and decryption on threads of their
    /// own, or `None` where the folder is decoded on the calling thread as
    /// before.
    ///
    /// Only for a folder of at least [`FOLDER_STAGE_BYTES`] — below that the
    /// threads cost more than the overlap gains, and the folder-parallel
    /// planner has it — given two threads or more, a complete input and a
    /// positional source, the one thing that can be read from another
    /// thread. Its checksums must be the caller's to fold
    /// (`checksum_splits`), or not wanted at all, so that none is taken on
    /// the thread that delivers the bytes. The spans in flight are held
    /// within the memory limit beside the chain's coders; a limit that
    /// cannot afford them, or a thread the system will not give, leaves the
    /// folder to the serial path.
    #[cfg(not(target_arch = "wasm32"))]
    fn stored_overlap(
        archive: &Archive,
        block_index: usize,
        block_offset: u64,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<Option<Box<dyn DecodeRead + 'static>>, Error> {
        let block = &archive.blocks[block_index];
        let unpack_size = block.get_unpack_size();
        let (Some(positional), Some(control)) = (opts.positional, opts.lzma2_control) else {
            return Ok(None);
        };
        if opts.threads < 2
            || !opts.input_complete
            || unpack_size < FOLDER_STAGE_BYTES
            || (opts.verify_checksums && opts.checksum_splits.is_empty())
            || block.packed_streams().len() != 1
        {
            return Ok(None);
        }
        let Some(aes) = stored_chain(block) else {
            return Ok(None);
        };
        let stages = 1 + u64::from(aes.is_some());
        let spans_kb =
            usize::try_from(stages * store_overlap::STAGE_BYTES / 1024).unwrap_or(usize::MAX);
        if opts
            .reserved_kb
            .saturating_add(opts.unsized_kb)
            .saturating_add(spans_kb)
            > opts.limits.memory_limit_kb()
        {
            return Ok(None);
        }
        let first_pack_stream_index = archive.stream_map.block_first_pack_stream_index[block_index];
        let pack_size = archive.pack_sizes[first_pack_stream_index];
        let mut cursor = ReadAtCursor::new(Arc::clone(positional));
        cursor.seek(SeekFrom::Start(block_offset))?;
        let packed = BoundedReader::new(
            cursor,
            usize::try_from(pack_size)
                .map_err(|_| Error::corrupted("pack stream size exceeds address space"))?,
        );
        let checksums = (!opts.checksum_splits.is_empty()).then(|| {
            store_overlap::Checksums::new(Arc::clone(control), opts.checksum_splits, unpack_size)
        });
        let plain = match aes {
            None => store_overlap::spawn(packed, unpack_size, checksums),
            #[cfg(feature = "aes256")]
            Some(index) => {
                let Ok(fetch) = store_overlap::spawn(packed, pack_size, None) else {
                    return Ok(None);
                };
                let coder = &block.coders[index];
                let len = block.get_unpack_size_at_index(index) as usize;
                // The serial path's decoder, over the fetched ciphertext.
                let crate::decoder::Decoder::Aes256Sha256(decrypting) =
                    add_decoder(fetch, len, coder, password, opts)?
                else {
                    return Ok(None);
                };
                store_overlap::spawn(decrypting, unpack_size, checksums)
            }
            #[cfg(not(feature = "aes256"))]
            Some(_) => {
                let _ = password;
                return Ok(None);
            }
        };
        let Ok(plain) = plain else {
            return Ok(None);
        };
        if !opts.checksum_splits.is_empty() {
            control.fold_elsewhere();
        }
        Ok(Some(Box::new(DeclaredLength {
            inner: plain,
            owed: unpack_size,
        })))
    }

    fn build_decode_stack2<'r>(
        source: &'r mut R,
        archive: &Archive,
        block_index: usize,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<(Box<dyn DecodeRead + 'r>, usize), Error> {
        const MAX_CODER_COUNT: usize = 32;
        let block = &archive.blocks[block_index];
        if block.coders.len() > MAX_CODER_COUNT {
            return Err(Error::unsupported(format!(
                "Too many coders: {}",
                block.coders.len()
            )));
        }

        assert!(block.total_input_streams() > block.total_output_streams());
        let shared_source = Rc::new(RefCell::new(source));
        let first_pack_stream_index = archive.stream_map.block_first_pack_stream_index[block_index];
        let start_pos = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .ok_or_else(|| Error::corrupted("pack position out of range"))?;
        let offsets = &archive.stream_map.pack_stream_offsets[first_pack_stream_index..];

        // A direct pack stream can be inspected by seeking over payload.
        // Unlike the adaptive reader's initial scan, this keeps no packed
        // input alongside the dictionary. Only a proven complete lone run
        // changes the plan; filters/encryption below LZMA2 and additional
        // resets retain their existing worker path. An adaptive chase may
        // expose only a downloaded prefix, so it must not probe ahead.
        // SharedBoundedReader
        // re-seeks before every read, including after an unsuccessful probe.
        let serial_coder = (|| {
            if !opts.input_complete
                || opts.threads < 2
                || opts.adaptive_lzma2
                || !block
                    .coders
                    .iter()
                    .any(|coder| coder.encoder_method_id() == EncoderMethod::ID_BCJ2)
            {
                return None;
            }
            let index = crate::pipeline::parallel_lzma2_coder(block)?;
            let coder = &block.coders[index];
            let unpacked = block.get_unpack_size_at_index(index);
            if coder.num_in_streams() != 1
                || coder.num_out_streams() != 1
                || coder.properties().len() != 1
                || coder.properties()[0] > 40
                || unpacked <= crate::codec::lzma_turbo::MT_MIN_BLOCK_BYTES
            {
                return None;
            }
            let input = block.coders[..index]
                .iter()
                .try_fold(0u64, |sum, coder| sum.checked_add(coder.num_in_streams()))?;
            let pack = block.packed_streams().iter().position(|&s| s == input)?;
            let offset = start_pos.checked_add(*offsets.get(pack)?)?;
            let packed = *archive.pack_sizes.get(first_pack_stream_index + pack)?;
            crate::codec::lzma_turbo::is_single_run(
                &mut **shared_source.borrow_mut(),
                offset,
                packed,
                unpacked,
            )
            .then_some(index)
        })();

        let mut sources = Vec::with_capacity(block.packed_streams().len());

        for (i, offset) in offsets[..block.packed_streams().len()].iter().enumerate() {
            let pack_pos = start_pos
                .checked_add(*offset)
                .ok_or_else(|| Error::corrupted("pack stream offset out of range"))?;
            let pack_size = archive.pack_sizes[first_pack_stream_index + i];
            let pack_end = pack_pos
                .checked_add(pack_size)
                .ok_or_else(|| Error::corrupted("pack stream size out of range"))?;

            let pack_reader =
                SharedBoundedReader::new(Rc::clone(&shared_source), (pack_pos, pack_end));

            sources.push(pack_reader);
        }

        let mut coder_to_stream_map = [usize::MAX; MAX_CODER_COUNT];

        let mut si = 0;
        for (i, coder) in block.coders.iter().enumerate() {
            coder_to_stream_map[i] = si;
            si += coder.num_in_streams() as usize;
        }

        let main_coder_index = {
            let mut coder_used = [false; MAX_CODER_COUNT];
            for bp in block.bind_pairs() {
                // `out_index` is validated `< total_output_streams` at parse time, but the
                // `coder_used` array indexes coders (max `MAX_CODER_COUNT`); reject an
                // index that would fall outside it instead of panicking.
                let out_index = bp.out_index as usize;
                if out_index >= block.coders.len() {
                    return Err(Error::corrupted("bind pair out index exceeds coder count"));
                }
                coder_used[out_index] = true;
            }
            let mut mci = 0;
            for (i, used) in coder_used[..block.coders.len()].iter().enumerate() {
                if !used {
                    mci = i;
                    break;
                }
            }
            mci
        };

        // Build the decoder for the folder's final output by resolving the main coder's
        // output stream. `get_in_stream2` recursively wires up the whole coder graph,
        // so this also handles single-input filters (e.g. Delta) layered on top of a
        // BCJ2 coder's output, not just a bare BCJ2 main coder.
        let mut opts = *opts;
        let mut chain = Chain::with_serial_coder(block, &mut opts, serial_coder);
        let opts = &opts;
        let stage = Self::get_in_stream2(
            block,
            &sources,
            &coder_to_stream_map,
            password,
            main_coder_index,
            0,
            opts,
            &mut chain,
        )?;
        let mut decoder: Box<dyn DecodeRead + 'r> = Box::new(DeclaredLength {
            inner: chain.pipeline.top(stage),
            owed: block.get_unpack_size(),
        });
        // On this thread, where the graph's last coder makes the bytes: see
        // `build_decode_stack`.
        if block.has_crc && opts.verify_checksums {
            decoder = Box::new(Crc32VerifyingReader::new(
                decoder,
                block.get_unpack_size() as usize,
                block.crc,
            ));
        }
        Ok((
            decoder,
            archive.pack_sizes[first_pack_stream_index] as usize,
        ))
    }

    // One more parameter than clippy likes, because the memory limit has to
    // reach `add_decoder` at the bottom of a BCJ2 chain. Bundling upstream's
    // five into a context struct would widen the diff against upstream for no
    // gain.
    #[allow(clippy::too_many_arguments)]
    fn get_in_stream<'r>(
        block: &Block,
        sources: &[SharedBoundedReader<'r, R>],
        coder_to_stream_map: &[usize],
        password: &Password,
        in_stream_index: usize,
        depth: usize,
        opts: &DecodeOptions<'_>,
        chain: &mut Chain<'r>,
    ) -> Result<Stage<'r>, Error>
    where
        R: 'r,
    {
        let index = block
            .packed_streams()
            .iter()
            .position(|&i| i == in_stream_index as u64);
        if let Some(index) = index {
            if opts.input_complete
                && opts.threads > 1
                && !opts.adaptive_lzma2
                && let Some(positional) = opts.positional
            {
                // Every pack stream gets an independent cursor, including
                // those consumed inline. Never mix positional worker reads
                // with the caller's shared seek/read cursor on this graph.
                let (start, end) = sources[index].bounds;
                let size = usize::try_from(end - start)
                    .map_err(|_| Error::corrupted("pack stream size exceeds address space"))?;
                let mut cursor = ReadAtCursor::new(Arc::clone(positional));
                cursor.seek(SeekFrom::Start(start))?;
                return Ok(Stage::OwnedLeaf(Box::new(BoundedReader::new(cursor, size))));
            }
            return Ok(Stage::Leaf(Box::new(sources[index].clone())));
        }

        let bp = block
            .find_bind_pair_for_in_stream(in_stream_index as u64)
            .ok_or_else(|| {
                Error::corrupted(format!(
                    "Couldn't find bind pair for stream {in_stream_index}"
                ))
            })?;
        let index = bp.out_index as usize;

        Self::get_in_stream2(
            block,
            sources,
            coder_to_stream_map,
            password,
            index,
            depth,
            opts,
            chain,
        )
    }

    // One more parameter than clippy likes, because the memory limit has to
    // reach `add_decoder` at the bottom of a BCJ2 chain. Bundling upstream's
    // five into a context struct would widen the diff against upstream for no
    // gain.
    #[allow(clippy::too_many_arguments)]
    fn get_in_stream2<'r>(
        block: &Block,
        sources: &[SharedBoundedReader<'r, R>],
        coder_to_stream_map: &[usize],
        password: &Password,
        in_stream_index: usize,
        depth: usize,
        opts: &DecodeOptions<'_>,
        chain: &mut Chain<'r>,
    ) -> Result<Stage<'r>, Error>
    where
        R: 'r,
    {
        // Each coder is visited at most once in an acyclic graph, so a traversal deeper
        // than the coder count means the bind pairs form a cycle. Bail out instead of
        // recursing until the stack overflows (an uncatchable abort).
        if depth > block.coders.len() {
            return Err(Error::corrupted("cyclic coder bind-pair graph"));
        }
        let (Some(coder), Some(&start_index)) = (
            block.coders.get(in_stream_index),
            coder_to_stream_map.get(in_stream_index),
        ) else {
            return Err(Error::corrupted("in_stream_index out of range"));
        };
        if start_index == usize::MAX {
            return Err(Error::corrupted("in_stream_index out of range"));
        }
        let uncompressed_len = *block
            .unpack_sizes()
            .get(in_stream_index)
            .ok_or_else(|| Error::corrupted("in_stream_index out of range"))?
            as usize;
        if coder.num_in_streams() == 1 {
            let input = Self::get_in_stream(
                block,
                sources,
                coder_to_stream_map,
                password,
                start_index,
                depth + 1,
                opts,
                chain,
            )?;
            return chain.add(block, input, in_stream_index, password, opts);
        }

        // BCJ2 is the only multi-input coder we support. It takes four input streams
        // (main, call, jump and range-coder) and produces a single output stream.
        if coder.encoder_method_id() == EncoderMethod::ID_BCJ2 {
            let num_in_streams = coder.num_in_streams() as usize;
            // The BCJ2 decoder indexes exactly four input streams; reject a malformed count
            // up front instead of handing a short/long input list to the upstream decoder.
            if num_in_streams != 4 {
                return Err(Error::corrupted(
                    "BCJ2 coder must declare exactly four input streams",
                ));
            }
            let mut inputs: Vec<Box<dyn DecodeRead>> = Vec::with_capacity(num_in_streams);
            for i in start_index..start_index + num_in_streams {
                let input = Self::get_in_stream(
                    block,
                    sources,
                    coder_to_stream_map,
                    password,
                    i,
                    depth + 1,
                    opts,
                    chain,
                )?;
                inputs.push(chain.pipeline.top(input));
            }
            return Ok(Stage::Here(Box::new(crate::decoder::ReadOnly(
                Bcj2Reader::with_chunks(inputs, uncompressed_len as u64, |input, max, spare| {
                    input.take_chunk(max, spare)
                }),
            ))));
        }

        Err(Error::unsupported(format!(
            "Unsupported multi-input coder: {:?}",
            coder.encoder_method_id()
        )))
    }

    /// Takes a closure to decode each files in the archive.
    ///
    /// Attention about solid archive:
    /// When decoding a solid archive, the data to be decompressed depends on the data in front of it,
    /// you cannot simply skip the previous data and only decompress the data in the back.
    pub fn for_each_entries<F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>>(
        &mut self,
        mut each: F,
    ) -> Result<(), Error> {
        self.for_each_entries_direct(|entry, reader| each(entry, reader))
    }

    /// As [`ArchiveReader::for_each_entries`], handing each entry over as an
    /// [`EntryRead`], which can also write itself on with
    /// [`EntryRead::write_rest`] straight from the decoder's output.
    pub fn for_each_entries_direct<F>(&mut self, mut each: F) -> Result<(), Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn EntryRead) -> Result<bool, Error>,
    {
        // `Archive` is public and `ArchiveReader::from_archive` takes one as
        // it is: a map that does not cover the archive is refused here,
        // before anything is decoded, and each block's own entries are
        // checked as it is opened.
        let map = &self.archive.stream_map;
        if map.file_block_index.len() != self.archive.files.len()
            || map.block_first_file_index.len() != self.archive.blocks.len()
            || map
                .file_block_index
                .iter()
                .flatten()
                .any(|&block| block >= self.archive.blocks.len())
        {
            return Err(Error::corrupted(
                "the archive's stream map does not match it",
            ));
        }
        for phase in self.folder_phases() {
            match phase {
                FolderPhase::Alone { block } => self.decode_block_alone(block, &mut each)?,
                #[cfg(not(target_arch = "wasm32"))]
                FolderPhase::Parallel {
                    blocks,
                    workers,
                    threads_per_folder,
                } => self.decode_blocks_parallel(blocks, workers, threads_per_folder, &mut each)?,
                // Never planned without threads; walked in order if it were.
                #[cfg(target_arch = "wasm32")]
                FolderPhase::Parallel { blocks, .. } => {
                    for block in blocks {
                        self.decode_block_alone(block, &mut each)?;
                    }
                }
            }
        }
        // decode empty files
        for file_index in 0..self.archive.files.len() {
            let block_index = self.archive.stream_map.file_block_index[file_index];
            if block_index.is_none() {
                let file = &self.archive.files[file_index];
                // The parser gives such a file no CRC; a caller's archive can.
                if empty_file_crc_mismatch(file, self.verify_checksums) {
                    return Err(Error::ChecksumVerificationFailed);
                }
                let empty_reader: &mut dyn EntryRead = &mut ([0u8; 0].as_slice());
                if !each(file, empty_reader)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// How [`ArchiveReader::for_each_entries`] walks the folders: every one
    /// alone on the calling thread, unless there is a positional source and
    /// more than one thread to share among runs of small folders.
    fn folder_phases(&self) -> impl Iterator<Item = FolderPhase> + use<R> {
        #[cfg(not(target_arch = "wasm32"))]
        let planned = if self.positional.is_some() && self.thread_count > 1 {
            let folders = self.archive.blocks.iter().map(|block| {
                let estimate = block.coders.iter().try_fold(0u64, |sum, coder| {
                    crate::container::coder_memory_estimate(coder)
                        .ok()
                        .map(|bytes| sum.saturating_add(bytes))
                });
                (block.get_unpack_size(), estimate.unwrap_or(u64::MAX))
            });
            Some(plan_folders(
                folders,
                self.thread_count,
                self.limits.memory_limit_bytes,
            ))
        } else {
            None
        };
        #[cfg(target_arch = "wasm32")]
        let planned: Option<Vec<FolderPhase>> = None;
        // Sequential decoding needs only a cursor, not one scheduling record
        // per folder. The returned iterator owns its plan and never borrows
        // the reader while callbacks decode the next block.
        let sequential_end = if planned.is_some() {
            0
        } else {
            self.archive.blocks.len()
        };
        planned
            .into_iter()
            .flatten()
            .chain((0..sequential_end).map(|block| FolderPhase::Alone { block }))
    }

    /// One folder, decoded on the calling thread with the reader's own thread
    /// count: the sequential path, unchanged.
    fn decode_block_alone<F>(&mut self, block_index: usize, each: &mut F) -> Result<(), Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn EntryRead) -> Result<bool, Error>,
    {
        let block_decoder = BlockDecoder {
            thread_count: self.thread_count,
            adaptive_lzma2: self.adaptive_lzma2,
            input_complete: self.input_complete,
            positional: self.positional.as_ref(),
            verify_checksums: self.verify_checksums,
            lzma2: Arc::clone(&self.lzma2),
            block_index,
            archive: &self.archive,
            password: &self.password,
            source: &mut self.source,
            limits: self.limits,
            on_sub_stream_complete: self
                .on_sub_stream_complete
                .as_mut()
                .map(|hook| &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send + '_)),
        };
        let outcome = block_decoder.walk_entries(each)?;
        // Upstream moves on to the next block when a callback returns
        // `false`, and consumers rely on that; only the hook treats it as
        // "this block was not decoded in full".
        Self::block_completed(
            &self.archive,
            self.on_block_complete
                .as_mut()
                .map(|hook| &mut **hook as &mut (dyn FnMut(BlockCompletion) + Send)),
            block_index,
            outcome,
        );
        Ok(())
    }

    /// Fires the block hook for a block the caller let run to the end.
    fn block_completed(
        archive: &Archive,
        hook: Option<&mut (dyn FnMut(BlockCompletion) + Send)>,
        block_index: usize,
        outcome: BlockOutcome,
    ) {
        // Only a block the caller let run to the end, and whose every byte
        // went through the callbacks' readers, has been decoded and checked
        // in full; a callback that stopped early, or went on with bytes of
        // the block's last file unread, leaves the block's own checksum
        // uncompared, and saying otherwise would be a completion claim
        // nobody verified.
        let (Some(hook), true) = (hook, outcome.finished && outcome.read_in_full) else {
            return;
        };
        let block = &archive.blocks[block_index];
        let sub_streams = archive.block_sub_streams(block_index);
        let crc_verified = block.has_crc
            || (!sub_streams.is_empty()
                && sub_streams
                    .iter()
                    .all(|sub_stream| sub_stream.crc.is_some()));
        hook(BlockCompletion {
            block_index,
            unpacked_size: block.get_unpack_size(),
            crc_verified,
        });
    }

    /// A run of small folders, decoded `workers` at a time from the
    /// positional source and handed to `each` in archive order.
    #[cfg(not(target_arch = "wasm32"))]
    fn decode_blocks_parallel<F>(
        &mut self,
        blocks: std::ops::Range<usize>,
        workers: u32,
        threads_per_folder: u32,
        each: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn EntryRead) -> Result<bool, Error>,
    {
        let Some(source) = self.positional.clone() else {
            for block in blocks {
                self.decode_block_alone(block, each)?;
            }
            return Ok(());
        };
        let first = blocks.start;
        let workers = workers as usize;
        // No one coder is the reader's while the run lasts, so what the last
        // block reported stops being true here, and nothing is reported in
        // its place. The ceiling still holds. It starts at the reader's own
        // count, where a block's coder puts it as it is built, and follows
        // the handle from there: see `FolderWorker::folders_at_once`.
        self.lzma2.set_block_index(first);
        self.lzma2.set_threads(self.thread_count);
        let worker_side = FolderWorker {
            archive: &self.archive,
            password: &self.password,
            limits: self.limits,
            verify_checksums: self.verify_checksums,
            threads: threads_per_folder,
            ceiling: &self.lzma2,
            source: &source,
        };
        let archive = &self.archive;
        let verify_checksums = self.verify_checksums;
        let on_block = &mut self.on_block_complete;
        let on_sub = &mut self.on_sub_stream_complete;
        let outcome = crate::ordered::run_paced(
            blocks.len(),
            workers,
            FOLDER_WINDOW_PER_WORKER * workers,
            FOLDER_STAGE_BYTES as usize,
            &|| worker_side.folders_at_once(),
            |job, tx| worker_side.decode(first + job, tx),
            |job, rx| {
                let block_index = first + job;
                let outcome = replay_folder(
                    archive,
                    archive.blocks[block_index].is_encrypted(),
                    verify_checksums,
                    block_index,
                    rx,
                    each,
                    on_sub
                        .as_mut()
                        .map(|hook| &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send)),
                )?;
                Self::block_completed(
                    archive,
                    on_block
                        .as_mut()
                        .map(|hook| &mut **hook as &mut (dyn FnMut(BlockCompletion) + Send)),
                    block_index,
                    outcome,
                );
                Ok(())
            },
        );
        match outcome {
            Some(result) => result,
            // Not one worker could be started: decode the run here instead.
            None => {
                for block in blocks {
                    self.decode_block_alone(block, each)?;
                }
                Ok(())
            }
        }
    }

    /// Returns the data of a file with the given path inside the archive.
    ///
    /// # Notice
    /// This function is very inefficient when used with solid archives, since
    /// it needs to decode all data before the actual file.
    pub fn read_file(&mut self, name: &str) -> Result<Vec<u8>, Error> {
        let index_entry = *self.index().get(name).ok_or(Error::FileNotFound)?;
        let file = &self.archive.files[index_entry.file_index];

        if !file.has_stream {
            return Ok(Vec::new());
        }

        let block_index = index_entry
            .block_index
            .ok_or_else(|| Error::corrupted("File has no associated block"))?;

        match self.archive.is_solid {
            true => {
                let mut result = None;
                let target_file_ptr = file as *const _;

                BlockDecoder {
                    thread_count: self.thread_count,
                    adaptive_lzma2: self.adaptive_lzma2,
                    input_complete: self.input_complete,
                    positional: self.positional.as_ref(),
                    verify_checksums: self.verify_checksums,
                    lzma2: Arc::clone(&self.lzma2),
                    block_index,
                    archive: &self.archive,
                    password: &self.password,
                    source: &mut self.source,
                    limits: self.limits,
                    on_sub_stream_complete: self.on_sub_stream_complete.as_mut().map(|hook| {
                        &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send + '_)
                    }),
                }
                .for_each_entries(&mut |archive_entry, reader| {
                    let mut data =
                        Vec::with_capacity((archive_entry.size as usize).min(MAX_PREALLOC_BYTES));
                    reader.read_to_end(&mut data)?;

                    if std::ptr::eq(archive_entry, target_file_ptr) {
                        result = Some(data);
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                })?;

                result.ok_or(Error::FileNotFound)
            }
            false => {
                check_block_mapping(&self.archive, block_index)?;
                let pack_index = self.archive.stream_map.block_first_pack_stream_index[block_index];
                let pack_offset = self.archive.stream_map.pack_stream_offsets[pack_index];
                let block_offset = SIGNATURE_HEADER_SIZE
                    .checked_add(self.archive.pack_pos)
                    .and_then(|v| v.checked_add(pack_offset))
                    .ok_or_else(|| Error::corrupted("block offset out of range"))?;

                self.source.seek(SeekFrom::Start(block_offset))?;

                let opts = DecodeOptions {
                    limits: &self.limits,
                    threads: self.thread_count,
                    adaptive_lzma2: self.adaptive_lzma2,
                    input_complete: self.input_complete,
                    positional: self.positional.as_ref(),
                    single_run_lzma2: false,
                    verify_checksums: self.verify_checksums,
                    lzma2_control: Some(&self.lzma2),
                    // One file is read here and its checksum is verified as it
                    // streams past; there are no other boundaries to declare.
                    checksum_splits: &[],
                    // Below, against the file's own CRC.
                    files_verified: true,
                    reserved_kb: 0,
                    unsized_kb: 0,
                    encrypted: self.archive.blocks[block_index].is_encrypted(),
                };
                self.lzma2.set_block_index(block_index);
                let (mut block_reader, _size) = Self::build_decode_stack(
                    &mut self.source,
                    &self.archive,
                    block_index,
                    &self.password,
                    &opts,
                )?;

                let mut data = Vec::with_capacity((file.size as usize).min(MAX_PREALLOC_BYTES));
                let mut decoder: Box<dyn Read> =
                    Box::new(BoundedReader::new(&mut block_reader, file.size as usize));

                if file.has_crc {
                    decoder = Box::new(Crc32VerifyingReader::new(
                        decoder,
                        file.size as usize,
                        file.crc,
                    ));
                }

                decoder.read_to_end(&mut data).map_err(|e| {
                    // A checksum that did not match is reported located and
                    // typed, as `for_each_entries` reports it: in a block
                    // that decrypts it may be the wrong key decrypting to
                    // garbage, so it stays `Password` there, never repairable
                    // damage. A block that does not decrypt is damaged
                    // whatever password the caller holds.
                    let e = Error::from(e);
                    let checksum = e.is_checksum_failure();
                    let e = e.maybe_bad_password(opts.encrypted);
                    if checksum {
                        let packed_offset = self
                            .archive
                            .block_pack_streams(block_index)
                            .first()
                            .map_or(0, |range| range.offset);
                        e.in_block(block_index, packed_offset)
                    } else {
                        e
                    }
                })?;

                Ok(data)
            }
        }
    }

    /// Get the compression method(s) used for a specific file in the archive.
    pub fn file_compression_methods(
        &self,
        file_name: &str,
        methods: &mut Vec<EncoderMethod>,
    ) -> Result<(), Error> {
        let index_entry = self.index().get(file_name).ok_or(Error::FileNotFound)?;
        let file = &self.archive.files[index_entry.file_index];

        if !file.has_stream {
            return Ok(());
        }

        let block_index = index_entry
            .block_index
            .ok_or_else(|| Error::corrupted("File has no associated block"))?;

        let block = self
            .archive
            .blocks
            .get(block_index)
            .ok_or_else(|| Error::corrupted("Block not found"))?;

        block
            .coders
            .iter()
            .filter_map(|coder| EncoderMethod::by_id(coder.encoder_method_id()))
            .for_each(|method| {
                methods.push(method);
            });

        Ok(())
    }
}

/// A file whose bytes were handed over before the parallel LZMA2 coder had
/// reported their checksum.
///
/// The coder reports a run's checksums when the run is finished. A run it
/// decodes on the calling thread, as its bytes arrive, stays one open piece
/// until the coder reaches the end of the stream, so a file inside such a run
/// is delivered before its checksum is. Its comparison and its completion
/// wait here until the checksum arrives.
struct OwedCompletion {
    file_index: usize,
    sub_stream_index: usize,
    unpacked_offset: u64,
    len: u64,
}

/// The files of one block that [`BlockDecoder::for_each_entries`] has handed
/// over and not yet compared with their CRCs, in file order.
struct OwedCompletions<'b> {
    archive: &'b Archive,
    lzma2: &'b Lzma2Control,
    block_index: usize,
    packed_offset: u64,
    queue: VecDeque<OwedCompletion>,
}

impl OwedCompletions<'_> {
    /// Compares the files at the front of the queue whose checksums have
    /// arrived and reports their completions, stopping at the first one that
    /// is still missing.
    ///
    /// `at_end` says the stream has been read to its end, after which a
    /// checksum that is missing will never arrive. A file with a CRC to
    /// compare is then an error, never a file passed unchecked.
    fn settle<H>(&mut self, at_end: bool, mut hook: Option<&mut H>) -> Result<(), Error>
    where
        H: FnMut(SubStreamCompletion) + ?Sized,
    {
        while let Some(next) = self.queue.front() {
            let file = &self.archive.files[next.file_index];
            match self.lzma2.folded(next.unpacked_offset, next.len) {
                Some(crc32) => {
                    if file.has_crc && u64::from(crc32) != file.crc {
                        return Err(Error::ChecksumVerificationFailed
                            .in_block(self.block_index, self.packed_offset));
                    }
                    if let Some(hook) = hook.as_deref_mut() {
                        hook(SubStreamCompletion {
                            block_index: self.block_index,
                            sub_stream_index: next.sub_stream_index,
                            file_index: next.file_index,
                            unpacked_offset: next.unpacked_offset,
                            len: next.len,
                            crc32,
                        });
                    }
                }
                None if !at_end => break,
                None if file.has_crc => return Err(self.unverified()),
                None => {}
            }
            self.queue.pop_front();
        }
        Ok(())
    }

    /// The failure of a block whose bytes were all read and whose coder
    /// reported no checksum for them: they cannot be vouched for.
    fn unverified(&self) -> Error {
        Error::internal("the decoder reported no checksum for bytes that were read to their end")
            .in_block(self.block_index, self.packed_offset)
    }
}

/// How much of a block's stream is read at once to reach its end.
const STREAM_END_CHUNK: usize = 64 << 10;

/// The files of `block_index`, always a range inside `archive.files`.
///
/// `Archive` and its `StreamMap` are public, so a caller can hand in a map
/// the parser would never have built: whatever it says, the range is
/// clamped to the files that exist and never runs backwards, and a block
/// the map does not cover has no files.
fn block_file_range(archive: &Archive, block_index: usize) -> std::ops::Range<usize> {
    let map = &archive.stream_map;
    let len = archive.files.len();
    let (Some(block), Some(&start)) = (
        archive.blocks.get(block_index),
        map.block_first_file_index.get(block_index),
    ) else {
        return len..len;
    };
    let start = start.min(len);
    if block.num_unpack_sub_streams == 0 {
        return start..start;
    }
    let end = archive
        .blocks
        .iter()
        .enumerate()
        .skip(block_index.saturating_add(1))
        .find(|(_, block)| block.num_unpack_sub_streams != 0)
        .and_then(|(index, _)| map.block_first_file_index.get(index).copied())
        .unwrap_or(len);
    let mut end = end.clamp(start, len);
    // SzReadHeader2: empty entries between a folder's streams still belong
    // to that folder. Its substream count alone is not its file span.
    // Empty entries after the final stream are delivered separately.
    while end > start && map.file_block_index.get(end - 1) != Some(&Some(block_index)) {
        end -= 1;
    }
    start..end
}

/// Refuses a block that `archive`'s stream map does not describe: no such
/// block, or a first pack stream, packed span, sub-stream or file index the
/// map does not have. The parser never builds such a map, but `Archive` and
/// `StreamMap` are public, so a caller can, and decoding indexes by these.
fn check_block_mapping(archive: &Archive, block_index: usize) -> Result<(), Error> {
    let map = &archive.stream_map;
    let block = archive
        .blocks
        .get(block_index)
        .ok_or_else(|| Error::invalid_argument("block index out of range"))?;
    let pack_streams = map
        .block_first_pack_stream_index
        .get(block_index)
        .and_then(|first| first.checked_add(block.packed_streams().len()))
        .filter(|&end| end <= map.pack_stream_offsets.len() && end <= archive.pack_sizes.len());
    if pack_streams.is_none()
        || map.block_first_sub_stream_index.get(block_index).is_none()
        || map.block_first_file_index.get(block_index).is_none()
    {
        return Err(Error::corrupted(
            "the archive's stream map does not describe this block",
        ));
    }
    Ok(())
}

/// Whether a file with no bytes declares a CRC that is not the CRC of no
/// bytes, which is zero. Never when checksums are not being verified.
fn empty_file_crc_mismatch(file: &ArchiveEntry, verify_checksums: bool) -> bool {
    verify_checksums && file.has_crc && file.crc != 0
}

/// Reads what a callback that asked to go on (`go_on`) left of its file,
/// when `drain` says a later file needs the stream lined up. It goes through
/// the reader the callback was handed, so a checksum over the file and the
/// recording of the block's faults see the bytes as if the callback had read
/// them. Returns `go_on`.
fn drain_unread(reader: &mut dyn EntryRead, go_on: bool, drain: bool) -> Result<bool, Error> {
    if go_on && drain {
        reader.write_rest(&mut io::sink())?;
    }
    Ok(go_on)
}

/// How [`BlockDecoder::walk_entries`] ended.
#[derive(Debug, Clone, Copy)]
struct BlockOutcome {
    /// No callback asked to stop.
    finished: bool,
    /// Every byte of the block's decoded stream went through the readers the
    /// callbacks were handed, so every checksum over it has been compared.
    read_in_full: bool,
}

/// Reads and discards `left` more bytes of a block's decoded stream, then
/// makes the read that sees the stream's end.
///
/// A parallel LZMA2 coder is asked for no more than its caller has room for,
/// so it does not look past the last byte it hands over for the end of its
/// own input, and it is on reaching that end that it reports the checksums of
/// a run it decoded on the calling thread. This is the read that takes it
/// there. One byte past `left` is asked for and no more, however long the
/// stream turns out to be.
fn read_to_stream_end<R: Read>(reader: &mut R, mut left: u64) -> io::Result<()> {
    if left > 0 {
        let mut scratch = vec![0u8; STREAM_END_CHUNK];
        while left > 0 {
            let want = left.min(scratch.len() as u64) as usize;
            match reader.read(&mut scratch[..want]) {
                // Shorter than its sizes say: the stream has ended all the
                // same, and what is missing is reported by whoever is owed it.
                Ok(0) => return Ok(()),
                Ok(n) => left -= n as u64,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
    let mut probe = [0u8; 1];
    loop {
        match reader.read(&mut probe) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            end => return end.map(|_| ()),
        }
    }
}

/// Decoder for a specific block within a 7z archive.
///
/// Provides access to entries within a single compression block and allows
/// decoding files from that block.
pub struct BlockDecoder<'a, R: Read + Seek> {
    thread_count: u32,
    adaptive_lzma2: bool,
    input_complete: bool,
    positional: Option<&'a Arc<dyn ReadAt>>,
    verify_checksums: bool,
    lzma2: Arc<Lzma2Control>,
    block_index: usize,
    archive: &'a Archive,
    password: &'a Password,
    source: &'a mut R,
    limits: ArchiveLimits,
    #[allow(clippy::type_complexity)]
    on_sub_stream_complete: Option<&'a mut (dyn FnMut(SubStreamCompletion) + Send + 'a)>,
}

impl<'a, R: Read + Seek> BlockDecoder<'a, R> {
    /// Creates a new [`BlockDecoder`] for decoding a specific block in the archive.
    ///
    /// # Arguments
    /// * `thread_count` - Number of threads to use for multi-threaded decompression (if supported
    ///   by the codec)
    /// * `block_index` - Index of the block to decode within the archive
    /// * `archive` - Reference to the archive containing the block
    /// * `password` - Password for encrypted blocks
    /// * `source` - Mutable reference to the reader providing archive data
    pub fn new(
        thread_count: u32,
        block_index: usize,
        archive: &'a Archive,
        password: &'a Password,
        source: &'a mut R,
    ) -> Self {
        Self::with_limits(
            thread_count,
            block_index,
            archive,
            password,
            source,
            ArchiveLimits::default(),
        )
    }

    /// Same as [`BlockDecoder::new`], with a bound on what the block's coders
    /// may allocate.
    ///
    /// The header has already been parsed by the time a caller holds a
    /// [`BlockDecoder`], so only the decoder-memory half of [`ArchiveLimits`]
    /// applies here; it bounds the block's coder chain, summed, before any of
    /// it is built ([`Error::MaxMemLimited`], located in the block).
    pub fn with_limits(
        thread_count: u32,
        block_index: usize,
        archive: &'a Archive,
        password: &'a Password,
        source: &'a mut R,
        limits: ArchiveLimits,
    ) -> Self {
        let thread_count = thread_count.clamp(1, 256);
        Self {
            thread_count,
            adaptive_lzma2: false,
            input_complete: false,
            positional: None,
            verify_checksums: true,
            lzma2: Arc::new(Lzma2Control::new(thread_count)),
            block_index,
            archive,
            password,
            source,
            limits,
            on_sub_stream_complete: None,
        }
    }

    /// Allows complete-input framing inspection, as
    /// [`ArchiveReader::set_input_complete`]. Defaults to false.
    pub fn set_input_complete(&mut self, complete: bool) {
        self.input_complete = complete;
    }

    /// Sets how many threads this block's LZMA2 coder may decode on, clamped
    /// to `1..=256`. See [`ArchiveReader::set_threads`].
    pub fn set_threads(&mut self, threads: u32) {
        self.thread_count = threads.clamp(1, 256);
        self.lzma2.set_threads(self.thread_count);
    }

    /// [`BlockDecoder::set_threads`], for a builder chain.
    #[must_use]
    pub fn with_threads(mut self, threads: u32) -> Self {
        self.set_threads(threads);
        self
    }

    /// Upstream's name for [`BlockDecoder::set_threads`].
    pub fn set_thread_count(&mut self, thread_count: u32) {
        self.set_threads(thread_count);
    }

    /// Builds this block's LZMA2 coder so that it can widen later even while
    /// the thread count is one. See [`ArchiveReader::set_adaptive_lzma2`].
    #[must_use]
    pub fn with_adaptive_lzma2(mut self) -> Self {
        self.adaptive_lzma2 = true;
        self
    }

    /// Whether this block's checksums are checked. See
    /// [`ArchiveReader::set_verify_checksums`].
    #[must_use]
    pub fn with_verify_checksums(mut self, verify: bool) -> Self {
        self.verify_checksums = verify;
        self
    }

    /// A handle on this block's LZMA2 coder, which can be held and used while
    /// the decode is running.
    pub fn lzma2_handle(&self) -> Lzma2Handle {
        Lzma2Handle {
            control: Arc::clone(&self.lzma2),
        }
    }

    /// Returns a slice of archive entries contained in this block.
    ///
    /// The entries are returned in the order they appear in the block.
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.archive.files[block_file_range(self.archive, self.block_index)]
    }

    /// Returns the number of entries contained in this block.
    pub fn entry_count(&self) -> usize {
        self.entries().len()
    }

    /// Takes a closure to decode each files in this block.
    ///
    /// When decoding files in a block, the data to be decompressed depends on the data in front of
    /// it, you cannot simply skip the previous data and only decompress the data in the back.
    ///
    /// Non-solid archives use one block per file and allow more effective decoding of single files.
    pub fn for_each_entries<F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>>(
        self,
        each: &mut F,
    ) -> Result<bool, Error> {
        self.for_each_entries_direct(&mut |entry, reader| each(entry, reader))
    }

    /// As [`BlockDecoder::for_each_entries`], handing each entry over as an
    /// [`EntryRead`], which can also write itself on with
    /// [`EntryRead::write_rest`] straight from the decoder's output.
    pub fn for_each_entries_direct<F>(self, each: &mut F) -> Result<bool, Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn EntryRead) -> Result<bool, Error>,
    {
        self.walk_entries(each).map(|outcome| outcome.finished)
    }

    /// [`BlockDecoder::for_each_entries_direct`], saying also whether every
    /// byte of the block went through the callbacks' readers, which is what
    /// the block hook needs to know before it calls the block checked.
    fn walk_entries<F>(self, each: &mut F) -> Result<BlockOutcome, Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn EntryRead) -> Result<bool, Error>,
    {
        // `Archive` is public, and so is this constructor: a block the archive
        // does not have is an error, not a panic.
        check_block_mapping(self.archive, self.block_index)?;
        let Self {
            thread_count,
            adaptive_lzma2,
            input_complete,
            positional,
            verify_checksums,
            lzma2,
            block_index,
            archive,
            password,
            source,
            limits,
            mut on_sub_stream_complete,
        } = self;
        // Where each of this block's files starts in its decoded stream. Handed
        // to the LZMA2 coder so that, if it decodes in parallel, each worker
        // checksums the pieces of its own output between those points, and no
        // CRC-32 is ever computed on the thread delivering the bytes.
        let splits = if verify_checksums {
            Self::file_boundaries(archive, block_index)
        } else {
            Vec::new()
        };
        let opts = DecodeOptions {
            limits: &limits,
            threads: thread_count,
            adaptive_lzma2,
            input_complete,
            positional,
            verify_checksums,
            lzma2_control: Some(&lzma2),
            single_run_lzma2: false,
            checksum_splits: &splits,
            // Every file below is checked against its CRC as it is read, on
            // this thread or folded from the workers'.
            files_verified: verify_checksums,
            unsized_kb: 0,
            reserved_kb: 0,
            encrypted: archive.blocks[block_index].is_encrypted(),
        };
        lzma2.set_block_index(block_index);
        // Where this block's packed bytes start, so a failure below can say
        // which region of the file it was reading.
        let packed_offset = archive
            .block_pack_streams(block_index)
            .first()
            .map_or(0, |range| range.offset);
        let (block_reader, _size) =
            ArchiveReader::build_decode_stack(source, archive, block_index, password, &opts)
                .map_err(|error| error.in_block(block_index, packed_offset))?;
        // Read once, here: the coder is built and has either engaged the
        // parallel path or degraded to the single-threaded one, and it stops
        // reporting as soon as the block finishes.
        let folding = opts.folding_checksums();
        let faulted = Rc::new(Cell::new(false));
        let mut block_reader = FaultRecordingReader {
            inner: block_reader,
            faulted: Rc::clone(&faulted),
        };
        let files = block_file_range(archive, block_index);
        // The last file of the block with bytes in it. A callback that asks to
        // go on having read only part of a file before it leaves the rest in
        // the stream, and the next file would begin inside it: the rest is
        // read for it, through the reader it was handed, so that the file's
        // checksum still sees every byte. After the last such file there is
        // nothing to line up, and nothing is decoded that nobody asked for.
        let last_sized = files.clone().rev().find(|&index| {
            let file = &archive.files[index];
            file.has_stream && file.size > 0
        });

        let first_sub_stream = archive
            .stream_map
            .block_first_sub_stream_index
            .get(block_index)
            .ok_or_else(|| Error::corrupted("block has no sub-stream mapping"))?;
        // Where each file starts inside the block's uncompressed stream: the
        // coordinate the parallel decoder's output blocks carry, so a folded
        // checksum and a file are in the same space.
        let mut unpacked_offset = 0u64;
        let mut sub_stream = 0usize;
        let crc_report = Rc::new(Cell::new(None));
        let unpack_size = archive.blocks[block_index].get_unpack_size();
        // How much of the block's decoded stream the callbacks have read, and
        // whether the read that sees its end has been made.
        let mut consumed = 0u64;
        let mut stream_ended = false;
        let mut owed = OwedCompletions {
            archive,
            lzma2: &lzma2,
            block_index,
            packed_offset,
            queue: VecDeque::new(),
        };
        let mut finished = true;

        for file_index in files {
            let file = &archive.files[file_index];
            if file.has_stream && file.size > 0 {
                let bound = file.size as usize;
                let drain = last_sized.is_some_and(|last| file_index < last);
                let mut bounded = BoundedReader::new(&mut block_reader, bound);
                let outcome = if file.has_crc && verify_checksums && !folding {
                    crc_report.set(None);
                    // A file that does not match its CRC is the block's
                    // fault, although the check sits above the recording
                    // reader: it is recorded too, so the failure leaves this
                    // block located and typed as the folded check's does.
                    let mut verifying = FaultRecordingReader {
                        inner: Crc32VerifyingReader::reporting(
                            &mut bounded,
                            bound,
                            file.crc,
                            Rc::clone(&crc_report),
                        ),
                        faulted: Rc::clone(&faulted),
                    };
                    each(file, &mut verifying)
                        .and_then(|go_on| drain_unread(&mut verifying, go_on, drain))
                } else {
                    // The bounded reader fails a file the block's stream ends
                    // inside of, and that is the block's fault as well.
                    let mut recording = FaultRecordingReader {
                        inner: &mut bounded,
                        faulted: Rc::clone(&faulted),
                    };
                    each(file, &mut recording)
                        .and_then(|go_on| drain_unread(&mut recording, go_on, drain))
                };
                // What the callback took of the file, and whether that was all
                // of it.
                let taken = (bound - bounded.remain) as u64;
                let read_to_end = taken == file.size;
                let outcome = outcome
                    .map_err(|e| e.maybe_bad_password(opts.encrypted))
                    .map_err(|e| {
                        // Only a failure that came out of the decode chain is
                        // this block's fault; the caller's own errors pass
                        // through as they were raised.
                        if faulted.get() {
                            e.in_block(block_index, packed_offset)
                        } else {
                            e
                        }
                    })?;
                consumed += taken;
                if folding {
                    // The files before this one first, so completions are
                    // heard in file order.
                    owed.settle(false, on_sub_stream_complete.as_deref_mut())?;
                    // The workers checksummed this file's bytes as they
                    // produced them; folding the pieces is a few multiplies
                    // over GF(2), and reads none of them back.
                    let check = lzma2.folded(unpacked_offset, file.size);
                    if let (Some(crc32), true) = (check, owed.queue.is_empty()) {
                        if file.has_crc && u64::from(crc32) != file.crc {
                            return Err(folded_mismatch(opts.encrypted)
                                .in_block(block_index, packed_offset));
                        }
                        crc_report.set(Some(crc32));
                    } else if check.is_some() || (read_to_end && file.has_crc) {
                        // Every byte was handed over and the coder has not
                        // reported their checksum yet, or a file before this
                        // one is still waiting for its own. It is compared
                        // when the checksum arrives, at the end of the stream
                        // at the latest, and never passed as checked before.
                        owed.queue.push_back(OwedCompletion {
                            file_index,
                            sub_stream_index: first_sub_stream + sub_stream,
                            unpacked_offset,
                            len: file.size,
                        });
                    }
                    // Otherwise there is nothing to compare: the file has no
                    // CRC, or the callback left bytes unread and nothing
                    // covers the range, which is not a checksum failure and is
                    // not reported as one.
                }
                if let (Some(hook), Some(crc32)) =
                    (on_sub_stream_complete.as_deref_mut(), crc_report.take())
                {
                    hook(SubStreamCompletion {
                        block_index,
                        sub_stream_index: first_sub_stream + sub_stream,
                        file_index,
                        unpacked_offset,
                        len: file.size,
                        crc32,
                    });
                }
                unpacked_offset += file.size;
                sub_stream += 1;
                if !owed.queue.is_empty() && consumed == unpack_size {
                    // The last byte of the stream is out. The coder publishes
                    // what it still holds when it reaches the end of its
                    // input, which is one read away: made here, so a folder of
                    // one file hears its verdict where the sequential path
                    // gives it, before any file after it is announced.
                    read_to_stream_end(&mut block_reader, 0)
                        .map_err(|e| Error::from(e).in_block(block_index, packed_offset))?;
                    stream_ended = true;
                    owed.settle(true, on_sub_stream_complete.as_deref_mut())?;
                }
                if !outcome {
                    finished = false;
                    break;
                }
            } else {
                if file.has_stream {
                    sub_stream += 1;
                }
                // C/7zArcIn.c compares a zero-length file's CRC too, against
                // the CRC of no bytes, which is zero.
                if empty_file_crc_mismatch(file, verify_checksums) {
                    return Err(
                        Error::ChecksumVerificationFailed.in_block(block_index, packed_offset)
                    );
                }
                let empty_reader: &mut dyn EntryRead = &mut ([0u8; 0].as_slice());
                if !each(file, empty_reader)? {
                    finished = false;
                    break;
                }
            }
        }
        if !owed.queue.is_empty() {
            // A file was read to its end and the callbacks stopped short of
            // the stream's: the rest is decoded here, because its checksum is
            // not known until the coder gets there, and a file handed over in
            // full is compared with its CRC before this returns.
            read_to_stream_end(&mut block_reader, unpack_size.saturating_sub(consumed))
                .map_err(|e| Error::from(e).in_block(block_index, packed_offset))?;
            stream_ended = true;
            owed.settle(true, on_sub_stream_complete)?;
        }
        // Every byte of the block went to the callbacks, so its own checksum is
        // owed whether or not the last of them then asked to stop: the
        // sequential path compares it on the read that hands the last byte
        // over, before the callback can answer.
        let read_in_full = unpack_size != 0 && consumed == unpack_size;
        let outcome = BlockOutcome {
            finished,
            read_in_full: consumed == unpack_size,
        };
        if !finished && !read_in_full {
            return Ok(outcome);
        }
        if folding && archive.blocks[block_index].has_crc {
            // The block's own checksum, folded from the same segments: the
            // stream was not wrapped in a verifying reader, because that
            // reader runs on the thread delivering the bytes.
            let mut check = lzma2.folded(0, unpack_size);
            if check.is_none() && read_in_full && !stream_ended {
                read_to_stream_end(&mut block_reader, 0)
                    .map_err(|e| Error::from(e).in_block(block_index, packed_offset))?;
                check = lzma2.folded(0, unpack_size);
            }
            match check {
                Some(crc32) if u64::from(crc32) != archive.blocks[block_index].crc => {
                    return Err(
                        folded_mismatch(opts.encrypted).in_block(block_index, packed_offset)
                    );
                }
                Some(_) => {}
                // Every byte was read and nothing answers for them.
                None if read_in_full => return Err(owed.unverified()),
                // The callbacks left bytes unread, as above.
                None => {}
            }
        }
        Ok(outcome)
    }

    /// The offsets this block's files start at in its decoded stream, without
    /// the leading zero: the split points a parallel LZMA2 coder checksums
    /// between.
    ///
    /// Empty when there is nothing to gain — a block whose only checksum is
    /// its own needs no interior boundary, and one with no checksums at all
    /// needs none either.
    fn file_boundaries(archive: &Archive, block_index: usize) -> Vec<u64> {
        let block = &archive.blocks[block_index];
        // The workers checksum what the LZMA2 coder produces. That is the
        // block's output only when LZMA2 *is* the block's coder: a filter
        // above it — BCJ, delta, BCJ2 — rewrites those bytes on the way out,
        // and a checksum taken underneath it would be of bytes nobody ever
        // sees. Such a block keeps the streaming checksum on the consuming
        // thread, which is where it has to happen, because the filter runs
        // there too.
        //
        // A stored folder's bytes are the files' too, whatever its coders —
        // COPY, AES, or both — and its reader thread checksums them the same
        // way when it has one: see `store_overlap`.
        if block.coders.len() != 1 && stored_chain(block).is_none() {
            return Vec::new();
        }
        let files = &archive.files[block_file_range(archive, block_index)];
        let mut offsets = Vec::new();
        let mut at = 0u64;
        let mut wanted = block.has_crc;
        for file in files {
            if file.has_stream && file.size > 0 {
                wanted |= file.has_crc;
                if at != 0 {
                    offsets.push(at);
                }
                at += file.size;
            }
        }
        if !wanted {
            // Nothing in this block is checked, so there is nothing to compute
            // and no reason to make the workers do it.
            return Vec::new();
        }
        if offsets.is_empty() {
            // One stream: its range is the whole block, which a worker
            // checksums without being given any interior point. The plan still
            // has to be asked for, and an offset past the end of the stream is
            // how it is asked for with no point inside it.
            return vec![u64::MAX];
        }
        offsets
    }
}

/// The largest folder, unpacked, that is decoded on a worker and staged for
/// the callback rather than decoded alone on the calling thread.
///
/// Staging is what lets folders decode out of order and still reach the caller
/// in order, and it is bounded by this per folder. A larger folder is decoded
/// alone, with the reader's whole thread count for its own LZMA2 coder, which
/// is where a large folder's parallelism is.
pub(crate) const FOLDER_STAGE_BYTES: u64 = 8 << 20;

/// A folded checksum that did not match, as the serial path reports it: a
/// plain [`Error::ChecksumVerificationFailed`], or for a folder that decrypts
/// (only a stored one folds), the verifying reader's error as a possible
/// wrong password.
fn folded_mismatch(encrypted: bool) -> Error {
    if encrypted {
        Error::from(io::Error::other(Error::ChecksumVerificationFailed)).maybe_bad_password(true)
    } else {
        Error::ChecksumVerificationFailed
    }
}

/// Whether `block` is stored data: one COPY coder, one AES coder, or the
/// two of them in a chain, one pack stream in. `Some(None)` for COPY alone,
/// `Some(Some(index))` naming the AES coder otherwise.
fn stored_chain(block: &Block) -> Option<Option<usize>> {
    if block.coders.is_empty()
        || block.coders.len() > 2
        || block.packed_streams().len() != 1
        || block.total_input_streams() != block.total_output_streams()
    {
        return None;
    }
    let mut aes = None;
    let mut copies = 0;
    for (index, coder) in block.coders.iter().enumerate() {
        if coder.num_in_streams() != 1 || coder.num_out_streams() != 1 {
            return None;
        }
        match coder.encoder_method_id() {
            EncoderMethod::ID_COPY => copies += 1,
            EncoderMethod::ID_AES256_SHA256 if aes.is_none() => aes = Some(index),
            _ => return None,
        }
    }
    (copies <= 1).then_some(aes)
}

/// How many folders each worker may have decoded ahead of the callback: one
/// being decoded and one waiting, so a worker that finishes a folder while the
/// callback is still on an earlier one starts the next instead of idling.
pub(crate) const FOLDER_WINDOW_PER_WORKER: usize = 2;

/// How much of a member a folder worker reads at once and hands over.
const FOLDER_CHUNK_BYTES: usize = 1 << 20;

/// What a folder worker holds beyond its folder's coders: their input buffer,
/// the stages of the folders it may be ahead by, and the piece it is filling
/// for the stage, which it holds on to while the stage has no room for it.
const FOLDER_WORKER_BYTES: u64 = crate::container::BLOCK_INPUT_BYTES
    + FOLDER_STAGE_BYTES * FOLDER_WINDOW_PER_WORKER as u64
    + FOLDER_CHUNK_BYTES as u64;

/// What a message costs in the stage beyond its bytes, so that a folder of
/// many empty members still fills its stage and waits.
#[cfg(not(target_arch = "wasm32"))]
const FOLDER_MESSAGE_WEIGHT: usize = 64;

/// One step of [`ArchiveReader::for_each_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FolderPhase {
    /// One folder, on the calling thread, with the reader's whole thread count
    /// for its own coder.
    Alone { block: usize },
    /// Consecutive small folders, decoded `workers` at a time, each with
    /// `threads_per_folder` threads for its own coder. `workers *
    /// threads_per_folder` never exceeds the reader's thread count.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Parallel {
        blocks: std::ops::Range<usize>,
        workers: u32,
        threads_per_folder: u32,
    },
}

/// Plans the folders of an archive, given each folder's unpacked size and
/// decoder memory estimate (`u64::MAX` when it has none), for a thread budget
/// of `threads` and a memory limit of `memory_limit_bytes`.
///
/// A folder of at most [`FOLDER_STAGE_BYTES`] can be staged; a run of at
/// least two of them is decoded in parallel, by as many workers as the budget,
/// the run's length and the memory limit allow. Every other folder is decoded
/// alone. With no memory limit, a run that has fewer folders than threads
/// gives each folder an equal share of the rest; with one, each folder decodes
/// on one thread, because a parallel LZMA2 coder sizes what it holds from the
/// limit and several of them would each take the whole of it.
///
/// Under a memory limit a worker is charged the largest decoder of its run and
/// [`FOLDER_WORKER_BYTES`], and the calling thread the piece its callback is
/// reading, so everything live at once in a parallel run is inside the limit.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn plan_folders(
    folders: impl Iterator<Item = (u64, u64)>,
    threads: u32,
    memory_limit_bytes: u64,
) -> Vec<FolderPhase> {
    let threads = threads.max(1);
    let mut phases = Vec::new();
    // The open run: where it starts, and the largest decoder in it.
    let mut run: Option<(usize, u64)> = None;
    let close = |phases: &mut Vec<FolderPhase>, start: usize, end: usize, largest: u64| {
        let len = end - start;
        let by_memory = if memory_limit_bytes == u64::MAX {
            u64::MAX
        } else {
            // The piece the callback is reading has left its stage, and is
            // the calling thread's whatever the number of workers.
            let shared = memory_limit_bytes.saturating_sub(FOLDER_CHUNK_BYTES as u64);
            shared / largest.saturating_add(FOLDER_WORKER_BYTES)
        };
        let workers = u64::from(threads).min(len as u64).min(by_memory).max(1) as u32;
        if len < 2 || workers < 2 {
            phases.extend((start..end).map(|block| FolderPhase::Alone { block }));
            return;
        }
        let threads_per_folder = if memory_limit_bytes == u64::MAX {
            (threads / workers).max(1)
        } else {
            1
        };
        phases.push(FolderPhase::Parallel {
            blocks: start..end,
            workers,
            threads_per_folder,
        });
    };
    let mut count = 0;
    for (index, (unpacked, estimate)) in folders.enumerate() {
        count = index + 1;
        if unpacked <= FOLDER_STAGE_BYTES {
            let largest = run.map_or(estimate, |(_, largest)| largest.max(estimate));
            run = Some((run.map_or(index, |(start, _)| start), largest));
            continue;
        }
        if let Some((start, largest)) = run.take() {
            close(&mut phases, start, index, largest);
        }
        phases.push(FolderPhase::Alone { block: index });
    }
    if let Some((start, largest)) = run {
        close(&mut phases, start, count, largest);
    }
    phases
}

/// What a folder worker hands the calling thread, in order.
#[cfg(not(target_arch = "wasm32"))]
enum FolderMessage {
    /// The callback is about to be given this file.
    Begin(usize),
    /// The next bytes of the file begun last.
    Data(Vec<u8>),
    /// The file begun last ended cleanly.
    End,
    /// Reading the file begun last failed here, after the bytes before it.
    Fault(io::Error),
    /// A file's checksum became final, for the sub-stream hook.
    Completion(SubStreamCompletion),
    /// The folder is over.
    Done(FolderEnd),
}

/// How a folder worker's decode ended.
#[cfg(not(target_arch = "wasm32"))]
enum FolderEnd {
    /// Every file was decoded and checked.
    Finished,
    /// A file failed partway, and its [`FolderMessage::Fault`] says how.
    Stopped,
    /// The folder failed outside any file: its coders could not be built, or
    /// its own checksum did not match. Already located.
    Failed(Error),
}

/// What every folder worker of one run shares.
#[cfg(not(target_arch = "wasm32"))]
struct FolderWorker<'a> {
    archive: &'a Archive,
    password: &'a Password,
    limits: ArchiveLimits,
    verify_checksums: bool,
    /// The threads the plan gave each folder's own coder.
    threads: u32,
    /// The reader's live thread ceiling, which [`Lzma2Handle::set_threads`]
    /// moves while the run goes on.
    ceiling: &'a Lzma2Control,
    source: &'a Arc<dyn ReadAt>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FolderWorker<'_> {
    /// How many folders may be decoding at once under the live ceiling: as
    /// many as it has threads for, at the threads each folder was planned.
    /// Asked as each folder is about to start, so a ceiling moved while the
    /// run goes on is followed from the next folder; the plan's own worker
    /// count, which the memory limit had a say in, bounds it from above.
    fn folders_at_once(&self) -> usize {
        (self.ceiling.threads() / self.threads.max(1)).max(1) as usize
    }

    /// Decodes one folder through the ordinary [`BlockDecoder`] over a cursor
    /// of its own, sending what the callback would have been given.
    fn decode(&self, block_index: usize, tx: &mut crate::ordered::Sender<'_, FolderMessage>) {
        let mut cursor = ReadAtCursor::new(Arc::clone(self.source));
        let completions = std::sync::Mutex::new(Vec::new());
        let mut hook = |completion: SubStreamCompletion| {
            if let Ok(mut pending) = completions.lock() {
                pending.push(completion);
            }
        };
        let mut decoder = BlockDecoder::with_limits(
            // A ceiling below the plan's share narrows this folder's own
            // coder too: one folder at a time, on no more threads than that.
            self.threads.min(self.ceiling.threads()),
            block_index,
            self.archive,
            self.password,
            &mut cursor,
            self.limits,
        )
        .with_verify_checksums(self.verify_checksums);
        decoder.on_sub_stream_complete = Some(&mut hook);

        let mut next_file = block_file_range(self.archive, block_index).start;
        let mut stopped = false;
        let mut cancelled = false;
        // The hook fires after the callback returns for a file, so what it
        // left is sent before the next file begins: the order the caller's
        // own hook would have seen.
        let flush = |tx: &mut crate::ordered::Sender<'_, FolderMessage>| {
            let pending = std::mem::take(&mut *completions.lock().map_err(|_| ())?);
            for completion in pending {
                tx.send(FolderMessage::Completion(completion), FOLDER_MESSAGE_WEIGHT)
                    .map_err(|_| ())?;
            }
            Ok::<(), ()>(())
        };
        let result = decoder.for_each_entries(&mut |entry, rd| {
            let file_index = next_file;
            next_file += 1;
            let send = |tx: &mut crate::ordered::Sender<'_, FolderMessage>, message, weight| {
                tx.send(message, weight).map_err(|_| Error::Cancelled)
            };
            if flush(tx).is_err() {
                cancelled = true;
                return Err(Error::Cancelled);
            }
            send(tx, FolderMessage::Begin(file_index), FOLDER_MESSAGE_WEIGHT)
                .inspect_err(|_| cancelled = true)?;
            let mut left = if entry.has_stream { entry.size } else { 0 };
            while left > 0 {
                let mut chunk = vec![0u8; left.min(FOLDER_CHUNK_BYTES as u64) as usize];
                let mut filled = 0;
                let mut fault = None;
                while filled < chunk.len() {
                    match rd.read(&mut chunk[filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            fault = Some(e);
                            break;
                        }
                    }
                }
                chunk.truncate(filled);
                left -= filled as u64;
                if !chunk.is_empty() {
                    let weight = chunk.len() + FOLDER_MESSAGE_WEIGHT;
                    send(tx, FolderMessage::Data(chunk), weight)
                        .inspect_err(|_| cancelled = true)?;
                }
                if let Some(e) = fault {
                    stopped = true;
                    send(tx, FolderMessage::Fault(e), FOLDER_MESSAGE_WEIGHT)
                        .inspect_err(|_| cancelled = true)?;
                    return Err(Error::internal("folder decode stopped"));
                }
                if filled == 0 {
                    // The stream ended early: the callback would have seen the
                    // end here too.
                    break;
                }
            }
            send(tx, FolderMessage::End, FOLDER_MESSAGE_WEIGHT)
                .inspect_err(|_| cancelled = true)?;
            Ok(true)
        });
        if cancelled || flush(tx).is_err() {
            return;
        }
        let end = match result {
            _ if stopped => FolderEnd::Stopped,
            Ok(_) => FolderEnd::Finished,
            Err(error) => FolderEnd::Failed(error),
        };
        let _ = tx.send(FolderMessage::Done(end), FOLDER_MESSAGE_WEIGHT);
    }
}

/// Hands one folder a worker decoded to `each`, in order, as
/// [`BlockDecoder::for_each_entries`] would have: the same files, the same
/// bytes, the same errors in the same places, and the sub-stream hook after
/// each file. Returns whether the callback let the folder run to its end.
#[cfg(not(target_arch = "wasm32"))]
fn replay_folder<F>(
    archive: &Archive,
    encrypted: bool,
    verify_checksums: bool,
    block_index: usize,
    rx: &mut crate::ordered::Receiver<'_, FolderMessage>,
    each: &mut F,
    mut on_sub_stream_complete: Option<&mut (dyn FnMut(SubStreamCompletion) + Send)>,
) -> Result<BlockOutcome, Error>
where
    F: FnMut(&ArchiveEntry, &mut dyn EntryRead) -> Result<bool, Error>,
{
    let packed_offset = archive
        .block_pack_streams(block_index)
        .first()
        .map_or(0, |range| range.offset);
    let lost = || {
        Error::internal("the worker decoding this folder stopped before finishing it")
            .in_block(block_index, packed_offset)
    };
    // A file's failure the callback was handed and did not return, kept so
    // that it is still the folder's error.
    let mut swallowed: Option<(io::ErrorKind, String)> = None;
    // How much of the folder the callbacks have been handed. Once that is all
    // of it the folder's own CRC is owed too, whether or not the callback
    // handed the last byte then asked to stop.
    let block = &archive.blocks[block_index];
    let unpack_size = block.get_unpack_size();
    let mut delivered = 0u64;
    let folder_owed = |delivered: u64| {
        verify_checksums && block.has_crc && unpack_size != 0 && delivered == unpack_size
    };
    let ended = |finished: bool, delivered: u64| BlockOutcome {
        finished,
        read_in_full: delivered == unpack_size,
    };
    // As on the sequential path: the last file of the folder with bytes in it,
    // before which a file the callback went on from is read to its end for it.
    let last_sized = block_file_range(archive, block_index)
        .rev()
        .find(|&index| archive.files[index].has_stream && archive.files[index].size > 0);
    loop {
        match rx.recv() {
            None => return Err(lost()),
            Some(FolderMessage::Begin(file_index)) => {
                let file = &archive.files[file_index];
                // Set once a callback has asked to stop: whether the file it
                // stopped at is owed a verdict of its own.
                let owed_file;
                if !(file.has_stream && file.size > 0) {
                    // An empty file has no bytes to fail on: its errors are the
                    // caller's own, passed through.
                    match rx.recv() {
                        Some(FolderMessage::End) => {}
                        _ => return Err(lost()),
                    }
                    // The worker compares it before it sends the file, and
                    // so does this, as the sequential path does.
                    if empty_file_crc_mismatch(file, verify_checksums) {
                        return Err(
                            Error::ChecksumVerificationFailed.in_block(block_index, packed_offset)
                        );
                    }
                    let empty_reader: &mut dyn EntryRead = &mut ([0u8; 0].as_slice());
                    if each(file, empty_reader)? {
                        continue;
                    }
                    if !folder_owed(delivered) {
                        return Ok(ended(false, delivered));
                    }
                    owed_file = false;
                } else {
                    let mut entry = ReplayEntry {
                        rx: &mut *rx,
                        chunk: Vec::new(),
                        pos: 0,
                        delivered: 0,
                        state: ReplayState::Open,
                        fault: None,
                    };
                    let outcome = each(file, &mut entry)
                        .map_err(|e| e.maybe_bad_password(encrypted))
                        .map_err(|e| {
                            // As on the sequential path: only a failure that
                            // came out of the decode is this folder's fault.
                            if entry.state.faulted() {
                                e.in_block(block_index, packed_offset)
                            } else {
                                e
                            }
                        })?;
                    let unseen = entry.finish();
                    // The sequential path reads the rest of a file the
                    // callback went on from, when a file with bytes follows
                    // it in the folder, so it counts as handed over there;
                    // the worker read it here.
                    let lined_up = outcome
                        && entry.state == ReplayState::Ended
                        && last_sized.is_some_and(|last| file_index < last);
                    delivered += if lined_up { file.size } else { entry.delivered };
                    if outcome {
                        if let Some(error) = unseen {
                            // The worker read the whole file; the callback
                            // stopped short of where it failed. The member is
                            // damaged all the same.
                            return Err(Error::from(error)
                                .maybe_bad_password(encrypted)
                                .in_block(block_index, packed_offset));
                        }
                        if let Some(fault) = entry.fault.take() {
                            swallowed = Some(fault);
                        }
                        continue;
                    }
                    if entry.state != ReplayState::Ended {
                        return Ok(ended(false, delivered));
                    }
                    owed_file = verify_checksums && file.has_crc && entry.delivered == file.size;
                }
                // The worker read the file to its end whatever the callback
                // did, and its completion, when it has one, is the worker's
                // next message unless the folder's coder could not report the
                // checksum before the end of its stream. The hook hears it
                // before the callback's `false` is honoured, as on the
                // sequential path.
                //
                // A file with a CRC that the callback read to its end is not
                // left unchecked because the callback then stopped: its
                // verdict is waited for, past whatever the worker sends about
                // the files after it. So is the folder's, which the worker
                // gives last, when the callbacks were handed every byte of it.
                let owed_folder = folder_owed(delivered);
                let owed = owed_file || owed_folder;
                loop {
                    match rx.recv() {
                        Some(FolderMessage::Completion(completion)) => {
                            let verdict = completion.file_index == file_index;
                            if let Some(hook) = on_sub_stream_complete.as_deref_mut() {
                                hook(completion);
                            }
                            if !owed_folder && (verdict || !owed_file) {
                                return Ok(ended(false, delivered));
                            }
                        }
                        Some(
                            FolderMessage::Begin(_) | FolderMessage::Data(_) | FolderMessage::End,
                        ) if owed => {}
                        Some(FolderMessage::Done(FolderEnd::Failed(error))) if owed => {
                            return Err(error);
                        }
                        // A file after this one failed, and the folder's
                        // decode stopped there with this one unchecked.
                        Some(FolderMessage::Fault(error)) if owed => {
                            return Err(Error::from(error)
                                .maybe_bad_password(encrypted)
                                .in_block(block_index, packed_offset));
                        }
                        Some(FolderMessage::Done(FolderEnd::Stopped)) | None if owed => {
                            return Err(lost());
                        }
                        _ => return Ok(ended(false, delivered)),
                    }
                }
            }
            Some(FolderMessage::Completion(completion)) => {
                if let Some(hook) = on_sub_stream_complete.as_deref_mut() {
                    hook(completion);
                }
            }
            Some(FolderMessage::Done(FolderEnd::Finished)) => return Ok(ended(true, delivered)),
            Some(FolderMessage::Done(FolderEnd::Failed(error))) => return Err(error),
            Some(FolderMessage::Done(FolderEnd::Stopped)) => {
                let (kind, message) = swallowed
                    .take()
                    .unwrap_or((io::ErrorKind::Other, "folder decode stopped".to_string()));
                return Err(Error::from(io::Error::new(kind, message))
                    .maybe_bad_password(encrypted)
                    .in_block(block_index, packed_offset));
            }
            Some(_) => return Err(lost()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayState {
    Open,
    Ended,
    Faulted,
    Lost,
}

#[cfg(not(target_arch = "wasm32"))]
impl ReplayState {
    fn faulted(self) -> bool {
        matches!(self, Self::Faulted | Self::Lost)
    }
}

/// The `Read` the callback is handed for one file a worker decoded.
#[cfg(not(target_arch = "wasm32"))]
struct ReplayEntry<'r, 'a> {
    rx: &'r mut crate::ordered::Receiver<'a, FolderMessage>,
    chunk: Vec<u8>,
    pos: usize,
    /// How much of the file the callback has been handed.
    delivered: u64,
    state: ReplayState,
    /// The failure the callback was handed, described, for a later read and
    /// for a callback that does not return it.
    fault: Option<(io::ErrorKind, String)>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ReplayEntry<'_, '_> {
    /// Takes the rest of the file's messages, returning a failure among them
    /// that the callback never read as far as.
    fn finish(&mut self) -> Option<io::Error> {
        if self.state != ReplayState::Open {
            return None;
        }
        loop {
            match self.rx.recv() {
                Some(FolderMessage::Data(_)) => {}
                Some(FolderMessage::End) => {
                    self.state = ReplayState::Ended;
                    return None;
                }
                Some(FolderMessage::Fault(error)) => {
                    self.state = ReplayState::Faulted;
                    return Some(error);
                }
                _ => {
                    self.state = ReplayState::Lost;
                    return Some(
                        Error::internal(
                            "the worker decoding this folder stopped before finishing it",
                        )
                        .into_io(),
                    );
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Read for ReplayEntry<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || !self.next_chunk()? {
            return Ok(0);
        }
        let n = buf.len().min(self.chunk.len() - self.pos);
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        self.delivered += n as u64;
        Ok(n)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl EntryRead for ReplayEntry<'_, '_> {
    fn write_rest(&mut self, sink: &mut dyn Write) -> io::Result<u64> {
        let mut total = 0u64;
        while self.next_chunk()? {
            let rest = &self.chunk[self.pos..];
            sink.write_all(rest)?;
            let n = rest.len();
            self.pos += n;
            self.delivered += n as u64;
            total += n as u64;
        }
        Ok(total)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ReplayEntry<'_, '_> {
    /// Makes sure there are bytes left in [`Self::chunk`], waiting for the
    /// worker's next piece if there are not. False at the end of the file.
    fn next_chunk(&mut self) -> io::Result<bool> {
        loop {
            if self.pos < self.chunk.len() {
                return Ok(true);
            }
            match self.state {
                ReplayState::Open => {}
                ReplayState::Ended => return Ok(false),
                ReplayState::Faulted | ReplayState::Lost => {
                    let (kind, message) = self
                        .fault
                        .clone()
                        .unwrap_or((io::ErrorKind::Other, "folder decode failed".to_string()));
                    return Err(io::Error::new(kind, message));
                }
            }
            match self.rx.recv() {
                Some(FolderMessage::Data(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Some(FolderMessage::End) => {
                    self.state = ReplayState::Ended;
                    return Ok(false);
                }
                Some(FolderMessage::Fault(error)) => {
                    self.state = ReplayState::Faulted;
                    self.fault = Some((error.kind(), error.to_string()));
                    return Err(error);
                }
                _ => {
                    self.state = ReplayState::Lost;
                    let error = Error::internal(
                        "the worker decoding this folder stopped before finishing it",
                    )
                    .into_io();
                    self.fault = Some((error.kind(), error.to_string()));
                    return Err(error);
                }
            }
        }
    }
}

/// Reads a decoded header of exactly `declared` bytes into a buffer that never
/// grows past it.
///
/// `declared` has already been bounded by
/// [`ArchiveLimits::max_header_unpacked_bytes`], so the buffer is reserved at
/// that size once, up front, rather than grown by doubling as `read_to_end`
/// would: doubling can leave the buffer's capacity at nearly twice the declared
/// size, which is memory the limit was meant to rule out. The reservation is
/// fallible, so a caller who lifted that limit and was handed a header claiming
/// more than the process can have is refused rather than aborted. Only the
/// pages the decoder actually writes are touched.
fn read_decoded_header(
    decoded: &mut dyn Read,
    declared: usize,
    encrypted: bool,
) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(declared).map_err(|e| {
        Error::io_msg(
            io::Error::new(io::ErrorKind::OutOfMemory, e),
            "encoded header unpack size cannot be allocated",
        )
    })?;
    // `take` stops the read at the declared size, and a buffer whose length
    // reaches its capacity is probed on the stack before `read_to_end` grows
    // it, so an exact-sized stream ends with the capacity unchanged.
    decoded
        .take(declared as u64)
        .read_to_end(&mut buf)
        .map_err(|e| Error::bad_password(e, encrypted))?;
    if buf.len() != declared {
        return Err(Error::bad_password(
            io::Error::from(io::ErrorKind::UnexpectedEof),
            encrypted,
        ));
    }
    Ok(buf)
}

#[cfg(test)]
mod decoded_header_buffer_tests {
    use super::*;

    /// Yields its bytes a few at a time, the way a decoder hands out output.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.0.len()).min(7);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn capacity_stays_at_the_declared_size() {
        // Sizes either side of the points where doubling from `read_to_end`'s
        // first reservation would have overshot.
        for declared in [1usize, 31, 32, 33, 4096, 8191, 8193, 100_000] {
            let data: Vec<u8> = (0..declared).map(|i| i as u8).collect();
            // A decoder with more to give than the header declares is cut off
            // at the declared size, and still does not grow the buffer.
            let mut longer = data.clone();
            longer.extend_from_slice(&[0xAA; 64]);
            for source in [&data, &longer] {
                let buf = read_decoded_header(&mut Trickle(source), declared, false).unwrap();
                assert_eq!(buf, data);
                assert_eq!(
                    buf.capacity(),
                    declared,
                    "the decoded header buffer must not grow past its declared size"
                );
            }
        }
    }

    #[test]
    fn a_short_decode_is_refused() {
        let err = read_decoded_header(&mut Trickle(&[1, 2, 3]), 4, false).unwrap_err();
        assert!(matches!(err, Error::Io(ref e, _) if e.kind() == io::ErrorKind::UnexpectedEof));
    }
}

#[cfg(test)]
mod count_limit_tests {
    use super::*;

    #[test]
    fn aggregate_counts_check_overflow_and_both_limits() {
        let limits = ArchiveLimits {
            max_entries: 10,
            ..ArchiveLimits::default()
        };
        let bounds = HeaderBounds::new(20, &limits);
        assert_eq!(bounds.add_count(4, 6).unwrap(), 10);
        assert_eq!(
            bounds.add_count(5, 6).unwrap_err().limit_hit(),
            Some(Limit::Entries)
        );
        assert_eq!(
            HeaderBounds::new(8, &limits)
                .add_count(4, 5)
                .unwrap_err()
                .limit_hit(),
            Some(Limit::ArchiveBytes)
        );
        assert!(
            HeaderBounds::new(usize::MAX, &ArchiveLimits::unlimited())
                .add_count(usize::MAX, 1)
                .is_err()
        );
    }
}

#[cfg(test)]
mod metadata_wire_tests {
    use super::*;

    fn parse_substreams(archive: &mut Archive, mut wire: &[u8]) -> Result<(), Error> {
        let len = wire.len();
        Archive::read_sub_streams_info(
            &mut wire,
            archive,
            HeaderBounds::new(len, &ArchiveLimits::default()),
        )
    }

    #[test]
    fn omitted_substream_crcs_inherit_only_single_stream_folders() {
        for crc in [0, 0x8000_0000u32, u32::MAX] {
            for defined in [false, true] {
                let mut archive = Archive::default();
                let mut block = Block::default();
                block.has_crc = defined;
                block.crc = u64::from(crc);
                archive.blocks.push(block);
                parse_substreams(&mut archive, &[K_END]).unwrap();
                assert_eq!(archive.sub_stream(0).unwrap().crc, defined.then_some(crc));
                archive.blocks[0].has_crc = !defined;
                archive.blocks[0].crc ^= u64::MAX;
                assert_eq!(archive.sub_stream(0).unwrap().crc, defined.then_some(crc));
            }
        }

        for explicit in [false, true] {
            let mut archive = Archive::default();
            for crc in [7u32, 0x8000_0000, 9, u32::MAX] {
                let mut block = Block::default();
                block.has_crc = true;
                block.crc = u64::from(crc);
                archive.blocks.push(block);
            }
            let mut wire = vec![K_NUM_UNPACK_STREAM, 0, 1, 2, 1, K_SIZE, 0];
            if explicit {
                // Only the two-stream folder consumes digest bits/values.
                wire.extend_from_slice(&[K_CRC, 0, 0x80]);
                wire.extend_from_slice(&0xfedc_ba98u32.to_le_bytes());
            }
            wire.push(K_END);
            parse_substreams(&mut archive, &wire).unwrap();
            let crcs: Vec<_> = (0..4).map(|i| archive.sub_stream(i).unwrap().crc).collect();
            assert_eq!(
                crcs,
                [
                    Some(0x8000_0000),
                    explicit.then_some(0xfedc_ba98),
                    None,
                    Some(u32::MAX)
                ]
            );
            assert!(archive.sub_stream(4).is_none());
        }
    }

    #[test]
    fn malformed_substream_crcs_do_not_publish_a_partial_snapshot() {
        let mut archive = Archive::default();
        let mut block = Block::default();
        block.has_crc = true;
        block.crc = 0x8000_0000;
        archive.blocks.push(block);
        parse_substreams(&mut archive, &[K_END]).unwrap();
        for wire in [vec![], vec![K_CRC], vec![K_CRC, 1], vec![K_NAME]] {
            let error = parse_substreams(&mut archive, &wire).unwrap_err();
            if wire == [K_NAME] {
                assert!(matches!(error, Error::BadTerminatedSubStreamsInfo));
            } else {
                assert!(
                    matches!(error, Error::Io(ref e, _) if e.kind() == io::ErrorKind::UnexpectedEof)
                );
            }
            assert_eq!(archive.sub_stream(0).unwrap().crc, Some(0x8000_0000));
        }
        // An explicitly required digest must still be fully present.
        archive.blocks[0].has_crc = false;
        let error = parse_substreams(&mut archive, &[K_CRC, 1, 0, 0, 0]).unwrap_err();
        assert!(matches!(error, Error::Io(ref e, _) if e.kind() == io::ErrorKind::UnexpectedEof));
        assert_eq!(archive.sub_stream(0).unwrap().crc, Some(0x8000_0000));
    }

    fn archive_with_inherited_crc(data: &[u8]) -> Vec<u8> {
        assert!(!data.is_empty() && data.len() < 120);
        // One uncompressed LZMA2 chunk, with a dictionary reset and end marker.
        let mut packed = vec![1, 0, (data.len() - 1) as u8];
        packed.extend_from_slice(data);
        packed.push(0);
        let mut header = vec![
            K_HEADER,
            K_MAIN_STREAMS_INFO,
            K_PACK_INFO,
            0,
            1,
            K_SIZE,
            packed.len() as u8,
            K_END,
            K_UNPACK_INFO,
            K_FOLDER,
            1,
            0,
            1,
            0x21,
            0x21,
            1,
            0,
            K_CODERS_UNPACK_SIZE,
            data.len() as u8,
            K_CRC,
            1,
        ];
        header.extend_from_slice(&crc32(data).to_le_bytes());
        header.extend_from_slice(&[
            K_END,
            K_SUB_STREAMS_INFO,
            K_END,
            K_END,
            K_FILES_INFO,
            1,
            K_END,
            K_END,
        ]);
        let mut start = Vec::new();
        start.extend_from_slice(&(packed.len() as u64).to_le_bytes());
        start.extend_from_slice(&(header.len() as u64).to_le_bytes());
        start.extend_from_slice(&crc32(&header).to_le_bytes());
        let mut bytes = vec![0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0, 4];
        bytes.extend_from_slice(&crc32(&start).to_le_bytes());
        bytes.extend_from_slice(&start);
        bytes.extend_from_slice(&packed);
        bytes.extend_from_slice(&header);
        bytes
    }

    #[test]
    fn inherited_crc_reaches_entry_and_positional_completion_paths() {
        use std::sync::{Mutex, atomic::AtomicBool};
        let data = b"folder checksum without a substream CRC property";
        for damaged in [false, true] {
            let mut bytes = archive_with_inherited_crc(data);
            if damaged {
                bytes[35] ^= 1;
            }
            for direct in [false, true] {
                let mut reader =
                    ArchiveReader::new(io::Cursor::new(bytes.clone()), Password::empty()).unwrap();
                assert!(reader.archive().files[0].has_crc);
                assert_eq!(reader.archive().files[0].crc, u64::from(crc32(data)));
                assert_eq!(
                    reader.archive().sub_stream(0).unwrap().crc,
                    Some(crc32(data))
                );
                let reports = Arc::new(Mutex::new(Vec::new()));
                let hook = reports.clone();
                reader.set_sub_stream_complete_hook(move |event| {
                    hook.lock().unwrap().push(event.crc32)
                });
                let mut output = Vec::new();
                let result = if direct {
                    reader.for_each_entries_direct(|_, input| {
                        input.write_rest(&mut output)?;
                        Ok(true)
                    })
                } else {
                    reader.for_each_entries(|_, input| {
                        io::copy(input, &mut output)?;
                        Ok(true)
                    })
                };
                if damaged {
                    assert!(matches!(
                        result,
                        Err(Error::BlockDecode {
                            kind: crate::BlockErrorKind::ChecksumMismatch,
                            ..
                        })
                    ));
                    assert!(reports.lock().unwrap().is_empty());
                } else {
                    result.unwrap();
                    assert_eq!(output, data);
                    assert_eq!(*reports.lock().unwrap(), [crc32(data)]);
                }
            }
            let bytes = Arc::new(bytes);
            let archive =
                Archive::read(&mut io::Cursor::new(bytes.as_slice()), &Password::empty()).unwrap();
            let folder = crate::PositionalLzma2Folder::new(
                Arc::new(archive),
                bytes.clone(),
                0,
                ArchiveLimits::default(),
            )
            .unwrap()
            .unwrap();
            let pack = folder.packed_range();
            let mut scanner = folder.scanner();
            scanner
                .feed(&bytes[pack.offset as usize..pack.end() as usize])
                .unwrap();
            let run = scanner.next_run().unwrap();
            scanner.finish().unwrap();
            let completion = run
                .decode(&AtomicBool::new(false), &mut |_, bytes: &[u8]| {
                    Ok(bytes.len())
                })
                .unwrap();
            let result = folder.verify_runs(&[completion]);
            if damaged {
                assert!(matches!(
                    result,
                    Err(Error::BlockDecode {
                        kind: crate::BlockErrorKind::ChecksumMismatch,
                        ..
                    })
                ));
            } else {
                let events = result.unwrap();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].crc32, crc32(data));
            }
        }
    }

    #[test]
    fn substream_crc_keeps_all_wire_bits_for_explicit_and_inherited_values() {
        for crc in [0x8000_0000u32, u32::MAX] {
            for inherited in [false, true] {
                let mut block = Block::default();
                block.has_crc = inherited;
                block.crc = u64::from(crc);
                let mut archive = Archive::default();
                archive.blocks.push(block);
                let mut wire = vec![K_CRC, 1];
                if !inherited {
                    wire.extend_from_slice(&crc.to_le_bytes());
                }
                wire.push(K_END);
                Archive::read_sub_streams_info(
                    &mut wire.as_slice(),
                    &mut archive,
                    HeaderBounds::new(wire.len(), &ArchiveLimits::default()),
                )
                .unwrap();
                assert_eq!(archive.sub_stream(0).unwrap().crc, Some(crc));
            }
        }
    }
}

#[cfg(all(test, feature = "compress"))]
mod block_checksum_tests {
    use std::io::Cursor;

    use super::*;
    use crate::ArchiveWriter;

    /// A one-file block whose only block checksum is the one it borrows from
    /// its file, with that borrowed copy made wrong while the file's own
    /// stays right. Which of the two a decode trips over says which check
    /// it ran.
    fn one_file_with_a_wrong_borrowed_crc() -> (Cursor<Vec<u8>>, Archive) {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
        writer
            .push_archive_entry(ArchiveEntry::new_file("one"), Some(data.as_slice()))
            .expect("push");
        let mut source = writer.finish().expect("finish");
        let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
        let block = &archive.blocks[0];
        assert!(!block.has_crc && block.num_unpack_sub_streams == 1);
        let info = archive.sub_streams_info.as_mut().expect("sub-streams");
        info.crcs[0] ^= 1;
        (source, archive)
    }

    /// `for_each_entries` checks the file itself, so the block's reader does
    /// not check the same bytes against the same CRC a second time.
    #[test]
    fn a_one_file_block_is_checked_once() {
        let (mut source, archive) = one_file_with_a_wrong_borrowed_crc();
        let password = Password::empty();
        BlockDecoder::new(1, 0, &archive, &password, &mut source)
            .for_each_entries(&mut |_, rd| {
                io::copy(rd, &mut io::sink())?;
                Ok(true)
            })
            .expect("only the file's own checksum is checked, and it is right");
    }

    /// A caller that does not check the file keeps the block's check, which
    /// is then the only one there is.
    #[test]
    fn the_block_check_stays_where_it_is_the_only_one() {
        let (mut source, archive) = one_file_with_a_wrong_borrowed_crc();
        let limits = ArchiveLimits::default();
        let opts = DecodeOptions::header(&limits);
        let (mut rd, _) = ArchiveReader::<Cursor<Vec<u8>>>::build_decode_stack(
            &mut source,
            &archive,
            0,
            &Password::empty(),
            &opts,
        )
        .expect("stack");
        let err = io::copy(&mut rd, &mut io::sink()).expect_err("the borrowed CRC is checked");
        assert!(Error::from(err).is_checksum_failure());
    }

    #[test]
    fn public_file_and_block_crc_values_are_not_narrowed() {
        let (mut source, mut archive) = one_file_with_a_wrong_borrowed_crc();
        archive.files[0].crc |= 1u64 << 32;
        let password = Password::empty();
        let err = BlockDecoder::new(1, 0, &archive, &password, &mut source)
            .for_each_entries(&mut |_, rd| {
                io::copy(rd, &mut io::sink())?;
                Ok(true)
            })
            .expect_err("public file CRC retains its high bits");
        // Entry iteration locates decode failures after the verifier has
        // compared the full public CRC value.
        assert!(matches!(
            err,
            Error::BlockDecode {
                block_index: 0,
                packed_offset,
                kind: crate::BlockErrorKind::ChecksumMismatch,
                ..
            } if packed_offset == archive.block_pack_streams(0)[0].offset
        ));

        let (mut source, mut archive) = one_file_with_a_wrong_borrowed_crc();
        archive.blocks[0].has_crc = true;
        archive.blocks[0].crc = archive.files[0].crc | (1u64 << 32);
        let limits = ArchiveLimits::default();
        let opts = DecodeOptions::header(&limits);
        let (mut rd, _) = ArchiveReader::<Cursor<Vec<u8>>>::build_decode_stack(
            &mut source,
            &archive,
            0,
            &password,
            &opts,
        )
        .expect("stack");
        let err =
            io::copy(&mut rd, &mut io::sink()).expect_err("public block CRC retains its high bits");
        assert!(Error::from(err).is_checksum_failure());
    }
}

/// The name index is built by the call that first needs it.
#[cfg(all(test, feature = "compress"))]
mod name_index_tests {
    use std::io::Cursor;

    use super::*;
    use crate::ArchiveWriter;

    fn reader(entries: &[(&str, &[u8])]) -> ArchiveReader<Cursor<Vec<u8>>> {
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
        for (name, data) in entries {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(*data))
                .expect("push");
        }
        let source = writer.finish().expect("finish");
        ArchiveReader::new(source, Password::empty()).expect("open")
    }

    /// A consumer that walks the entries never pays for the index.
    #[test]
    fn opening_and_walking_build_no_index() {
        let mut reader = reader(&[("a", b"first"), ("b", b"second")]);
        assert!(reader.index.get().is_none(), "opening built the name index");
        reader
            .for_each_entries(|_, data| {
                io::copy(data, &mut io::sink())?;
                Ok(true)
            })
            .expect("walk");
        assert!(reader.index.get().is_none(), "walking built the name index");

        assert_eq!(reader.read_file("b").expect("read"), b"second");
        assert_eq!(reader.index.get().map(HashMap::len), Some(2));
        assert!(matches!(reader.read_file("c"), Err(Error::FileNotFound)));
    }

    /// Two entries of one name: a lookup finds the later, as it did when the
    /// index was built at open.
    #[test]
    fn a_name_held_twice_finds_the_later_entry() {
        let mut reader = reader(&[("a", b"first"), ("a", b"second")]);
        assert_eq!(reader.read_file("a").expect("read"), b"second");
        let mut methods = Vec::new();
        reader
            .file_compression_methods("a", &mut methods)
            .expect("methods");
        assert!(!methods.is_empty());
    }
}

/// What a header that cannot be true costs a block that does not decrypt:
/// damage, said once, in that block.
#[cfg(all(test, feature = "compress"))]
mod header_damage_tests {
    use std::io::Cursor;

    use super::*;
    use crate::{ArchiveWriter, BlockErrorKind, EncoderConfiguration, EncoderMethod};

    fn one_file(method: EncoderMethod) -> (Cursor<Vec<u8>>, Archive) {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
        writer.set_content_methods(vec![EncoderConfiguration::new(method)]);
        writer
            .push_archive_entry(ArchiveEntry::new_file("one"), Some(data.as_slice()))
            .expect("push");
        let mut source = writer.finish().expect("finish");
        let archive = Archive::read(&mut source, &Password::empty()).expect("parse");
        (source, archive)
    }

    fn read_all(
        archive: &Archive,
        password: &Password,
        source: &mut Cursor<Vec<u8>>,
        verify: bool,
    ) -> Result<bool, Error> {
        BlockDecoder::new(1, 0, archive, password, source)
            .with_verify_checksums(verify)
            .for_each_entries(&mut |_, rd| {
                io::copy(rd, &mut io::sink())?;
                Ok(true)
            })
    }

    /// Copy has no count of its own: a block that declares more bytes than
    /// were packed used to end 64 bytes short with `Ok`.
    #[test]
    fn a_store_block_that_declares_more_than_was_packed_ends_early() {
        let (mut source, mut archive) = one_file(EncoderMethod::COPY);
        archive.blocks[0].unpack_sizes_mut()[0] += 64;
        archive.files[0].size += 64;
        archive.files[0].has_crc = false;
        for verify in [true, false] {
            match read_all(&archive, &Password::empty(), &mut source, verify) {
                Err(
                    error @ Error::BlockDecode {
                        block_index: 0,
                        kind: BlockErrorKind::Io,
                        ..
                    },
                ) => assert_eq!(
                    error.io_error().map(std::io::Error::kind),
                    Some(std::io::ErrorKind::UnexpectedEof),
                    "{error}"
                ),
                other => panic!("verify {verify}: expected an early end, got {other:?}"),
            }
        }
    }

    /// A coder that cannot be built from its properties is damage in a block
    /// that does not decrypt, whatever password the caller holds.
    #[test]
    fn coder_properties_that_cannot_be_read_are_not_a_password_error() {
        let (mut source, mut archive) = one_file(EncoderMethod::LZMA);
        // No `lc`, `lp` and `pb` encode to this.
        archive.blocks[0].coders[0].properties_mut()[0] = 0xFF;
        for password in [Password::empty(), Password::from("the password of the job")] {
            match read_all(&archive, &password, &mut source, true) {
                Err(Error::BlockDecode {
                    block_index: 0,
                    kind,
                    ..
                }) => assert_ne!(kind, BlockErrorKind::Password),
                other => panic!("expected damage located in block 0, got {other:?}"),
            }
        }
    }

    /// A read of no bytes is answered with none and is never an early end; a
    /// read past the declared length is passed on, which is how an LZMA2
    /// coder is brought to its end marker.
    #[test]
    fn only_a_read_that_is_owed_bytes_can_end_early() {
        let source = [1u8, 2, 3];
        let mut buf = [0u8; 8];

        let mut bounded = BoundedReader::new(&source[..], 5);
        assert_eq!(bounded.read(&mut []).expect("no bytes asked for"), 0);
        assert_eq!(bounded.read(&mut buf).expect("three bytes"), 3);
        assert_eq!(bounded.read(&mut []).expect("no bytes asked for"), 0);
        let error = bounded.read(&mut buf).expect_err("two bytes are owed");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);

        let mut declared = DeclaredLength {
            inner: &source[..],
            owed: 2,
        };
        assert_eq!(declared.read(&mut buf).expect("not capped"), 3);
        assert_eq!(declared.read(&mut buf).expect("nothing is owed"), 0);

        let mut declared = DeclaredLength {
            inner: &source[..],
            owed: 4,
        };
        assert_eq!(declared.read(&mut []).expect("no bytes asked for"), 0);
        assert_eq!(declared.read(&mut buf).expect("three bytes"), 3);
        let error = declared.read(&mut buf).expect_err("one byte is owed");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}

#[cfg(all(test, feature = "compress"))]
mod chain_plan_tests {
    use std::io::Cursor;

    use super::*;
    use crate::{ArchiveWriter, encoder_options::Lzma2Options};

    /// A chain of two LZMA2 coders with enough to decode in parallel each is
    /// given one parallel reader. Each coder used to plan for itself: two
    /// readers of four threads at four threads, each with the whole of the
    /// in-flight budget.
    #[test]
    fn a_chain_of_two_lzma2_coders_is_given_one_parallel_reader() {
        // Bytes that do not compress, so that the coder underneath has as
        // much to decode as the one on top.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let data: Vec<u8> = (0..8 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 32) as u8
            })
            .collect();
        let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
        options.set_dictionary_size(1 << 20);
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
        writer.set_content_methods(vec![options.clone().into(), options.into()]);
        writer
            .push_archive_entry(ArchiveEntry::new_file("one"), Some(data.as_slice()))
            .expect("push");
        let mut source = writer.finish().expect("finish");
        let archive = Archive::read(&mut source, &Password::empty()).expect("parse");
        let block = &archive.blocks[0];
        assert_eq!(block.coders.len(), 2);
        for index in 0..2 {
            assert!(
                block.get_unpack_size_at_index(index) > 4 << 20,
                "coder {index} has too little to decode in parallel"
            );
        }

        let password = Password::empty();
        crate::decoder::PARALLEL_LZMA2_PLANS.with(|plans| plans.set(0));
        let mut out = Vec::new();
        BlockDecoder::new(4, 0, &archive, &password, &mut source)
            .for_each_entries(&mut |_, rd| {
                rd.read_to_end(&mut out)?;
                Ok(true)
            })
            .expect("decodes");
        assert!(out == data, "the chain decoded to other bytes");
        assert_eq!(crate::decoder::PARALLEL_LZMA2_PLANS.with(Cell::get), 1);
    }
}

#[cfg(test)]
mod folder_plan_tests {
    use super::*;

    /// Archives of varied shapes: unpacked sizes either side of the staging
    /// bound, with and without decoder estimates.
    fn shapes() -> Vec<Vec<(u64, u64)>> {
        let small = |n: usize, estimate: u64| vec![(32 << 10, estimate); n];
        let mut mixed = Vec::new();
        for i in 0..200u64 {
            let unpacked = if i % 37 == 0 {
                64 << 20
            } else {
                (i * 7919) % (9 << 20)
            };
            mixed.push((unpacked, if i % 5 == 0 { u64::MAX } else { 1 << 20 }));
        }
        vec![
            Vec::new(),
            small(1, 1 << 20),
            small(2, 1 << 20),
            small(3, 64 << 20),
            small(8192, 2 << 20),
            vec![(FOLDER_STAGE_BYTES, 0), (FOLDER_STAGE_BYTES + 1, 0), (1, 0)],
            mixed,
        ]
    }

    #[test]
    fn the_plan_never_exceeds_the_thread_budget() {
        let limits = [u64::MAX, 0, 1, 16 << 20, 40 << 20, 1 << 30, 64 << 30];
        for folders in shapes() {
            for threads in [0, 1, 2, 3, 4, 5, 7, 8, 13, 18, 64, 255] {
                for limit in limits {
                    let phases = plan_folders(folders.iter().copied(), threads, limit);
                    let mut next = 0;
                    for phase in &phases {
                        match phase {
                            FolderPhase::Alone { block } => {
                                assert_eq!(*block, next);
                                next += 1;
                            }
                            FolderPhase::Parallel {
                                blocks,
                                workers,
                                threads_per_folder,
                            } => {
                                assert_eq!(blocks.start, next);
                                next = blocks.end;
                                assert!(blocks.len() >= 2);
                                assert!(*workers >= 2 && *threads_per_folder >= 1);
                                assert!(*workers as usize <= blocks.len());
                                assert!(
                                    u64::from(*workers) * u64::from(*threads_per_folder)
                                        <= u64::from(threads.max(1)),
                                    "{workers} x {threads_per_folder} over {threads} threads"
                                );
                                if limit != u64::MAX {
                                    assert_eq!(*threads_per_folder, 1);
                                    let largest =
                                        folders[blocks.clone()].iter().map(|f| f.1).max().unwrap();
                                    let per_worker = largest.saturating_add(FOLDER_WORKER_BYTES);
                                    assert!(
                                        u64::from(*workers)
                                            .saturating_mul(per_worker)
                                            .saturating_add(FOLDER_CHUNK_BYTES as u64)
                                            <= limit
                                    );
                                }
                                for &(unpacked, _) in &folders[blocks.clone()] {
                                    assert!(unpacked <= FOLDER_STAGE_BYTES);
                                }
                            }
                        }
                    }
                    assert_eq!(next, folders.len(), "every folder planned once, in order");
                    if threads <= 1 {
                        assert!(
                            phases
                                .iter()
                                .all(|p| matches!(p, FolderPhase::Alone { .. })),
                            "one thread decodes every folder alone"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_run_is_charged_everything_its_workers_and_the_caller_hold() {
        // Two stored folders of the largest size that is staged, which have
        // no decoder to speak of: what a worker holds is its two stages, the
        // piece it is filling and its input buffer.
        let folders = [(FOLDER_STAGE_BYTES, 0), (FOLDER_STAGE_BYTES, 0)];
        let plan = |limit| plan_folders(folders.into_iter(), 8, limit);
        let alone = vec![
            FolderPhase::Alone { block: 0 },
            FolderPhase::Alone { block: 1 },
        ];
        let together = vec![FolderPhase::Parallel {
            blocks: 0..2,
            workers: 2,
            threads_per_folder: 1,
        }];
        // The stages alone are the whole of 32 MiB.
        assert_eq!(plan(32 << 20), alone);
        let fits = 2 * FOLDER_WORKER_BYTES + FOLDER_CHUNK_BYTES as u64;
        assert_eq!(plan(fits), together);
        assert_eq!(plan(fits - 1), alone);
        // A decoder is charged to each worker as well.
        let folders = [(1 << 20, 4 << 20), (1 << 20, 1 << 20)];
        let plan = |limit| plan_folders(folders.into_iter(), 8, limit);
        assert_eq!(plan(fits + (8 << 20)), together);
        assert_eq!(plan(fits + (8 << 20) - 1), alone);
    }

    #[test]
    fn a_large_folder_keeps_the_whole_budget() {
        let phases = plan_folders(
            [
                (1, 0),
                (2, 0),
                (FOLDER_STAGE_BYTES + 1, 0),
                (3, 0),
                (4, 0),
                (5, 0),
            ]
            .into_iter(),
            8,
            u64::MAX,
        );
        assert!(matches!(
            phases[..],
            [
                FolderPhase::Parallel { ref blocks, workers: 2, threads_per_folder: 4 },
                FolderPhase::Alone { block: 2 },
                FolderPhase::Parallel { blocks: ref rest, workers: 3, threads_per_folder: 2 },
            ] if *blocks == (0..2) && *rest == (3..6)
        ));
    }
}
