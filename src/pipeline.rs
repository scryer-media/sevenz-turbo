//! Coder stages of one block on threads of their own.
//!
//! A block's coders run as a chain of readers, each pulling from the one
//! below it, and on one thread that is one coder at a time: AES decrypting
//! while the LZMA coder above it waits, or the three LZMA coders of a BCJ2
//! graph taking turns. When the chain holds two or more coders that cost real
//! CPU, the ones below the top can each be given a thread, and the chain then
//! runs as a pipeline: every stage decodes its next piece while the stage
//! above it is still on the last one.
//!
//! The stages are joined by pipes of a few [`PIPE_CHUNK`]-sized pieces each.
//! A stage whose pipe is full waits until the stage above has taken a piece,
//! so nothing runs further ahead than [`PIPE_DEPTH`] pieces, whatever the
//! speeds. The pack streams themselves borrow the caller's reader and cannot
//! leave its thread, so the thread reading the block's output also tops up
//! the pipes of the stages that read a pack stream, each time it takes a piece
//! and whenever it has to wait.
//!
//! An error keeps its place in the stream: whatever a stage produced before it
//! is delivered first, and the error itself, unchanged, is what the stage
//! above then reads. The block's context is added where it always was, above
//! the whole chain, so an error reads the same whether its coder ran on a
//! thread of its own or not.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;

use crate::archive::EncoderMethod;
use crate::block::Block;
use crate::codec::lzma_turbo::MT_MIN_BLOCK_BYTES;
use crate::decoder::{DecodeOptions, INPUT_BUF_SIZE, add_decoder};
use crate::{Error, Password};

/// Bytes a stage hands on at a time.
pub(crate) const PIPE_CHUNK: usize = 256 << 10;

/// Pieces a pipe holds before the stage writing it waits.
pub(crate) const PIPE_DEPTH: usize = 4;

/// The most one pipe holds: its queued pieces and the one its reader is
/// working through. A piece being filled is one the reader handed back, or
/// one the queue has room for.
pub(crate) const PIPE_BYTES: usize = (PIPE_DEPTH + 1) * PIPE_CHUNK;

/// The least output a coder must have to be worth a thread: below this the
/// thread costs more to start than the coder takes to run.
pub(crate) const MIN_STAGE_BYTES: u64 = 1 << 20;

