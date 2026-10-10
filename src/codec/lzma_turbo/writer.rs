//! `Write` adapters over `lzma-turbo`'s LZMA and LZMA2 encoders, for the
//! coder chain `crate::encoder` builds.
//!
//! `lzma-turbo`'s encoders are pull-driven, as the SDK's are: the match finder
//! reads its window from a `SeqInStream` whenever it runs low, and an empty
//! read is the end of the input. The archive writer pushes. The bridge is a
//! thread: the encoder runs on it, pulling from a bounded queue the writer
//! feeds, and its output comes back over a second bounded queue that the
//! writer drains into the inner sink on every write, and while it waits for
//! room in the first. Memory is bounded by the input queue's depth times the
//! chunk size and by the output queue's cap (or one finished block), plus
//! whatever the encoder itself holds (its dictionary, and for block-parallel
//! LZMA2 one block per thread).
//!
//! Where a thread cannot be started - `wasm32-unknown-unknown` has none - the
//! adapter drives `lzma-turbo`'s push encoders on the caller's thread instead:
//! each write is queued and the encoder's block loop runs as far as the queue
//! allows, so what is held is the encoder's window plus about one LZMA2 chunk,
//! whatever the folder's size. That path is always one solid stream, which is
//! what the threaded path writes at one thread, byte for byte.
//!
//! Building with `--cfg sevenz_turbo_unthreaded` makes every writer take that
//! path, so that it can be tested and measured on a host with threads.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use lzma_turbo::{
    BLOCK_SIZE_SOLID, Error as LzmaError, Lzma2Encoder, Lzma2PushEncoder, LzmaEncProps,
    LzmaEncoder, LzmaPushEncoder, SeqInStream, SeqOutStream,
};

/// Bytes per message on the input channel. A caller's large write is cut into
/// these so that what sits in flight is bounded whatever it hands over.
const CHUNK: usize = 256 << 10;

/// Messages the input channel holds before a write blocks. Small on purpose:
/// the encoder consumes at most this much ahead of the writer.
const INPUT_DEPTH: usize = 2;

/// Compressed bytes the encoder may queue for the writer before it waits for
/// the writer to take them. A finished block of a block-parallel coder is
/// larger and waits alone, so what is queued is at most one block or this.
const OUTPUT_CAP: usize = 4 << 20;

/// Which coder the writer fronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coder {
    /// A raw LZMA stream: no header, no end marker. The five property bytes
    /// live in the folder's coder record.
    Lzma,
    /// A raw LZMA2 stream. `block_size` and `threads` are the block-parallel
    /// settings; one thread is the solid, single-threaded stream.
    Lzma2 { block_size: u64, threads: usize },
}

/// The fixed settings of a single-threaded LZMA side coder, such as BCJ2's
/// call and jump coders: the level's match finder with an explicit
/// dictionary, fast bytes and literal/position bits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SideCoder {
    pub(crate) level: u32,
    pub(crate) dict_size: u32,
    pub(crate) fast_bytes: u32,
    pub(crate) lc: u8,
    pub(crate) lp: u8,
    pub(crate) pb: u8,
}

impl SideCoder {
    /// A raw LZMA writer (no header, no end marker) with these settings.
    ///
    /// # Errors
    ///
    /// As [`LzmaTurboWriter::new`].
    pub(crate) fn writer<W: Write>(&self, inner: W) -> io::Result<LzmaTurboWriter<W>> {
        LzmaTurboWriter::new(inner, &self.props(), Coder::Lzma)
    }

    /// The same writer with its encoder on the thread that writes to it: the
    /// same stream, and no thread started for it.
    ///
    /// # Errors
    ///
    /// As [`LzmaTurboWriter::new`].
    pub(crate) fn inline_writer<W: Write>(&self, inner: W) -> io::Result<LzmaTurboWriter<W>> {
        LzmaTurboWriter::inline(inner, &self.props(), Coder::Lzma)
    }

    fn props(&self) -> LzmaEncProps {
        LzmaEncProps::new()
            .with_level(self.level)
            .with_dict_size(self.dict_size)
            .with_fast_bytes(self.fast_bytes)
            .with_lclppb(self.lc, self.lp, self.pb)
            .with_num_threads(1)
    }
}

/// The encoder, built up front so that a bad setting is refused where the
/// coder is added and not later on a thread.
enum Encoder {
    Lzma(LzmaEncoder),
    Lzma2(Box<Lzma2Encoder>),
}

impl Encoder {
    fn new(props: &LzmaEncProps, coder: Coder) -> Result<Self, LzmaError> {
        match coder {
            Coder::Lzma => Ok(Encoder::Lzma(LzmaEncoder::new(
                &props.with_end_mark(false),
            )?)),
            Coder::Lzma2 {
                block_size,
                threads,
            } => {
                let mut enc = Lzma2Encoder::new(props)?;
                enc.set_block_size(block_size);
                enc.set_threads(threads);
                Ok(Encoder::Lzma2(Box::new(enc)))
            }
        }
    }

