//! The one place in this crate that names `lzma_turbo`.
//!
//! Upstream `sevenz-rust2` decodes the LZMA (`03 01 01`) and LZMA2 (`21`)
//! coders with `lzma-rust2`. This fork decodes them with
//! [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo), a port of Igor
//! Pavlov's reference decoder. Everything that swap needs is behind this
//! module: adopting `lzma-turbo`'s multi-threaded LZMA2 decoder was a change to
//! this file and to how the reader hands it a thread count, and to nothing
//! else. What is still asked of that crate is in `docs/lzma-turbo-requests.md`;
//! the seam is [`Lzma2Plan`] below.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lzma_turbo::crc::CrcFolder;
use lzma_turbo::{
    Checksum, ChecksumPlan, DrainStatus, Lzma2AdaptiveDecoder, Lzma2MtOptions, Lzma2Reader,
    Lzma2RunScanner, LzmaProps, LzmaReader,
};

use crate::error::Error;

/// The `Write` fronts over `lzma-turbo`'s encoders, which the coder chain
/// in `crate::encoder` builds unless the build chose `lzma-rust2`'s.
#[cfg(all(feature = "compress", not(feature = "lzma-rust2-encoder")))]
pub(crate) mod writer;

/// Bytes an LZMA or LZMA2 decoder holds beyond its dictionary: the range
/// decoder's input buffer, the probability tables and the chunk buffer. The
/// reference decoder's own accounting is tens of kilobytes; a megabyte covers
/// it with room, and `crate::limits` documents the whole table.
pub(crate) const LZ_STATE_BYTES: u64 = 1 << 20;

/// Decodes the dictionary size an LZMA coder's five property bytes declare.
///
/// The properties are `lc/lp/pb` packed into byte 0, then the dictionary size
/// as a little-endian `u32`.
pub(crate) fn lzma_dictionary_size(properties: &[u8]) -> Result<u32, Error> {
    if properties.len() < 5 {
        return Err(Error::other("LZMA properties too short"));
    }
    Ok(u32::from_le_bytes([
        properties[1],
        properties[2],
        properties[3],
        properties[4],
    ]))
}

/// Decodes the dictionary size an LZMA2 coder's single property byte declares.
///
/// Values up to 39 map to `(2 | (p & 1)) << (p / 2 + 11)`; 40 is the 4 GiB
/// maximum; anything else, or any reserved bit, is a malformed archive.
pub(crate) fn lzma2_dictionary_size(properties: &[u8]) -> Result<u32, Error> {
    let Some(&bits) = properties.first() else {
        return Err(Error::other("LZMA2 properties too short"));
    };
    let bits = u32::from(bits);
    if (bits & !0x3F) != 0 {
        return Err(Error::other("Unsupported LZMA2 property bits"));
    }
    if bits > 40 {
        return Err(Error::other("Dictionary larger than 4GiB maximum size"));
    }
    if bits == 40 {
        return Ok(0xFFFF_FFFF);
    }
    Ok((2 | (bits & 1)) << (bits / 2 + 11))
}

/// The dictionary a coder actually needs: no more than the output it is going
/// to produce.
///
/// A match distance can never reach further back than the number of bytes the
/// stream has produced, so a dictionary larger than the coder's declared
/// unpacked size is never read from — it is only allocated. The size is one
/// header field and the output size is another, so an archive can name a 4 GiB
/// dictionary for a 1 KiB stream and make a reader allocate the difference.
/// 7-Zip clamps the same way (it calls it reducing the dictionary size), so
/// this rejects nothing that decodes today.
///
/// Never goes below 4 KiB, the smallest dictionary the format expresses.
pub(crate) fn clamp_dictionary(dict_size: u32, unpacked_size: u64) -> u32 {
    const MIN_DICT: u64 = 1 << 12;
    let needed = unpacked_size.max(MIN_DICT).min(u64::from(u32::MAX));
    dict_size.min(needed as u32)
}

/// The LZMA2 property byte for the smallest dictionary that is still at least
/// `dict_size`, never larger than `prop` itself declares.
///
/// LZMA2 carries its dictionary as one byte out of a 41-value table rather than
/// as a number, so [`clamp_dictionary`]'s answer has to be rounded back up to a
/// value the table can express.
pub(crate) fn lzma2_clamped_prop(prop: u8, unpacked_size: u64) -> u8 {
    let Ok(dict) = lzma2_dictionary_size(&[prop]) else {
        return prop;
    };
    let target = u64::from(clamp_dictionary(dict, unpacked_size));
    for candidate in 0..=prop {
        if let Ok(size) = lzma2_dictionary_size(&[candidate])
            && u64::from(size) >= target
        {
            return candidate;
        }
    }
    prop
}

/// Kilobytes an LZMA2 decode of `dict_size` needs, for the reader's
/// `max_mem_limit_kb` check.
pub(crate) fn lzma2_memory_usage_kb(dict_size: u32) -> usize {
    let bytes = u64::from(dict_size).saturating_add(LZ_STATE_BYTES);
    bytes.div_ceil(1024) as usize
}

