//! Stored folders read, checksummed and decrypted off the calling thread.
//!
//! A stored (COPY) folder has no coder to speak of: decoding it is reading
//! the pack stream, checking each file's CRC-32 and handing the bytes to the
//! caller's sink. On one thread those three take turns, so the kernel's copy
//! out of the archive and its copy into the output never overlap. Here a
//! reader thread fills [`SPAN_BYTES`] spans from a positional cursor of its
//! own and checksums them as it reads, and the calling thread only writes:
//! the two copies run at once, and no checksum is taken on the thread that
//! delivers the bytes (see `AGENTS.md`).
//!
//! An AES folder over stored data adds one stage: the reader thread fetches
//! ciphertext, a second thread decrypts it through the same
//! `Aes256Sha256Decoder` the serial path uses and checksums the plaintext,
//! which is where it is produced, and the caller writes.
//!
//! The checksums go where the parallel LZMA2 coder's go, into the reader's
//! [`Lzma2Control`] fold, a piece per file, before the span holding a file's
//! last byte is sent; so a file is always covered by the time the caller has
//! read its last byte, and `BlockDecoder::walk_entries` checks it exactly as
//! it checks a file the LZMA2 workers checksummed.
//!
//! Each stage holds at most [`SPANS_PER_STAGE`] spans: [`QUEUE_DEPTH`]
//! queued, one being filled and one its consumer is working through. Spans
//! are full-length buffers recycled back to the stage that filled them, with
//! the filled length carried beside them, so none is zeroed twice.
//!
//! A stage's error reaches its consumer after every span read before it, and
//! every read after that returns it again rather than an end of stream. A
//! consumer that is dropped — the callback stopped early, or its sink failed
//! — closes the channel: the stage's next send fails and it exits, and the
//! drop waits for it, so nothing reads on once the caller has moved on. A
//! stage that panics is resumed on the thread that reads it.

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::JoinHandle;

use lzma_turbo::crc::Crc32;

use crate::codec::lzma_turbo::Lzma2Control;
use crate::decoder::DecodeRead;

/// Bytes a stage reads, decrypts and hands on at a time.
pub(crate) const SPAN_BYTES: usize = 1 << 20;

/// Spans a stage may have sent and its consumer not yet taken.
const QUEUE_DEPTH: usize = 2;

/// The most spans one stage ever allocates: [`QUEUE_DEPTH`] queued, one it
/// is filling and one its consumer is reading.
const SPANS_PER_STAGE: usize = QUEUE_DEPTH + 2;

/// What one stage holds at most, for the memory limit.
pub(crate) const STAGE_BYTES: u64 = (SPANS_PER_STAGE * SPAN_BYTES) as u64;

/// What a stage sends its consumer, in order.
enum Message {
    /// A span and how much of it is filled.
    Span(Vec<u8>, usize),
    /// The stream ended cleanly.
    End,
    /// Reading failed, after the spans sent before this.
    Fail(io::Error),
}

/// The CRC-32 of each file of a folder, taken as its bytes are read and
/// folded into the reader's control a file at a time.
pub(crate) struct Checksums {
    control: Arc<Lzma2Control>,
    /// Where each file after the first starts; `u64::MAX` alone for a
    /// folder whose one range is the whole stream. See
    /// `BlockDecoder::file_boundaries`.
    splits: Vec<u64>,
    next: usize,
    /// Where the piece being checksummed starts.
    start: u64,
    pos: u64,
    end: u64,
    crc: Crc32,
}