#[cfg(test)]
thread_local! {
    /// A lower [`MIN_STAGE_BYTES`] for this thread's decodes, so that the
    /// small archives the tests carry can be run as pipelines too.
    static MIN_STAGE_BYTES_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

fn min_stage_bytes() -> u64 {
    #[cfg(test)]
    if let Some(bytes) = MIN_STAGE_BYTES_OVERRIDE.with(std::cell::Cell::get) {
        return bytes;
    }
    MIN_STAGE_BYTES
}

/// Runs `f` with every coder of any size counted as worth a thread.
#[cfg(test)]
pub(crate) fn with_any_stage_size<T>(f: impl FnOnce() -> T) -> T {
    MIN_STAGE_BYTES_OVERRIDE.with(|cell| cell.set(Some(0)));
    let out = f();
    MIN_STAGE_BYTES_OVERRIDE.with(|cell| cell.set(None));
    out
}

#[cfg(test)]
thread_local! {
    /// How many more stage threads this thread's decodes may start, when a
    /// test is running a chain that cannot have every thread it planned.
    static STAGE_THREADS_LEFT: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Takes one of the stage threads a test allowed; `false` when none is left.
#[cfg(test)]
fn take_stage_thread() -> bool {
    STAGE_THREADS_LEFT.with(|cell| match cell.get() {
        None => true,
        Some(0) => false,
        Some(left) => {
            cell.set(Some(left - 1));
            true
        }
    })
}

/// Runs `f` with only `allowed` stage threads to be had: every one asked for
/// after those is refused, as a system out of threads refuses it.
#[cfg(test)]
pub(crate) fn with_stage_threads<T>(allowed: usize, f: impl FnOnce() -> T) -> T {
    STAGE_THREADS_LEFT.with(|cell| cell.set(Some(allowed)));
    let out = f();
    STAGE_THREADS_LEFT.with(|cell| cell.set(None));
    out
}

/// Whether a coder spends real CPU on every byte. The filters (BCJ, BCJ2's
/// combiner, Delta) and Copy run at memory speed and never are.
fn is_heavy(method: &[u8]) -> bool {
    [
        EncoderMethod::ID_LZMA,
        EncoderMethod::ID_LZMA2,
        EncoderMethod::ID_PPMD,
        EncoderMethod::ID_BZIP2,
        EncoderMethod::ID_ZSTD,
        EncoderMethod::ID_BROTLI,
        EncoderMethod::ID_LZ4,
        EncoderMethod::ID_DEFLATE,
        EncoderMethod::ID_DEFLATE64,
        EncoderMethod::ID_AES256_SHA256,
    ]
    .contains(&method)
}

/// Which of a block's coders, as a bit per coder index, decode on a thread of
/// their own when the block is given `threads`.
///
/// None of them unless at least two coders that cost real CPU would otherwise
/// share the caller's thread, and there is more than one thread to run them
/// on. None either in a chain holding an LZMA2 coder large enough to decode in
/// parallel: that coder's workers are already the whole of `threads`, and they
/// follow the reader's live ceiling, which a stage's thread would not. A stage
/// beside them would be a thread more than the caller asked for.
/// The top coder is never given a thread: the caller's thread runs it. Of the
/// rest, those with at least [`MIN_STAGE_BYTES`] of output are given the
/// threads beyond the caller's, largest first, each once every coder it reads
/// from is a pack stream or has a thread too.
pub(crate) fn offload_plan(block: &Block, threads: u32) -> u64 {
    let coders = &block.coders;
    let count = coders.len();
    if cfg!(target_arch = "wasm32") || threads < 2 || count > 64 {
        return 0;
    }
    let size = |index: usize| block.get_unpack_size_at_index(index);
    let parallel = |index: usize| {
        coders[index].encoder_method_id() == EncoderMethod::ID_LZMA2
            && size(index) > MT_MIN_BLOCK_BYTES
    };
    if (0..count).any(parallel) {
        return 0;
    }
    let heavy: Vec<bool> = (0..count)
        .map(|index| is_heavy(coders[index].encoder_method_id()))
        .collect();
    if heavy.iter().filter(|&&h| h).count() < 2 {
        return 0;
    }
    let bound = |index: usize| {
        block
            .bind_pairs
            .iter()
            .any(|bp| bp.out_index == index as u64)
    };
    let Some(top) = (0..count).find(|&index| !bound(index)) else {
        return 0;
    };
    let mut first_in = Vec::with_capacity(count);
    let mut stream = 0u64;
    for coder in coders {
        first_in.push(stream);
        stream = stream.saturating_add(coder.num_in_streams);
    }
    let candidate = |index: usize| {
        let coder = &coders[index];
        heavy[index]
            && index != top
            && coder.num_in_streams == 1
            && coder.num_out_streams == 1
            && size(index) >= min_stage_bytes()
    };
    // Whether a coder's one input is a pack stream or a coder already chosen.
    let fed = |index: usize, chosen: u64| {
        let stream = first_in[index];
        if block.packed_streams.contains(&stream) {
            return true;
        }
        block
            .find_bind_pair_for_in_stream(stream)
            .is_some_and(|bp| bp.out_index < 64 && chosen & (1 << bp.out_index) != 0)
    };
    let mut chosen = 0u64;
    for _ in 1..threads {
        let next = (0..count)
            .filter(|&index| chosen & (1 << index) == 0 && candidate(index) && fed(index, chosen))
            .max_by_key(|&index| size(index));
        match next {
            Some(index) => chosen |= 1 << index,
            None => break,
        }
    }
    chosen
}

/// Pipes a plan opens: one for the output of each coder given a thread, and
/// one more for each of those that reads a pack stream, which the caller's
/// thread fills.
fn plan_pipes(block: &Block, offload: u64) -> usize {
    let mut stream = 0u64;
    let mut pipes = 0;
    for (index, coder) in block.coders.iter().enumerate() {
        // A coder given a thread has one input: see `offload_plan`.
        if index < 64 && offload & (1 << index) != 0 {
            pipes += 1 + usize::from(block.packed_streams.contains(&stream));
        }
        stream = stream.saturating_add(coder.num_in_streams);
    }
    pipes
}

/// Where a coder's input comes from while a chain is being assembled.
pub(crate) enum Stage<'r> {
    /// A pack stream, read on the caller's thread.
    Leaf(Box<dyn Read + 'r>),
    /// A coder on the caller's thread.
    Here(Box<dyn Read + 'r>),
    /// The pipe a coder on a thread of its own writes.
    There(usize),
}

/// How a pipe ends, once the stage writing it has stopped.
enum End {
    Open,
    Done,
    Failed(io::Error),
    /// The error has been handed on; a read after it gets another.
    Reported,
}

struct PipeState {
    chunks: VecDeque<Vec<u8>>,
    /// Emptied pieces, handed back for the writer to fill again.
    spare: Vec<Vec<u8>>,
    end: End,
}

impl PipeState {
    fn has_room(&self) -> bool {
        self.chunks.len() < PIPE_DEPTH
    }

    fn spare(&mut self) -> Vec<u8> {
        self.spare
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(PIPE_CHUNK))
    }

    /// Hands on a filled piece and, when the writer has stopped, how.
    fn send(&mut self, chunk: Vec<u8>, outcome: io::Result<bool>) {
        if !chunk.is_empty() {
            self.chunks.push_back(chunk);
        } else if self.spare.len() < PIPE_DEPTH {
            self.spare.push(chunk);
        }
        match outcome {
            Ok(true) => {}
            Ok(false) => self.end = End::Done,
            Err(error) => self.end = End::Failed(error),
        }
    }

    /// Swaps the next piece into `buf`; `Ok(false)` at the end of the stream,
    /// `None` when the writer has more to come but has not sent it yet. At
    /// the end, or at an error, `buf` is left empty: the piece in it has been
    /// read, and a read after the end must not find it again.
    fn take(&mut self, buf: &mut Vec<u8>) -> Option<io::Result<bool>> {
        if let Some(chunk) = self.chunks.pop_front() {
            let mut used = std::mem::replace(buf, chunk);
            if used.capacity() > 0 && self.spare.len() < PIPE_DEPTH {
                used.clear();
                self.spare.push(used);
            }
            return Some(Ok(true));
        }
        if !matches!(self.end, End::Open) {
            buf.clear();
        }
        match std::mem::replace(&mut self.end, End::Reported) {
            End::Open => {
                self.end = End::Open;
                None
            }
            End::Done => {
                self.end = End::Done;
                Some(Ok(false))
            }
            End::Failed(error) => Some(Err(error)),
            End::Reported => Some(Err(io::Error::other("a coder stage already failed"))),
        }
    }
}

struct State {
    pipes: Vec<PipeState>,
    /// The block's reader is gone: every stage stops at its next wait.
    abandoned: bool,
    /// Bumped on every change, for a waiter that has to know whether
    /// anything happened while it was not holding the lock.
    generation: u64,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self, state: &mut State) {
        state.generation = state.generation.wrapping_add(1);
        self.changed.notify_all();
    }
}

fn abandoned() -> io::Error {
    io::Error::other("the block's reader was dropped")
}

/// Fills `chunk` to [`PIPE_CHUNK`] from `reader`: `Ok(true)` when full,
/// `Ok(false)` at the end of the stream. What was read before an error stays
/// in `chunk`.
fn fill(reader: &mut dyn Read, chunk: &mut Vec<u8>) -> io::Result<bool> {
    chunk.resize(PIPE_CHUNK, 0);
    let mut len = 0;
    let outcome = loop {
        if len == PIPE_CHUNK {
            break Ok(true);
        }
        match reader.read(&mut chunk[len..]) {
            Ok(0) => break Ok(false),
            Ok(n) => len += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => break Err(error),
        }
    };
    chunk.truncate(len);
    outcome
}

/// Copies what is left of `buf` from `pos` into `out`.
fn copy_out(buf: &[u8], pos: &mut usize, out: &mut [u8]) -> usize {
    let n = out.len().min(buf.len() - *pos);
    out[..n].copy_from_slice(&buf[*pos..*pos + n]);
    *pos += n;
    n
}

/// A stage's thread: decodes into its pipe until the coder ends, fails, or
/// the block is abandoned.
fn run_stage(shared: &Shared, pipe: usize, coder: &mut dyn Read) {
    loop {
        let mut chunk = {
            let mut state = shared.lock();
            loop {
                if state.abandoned {
                    return;
                }
                if state.pipes[pipe].has_room() {
                    break state.pipes[pipe].spare();
                }
                state = shared.wait(state);
            }
        };
        let outcome = fill(coder, &mut chunk);
        let more = matches!(outcome, Ok(true));
        let mut state = shared.lock();
        state.pipes[pipe].send(chunk, outcome);
        shared.changed(&mut state);
        if !more {
            return;
        }
    }
}

/// What a stage on a thread of its own reads: another stage's pipe, or a pack
/// stream's that the caller's thread fills.
struct StageInput {
    shared: Arc<Shared>,
    pipe: usize,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for StageInput {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos == self.buf.len() {
            let mut state = self.shared.lock();
            loop {
                if state.abandoned {
                    return Err(abandoned());
                }
                if let Some(outcome) = state.pipes[self.pipe].take(&mut self.buf) {
                    self.shared.changed(&mut state);
                    self.pos = 0;
                    if !outcome? {
                        return Ok(0);
                    }
                    break;
                }
                state = self.shared.wait(state);
            }
        }
        Ok(copy_out(&self.buf, &mut self.pos, out))
    }
}

/// A pack stream the caller's thread reads into a pipe for a stage.
struct Feed<'r> {
    source: Box<dyn Read + 'r>,
    pipe: usize,
    done: bool,
}