/// Builds the LZMA1 decoder for a 7z coder.
///
/// `uncompressed_len` is the coder's declared output size; a 7z LZMA1 stream
/// carries no end marker, so the length is what stops the decode.
pub(crate) fn lzma_decoder<R: Read>(
    input: R,
    uncompressed_len: usize,
    properties: &[u8],
    dict_size: u32,
) -> Result<LzmaReader<R>, std::io::Error> {
    // The caller has already rejected a properties field shorter than five
    // bytes; `LzmaProps::parse` wants exactly five.
    let mut raw = [0u8; lzma_turbo::LZMA_PROPS_SIZE];
    raw.copy_from_slice(&properties[..lzma_turbo::LZMA_PROPS_SIZE]);
    // The dictionary the caller settled on, which is the declared one clamped
    // to what the stream can actually reach back into.
    raw[1..5].copy_from_slice(&dict_size.to_le_bytes());
    let props = LzmaProps::parse(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    LzmaReader::with_props(input, props, Some(uncompressed_len as u64))
}
/// Bytes of input read from the coder below in one go when the adaptive
/// decoder asks for more. `lzma-turbo`'s own single-threaded path reads in
/// 1 MiB pieces (`IN_BUF_SIZE_ST`); matching it keeps the feed loop's
/// bookkeeping off the profile.
const MT_INPUT_CHUNK: usize = 1 << 20;

/// The size of one piece of buffered output. See [`Lzma2MtReader::out`].
const MT_OUTPUT_CHUNK: usize = 1 << 20;

/// Pieces of buffered output kept for refilling. See [`Lzma2MtReader::spare`].
const MT_OUTPUT_SPARE_PIECES: usize = 8;

/// Capacity [`Lzma2MtReader::inbuf`] keeps once it has held a long run.
const MT_INPUT_KEEP_BYTES: usize = 8 * MT_INPUT_CHUNK;

/// Smallest in-flight budget a parallel LZMA2 decode is given. Below this the
/// coder decodes single-threaded instead: see [`Lzma2Plan::for_block`].
const MT_MIN_BUDGET_BYTES: u64 = 32 * 1024 * 1024;

/// Backstop on the in-flight budget, per thread, when the caller set no memory
/// limit at all.
///
/// This is a ceiling and not a target. What a parallel decode actually settles
/// at is decided by its backlog — [`MT_BACKLOG_RUNS_PER_THREAD`] complete runs
/// for every thread the budget affords — so for the runs an encoder writes the
/// decode stops well short of this. The backstop is what is left if a stream's
/// runs are larger than any encoder writes: the decode is then throttled below
/// what it would like, which costs speed, rather than allowed to grow without
/// an end.
///
/// It is deliberately *not* the number the decode settles at. It used to be:
/// the budget was 512 MiB a thread and the read-ahead a flat 128 MiB of it,
/// neither of them anything to do with the stream in hand, and a decoder fills
/// whatever ceiling it is given.
const MT_BACKSTOP_PER_THREAD_BYTES: u64 = 384 * 1024 * 1024;

/// Recently closed runs whose sizes the read-ahead is taken from.
///
/// One run is enough for the streams an encoder writes, where every run but
/// the last is exactly the size the encoder chose. A window rather than a
/// single sample is for the ones that are not uniform: the target is the
/// largest of the window, so a decode that meets a run twice the size of its
/// neighbours has room for it, and a window rather than the high-water mark of
/// the whole stream is so that one outsized run does not hold the footprint up
/// for the gigabytes after it.
const MT_RUN_WINDOW: usize = 8;

/// Complete runs to keep waiting in the decoder, per thread.
///
/// The read-ahead above is a ceiling on bytes, and reaching it in one go is
/// wrong when runs are small: `7zz -mx1` writes 1 MiB runs for data that does
/// not compress, the byte ceiling is then hundreds of runs, and every worker
/// sat idle while this thread read and copied all of them - 0.2 s of a 1.3 s
/// decode. So a feed stops once there are this many runs per thread
/// waiting, and is topped up before every drain instead, while the workers are
/// busy. Two per thread means a worker that finishes always finds a run there
/// even if the top-up is a whole read behind. With 128 MiB runs the byte
/// ceiling is reached first and nothing changes.
const MT_BACKLOG_RUNS_PER_THREAD: u64 = 2;

/// Packed bytes of backlog to keep per thread, once the runs a free worker
/// would need are already there.
///
/// Reading a run is a copy and takes tens of milliseconds; decoding one takes
/// seconds. Read-ahead beyond what the workers can start on therefore buys
/// nothing, and under a caller's limit it costs the very room the decoder
/// needs to dispatch into. Two 128 MiB runs per thread is a quarter of a
/// gigabyte per thread that nobody can use; this ceiling turns that into the
/// runs actually wanted, while leaving a stream of 1 MiB runs — where two per
/// thread is a rounding error — under the count rule instead.
const MT_BACKLOG_BYTES_PER_THREAD: u64 = 8 << 20;

/// How many turns of the read loop may produce nothing at all before the
/// decode is called stopped.
///
/// A turn that reaches the count has drained no bytes, fed no bytes and found
/// no worker to wait for, three times over: there is nothing in the decode
/// that can change and nothing arriving that could change it. One such turn is
/// legitimate — the feed that follows it is the escape — and a handful more
/// are cheap, so the count is set where no working decode can reach it and a
/// stuck one cannot outlast it.
const MT_IDLE_TURNS_BEFORE_STALLED: u32 = 1000;

/// How much may be read ahead without a single worker having been spawned
/// before the reader concludes that reading ahead is buying nothing.
///
/// A stream with no dictionary resets — what `7zz -mmt=1` writes — is one run
/// from beginning to end, and a run is dispatched only once it has arrived
/// whole, so reading ahead on such a stream buffers the entire archive to
/// hand it to a single worker at the end. That is slower than decoding it as
/// it arrives, and holds the whole archive in memory to be so. Past this
/// point the reader stops getting ahead and lets the decoder stream, and it
/// starts again the moment a worker does appear.
const MT_NO_WORKER_GIVE_UP_BYTES: u64 = 256 * 1024 * 1024;

/// Packed bytes this reader will hold that it has not been able to hand to the
/// decoder, because they are part of a run whose end has not been seen yet.
///
/// The bound matters only for a stream whose runs are larger than it. There the
/// decoder runs out of work, the reader switches the chase path on and feeds
/// these bytes as they come; the cap is what stops this reader from reading an
/// entire single-run archive into memory while it waits for a boundary that is
/// not coming.
const MT_INPUT_HOLD_BYTES: usize = 192 * 1024 * 1024;

/// The live link between an [`ArchiveReader`] and the LZMA2 coder that is
/// decoding one of its blocks right now.
///
/// Weaver drives an adaptive chase: it decodes with one thread while it is
/// following the tail of a download and widens to several once a backlog of
/// complete runs has built up, then narrows again. Both directions have to
/// work *during* a block, so the thread count cannot be a constructor
/// argument that the coder copies once.
///
/// It is a handful of atomics rather than a lock because the reader writes
/// the thread count from the caller's thread while the coder reads it from
/// the same thread between blocks of output, and the statistics go the other
/// way. Nothing here is a synchronisation point for the decode itself: the
/// coder applies a new thread count at the next run boundary, which is the
/// only place where changing it is lossless.
///
/// [`ArchiveReader`]: crate::ArchiveReader
#[derive(Debug)]
pub(crate) struct Lzma2Control {
    threads: AtomicU32,
    engaged: AtomicBool,
    block_index: AtomicUsize,
    pending_runs: AtomicUsize,
    runs_claimed: AtomicU64,
    in_flight_bytes: AtomicU64,
    spawned_threads: AtomicU32,
    /// Checksums the workers computed, keyed by where in the block's decoded
    /// stream they came from. Empty unless the coder was built with split
    /// points; see [`Lzma2Control::folded`].
    folder: Mutex<CrcFolder<u32>>,
}

impl Lzma2Control {
    pub(crate) fn new(threads: u32) -> Self {
        Self {
            threads: AtomicU32::new(threads.max(1)),
            engaged: AtomicBool::new(false),
            block_index: AtomicUsize::new(0),
            pending_runs: AtomicUsize::new(0),
            runs_claimed: AtomicU64::new(0),
            in_flight_bytes: AtomicU64::new(0),
            spawned_threads: AtomicU32::new(0),
            folder: Mutex::new(CrcFolder::new()),
        }
    }

    /// Adds what a worker computed. Called from the decoding thread, as blocks
    /// are delivered; the pieces arrive in whatever order the workers finished
    /// and the folder sorts them out.
    fn fold_segments(&self, segments: &[lzma_turbo::Segment]) {
        if segments.is_empty() {
            return;
        }
        let Ok(mut folder) = self.folder.lock() else {
            return;
        };
        for segment in segments {
            if let Some(crc32) = segment.check.crc32() {
                folder.push(segment.offset, segment.len, crc32);
            }
        }
    }

    /// The CRC-32 of `[offset, offset + len)` of the block's decoded stream,
    /// folded from the worker-computed pieces, or `None` when the pieces do
    /// not cover that range — because the coder was not the parallel one,
    /// because no split points were given, or because the caller has not read
    /// that far.
    pub(crate) fn folded(&self, offset: u64, len: u64) -> Option<u32> {
        self.folder.lock().ok()?.range(offset, len)
    }

    /// Drops the pieces of the block just finished. A new block starts its own
    /// stream at offset zero, so keeping the old ones would let a stale piece
    /// answer for a new range.
    pub(crate) fn clear_folded(&self) {
        if let Ok(mut folder) = self.folder.lock() {
            folder.clear();
        }
    }

    /// Sets the ceiling the next run boundary will use.
    pub(crate) fn set_threads(&self, threads: u32) {
        self.threads.store(threads.max(1), Ordering::Relaxed);
    }

    pub(crate) fn threads(&self) -> u32 {
        self.threads.load(Ordering::Relaxed)
    }

    /// Called by the coder as it is built.
    fn engage(&self) {
        self.engaged.store(true, Ordering::Relaxed);
    }

    /// Called by the coder when its block is done, or has failed. What it was
    /// holding is gone, but what it did — the runs it claimed and the threads
    /// it spawned — stays readable until the next block starts: a caller that
    /// decodes a whole block in one `read` would otherwise have no moment at
    /// which it could see anything at all.
    fn finish(&self) {
        self.pending_runs.store(0, Ordering::Relaxed);
        self.in_flight_bytes.store(0, Ordering::Relaxed);
    }

    /// Called as each block starts, before its coder exists. Everything the
    /// last block reported stops being true here.
    pub(crate) fn set_block_index(&self, block_index: usize) {
        self.block_index.store(block_index, Ordering::Relaxed);
        self.engaged.store(false, Ordering::Relaxed);
        self.pending_runs.store(0, Ordering::Relaxed);
        self.runs_claimed.store(0, Ordering::Relaxed);
        self.in_flight_bytes.store(0, Ordering::Relaxed);
        self.spawned_threads.store(0, Ordering::Relaxed);
        self.clear_folded();
    }

    /// What the coder currently decoding sees, or `None` when no LZMA2 coder
    /// with a parallel plan is live.
    pub(crate) fn progress(&self) -> Option<Lzma2Progress> {
        if !self.engaged.load(Ordering::Relaxed) {
            return None;
        }
        Some(Lzma2Progress {
            block_index: self.block_index.load(Ordering::Relaxed),
            threads: self.threads.load(Ordering::Relaxed),
            spawned_threads: self.spawned_threads.load(Ordering::Relaxed),
            pending_runs: self.pending_runs.load(Ordering::Relaxed),
            runs_claimed: self.runs_claimed.load(Ordering::Relaxed),
            in_flight_bytes: self.in_flight_bytes.load(Ordering::Relaxed),
        })
    }
}

/// A live handle on the LZMA2 coder of whichever block is being decoded.
///
/// This is how a consumer drives an adaptive chase. It is an owned handle
/// rather than a method on the reader because a decode borrows the reader for
/// its duration: the handle is taken first, and then used — from the decoding
/// thread between reads, or from another thread entirely — while the decode
/// runs.
///
/// A handle whose reader is decoding a block whose coder is not LZMA2, or
/// whose LZMA2 coder was built single-threaded, reports
/// [`progress`](Lzma2Handle::progress) as `None` and its
/// [`set_threads`](Lzma2Handle::set_threads) applies to the next block that
/// can use it.
#[derive(Debug, Clone)]
pub struct Lzma2Handle {
    pub(crate) control: Arc<Lzma2Control>,
}

impl Lzma2Handle {
    /// Sets the thread ceiling, effective at the next run boundary. One means
    /// the next run decodes inline on the calling thread.
    ///
    /// The value is clamped to `1..=256`, as on the reader.
    pub fn set_threads(&self, threads: u32) {
        self.control.set_threads(threads.clamp(1, 256));
    }

    /// The ceiling currently in force.
    #[must_use]
    pub fn threads(&self) -> u32 {
        self.control.threads()
    }

    /// What the LZMA2 coder of the block being decoded is doing right now, or
    /// `None` when no block is decoding through the adaptive path.
    #[must_use]
    pub fn progress(&self) -> Option<Lzma2Progress> {
        self.control.progress()
    }
}

/// What the LZMA2 coder of the block being decoded is doing right now.
///
/// A *run* is a piece of the LZMA2 stream that begins with a dictionary reset
/// and is therefore decodable on its own. The count of complete runs that have
/// arrived and have not yet been claimed by a decoder is the backlog an
/// adaptive caller widens on: while it is zero the stream is being chased and
/// there is nothing to parallelise; while it grows there is work that more
/// threads would finish sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lzma2Progress {
    /// The block whose LZMA2 coder this describes.
    pub block_index: usize,
    /// Thread ceiling currently in force. One means the next run is decoded
    /// inline on the calling thread.
    pub threads: u32,
    /// Worker threads that exist. Zero until a run is actually dispatched, so
    /// a decoder that never widens never creates one.
    pub spawned_threads: u32,
    /// Complete runs that have arrived and not yet been claimed: the backlog.
    pub pending_runs: usize,
    /// Runs handed to a decoder so far, by either path — the run index of the
    /// current block.
    pub runs_claimed: u64,
    /// Bytes the decoder is holding: buffered input, runs being decoded, and
    /// decoded output not yet handed to the caller.
    pub in_flight_bytes: u64,
}

/// How the LZMA2 coder of one block should be decoded.
///
/// This is the seam, and it is deliberately the whole of it: `decoder.rs` asks
/// the question here and nowhere else.
///
/// # Why the adaptive decoder and not the ring
///
/// `lzma-turbo` has two multi-threaded LZMA2 drivers. `Lzma2ParallelDecoder` is
/// the faithful port of 7-Zip's `Lzma2DecMt.c` over `MtDec.c`: it pulls from a
/// `Read` and owns the threads for the whole call, which is the fastest way to
/// decode an archive that is already on disk. Its `Read` adapter spawns the
/// ring behind a thread, so the reader it is given must be `Send + 'static` —
/// and the input of a 7z coder is a bounded view of the caller's archive
/// source, which is neither.
///
/// `Lzma2AdaptiveDecoder` is fed bytes and polled for output, so it borrows
/// nothing: the work it hands to threads is owned copies of complete runs. It
/// is also the only one of the two that can change its mind mid-stream, which
/// is what a consumer chasing a download needs, and what
/// [`Lzma2Control`] exposes here. Both drivers find runs with the same
/// scanner and decode them with the same decoder, so the bytes are identical
/// either way.
pub(crate) enum Lzma2Plan {
    /// One dependent stream decoded on the calling thread, exactly as before:
    /// no control block, no worker, no extra allocation. This is what a caller
    /// that asks for nothing gets.
    SingleThreaded,
    /// The adaptive decoder, with a live thread ceiling and a ceiling on what
    /// it may hold in flight.
    Adaptive {
        /// Workers to use at the first run boundary. Already clamped to
        /// 1..=256 by the reader; one means "capable but inline".
        threads: u32,
        /// Ceiling on what the parallel decode may hold: buffered input, runs
        /// being decoded, and decoded output not yet read. This is *not* the
        /// dictionary-based number `Archive::decoder_memory_estimate` reports,
        /// and it is enforced by the decoder rather than assumed.
        memory_limit: u64,
        /// The live link back to the reader.
        control: Arc<Lzma2Control>,
        /// Where the consumer's boundaries fall in this block's decoded
        /// stream, so each worker checksums the pieces of its own output as
        /// it produces them. Empty when the caller has no boundaries to
        /// declare, in which case no checksum is computed here at all.
        splits: Vec<u64>,
    },
}