    /// Runs the whole stream on the calling thread, from an input that may
    /// not be sent anywhere. The same bytes as [`Encoder::run`] for a coder
    /// [`PullCoder::drives`]: with one block thread and the one-thread match
    /// finder, `encode_mt` and `encode_send` are both this loop.
    fn run_here(
        &mut self,
        input: &mut dyn SeqInStream,
        out: &mut dyn SeqOutStream,
    ) -> Result<(), LzmaError> {
        match self {
            Encoder::Lzma(enc) => enc.encode(input, out),
            Encoder::Lzma2(enc) => enc.encode(input, out),
        }
    }

    /// Runs the whole stream.
    fn run(
        &mut self,
        input: &mut (dyn SeqInStream + Send),
        out: &mut (dyn SeqOutStream + Send),
    ) -> Result<(), LzmaError> {
        match self {
            Encoder::Lzma(enc) => enc.encode_send(input, out),
            Encoder::Lzma2(enc) => enc.encode_mt(input, out),
        }
    }
}

/// The encoder the thread-less path pushes into. LZMA2 is one solid block on
/// one thread, whatever block plan the coder was given: block threads are
/// what that plan is for, and there are none.
enum Pusher {
    Lzma(Box<LzmaPushEncoder>),
    Lzma2(Box<Lzma2PushEncoder>),
}

impl Pusher {
    fn new(props: &LzmaEncProps, coder: Coder) -> Result<Self, LzmaError> {
        match coder {
            Coder::Lzma => Ok(Pusher::Lzma(Box::new(LzmaPushEncoder::new(
                &props.with_end_mark(false),
            )?))),
            Coder::Lzma2 { .. } => Ok(Pusher::Lzma2(Box::new(Lzma2PushEncoder::new(props)?))),
        }
    }

    fn push(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), LzmaError> {
        match self {
            Pusher::Lzma(enc) => enc.push(data, out),
            Pusher::Lzma2(enc) => enc.push(data, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), LzmaError> {
        match self {
            Pusher::Lzma(enc) => enc.finish(out),
            Pusher::Lzma2(enc) => enc.finish(out),
        }
    }
}

/// What the writer and the encoder thread hand each other: input chunks one
/// way, compressed output the other, both bounded, behind one lock so that
/// the writer can wait for either. Two channels cannot do that: a writer
/// blocked on a full input channel cannot take output, and an encoder
/// blocked on a full output channel then never reads input again.
struct Bridge {
    queues: Mutex<Queues>,
    /// Signalled on every change to `queues`.
    changed: Condvar,
}

#[derive(Default)]
struct Queues {
    /// At most `INPUT_DEPTH` chunks.
    input: VecDeque<Vec<u8>>,
    /// `finish` has sent the last chunk.
    input_closed: bool,
    /// Held to at most `OUTPUT_CAP` bytes, except that one message larger
    /// than that - a whole block - may wait alone.
    output: VecDeque<Vec<u8>>,
    output_bytes: usize,
    /// The encoder thread has returned: no more output, no more reads.
    encoder_gone: bool,
    /// The writer was dropped without `finish`: the encoder's input ends and
    /// its output has nowhere to go.
    writer_gone: bool,
    /// The most output that has waited at once.
    #[cfg(test)]
    output_peak: usize,
}

impl Bridge {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        // Nothing panics while holding the lock, so a poisoned one is still
        // consistent.
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, q: MutexGuard<'a, Queues>) -> MutexGuard<'a, Queues> {
        self.changed.wait(q).unwrap_or_else(PoisonError::into_inner)
    }

    /// The encoder's side: queue `buf` for the writer, waiting while the
    /// output already held is at the cap.
    fn put_output(&self, buf: Vec<u8>) -> Result<(), LzmaError> {
        let mut q = self.lock();
        while !q.writer_gone && !q.output.is_empty() && q.output_bytes + buf.len() > OUTPUT_CAP {
            q = self.wait(q);
        }
        if q.writer_gone {
            // The encoder has nowhere to put bytes and stops.
            return Err(LzmaError::Write);
        }
        q.output_bytes += buf.len();
        q.output.push_back(buf);
        #[cfg(test)]
        {
            q.output_peak = q.output_peak.max(q.output_bytes);
        }
        drop(q);
        self.changed.notify_all();
        Ok(())
    }
}

/// Marks the encoder thread's end, however it ends, so that the writer
/// never waits for it after.
struct EncoderEnd(Arc<Bridge>);

impl Drop for EncoderEnd {
    fn drop(&mut self) {
        self.0.lock().encoder_gone = true;
        self.0.changed.notify_all();
    }
}

/// The encoder's view of the input queue.
struct ChannelSource {
    bridge: Arc<Bridge>,
    /// Where a chunk goes once it has been read, for the writer to fill
    /// again.
    spent: Sender<Vec<u8>>,
    pending: Vec<u8>,
    pos: usize,
}

impl SeqInStream for ChannelSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, LzmaError> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let left = &self.pending[self.pos..];
            if !left.is_empty() {
                let n = buf.len().min(left.len());
                buf[..n].copy_from_slice(&left[..n]);
                self.pos += n;
                return Ok(n);
            }
            let mut q = self.bridge.lock();
            let chunk = loop {
                if let Some(chunk) = q.input.pop_front() {
                    break chunk;
                }
                // The writer having finished, or gone: end of input.
                if q.input_closed || q.writer_gone {
                    return Ok(0);
                }
                q = self.bridge.wait(q);
            };
            drop(q);
            self.bridge.changed.notify_all();
            let spent = core::mem::replace(&mut self.pending, chunk);
            self.pos = 0;
            // The writer has gone only if it is being dropped; the buffer is
            // then freed here.
            if spent.capacity() != 0 {
                let _ = self.spent.send(spent);
            }
        }
    }
}