struct Inner<'r> {
    shared: Arc<Shared>,
    feeds: RefCell<Vec<Feed<'r>>>,
    workers: RefCell<Vec<JoinHandle<()>>>,
}

impl Inner<'_> {
    /// Tops up every pack stream's pipe; whether anything was read.
    fn pump(&self) -> bool {
        let mut feeds = self.feeds.borrow_mut();
        let mut progressed = false;
        for feed in feeds.iter_mut().filter(|feed| !feed.done) {
            while !feed.done {
                let mut chunk = {
                    let mut state = self.shared.lock();
                    let pipe = &mut state.pipes[feed.pipe];
                    if !pipe.has_room() {
                        break;
                    }
                    pipe.spare()
                };
                let outcome = fill(&mut *feed.source, &mut chunk);
                feed.done = !matches!(outcome, Ok(true));
                let mut state = self.shared.lock();
                state.pipes[feed.pipe].send(chunk, outcome);
                self.shared.changed(&mut state);
                progressed = true;
            }
        }
        progressed
    }
}

impl Drop for Inner<'_> {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.abandoned = true;
            self.shared.changed(&mut state);
        }
        for worker in self.workers.get_mut().drain(..) {
            let _ = worker.join();
        }
    }
}

/// What the caller's thread reads from a stage's pipe.
struct CallerInput<'r> {
    inner: Rc<Inner<'r>>,
    pipe: usize,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for CallerInput<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos == self.buf.len() {
            let shared = &self.inner.shared;
            loop {
                let seen = {
                    let mut state = shared.lock();
                    if let Some(outcome) = state.pipes[self.pipe].take(&mut self.buf) {
                        shared.changed(&mut state);
                        drop(state);
                        self.pos = 0;
                        // The stages below keep going on what this tops up
                        // while this thread works through the piece.
                        self.inner.pump();
                        if !outcome? {
                            return Ok(0);
                        }
                        break;
                    }
                    state.generation
                };
                if self.inner.pump() {
                    continue;
                }
                let mut state = shared.lock();
                while state.generation == seen {
                    state = shared.wait(state);
                }
            }
        }
        Ok(copy_out(&self.buf, &mut self.pos, out))
    }
}