impl Lzma2Plan {
    /// Chooses how to decode one block's LZMA2 coder.
    ///
    /// `threads` is the caller's live ceiling and `adaptive` is whether the
    /// caller asked for a coder that can be widened later even though the
    /// ceiling is one right now. `memory_limit_bytes` is
    /// [`ArchiveLimits::memory_limit_bytes`], and `dict_size` the dictionary
    /// this coder declares.
    ///
    /// # The rule when the budget is too small
    ///
    /// A parallel decode holds whole runs, so it needs room the dictionary
    /// number says nothing about. What is left of the caller's budget after
    /// the decoder's own footprint is the in-flight budget; when the caller
    /// set no budget at all it is the backstop, `threads` x
    /// [`MT_BACKSTOP_PER_THREAD_BYTES`]. Either way it is a ceiling and not a
    /// target: what the decode settles at is its backlog of complete runs,
    /// taken from the size of the runs the stream turns out to have.
    ///
    /// If that leaves less than 32 MiB, **the coder decodes single-threaded
    /// rather than failing**. A memory limit is a statement about what the
    /// caller can afford, not a request to decode in parallel; refusing the
    /// archive because it cannot *also* be decoded quickly would turn a
    /// performance knob into a correctness one. The single-threaded decoder
    /// streams, so it needs nothing beyond the dictionary — which the reader
    /// has already checked against the same budget, and which is what would
    /// actually refuse the archive.
    ///
    /// [`ArchiveLimits::memory_limit_bytes`]: crate::ArchiveLimits::memory_limit_bytes
    pub(crate) fn for_block(
        threads: u32,
        adaptive: bool,
        memory_limit_bytes: u64,
        dict_size: u32,
        control: &Arc<Lzma2Control>,
        splits: &[u64],
    ) -> Self {
        if threads <= 1 && !adaptive {
            return Self::SingleThreaded;
        }
        let Some(memory_limit) = Self::mt_budget(threads, memory_limit_bytes, dict_size) else {
            return Self::SingleThreaded;
        };
        Self::Adaptive {
            threads,
            memory_limit,
            control: Arc::clone(control),
            splits: splits.to_vec(),
        }
    }

    /// The in-flight budget, or `None` when it is too small to be worth
    /// engaging the parallel path.
    fn mt_budget(threads: u32, memory_limit_bytes: u64, dict_size: u32) -> Option<u64> {
        let budget = if memory_limit_bytes == u64::MAX {
            u64::from(threads).saturating_mul(MT_BACKSTOP_PER_THREAD_BYTES)
        } else {
            let own = u64::from(dict_size).saturating_add(LZ_STATE_BYTES);
            memory_limit_bytes.saturating_sub(own)
        };
        (budget >= MT_MIN_BUDGET_BYTES).then_some(budget)
    }
}

/// The LZMA2 coder of a block, single-threaded or adaptive.
///
/// Both are a plain `Read` that produces the block's bytes in order, so
/// everything above them — the rest of the coder chain, the CRC verification,
/// the block-completion hook — is the same code either way.
pub(crate) enum Lzma2Coder<R: Read> {
    SingleThreaded(Box<Lzma2Reader<R>>),
    Adaptive(Box<Lzma2MtReader<R>>),
}

impl<R: Read> Read for Lzma2Coder<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::SingleThreaded(r) => r.read(buf),
            Self::Adaptive(r) => r.read(buf),
        }
    }
}

/// A `Read` over [`Lzma2AdaptiveDecoder`], pulling from the coder below.
///
/// The adaptive decoder is a feed/drain machine: this is the thin loop that
/// turns it back into a reader for callers who have a stream in hand rather
/// than one arriving. `crate::Lzma2BlockFeeder` is the same decoder with the
/// loop left to the caller.
pub(crate) struct Lzma2MtReader<R: Read> {
    decoder: Lzma2AdaptiveDecoder,
    input: R,
    control: Arc<Lzma2Control>,
    /// The ceiling currently applied to `decoder`, so that a caller that does
    /// not change it costs one relaxed load per block of output.
    applied_threads: u32,
    inbuf: Vec<u8>,
    in_pos: usize,
    /// Stream offset of `inbuf[0]`.
    base: u64,
    /// Index into `inbuf` of the first byte this reader has not scanned.
    scan_pos: usize,
    /// This reader's own walk of the chunk headers, which is how it knows
    /// where a run ends without decoding anything. See [`Self::safe_limit`].
    scanner: Lzma2RunScanner,
    /// Run boundaries [`Self::scanner`] has found. Zero of them after a good
    /// look is what a stream that cannot be parallelised at all looks like.
    runs_seen: u64,
    /// Packed and unpacked sizes of the last [`MT_RUN_WINDOW`] runs the
    /// scanner closed, and where the next one goes. This is what sizes the
    /// decode: see [`Lzma2MtReader::affordable_threads`].
    recent_runs: [(u64, u64); MT_RUN_WINDOW],
    recent_at: usize,
    /// Set once bytes have been fed that do not end on a run boundary, which
    /// puts the decoder's chase path inside a run until that run is over.
    chasing: bool,
    /// Whether the decoder's chase path is currently switched on, so that a
    /// steady-state feed costs no calls into it. See [`Self::set_chase`].
    chase: bool,
    /// Where the runs [`Self::scanner`] has closed end, in stream offsets, for
    /// those the feed has not yet reached. See [`Self::fed_at_boundary`].
    run_ends: VecDeque<u64>,
    /// The last run end the feed has reached, so that where it has got to can
    /// be compared against where a run ends without keeping every boundary the
    /// stream ever had.
    last_boundary: u64,
    /// Where each scanned run ends in the *output*, for those whose output has
    /// not yet been handed over, and how many runs have been handed over whole.
    ///
    /// The decoder says how many runs it has claimed but not how many it has
    /// finished, and the difference is what its workers are busy with. A run's
    /// output is delivered in order, so a run whose end has been handed to the
    /// caller is a run no worker still holds: counting those against the claims
    /// is how this reader knows whether a worker is free and waiting for a run.
    /// Runs this reader never saw are left out, which can only make the count
    /// of busy workers too low and so the read-ahead too generous, never too
    /// mean — the direction that cannot starve a worker.
    unpacked_ends: VecDeque<u64>,
    /// Running total of the scanned runs' unpacked sizes, which is where the
    /// next run's output ends.
    unpacked_seen: u64,
    /// Bytes of output handed to the caller.
    delivered_out: u64,
    /// Runs whose output has been handed over whole.
    runs_delivered: u64,
    /// Set if the chunk headers could not be walked, which hands the steering
    /// back to the decoder and its error reporting.
    scan_broken: bool,
    /// [`MT_NO_WORKER_GIVE_UP_BYTES`], as a field so that a test can reach the
    /// single-run path without a quarter-gigabyte fixture.
    give_up_bytes: u64,
    /// [`MT_INPUT_HOLD_BYTES`], a field for the same reason.
    hold_bytes: usize,
    /// [`MT_BACKLOG_BYTES_PER_THREAD`], a field for the same reason: the three
    /// run-size regimes it sorts streams into are reachable in a test by moving
    /// the ceiling rather than by building runs of a hundred megabytes.
    backlog_bytes: u64,
    input_done: bool,
    /// Decoded output not yet handed to the caller, in fixed-size pieces.
    ///
    /// One `drain` can deliver everything the fed bytes allow — a gigabyte,
    /// for an archive with a run per thread in flight — while a caller reads
    /// in kilobytes. Growing one buffer to hold that copies it again at every
    /// doubling; a queue of same-sized pieces, recycled through `spare`, never
    /// reallocates and never copies twice.
    out: VecDeque<Vec<u8>>,
    /// Pieces already read out, kept to be filled again. Capped at
    /// [`MT_OUTPUT_SPARE_PIECES`]: past a handful the recycling has stopped
    /// paying for itself and the rest is only held memory.
    spare: Vec<Vec<u8>>,
    out_pos: usize,
    finished: bool,
    /// Whether the workers are computing checksums to be collected.
    checksums: bool,
    /// Packed bytes handed to the decoder so far, to notice a stream whose
    /// read-ahead is buying nothing. See [`MT_NO_WORKER_GIVE_UP_BYTES`].
    fed_total: u64,
    trace: Option<Box<MtTrace>>,
}

/// Phase timing for the parallel path, off unless `SEVENZ_TURBO_MT_TRACE` is
/// set. See `docs/benchmarking.md`.
#[derive(Default)]
pub(crate) struct MtTrace {
    pump: std::time::Duration,
    drain: std::time::Duration,
    drains: u64,
    /// Bytes fed from a partial run, which is what this reader does when it has
    /// no complete run to hand over. It says nothing about whether the decoder
    /// then decoded them on the calling thread; that is the decoder's own
    /// `chase_decoded_bytes`, reported beside it.
    chased: u64,
    out_bytes: u64,
    sink: std::time::Duration,
    blocks: u64,
    small_bytes: u64,
    /// Bytes the input buffer has copied down over itself to make room, and
    /// the number of times its capacity changed either way. Both are the cost
    /// of holding the packed stream in one buffer rather than a queue of
    /// pieces, and both are invisible in a profile that only counts decoding.
    moved: u64,
    resizes: u64,
}

impl<R: Read> Lzma2MtReader<R> {
    fn new(
        input: R,
        dict_prop: u8,
        threads: u32,
        memory_limit: u64,
        control: Arc<Lzma2Control>,
        splits: &[u64],
    ) -> Result<Self, std::io::Error> {
        let options = Lzma2MtOptions {
            threads: threads as usize,
            memory_limit,
        };
        let mut decoder = Lzma2AdaptiveDecoder::new(dict_prop, &options).map_err(decode_error)?;
        // This reader hands over whole runs and nothing else, so the run at
        // the decoder's cursor is incomplete only because the rest of it has
        // not been fed yet. Feeding it is cheaper than decoding it here with
        // the workers stood down; see [`Self::set_chase`].
        decoder.set_chase(false);
        let checksums = !splits.is_empty();
        if checksums {
            // The worker that produced the bytes checksums them, before it
            // queues to hand the block on. Nothing downstream of here —
            // neither this reader nor the caller consuming it — then has to
            // touch the bytes a second time to know a file's CRC-32.
            decoder.set_checksum(
                &ChecksumPlan::new(Checksum::Crc32).with_split_points(splits.iter().copied()),
            );
        }
        control.engage();
        control.set_threads(threads);
        Ok(Self {
            decoder,
            input,
            control,
            applied_threads: threads,
            inbuf: Vec::new(),
            in_pos: 0,
            base: 0,
            scan_pos: 0,
            scanner: Lzma2RunScanner::new(),
            runs_seen: 0,
            recent_runs: [(0, 0); MT_RUN_WINDOW],
            recent_at: 0,
            chasing: false,
            chase: false,
            run_ends: VecDeque::new(),
            last_boundary: 0,
            unpacked_ends: VecDeque::new(),
            unpacked_seen: 0,
            delivered_out: 0,
            runs_delivered: 0,
            scan_broken: false,
            give_up_bytes: MT_NO_WORKER_GIVE_UP_BYTES,
            hold_bytes: MT_INPUT_HOLD_BYTES,
            backlog_bytes: MT_BACKLOG_BYTES_PER_THREAD,
            input_done: false,
            out: VecDeque::new(),
            spare: Vec::new(),
            out_pos: 0,
            finished: false,
            checksums,
            fed_total: 0,
            trace: std::env::var_os("SEVENZ_TURBO_MT_TRACE").map(|_| Box::default()),
        })
    }