impl Checksums {
    pub(crate) fn new(control: Arc<Lzma2Control>, splits: &[u64], end: u64) -> Self {
        Self {
            control,
            splits: splits.to_vec(),
            next: 0,
            start: 0,
            pos: 0,
            end,
            crc: Crc32::new(),
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() && self.pos < self.end {
            let boundary = self
                .splits
                .get(self.next)
                .copied()
                .filter(|&split| split > self.pos && split < self.end)
                .unwrap_or(self.end);
            let take = (boundary - self.pos).min(bytes.len() as u64) as usize;
            self.crc.update(&bytes[..take]);
            self.pos += take as u64;
            bytes = &bytes[take..];
            if self.pos == boundary {
                let crc = std::mem::replace(&mut self.crc, Crc32::new()).finalize();
                self.control
                    .fold_piece(self.start, self.pos - self.start, crc);
                self.start = self.pos;
                while self
                    .splits
                    .get(self.next)
                    .is_some_and(|&split| split <= self.pos)
                {
                    self.next += 1;
                }
            }
        }
    }
}

/// Starts a stage that reads `reader` to its end, or to `limit` bytes,
/// checksumming what it reads when given `checksums`.
///
/// # Errors
///
/// Whatever the system refused the thread with. Nothing has been read then.
pub(crate) fn spawn<R: Read + Send + 'static>(
    reader: R,
    limit: u64,
    checksums: Option<Checksums>,
) -> io::Result<SpanReader> {
    let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
    let (recycle, spares) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("sevenz-store".into())
        .spawn(move || produce(reader, limit, checksums, &tx, &spares))?;
    Ok(SpanReader {
        rx: Some(rx),
        recycle: Some(recycle),
        worker: Some(worker),
        span: Vec::new(),
        filled: 0,
        pos: 0,
        tail: Tail::Open,
    })
}

/// How long a stage's spans are, and how many it makes at most, for a stage
/// of `limit` bytes: [`SPAN_BYTES`] and [`SPANS_PER_STAGE`], or no more than
/// the stage can fill, so a short stream neither zeroes a whole span nor
/// makes spans it will never fill.
fn span_shape(limit: u64) -> (usize, usize) {
    let size = usize::try_from(limit).map_or(SPAN_BYTES, |limit| limit.min(SPAN_BYTES));
    let needed = usize::try_from(limit.div_ceil(size.max(1) as u64)).unwrap_or(usize::MAX);
    (size, needed.clamp(1, SPANS_PER_STAGE))
}

/// A span to fill: a spare the consumer handed back, a new one of `size`
/// bytes while the stage has made fewer than `cap`, or the next spare to
/// come back. `None` once the consumer is gone.
fn next_spare(
    spares: &Receiver<Vec<u8>>,
    made: &mut usize,
    size: usize,
    cap: usize,
) -> Option<io::Result<Vec<u8>>> {
    match spares.try_recv() {
        Ok(span) => Some(Ok(span)),
        Err(TryRecvError::Empty) if *made < cap => {
            let mut span = Vec::new();
            if let Err(error) = span.try_reserve_exact(size) {
                return Some(Err(io::Error::new(io::ErrorKind::OutOfMemory, error)));
            }
            span.resize(size, 0);
            *made += 1;
            Some(Ok(span))
        }
        Err(TryRecvError::Empty) => spares.recv().ok().map(Ok),
        Err(TryRecvError::Disconnected) => None,
    }
}