/// The stages of one block's chain that have threads of their own.
///
/// Built empty, and costs nothing until a coder is given a thread: a chain
/// with none is the same chain of readers as without this.
#[derive(Default)]
pub(crate) struct Pipeline<'r> {
    inner: Option<Rc<Inner<'r>>>,
}

impl<'r> Pipeline<'r> {
    fn inner(&mut self) -> &Rc<Inner<'r>> {
        self.inner.get_or_insert_with(|| {
            Rc::new(Inner {
                shared: Arc::new(Shared {
                    state: Mutex::new(State {
                        pipes: Vec::new(),
                        abandoned: false,
                        generation: 0,
                    }),
                    changed: Condvar::new(),
                }),
                feeds: RefCell::new(Vec::new()),
                workers: RefCell::new(Vec::new()),
            })
        })
    }

    fn new_pipe(&mut self) -> usize {
        let mut state = self.inner().shared.lock();
        state.pipes.push(PipeState {
            chunks: VecDeque::with_capacity(PIPE_DEPTH),
            spare: Vec::new(),
            end: End::Open,
        });
        state.pipes.len() - 1
    }

    /// The input for a coder on the caller's thread.
    pub(crate) fn here(&mut self, stage: Stage<'r>) -> Box<dyn Read + 'r> {
        match stage {
            Stage::Leaf(reader) | Stage::Here(reader) => reader,
            Stage::There(pipe) => Box::new(CallerInput {
                inner: Rc::clone(self.inner()),
                pipe,
                buf: Vec::new(),
                pos: 0,
            }),
        }
    }

    /// The input for a coder on a thread of its own. A coder on the caller's
    /// thread cannot be one: [`offload_plan`] never asks for that.
    pub(crate) fn there(&mut self, stage: Stage<'r>) -> io::Result<Box<dyn Read + Send>> {
        let pipe = match stage {
            Stage::There(pipe) => pipe,
            Stage::Leaf(source) => {
                let pipe = self.new_pipe();
                self.inner().feeds.borrow_mut().push(Feed {
                    source,
                    pipe,
                    done: false,
                });
                pipe
            }
            Stage::Here(_) => {
                return Err(io::Error::other(
                    "a coder stage cannot read a coder on the caller's thread",
                ));
            }
        };
        Ok(Box::new(StageInput {
            shared: Arc::clone(&self.inner().shared),
            pipe,
            buf: Vec::new(),
            pos: 0,
        }))
    }

    /// Starts a thread for a coder that is still to be built.
    ///
    /// The thread comes first because building the coder takes its input,
    /// and a pack stream given to a stage cannot be taken back: a thread that
    /// cannot be had has to be known while the coder can still be built on
    /// the caller's.
    ///
    /// # Errors
    ///
    /// Whatever the system refused the thread with.
    pub(crate) fn reserve(&mut self) -> io::Result<Reserved> {
        #[cfg(test)]
        if !take_stage_thread() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let (coder, waiting) = mpsc::channel::<(usize, Box<dyn Read + Send>)>();
        let shared = Arc::clone(&self.inner().shared);
        let worker = std::thread::Builder::new()
            .name("sevenz-coder".into())
            .spawn(move || {
                // No coder: its chain failed to build, and is gone.
                let Ok((pipe, mut coder)) = waiting.recv() else {
                    return;
                };
                let ran = catch_unwind(AssertUnwindSafe(|| run_stage(&shared, pipe, &mut *coder)));
                if ran.is_err() {
                    let mut state = shared.lock();
                    state.pipes[pipe].end = End::Failed(io::Error::other("a coder stage panicked"));
                    shared.changed(&mut state);
                }
            })?;
        self.inner().workers.borrow_mut().push(worker);
        Ok(Reserved { coder })
    }

    /// Hands `coder` to the thread reserved for it.
    pub(crate) fn start(&mut self, thread: Reserved, coder: Box<dyn Read + Send>) -> Stage<'r> {
        let pipe = self.new_pipe();
        if thread.coder.send((pipe, coder)).is_err() {
            // The thread waits for exactly this and cannot have gone. Were
            // it gone all the same, the pipe's reader is told so and is not
            // left waiting for a writer.
            let shared = &self.inner().shared;
            let mut state = shared.lock();
            state.pipes[pipe].end = End::Failed(io::Error::other("a coder stage did not start"));
            shared.changed(&mut state);
        }
        Stage::There(pipe)
    }

    /// Starts `coder` on a thread of its own.
    #[cfg(test)]
    fn spawn(&mut self, coder: Box<dyn Read + Send>) -> io::Result<Stage<'r>> {
        let thread = self.reserve()?;
        Ok(self.start(thread, coder))
    }
}