/// The encoder's view of the output queue.
struct ChannelSink {
    bridge: Arc<Bridge>,
    /// Buffers the writer has written out, to be filled again.
    spare: Receiver<Vec<u8>>,
}

impl SeqOutStream for ChannelSink {
    fn write(&mut self, data: &[u8]) -> Result<(), LzmaError> {
        let mut buf = self.spare.try_recv().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(data);
        self.bridge.put_output(buf)
    }

    /// A block-parallel coder's finished block goes to the writer as the
    /// buffer it was written into, and the coder gets a written-out buffer
    /// back for its next block, so a block is never held twice.
    fn write_vec(&mut self, data: &mut Vec<u8>) -> Result<(), LzmaError> {
        let mut next = self.spare.try_recv().unwrap_or_default();
        next.clear();
        let block = core::mem::replace(data, next);
        self.bridge.put_output(block)
    }
}

/// The writer's hold on the bridge. Dropping it without `finish` - a writer
/// abandoned mid-stream - lets the encoder thread run out and stop.
struct WriterEnd(Arc<Bridge>);

impl Drop for WriterEnd {
    fn drop(&mut self) {
        self.0.lock().writer_gone = true;
        self.0.changed.notify_all();
    }
}

enum State {
    Streaming {
        bridge: WriterEnd,
        /// Chunks the encoder has read, to be filled again.
        spent: Receiver<Vec<u8>>,
        /// Where an output buffer goes once it is written out.
        spare: Sender<Vec<u8>>,
        /// Input chunks allocated rather than taken back from `spent`.
        #[cfg(test)]
        input_allocs: usize,
        worker: Option<JoinHandle<Result<(), LzmaError>>>,
    },
    /// No encoder thread: the encoder runs inside `write`.
    Pushed {
        encoder: Pusher,
        /// What the encoder produced during the current call, handed to the
        /// inner writer before the call returns.
        out: Vec<u8>,
    },
}

/// A `Write` that compresses into `inner` with one of `lzma-turbo`'s
/// encoders.
pub(crate) struct LzmaTurboWriter<W: Write> {
    inner: Option<W>,
    state: State,
}

/// The oldest output waiting, if any, taken off the queue.
fn take_output(q: &mut Queues) -> Option<Vec<u8>> {
    let chunk = q.output.pop_front()?;
    q.output_bytes -= chunk.len();
    Some(chunk)
}

/// Writes `chunk` to `inner`, then gives it back to the encoder to fill
/// again.
fn write_out<W: Write>(inner: &mut W, spare: &Sender<Vec<u8>>, chunk: Vec<u8>) -> io::Result<()> {
    inner.write_all(&chunk)?;
    // The encoder has gone only once it is done.
    let _ = spare.send(chunk);
    Ok(())
}

fn io_error(err: LzmaError) -> io::Error {
    let kind = match err {
        LzmaError::Param => io::ErrorKind::InvalidInput,
        LzmaError::Alloc => io::ErrorKind::OutOfMemory,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, err)
}

impl<W: Write> LzmaTurboWriter<W> {
    /// A writer that will compress everything pushed into it with `props`,
    /// as the coder `coder`.
    ///
    /// # Errors
    ///
    /// `InvalidInput` if `lzma-turbo` refuses a setting, `OutOfMemory` if it
    /// could not allocate the encoder.
    pub(crate) fn new(inner: W, props: &LzmaEncProps, coder: Coder) -> io::Result<Self> {
        let mut encoder = Encoder::new(props, coder).map_err(io_error)?;
        let bridge = Arc::new(Bridge {
            queues: Mutex::new(Queues::default()),
            changed: Condvar::new(),
        });
        let theirs = Arc::clone(&bridge);
        // The way back for both: a chunk the encoder has read returns to the
        // writer, and a buffer the writer has written out returns to the
        // encoder, so a stream allocates its buffers once instead of once a
        // chunk, each on one thread and freed on the other.
        let (spent_tx, spent_rx) = mpsc::channel::<Vec<u8>>();
        let (spare_tx, spare_rx) = mpsc::channel::<Vec<u8>>();
        let spawned = if cfg!(sevenz_turbo_unthreaded) {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        } else {
            std::thread::Builder::new()
                .name("sevenz-turbo lzma encoder".into())
                .spawn(move || {
                    // Dropped last, on any way out of here, a panic's too:
                    // what ends the writer's drain.
                    let _end = EncoderEnd(Arc::clone(&theirs));
                    let mut source = ChannelSource {
                        bridge: Arc::clone(&theirs),
                        spent: spent_tx,
                        pending: Vec::new(),
                        pos: 0,
                    };
                    let mut sink = ChannelSink {
                        bridge: theirs,
                        spare: spare_rx,
                    };
                    encoder.run(&mut source, &mut sink)
                })
        };
        let state = match spawned {
            Ok(worker) => State::Streaming {
                bridge: WriterEnd(bridge),
                spent: spent_rx,
                spare: spare_tx,
                #[cfg(test)]
                input_allocs: 0,
                worker: Some(worker),
            },
            Err(_) => {
                // No threads on this target: run the encoder inside `write`.
                // The same settings were accepted a moment ago.
                State::Pushed {
                    encoder: Pusher::new(props, coder).map_err(io_error)?,
                    out: Vec::new(),
                }
            }
        };
        Ok(LzmaTurboWriter {
            inner: Some(inner),
            state,
        })
    }