fn produce<R: Read>(
    mut reader: R,
    mut left: u64,
    mut checksums: Option<Checksums>,
    tx: &SyncSender<Message>,
    spares: &Receiver<Vec<u8>>,
) {
    let mut made = 0;
    let (size, cap) = span_shape(left);
    loop {
        let mut span = match next_spare(spares, &mut made, size, cap) {
            None => return,
            Some(Ok(span)) => span,
            Some(Err(error)) => {
                let _ = tx.send(Message::Fail(error));
                return;
            }
        };
        let want = (span.len() as u64).min(left) as usize;
        let mut filled = 0;
        let mut outcome = Ok(left != 0);
        while filled < want {
            match reader.read(&mut span[filled..want]) {
                Ok(0) => {
                    outcome = Ok(false);
                    break;
                }
                Ok(n) => filled += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        left -= filled as u64;
        if left == 0 && matches!(outcome, Ok(true)) {
            outcome = Ok(false);
        }
        if filled != 0 {
            // Before the span is sent: a consumer that has the last byte of
            // a file finds the file's checksum folded.
            if let Some(checksums) = checksums.as_mut() {
                checksums.update(&span[..filled]);
            }
            if tx.send(Message::Span(span, filled)).is_err() {
                return;
            }
        }
        match outcome {
            Ok(true) => {}
            Ok(false) => {
                let _ = tx.send(Message::End);
                return;
            }
            Err(error) => {
                let _ = tx.send(Message::Fail(error));
                return;
            }
        }
    }
}

/// How a stage's stream ended, once its consumer has seen it.
enum Tail {
    Open,
    Ended,
    /// A copy of the error, kept so a read after it fails the same way.
    Failed(io::Error),
}

/// What a stage's consumer reads: the spans, in order.
pub(crate) struct SpanReader {
    rx: Option<Receiver<Message>>,
    recycle: Option<Sender<Vec<u8>>>,
    worker: Option<JoinHandle<()>>,
    span: Vec<u8>,
    filled: usize,
    pos: usize,
    tail: Tail,
}

impl SpanReader {
    /// Makes the next bytes current; `Ok(false)` at the end of the stream.
    fn ready(&mut self) -> io::Result<bool> {
        while self.pos == self.filled {
            match &self.tail {
                Tail::Open => {}
                Tail::Ended => return Ok(false),
                Tail::Failed(error) => return Err(crate::error::copy_io(error)),
            }
            let received = match self.rx.as_ref() {
                Some(rx) => rx.recv(),
                None => Err(mpsc::RecvError),
            };
            match received {
                Ok(Message::Span(span, filled)) => {
                    let spent = std::mem::replace(&mut self.span, span);
                    if !spent.is_empty()
                        && let Some(recycle) = self.recycle.as_ref()
                    {
                        let _ = recycle.send(spent);
                    }
                    self.filled = filled;
                    self.pos = 0;
                }
                Ok(Message::End) => {
                    self.tail = Tail::Ended;
                    self.join();
                }
                Ok(Message::Fail(error)) => {
                    self.tail = Tail::Failed(crate::error::copy_io(&error));
                    self.join();
                    return Err(error);
                }
                Err(_) => {
                    // Gone without saying how: it panicked, and `join`
                    // resumes the panic here.
                    self.tail =
                        Tail::Failed(io::Error::other("a stored folder's reader thread stopped"));
                    self.join();
                }
            }
        }
        Ok(true)
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(payload);
        }
    }
}

impl Read for SpanReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || !self.ready()? {
            return Ok(0);
        }
        let n = out.len().min(self.filled - self.pos);
        out[..n].copy_from_slice(&self.span[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl DecodeRead for SpanReader {
    fn push(&mut self, max: usize, sink: &mut dyn FnMut(&[u8])) -> Option<io::Result<usize>> {
        if max == 0 {
            return Some(Ok(0));
        }
        match self.ready() {
            Ok(true) => {}
            Ok(false) => return Some(Ok(0)),
            Err(error) => return Some(Err(error)),
        }
        let n = max.min(self.filled - self.pos);
        sink(&self.span[self.pos..self.pos + n]);
        self.pos += n;
        Some(Ok(n))
    }
}

impl Drop for SpanReader {
    fn drop(&mut self) {
        // Closing both channels is what stops the stage: a send to a closed
        // channel fails, and so does waiting for a spare that cannot come.
        self.rx.take();
        self.recycle.take();
        self.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + i / 977) as u8).collect()
    }

    /// Hands out `good` bytes, then fails every read.
    struct FailsAfter {
        data: Vec<u8>,
        pos: usize,
        good: usize,
    }

    impl Read for FailsAfter {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.good {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged"));
            }
            let n = out.len().min(self.good - self.pos).min(100_003);
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// A stage shorter than a span fills one span its own size; a longer one
    /// makes no more spans than it can fill, up to the per-stage bound.
    #[test]
    fn spans_are_sized_to_the_stage() {
        assert_eq!(span_shape(0), (0, 1));
        assert_eq!(span_shape(4_097), (4_097, 1));
        assert_eq!(span_shape(SPAN_BYTES as u64), (SPAN_BYTES, 1));
        assert_eq!(span_shape(2 * SPAN_BYTES as u64 + 1), (SPAN_BYTES, 3));
        assert_eq!(span_shape(u64::MAX), (SPAN_BYTES, SPANS_PER_STAGE));

        let input = data(4_097);
        let mut reader = spawn(io::Cursor::new(input.clone()), 4_097, None).unwrap();
        let mut first = [0u8; 1];
        assert_eq!(reader.read(&mut first).unwrap(), 1);
        assert_eq!(reader.span.len(), 4_097);
        assert_eq!(reader.span.capacity(), 4_097);
        let mut out = first.to_vec();
        reader.read_to_end(&mut out).unwrap();
        assert!(out == input);
    }

    #[test]
    fn spans_arrive_in_order_and_stop_at_the_limit() {
        let input = data(5 * SPAN_BYTES + 12_345);
        let limit = 4 * SPAN_BYTES as u64 + 7;
        let mut reader = spawn(io::Cursor::new(input.clone()), limit, None).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert!(out == input[..limit as usize]);
        assert_eq!(reader.read(&mut [0; 8]).unwrap(), 0);
    }

    #[test]
    fn an_error_follows_every_byte_before_it_and_stays() {
        let input = data(3 * SPAN_BYTES);
        let good = SPAN_BYTES + 777;
        let mut reader = spawn(
            FailsAfter {
                data: input.clone(),
                pos: 0,
                good,
            },
            u64::MAX,
            None,
        )
        .unwrap();
        let mut out = Vec::new();
        let error = loop {
            match reader.push(SPAN_BYTES, &mut |bytes| out.extend_from_slice(bytes)) {
                Some(Ok(0)) => panic!("ended instead of failing"),
                Some(Ok(_)) => {}
                Some(Err(error)) => break error,
                None => unreachable!(),
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(out == input[..good]);
        // A retried read fails again; it is not the end of the stream.
        for _ in 0..2 {
            let again = reader.read(&mut [0; 16]).unwrap_err();
            assert_eq!(again.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn checksums_are_folded_per_file_before_the_bytes_arrive() {
        let input = data(3 * SPAN_BYTES + 99);
        let end = input.len() as u64;
        let splits = [
            10,
            SPAN_BYTES as u64,
            SPAN_BYTES as u64 + 1,
            2 * SPAN_BYTES as u64 + 5,
        ];
        let control = Arc::new(Lzma2Control::new(1));
        let checksums = Checksums::new(Arc::clone(&control), &splits, end);
        let mut reader = spawn(io::Cursor::new(input.clone()), end, Some(checksums)).unwrap();
        let mut starts = vec![0];
        starts.extend(splits);
        starts.push(end);
        let mut seen = 0usize;
        for pair in starts.windows(2) {
            let (start, stop) = (pair[0] as usize, pair[1] as usize);
            while seen < stop {
                let mut buf = vec![0; stop - seen];
                let n = reader.read(&mut buf).unwrap();
                assert!(n > 0);
                seen += n;
            }
            let crc = lzma_turbo::crc::crc32(&input[start..stop]);
            assert_eq!(
                control.folded(start as u64, (stop - start) as u64),
                Some(crc)
            );
        }
        assert_eq!(control.folded(0, end), Some(lzma_turbo::crc::crc32(&input)));
    }

    /// A consumer that goes away stops its stage after at most the spans it
    /// may be ahead by, and the drop waits for it.
    #[test]
    fn dropping_the_consumer_stops_the_stage() {
        struct Counting(Arc<std::sync::atomic::AtomicU64>);
        impl Read for Counting {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.0
                    .fetch_add(out.len() as u64, std::sync::atomic::Ordering::Relaxed);
                Ok(out.len())
            }
        }
        let read = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut reader = spawn(Counting(Arc::clone(&read)), u64::MAX, None).unwrap();
        let mut first = [0u8; 10];
        reader.read_exact(&mut first).unwrap();
        drop(reader);
        let total = read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(total <= STAGE_BYTES, "read {total} bytes ahead");
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(read.load(std::sync::atomic::Ordering::Relaxed), total);
    }

    #[test]
    #[should_panic(expected = "stage panicked")]
    fn a_panicking_stage_panics_its_consumer() {
        struct Panics;
        impl Read for Panics {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("stage panicked");
            }
        }
        let mut reader = spawn(Panics, u64::MAX, None).unwrap();
        let _ = reader.read(&mut [0; 4]);
    }
}