/// A thread waiting for the coder it will run. Dropped without one, it ends.
pub(crate) struct Reserved {
    coder: mpsc::Sender<(usize, Box<dyn Read + Send>)>,
}

/// One block's chain of coders as it is assembled, bottom up: each coder is
/// built on the caller's thread or given one of its own, as
/// [`offload_plan`] decided for the block.
pub(crate) struct Chain<'r> {
    pub(crate) pipeline: Pipeline<'r>,
    offload: u64,
}

impl<'r> Chain<'r> {
    /// Plans `block`'s chain for `opts.threads`.
    ///
    /// The pipes of a plan are memory the chain holds beyond its coders'.
    /// They are added to `opts.reserved_kb`, where the coders' own share
    /// already is, and a plan whose pipes do not fit under
    /// `memory_limit_bytes` beside the coders is given up: the block then
    /// decodes on the caller's thread, as it would with one thread.
    pub(crate) fn new(block: &Block, opts: &mut DecodeOptions<'_>) -> Self {
        let mut offload = offload_plan(block, opts.threads);
        if offload != 0 {
            let pipes_kb = plan_pipes(block, offload)
                .saturating_mul(PIPE_BYTES)
                .div_ceil(1024);
            let chain_kb = opts.reserved_kb.saturating_add(pipes_kb);
            if chain_kb > opts.limits.memory_limit_kb() {
                offload = 0;
            } else {
                opts.reserved_kb = chain_kb;
            }
        }
        Self {
            pipeline: Pipeline::default(),
            offload,
        }
    }

    /// Builds the coder at `index` of `block` over `input`.
    pub(crate) fn add(
        &mut self,
        block: &Block,
        input: Stage<'r>,
        index: usize,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<Stage<'r>, Error> {
        let coder = &block.coders[index];
        let len = block.get_unpack_size_at_index(index) as usize;
        if index < 64 && self.offload & (1 << index) != 0 {
            match self.pipeline.reserve() {
                Ok(thread) => {
                    let input = self.pipeline.there(input)?;
                    let decoder = add_decoder(input, len, coder, password, opts)?;
                    return Ok(self.pipeline.start(thread, Box::new(decoder)));
                }
                // No thread to be had. This coder and every one after it is
                // built on the caller's thread, reading the stages already
                // started through their pipes: slower, and the same bytes.
                Err(_) => self.offload = 0,
            }
        }
        // A pack stream read on this thread is buffered, for a coder that
        // reads it in small pieces: see `INPUT_BUF_SIZE`.
        let input: Box<dyn Read + 'r> = match input {
            Stage::Leaf(source) => Box::new(io::BufReader::with_capacity(INPUT_BUF_SIZE, source)),
            stage => self.pipeline.here(stage),
        };
        Ok(Stage::Here(Box::new(add_decoder(
            input, len, coder, password, opts,
        )?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 300) as u8).collect()
    }