    /// The writer of [`LzmaTurboWriter::new`] with no encoder thread: the
    /// encoder runs inside `write`, on whichever thread calls it, as it does
    /// where a thread cannot be started.
    ///
    /// # Errors
    ///
    /// As [`LzmaTurboWriter::new`].
    pub(crate) fn inline(inner: W, props: &LzmaEncProps, coder: Coder) -> io::Result<Self> {
        Ok(LzmaTurboWriter {
            inner: Some(inner),
            state: State::Pushed {
                encoder: Pusher::new(props, coder).map_err(io_error)?,
                out: Vec::new(),
            },
        })
    }

    /// Hands what the encoder has produced so far to the inner writer.
    fn drain(&mut self) -> io::Result<()> {
        let State::Streaming { bridge, spare, .. } = &self.state else {
            return Ok(());
        };
        let inner = self.inner.as_mut().expect("open");
        loop {
            let mut q = bridge.0.lock();
            let Some(chunk) = take_output(&mut q) else {
                return Ok(());
            };
            drop(q);
            bridge.0.changed.notify_all();
            write_out(inner, spare, chunk)?;
        }
    }

    /// The error a worker that has stopped early holds.
    fn worker_error(worker: &mut Option<JoinHandle<Result<(), LzmaError>>>) -> io::Error {
        match worker.take().map(JoinHandle::join) {
            Some(Ok(Err(err))) => io_error(err),
            Some(Err(_)) => io::Error::other("the LZMA encoder thread panicked"),
            // The worker finished cleanly, yet the channel to it is closed:
            // it read an end of input this writer never sent. Not reachable
            // while `input` is held, so report it rather than reason about it.
            Some(Ok(Ok(()))) | None => io::Error::other("the LZMA encoder stopped early"),
        }
    }

    /// Compresses whatever is still pending, writes it out, and returns the
    /// wrapped writer.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        let mut inner = self.inner.take().expect("finish once");
        match &mut self.state {
            State::Streaming {
                bridge,
                spare,
                worker,
                ..
            } => {
                // Closing the input is the end-of-stream the encoder waits
                // for; it then writes the rest of its output and returns.
                bridge.0.lock().input_closed = true;
                bridge.0.changed.notify_all();
                loop {
                    let mut q = bridge.0.lock();
                    let chunk = loop {
                        if let Some(chunk) = take_output(&mut q) {
                            break Some(chunk);
                        }
                        if q.encoder_gone {
                            break None;
                        }
                        q = bridge.0.wait(q);
                    };
                    drop(q);
                    let Some(chunk) = chunk else { break };
                    bridge.0.changed.notify_all();
                    write_out(&mut inner, spare, chunk)?;
                }
                match worker.take().expect("joined once").join() {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => return Err(io_error(err)),
                    Err(_) => return Err(io::Error::other("the LZMA encoder thread panicked")),
                }
            }
            State::Pushed { encoder, out } => {
                encoder.finish(out).map_err(io_error)?;
                inner.write_all(out)?;
            }
        }
        inner.flush()?;
        Ok(inner)
    }
}

impl<W: Write> Write for LzmaTurboWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.state {
            State::Streaming { .. } => {
                for chunk in buf.chunks(CHUNK) {
                    let State::Streaming {
                        bridge,
                        worker,
                        spent,
                        spare,
                        #[cfg(test)]
                        input_allocs,
                    } = &mut self.state
                    else {
                        unreachable!()
                    };
                    let inner = self.inner.as_mut().expect("open");
                    let mut buf = spent.try_recv().unwrap_or_else(|_| {
                        #[cfg(test)]
                        {
                            *input_allocs += 1;
                        }
                        Vec::new()
                    });
                    buf.clear();
                    buf.extend_from_slice(chunk);
                    // Queue the chunk, writing out what the encoder produces
                    // while the queue is full: the encoder may be waiting for
                    // exactly that before it reads again.
                    let mut buf = Some(buf);
                    while let Some(chunk) = buf.take() {
                        let mut q = bridge.0.lock();
                        let out = loop {
                            if q.encoder_gone {
                                drop(q);
                                return Err(Self::worker_error(worker));
                            }
                            if let Some(out) = take_output(&mut q) {
                                buf = Some(chunk);
                                break Some(out);
                            }
                            if q.input.len() < INPUT_DEPTH {
                                q.input.push_back(chunk);
                                break None;
                            }
                            q = bridge.0.wait(q);
                        };
                        drop(q);
                        bridge.0.changed.notify_all();
                        if let Some(out) = out {
                            write_out(inner, spare, out)?;
                        }
                    }
                    self.drain()?;
                }
            }
            State::Pushed { encoder, out } => {
                let inner = self.inner.as_mut().expect("open");
                // Cut as the threaded path cuts, so that what one call leaves
                // in `out` is bounded too.
                for chunk in buf.chunks(CHUNK) {
                    encoder.push(chunk, out).map_err(io_error)?;
                    if !out.is_empty() {
                        inner.write_all(out)?;
                        out.clear();
                    }
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain()?;
        self.inner.as_mut().expect("open").flush()
    }
}

/// The encoder's view of a reader: what it reads is the stream's input, and
/// an I/O error is kept here, for [`PullCoder::encode`] to return as it was.
struct ReadSource<'a> {
    inner: &'a mut dyn io::Read,
    error: Option<io::Error>,
}