    /// Moves the checksums the workers computed into the shared folder, where
    /// the reader's per-file verification picks them up. Cheap: a handful of
    /// `(offset, len, crc)` triples per block, never the bytes.
    fn collect_checks(&mut self) {
        if !self.checksums {
            return;
        }
        for block in self.decoder.take_checks() {
            self.control.fold_segments(&block.segments);
        }
    }

    /// Publishes what the caller decides on: the backlog, the run index and
    /// what is held in memory.
    fn publish(&self) {
        self.control
            .pending_runs
            .store(self.decoder.pending_runs(), Ordering::Relaxed);
        self.control
            .runs_claimed
            .store(self.decoder.runs_claimed(), Ordering::Relaxed);
        self.control
            .in_flight_bytes
            .store(self.decoder.in_flight_bytes(), Ordering::Relaxed);
        self.control.spawned_threads.store(
            u32::try_from(self.decoder.spawned_threads()).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
    }

    /// Applies a thread count the caller changed since the last look. The
    /// decoder itself defers it to the next run boundary.
    fn sync_threads(&mut self) {
        let want = self.control.threads();
        if want != self.applied_threads {
            self.decoder.set_threads(want as usize);
            self.applied_threads = want;
        }
    }

    /// Switches the decoder's chase path on or off.
    ///
    /// The chase path decodes the run at the decoder's cursor on this thread,
    /// a chunk at a time, without waiting for the whole of it — and while it
    /// holds the cursor no worker may claim a run, so a decoder that chases is
    /// a decoder that is not threading.
    ///
    /// Which of those is wanted is this reader's business and not the
    /// decoder's, because this reader is the one deciding what to hand over.
    /// While it is feeding whole runs, a run that is incomplete at the cursor
    /// is incomplete only because the rest of it has not been fed yet, and
    /// reading it is cheaper than decoding it here: chasing off. The reader
    /// also has three modes in which it feeds the front of a run on purpose —
    /// a stream written as one run, a run larger than what it will hold, and a
    /// stream whose headers it could not walk — and there the decoder must
    /// chase or nothing would decode at all.
    ///
    /// Off is not never: a run too large for the memory limit, a run the chase
    /// has already begun, a decoder narrowed to one thread, and anything at
    /// all once the input is over are decoded here whatever this says.
    fn set_chase(&mut self, chase: bool) {
        if self.chase != chase {
            self.decoder.set_chase(chase);
            self.chase = chase;
        }
    }

    /// Whether what has been handed to the decoder stops where a run does.
    ///
    /// Getting no further ahead is only safe there. A decoder that is not
    /// chasing and has been given the front of a run will wait for the rest
    /// rather than decode it, so a reader that stopped part way through one
    /// and then decided it was far enough ahead would be waiting for a decoder
    /// that was waiting for it.
    ///
    /// The three modes that feed the front of a run on purpose all have the
    /// decoder chasing, and a decoder that has been told the input is over
    /// decodes what it holds whatever else it has been told, so each of them
    /// is a place it is safe to stop.
    fn fed_at_boundary(&mut self) -> bool {
        if self.chasing || self.input_done || self.scan_broken {
            return true;
        }
        let fed_to = self.base + self.in_pos as u64;
        while self.run_ends.front().is_some_and(|&end| end <= fed_to) {
            self.last_boundary = self.run_ends.pop_front().expect("just looked");
        }
        fed_to == self.last_boundary
    }

    /// What one run of this stream costs, as `(packed, unpacked)`, taken as
    /// the largest of the last [`MT_RUN_WINDOW`] the scanner closed. `(0, 0)`
    /// until the first one has closed.
    fn run_size(&self) -> (u64, u64) {
        self.recent_runs
            .iter()
            .fold((0, 0), |(p, u), &(run_p, run_u)| {
                (p.max(run_p), u.max(run_u))
            })
    }

    /// How many threads this decode can actually keep decoding at once.
    ///
    /// A caller's memory limit does not slow a decode down evenly; it decides
    /// how many runs can be inside the decoder at the same time, and a thread
    /// the limit cannot find room for is not a thread. Reading ahead for one
    /// buys nothing and spends the very thing that is short. So the count is
    /// what the limit affords — its own size over what one run of this stream
    /// costs to have in hand — never more than the caller asked for, and never
    /// less than one, because a decode of one run at a time is still a decode.
    ///
    /// Until the first run has closed there is no cost to divide by and every
    /// thread is taken as affordable; the backlog is empty then in any case.
    ///
    /// The cost divided by is the run in flight alone, not that run plus the
    /// runs read ahead for it. Charging a thread for its read-ahead as well is
    /// the truer figure and does hold less — but it affords fewer threads, and
    /// under a tight limit the runs those threads would have decoded are
    /// decoded on the calling thread instead, which costs more time than the
    /// memory is worth here.
    fn affordable_threads(&self) -> u64 {
        let threads = u64::from(self.applied_threads.max(1));
        let (packed, unpacked) = self.run_size();
        self.decoder
            .memory_limit()
            .checked_div(packed.saturating_add(unpacked))
            .map_or(threads, |fits| fits.clamp(1, threads))
    }

    /// Complete runs the decoder should have waiting once this reader has read
    /// far enough ahead: [`MT_BACKLOG_RUNS_PER_THREAD`] for every thread that
    /// can be decoding at once.
    fn backlog_target(&self) -> u64 {
        self.affordable_threads() * MT_BACKLOG_RUNS_PER_THREAD
    }

    /// How many more complete runs the decoder should be holding than it is.
    fn backlog_wanted(&self) -> u64 {
        self.backlog_target()
            .saturating_sub(self.decoder.pending_runs() as u64)
    }

    /// Whether the decoder has as much work in hand as reading further ahead
    /// could give it.
    ///
    /// Read-ahead exists to keep runs waiting for threads, so runs waiting for
    /// threads is what it is measured by. It is deliberately not a count of
    /// the bytes the decoder is holding: that figure also moves with how much
    /// decoding is under way and how much finished output is waiting its turn,
    /// neither of which more input can help with. A reader watching it stops
    /// feeding exactly when a decode gets busy, which leaves the run at the
    /// cursor half fed and hands it to the chase path — the fastest way to
    /// turn a parallel decode into a single-threaded one.
    ///
    /// So the feed stops at a floor and two ceilings. The floor is what a free
    /// worker needs — one run each, and one more so that a worker finishing
    /// while this thread is elsewhere still finds one — and nothing stops the
    /// feed below it. Above the floor, either ceiling ends the read-ahead:
    /// [`MT_BACKLOG_RUNS_PER_THREAD`] runs per thread, which is what small runs
    /// reach, or [`MT_BACKLOG_BYTES_PER_THREAD`] of packed backlog per thread,
    /// which is what large ones reach long before the count. Neither ceiling is
    /// allowed to exceed what the caller's limit leaves room for.
    fn backlog_full(&self) -> bool {
        let pending = self.decoder.pending_runs() as u64;
        if pending < self.demand_floor() {
            return false;
        }
        pending >= self.backlog_target() || self.backlog_packed() >= self.byte_cap()
    }

    /// Complete runs a worker that is free right now would need to find.
    ///
    /// One for each thread the limit affords and that is not already decoding,
    /// and one spare: a worker can finish at any point between this reader's
    /// feeds, and the spare is what it finds when it does.
    fn demand_floor(&self) -> u64 {
        self.idle_workers() + 1
    }

    /// Threads the limit affords that are not decoding a run at the moment.
    fn idle_workers(&self) -> u64 {
        self.affordable_threads()
            .saturating_sub(self.busy_workers())
    }

    /// Threads decoding a run at the moment: runs the decoder has claimed and
    /// whose output has not come back out.
    fn busy_workers(&self) -> u64 {
        self.decoder
            .runs_claimed()
            .saturating_sub(self.runs_delivered)
    }

    /// Takes note of how far the decoder's output has been handed over, and
    /// retires the runs that ends inside.
    ///
    /// Output arrives in stream order, so the runs are retired in order too and
    /// only the ends not yet reached need keeping.
    fn note_delivered(&mut self, handed: u64) {
        self.delivered_out = self.delivered_out.max(handed);
        while self
            .unpacked_ends
            .front()
            .is_some_and(|&end| end <= self.delivered_out)
        {
            self.unpacked_ends.pop_front();
            self.runs_delivered += 1;
        }
    }

    /// Packed bytes of complete runs waiting in the decoder.
    fn backlog_packed(&self) -> u64 {
        self.decoder.backlog().map(|run| run.packed_len).sum()
    }

    /// The byte ceiling on read-ahead, never more than the caller's limit,
    /// which has the whole decode to pay for and not just the backlog.
    fn byte_cap(&self) -> u64 {
        self.backlog_bytes
            .saturating_mul(self.affordable_threads())
            .min(self.decoder.memory_limit())
    }

    /// The stream offset past which feeding would put the decoder's chase
    /// path inside a run.
    ///
    /// Everything before the run currently being scanned belongs to a run
    /// whose end this reader has seen, so the decoder can hand it to a worker
    /// whole. Feeding one byte beyond that is not a small mistake: the chase
    /// path takes whatever incomplete run sits at the cursor, and once it is
    /// inside a run it keeps that run — single-threaded, with dispatch turned
    /// off — to the end, while the workers idle. Feeding "a run per thread and
    /// then whatever is left over" therefore gave away about one run in three
    /// at two threads, which is where the 1.7x deficit against a bare parallel
    /// decode came from.
    fn safe_limit(&self) -> u64 {
        if self.input_done || self.scan_broken {
            return self.base + self.inbuf.len() as u64;
        }
        self.scanner
            .open_run_offset()
            .unwrap_or_else(|| self.scanner.in_position())
    }

    /// Reads another chunk of the packed stream and walks its chunk headers.
    ///
    /// Scanning is header arithmetic — the compressed payload is stepped over,
    /// never read — so this costs nothing measurable next to decoding it.
    fn refill(&mut self) -> std::io::Result<()> {
        let capacity_was = self.inbuf.capacity();
        if self.in_pos >= MT_INPUT_CHUNK {
            if let Some(t) = self.trace.as_mut() {
                t.moved += (self.inbuf.len() - self.in_pos) as u64;
            }
            self.inbuf.drain(..self.in_pos);
            self.base += self.in_pos as u64;
            self.scan_pos -= self.in_pos;
            self.in_pos = 0;
            // A run longer than [`MT_INPUT_HOLD_BYTES`] grows this buffer to
            // hold what has been read of it, and a `Vec` never gives room
            // back. Once the stream is past such a run the extra is dead
            // weight. Releasing it here rather than on every refill, and only
            // once what is left is a small part of what is held, keeps an
            // ordinary feed — where the buffer is a couple of chunks and stays
            // that way — from reallocating at all.
            if self.inbuf.capacity() > MT_INPUT_KEEP_BYTES
                && self.inbuf.len() <= self.inbuf.capacity() / 4
            {
                self.inbuf.shrink_to(MT_INPUT_KEEP_BYTES);
            }
        }
        let was = self.inbuf.len();
        self.inbuf.resize(was + MT_INPUT_CHUNK, 0);
        if let Some(t) = self.trace.as_mut() {
            t.resizes += u64::from(self.inbuf.capacity() != capacity_was);
        }
        let mut filled = was;
        while filled < self.inbuf.len() {
            match self.input.read(&mut self.inbuf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        self.inbuf.truncate(filled);
        if filled == was {
            self.input_done = true;
            self.decoder.end_of_input();
            return Ok(());
        }
        if !self.scan_broken && !self.scanner.finished() {
            match self.scanner.feed(&self.inbuf[self.scan_pos..]) {
                // A stream this reader cannot walk is still a stream the
                // decoder may be able to decode, and it is the decoder's job
                // to say so. Stop steering and feed it everything.
                Err(_) => self.scan_broken = true,
                Ok(n) => {
                    self.scan_pos += n;
                    while let Some(run) = self.scanner.next_run() {
                        self.runs_seen += 1;
                        self.run_ends.push_back(run.in_offset + run.packed_len);
                        self.unpacked_seen += run.unpacked_len;
                        self.unpacked_ends.push_back(self.unpacked_seen);
                        self.recent_runs[self.recent_at] = (run.packed_len, run.unpacked_len);
                        self.recent_at = (self.recent_at + 1) % MT_RUN_WINDOW;
                    }
                }
            }
        }
        if self.scan_broken || self.scanner.finished() {
            self.scan_pos = self.inbuf.len();
        }
        Ok(())
    }

    /// Feeds the packed stream until every worker has something to do, the
    /// decoder is holding as much as its budget allows, or the input is
    /// exhausted. Returns whether anything was fed.
    ///
    /// Feeding one chunk per call would be enough to keep a single-threaded
    /// decoder busy, and is exactly what starves a parallel one: a run is
    /// dispatched to a worker only once it has arrived *whole*, so a decoder
    /// holding one chunk has at most one incomplete run and nothing to give
    /// anybody. The runs `7zz -mmt=on` writes are 128 MiB.
    ///
    /// The other end is just as wrong: a `drain` decodes everything the bytes
    /// fed so far allow, so feeding the whole packed stream would decode the
    /// whole block into this reader's buffer before the caller saw its first
    /// byte. So feed until there is a complete run waiting for every thread —
    /// past that point more input buys no more parallelism, only memory — or
    /// until the decoder says it is full, which is the budget
    /// [`Lzma2Plan::for_block`] chose.
    ///
    /// What is fed always ends on a run boundary while one is in reach; see
    /// [`Self::safe_limit`]. Only when no boundary is in reach and the decoder
    /// has nothing left to do — a block written as a single run, or the tail
    /// of one still arriving — is the chase path switched on and handed a
    /// partial run, which is the case it exists for; see [`Self::set_chase`].
    ///
    /// Stopping is the other half of that bargain: everywhere this loop
    /// decides it has got far enough ahead it must first have got as far as
    /// the end of a run. See [`Self::fed_at_boundary`].
    ///
    /// `want_runs` ends the call early, once that many more runs have been
    /// seen and something has been fed: see [`MT_BACKLOG_RUNS_PER_THREAD`].
    ///
    /// `whatever_is_held` lifts the read-ahead target for this call, and only
    /// it: the decoder is holding more than the target already, and the caller
    /// has established that no worker is going to hand anything back. In that
    /// state the target has nothing left to ration — read-ahead is measured
    /// against what a decode in progress holds, and there is no decode in
    /// progress — while feeding nothing would leave the stream with no way to
    /// make another byte. It is the same rule the chase path is built on, and
    /// it is what stops [`Lzma2MtReader::read`] having a state it can neither
    /// leave nor make progress in. See [`Lzma2MtReader::backlog_full`].
    fn pump_input(&mut self, want_runs: u64, whatever_is_held: bool) -> std::io::Result<bool> {
        let mut fed = false;
        let runs_at_start = self.runs_seen;
        loop {
            // Both ways out of this loop that are a choice rather than a
            // necessity ask [`Self::fed_at_boundary`] first, and ask it last,
            // so that a decode that is going to stop anyway does not pay for
            // the question.
            if fed && self.runs_seen - runs_at_start >= want_runs && self.fed_at_boundary() {
                break;
            }
            // A stream whose first run has not ended within a good look at it
            // is a stream with one run in it — what `7zz -mmt=1` writes — and
            // no amount of reading ahead will find a second worker anything to
            // do. Reading ahead anyway would buffer the whole archive to hand
            // it to one worker at the very end, which is both slower than
            // decoding it as it arrives and gigabytes more expensive. The test
            // is made again every time round because it only becomes true part
            // way through a batch this loop would otherwise finish.
            let one_run = self.runs_seen == 0
                && self.scanner.in_position() > self.give_up_bytes
                && !self.input_done;
            // Two of the three modes in which the front of a run is handed
            // over deliberately: a stream that is one run from beginning to
            // end, and a stream whose headers could not be walked, where the
            // decoder is steering and is given everything. The third is the
            // long run below. In all three the decoder has to chase or nothing
            // would decode at all.
            if one_run || self.scan_broken {
                self.set_chase(true);
            }
            // A stream being chased has no runs to count, so the only thing
            // that can say "far enough" is what the decoder is holding, and a
            // chase wants no read-ahead at all: it decodes what it is given,
            // on this thread, and more input only sits there. Everything else
            // stops on its backlog.
            let far_enough = if one_run || self.scan_broken {
                self.decoder.in_flight_bytes() >= MT_INPUT_CHUNK as u64
            } else {
                self.backlog_full()
            };
            let hold = if one_run {
                2 * MT_INPUT_CHUNK
            } else {
                self.hold_bytes
            };
            if !whatever_is_held && far_enough && self.fed_at_boundary() {
                break;
            }
            let limit = self.safe_limit().saturating_sub(self.base);
            let mut end = self
                .inbuf
                .len()
                .min(usize::try_from(limit).unwrap_or(usize::MAX));
            if end <= self.in_pos {
                if !self.input_done && self.inbuf.len() - self.in_pos < hold {
                    self.refill()?;
                    continue;
                }
                let idle = self.decoder.in_flight_bytes() == 0;
                if (self.chasing || idle) && self.in_pos < self.inbuf.len() {
                    self.chasing = true;
                    self.set_chase(true);
                    end = self.inbuf.len().min(self.in_pos + MT_INPUT_CHUNK);
                    if let Some(t) = self.trace.as_mut() {
                        t.chased += (end - self.in_pos) as u64;
                    }
                } else {
                    break;
                }
            } else {
                self.chasing = false;
                if !self.scan_broken {
                    self.set_chase(false);
                }
            }
            let offered = end - self.in_pos;
            let taken = self
                .decoder
                .feed(&self.inbuf[self.in_pos..end])
                .map_err(decode_error)?;
            self.in_pos += taken;
            self.fed_total += taken as u64;
            fed |= taken > 0;
            if taken < offered {
                // The decoder is holding all it is allowed to.
                break;
            }
        }
        Ok(fed)
    }
}

impl<R: Read> Drop for Lzma2MtReader<R> {
    fn drop(&mut self) {
        if let Some(t) = self.trace.as_ref() {
            eprintln!(
                "mt-trace: threads={} spawned={} drains={} drain={:.3}s sink={:.3}s/{} small={} MiB pump={:.3}s fed={} MiB fed_partial={} MiB st_decoded={} MiB out={} MiB runs={} moved={} MiB resizes={}",
                self.applied_threads,
                self.decoder.spawned_threads(),
                t.drains,
                t.drain.as_secs_f64(),
                t.sink.as_secs_f64(),
                t.blocks,
                t.small_bytes >> 20,
                t.pump.as_secs_f64(),
                self.fed_total >> 20,
                t.chased >> 20,
                self.decoder.chase_decoded_bytes() >> 20,
                t.out_bytes >> 20,
                self.decoder.runs_claimed(),
                t.moved >> 20,
                t.resizes,
            );
        }
    }
}

impl<R: Read> Read for Lzma2MtReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Turns in a row that produced no output, fed nothing and had no
        // worker to wait for. Any one of the three resets it.
        let mut idle_turns = 0u32;
        loop {
            if let Some(front) = self.out.front() {
                if self.out_pos < front.len() {
                    let n = (front.len() - self.out_pos).min(buf.len());
                    buf[..n].copy_from_slice(&front[self.out_pos..self.out_pos + n]);
                    self.out_pos += n;
                    return Ok(n);
                }
                let mut done = self.out.pop_front().expect("front");
                if self.spare.len() < MT_OUTPUT_SPARE_PIECES {
                    done.clear();
                    self.spare.push(done);
                }
                self.out_pos = 0;
                continue;
            }
            if self.finished || buf.is_empty() {
                return Ok(0);
            }

            self.sync_threads();

            // Keep the backlog up while the workers are busy, rather than
            // waiting for the decoder to run dry and ask.
            if !self.input_done {
                let want = self.backlog_wanted();
                if want > 0 {
                    let t1 = std::time::Instant::now();
                    self.pump_input(want, false)?;
                    if let Some(t) = self.trace.as_mut() {
                        t.pump += t1.elapsed();
                    }
                }
            }

            // The decoder is asked for no more than `buf` holds, so what it
            // hands over goes straight into the caller's buffer and the rest
            // of the block it was in stays with the decoder for the next
            // call. An unbounded `drain` decodes everything the bytes fed so
            // far allow - at eight threads, up to a whole run per worker -
            // and everything past `buf` had to be spilled here and copied a
            // second time on the way out, which was measured at 0.7 s of a
            // 4.8 s decode of a gigabyte. The spill path below is kept as
            // the safety net for a sink handed more than it asked for; with
            // `drain_upto` honouring its limit it is never taken.
            //
            // The call borrows the decoder mutably and the sink needs the
            // spill buffer, so the buffer is lent to the call and taken back.
            let mut direct = 0usize;
            // Whether the decoder knew, going into this drain, that it had
            // been given everything.
            let told_the_end = self.input_done;
            let t0 = std::time::Instant::now();
            let mut out = std::mem::take(&mut self.out);
            let mut spare = std::mem::take(&mut self.spare);
            let mut sink_time = std::time::Duration::ZERO;
            let mut blocks = 0u64;
            let mut small = 0u64;
            let traced = self.trace.is_some();
            let mut handed = self.delivered_out;
            let status = self.decoder.drain_upto(buf.len(), |offset, bytes| {
                handed = handed.max(offset + bytes.len() as u64);
                let ts = traced.then(std::time::Instant::now);
                let mut rest = if direct < buf.len() {
                    let n = (buf.len() - direct).min(bytes.len());
                    buf[direct..direct + n].copy_from_slice(&bytes[..n]);
                    direct += n;
                    &bytes[n..]
                } else {
                    bytes
                };
                while !rest.is_empty() {
                    if out
                        .back()
                        .is_none_or(|piece| piece.len() == piece.capacity())
                    {
                        out.push_back(
                            spare
                                .pop()
                                .unwrap_or_else(|| Vec::with_capacity(MT_OUTPUT_CHUNK)),
                        );
                    }
                    let piece = out.back_mut().expect("just pushed");
                    let room = piece.capacity() - piece.len();
                    let n = room.min(rest.len());
                    piece.extend_from_slice(&rest[..n]);
                    rest = &rest[n..];
                }
                if let Some(ts) = ts {
                    sink_time += ts.elapsed();
                    blocks += 1;
                    if bytes.len() <= (4 << 20) {
                        small += bytes.len() as u64;
                    }
                }
            });
            self.out = out;
            self.spare = spare;
            self.note_delivered(handed);
            if let Some(t) = self.trace.as_mut() {
                t.drain += t0.elapsed();
                t.drains += 1;
                t.sink += sink_time;
                t.blocks += blocks;
                t.small_bytes += small;
                t.out_bytes += direct as u64 + self.out.iter().map(|p| p.len() as u64).sum::<u64>();
            }
            let status = match status {
                Ok(status) => status,
                Err(err) => {
                    self.control.finish();
                    return Err(decode_error(err));
                }
            };
            self.collect_checks();
            self.publish();

            match status {
                DrainStatus::Finished => {
                    self.finished = true;
                    self.control.finish();
                }
                DrainStatus::Progress => {}
                DrainStatus::NeedsMoreInput => {
                    let t1 = std::time::Instant::now();
                    let pumped = self.pump_input(self.backlog_wanted().max(1), false)?;
                    if let Some(t) = self.trace.as_mut() {
                        t.pump += t1.elapsed();
                    }
                    if pumped {
                        idle_turns = 0;
                    }
                    if !pumped && told_the_end && direct == 0 && self.out.is_empty() {
                        // End of the packed stream with no end marker: the
                        // stream is short, which is a corrupt archive rather
                        // than a decode that can continue.
                        self.control.finish();
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "LZMA2 stream ended without its end marker",
                        ));
                    }
                }
            }

            if direct > 0 {
                return Ok(direct);
            }
            if !self.finished && self.out.is_empty() {
                // Nothing came out of the drain and nothing more could be
                // fed, so every byte still owed is inside a worker and there
                // is nothing for this thread to do until one hands its run
                // back. Asking again instead is not free: it is a core the
                // workers are not getting, and at two threads that was half
                // the machine's useful capacity — the same loop was measured
                // at fourteen million drains over one archive. So it waits on
                // the worker, once, with nothing to poll and no deadline.
                //
                // `wait_for_worker` says at once, without waiting, that there
                // is no worker to wait for. That is the state this loop can
                // neither leave nor make progress in unless it is named: the
                // caller is owed bytes, the drain wants input, nothing is
                // outstanding to hand any back, and the feed above declined
                // because the decoder is already holding more than the
                // read-ahead target allows. Nobody is coming, so the target is
                // lifted and the stream is fed anyway — that is the only thing
                // that can make another byte, and it is why the state cannot
                // last.
                if !self.decoder.wait_for_worker() {
                    let t1 = std::time::Instant::now();
                    let fed = self.pump_input(1, true)?;
                    if let Some(t) = self.trace.as_mut() {
                        t.pump += t1.elapsed();
                    }
                    if fed {
                        idle_turns = 0;
                    } else {
                        // Nothing outstanding and nothing feedable, turn after
                        // turn. A short stream is caught above by its missing
                        // end marker; anything else that reaches here is a
                        // decode that cannot finish, and saying so is better
                        // than a thread that never returns.
                        idle_turns += 1;
                        if idle_turns >= MT_IDLE_TURNS_BEFORE_STALLED {
                            self.control.finish();
                            return Err(std::io::Error::other(
                                "LZMA2 decode stopped making progress",
                            ));
                        }
                    }
                } else {
                    idle_turns = 0;
                }
            }
        }
    }
}