    /// Bytes through a stage that only copies come out as they went in.
    #[test]
    fn a_copying_stage_passes_its_input_through() {
        let input = data(10 << 20 | 12345);
        let mut pipeline = Pipeline::default();
        let leaf = Stage::Leaf(Box::new(io::Cursor::new(input.clone())));
        let stage_input = pipeline.there(leaf).unwrap();
        let stage = pipeline.spawn(stage_input).unwrap();
        let second = pipeline.there(stage).unwrap();
        let stage = pipeline.spawn(second).unwrap();
        let mut out = Vec::new();
        let mut reader = pipeline.here(stage);
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), input.len());
        assert!(out == input);
        // The end stays the end: the last piece is not read a second time.
        let mut more = [0u8; 64];
        assert_eq!(reader.read(&mut more).unwrap(), 0);
        assert_eq!(reader.read(&mut more).unwrap(), 0);
    }

    /// A reader that hands out `good` bytes and then fails.
    struct FailsAfter {
        good: usize,
    }

    impl Read for FailsAfter {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.good == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "broken coder"));
            }
            let n = out.len().min(self.good).min(1000);
            out[..n].fill(7);
            self.good -= n;
            Ok(n)
        }
    }

    /// What a stage produced before it failed arrives first, and then its
    /// error, unchanged, in the reader above it.
    #[test]
    fn a_failing_stage_delivers_what_it_made_and_then_its_error() {
        let good = 3 * PIPE_CHUNK + 777;
        let mut pipeline = Pipeline::default();
        let stage = pipeline.spawn(Box::new(FailsAfter { good })).unwrap();
        let second = pipeline.there(stage).unwrap();
        let stage = pipeline.spawn(second).unwrap();
        let mut reader = pipeline.here(stage);
        let mut out = Vec::new();
        let error = reader.read_to_end(&mut out).unwrap_err();
        assert_eq!(out.len(), good);
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "broken coder");
        // Reading on after the error is still an error, never a clean end.
        assert!(reader.read(&mut [0u8; 8]).is_err());
    }

    /// A reader dropped half way stops its stages, whatever they were doing.
    #[test]
    fn a_reader_dropped_half_way_stops_its_stages() {
        let input = data(8 << 20);
        let mut pipeline = Pipeline::default();
        let leaf = Stage::Leaf(Box::new(io::Cursor::new(input)));
        let stage_input = pipeline.there(leaf).unwrap();
        let stage = pipeline.spawn(stage_input).unwrap();
        let mut reader = pipeline.here(stage);
        let mut some = vec![0u8; 1000];
        reader.read_exact(&mut some).unwrap();
        // Joins the stage, which is waiting for room in a full pipe.
        drop(reader);
        drop(pipeline);
    }

    fn coder(method: &[u8], inputs: u64) -> crate::block::Coder {
        let mut coder = crate::block::Coder::default();
        coder.id_size = method.len();
        coder.num_in_streams = inputs;
        coder.num_out_streams = 1;
        coder.decompression_method_id_mut().copy_from_slice(method);
        coder
    }

    fn bind(in_index: u64, out_index: u64) -> crate::block::BindPair {
        crate::block::BindPair {
            in_index,
            out_index,
        }
    }

    /// AES (coder 1) under LZMA2 (coder 0), as 7-Zip writes it, with the
    /// LZMA2 coder of `lzma2` bytes over `aes` bytes of ciphertext.
    fn aes_under(method: &[u8], unpacked: u64, packed: u64) -> Block {
        Block {
            coders: vec![coder(method, 1), coder(EncoderMethod::ID_AES256_SHA256, 1)],
            total_input_streams: 2,
            total_output_streams: 2,
            bind_pairs: vec![bind(0, 1)],
            packed_streams: vec![1],
            unpack_sizes: vec![unpacked, packed],
            ..Default::default()
        }
    }

    /// A BCJ2 graph as 7-Zip writes it: BCJ2 (coder 0) over a main-stream
    /// coder (1) and two LZMA coders for the call and jump streams (2, 3),
    /// with the range-coder stream straight off the pack streams.
    fn bcj2(main: &[u8], main_len: u64, side_len: u64) -> Block {
        Block {
            coders: vec![
                coder(EncoderMethod::ID_BCJ2, 4),
                coder(main, 1),
                coder(EncoderMethod::ID_LZMA, 1),
                coder(EncoderMethod::ID_LZMA, 1),
            ],
            total_input_streams: 7,
            total_output_streams: 4,
            bind_pairs: vec![bind(0, 1), bind(1, 2), bind(2, 3)],
            packed_streams: vec![4, 5, 6, 3],
            unpack_sizes: vec![main_len + 2 * side_len, main_len, side_len, side_len],
            ..Default::default()
        }
    }

    /// The decisions, for the chains 7-Zip writes.
    #[test]
    fn the_plan_gives_threads_only_to_coders_sharing_the_callers_thread() {
        const MIB: u64 = 1 << 20;
        // One thread: always the sequential chain.
        assert_eq!(
            offload_plan(&aes_under(EncoderMethod::ID_LZMA, 64 * MIB, 32 * MIB), 1),
            0
        );
        // AES under LZMA: the cipher gets a thread, LZMA stays on top.
        assert_eq!(
            offload_plan(&aes_under(EncoderMethod::ID_LZMA, 64 * MIB, 32 * MIB), 2),
            0b10
        );
        // AES under an LZMA2 coder that decodes in parallel: its workers are
        // the whole thread count, and the cipher stays on the caller's.
        assert_eq!(
            offload_plan(&aes_under(EncoderMethod::ID_LZMA2, 64 * MIB, 32 * MIB), 8),
            0
        );
        // An LZMA2 coder of one run decodes on the caller's thread, so the
        // cipher under it is worth a thread.
        assert_eq!(
            offload_plan(&aes_under(EncoderMethod::ID_LZMA2, MIB, MIB), 2),
            0b10
        );
        // Too little ciphertext to be worth a thread.
        assert_eq!(
            offload_plan(&aes_under(EncoderMethod::ID_LZMA, 64 * MIB, MIB / 2), 4),
            0
        );

        // BCJ2 over an LZMA2 coder that decodes in parallel: no stage gets a
        // thread, whatever the call and jump streams' sizes. The main coder's
        // workers are already every thread the caller gave.
        assert_eq!(
            offload_plan(&bcj2(EncoderMethod::ID_LZMA2, 64 * MIB, 2 * MIB), 8),
            0
        );
        assert_eq!(
            offload_plan(&bcj2(EncoderMethod::ID_LZMA2, 64 * MIB, 2 * MIB), 2),
            0
        );
        // BCJ2 over an LZMA2 coder of one run, which decodes on the caller's
        // thread: all three coders, as over LZMA.
        assert_eq!(
            offload_plan(&bcj2(EncoderMethod::ID_LZMA2, MIB, 2 * MIB), 8),
            0b1110
        );
        // BCJ2 over LZMA: all three, as the threads allow, largest first.
        assert_eq!(
            offload_plan(&bcj2(EncoderMethod::ID_LZMA, 64 * MIB, 2 * MIB), 8),
            0b1110
        );
        assert_eq!(
            offload_plan(&bcj2(EncoderMethod::ID_LZMA, 64 * MIB, 2 * MIB), 2),
            0b0010
        );
        assert_eq!(
            offload_plan(&bcj2(EncoderMethod::ID_LZMA, 64 * MIB, 2 * MIB), 3).count_ones(),
            2
        );
        // Call and jump streams too small for a thread: only the main coder
        // is given one.
        assert_eq!(
            offload_plan(&bcj2(EncoderMethod::ID_LZMA, 64 * MIB, MIB / 4), 8),
            0b0010
        );
    }

    /// The pipes of a plan are charged to the memory limit with the coders,
    /// and a plan they do not fit beside is given up for the caller's thread.
    #[test]
    fn a_plan_whose_pipes_do_not_fit_the_memory_limit_is_given_up() {
        const MIB: u64 = 1 << 20;
        let pipe_kb = PIPE_BYTES / 1024;
        // What the chain's coders were already granted.
        let coders_kb = 4096;
        let plan = |block: &Block, threads: u32, limit_kb: Option<usize>| {
            let limits = crate::ArchiveLimits {
                memory_limit_bytes: limit_kb.map_or(u64::MAX, |kb| kb as u64 * 1024),
                ..crate::ArchiveLimits::default()
            };
            let mut opts = DecodeOptions::header(&limits);
            opts.threads = threads;
            opts.reserved_kb = coders_kb;
            let chain = Chain::new(block, &mut opts);
            (chain.offload, opts.reserved_kb)
        };

        // AES under LZMA at two threads: the cipher's own pipe, and the pack
        // stream's that the caller's thread fills for it.
        let aes = aes_under(EncoderMethod::ID_LZMA, 64 * MIB, 32 * MIB);
        assert_eq!(plan_pipes(&aes, 0b10), 2);
        assert_eq!(
            plan(&aes, 2, Some(coders_kb + 2 * pipe_kb)),
            (0b10, coders_kb + 2 * pipe_kb)
        );
        assert_eq!(
            plan(&aes, 2, Some(coders_kb + 2 * pipe_kb - 1)),
            (0, coders_kb)
        );
        // Without a limit the plan stands, and what it holds is still said.
        assert_eq!(plan(&aes, 2, None), (0b10, coders_kb + 2 * pipe_kb));
        // One thread plans nothing and reserves nothing.
        assert_eq!(plan(&aes, 1, Some(coders_kb)), (0, coders_kb));

        // BCJ2 over LZMA at eight threads: three coders, each off a pack
        // stream of its own.
        let graph = bcj2(EncoderMethod::ID_LZMA, 64 * MIB, 2 * MIB);
        assert_eq!(plan_pipes(&graph, 0b1110), 6);
        assert_eq!(
            plan(&graph, 8, Some(coders_kb + 6 * pipe_kb)),
            (0b1110, coders_kb + 6 * pipe_kb)
        );
        assert_eq!(
            plan(&graph, 8, Some(coders_kb + 6 * pipe_kb - 1)),
            (0, coders_kb)
        );
    }

    /// Every entry of an archive, in order, read at `threads`.
    fn decode_all(archive: &[u8], password: &str, threads: u32) -> Vec<(String, Vec<u8>)> {
        let mut reader =
            crate::ArchiveReader::new(io::Cursor::new(archive), password.into()).unwrap();
        reader.set_threads(threads);
        let mut out = Vec::new();
        reader
            .for_each_entries(|entry, data| {
                let mut bytes = Vec::new();
                data.read_to_end(&mut bytes)?;
                out.push((entry.name().to_string(), bytes));
                Ok(true)
            })
            .unwrap();
        out
    }

    /// Whether any block of the archive runs a coder on a thread of its own
    /// at `threads`.
    fn pipelined(archive: &[u8], password: &str, threads: u32) -> bool {
        let archive =
            crate::Archive::read(&mut io::Cursor::new(archive), &password.into()).unwrap();
        archive
            .blocks
            .iter()
            .any(|block| offload_plan(block, threads) != 0)
    }

    fn fixtures() -> Vec<(&'static str, &'static [u8], &'static str)> {
        // Only the codec features below add to the list, so with none of them
        // it is never changed.
        #[allow(unused_mut)]
        let mut fixtures: Vec<(&'static str, &'static [u8], &'static str)> = vec![
            (
                "lzma2_bcj2",
                include_bytes!("../tests/resources/7za433_7zip_lzma2_bcj2.7z"),
                "",
            ),
            (
                "delta_bcj2",
                include_bytes!("../tests/resources/delta_bcj2.7z"),
                "",
            ),
            (
                "lzma2_bcj_x86",
                include_bytes!("../tests/resources/decompress_example_lzma2_bcj_x86.7z"),
                "",
            ),
            (
                "bcj_arm64",
                include_bytes!("../tests/resources/decompress_example_bcj_arm64.7z"),
                "",
            ),
            ("delta", include_bytes!("../tests/resources/delta.7z"), ""),
            ("solid", include_bytes!("../tests/resources/solid.7z"), ""),
            (
                "non_solid",
                include_bytes!("../tests/resources/non_solid.7z"),
                "",
            ),
        ];
        #[cfg(feature = "aes256")]
        fixtures.extend([
            (
                "aes",
                include_bytes!("../tests/resources/encrypted.7z").as_slice(),
                "sevenz-rust",
            ),
            (
                "aes_small",
                include_bytes!("../tests/resources/aes_small_test.7z").as_slice(),
                "iBlm8NTigvru0Jr0",
            ),
        ]);
        #[cfg(feature = "ppmd")]
        fixtures.push(("ppmd", include_bytes!("../tests/resources/ppmd.7z"), ""));
        #[cfg(feature = "bzip2")]
        fixtures.push((
            "bzip2",
            include_bytes!("../tests/resources/bzip2_file.7z"),
            "",
        ));
        #[cfg(all(feature = "zstd", feature = "brotli"))]
        fixtures.push((
            "zstd_brotli",
            include_bytes!("../tests/resources/zstdmt-brotli.7z"),
            "",
        ));
        #[cfg(all(feature = "zstd", feature = "lz4"))]
        fixtures.push((
            "zstd_lz4",
            include_bytes!("../tests/resources/zstdmt-lz4.7z"),
            "",
        ));
        fixtures
    }

    /// Every chain the fixtures carry decodes to the same bytes with its
    /// coders on threads of their own as on the caller's thread alone.
    #[test]
    fn every_fixture_chain_decodes_the_same_as_a_pipeline() {
        for (name, archive, password) in fixtures() {
            let alone = decode_all(archive, password, 1);
            for threads in [2, 3, 4, 8] {
                let piped = with_any_stage_size(|| decode_all(archive, password, threads));
                assert!(piped == alone, "{name} at {threads} threads");
            }
        }
    }

    /// A stage thread the system refuses is not an error. The coder it was
    /// for and those after it are built on the caller's thread, beside the
    /// stages that did start, and the bytes are the same however many
    /// threads were to be had.
    #[test]
    fn a_chain_refused_its_threads_decodes_on_the_callers() {
        for (name, archive, password) in fixtures() {
            let alone = decode_all(archive, password, 1);
            for allowed in 0..3 {
                let short = with_stage_threads(allowed, || {
                    with_any_stage_size(|| decode_all(archive, password, 8))
                });
                assert!(short == alone, "{name} with {allowed} stage threads");
            }
        }
    }

    /// The refusal is the system's own error, and it comes before the coder
    /// is built: the pack stream is still the chain's to read.
    #[test]
    fn a_refused_stage_thread_is_reported_before_the_coder_is_built() {
        let mut pipeline = Pipeline::default();
        let refused = with_stage_threads(0, || pipeline.reserve());
        assert!(refused.is_err());
        // A thread reserved and never given a coder ends on its own, and the
        // pipeline it belonged to still joins it.
        let unused = pipeline.reserve().unwrap();
        drop(unused);
        drop(pipeline);
    }

    /// The chains with two coders that cost CPU are the ones that run as a
    /// pipeline, so the test above covers the pipeline and not just the
    /// sequential path.
    #[test]
    fn the_chains_with_two_heavy_coders_are_pipelined() {
        for (name, archive, password) in fixtures() {
            let heavy_pair = matches!(name, "lzma2_bcj2" | "aes" | "aes_small");
            assert_eq!(
                with_any_stage_size(|| pipelined(archive, password, 4)),
                heavy_pair,
                "{name}"
            );
            // One thread is always the sequential chain.
            assert!(
                !with_any_stage_size(|| pipelined(archive, password, 1)),
                "{name}"
            );
        }
    }
    /// A checksum mismatch in a block whose coders ran as a pipeline is the
    /// same located error as without one, naming the block it was in.
    #[cfg(all(feature = "compress", feature = "aes256"))]
    #[test]
    fn a_pipelined_block_reports_its_own_checksum_mismatch() {
        use crate::encoder_options::AesEncoderOptions;
        use crate::{ArchiveEntry, ArchiveWriter, BlockDecoder};

        let files: Vec<Vec<u8>> = (0..3).map(|i| data((300 << 10) + i * 4099)).collect();
        let mut writer = ArchiveWriter::new(io::Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(vec![
            AesEncoderOptions::new("pw".into()).into(),
            EncoderMethod::LZMA.into(),
        ]);
        for (i, file) in files.iter().enumerate() {
            writer
                .push_archive_entry(
                    ArchiveEntry::new_file(&format!("f{i}")),
                    Some(io::Cursor::new(file.as_slice())),
                )
                .unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let password: Password = "pw".into();
        let mut source = io::Cursor::new(bytes);
        let mut archive = crate::Archive::read(&mut source, &password).unwrap();
        assert_eq!(archive.blocks.len(), 3);
        archive.files[1].crc ^= 1;

        let mut decode = |block_index: usize, threads: u32| {
            BlockDecoder::new(threads, block_index, &archive, &password, &mut source)
                .for_each_entries(&mut |_, reader| {
                    let mut sink = Vec::new();
                    reader.read_to_end(&mut sink)?;
                    Ok(true)
                })
                .map(|_| ())
        };
        for block_index in 0..3 {
            assert_ne!(
                with_any_stage_size(|| offload_plan(&archive.blocks[block_index], 4)),
                0
            );
            let piped = with_any_stage_size(|| decode(block_index, 4));
            let alone = decode(block_index, 1);
            if block_index != 1 {
                piped.unwrap();
                alone.unwrap();
                continue;
            }
            // Under AES a mismatch may be the wrong password, and says so;
            // either way it is block 1's, and the same with or without the
            // pipeline.
            let piped = piped.unwrap_err();
            assert!(
                matches!(piped, Error::BlockDecode { block_index: 1, .. }),
                "{piped:?}"
            );
            assert!(format!("{piped:?}").contains("ChecksumVerificationFailed"));
            assert_eq!(format!("{piped:?}"), format!("{:?}", alone.unwrap_err()));
        }
    }
}