impl SeqInStream for ReadSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, LzmaError> {
        loop {
            match self.inner.read(buf) {
                Ok(n) => return Ok(n),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    self.error = Some(err);
                    return Err(LzmaError::Read);
                }
            }
        }
    }
}

/// The encoder's view of a writer, keeping an I/O error as [`ReadSource`]
/// does.
struct WriteSink<'a> {
    inner: &'a mut dyn Write,
    error: Option<io::Error>,
}

impl SeqOutStream for WriteSink<'_> {
    fn write(&mut self, data: &[u8]) -> Result<(), LzmaError> {
        self.inner.write_all(data).map_err(|err| {
            self.error = Some(err);
            LzmaError::Write
        })
    }
}

/// An encoder kept between streams, with what it was built for.
struct Kept {
    props: LzmaEncProps,
    coder: Coder,
    encoder: Encoder,
}

/// An LZMA or LZMA2 coder that runs on the thread that calls it, pulling each
/// stream's input from a reader, and keeps its encoder from one stream to the
/// next.
///
/// [`LzmaTurboWriter`] is pushed bytes, so it puts the encoder on a thread of
/// its own (or behind a queue where there are none) and builds it for one
/// stream. A caller that has the whole input as a reader needs neither: the
/// encoder reads it straight into its window here. And a caller that codes
/// many streams with the same settings - one folder after another - builds
/// the encoder, its window and its tables once. Each stream re-initialises
/// the encoder as `LzmaEnc_Prepare` does (lzma-turbo documents its encoders
/// as reusable on that footing), so a stream's bytes are the ones a fresh
/// encoder writes; the tests below hold this to it.
///
/// Only a coder on one thread is driven here: one block thread and the
/// one-thread match finder (see [`PullCoder::drives`]).
#[derive(Default)]
pub(crate) struct PullCoder {
    kept: Option<Kept>,
}

impl PullCoder {
    /// Whether a coder of `props` and `coder` runs on one thread, and so can
    /// be driven here, byte for byte as [`LzmaTurboWriter`] would code it.
    pub(crate) fn drives(props: &LzmaEncProps, coder: Coder) -> bool {
        let one_block_thread = match coder {
            Coder::Lzma => true,
            Coder::Lzma2 { threads, .. } => threads <= 1,
        };
        // A wasm target may or may not start the writer's thread, and the
        // writer cuts LZMA2 blocks only when it does (see
        // [`PullCoder::as_written`]): only a solid stream is the same both ways.
        let same_either_way = !cfg!(target_family = "wasm")
            || !matches!(coder, Coder::Lzma2 { block_size, .. } if block_size != BLOCK_SIZE_SOLID);
        one_block_thread && same_either_way && props.normalized().num_threads <= 1
    }

    /// The coder [`LzmaTurboWriter::new`] actually runs for `coder`. Built
    /// with `--cfg sevenz_turbo_unthreaded` the writer never has a thread and
    /// pushes into one solid LZMA2 stream, whatever block plan it was given,
    /// so a block plan that would cut the stream is dropped here as well.
    fn as_written(coder: Coder) -> Coder {
        match coder {
            Coder::Lzma2 { threads, .. } if cfg!(sevenz_turbo_unthreaded) => Coder::Lzma2 {
                block_size: BLOCK_SIZE_SOLID,
                threads,
            },
            coder => coder,
        }
    }