fn decode_error(err: lzma_turbo::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

/// Builds the LZMA2 decoder for a 7z coder.
pub(crate) fn lzma2_decoder<R: Read>(
    input: R,
    dict_prop: u8,
    plan: Lzma2Plan,
) -> Result<Lzma2Coder<R>, std::io::Error> {
    match plan {
        Lzma2Plan::SingleThreaded => Ok(Lzma2Coder::SingleThreaded(Box::new(Lzma2Reader::new(
            input, dict_prop,
        )?))),
        Lzma2Plan::Adaptive {
            threads,
            memory_limit,
            control,
            splits,
        } => Ok(Lzma2Coder::Adaptive(Box::new(Lzma2MtReader::new(
            input,
            dict_prop,
            threads,
            memory_limit,
            control,
            &splits,
        )?))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dictionary is only ever as big as the output it could be read from.
    #[test]
    fn a_dictionary_is_clamped_to_the_output_it_serves() {
        // A gigabyte of dictionary for a kilobyte of output.
        assert_eq!(clamp_dictionary(1 << 30, 1024), 4096);
        // Never below the smallest dictionary the format expresses.
        assert_eq!(clamp_dictionary(1 << 30, 0), 4096);
        // A dictionary smaller than the output is left alone: the stream needs it.
        assert_eq!(clamp_dictionary(1 << 20, 1 << 30), 1 << 20);
        // And so is one that exactly fits.
        assert_eq!(clamp_dictionary(1 << 20, 1 << 20), 1 << 20);
    }

    /// LZMA2 carries the dictionary as a table index, so the clamp has to round
    /// back up to a value the table can express — never past what was declared.
    #[test]
    fn a_clamped_lzma2_property_stays_in_the_table() {
        // 16 MiB declared, 1 KiB of output: the smallest entry, 4 KiB.
        assert_eq!(lzma2_clamped_prop(24, 1024), 0);
        // 16 MiB declared for 10 MiB of output: the smallest entry that covers it.
        let prop = lzma2_clamped_prop(24, 10 << 20);
        assert!(u64::from(lzma2_dictionary_size(&[prop]).unwrap()) >= 10 << 20);
        assert!(prop < 24);
        // A stream that needs all of what it declared keeps it.
        assert_eq!(lzma2_clamped_prop(24, 1 << 30), 24);
        // A property byte the table does not have is left for the decoder to reject.
        assert_eq!(lzma2_clamped_prop(41, 1024), 41);
    }

    /// The property byte table, against the values the reference decoder
    /// computes for the ends and a midpoint of the range.
    #[test]
    fn lzma2_property_bytes_decode_like_the_reference() {
        assert_eq!(lzma2_dictionary_size(&[0]).unwrap(), 4096);
        assert_eq!(lzma2_dictionary_size(&[1]).unwrap(), 6144);
        assert_eq!(lzma2_dictionary_size(&[24]).unwrap(), 16 << 20);
        assert_eq!(lzma2_dictionary_size(&[40]).unwrap(), u32::MAX);
        assert!(lzma2_dictionary_size(&[41]).is_err());
        assert!(lzma2_dictionary_size(&[0x40]).is_err());
        assert!(lzma2_dictionary_size(&[]).is_err());
    }

    /// The documented rule: a budget too small for a parallel decode picks
    /// the single-threaded coder, and never an error.
    #[test]
    fn a_budget_too_small_for_threads_degrades_to_single_threaded() {
        let control = Arc::new(Lzma2Control::new(8));
        let dict = 32 << 20;

        // Room for the dictionary and 64 MiB besides: parallel.
        let plan = Lzma2Plan::for_block(
            8,
            false,
            (32 << 20) + (64 << 20) + LZ_STATE_BYTES,
            dict,
            &control,
            &[],
        );
        assert!(matches!(plan, Lzma2Plan::Adaptive { .. }));

        // Room for the dictionary and almost nothing else: single-threaded,
        // not a memory-limit error.
        let plan = Lzma2Plan::for_block(8, false, (32 << 20) + (1 << 20), dict, &control, &[]);
        assert!(matches!(plan, Lzma2Plan::SingleThreaded));
    }

    /// One thread is the default and costs nothing: no control block, no
    /// adaptive decoder, exactly the coder the fork shipped before.
    #[test]
    fn one_thread_is_the_plain_reader_unless_the_caller_asks_to_widen_later() {
        let control = Arc::new(Lzma2Control::new(1));
        assert!(matches!(
            Lzma2Plan::for_block(1, false, u64::MAX, 1 << 20, &control, &[]),
            Lzma2Plan::SingleThreaded
        ));
        assert!(matches!(
            Lzma2Plan::for_block(1, true, u64::MAX, 1 << 20, &control, &[]),
            Lzma2Plan::Adaptive { threads: 1, .. }
        ));
    }

    /// With no caller budget the in-flight ceiling is the backstop, which
    /// scales with the threads asked for. Nothing floors it: two threads are
    /// budgeted for two threads. It is a ceiling and not what the decode
    /// settles at; that is the backlog, which is tested on its own.
    #[test]
    fn an_unset_budget_is_the_backstop_for_the_thread_count() {
        assert_eq!(
            Lzma2Plan::mt_budget(8, u64::MAX, 32 << 20),
            Some(8 * MT_BACKSTOP_PER_THREAD_BYTES)
        );
        assert_eq!(
            Lzma2Plan::mt_budget(2, u64::MAX, 32 << 20),
            Some(2 * MT_BACKSTOP_PER_THREAD_BYTES)
        );
    }

    #[test]
    fn lzma_dictionary_comes_from_the_last_four_property_bytes() {
        let mut props = vec![0x5D];
        props.extend_from_slice(&(8u32 << 20).to_le_bytes());
        assert_eq!(lzma_dictionary_size(&props).unwrap(), 8 << 20);
        assert!(lzma_dictionary_size(&[0x5D, 0, 0]).is_err());
    }
}

/// The reader against streams whose run layout is chosen rather than coaxed
/// out of an encoder, which is what the paths that hand the decoder a partial
/// run turn on.
///
/// Every test here decodes to the end. That is the assertion: a reader that
/// stops feeding a decoder that has been told not to chase, or switches
/// chasing off where nothing else can make progress, does not produce wrong
/// bytes — it produces no more bytes at all, and the test never returns.
#[cfg(test)]
mod stall_tests {
    use std::io::{Cursor, ErrorKind, Read};
    use std::sync::Arc;

    use super::{Lzma2Control, Lzma2MtReader};

    /// 512 KiB, comfortably more than one run of the streams below.
    const DICT_PROP: u8 = 14;
    /// Small enough that the read-ahead ceiling is reached inside a stream of
    /// a few megabytes, rather than never.
    const LIMIT: u64 = 8 << 20;
    const CHUNK: usize = 64 << 10;
    /// A decoder holding nothing takes a little input whatever its limit says,
    /// because one that took none could never start. So a limit is a limit on
    /// what is held, give or take the one piece that gets a stalled decode
    /// moving again.
    const LIMIT_SLACK: u64 = 64 << 10;

    /// Deterministic bytes, so that a run decoded in the wrong place shows up
    /// as a mismatch rather than as more of the same.
    fn payload(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    /// An LZMA2 stream of `runs` runs of `chunks` chunks each, and the bytes
    /// it decodes to.
    ///
    /// The chunks copy their payload out verbatim — control byte, the
    /// payload's length less one as a big-endian `u16`, the payload — and the
    /// first chunk of each run asks for the dictionary reset that makes it a
    /// run. Built rather than compressed because the run layout is the whole
    /// point: an encoder has to be talked into one, and then only
    /// approximately.
    fn stream(runs: usize, chunks: usize) -> (Vec<u8>, Vec<u8>) {
        let mut packed = Vec::new();
        let mut plain = Vec::new();
        for run in 0..runs {
            for chunk in 0..chunks {
                let bytes = payload(CHUNK, (run * chunks + chunk) as u64 + 1);
                packed.push(if chunk == 0 { 0x01 } else { 0x02 });
                packed.extend_from_slice(&((CHUNK - 1) as u16).to_be_bytes());
                packed.extend_from_slice(&bytes);
                plain.extend_from_slice(&bytes);
            }
        }
        // The end marker.
        packed.push(0x00);
        (packed, plain)
    }

    fn reader<R: Read>(input: R, threads: u32) -> (Lzma2MtReader<R>, Arc<Lzma2Control>) {
        reader_limited(input, threads, LIMIT)
    }

    fn reader_limited<R: Read>(
        input: R,
        threads: u32,
        limit: u64,
    ) -> (Lzma2MtReader<R>, Arc<Lzma2Control>) {
        let control = Arc::new(Lzma2Control::new(threads));
        let rd = Lzma2MtReader::new(input, DICT_PROP, threads, limit, Arc::clone(&control), &[])
            .expect("build the reader");
        (rd, control)
    }

    /// What one run of `stream(_, chunks)` costs a decoder holding it: the
    /// bytes it was handed and the bytes it makes from them.
    fn run_cost(chunks: usize) -> u64 {
        run_packed(chunks) + run_unpacked(chunks)
    }

    fn run_packed(chunks: usize) -> u64 {
        (chunks * (CHUNK + 3)) as u64
    }

    fn run_unpacked(chunks: usize) -> u64 {
        (chunks * CHUNK) as u64
    }

    /// The most a decode of runs of `chunks` chunks may be found holding
    /// against a caller limit of `limit`.
    ///
    /// It is the limit plus two things the decoder keeps outside it, neither
    /// of which this reader can do anything about from here — both are
    /// written up in `docs/lzma-turbo-requests.md`:
    ///
    /// * a run inside a worker is counted twice, as the copy the worker was
    ///   handed and as the input it was copied from, which is let go of only
    ///   once the run lands; so every run that fits inside the limit costs its
    ///   packed bytes a second time while it is out; and
    /// * the streaming path decodes what it is holding whether or not there is
    ///   room for the output, which is up to one run of it.
    ///
    /// What the limit does hold, and is asserted here to hold, is the part
    /// that grows with the archive: neither term depends on how much is left
    /// to decode.
    fn limit_ceiling(limit: u64, chunks: usize) -> u64 {
        let fit = limit / run_cost(chunks);
        limit + fit * run_packed(chunks) + run_unpacked(chunks) + LIMIT_SLACK
    }

    /// Reads to the end, reporting the largest in-flight count seen on the
    /// way. The reads are small so that the decode is looked at often.
    fn drain_watching<R: Read>(
        rd: &mut Lzma2MtReader<R>,
        control: &Arc<Lzma2Control>,
    ) -> (Vec<u8>, u64, u32) {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 16 << 10];
        let mut peak = 0;
        let mut widest = 0;
        loop {
            if let Some(p) = control.progress() {
                peak = peak.max(p.in_flight_bytes);
                widest = widest.max(p.spawned_threads);
            }
            let n = rd.read(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        (out, peak, widest)
    }

    /// A source that hands over a few bytes at a time, as a socket would.
    struct Trickle<R> {
        inner: R,
        most: usize,
    }

    impl<R: Read> Read for Trickle<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.most);
            self.inner.read(&mut buf[..n])
        }
    }

    /// The ordinary path: whole runs handed over, the decoder told not to
    /// chase them, and the same bytes out of it at every thread count.
    #[test]
    fn a_multi_run_stream_decodes_the_same_at_every_thread_count() {
        let (packed, plain) = stream(48, 4);
        for threads in [1, 2, 4, 8] {
            let (mut rd, _control) = reader(Cursor::new(packed.clone()), threads);
            let mut out = Vec::new();
            rd.read_to_end(&mut out).expect("decode");
            assert_eq!(out, plain, "threads={threads}");
            assert!(rd.runs_seen > 1, "threads={threads}");
        }
    }

    /// The same stream arriving a few bytes per read, which is what puts a
    /// run boundary out of reach for call after call.
    #[test]
    fn a_stream_that_arrives_in_small_pieces_decodes() {
        let (packed, plain) = stream(8, 4);
        let (mut rd, _control) = reader(
            Trickle {
                inner: Cursor::new(packed),
                most: 17,
            },
            4,
        );
        let mut out = Vec::new();
        rd.read_to_end(&mut out).expect("decode");
        assert_eq!(out, plain);
    }

    /// A stream written as one run from beginning to end. No boundary is ever
    /// in reach, so the reader gives up reading ahead and switches the chase
    /// path on; with it left off nothing would decode at all.
    #[test]
    fn a_single_run_stream_decodes_once_reading_ahead_is_given_up_on() {
        let (packed, plain) = stream(1, 64);
        let (mut rd, _control) = reader(Cursor::new(packed), 4);
        // The thresholds the give-up is made at, brought down to the size of
        // a stream a test can hold: a quarter of a gigabyte of one run says
        // nothing that four megabytes of it does not.
        rd.give_up_bytes = 64 << 10;
        rd.hold_bytes = 256 << 10;

        let mut out = Vec::new();
        let mut buf = vec![0u8; 32 << 10];
        let mut chased = false;
        loop {
            let n = rd.read(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            chased |= rd.chasing;
            out.extend_from_slice(&buf[..n]);
        }
        assert!(
            chased,
            "a stream with one run in it is decoded by chasing it"
        );
        // One, and only once the end marker closed it: for the whole of the
        // decode there was no boundary to feed up to.
        assert_eq!(rd.runs_seen, 1);
        assert_eq!(out, plain);
    }

    /// A stream cut part way through a run: the decoder is owed input that is
    /// not coming, and must say so rather than wait for it.
    #[test]
    fn a_truncated_stream_is_refused_rather_than_waited_on() {
        let (packed, _) = stream(8, 4);
        let cut = packed.len() - (100 << 10);
        for threads in [1, 2, 4] {
            let (mut rd, _control) = reader(Cursor::new(packed[..cut].to_vec()), threads);
            let mut out = Vec::new();
            let err = rd
                .read_to_end(&mut out)
                .expect_err("a cut stream is corrupt");
            assert!(
                matches!(
                    err.kind(),
                    ErrorKind::UnexpectedEof | ErrorKind::InvalidData
                ),
                "threads={threads}: {err}"
            );
        }
    }

    /// The adaptive narrowing a consumer following an arriving stream does:
    /// one thread through the middle of the block and several either side.
    /// A decoder narrowed to one thread decodes on the calling thread whatever
    /// it has been told about chasing, so output has to keep coming.
    #[test]
    fn narrowing_to_one_thread_mid_stream_keeps_producing() {
        let (packed, plain) = stream(32, 4);
        let (mut rd, control) = reader(Cursor::new(packed), 4);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 32 << 10];
        loop {
            let narrow = out.len() > (2 << 20) && out.len() < (6 << 20);
            control.set_threads(if narrow { 1 } else { 4 });
            let n = rd.read(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, plain);
    }

    /// The whole of the chase a consumer following a download drives: one
    /// thread while it is on the tail, several once a backlog has built up,
    /// one again, and several again — all inside a single block, over input
    /// that arrives a few bytes at a time.
    ///
    /// Each narrowing hands the decode back to the calling thread and each
    /// widening takes it away again, at whatever run boundary comes next, so
    /// this is where a feed that stopped in the wrong place or a chase left
    /// switched off would show up as a decode that stops.
    #[test]
    fn widening_and_narrowing_repeatedly_mid_stream_keeps_producing() {
        let (packed, plain) = stream(24, 4);
        let (mut rd, control) = reader(
            Trickle {
                inner: Cursor::new(packed),
                most: 29,
            },
            4,
        );
        let mut out = Vec::new();
        let mut buf = vec![0u8; 16 << 10];
        let mut widest = 0;
        loop {
            // One thread either side of a widened middle, and widened again
            // for the tail: four crossings in one block.
            let megabytes = out.len() >> 20;
            control.set_threads(if matches!(megabytes, 0 | 3) { 1 } else { 4 });
            if let Some(p) = control.progress() {
                widest = widest.max(p.spawned_threads);
            }
            let n = rd.read(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, plain);
        assert!(
            widest > 1,
            "the widened stretches decoded on more than one thread"
        );
    }

    /// A caller whose limit is smaller than a single run of the stream it
    /// asked for. No worker can be given a run that does not fit, so the
    /// decode falls to the path that streams one out of its dictionary — and
    /// waiting for room that is never coming would be a deadlock.
    #[test]
    fn a_limit_smaller_than_one_run_streams_instead_of_waiting() {
        let (packed, plain) = stream(12, 4);
        let limit = run_cost(4) / 2;
        let (mut rd, control) = reader_limited(Cursor::new(packed), 8, limit);
        let (out, peak, widest) = drain_watching(&mut rd, &control);
        assert_eq!(out, plain);
        assert_eq!(widest, 0, "no run ever fit inside the limit");
        let ceiling = limit_ceiling(limit, 4);
        assert!(
            peak <= ceiling,
            "held {peak} against a limit of {limit} (ceiling {ceiling})"
        );
    }

    /// The state the read loop used to have no way out of: the reader has
    /// read as far ahead as it is meant to, and no worker is outstanding to
    /// turn any of it into output.
    ///
    /// Fed and scanned without ever being drained for output, a decode
    /// reaches that state and stays in it: the ordinary feed declines, turn
    /// after turn, on a backlog that is already full, and nothing claims the
    /// backlog. What used to follow was a loop with nothing to do and no
    /// reason to stop doing it. The escape is the last feed here, and what is
    /// asserted is that it moves when the ordinary one cannot: with it the
    /// state lasts a single turn, and without it for as long as the caller is
    /// prepared to wait.
    #[test]
    fn a_feed_past_its_target_is_what_leaves_the_idle_state() {
        let (packed, _plain) = stream(24, 4);
        let (mut rd, _control) = reader_limited(Cursor::new(packed), 1, 64 * run_cost(4));
        // Feed, and scan what was fed without taking any output, until the
        // backlog is as full as reading ahead is meant to make it.
        let mut turns = 0;
        while !rd.backlog_full() {
            rd.pump_input(1, false).expect("feed");
            rd.decoder.drain_upto(0, |_, _| {}).expect("scan");
            turns += 1;
            assert!(turns < 1000, "the backlog never filled");
        }
        let fed_before = rd.fed_total;
        assert!(rd.fed_at_boundary(), "the feed stops at a boundary");
        // The state, entered: the ordinary feed has nothing more to give.
        assert!(!rd.pump_input(1, false).expect("feed"));
        // The escape, taken.
        assert!(
            rd.pump_input(1, true).expect("feed past the target"),
            "a feed that ignores the backlog has to move when the backlog cannot"
        );
        assert!(
            rd.fed_total > fed_before,
            "and to have put bytes into the decoder, not just returned"
        );
    }

    /// A limit with room for about two runs, against eight threads asking for
    /// eight. The decode has to go on with the parallelism the limit leaves
    /// rather than wait for seven lots of room that are not coming, and it
    /// has to stay inside the limit while it does.
    #[test]
    fn a_limit_of_two_runs_decodes_on_the_threads_it_can_afford() {
        let (packed, plain) = stream(16, 4);
        let limit = 2 * run_cost(4);
        let (mut rd, control) = reader_limited(Cursor::new(packed), 8, limit);
        let (out, peak, _widest) = drain_watching(&mut rd, &control);
        assert_eq!(out, plain);
        let ceiling = limit_ceiling(limit, 4);
        assert!(
            peak <= ceiling,
            "held {peak} against a limit of {limit} (ceiling {ceiling})"
        );
        // Sixteen runs in the stream and room for two: what is held is the
        // limit's business and not the archive's.
        assert!(peak < 4 * run_cost(4), "held {peak}");
    }
    /// What the read-ahead is measured by, and the only thing it is measured
    /// by: complete runs waiting for a thread that can decode them.
    ///
    /// A limit decides how many of those threads there are. Given room for
    /// one run it is one thread however many were asked for, given room for
    /// two it is two, and given room for all of them it is all of them — the
    /// caller's count is the ceiling, and one is the floor, because a decode
    /// that can only hold one run at a time is still a decode and must not
    /// stop reading for it.
    #[test]
    fn the_backlog_is_sized_by_the_threads_the_limit_affords() {
        let cost = run_cost(4);
        for (limit, afford) in [
            (cost, 1),
            (cost + cost / 2, 1),
            (2 * cost, 2),
            (5 * cost, 5),
            (100 * cost, 8),
            (cost / 4, 1),
        ] {
            let (packed, _plain) = stream(4, 4);
            let (rd, _control) = reader_limited(Cursor::new(packed), 8, limit);
            // Nothing has been fed, so no run size is known and every thread
            // still looks affordable.
            assert_eq!(rd.affordable_threads(), 8, "limit={limit} before any run");
            let mut rd = rd;
            rd.pump_input(1, false).expect("feed");
            assert_eq!(
                rd.affordable_threads(),
                afford,
                "limit={limit} is {afford} run(s) of {cost}"
            );
            assert_eq!(
                rd.backlog_target(),
                afford * super::MT_BACKLOG_RUNS_PER_THREAD,
                "limit={limit}"
            );
        }
    }

    /// Read-ahead reads ahead, and stops: a decode of a long stream must not
    /// pull the whole packed stream in behind it just because the stream is
    /// there.
    #[test]
    fn a_full_backlog_is_what_stops_the_feed() {
        let (packed, plain) = stream(64, 4);
        let whole = packed.len() as u64;
        let (mut rd, _control) = reader_limited(Cursor::new(packed), 2, 64 * run_cost(4));
        // Two threads, two runs each: four runs of work in hand, not
        // sixty-four.
        assert_eq!(rd.backlog_target(), 4);
        let mut out = vec![0u8; 8 << 10];
        let n = rd.read(&mut out).expect("decode");
        assert!(n > 0);
        assert!(
            rd.fed_total < whole / 4,
            "read {} of {whole} packed bytes ahead to make {n} bytes of output",
            rd.fed_total
        );
        // And it still decodes the whole thing.
        let mut rest = out[..n].to_vec();
        rd.read_to_end(&mut rest).expect("decode the rest");
        assert_eq!(rest, plain);
    }

    /// Decodes a stream of one-megabyte runs whole, watching the backlog at
    /// every turn of the read loop. Answers with the most packed backlog and
    /// the most waiting runs it ever held, and checks the output on the way.
    ///
    /// The per-thread byte ceiling is the parameter because it is what sorts
    /// streams into run-size regimes: a ceiling of many runs is what a stream
    /// of small runs looks like, and a ceiling of less than one run is what a
    /// stream of very large ones looks like. What is asserted is a ceiling on
    /// what was held and not the figure it stopped at, because how far the
    /// backlog is drawn down between feeds is the workers' business and not
    /// this rule's.
    fn most_held_decoding(threads: u32, per_thread_bytes: u64) -> (u64, u64) {
        let (packed, plain) = stream(48, 16);
        let (mut rd, _control) = reader(Cursor::new(packed), threads);
        rd.backlog_bytes = per_thread_bytes;
        let (mut most_bytes, mut most_runs) = (0, 0);
        let mut got = Vec::new();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            most_bytes = most_bytes.max(rd.backlog_packed());
            most_runs = most_runs.max(rd.decoder.pending_runs() as u64);
            match rd.read(&mut buf).expect("decode") {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, plain, "threads={threads} cap={per_thread_bytes}");
        (most_bytes, most_runs)
    }

    /// What the read-ahead is for is a run waiting when a worker comes free,
    /// and what it costs is the bytes of every run waiting that no worker can
    /// start on. Which of those dominates is a question about the size of a
    /// run, so the rule answers it per stream rather than once.
    ///
    /// Small runs cost so little that the count rule decides and the bytes
    /// never come near their ceiling. Runs the size of the ceiling itself are
    /// held to about a thread's worth. Runs larger than the whole ceiling are
    /// held to what a free worker needs and no further, because a third such
    /// run would be a hundred megabytes nobody can use.
    ///
    /// Each regime is checked by what it may hold at most, which the feed rule
    /// decides on its own, rather than by what it happened to be holding when
    /// the workers last came up for air.
    #[test]
    fn the_read_ahead_is_what_the_run_size_asks_for() {
        let run = run_packed(16);
        let threads = 4u32;
        let floor_runs = u64::from(threads) + 1;

        // Small runs: two per thread, and never the byte ceiling, which is
        // sixty-four runs per thread away.
        let (bytes, runs) = most_held_decoding(threads, 64 * run);
        assert!(
            runs <= u64::from(threads) * super::MT_BACKLOG_RUNS_PER_THREAD + 1,
            "small runs held {runs} runs"
        );
        assert!(
            bytes < 64 * run * u64::from(threads),
            "small runs held {bytes} bytes of a ceiling of {}",
            64 * run * u64::from(threads)
        );

        // Runs the size of the ceiling: a thread's worth each, so the count
        // rule's two per thread is never reached.
        let (bytes, _runs) = most_held_decoding(threads, run);
        assert!(
            bytes <= (u64::from(threads) + floor_runs) * run,
            "runs the ceiling's size held {bytes} bytes"
        );

        // Runs larger than the ceiling: the floor, and the run the feed was in
        // the middle of when it reached it.
        let (bytes, runs) = most_held_decoding(threads, run / 4);
        assert!(
            bytes <= (floor_runs + 1) * run,
            "large runs held {bytes} bytes, past a floor of {floor_runs} runs"
        );
        assert!(
            runs <= floor_runs + 1,
            "large runs held {runs} runs, past a floor of {floor_runs}"
        );
    }

    /// The floor is a floor: however large the runs are, and whatever the byte
    /// ceiling says, the feed does not stop while a worker could come free and
    /// find nothing to take.
    #[test]
    fn a_free_worker_always_finds_a_run_waiting() {
        let (packed, plain) = stream(48, 16);
        let (mut rd, _control) = reader(Cursor::new(packed), 4);
        // A ceiling below one run: the byte rule would stop the feed at once
        // if the floor did not hold it open.
        rd.backlog_bytes = run_packed(16) / 8;
        let mut got = Vec::new();
        let mut buf = vec![0u8; 64 << 10];
        loop {
            // Whenever the read-ahead calls itself finished, it is holding at
            // least what a worker coming free would need.
            if rd.backlog_full() {
                let pending = rd.decoder.pending_runs() as u64;
                assert!(
                    pending >= rd.demand_floor(),
                    "stopped at {pending} runs with a floor of {}",
                    rd.demand_floor()
                );
            }
            match rd.read(&mut buf).expect("decode") {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, plain);
    }
}