    /// Codes everything `input` yields as one stream into `out`, with the
    /// encoder kept from the last stream when it was built for the same
    /// `props` and `coder`, and a new one otherwise.
    ///
    /// # Errors
    ///
    /// What `input` or `out` returned, as they returned it; otherwise
    /// `InvalidInput` for a setting `lzma-turbo` refuses and `OutOfMemory`
    /// for an encoder it could not allocate. After an error the encoder is
    /// dropped rather than kept.
    pub(crate) fn encode(
        &mut self,
        props: &LzmaEncProps,
        coder: Coder,
        input: &mut dyn io::Read,
        out: &mut dyn Write,
    ) -> io::Result<()> {
        debug_assert!(Self::drives(props, coder));
        let coder = Self::as_written(coder);
        let reuse = self
            .kept
            .as_ref()
            .is_some_and(|kept| kept.props == *props && kept.coder == coder);
        if !reuse {
            // Free the last encoder before building the next, so the two are
            // never held at once.
            self.kept = None;
            self.kept = Some(Kept {
                props: *props,
                coder,
                encoder: Encoder::new(props, coder).map_err(io_error)?,
            });
        }
        let kept = self.kept.as_mut().expect("built above");
        let mut source = ReadSource {
            inner: input,
            error: None,
        };
        let mut sink = WriteSink {
            inner: out,
            error: None,
        };
        match kept.encoder.run_here(&mut source, &mut sink) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.kept = None;
                Err(source.error.or(sink.error).unwrap_or_else(|| io_error(err)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};

    use lzma_turbo::{BLOCK_SIZE_SOLID, Lzma2Reader, LzmaEncProps, LzmaProps, LzmaReader};

    use super::{CHUNK, Coder, LzmaTurboWriter, PullCoder, SideCoder, State};
    #[cfg(not(any(sevenz_turbo_unthreaded, target_family = "wasm")))]
    use super::{INPUT_DEPTH, OUTPUT_CAP};

    fn sample(len: usize) -> Vec<u8> {
        // Compressible but not trivial: a short period with a slow drift.
        (0..len)
            .map(|i| ((i % 251) as u8).wrapping_add((i / 4096) as u8))
            .collect()
    }

    fn props() -> LzmaEncProps {
        LzmaEncProps::new().with_level(1).with_dict_size(1 << 16)
    }

    /// The LZMA2 property byte for the 64 KiB dictionary `props` sets.
    const DICT_PROP: u8 = 8;

    fn decode_lzma2(packed: &[u8], unpacked: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(unpacked);
        Lzma2Reader::new(Cursor::new(packed), DICT_PROP)
            .expect("reader")
            .read_to_end(&mut out)
            .expect("decode");
        out
    }

    fn decode_lzma(packed: &[u8], unpacked: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(unpacked);
        let mut raw = [0x5Du8; 5];
        raw[1..].copy_from_slice(&(1u32 << 16).to_le_bytes());
        let props = LzmaProps::parse(&raw).expect("props");
        LzmaReader::with_props(Cursor::new(packed), props, Some(unpacked as u64))
            .expect("reader")
            .read_to_end(&mut out)
            .expect("decode");
        out
    }

    /// The writer with the thread bridge, written in pieces of every size
    /// around the chunk cut.
    #[test]
    fn streams_lzma2_in_uneven_pieces() {
        let data = sample(3 * CHUNK + 12345);
        let mut w = LzmaTurboWriter::new(
            Vec::new(),
            &props(),
            Coder::Lzma2 {
                block_size: BLOCK_SIZE_SOLID,
                threads: 1,
            },
        )
        .expect("writer");
        #[cfg(not(any(sevenz_turbo_unthreaded, target_family = "wasm")))]
        assert!(matches!(w.state, State::Streaming { .. }));
        let mut pos = 0;
        for piece in [1, 7, CHUNK - 1, CHUNK, CHUNK + 1, 1000] {
            let end = (pos + piece).min(data.len());
            w.write_all(&data[pos..end]).expect("write");
            pos = end;
        }
        w.write_all(&data[pos..]).expect("write");
        let packed = w.finish().expect("finish");
        assert_eq!(decode_lzma2(&packed, data.len()), data);
    }

    #[test]
    fn streams_lzma_without_header_or_end_mark() {
        let data = sample(CHUNK + 999);
        let mut w = LzmaTurboWriter::new(Vec::new(), &props(), Coder::Lzma).expect("writer");
        w.write_all(&data).expect("write");
        let packed = w.finish().expect("finish");
        assert_eq!(decode_lzma(&packed, data.len()), data);
    }

    #[test]
    fn block_threads_produce_one_decodable_stream() {
        let data = sample(5 * CHUNK);
        let mut w = LzmaTurboWriter::new(
            Vec::new(),
            &props(),
            Coder::Lzma2 {
                block_size: CHUNK as u64,
                threads: 3,
            },
        )
        .expect("writer");
        w.write_all(&data).expect("write");
        let packed = w.finish().expect("finish");
        assert_eq!(decode_lzma2(&packed, data.len()), data);
    }

    /// A stream of many chunks allocates its input chunks a few times, not
    /// once a chunk: the encoder hands each one back once it has read it.
    /// What can be out at once when the writer finds none to take back is
    /// the channel's `INPUT_DEPTH`, the chunk the encoder is reading and the
    /// one it has just replaced, so the writer has allocated at most one more
    /// than that; the bound holds on any schedule. The bytes are the
    /// unthreaded path's.
    #[cfg(not(any(sevenz_turbo_unthreaded, target_family = "wasm")))]
    #[test]
    fn a_long_stream_reuses_its_input_chunks() {
        let chunks = 48;
        let data = sample(chunks * CHUNK + 777);
        let coder = Coder::Lzma2 {
            block_size: BLOCK_SIZE_SOLID,
            threads: 1,
        };
        let mut w = LzmaTurboWriter::new(Vec::new(), &props(), coder).expect("writer");
        for piece in data.chunks(CHUNK / 2 + 3) {
            w.write_all(piece).expect("write");
        }
        let State::Streaming { input_allocs, .. } = &w.state else {
            panic!("not streaming");
        };
        let allocs = *input_allocs;
        assert!(
            allocs <= INPUT_DEPTH + 3,
            "{allocs} input chunks allocated for {chunks} chunks of input"
        );
        let packed = w.finish().expect("finish");
        let mut inline = unthreaded(coder);
        inline.write_all(&data).expect("write");
        assert_eq!(inline.finish().expect("finish"), packed);
    }

    /// Output the encoder cannot compress, so that what it queues is as large
    /// as what it reads.
    #[cfg(not(any(sevenz_turbo_unthreaded, target_family = "wasm")))]
    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    /// The compressed output waiting for the writer is capped: a block-
    /// parallel coder whose output outruns the cap many times over waits for
    /// the writer instead, and the writer, handed the whole input in one call,
    /// takes output while it waits to queue input, so neither stalls the
    /// other. A block larger than the cap waits alone. The bound holds on any
    /// schedule, and the bytes are the ones the stream decodes to.
    #[cfg(not(any(sevenz_turbo_unthreaded, target_family = "wasm")))]
    #[test]
    fn the_output_waiting_for_the_writer_is_capped() {
        for (len, block) in [
            (6 * OUTPUT_CAP, 1 << 20),
            (3 * OUTPUT_CAP, 3 * OUTPUT_CAP / 2),
        ] {
            let data = noise(len);
            let mut w = LzmaTurboWriter::new(
                Vec::new(),
                &props(),
                Coder::Lzma2 {
                    block_size: block as u64,
                    threads: 3,
                },
            )
            .expect("writer");
            w.write_all(&data).expect("write");
            let State::Streaming { bridge, .. } = &w.state else {
                panic!("not streaming");
            };
            let bridge = std::sync::Arc::clone(&bridge.0);
            let packed = w.finish().expect("finish");
            assert!(packed.len() > 2 * OUTPUT_CAP, "the output outruns the cap");
            // A block of incompressible input is stored: a little longer
            // than the block, by a few header bytes per 64 KiB chunk.
            let largest = block + block / 1024;
            let peak = bridge.lock().output_peak;
            assert!(
                peak <= OUTPUT_CAP.max(largest),
                "{peak} bytes of output waited at once for {block}-byte blocks"
            );
            assert_eq!(decode_lzma2(&packed, data.len()), data);
        }
    }

    /// The writer a target without threads gets, and the one a caller asks
    /// for to keep the encoder on its own thread.
    fn unthreaded(coder: Coder) -> LzmaTurboWriter<Vec<u8>> {
        let w = LzmaTurboWriter::inline(Vec::new(), &props(), coder).expect("encoder");
        assert!(matches!(w.state, State::Pushed { .. }));
        w
    }

    fn streamed(coder: Coder, data: &[u8]) -> Vec<u8> {
        let mut w = LzmaTurboWriter::new(Vec::new(), &props(), coder).expect("writer");
        w.write_all(data).expect("write");
        w.finish().expect("finish")
    }

    /// The path a target without threads takes, and its bytes are the
    /// streamed path's, in pieces of every size around the chunk cut.
    #[test]
    fn the_unthreaded_path_matches_the_stream() {
        let data = sample(5 * CHUNK + 4321);
        for coder in [
            Coder::Lzma2 {
                block_size: BLOCK_SIZE_SOLID,
                threads: 1,
            },
            Coder::Lzma,
        ] {
            let want = streamed(coder, &data);
            let mut w = unthreaded(coder);
            let mut pos = 0;
            for piece in [1, 7, CHUNK - 1, CHUNK, CHUNK + 1, 1000, 3 * CHUNK] {
                let end = (pos + piece).min(data.len());
                w.write_all(&data[pos..end]).expect("write");
                pos = end;
            }
            w.write_all(&data[pos..]).expect("write");
            assert_eq!(w.finish().expect("finish"), want, "{coder:?}");
        }
        let packed = streamed(
            Coder::Lzma2 {
                block_size: BLOCK_SIZE_SOLID,
                threads: 1,
            },
            &data,
        );
        assert_eq!(decode_lzma2(&packed, data.len()), data);
    }

    /// A side coder asked to run inline starts no thread, and writes the
    /// stream its threaded writer writes.
    #[test]
    fn an_inline_side_coder_writes_the_threaded_coders_stream() {
        let coder = SideCoder {
            level: 5,
            dict_size: 1 << 20,
            fast_bytes: 128,
            lc: 0,
            lp: 2,
            pb: 2,
        };
        for len in [0, 1, CHUNK - 1, 3 * CHUNK + 77] {
            let data = sample(len);
            let mut threaded = coder.writer(Vec::new()).expect("writer");
            threaded.write_all(&data).expect("write");
            let want = threaded.finish().expect("finish");

            let mut inline = coder.inline_writer(Vec::new()).expect("writer");
            assert!(matches!(inline.state, State::Pushed { .. }));
            for piece in data.chunks(1000) {
                inline.write_all(piece).expect("write");
            }
            assert_eq!(inline.finish().expect("finish"), want, "{len} bytes");
        }
    }

    /// Block threads cannot run without threads: the unthreaded path writes
    /// the solid stream the one-thread plan writes.
    #[test]
    fn the_unthreaded_path_ignores_the_block_plan() {
        let data = sample(3 * CHUNK);
        let want = streamed(
            Coder::Lzma2 {
                block_size: BLOCK_SIZE_SOLID,
                threads: 1,
            },
            &data,
        );
        let mut w = unthreaded(Coder::Lzma2 {
            block_size: CHUNK as u64,
            threads: 3,
        });
        w.write_all(&data).expect("write");
        assert_eq!(w.finish().expect("finish"), want);
    }

    /// The unthreaded path writes as it goes rather than holding the folder:
    /// compressed bytes reach the inner writer before `finish`, and what is
    /// still queued is under one LZMA2 chunk and a margin.
    #[test]
    fn the_unthreaded_path_streams() {
        let data = sample(16 * CHUNK);
        let mut w = unthreaded(Coder::Lzma2 {
            block_size: BLOCK_SIZE_SOLID,
            threads: 1,
        });
        let mut written_before_finish = 0;
        for piece in data.chunks(CHUNK) {
            w.write_all(piece).expect("write");
            written_before_finish = w.inner.as_ref().expect("open").len();
            let State::Pushed { out, .. } = &w.state else {
                unreachable!()
            };
            assert!(out.is_empty(), "every write hands its output on");
        }
        assert!(written_before_finish > 0);
        let packed = w.finish().expect("finish");
        assert!(packed.len() > written_before_finish);
        assert_eq!(decode_lzma2(&packed, data.len()), data);
    }

    #[test]
    fn the_unthreaded_path_handles_empty_input() {
        for coder in [
            Coder::Lzma2 {
                block_size: BLOCK_SIZE_SOLID,
                threads: 1,
            },
            Coder::Lzma,
        ] {
            assert_eq!(
                unthreaded(coder).finish().expect("finish"),
                streamed(coder, &[]),
                "{coder:?}"
            );
        }
    }

    fn pulled(pull: &mut PullCoder, props: &LzmaEncProps, coder: Coder, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        pull.encode(props, coder, &mut &data[..], &mut out)
            .expect("pull");
        out
    }

    /// One coder kept across streams of every length, both coders and a
    /// change of settings in between, writes each stream exactly as the
    /// writer with a fresh encoder on its own thread does.
    #[test]
    fn a_kept_pull_coder_writes_each_stream_as_a_fresh_writer() {
        let solid = Coder::Lzma2 {
            block_size: BLOCK_SIZE_SOLID,
            threads: 1,
        };
        let one_block = Coder::Lzma2 {
            block_size: CHUNK as u64,
            threads: 1,
        };
        let level5 = LzmaEncProps::new().with_level(5).with_dict_size(1 << 16);
        let mut pull = PullCoder::default();
        let streams: [(LzmaEncProps, Coder, usize); 10] = [
            (props(), solid, 2000),
            (props(), solid, 0),
            (props(), solid, 3 * CHUNK + 77),
            (props(), solid, 1),
            (level5, solid, 70_000),
            (level5, solid, 5000),
            (props(), Coder::Lzma, CHUNK + 999),
            (props(), Coder::Lzma, 300),
            (level5, one_block, CHUNK - 3),
            (props(), solid, 2000),
        ];
        for (i, (p, coder, len)) in streams.into_iter().enumerate() {
            assert!(PullCoder::drives(&p, coder));
            let data = sample(len + i);
            let mut fresh = LzmaTurboWriter::new(Vec::new(), &p, coder).expect("writer");
            fresh.write_all(&data).expect("write");
            let want = fresh.finish().expect("finish");
            assert_eq!(pulled(&mut pull, &p, coder, &data), want, "stream {i}");
        }
        let decoded = sample(5);
        let packed = pulled(&mut pull, &props(), solid, &decoded);
        assert_eq!(decode_lzma2(&packed, decoded.len()), decoded);
    }

    /// Block threads and the threaded match finder need threads of their own,
    /// so such a coder is not driven here.
    #[test]
    fn a_coder_on_more_than_one_thread_is_not_pulled() {
        assert!(!PullCoder::drives(
            &props(),
            Coder::Lzma2 {
                block_size: CHUNK as u64,
                threads: 2,
            }
        ));
        assert!(!PullCoder::drives(
            &LzmaEncProps::new().with_level(5).with_num_threads(2),
            Coder::Lzma
        ));
    }

    /// A reader's error comes back as it was, and the coder that saw it is
    /// not kept for the next stream.
    #[test]
    fn a_pull_coders_read_error_is_returned_and_the_coder_dropped() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "no",
                ))
            }
        }
        let coder = Coder::Lzma2 {
            block_size: BLOCK_SIZE_SOLID,
            threads: 1,
        };
        let mut pull = PullCoder::default();
        let err = pull
            .encode(&props(), coder, &mut Failing, &mut Vec::new())
            .expect_err("the read fails");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(pull.kept.is_none());
        let data = sample(4000);
        assert_eq!(
            pulled(&mut pull, &props(), coder, &data),
            streamed(coder, &data)
        );
    }

    #[test]
    fn a_bad_setting_is_refused_up_front() {
        let props = LzmaEncProps::new().with_lclppb(4, 4, 2);
        let err = LzmaTurboWriter::new(
            Vec::new(),
            &props,
            Coder::Lzma2 {
                block_size: BLOCK_SIZE_SOLID,
                threads: 1,
            },
        )
        .err()
        .expect("lc + lp above 4 is refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
