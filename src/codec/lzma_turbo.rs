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
use std::sync::{Arc, Condvar, Mutex};

use lzma_turbo::crc::CrcFolder;
use lzma_turbo::{
    Checksum, ChecksumPlan, DrainStatus, Lzma2AdaptiveDecoder, Lzma2MtOptions, Lzma2Reader,
    Lzma2Run, Lzma2RunScanner, LzmaProps, LzmaReader,
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

/// Bytes read from the coder below in one go, and so the size of a piece this
/// reader hands over.
///
/// A piece is taken or refused whole, so this is also the granularity the
/// decoder's input budget is spent in: too small and the decode pays a read
/// and a queue entry per fraction of a run, too large and a piece is refused
/// for a want the next drain would have covered. Measured on an incompressible
/// archive at four threads, where the input path is most of the decode.
const MT_INPUT_READ_BYTES: usize = 4 << 20;

/// The smallest read [`Lzma2MtReader::refill`] makes when the stream is
/// expected to be nearly over. See [`Lzma2MtReader::input_left`].
const MT_INPUT_MIN_READ_BYTES: usize = 64 << 10;

/// How much of a refused piece is copied over so that the decoder sees the
/// header at its front. One byte would do — the decoder closes a run on the
/// control byte after it — and a page is the smallest copy worth making. See
/// [`Lzma2MtReader::declare_run`].
const MT_DECLARE_BYTES: usize = 4096;

/// The most an LZMA2 stream that decodes to `unpacked_len` bytes is expected
/// to take in, used only to size reads.
///
/// An encoder stores a chunk it cannot shrink, so an LZMA2 stream is its
/// output plus chunk headers: three bytes per stored chunk of up to 64 KiB,
/// six per LZMA chunk, and the end marker. A thousandth plus 64 KiB covers
/// that with room to spare. A stream longer than this is still read to its
/// end, in [`MT_INPUT_MIN_READ_BYTES`] pieces: the bound is a hint and
/// nothing is refused for exceeding it.
fn lzma2_packed_bound(unpacked_len: u64) -> u64 {
    unpacked_len
        .saturating_add(unpacked_len / 1024)
        .saturating_add(MT_INPUT_MIN_READ_BYTES as u64)
}

/// The size of one piece of buffered output. See [`Lzma2MtReader::out`].
const MT_OUTPUT_CHUNK: usize = 1 << 20;

/// Pieces of buffered output kept for refilling. See [`Lzma2MtReader::spare`].
const MT_OUTPUT_SPARE_PIECES: usize = 8;

/// Smallest block, in decoded bytes, that is worth decoding in parallel.
///
/// At or below this the block is decoded single-threaded whatever the caller
/// asked for. A stream this small is almost always a single run: 7-Zip's and
/// `lzma-turbo`'s multi-threaded encoders cut a run every `max(4 x dict, 1
/// MiB)` bytes, so nothing they write is split any finer than this, and a
/// stream of one run is decoded on the calling thread by the parallel path
/// too, after it has started workers, allocated its read-ahead and checksummed
/// through its fold. A non-solid archive of small files is thousands of such
/// blocks, and paid that setup on every one of them for no parallelism at all.
///
/// The knee is that minimum run and not a property of the machine: measured
/// on x86 and on arm64 alike, the parallel path first wins at the second run,
/// and a block of exactly one run decoded in parallel was 4.5% slower.
///
/// The block's declared size is what is compared, and it is only a hint here:
/// a stream that is larger than it says is still decoded correctly by the
/// single-threaded decoder, just not in parallel.
pub(crate) const MT_MIN_BLOCK_BYTES: u64 = 1 << 20;

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

/// The share of a run's output, as one part in this many, that may come from
/// LZMA-coded chunks for the run still to count as stored.
///
/// The shape the narrow decode is for is the stored one. An encoder that
/// cannot beat a chunk writes it out as it stands — an LZMA2 uncompressed
/// chunk — and decoding that is a copy, bound by the read. A chunk that is
/// LZMA-coded is the opposite however little it shrank: data that barely
/// compresses is literal after literal, the slowest LZMA there is, and
/// decoding it is bound by the decode, so it wants every thread it can get.
///
/// Which of the two a chunk is, its header says, and [`RunShape`] carries the
/// count from the run record: the split is exact, not read off a ratio. The
/// ratio was what this used to go by — a run packed at least as large as it
/// unpacked was taken to be stored — and it is the wrong question, because an
/// LZMA chunk of media packed at 99.5 percent and a stored chunk at 100.005
/// are a few hundredths apart in size and two orders of magnitude apart in
/// what they cost to decode. Drawn at 90 percent, the ratio held media at an
/// ordinary level to two threads, five times slower than 7-Zip.
///
/// The cost gap is also why a run is not split down the middle: decoding a
/// byte of incompressible LZMA costs about a hundred times what copying one
/// does, so a run with a few percent of its output LZMA-coded already spends
/// as long decoding as copying. At one part in 64 the LZMA chunks cost about
/// as much as the copy, and past it they are what the decode is waiting on.
const MT_STORED_LZMA_SHARE: u64 = 64;

/// What one run of an LZMA2 stream is made of, as the decode's shape is
/// decided from it: the classification input of [`Lzma2MtReader`].
///
/// It is the run record's own figures — the packed and unpacked sizes and how
/// much of the output comes from LZMA-coded rather than stored chunks — so
/// that whatever decides how wide a decode goes asks the chunk headers and
/// not a ratio. [`RunShape::is_stored`] is the one rule this module applies
/// to it; a planner that wants another can read the same fields.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RunShape {
    /// The run's length in the packed stream, chunk headers included.
    pub(crate) packed: u64,
    /// What the run decodes to.
    pub(crate) unpacked: u64,
    /// The part of [`Self::unpacked`] that LZMA-coded chunks decode to. The
    /// rest comes from stored chunks, whose decode is a copy.
    pub(crate) lzma_unpacked: u64,
}

impl RunShape {
    /// The shape of a run the scanner closed.
    pub(crate) fn of(run: &Lzma2Run) -> Self {
        Self {
            packed: run.packed_len,
            unpacked: run.unpacked_len,
            lzma_unpacked: run.chunks.lzma_unpacked,
        }
    }

    /// Whether decoding the run is a copy rather than a decode: no more than
    /// one part in [`MT_STORED_LZMA_SHARE`] of its output is LZMA-coded. An
    /// empty run is not stored; it is nothing at all.
    pub(crate) fn is_stored(&self) -> bool {
        self.unpacked > 0
            && u128::from(self.lzma_unpacked) * u128::from(MT_STORED_LZMA_SHARE)
                <= u128::from(self.unpacked)
    }
}

/// Threads an incompressible stream — one whose runs are stored chunks, see
/// [`RunShape::is_stored`] — is decoded with, however many the caller asked
/// for.
///
/// Such a stream is read-bound rather than decode-bound: its runs are large,
/// every byte of them has to be moved, and the decoding between the reads is
/// almost nothing. Threads beyond a couple therefore buy no wall at all —
/// measured, an archive of a gigabyte and a half of incompressible payload
/// decoded no faster at eight threads than at two — while each of them holds a
/// run of its own, which on that archive was the difference between three
/// quarters of a gigabyte resident and three gigabytes.
///
/// It also sets what such a stream costs: the read-ahead an incompressible
/// decode may spend is this many threads' worth of
/// [`MT_BACKSTOP_PER_THREAD_BYTES`] — 768 MiB as both stand today — or the
/// caller's limit where that is lower, so changing either constant moves the
/// incompressible lane's footprint.
const MT_DENSE_THREADS: u32 = 2;

/// Runs in a row that must disagree with the shape in hand before it changes.
///
/// An archive is not all one thing: a film beside a text file gives a stream
/// whose runs change shape part way through, and the decode should narrow for
/// the one and widen again for the other. What it must not do is follow every
/// single run, because an encoder that meets one compressible megabyte in the
/// middle of a film writes one compressible run, and a thread count that went
/// wide there and narrow again would cost more than either shape saves.
const MT_SHAPE_RUNS: u32 = 2;

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

/// The longest run an LZMA2 encoder that cuts its stream into runs writes for
/// a dictionary of `dict_size`, in decoded bytes.
///
/// C: the `LZMA2_ENC_PROPS_BLOCK_SIZE_AUTO` case of `Lzma2EncProps_Normalize`:
/// four dictionaries, clamped to 1..=256 MiB, never less than one dictionary,
/// rounded up to a whole mebibyte. `lzma-turbo`'s encoder and this crate's
/// writer cut runs by the same rule, and xz's three dictionaries are shorter.
///
/// It is what lets a reader call a stream single-run long before
/// [`MT_NO_WORKER_GIVE_UP_BYTES`] or [`MT_INPUT_HOLD_BYTES`] would: a first run
/// that has gone on past this without a dictionary reset was not written to be
/// decoded in parallel, and every byte read ahead waiting for its end is held
/// for nothing. That was 192 MiB of a single-run archive held for a parallel
/// plan that never happened, and a single-run block under that size read
/// whole before a byte of it was decoded. The dictionary is the one the coder
/// declares, which is never smaller than the one the encoder used, so the cap
/// errs long.
///
/// A caller-chosen block size (`7zz -m0=lzma2:c=…`) can be longer than this.
/// Such a stream's first run is then decoded on the calling thread, as a run
/// longer than [`MT_INPUT_HOLD_BYTES`] already is, and the decode goes wide
/// again from the second run, whose boundary the scan still finds.
fn lzma2_encoder_run_cap(dict_size: u32) -> u64 {
    const MIB: u64 = 1 << 20;
    let dict = u64::from(dict_size);
    (dict * 4).clamp(MIB, 256 * MIB).max(dict).div_ceil(MIB) * MIB
}

/// Packed bytes this reader will hold that it has not been able to hand to the
/// decoder, because they are part of a run whose end has not been seen yet.
///
/// The bound matters only for a stream whose runs are larger than it. There the
/// decoder runs out of work, the reader switches the chase path on and feeds
/// these bytes as they come; the cap is what stops this reader from reading an
/// entire single-run archive into memory while it waits for a boundary that is
/// not coming.
const MT_INPUT_HOLD_BYTES: usize = 192 * 1024 * 1024;

/// What a decode that may be widened may read ahead beyond doubling its
/// threads, so that a stream of small runs offers its caller every thread it
/// could use at the first look rather than doubling towards it a look at a
/// time. See [`Lzma2MtReader::sync_threads`]. One 128 MiB run in and out, so a
/// stream of large runs still doubles, and a caller held at a ceiling below
/// the offer holds at most this much more than a decode fixed at it.
const MT_OFFER_BYTES: u64 = 256 << 20;

/// Decoded size of a run from which a decode that may be widened listens for
/// its caller while it waits for a worker, rather than waiting on the worker
/// alone. See [`Lzma2MtReader::listening`].
///
/// A wait on a worker cannot be interrupted, and only this thread can hand
/// the decoder a run, so a widening that arrives during one is applied when
/// the run lands. A run this size takes tens of milliseconds to decode, which
/// is what that costs; a 128 MiB run takes seconds. Below it a worker is back
/// before listening would have bought anything.
const MT_LISTEN_RUN_BYTES: u64 = 16 << 20;

/// How long a listening decode waits for its caller before it looks at its
/// workers again. See [`Lzma2MtReader::listening`]: this is the most a run
/// that lands during the wait is left unread for, against runs that take
/// seconds.
const MT_LISTEN_SLICE: std::time::Duration = std::time::Duration::from_millis(5);

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
    /// Bumped by every [`Lzma2Control::set_threads`], which is what a decode
    /// listening for its caller waits on. See [`Lzma2MtReader::listening`].
    changes: Mutex<u64>,
    changed: Condvar,
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
            changes: Mutex::new(0),
            changed: Condvar::new(),
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
        if let Ok(mut changes) = self.changes.lock() {
            *changes = changes.wrapping_add(1);
        }
        self.changed.notify_all();
    }

    /// Waits until the thread count differs from `applied`, or for `slice`.
    fn wait_for_change(&self, applied: u32, slice: std::time::Duration) {
        let Ok(changes) = self.changes.lock() else {
            return;
        };
        // Asked under the lock, which every change takes after storing, so a
        // change cannot land between the look and the wait and go unheard.
        if self.threads() != applied {
            return;
        }
        let seen = *changes;
        let _ = self
            .changed
            .wait_timeout_while(changes, slice, |changes| *changes == seen);
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
///
/// A reader with a [positional source] and more than one thread decodes runs
/// of small folders several at a time. No one coder is the reader's during
/// such a run, so `progress` is `None` then. The run starts at the reader's
/// thread count, as a block's coder does, and `set_threads` bounds how many
/// of its folders decode at once and on how many threads each, from the next
/// folder to start.
///
/// [positional source]: crate::ArchiveReader::set_positional_source
#[derive(Debug, Clone)]
pub struct Lzma2Handle {
    pub(crate) control: Arc<Lzma2Control>,
}

impl Lzma2Handle {
    /// Sets the thread ceiling, effective at the next run boundary. One means
    /// one run is decoded at a time.
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
/// and is therefore decodable on its own. The count of complete runs in hand
/// behind the one at the front of the output is the backlog an adaptive caller
/// widens on: while it is zero the stream is being chased and there is nothing
/// to parallelise; while it grows there is work that more threads would finish
/// sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lzma2Progress {
    /// The block whose LZMA2 coder this describes.
    pub block_index: usize,
    /// Thread ceiling currently in force. One means one run is decoded at a
    /// time.
    pub threads: u32,
    /// Worker threads that exist. Zero until a run is actually dispatched, so
    /// a decoder that never widens never creates one.
    pub spawned_threads: u32,
    /// Complete runs in hand behind the one at the front of the output: the
    /// backlog. A run counts whether a worker has it yet or not, so a caller
    /// widening by one thread per run keeps the threads it widened to until
    /// their runs are done. Never more than the stream's shape lets the decode
    /// use: an incompressible stream reports at most one.
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
        /// What the block declares it decodes to, which sizes the reads.
        unpacked_len: u64,
        /// The widest a caller may take this decode while it runs, which is
        /// what the read-ahead offers it room to widen into. The thread
        /// count for a coder whose caller will not widen it. See
        /// [`Lzma2MtReader::sync_threads`].
        widen_to: u32,
    },
}

impl Lzma2Plan {
    /// Chooses how to decode one block's LZMA2 coder.
    ///
    /// `threads` is the caller's live ceiling and `adaptive` is whether the
    /// caller asked for a coder that can be widened later even though the
    /// ceiling is one right now. `memory_limit_bytes` is
    /// [`ArchiveLimits::memory_limit_bytes`], `dict_size` the dictionary
    /// this coder declares, and `unpacked_len` the bytes it declares it
    /// decodes to: a block no larger than [`MT_MIN_BLOCK_BYTES`] is decoded
    /// single-threaded, adaptive or not, because there is nothing in it to
    /// widen to.
    ///
    /// # The rule when the budget is too small
    ///
    /// A parallel decode holds whole runs, so it needs room the dictionary
    /// number says nothing about. What is left of the caller's budget after
    /// the decoder's own footprint is the in-flight budget; when the caller
    /// set no budget at all it is the backstop, `threads` x
    /// [`MT_BACKSTOP_PER_THREAD_BYTES`], where an adaptive coder counts the
    /// threads it may widen to rather than the one it starts at. Either way it is a ceiling and not a
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
        unpacked_len: u64,
        control: &Arc<Lzma2Control>,
        splits: &[u64],
    ) -> Self {
        if (threads <= 1 && !adaptive) || unpacked_len <= MT_MIN_BLOCK_BYTES {
            return Self::SingleThreaded;
        }
        let budgeted = Self::budgeted_threads(threads, adaptive, memory_limit_bytes);
        let Some(memory_limit) = Self::mt_budget(budgeted, memory_limit_bytes, dict_size) else {
            return Self::SingleThreaded;
        };
        Self::Adaptive {
            threads,
            memory_limit,
            control: Arc::clone(control),
            splits: splits.to_vec(),
            unpacked_len,
            widen_to: if adaptive {
                Self::machine_threads().max(threads)
            } else {
                threads
            },
        }
    }

    /// The machine's parallelism, clamped to the 256 threads a decoder takes.
    fn machine_threads() -> u32 {
        let parallelism = std::thread::available_parallelism().map_or(1, |n| n.get());
        u32::try_from(parallelism).unwrap_or(u32::MAX).min(256)
    }

    /// The thread count the in-flight budget is sized for.
    ///
    /// The budget is fixed when the coder is built, but an adaptive coder is
    /// widened afterwards, so with no caller limit it is sized for the widest
    /// it may usefully go: the machine's parallelism, or the threads asked for
    /// if that is more. Sized for the one thread it starts at, the backstop
    /// never holds enough runs to widen at all. A caller limit is the budget
    /// as given, so this only matters without one.
    fn budgeted_threads(threads: u32, adaptive: bool, memory_limit_bytes: u64) -> u32 {
        if adaptive && memory_limit_bytes == u64::MAX {
            threads.max(Self::machine_threads())
        } else {
            threads
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
    /// The ceiling currently applied to `decoder` — what the caller asked for,
    /// narrowed by what the stream's shape is worth — so that a caller that
    /// does not change it costs one relaxed load per block of output.
    applied_threads: u32,
    /// Packed bytes read but not yet handed to the decoder, in the pieces they
    /// were read in and in stream order. Feeding gives a whole piece away by
    /// value, so nothing is copied out of here and nothing is copied down over
    /// itself: the queue is the buffer, and its bytes leave it for good.
    segs: VecDeque<Vec<u8>>,
    /// Bytes across [`Self::segs`], which is what the read-ahead allowance is
    /// measured against.
    held: usize,
    /// Stream offset of the first byte of `segs.front()`, and so of the first
    /// byte the decoder has not been given.
    fed_to: u64,
    /// Bytes read past the last chunk header [`Self::scanner`] could finish.
    ///
    /// A header can straddle two reads. The few bytes of the tail that the
    /// scanner could not walk are cut off the piece before it is queued and
    /// put at the head of the next one, so every queued piece is walked and
    /// the scanner sees one unbroken stream.
    carry: Vec<u8>,
    /// This reader's own walk of the chunk headers, which is how it knows
    /// where a run ends without decoding anything. See [`Self::safe_limit`].
    scanner: Lzma2RunScanner,
    /// Run boundaries [`Self::scanner`] has found. Zero of them after a good
    /// look is what a stream that cannot be parallelised at all looks like.
    runs_seen: u64,
    /// The shapes of the last [`MT_RUN_WINDOW`] runs the scanner closed, and
    /// where the next one goes. Their sizes are what sizes the decode: see
    /// [`Lzma2MtReader::affordable_threads`].
    recent_runs: [RunShape; MT_RUN_WINDOW],
    recent_at: usize,
    /// Whether the runs going past are stored chunks — data that did not
    /// compress — which is what decides how wide this decode is allowed to
    /// be. See [`RunShape::is_stored`] and [`MT_DENSE_THREADS`].
    dense: bool,
    /// Closed runs in a row that disagreed with [`Self::dense`]. Reset by any
    /// run that agrees, so it counts a run of disagreement and not a total.
    shape_streak: u32,
    /// [`MT_DENSE_THREADS`], as a field so that a test can lift the shape
    /// ceiling and look at what the memory limit affords on its own: every
    /// stream a test writes by hand is an incompressible one.
    dense_threads: u32,
    /// The widest the caller may take this decode, from the plan: the thread
    /// count for a caller that will not widen it, and the machine for one
    /// that may. See [`Self::sync_threads`].
    widen_to: u32,
    /// The threads the read-ahead is sized for, which is the applied count
    /// unless the decode is offering its caller room to widen. See
    /// [`Self::sync_threads`].
    read_ahead: u32,
    /// The widest read-ahead offered so far. A caller that has not widened
    /// to it is not following the offer, so no wider one is made.
    offered: u32,
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
    /// [`lzma2_encoder_run_cap`] for this stream's dictionary: decoded bytes
    /// of a first run past which the stream is taken to be a single run.
    run_cap: u64,
    /// [`MT_INPUT_HOLD_BYTES`], a field for the same reason.
    hold_bytes: usize,
    /// [`MT_BACKLOG_BYTES_PER_THREAD`], a field for the same reason: the three
    /// run-size regimes it sorts streams into are reachable in a test by moving
    /// the ceiling rather than by building runs of a hundred megabytes.
    backlog_bytes: u64,
    /// Set once the source has no more bytes to give. Not the same thing as
    /// the decoder having been told: see [`Self::told_end`].
    input_done: bool,
    /// Set once the decoder has been told the input is over, which is only
    /// after the last piece read has been taken. See [`Self::settle_end`].
    told_end: bool,
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
    /// Packed bytes the stream is still expected to hold, from its declared
    /// size, so that a read is not sized, and zero-filled, for 4 MiB when a
    /// fraction of that is left. `u64::MAX` when nothing was declared. See
    /// [`lzma2_packed_bound`].
    input_left: u64,
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
    /// Bytes copied to cut a queued piece at a run boundary, and the number of
    /// cuts. A piece is cut at most once and holds a chunk, so this is the
    /// whole cost of the input path beyond the read itself.
    moved: u64,
    resizes: u64,
    /// Reads that refilled a buffer the decoder handed back instead of
    /// allocating one. In the steady state this is every read but the first
    /// few, and a shortfall means pieces are being held, not leaked.
    reclaimed: u64,
    /// Pieces the decoder handed back for want of room, each of which ended
    /// that feed and was offered again after the next drain. See
    /// [`Lzma2MtReader::hand_over`].
    refusals: u64,
    /// Feeds that copied because a whole piece would not fit at all. Zero for
    /// every decode whose allowance holds one read; see
    /// [`Lzma2MtReader::offer_part_of_the_front`].
    copied_feeds: u64,
    /// Refused pieces whose first bytes were copied over anyway, so that the
    /// decoder would see the header that closes the run it is holding and
    /// hand that run to a worker. See [`Lzma2MtReader::declare_run`].
    declared: u64,
    /// Turns on which the source had run dry with bytes still queued here, so
    /// the end of the input was held back rather than announced over the top
    /// of a tail the decoder had not taken. See [`Lzma2MtReader::settle_end`].
    end_deferred: u64,
    /// The most the decoder held, the most this reader had queued for it, and
    /// the most the two came to at one moment, sampled after every drain and
    /// every feed. The first is what the memory limit governs; the second is
    /// outside it.
    peak_held: u64,
    peak_queue: u64,
    peak_sum: u64,
    /// Runs the decoder claimed between two waits of this thread, and the
    /// runs out with a worker as it went to wait, one pair per wait.
    waves: Vec<(u64, u64)>,
    claimed_at_wait: u64,
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
        decoder.set_threads(Self::decoder_threads(threads));
        // The property was accepted by the decoder just now, so this cannot
        // fail; a cap of "never" would only cost the early give-up.
        let run_cap = lzma2_dictionary_size(&[dict_prop]).map_or(u64::MAX, lzma2_encoder_run_cap);
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
            segs: VecDeque::new(),
            held: 0,
            fed_to: 0,
            carry: Vec::new(),
            scanner: Lzma2RunScanner::new(),
            runs_seen: 0,
            recent_runs: [RunShape::default(); MT_RUN_WINDOW],
            recent_at: 0,
            dense: false,
            shape_streak: 0,
            dense_threads: MT_DENSE_THREADS,
            widen_to: threads,
            read_ahead: threads,
            offered: 0,
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
            run_cap,
            hold_bytes: MT_INPUT_HOLD_BYTES,
            backlog_bytes: MT_BACKLOG_BYTES_PER_THREAD,
            input_done: false,
            told_end: false,
            out: VecDeque::new(),
            spare: Vec::new(),
            out_pos: 0,
            finished: false,
            checksums,
            fed_total: 0,
            input_left: u64::MAX,
            trace: std::env::var_os("SEVENZ_TURBO_MT_TRACE").map(|_| Box::default()),
        })
    }

    /// Records what the decoder and this reader hold, for the trace.
    fn sample_ledger(&mut self) {
        if let Some(t) = self.trace.as_mut() {
            let held = self.decoder.held_bytes();
            let queue = self.held as u64;
            t.peak_held = t.peak_held.max(held);
            t.peak_queue = t.peak_queue.max(queue);
            t.peak_sum = t.peak_sum.max(held + queue);
        }
    }

    /// Records a wave for the trace, as this thread goes to wait.
    fn note_wave(&mut self) {
        let claimed = self.decoder.runs_claimed();
        let out = self.busy_workers();
        if let Some(t) = self.trace.as_mut() {
            t.waves.push((claimed - t.claimed_at_wait, out));
            t.claimed_at_wait = claimed;
        }
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
            .store(self.backlog_runs(), Ordering::Relaxed);
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

    /// Applies a thread count the caller changed since the last look, and the
    /// ceiling the shape of the stream puts on it. The decoder itself defers
    /// the change to the next run boundary.
    ///
    /// The narrowed count is what the read-ahead is then sized from as well,
    /// because a thread the decode will not run is a thread there is no point
    /// reading a run ahead for: see [`Self::affordable_threads`].
    ///
    /// # Room to widen
    ///
    /// A caller widening a decode as its backlog grows — weaver's governor
    /// asks for a thread per complete run in hand — can only widen into runs
    /// this reader has read. Sized for the applied threads alone, the
    /// read-ahead holds one run more than those threads are decoding, so
    /// such a caller widens one thread per look, and on a stream of large
    /// runs most of the work has been claimed narrow before it gets there.
    ///
    /// So a decode that may be widened reads ahead for twice its applied
    /// threads, or for as many runs as [`MT_OFFER_BYTES`] holds where that is
    /// more, up to [`Self::widen_to`], as long as the caller has taken up
    /// what was offered last time. A caller held at its own ceiling stops
    /// following the offer, and from then on the read-ahead is what a decode
    /// asked for at that width would hold: the overshoot is the one doubling
    /// the caller did not take. An incompressible stream is never offered
    /// more, because its shape has already decided its width.
    fn sync_threads(&mut self) {
        let want = self.control.threads().min(self.shape_threads());
        if want != self.applied_threads {
            self.decoder.set_threads(Self::decoder_threads(want));
            self.applied_threads = want;
        }
        let (packed, unpacked) = self.run_size();
        // No offer before the first run has closed: what it can be sized by
        // is not known yet, and an offer made blind would stand.
        let sized = packed.saturating_add(unpacked) > 0;
        self.read_ahead = if sized && !self.dense && self.widen_to > want && want >= self.offered {
            let fit = MT_OFFER_BYTES
                .checked_div(packed.saturating_add(unpacked))
                .map_or(0, |fit| u32::try_from(fit).unwrap_or(u32::MAX));
            // Never more than the budget has room for: a run read ahead here
            // is held whether or not the decoder could take it.
            let room = self
                .budget()
                .checked_div(packed.saturating_add(unpacked))
                .map_or(u32::MAX, |room| {
                    u32::try_from(room.max(1)).unwrap_or(u32::MAX)
                });
            let wider = want
                .saturating_mul(2)
                .max(fit)
                .min(self.widen_to)
                .min(room)
                .max(want);
            self.offered = wider;
            wider
        } else {
            want
        };
    }

    /// The thread count the decoder is given for a ceiling of `threads`.
    ///
    /// The decoder takes one thread to mean the calling thread, and decodes
    /// every run inline. A run being decoded inline holds the decoder's
    /// cursor until it ends, and no run behind it can be claimed until then
    /// whatever the ceiling has become: on a stream of 128 MiB runs, a decode
    /// started at one thread and widened a tenth of a second later decoded
    /// its whole first run alone while every other one waited. So one thread
    /// is given to the decoder as two, and this reader keeps the second one
    /// idle by handing over a run only once the last one is back; see
    /// [`Self::one_worker_busy`]. The decode still runs on one thread at a
    /// time, and a widening applies at the very next run.
    fn decoder_threads(threads: u32) -> usize {
        if cfg!(target_arch = "wasm32") {
            // No worker can be started there, so one is the calling thread.
            return threads.max(1) as usize;
        }
        threads.max(2) as usize
    }

    /// Whether to listen for the caller while waiting for a worker, instead
    /// of waiting on the worker alone.
    ///
    /// Only this thread can hand the decoder a run, and a wait on a worker
    /// cannot be broken into, so a caller widening the decode while this
    /// thread waits is heard when a run lands. That is the whole decode on a
    /// stream of 128 MiB runs started at one thread: the first run went out,
    /// the caller widened a tenth of a second later, and every other run was
    /// claimed 2.3 seconds after that. So a decode the caller may widen,
    /// holding runs it has not got the threads for, waits on the caller and
    /// looks at its workers between slices. A caller that is not going to
    /// widen costs a wake-up every [`MT_LISTEN_SLICE`] of a wait that lasts
    /// seconds; small runs, where the wait is short anyway, never listen.
    fn listening(&self) -> bool {
        !cfg!(target_arch = "wasm32")
            && !self.dense
            && !self.scan_broken
            && self.widen_to > self.applied_threads
            && self.busy_workers() > 0
            && self.run_size().1 >= MT_LISTEN_RUN_BYTES
            && self.backlog_runs() as u64 >= u64::from(self.applied_threads)
    }

    /// Whether this decode is at one thread with its one run out, which is
    /// when nothing more may be handed over. See [`Self::decoder_threads`].
    ///
    /// Out is counted here, from the runs fed whole and not yet handed back,
    /// and not from the decoder's claims: the decoder claims a run only once
    /// it has seen the header after it, and only when it is drained, so a
    /// feed is over before the decoder knows what it was given. Two runs in
    /// the decoder is the one a worker has and the one that told the decoder
    /// where it ends.
    ///
    /// The decoder's own chase is not held back: a run it is chasing has to
    /// be fed to be decoded at all. Nor is a stream whose headers this reader
    /// could not walk, whose runs it cannot count back in.
    fn one_worker_busy(&mut self) -> bool {
        self.applied_threads <= 1
            && !self.chase
            && !self.scan_broken
            && self.runs_fed() >= self.runs_delivered + 2
            && self.fed_at_boundary()
    }

    /// Runs this reader has seen end and has handed over whole.
    fn runs_fed(&self) -> u64 {
        self.runs_seen - self.held_runs()
    }

    /// Complete runs this reader has read and not yet handed over.
    fn held_runs(&self) -> u64 {
        let reached = self.run_ends.partition_point(|&end| end <= self.fed_to);
        (self.run_ends.len() - reached) as u64
    }

    /// The backlog a caller widens on: complete runs in hand behind the one
    /// at the front of the output, whether a worker has one yet or not.
    ///
    /// Counting only the runs no worker has claimed is what a caller asking
    /// for a thread per waiting run cannot follow: the moment it widens, the
    /// runs it widened for are claimed, the count drops, and it narrows
    /// again before they are done. A run a worker is decoding still needs
    /// that worker. So the count is every run this reader has seen end and
    /// not yet handed over whole — read ahead and still held here, fed, out
    /// with a worker or waiting for one — less the one at the front. Never
    /// more than the stream's shape will let the decode use, so that a caller
    /// does not pay for threads that will not run.
    fn backlog_runs(&self) -> usize {
        let in_hand = self
            .runs_seen
            .saturating_sub(self.runs_delivered)
            .saturating_sub(1);
        let shaped = u64::from(self.shape_threads().saturating_sub(1));
        usize::try_from(in_hand.min(shaped)).unwrap_or(usize::MAX)
    }

    /// The most threads the stream in hand is worth decoding with.
    ///
    /// Unlimited until enough runs have gone past to say what shape the stream
    /// is, so a decode of an archive whose first run has not closed yet is as
    /// wide as it was asked to be; the runs of an incompressible stream are
    /// large enough that nothing has been handed to a worker by then in any
    /// case.
    fn shape_threads(&self) -> u32 {
        if self.dense {
            self.dense_threads
        } else {
            u32::MAX
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
        let fed_to = self.fed_to;
        while self.run_ends.front().is_some_and(|&end| end <= fed_to) {
            self.last_boundary = self.run_ends.pop_front().expect("just looked");
        }
        fed_to == self.last_boundary
    }

    /// What one run of this stream costs, as `(packed, unpacked)`, taken as
    /// the largest of the last [`MT_RUN_WINDOW`] the scanner closed. `(0, 0)`
    /// until the first one has closed.
    fn run_size(&self) -> (u64, u64) {
        self.recent_runs.iter().fold((0, 0), |(p, u), run| {
            (p.max(run.packed), u.max(run.unpacked))
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
        let threads = u64::from(self.applied_threads.max(self.read_ahead).max(1));
        let (packed, unpacked) = self.run_size();
        self.budget()
            .checked_div(packed.saturating_add(unpacked))
            .map_or(threads, |fits| fits.clamp(1, threads))
    }

    /// The in-flight bytes this decode will actually put to work, which is
    /// what the read-ahead is measured against.
    ///
    /// It is the caller's limit, except for a decode the shape of the stream
    /// has narrowed. A limit is a ceiling and not a target, and the bytes a
    /// thread that will never run would have held buy nothing at all: they
    /// are read ahead, held, and handed to the same two workers a great deal
    /// later than they were read. So a narrowed decode is given what a decode
    /// of that width would have been given had the caller asked for it — the
    /// same per-thread figure the unlimited path sizes itself by — and the
    /// bytes above it are simply never read. Measured on an archive of
    /// incompressible payload asked for at eight threads: three gigabytes
    /// resident against two threads' worth of work, where narrowing the
    /// thread count alone had left the reading-ahead untouched.
    fn budget(&self) -> u64 {
        let limit = self.decoder.memory_limit();
        if self.dense {
            limit.min(
                MT_BACKSTOP_PER_THREAD_BYTES.saturating_mul(u64::from(self.applied_threads.max(1))),
            )
        } else {
            limit
        }
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
    ///
    /// Under all of that sits the budget itself. The floor asks for runs a free
    /// worker could take, and a worker can only take one if the budget has room
    /// to decode it; where it has not, reading further ahead buys no thread and
    /// spends the very bytes a running thread needs in order to finish and
    /// hand its own back. That was measured: at eight threads under a gigabyte
    /// the packed read-ahead the floor asked for grew the decoder's input
    /// buffer to half the whole allowance, leaving room for three runs at once
    /// where the allowance should have paid for six. So the budget is asked
    /// first, and a budget with no room for another run is itself the answer.
    fn backlog_full(&self) -> bool {
        if !self.room_for_a_run() {
            return true;
        }
        let pending = self.decoder.pending_runs() as u64;
        if pending < self.demand_floor() {
            return false;
        }
        pending >= self.backlog_target() || self.backlog_packed() >= self.byte_cap()
    }

    /// Whether the decode has nothing to get on with but the run at the
    /// cursor.
    ///
    /// A complete run waiting for a worker, or one a worker already has, is
    /// work that will finish and be drained, and draining it is what frees the
    /// allowance the next boundary needs. So while either exists, a boundary
    /// can still come into reach and the chase path would only be taking a
    /// run away from the workers. It is the counts that are asked and not the
    /// bytes the decoder holds: bytes move with every drain, and a decode is
    /// briefly holding none between finishing one run and reading the next.
    fn nothing_left_to_decode(&self) -> bool {
        self.decoder.pending_runs() == 0 && self.busy_workers() == 0
    }

    /// Whether what the decoder is already holding leaves room to decode one
    /// more run of this stream.
    ///
    /// True while the run size is still unknown, and true always for a caller
    /// that set no limit: an unlimited decode is never held back here and
    /// reads ahead exactly as it did before.
    fn room_for_a_run(&self) -> bool {
        let (packed, unpacked) = self.run_size();
        let cost = packed.saturating_add(unpacked);
        cost == 0 || self.decoder.held_bytes().saturating_add(cost) <= self.budget()
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
            .min(self.budget())
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
            return self.read_to();
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
        // A piece is given away for good, so without asking for one back this
        // loop allocates a buffer per read and frees one per decoded run, from
        // two different threads, which the allocator answers by holding on to
        // the difference. `reclaim_piece` hands back an allocation the decode
        // has finished with, emptied and with its capacity intact, and reusing
        // it keeps the reader's footprint to the pieces actually in flight.
        //
        // The piece starts with whatever the scanner could not finish last
        // time, so that it walks one unbroken stream, and the rest of it is
        // read from the input in place. Nothing else ever writes here: once
        // the piece is queued its bytes are only ever given away.
        let mut seg = match self.decoder.reclaim_piece() {
            Some(mut spare) => {
                if let Some(t) = self.trace.as_mut() {
                    t.reclaimed += 1;
                }
                spare.clear();
                spare.extend_from_slice(&self.carry);
                self.carry.clear();
                spare
            }
            None => std::mem::take(&mut self.carry),
        };
        let carried = seg.len();
        // A read is the full piece until the stream is nearly over, and then
        // what is expected to be left of it. The space is zero-filled before
        // it is read into, because a `Read` may look at the buffer it is
        // handed and so must be handed initialised bytes; sizing the read is
        // what keeps that from being 4 MiB for the last few kilobytes of a
        // block, and the reservation from being 4 MiB for a block that is
        // only a little over the smallest one decoded in parallel.
        let want = usize::try_from(self.input_left)
            .unwrap_or(usize::MAX)
            .clamp(MT_INPUT_MIN_READ_BYTES, MT_INPUT_READ_BYTES);
        // A no-op when the reclaimed buffer is already big enough, which is the
        // steady state; exact so that a piece never grows past one read.
        seg.reserve_exact(want);
        seg.resize(carried + want, 0);
        let mut filled = carried;
        while filled < seg.len() {
            match self.input.read(&mut seg[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        seg.truncate(filled);
        self.input_left = self.input_left.saturating_sub((filled - carried) as u64);
        if filled == carried {
            // Nothing new arrived. The carried bytes are the tail of a header
            // the stream ended in the middle of; the decoder is the one that
            // gets to call that an error, so they go over with everything
            // else.
            self.queue(seg);
            self.input_done = true;
            self.settle_end();
            return Ok(());
        }
        if !self.scan_broken && !self.scanner.finished() {
            let was_dense = self.dense;
            match self.scanner.feed(&seg) {
                // A stream this reader cannot walk is still a stream the
                // decoder may be able to decode, and it is the decoder's job
                // to say so. Stop steering and feed it everything.
                Err(_) => self.scan_broken = true,
                Ok(n) => {
                    while let Some(run) = self.scanner.next_run() {
                        self.runs_seen += 1;
                        self.run_ends.push_back(run.in_offset + run.packed_len);
                        self.unpacked_seen += run.unpacked_len;
                        self.unpacked_ends.push_back(self.unpacked_seen);
                        let shape = RunShape::of(&run);
                        self.recent_runs[self.recent_at] = shape;
                        self.recent_at = (self.recent_at + 1) % MT_RUN_WINDOW;
                        // What shape this run is, and whether enough of them
                        // in a row have been that shape to change the decode.
                        let dense = shape.is_stored();
                        if dense == self.dense {
                            self.shape_streak = 0;
                        } else {
                            self.shape_streak += 1;
                            if self.shape_streak >= MT_SHAPE_RUNS {
                                self.dense = dense;
                                self.shape_streak = 0;
                            }
                        }
                    }
                    if n < seg.len() {
                        self.carry.extend_from_slice(&seg[n..]);
                        seg.truncate(n);
                    }
                }
            }
            if self.dense != was_dense {
                // What these bytes said about the shape of the stream has to
                // reach the decoder before they are handed to it, because a
                // feed is also a dispatch: a narrowing left until the next
                // turn would have given the runs just scanned to as many
                // workers as the caller asked for, and the point of narrowing
                // is that those workers never start.
                self.sync_threads();
            }
        }
        self.queue(seg);
        Ok(())
    }

    /// Tells the decoder the input is over, once it actually is.
    ///
    /// A source with nothing left to give is not the end of the input as far
    /// as the decoder is concerned. Pieces already read can still be queued
    /// here, and a decoder whose budget is spent hands a piece back rather
    /// than taking it, so the last bytes of an archive can be sitting in this
    /// reader waiting for a drain to make room for them.
    ///
    /// Told the input is over, the decoder takes itself to have everything,
    /// and a drain that then finds a stream it cannot complete and nothing to
    /// get on with calls the archive corrupt. A tail this reader is still
    /// holding would be reported as a truncated stream that is nothing of the
    /// kind. So the announcement waits for the queue to empty: the source
    /// being over and every byte of it taken are two conditions, and both are
    /// asked here.
    ///
    /// A stream that really is short still says so. Its last bytes are taken
    /// like any others — they are what is left of a header, far smaller than
    /// any budget — and the decoder is told immediately afterwards, which is
    /// what turns the next empty drain into the missing-end-marker error.
    ///
    /// A decode listening for its caller holds the announcement back as well:
    /// a decoder told the input is over waits on its workers inside every
    /// drain, which is the wait listening exists to avoid. The stream's end
    /// marker has been fed, so nothing about the decode is left undecided by
    /// the wait; the decoder is told as soon as the decode stops listening.
    /// See [`Self::listening`].
    fn settle_end(&mut self) {
        if self.input_done && !self.told_end {
            if self.listening() {
                return;
            }
            if self.fed_to == self.read_to() {
                self.told_end = true;
                self.decoder.end_of_input();
            } else if let Some(t) = self.trace.as_mut() {
                // The source is over and the decoder has not been told,
                // because bytes it has not taken are still queued here.
                t.end_deferred += 1;
            }
        }
    }

    /// Adds a read piece to the back of the queue.
    fn queue(&mut self, seg: Vec<u8>) {
        if seg.is_empty() {
            return;
        }
        self.held += seg.len();
        self.segs.push_back(seg);
    }

    /// Stream offset one past the last byte read, counting what is queued but
    /// not the header tail waiting for its next read.
    fn read_to(&self) -> u64 {
        self.fed_to + self.held as u64
    }

    /// Hands the first `want` queued bytes to the decoder, whole pieces at a
    /// time, and returns how many it took.
    ///
    /// A piece is given away by value: the bytes are the decoder's from here,
    /// and the only copy on this path is the one that cuts a piece when the
    /// point to stop at — a run boundary, or the end of the chase's ration —
    /// falls inside it. That cut moves at most a chunk, and a stream of long
    /// runs pays it about once per run.
    ///
    /// A decoder whose budget is spent hands the piece back unchanged, and the
    /// piece goes to the front of the queue, where it is the first thing
    /// offered next time. Stopping short is what tells the feed loop the
    /// budget is spent, exactly as a short take used to, and no read-ahead is
    /// lost by stopping: the bytes are still read, still in order, and still
    /// the next thing to go over.
    ///
    /// What this must not do is wait here for the room. A refusal means the
    /// decoder is full, which on a stream of long runs means a worker is
    /// holding a run this thread has not drained yet — so the thing to do is
    /// return, drain it, and offer the piece again with the room that drain
    /// freed. Waiting instead puts the one thread that can deliver output to
    /// sleep until a worker lands, and it was measured costing about a run's
    /// decode per refusal: a three-gigabyte decode at eight threads went from
    /// seventeen seconds to forty, using *less* processor time, because the
    /// workers spent most of it with nothing to hand back to.
    fn hand_over(&mut self, want: usize, last_resort: bool) -> std::io::Result<usize> {
        let mut given = 0;
        while given < want {
            let front_len = self.segs.front().map_or(0, Vec::len);
            let take = (want - given).min(front_len);
            if take == 0 {
                break;
            }
            let seg = if take == front_len {
                self.segs.pop_front().expect("not empty, it has a length")
            } else {
                let front = self.segs.front_mut().expect("not empty, it has a length");
                let rest = front.split_off(take);
                let head = std::mem::replace(front, rest);
                if let Some(t) = self.trace.as_mut() {
                    t.moved += (front_len - take) as u64;
                    t.resizes += 1;
                }
                head
            };
            self.held -= take;
            match self.decoder.feed_owned(seg).map_err(decode_error)? {
                None => {
                    self.fed_to += take as u64;
                    given += take;
                }
                Some(rest) => {
                    self.held += rest.len();
                    self.segs.push_front(rest);
                    if let Some(t) = self.trace.as_mut() {
                        t.refusals += 1;
                    }
                    if self.run_undeclared() {
                        given += self.declare_run()?;
                    }
                    if given == 0 && last_resort {
                        given += self.starve(take)?;
                    }
                    break;
                }
            }
        }
        // Whatever went over may have been the last of it.
        self.settle_end();
        Ok(given)
    }

    /// Gets a decode moving again that has nothing left to move it.
    ///
    /// Reached only with the piece refused, nothing fed and no worker to wait
    /// for: the decoder will take no more input, and nothing it is holding is
    /// going to hand any back. Two things can be wrong, and both are answered
    /// here because neither costs anything anywhere else.
    ///
    /// The allowance can be smaller than a read, so that every piece is
    /// refused for being a piece while there is room for part of one. Offered
    /// by reference the same bytes are taken in part, which is the one place
    /// this reader copies its input and the price of a limit that small.
    ///
    /// The decoder now takes a whole piece a little larger than what its
    /// budget leaves, rather than refusing it into a stall, and it has a test
    /// of its own for that. A little larger is not the case kept here: a read
    /// is several megabytes and a caller may set a limit smaller than one, and
    /// then no amount of taking pieces whole gets the first byte in. So this
    /// stays, and the counter that says it never runs on a real archive —
    /// `copied_feeds`, zero on every lane measured — is what says it costs
    /// nothing to keep.
    ///
    /// Or there can be no room at all, because what the decoder holds is a run
    /// it cannot claim — a run is complete only once the header after it has
    /// arrived, and a run that nearly fills the allowance leaves nowhere to
    /// put that header. Feeding cannot resolve that and neither can waiting;
    /// what can is the chase, which decodes the chunks already in hand on this
    /// thread and frees the room the header needs. It is the same condition
    /// the chase is armed on elsewhere — nothing pending, nothing outstanding,
    /// no way to read further — arrived at from the other side; see
    /// [`Self::nothing_left_to_decode`].
    fn starve(&mut self, want: usize) -> std::io::Result<usize> {
        let took = self.offer_part_of_the_front(want)?;
        if took == 0 && self.nothing_left_to_decode() {
            self.chasing = true;
            self.set_chase(true);
        }
        Ok(took)
    }

    /// Feeds as much of the front piece as the decoder will take, copying it.
    fn offer_part_of_the_front(&mut self, want: usize) -> std::io::Result<usize> {
        let (took, tail) = self.feed_front_by_copy(want)?;
        if took > 0
            && let Some(t) = self.trace.as_mut()
        {
            t.moved += tail as u64;
            t.resizes += 1;
            t.copied_feeds += 1;
        }
        Ok(took)
    }

    /// Whether the decoder is holding a run it has not yet recognised as one.
    ///
    /// The decoder closes a run on the control byte after it, and what was
    /// handed over stops at the run's end, so the byte that would close it is
    /// at the front of the piece still queued here. The run is in hand whole
    /// and nobody can be given it: not a worker, because the decoder has no
    /// run to give, and not the chase, because this reader has it turned off.
    /// That is the reader's own scanner counting one more run handed over
    /// than the decoder has claimed, with nothing waiting to be claimed.
    fn run_undeclared(&mut self) -> bool {
        !self.chasing
            && !self.scan_broken
            && self.decoder.pending_runs() == 0
            && self.runs_fed() > self.decoder.runs_claimed()
            && self.fed_at_boundary()
    }

    /// Gets a run the decoder is holding undeclared in front of a worker.
    ///
    /// A piece refused at a run boundary leaves the decode in a state no drain
    /// gets it out of: the input budget is spent on the run in hand and on the
    /// runs out with workers, so the piece whose first byte would close the
    /// run in hand is refused for its size, while the run it would close sits
    /// undispatched with a worker idle. A drain changes nothing — there is no
    /// declared run to dispatch — and the room a landing worker frees is spent
    /// by this reader on the next whole piece, so the decode settles on one
    /// run out fewer than the limit pays for. Measured on a stream of 128 MiB
    /// runs at four threads: three runs out of every four, and a decode twice
    /// as long as 7-Zip's.
    ///
    /// The header is a page away, so a page of the refused piece is copied
    /// over, by reference, the one way the decoder takes part of a piece. That
    /// copy is counted where the limit can see it, it is the smallest the
    /// decoder's floor always has room for, and the piece it was cut from is
    /// still the next thing offered whole.
    fn declare_run(&mut self) -> std::io::Result<usize> {
        let (took, tail) = self.feed_front_by_copy(MT_DECLARE_BYTES)?;
        if took > 0
            && let Some(t) = self.trace.as_mut()
        {
            t.moved += tail as u64;
            t.declared += 1;
        }
        Ok(took)
    }

    /// Feeds up to `want` bytes of the front piece by reference and cuts them
    /// off it. Returns how many went over and how many bytes of the piece were
    /// moved to cut them off.
    fn feed_front_by_copy(&mut self, want: usize) -> std::io::Result<(usize, usize)> {
        let front = match self.segs.front_mut() {
            Some(front) => front,
            None => return Ok((0, 0)),
        };
        let offer = want.min(front.len());
        let took = self
            .decoder
            .feed(&front[..offer])
            .map_err(decode_error)?
            .min(offer);
        if took == 0 {
            return Ok((0, 0));
        }
        let tail = front.len() - took;
        let kept = front.split_off(took);
        drop(std::mem::replace(front, kept));
        self.held -= took;
        self.fed_to += took as u64;
        if self.segs.front().is_some_and(Vec::is_empty) {
            self.segs.pop_front();
        }
        Ok((took, tail))
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
            // At one thread a run goes over only once the last one is back,
            // but the read-ahead is still read, so that the caller can see
            // the runs there are to widen for. The last feed is the exception,
            // taken only with no worker to wait for: it may hand over one run
            // past the gate, which is what gets a decode with nothing out
            // moving, and no more than one.
            let gated = if whatever_is_held {
                fed && self.applied_threads <= 1 && self.fed_at_boundary()
            } else {
                self.one_worker_busy()
            };
            if gated {
                if self.held_runs() + 1 < u64::from(self.read_ahead)
                    && !self.input_done
                    && !self.scan_broken
                    && self.held < self.hold_bytes
                {
                    self.refill()?;
                    continue;
                }
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
            //
            // A good look is the longest run an encoder writes for this
            // dictionary, in decoded bytes, which the chunk headers say
            // exactly; the packed give-up is the backstop for a dictionary so
            // large that the cap says nothing. See [`lzma2_encoder_run_cap`].
            let one_run = self.runs_seen == 0
                && (self.scanner.out_position() > self.run_cap
                    || self.scanner.in_position() > self.give_up_bytes)
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
            //
            // A first run chased because it outgrew the hold allowance before
            // the give-up fired is the same stream seen earlier, and wants the
            // same: fed as it is decoded, not read into the decoder until the
            // budget refuses it.
            let first_run_chased = self.chasing && self.runs_seen == 0;
            let far_enough = if one_run || self.scan_broken || first_run_chased {
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
            let mut end = self.read_to().min(self.safe_limit());
            if end <= self.fed_to {
                if !self.input_done && self.held < hold {
                    self.refill()?;
                    continue;
                }
                // A boundary out of reach with nothing else to decode: the run
                // at the cursor is longer than the hold allowance, so waiting
                // for its end would wait forever and the chase path is the
                // only way it gets decoded at all.
                //
                // What must not be asked here is whether the decoder happens
                // to be empty. Empty is a property of an instant, not of the
                // stream: a decode of runs near the size of the allowance is
                // empty for a moment after one run is handed over and before
                // the next boundary is read, with whole runs still to come,
                // and a chase entered there takes a run the workers should
                // have had and keeps it to the end. Measured before the rule
                // below replaced it, a stream of 128 MiB runs decoded 1280 MiB
                // of 1536 on this thread, with two runs of twelve ever
                // reaching a worker.
                //
                // The question is instead whether a boundary can still come
                // into reach. Reaching this point has already answered half of
                // it: the allowance is spent, or the input is over, or the
                // give-up fired, or the scan broke. The other half is that
                // nothing else can decode — no complete run waiting for a
                // worker and none out with one — because a run in either of
                // those places will be drained, which frees the allowance,
                // which reads the boundary. See [`Self::nothing_left_to_decode`].
                if (self.chasing || self.nothing_left_to_decode()) && self.held > 0 {
                    self.chasing = true;
                    self.set_chase(true);
                    // Before any run has closed every queued byte is the
                    // first run's — each piece is walked before it is queued
                    // — so the front piece goes over whole rather than cut at
                    // a ration: cutting a 4 MiB read into 1 MiB feeds copied
                    // the rest of it every time, which on a single-run
                    // gigabyte was 1.3 GiB moved for nothing. Past the first
                    // run the ration stands, because a piece there can run on
                    // past the end of the run being chased.
                    let ration = if self.runs_seen == 0 {
                        self.segs.front().map_or(0, Vec::len).max(MT_INPUT_CHUNK)
                    } else {
                        MT_INPUT_CHUNK
                    };
                    end = self.read_to().min(self.fed_to + ration as u64);
                    if let Some(t) = self.trace.as_mut() {
                        t.chased += end - self.fed_to;
                    }
                } else {
                    break;
                }
            } else {
                self.chasing = false;
                if !self.scan_broken {
                    self.set_chase(false);
                    if self.applied_threads <= 1 {
                        // One run at a time at one thread, or the feed that
                        // completes the first would hand a second worker the
                        // next. See [`Self::decoder_threads`].
                        let reached = self.run_ends.partition_point(|&e| e <= self.fed_to);
                        if let Some(&next) = self.run_ends.get(reached) {
                            end = end.min(next);
                        }
                    }
                }
            }
            let offered = usize::try_from(end - self.fed_to).unwrap_or(usize::MAX);
            let taken = self.hand_over(offered, whatever_is_held)?;
            self.sample_ledger();
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
            let waves: Vec<String> = t.waves.iter().map(|(c, o)| format!("{c}/{o}")).collect();
            eprintln!(
                "mt-trace: threads={} spawned={} drains={} drain={:.3}s sink={:.3}s/{} small={} MiB pump={:.3}s fed={} MiB fed_partial={} MiB st_decoded={} MiB out={} MiB runs={} moved={} MiB resizes={} refused={} copied_feeds={} declared={} reclaimed={} end_deferred={} dense={} peak_held={} peak_queue={} peak_sum={} waves={}",
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
                t.refusals,
                t.copied_feeds,
                t.declared,
                t.reclaimed,
                t.end_deferred,
                self.dense,
                t.peak_held,
                t.peak_queue,
                t.peak_sum,
                waves.join(","),
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
            self.settle_end();

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
            // been given everything. A source that has run dry is not enough:
            // a piece it read can still be queued here, refused for want of
            // room, and a decode holding output it has not yet handed back is
            // not a short stream. See [`Lzma2MtReader::settle_end`].
            let told_the_end = self.told_end;
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
            self.sample_ledger();

            // Whether input went over after the drain, which the decoder has
            // not looked at yet: the next thing is to drain again, not to wait
            // on a worker the new input may be about to give a run to.
            let mut fed_since_drain = false;
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
                        fed_since_drain = true;
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
            if !self.finished && self.out.is_empty() && !fed_since_drain {
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
                self.sample_ledger();
                self.note_wave();
                if self.listening() {
                    // Whichever comes first: the caller widening, or the
                    // slice ending, after which the drain above collects any
                    // run that landed.
                    self.control
                        .wait_for_change(self.applied_threads, MT_LISTEN_SLICE);
                } else if !self.decoder.wait_for_worker() {
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
            unpacked_len,
            widen_to,
        } => {
            let mut rd =
                Lzma2MtReader::new(input, dict_prop, threads, memory_limit, control, &splits)?;
            rd.input_left = lzma2_packed_bound(unpacked_len);
            rd.widen_to = widen_to;
            Ok(Lzma2Coder::Adaptive(Box::new(rd)))
        }
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
            u64::MAX,
            &control,
            &[],
        );
        assert!(matches!(plan, Lzma2Plan::Adaptive { .. }));

        // Room for the dictionary and almost nothing else: single-threaded,
        // not a memory-limit error.
        let plan = Lzma2Plan::for_block(
            8,
            false,
            (32 << 20) + (1 << 20),
            dict,
            u64::MAX,
            &control,
            &[],
        );
        assert!(matches!(plan, Lzma2Plan::SingleThreaded));
    }

    /// One thread is the default and costs nothing: no control block, no
    /// adaptive decoder, exactly the coder the fork shipped before.
    #[test]
    fn one_thread_is_the_plain_reader_unless_the_caller_asks_to_widen_later() {
        let control = Arc::new(Lzma2Control::new(1));
        assert!(matches!(
            Lzma2Plan::for_block(1, false, u64::MAX, 1 << 20, u64::MAX, &control, &[]),
            Lzma2Plan::SingleThreaded
        ));
        assert!(matches!(
            Lzma2Plan::for_block(1, true, u64::MAX, 1 << 20, u64::MAX, &control, &[]),
            Lzma2Plan::Adaptive { threads: 1, .. }
        ));
    }

    /// A block too small to hold a second run is decoded single-threaded,
    /// whatever was asked for; one that can is not.
    #[test]
    fn a_block_smaller_than_a_run_is_single_threaded() {
        let control = Arc::new(Lzma2Control::new(8));
        for (threads, adaptive) in [(8, false), (8, true), (1, true)] {
            // Exactly one run is still one run: the encoder's smallest run is
            // this size, so a block of it has nothing to hand a second worker.
            for len in [0, 16 << 10, MT_MIN_BLOCK_BYTES - 1, MT_MIN_BLOCK_BYTES] {
                assert!(
                    matches!(
                        Lzma2Plan::for_block(
                            threads,
                            adaptive,
                            u64::MAX,
                            1 << 20,
                            len,
                            &control,
                            &[]
                        ),
                        Lzma2Plan::SingleThreaded
                    ),
                    "{threads} threads, adaptive {adaptive}, {len} bytes"
                );
            }
            assert!(matches!(
                Lzma2Plan::for_block(
                    threads,
                    adaptive,
                    u64::MAX,
                    1 << 20,
                    MT_MIN_BLOCK_BYTES + 1,
                    &control,
                    &[]
                ),
                Lzma2Plan::Adaptive { .. }
            ));
        }
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

    /// An adaptive coder starts at one thread and is widened later, so with
    /// no caller limit its budget holds runs for every thread it may widen
    /// to, not just the one it starts at. A fixed coder, or any coder under a
    /// caller limit, is budgeted exactly as before.
    #[test]
    fn an_unset_budget_lets_an_adaptive_coder_widen_to_the_machine() {
        let parallelism = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(256) as u32;
        let control = Arc::new(Lzma2Control::new(1));
        let Lzma2Plan::Adaptive { memory_limit, .. } =
            Lzma2Plan::for_block(1, true, u64::MAX, 1 << 20, u64::MAX, &control, &[])
        else {
            panic!("an adaptive coder over a large block is the parallel one");
        };
        assert_eq!(
            memory_limit,
            u64::from(parallelism) * MT_BACKSTOP_PER_THREAD_BYTES
        );
        assert_eq!(Lzma2Plan::budgeted_threads(1, true, u64::MAX), parallelism);
        assert_eq!(Lzma2Plan::budgeted_threads(300, true, u64::MAX), 300);
        assert_eq!(Lzma2Plan::budgeted_threads(1, false, u64::MAX), 1);
        assert_eq!(Lzma2Plan::budgeted_threads(8, false, u64::MAX), 8);
        assert_eq!(Lzma2Plan::budgeted_threads(1, true, 4 << 30), 1);
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

    /// A source that records the largest buffer it was asked to fill.
    struct Widest<'a> {
        inner: &'a [u8],
        widest: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Read for Widest<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.widest.set(self.widest.get().max(buf.len()));
            self.inner.read(buf)
        }
    }

    /// A block a little over the smallest one decoded in parallel is read in
    /// pieces sized to what it declares, not in the 4 MiB pieces of a long
    /// stream, and decodes to the same bytes.
    #[test]
    fn a_short_block_is_not_read_four_mebibytes_at_a_time() {
        let (packed, plain) = stream(4, 6);
        let widest = std::rc::Rc::new(std::cell::Cell::new(0));
        let control = Arc::new(Lzma2Control::new(4));
        let plan = super::Lzma2Plan::for_block(
            4,
            false,
            u64::MAX,
            1 << 19,
            plain.len() as u64,
            &control,
            &[],
        );
        assert!(matches!(plan, super::Lzma2Plan::Adaptive { .. }));
        let input = Widest {
            inner: &packed,
            widest: std::rc::Rc::clone(&widest),
        };
        let mut rd = super::lzma2_decoder(input, DICT_PROP, plan).expect("build the reader");
        let mut out = Vec::new();
        rd.read_to_end(&mut out).expect("decode");
        assert!(out == plain, "the decode differs");
        assert!(
            widest.get() <= super::lzma2_packed_bound(plain.len() as u64) as usize,
            "read {} bytes at once for a {} byte stream",
            widest.get(),
            packed.len()
        );
        assert!(widest.get() < super::MT_INPUT_READ_BYTES);
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

    /// A stream longer than the allowance is refused somewhere, and every
    /// piece refused is handed over later instead of being dropped or cut up.
    ///
    /// A refusal is all or nothing, so a piece is turned away for the want of
    /// its last byte and the decode has to come back to it. What proves it
    /// came back is the output: the stream decodes whole, with every run
    /// claimed, and without a single byte of it reaching the decoder by the
    /// one path that copies. The count is not asserted exactly — how many
    /// refusals a decode meets depends on the order its workers finish in —
    /// only that the budget was reached at all, which a stream this much
    /// larger than the limit cannot avoid.
    #[test]
    fn a_refused_piece_is_offered_again_and_taken() {
        let (packed, plain) = stream(48, 4);
        for threads in [2, 4, 8] {
            let (mut rd, control) = reader(Cursor::new(packed.clone()), threads);
            rd.trace = Some(Box::default());
            let (out, _peak, _widest) = drain_watching(&mut rd, &control);
            assert_eq!(out, plain, "threads={threads}");
            assert_eq!(rd.decoder.runs_claimed(), 48, "threads={threads}");
            let t = rd.trace.as_ref().expect("the trace this test turned on");
            assert!(t.refusals > 0, "the budget was never reached: {threads}");
            assert_eq!(t.copied_feeds, 0, "threads={threads}");
        }
    }

    /// Buffers come back from the decode and are read into again, so a long
    /// stream does not allocate a piece per read.
    ///
    /// The count is compared against the reads rather than pinned: the first
    /// reads have nothing to reclaim, and a piece the decode is still holding
    /// is not spare. What would be wrong is refilling nothing, which is what
    /// handing every allocation away one-way looks like.
    #[test]
    fn read_buffers_come_back_from_the_decode() {
        let (packed, plain) = stream(48, 4);
        let reads = packed.len().div_ceil(super::MT_INPUT_READ_BYTES);
        for threads in [2, 4, 8] {
            let (mut rd, control) = reader(Cursor::new(packed.clone()), threads);
            rd.trace = Some(Box::default());
            let (out, _peak, _widest) = drain_watching(&mut rd, &control);
            assert_eq!(out, plain, "threads={threads}");
            let t = rd.trace.as_ref().expect("the trace this test turned on");
            assert!(
                t.reclaimed > 0,
                "threads={threads}: nothing reclaimed in {reads} reads",
            );
        }
    }

    /// The input reaches the decoder as the pieces it was read in, and the
    /// only copy on the way is the cut that ends a piece at a run boundary.
    ///
    /// Both counters are bounded by the reads rather than by the runs or the
    /// bytes, which is the property worth keeping: a piece is cut at most
    /// once, so a stream of any length pays for its input once per read and
    /// never again. A reader that copied what it had not fed yet — down over
    /// itself to make room, or out into a feed — would pay per refill, and on
    /// a stream that is refused often that cost grows with the square of what
    /// it is holding.
    #[test]
    fn the_input_is_never_copied_more_than_once_per_read() {
        for (runs, chunks) in [(48, 4), (12, 16), (96, 2)] {
            let (packed, plain) = stream(runs, chunks);
            let reads = packed.len().div_ceil(super::MT_INPUT_READ_BYTES);
            for threads in [2, 4, 8] {
                let (mut rd, control) = reader(Cursor::new(packed.clone()), threads);
                rd.trace = Some(Box::default());
                let (out, _peak, _widest) = drain_watching(&mut rd, &control);
                assert_eq!(out, plain, "{runs}x{chunks} threads={threads}");
                let t = rd.trace.as_ref().expect("the trace this test turned on");
                assert_eq!(t.copied_feeds, 0, "{runs}x{chunks} threads={threads}");
                assert!(
                    t.resizes as usize <= reads,
                    "{runs}x{chunks} threads={threads}: {} cuts in {reads} reads",
                    t.resizes,
                );
            }
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

    /// The end of the source is not the end of the input: bytes already read
    /// can still be queued here while the source has nothing more to give.
    ///
    /// A decoder told the input is over takes itself to have everything, and
    /// a drain that then finds a stream it cannot complete and nothing to get
    /// on with calls the archive corrupt. A tail this reader is still holding
    /// — a piece the budget has no room for, or the part of a read the header
    /// walk could not use — is not that, and announcing the end over the top
    /// of it would turn a decode that had further to go into a corrupt one.
    ///
    /// The state the queue has to survive is the one where the source runs
    /// dry while bytes sit here unfed, which happens when the last run this
    /// reader scanned is still open: nothing past the start of an open run
    /// may be handed over, so those bytes wait, and the read that would have
    /// closed the run returns nothing instead. `end_deferred` counts the
    /// turns on which the end was held back for that reason, and this asserts
    /// the state is reached, that the end is announced once the tail has been
    /// taken, and — since a run that never closes is a stream that really is
    /// short — that the decode still says so rather than waiting on it.
    #[test]
    fn a_tail_still_queued_is_not_the_end_of_the_input() {
        let (packed, _plain) = stream(8, 4);
        let cut = packed.len() - (100 << 10);
        for threads in [1, 2, 4] {
            let (mut rd, _control) = reader(Cursor::new(packed[..cut].to_vec()), threads);
            rd.trace = Some(Box::default());
            let mut buf = vec![0u8; 8 << 10];
            let err = loop {
                match rd.read(&mut buf) {
                    Ok(0) => panic!("threads={threads}: a cut stream is not a clean end"),
                    Ok(_) => {}
                    Err(err) => break err,
                }
            };
            assert!(
                matches!(
                    err.kind(),
                    ErrorKind::UnexpectedEof | ErrorKind::InvalidData
                ),
                "threads={threads}: {err}"
            );
            let t = rd.trace.as_ref().expect("the trace this test turned on");
            assert!(
                t.end_deferred > 0,
                "threads={threads}: the source never ran out with a tail still queued",
            );
            assert!(
                rd.told_end,
                "threads={threads}: the end should be announced once the tail is taken",
            );
        }
    }

    /// The same rule from the other side: a decode whose allowance is smaller
    /// than what it is holding refuses pieces all the way to the end of the
    /// stream, and the end is announced once — after the last of them has
    /// been taken — so the decode finishes byte-exact instead of being called
    /// corrupt.
    #[test]
    fn a_refused_piece_at_the_end_of_the_stream_still_finishes() {
        let (packed, plain) = stream(24, 4);
        // Not one thread: that hands over a run at a time, which an allowance
        // of two never refuses.
        for threads in [2, 4] {
            let (mut rd, control) =
                reader_limited(Cursor::new(packed.clone()), threads, 2 * run_cost(4));
            rd.trace = Some(Box::default());
            let (out, _peak, _widest) = drain_watching(&mut rd, &control);
            assert_eq!(out, plain, "threads={threads}");
            assert!(rd.told_end, "threads={threads}");
            assert_eq!(rd.held, 0, "threads={threads}: bytes left over");
            let t = rd.trace.as_ref().expect("the trace this test turned on");
            assert!(
                t.refusals > 0,
                "threads={threads}: the allowance was never reached",
            );
        }
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
        // ordinary feed stops: at one thread, once the run out and the one
        // after it are in the decoder.
        let mut turns = 0;
        while rd.pump_input(1, false).expect("feed") {
            rd.decoder.drain_upto(0, |_, _| {}).expect("scan");
            turns += 1;
            assert!(turns < 1000, "the feed never stopped");
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
            // What a hand-written stream's runs hold is data that did not
            // compress, so the decode of one narrows itself on shape as well.
            // This test is about the other rule, so the shape ceiling is
            // lifted and the limit left to answer on its own.
            rd.dense_threads = u32::MAX;
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
    /// ceiling says, the feed does not stop while a worker could come free,
    /// find nothing to take, and have room to decode it.
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
            // Whenever the read-ahead calls itself finished with the budget
            // still able to pay for a run, it is holding at least what a
            // worker coming free would need.
            if rd.backlog_full() && rd.room_for_a_run() {
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

    /// An allowance smaller than one run still gets every run to a worker.
    ///
    /// This is the shape the chase rule is decided in, and it is arrived at
    /// without any dependence on how the threads are scheduled: the hold is a
    /// third of a run, so no run boundary is ever within reach of what is
    /// held, and the feed arrives at the decision every turn with the next
    /// run's header one turn behind the decoder going quiet. Asking whether
    /// the decoder was empty was wrong here — it holds the front of the run at
    /// the cursor, so it never looked empty, the chase never armed, no
    /// boundary could come into reach and the decode stopped for want of
    /// progress with two runs of twelve ever dispatched. Asking what is left
    /// to decode arms it exactly when nothing else can make progress.
    #[test]
    fn an_allowance_below_one_run_still_decodes_on_the_workers() {
        const RUNS: u64 = 12;
        let (packed, plain) = stream(RUNS as usize, 4);
        let (mut rd, _control) = reader(Cursor::new(packed), 4);
        rd.hold_bytes = (run_packed(4) / 3) as usize;
        let mut got = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            match rd
                .read(&mut buf)
                .expect("the decode should keep making progress")
            {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, plain, "the bytes out must be the bytes in");
        assert_eq!(
            rd.decoder.runs_claimed(),
            RUNS,
            "every run should have gone to a worker"
        );
        assert_eq!(
            rd.decoder.chase_decoded_bytes(),
            0,
            "the calling thread should have decoded nothing"
        );
    }

    /// Every run of a stream the allowance can hold goes to a worker, and the
    /// calling thread decodes none of it.
    ///
    /// The read is one run's worth of output at a time, so the decode is
    /// looked at between runs — the moment at which the decoder has handed
    /// one over and not yet read the next boundary, and the moment the chase
    /// used to be armed in. Whenever this reader is deciding whether to chase,
    /// the rule it decides by is checked against the counts it is made of.
    #[test]
    fn a_run_waiting_or_out_with_a_worker_keeps_the_chase_off() {
        const RUNS: u64 = 12;
        let (packed, plain) = stream(RUNS as usize, 4);
        let (mut rd, _control) = reader(Cursor::new(packed), 4);
        let mut got = Vec::new();
        let mut buf = vec![0u8; run_unpacked(4) as usize];
        loop {
            let waiting = rd.decoder.pending_runs() as u64 + rd.busy_workers();
            assert_eq!(
                rd.nothing_left_to_decode(),
                waiting == 0,
                "{waiting} runs waiting or out, and the chase rule says {}",
                rd.nothing_left_to_decode()
            );
            match rd.read(&mut buf).expect("decode") {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, plain, "the bytes out must be the bytes in");
        assert_eq!(
            rd.decoder.runs_claimed(),
            RUNS,
            "every run should have gone to a worker"
        );
        assert_eq!(
            rd.decoder.chase_decoded_bytes(),
            0,
            "the calling thread should have decoded nothing"
        );
    }

    /// A piece refused at a run boundary has its first page handed over, so
    /// that the decoder sees the header that closes the run it is holding and
    /// a worker can be given that run.
    ///
    /// The decode is driven by hand so that the state is exact: one run out
    /// with a worker, the next run fed whole up to its last byte, and the
    /// budget's floor left with less room than a piece. A worker finishing in
    /// the meantime changes nothing here, because what it hands back is taken
    /// in only by a drain and no drain runs between the steps. The runs are
    /// 128 chunks so that a run is two pieces and a run's end falls inside a
    /// piece, which is where a real stream puts it.
    #[test]
    fn a_refusal_at_a_boundary_declares_the_run_in_hand() {
        const CHUNKS: usize = 128;
        let (packed, plain) = stream(3, CHUNKS);
        let run = run_packed(CHUNKS);
        let piece = super::MT_INPUT_READ_BYTES as u64;
        // Four threads, and room for three runs in and out. The refusal below
        // comes from the decoder's input floor, which is written in run sizes
        // and not in the limit, so the limit only has to leave the second run
        // room to be dispatched whether or not the first has landed by then.
        let limit = 6 * run;
        let (mut rd, _control) = reader_limited(Cursor::new(packed), 4, limit);
        rd.trace = Some(Box::default());

        // The first run and the start of the second: three pieces read, the
        // first run handed over whole, and the decoder shown the header after
        // it so that it closes the run.
        for _ in 0..3 {
            rd.refill().expect("read");
        }
        assert_eq!(rd.run_ends.front().copied(), Some(run));
        let fed = rd.hand_over(run as usize, false).expect("feed");
        assert_eq!(fed as u64, run, "the first run should go over whole");
        let rest = (3 * piece - run) as usize;
        assert_eq!(rd.hand_over(rest, false).expect("feed"), rest);

        // The decoder scans what it was fed only as it drains, and a drain
        // allowed no output scans, dispatches one run and returns: one run
        // out with a worker, and nothing taken in from it.
        rd.decoder.drain_upto(0, |_, _| {}).expect("dispatch");
        assert_eq!(
            rd.decoder.runs_claimed(),
            1,
            "the first run went to a worker"
        );
        assert_eq!(rd.decoder.pending_runs(), 0);

        // The second run whole, up to its last byte, and the piece holding
        // the header that would close it is left queued here.
        for _ in 0..2 {
            rd.refill().expect("read");
        }
        assert_eq!(rd.run_ends.back().copied(), Some(2 * run));
        let want = (2 * run - rd.fed_to) as usize;
        assert_eq!(rd.hand_over(want, false).expect("feed"), want);
        assert!(rd.fed_at_boundary());
        assert_eq!(
            rd.decoder.pending_runs(),
            0,
            "the second run is in hand undeclared"
        );
        assert_eq!(rd.runs_fed(), 2);

        // The piece is refused for its size. Its first page goes over anyway,
        // which is what declares the run.
        let queued = rd.segs.front().map_or(0, Vec::len);
        let given = rd.hand_over(queued, false).expect("feed");
        let t = rd.trace.as_ref().expect("the trace this test turned on");
        assert_eq!(t.refusals, 1, "the piece should have been refused");
        assert_eq!(given, super::MT_DECLARE_BYTES, "a page should go over");
        assert_eq!(t.declared, 1);
        assert_eq!(t.copied_feeds, 0, "this is not the starve path");
        assert_eq!(
            rd.segs.front().map_or(0, Vec::len),
            queued - super::MT_DECLARE_BYTES,
            "the rest of the piece is still the next thing offered"
        );
        // The next drain sees the header, closes the run and hands it out.
        rd.decoder.drain_upto(0, |_, _| {}).expect("dispatch");
        assert_eq!(
            rd.decoder.runs_claimed(),
            2,
            "the second run should have gone to a worker"
        );

        // And the decode finishes from here as it would have.
        let mut out = Vec::new();
        rd.read_to_end(&mut out).expect("decode");
        assert_eq!(out, plain);
        assert_eq!(rd.decoder.runs_claimed(), 3);
    }

    /// A budget with no room left for another run stops the read-ahead even
    /// below the floor: more packed input cannot be decoded by anybody, and
    /// the bytes it would take are the ones a running worker needs in order to
    /// finish and give its own back.
    #[test]
    fn a_spent_budget_stops_the_read_ahead_below_the_floor() {
        let (packed, _plain) = stream(8, 16);
        let (mut rd, _control) = reader(Cursor::new(packed), 4);
        // Nothing has been fed, so a free worker would find nothing: without
        // the budget rule this is the state the floor holds the feed open in.
        assert_eq!(rd.decoder.pending_runs(), 0);
        assert!(rd.demand_floor() > 0);
        // A run of this stream costs more than the whole allowance. Whatever
        // is read, no worker can be given one, so reading further ahead is
        // only spending.
        rd.recent_runs[0] = super::RunShape {
            packed: LIMIT,
            unpacked: LIMIT,
            lzma_unpacked: 0,
        };
        assert!(!rd.room_for_a_run());
        assert!(rd.backlog_full(), "a spent budget is a full backlog");
        // The control: a run the allowance can pay for leaves the floor in
        // charge, and the floor keeps the feed open.
        rd.recent_runs[0] = super::RunShape {
            packed: run_packed(16),
            unpacked: run_unpacked(16),
            lzma_unpacked: 0,
        };
        assert!(rd.room_for_a_run());
        assert!(!rd.backlog_full(), "the floor should hold the feed open");
    }

    /// The chunks `stream` writes copy their payload out verbatim, which is
    /// exactly what an encoder does with data it cannot beat, so every stream
    /// above is an incompressible one: the decode of it should narrow itself
    /// however many threads it was given, and give back the same bytes.
    ///
    /// The runs here are small enough that the whole stream is scanned in one
    /// read, so the narrowing lands before anything is handed over and the
    /// count of workers ever started stays under the ceiling rather than
    /// merely coming back down to it.
    #[test]
    fn an_incompressible_stream_is_decoded_narrow() {
        let (packed, plain) = stream(48, 4);
        for threads in [4, 8] {
            let (mut rd, control) = reader(Cursor::new(packed.clone()), threads);
            let (out, _peak, widest) = drain_watching(&mut rd, &control);
            assert_eq!(out, plain, "threads={threads}");
            assert!(rd.dense, "threads={threads}: the shape was not noticed");
            assert!(
                widest <= super::MT_DENSE_THREADS,
                "threads={threads}: {widest} workers started",
            );
            assert_eq!(
                rd.applied_threads,
                super::MT_DENSE_THREADS,
                "threads={threads}",
            );
        }
    }

    /// A decode the shape rule narrowed reads ahead as narrowly as it
    /// decodes. Narrowing the thread count alone leaves the caller's whole
    /// allowance to read ahead into, and a decode fills whatever it is given:
    /// what those bytes buy there is a queue in front of two workers.
    #[test]
    fn a_narrowed_decode_reads_ahead_for_the_threads_it_will_run() {
        let (packed, _plain) = stream(8, 4);
        let generous = 8 << 30;
        let (mut rd, _control) = reader_limited(Cursor::new(packed), 8, generous);
        assert_eq!(
            rd.budget(),
            generous,
            "the caller's limit, until a run says"
        );
        rd.pump_input(1, false).expect("feed");
        assert!(rd.dense, "the fixture is incompressible");
        assert_eq!(
            rd.budget(),
            super::MT_BACKSTOP_PER_THREAD_BYTES * u64::from(super::MT_DENSE_THREADS),
            "a narrowed decode should hold what its own threads can use",
        );
    }

    /// What a governed decode decided, looked at between every read.
    #[derive(Default)]
    struct Governed {
        out: Vec<u8>,
        /// The widest thread count the governor set and the reader applied.
        widest_applied: u32,
        /// The widest read-ahead the reader offered beyond its applied threads.
        widest_offer: u32,
        /// The most the decoder was seen holding.
        peak_in_flight: u64,
        /// The largest backlog the reader published.
        most_backlog: usize,
        /// The most runs ever out with workers at once before the first
        /// widening. A narrowing recalls nothing, so the runs out after one
        /// say nothing about how one thread is decoded.
        most_out_at_one: u64,
        /// The widest read-ahead offered while at the ceiling.
        widest_offer_at_ceiling: u32,
    }

    /// Decodes to the end under weaver's governor — a thread per complete run
    /// in hand, never more than `ceiling` — applied between reads rather than
    /// on a timer, so that what is asserted is what the reader decided and
    /// never how fast anything ran.
    fn governed<R: Read>(
        rd: &mut Lzma2MtReader<R>,
        control: &Arc<Lzma2Control>,
        ceiling: u32,
    ) -> Governed {
        let mut seen = Governed::default();
        let mut buf = vec![0u8; 16 << 10];
        control.set_threads(1);
        loop {
            if let Some(p) = control.progress() {
                seen.most_backlog = seen.most_backlog.max(p.pending_runs);
                let target = u32::try_from(p.pending_runs)
                    .unwrap_or(u32::MAX)
                    .saturating_add(1)
                    .clamp(1, ceiling);
                control.set_threads(target);
            }
            let n = rd.read(&mut buf).expect("decode");
            seen.widest_applied = seen.widest_applied.max(rd.applied_threads);
            if rd.read_ahead > rd.applied_threads {
                seen.widest_offer = seen.widest_offer.max(rd.read_ahead);
            }
            seen.peak_in_flight = seen.peak_in_flight.max(rd.decoder.in_flight_bytes());
            if rd.applied_threads == ceiling {
                seen.widest_offer_at_ceiling = seen.widest_offer_at_ceiling.max(rd.read_ahead);
            }
            if seen.widest_applied <= 1 {
                seen.most_out_at_one = seen
                    .most_out_at_one
                    .max(rd.decoder.runs_claimed().saturating_sub(rd.runs_delivered));
            }
            if n == 0 {
                break;
            }
            seen.out.extend_from_slice(&buf[..n]);
        }
        seen
    }

    /// Stored chunks of random bytes stay narrow under a governor that would
    /// take every thread offered: the shape decides the width, so the reader
    /// neither offers more nor publishes a backlog asking for more.
    #[test]
    fn a_governed_decode_of_stored_random_chunks_stays_narrow() {
        let (packed, plain) = stream(48, 4);
        let (mut rd, control) = reader_limited(Cursor::new(packed), 1, 64 * run_cost(4));
        rd.widen_to = 8;
        let seen = governed(&mut rd, &control, 8);
        assert!(seen.out == plain, "the decode differs");
        assert!(rd.dense, "the fixture is incompressible");
        assert!(
            seen.widest_applied <= super::MT_DENSE_THREADS,
            "widened to {}",
            seen.widest_applied
        );
        assert!(
            seen.most_backlog < super::MT_DENSE_THREADS as usize,
            "published a backlog of {}",
            seen.most_backlog
        );
    }

    /// A source that counts what it has handed over, so a test can see how
    /// far ahead of the output the reader has read: the input it is holding,
    /// measured from outside it.
    struct Counted<R> {
        inner: R,
        read: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl<R: Read> Read for Counted<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.set(self.read.get() + n as u64);
            Ok(n)
        }
    }

    /// The most a decode of `packed` has read ahead of its output, in bytes.
    /// The streams here are stored chunks, so a byte of output is a byte of
    /// input give or take three header bytes per 64 KiB chunk.
    fn most_read_ahead(packed: &[u8], plain: &[u8], threads: u32) -> u64 {
        let read = std::rc::Rc::new(std::cell::Cell::new(0));
        let input = Counted {
            inner: Cursor::new(packed),
            read: std::rc::Rc::clone(&read),
        };
        let (mut rd, _control) = reader_limited(input, threads, u64::MAX >> 1);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 << 10];
        let mut most = 0;
        loop {
            let n = rd.read(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            most = most.max(read.get().saturating_sub(out.len() as u64));
        }
        assert!(out == plain, "the decode differs");
        most
    }

    /// The longest run an encoder writes is four dictionaries, kept to
    /// 1..=256 MiB, never under one dictionary, in whole mebibytes.
    #[test]
    fn the_run_cap_is_the_encoders_block_size() {
        const MIB: u64 = 1 << 20;
        let cap = super::lzma2_encoder_run_cap;
        assert_eq!(cap(64 << 10), MIB);
        assert_eq!(cap(512 << 10), 2 * MIB);
        assert_eq!(cap(3 << 19), 6 * MIB);
        assert_eq!(cap(16 << 20), 64 * MIB);
        assert_eq!(cap(64 << 20), 256 * MIB);
        assert_eq!(cap(128 << 20), 256 * MIB);
        assert_eq!(cap(1536 << 20), 1536 * MIB);
        assert_eq!(cap(u32::MAX), u64::from(u32::MAX).div_ceil(MIB) * MIB);
    }

    /// A stream that is one run from start to end is streamed once its first
    /// run has gone past the longest one an encoder writes for its
    /// dictionary. It is not read ahead waiting for a boundary that is not
    /// coming: the input the reader holds stays at a few reads, where it used
    /// to be the whole hold allowance, or the whole block when that was
    /// smaller — 192 MiB of a single-run archive, all of a 158 MiB one.
    ///
    /// Asserted from outside, against what the source handed over and the
    /// caller got back, so the bound covers the decoder's own input as well
    /// as this reader's queue.
    #[test]
    fn a_single_run_stream_is_streamed_rather_than_read_ahead() {
        // 32 MiB in one run; the dictionary's cap is 2 MiB.
        let (packed, plain) = stream(1, 512);
        let cap = super::lzma2_encoder_run_cap(1 << 19);
        let bound = cap + 2 * super::MT_INPUT_READ_BYTES as u64;
        assert!(
            bound * 2 < packed.len() as u64,
            "the stream must dwarf the bound"
        );
        for threads in [2, 8] {
            let most = most_read_ahead(&packed, &plain, threads);
            assert!(
                most <= bound,
                "threads={threads}: read {most} bytes ahead of the output, bound {bound}",
            );
        }
        // The control: runs no longer than the cap are still read ahead for
        // the workers, which is what the hold exists for.
        let (packed, plain) = stream(16, 32);
        assert_eq!(run_packed(32) - 32 * 3, cap);
        let ahead = most_read_ahead(&packed, &plain, 8);
        assert!(
            ahead > bound,
            "a multi-run stream was not read ahead: {ahead} <= {bound}"
        );
    }

    /// A run made of LZMA chunk headers over filler, which the scan walks
    /// and nothing here decodes: what is being tested is the classification,
    /// and the scan never looks at a payload.
    fn lzma_chunk(out: &mut Vec<u8>, control: u8, unpacked: u32, packed: u32) {
        let u = unpacked - 1;
        out.push(control | ((u >> 16) as u8 & 0x1F));
        out.extend_from_slice(&(u as u16).to_be_bytes());
        out.extend_from_slice(&((packed - 1) as u16).to_be_bytes());
        if control >= 0xC0 {
            out.push(0x5D);
        }
        out.resize(out.len() + packed as usize, 0xAA);
    }

    fn copy_chunks(out: &mut Vec<u8>, first: u8, len: usize) {
        let mut control = first;
        let mut left = len;
        while left > 0 {
            let n = left.min(CHUNK);
            out.push(control);
            out.extend_from_slice(&((n - 1) as u16).to_be_bytes());
            out.resize(out.len() + n, 0x55);
            control = 0x02;
            left -= n;
        }
    }

    /// Scans `packed` as far as the reader will on its own, without handing
    /// a byte to the decoder, and returns the reader to look at.
    fn scanned(packed: Vec<u8>, threads: u32) -> Lzma2MtReader<Cursor<Vec<u8>>> {
        let (mut rd, _control) = reader_limited(Cursor::new(packed), threads, u64::MAX >> 1);
        while !rd.input_done {
            rd.refill().expect("scan");
        }
        rd
    }

    /// Stored runs go narrow, LZMA-coded runs stay wide, and a mixed run is
    /// classified by the exact share of its output that is LZMA-coded, as
    /// the chunk headers declare it — not by how its packed size compares
    /// with its unpacked size.
    #[test]
    fn runs_are_classified_by_their_chunk_kinds_not_their_ratio() {
        const RUNS: usize = 4;
        // All stored: narrow.
        let mut packed = Vec::new();
        for _ in 0..RUNS {
            copy_chunks(&mut packed, 0x01, 200_000);
        }
        packed.push(0);
        let rd = scanned(packed, 8);
        assert_eq!(rd.runs_seen, RUNS as u64);
        for shape in &rd.recent_runs[..RUNS] {
            assert_eq!(
                *shape,
                super::RunShape {
                    packed: 200_000 + 4 * 3,
                    unpacked: 200_000,
                    lzma_unpacked: 0,
                }
            );
            assert!(shape.is_stored());
        }
        assert!(rd.dense);
        assert_eq!(rd.applied_threads, super::MT_DENSE_THREADS);

        // All LZMA-coded, and packed *larger* than it unpacks — which the
        // ratio called incompressible and sent narrow. It is a decode, and
        // stays as wide as it was asked to be.
        let mut packed = Vec::new();
        for _ in 0..RUNS {
            lzma_chunk(&mut packed, 0xE0, 60_000, 65_536);
            lzma_chunk(&mut packed, 0x80, 60_000, 65_536);
        }
        packed.push(0);
        let rd = scanned(packed, 8);
        assert_eq!(rd.runs_seen, RUNS as u64);
        for shape in &rd.recent_runs[..RUNS] {
            assert_eq!(
                *shape,
                super::RunShape {
                    packed: 2 * 65_536 + 6 + 5,
                    unpacked: 120_000,
                    lzma_unpacked: 120_000,
                }
            );
            assert!(shape.packed > shape.unpacked && !shape.is_stored());
        }
        assert!(!rd.dense);
        assert_eq!(rd.applied_threads, 8);

        // Mixed: one LZMA chunk ahead of stored ones. 10 000 of 650 000 is
        // under one part in 64 and the run is a copy; 20 000 of 660 000 is
        // over it and the run is a decode.
        for (lzma, stored) in [(10_000u32, true), (20_000, false)] {
            let mut packed = Vec::new();
            for _ in 0..RUNS {
                lzma_chunk(&mut packed, 0xE0, lzma, lzma / 2);
                copy_chunks(&mut packed, 0x02, 640_000);
            }
            packed.push(0);
            let rd = scanned(packed, 8);
            assert_eq!(rd.runs_seen, RUNS as u64);
            let want = super::RunShape {
                packed: u64::from(lzma / 2) + 6 + 640_000 + 10 * 3,
                unpacked: u64::from(lzma) + 640_000,
                lzma_unpacked: u64::from(lzma),
            };
            for shape in &rd.recent_runs[..RUNS] {
                assert_eq!(*shape, want, "lzma={lzma}");
                assert_eq!(shape.is_stored(), stored, "lzma={lzma}");
            }
            assert_eq!(rd.dense, stored, "lzma={lzma}");
        }
    }

    /// The shape rule against streams an encoder actually produced, which is
    /// the only way to get a run that is genuinely smaller than what it
    /// decodes to.
    #[cfg(all(feature = "compress", not(feature = "lzma-rust2-encoder")))]
    mod shape {
        use std::io::{Cursor, Read, Write};

        use lzma_turbo::{BLOCK_SIZE_SOLID, LzmaEncProps};

        use super::{drain_watching, governed, reader_limited, stream};
        use crate::codec::lzma_turbo::writer::{Coder, LzmaTurboWriter};
        use crate::codec::lzma_turbo::{MT_DENSE_THREADS, MT_SHAPE_RUNS};

        /// Room for every run of these streams at once, so that nothing but the
        /// shape rule narrows the decode.
        const LIMIT: u64 = 256 << 20;

        /// One run of data that compresses, and the bytes it decodes to.
        ///
        /// One encode per run: an encoder opens a stream by resetting the
        /// dictionary, which is what a run boundary is, so encodes laid end to end
        /// are runs without the encoder having to be talked into a block size. The
        /// end marker each one finishes with is cut off, and one put back after
        /// the last.
        fn compressible_run(len: usize) -> (Vec<u8>, Vec<u8>) {
            let plain: Vec<u8> = (0..len).map(|i| (i % 41) as u8).collect();
            let props = LzmaEncProps::new().with_level(1).with_dict_size(1 << 16);
            let mut w = LzmaTurboWriter::new(
                Vec::new(),
                &props,
                Coder::Lzma2 {
                    block_size: BLOCK_SIZE_SOLID,
                    threads: 1,
                },
            )
            .expect("build the encoder");
            w.write_all(&plain).expect("encode");
            let mut packed = w.finish().expect("finish the encode");
            assert_eq!(packed.pop(), Some(0x00), "an encode ends in its end marker");
            assert!(
                (packed.len() as u64) * 2 < len as u64,
                "the fixture has to compress for this to be testing anything",
            );
            (packed, plain)
        }

        /// `runs` runs of compressible data, as one stream.
        fn compressible_stream(runs: usize, len: usize) -> (Vec<u8>, Vec<u8>) {
            let (one, its_plain) = compressible_run(len);
            let mut packed = Vec::new();
            let mut plain = Vec::new();
            for _ in 0..runs {
                packed.extend_from_slice(&one);
                plain.extend_from_slice(&its_plain);
            }
            packed.push(0x00);
            (packed, plain)
        }

        /// A stream whose runs compress is decoded with everything the caller
        /// asked for: the rule is about the payload and not about the thread
        /// count, and a decode that narrowed here would be giving away the
        /// parallelism this reader exists for.
        ///
        /// What is asserted is the ceiling the reader applied, not how many
        /// workers came to exist under it: the decoder spawns a worker only
        /// when every one it has is busy, so on a machine whose workers
        /// finish runs faster than the reader hands them out the count stops
        /// short of the ceiling. That is scheduling, not narrowing.
        #[test]
        fn a_governed_decode_widens_to_the_fixed_width() {
            // Started at one thread under a governor asking for a thread per
            // run in hand, a decode of compressible runs has to reach the
            // width a decode fixed at the ceiling runs at, with no more than
            // one run out while it is still at one thread.
            let (packed, plain) = compressible_stream(48, 512 << 10);
            let (mut fixed, control) = reader_limited(Cursor::new(packed.clone()), 8, LIMIT);
            let (out, _peak, _widest) = drain_watching(&mut fixed, &control);
            assert_eq!(out, plain);

            let (mut rd, control) = reader_limited(Cursor::new(packed), 1, LIMIT);
            rd.widen_to = 8;
            let seen = governed(&mut rd, &control, 8);
            assert!(seen.out == plain, "the decode differs");
            assert!(!rd.dense, "narrowed on compressible runs");
            assert_eq!(
                seen.widest_applied, fixed.applied_threads,
                "the governed decode should reach the width a fixed one decodes at"
            );
            assert!(
                seen.most_backlog >= 7,
                "published a backlog of {}, not one asking for every thread",
                seen.most_backlog
            );
            assert!(
                seen.most_out_at_one <= 1,
                "{} runs out at once at one thread",
                seen.most_out_at_one
            );
        }

        /// A caller held at a ceiling below what is offered is offered no
        /// more, so the read-ahead settles at what a decode fixed at that
        /// ceiling would hold, overshooting by at most the one offer it did
        /// not take.
        #[test]
        fn a_governed_decode_held_at_its_ceiling_is_offered_no_more() {
            let (packed, plain) = compressible_stream(48, 512 << 10);
            let (mut rd, control) = reader_limited(Cursor::new(packed), 1, LIMIT);
            rd.widen_to = 16;
            let seen = governed(&mut rd, &control, 2);
            assert!(seen.out == plain, "the decode differs");
            assert_eq!(seen.widest_applied, 2);
            assert!(rd.offered > 2, "an offer was made");
            assert_eq!(
                seen.widest_offer_at_ceiling, 2,
                "and not followed, so withdrawn"
            );
        }

        /// One run at a time at one thread, and an offer no larger than the
        /// budget has room for.
        #[test]
        fn a_governed_decode_at_one_thread_or_a_small_budget_stays_within_it() {
            let (packed, plain) = compressible_stream(24, 512 << 10);
            let (mut rd, control) = reader_limited(Cursor::new(packed.clone()), 1, LIMIT);
            rd.widen_to = 8;
            let seen = governed(&mut rd, &control, 1);
            assert!(seen.out == plain, "the decode differs at a ceiling of one");
            assert_eq!(seen.widest_applied, 1);
            assert!(
                seen.most_out_at_one <= 1,
                "{} runs out",
                seen.most_out_at_one
            );

            let cost = rd.run_size().0 + rd.run_size().1;
            let (mut rd, control) = reader_limited(Cursor::new(packed), 1, 3 * cost);
            rd.widen_to = 8;
            let seen = governed(&mut rd, &control, 8);
            assert!(seen.out == plain, "the decode differs under a small budget");
            assert!(
                seen.widest_offer <= 3,
                "offered {} runs into room for 3",
                seen.widest_offer
            );
            // The limit, and what the decoder keeps outside it: see
            // `limit_ceiling`.
            let (packed_run, unpacked_run) = rd.run_size();
            let ceiling = 3 * cost + 3 * packed_run + unpacked_run + super::LIMIT_SLACK;
            assert!(
                seen.peak_in_flight <= ceiling,
                "held {} against a limit of {}",
                seen.peak_in_flight,
                3 * cost
            );
        }

        #[test]
        fn a_compressible_stream_keeps_every_thread() {
            let (packed, plain) = compressible_stream(24, 512 << 10);
            for threads in [2, 4, 8] {
                let (mut rd, control) = reader_limited(Cursor::new(packed.clone()), threads, LIMIT);
                let (out, _peak, widest) = drain_watching(&mut rd, &control);
                assert_eq!(out, plain, "threads={threads}");
                assert!(
                    !rd.dense,
                    "threads={threads}: narrowed on compressible runs"
                );
                assert_eq!(rd.applied_threads, threads, "threads={threads}");
                assert!(widest <= threads, "threads={threads}: {widest} workers");
            }
        }

        /// One run of data LZMA still codes but barely shrinks — media
        /// archived at an ordinary level — and the bytes it decodes to. The
        /// payload is pseudo-random bytes with a short run of zeros every
        /// 4 KiB, so each chunk comes out a couple of percent smaller than
        /// its input: coded, not stored, and above the old 90 percent line.
        fn barely_compressible_run(len: usize, seed: u64) -> (Vec<u8>, Vec<u8>) {
            let mut x = seed | 1;
            let plain: Vec<u8> = (0..len)
                .map(|i| {
                    if i % 4096 >= 4016 {
                        return 0;
                    }
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 24) as u8
                })
                .collect();
            let props = LzmaEncProps::new().with_level(1).with_dict_size(1 << 16);
            let mut w = LzmaTurboWriter::new(
                Vec::new(),
                &props,
                Coder::Lzma2 {
                    block_size: BLOCK_SIZE_SOLID,
                    threads: 1,
                },
            )
            .expect("build the encoder");
            w.write_all(&plain).expect("encode");
            let mut packed = w.finish().expect("finish the encode");
            assert_eq!(packed.pop(), Some(0x00), "an encode ends in its end marker");
            let (p, u) = (packed.len() as u64, len as u64);
            assert!(
                p < u && p * 100 >= u * 90,
                "the fixture has to be coded and barely smaller: {p} of {u}",
            );
            (packed, plain)
        }

        /// A stream of LZMA-coded runs that barely shrank is decode-bound,
        /// not read-bound, and keeps every thread the caller asked for: only
        /// runs no smaller than their output — stored chunks — are the shape
        /// the narrow decode is for. At the old 90 percent line this stream
        /// was held to two threads.
        #[test]
        fn a_barely_compressible_coded_stream_is_not_narrowed() {
            let mut packed = Vec::new();
            let mut plain = Vec::new();
            for seed in 0..6u64 {
                let (run, run_plain) = barely_compressible_run(256 << 10, 0x9e37_79b9 + seed);
                packed.extend_from_slice(&run);
                plain.extend_from_slice(&run_plain);
            }
            packed.push(0x00);
            let threads = 8;
            let (mut rd, control) = reader_limited(Cursor::new(packed), threads, LIMIT);
            let (out, _peak, widest) = drain_watching(&mut rd, &control);
            assert_eq!(out, plain);
            assert!(!rd.dense, "narrowed on coded runs");
            assert_eq!(rd.applied_threads, threads);
            assert!(widest <= threads, "{widest} workers");
        }

        /// An archive of a film beside a text file: compressible runs, then a span
        /// of runs that did not compress, then compressible ones again. The decode
        /// should narrow for the middle and widen again after it, and hand back
        /// every byte either way.
        #[test]
        fn a_mixed_stream_widens_again_after_the_incompressible_span() {
            let (head, head_plain) = compressible_stream(8, 512 << 10);
            // Large enough that the span is still being decoded after the
            // read that scanned it, so the narrowing can be seen from here
            // rather than having come and gone inside one refill.
            let (middle, middle_plain) = stream(24, 16);
            let (tail, tail_plain) = compressible_stream(8, 512 << 10);
            let mut packed = Vec::new();
            let mut plain = Vec::new();
            for (part, part_plain) in [
                (head, head_plain),
                (middle, middle_plain),
                (tail, tail_plain),
            ] {
                // Each part carries its own end marker, which is only the end
                // where the last one is.
                packed.extend_from_slice(&part[..part.len() - 1]);
                plain.extend_from_slice(&part_plain);
            }
            packed.push(0x00);

            let threads = 8;
            let (mut rd, _control) = reader_limited(Cursor::new(packed), threads, LIMIT);
            let mut out = Vec::new();
            let mut buf = vec![0u8; 16 << 10];
            let mut narrowed = false;
            loop {
                let n = rd.read(&mut buf).expect("decode");
                if n == 0 {
                    break;
                }
                narrowed |= rd.applied_threads == MT_DENSE_THREADS;
                out.extend_from_slice(&buf[..n]);
            }
            assert_eq!(out, plain);
            assert!(narrowed, "the incompressible span should have narrowed it");
            assert!(!rd.dense, "the tail should have widened it again");
            assert_eq!(rd.applied_threads, threads);
        }

        /// One run of the other shape does not move the decode: an encoder that
        /// meets a compressible megabyte in the middle of a film writes one such
        /// run, and a thread count that followed it would be changed twice for
        /// nothing.
        #[test]
        fn a_single_odd_run_does_not_change_the_shape() {
            let (odd, odd_plain) = compressible_run(512 << 10);
            let (dense_head, dense_head_plain) = stream(8, 4);
            let (dense_tail, dense_tail_plain) = stream(8, 4);
            let mut packed = dense_head[..dense_head.len() - 1].to_vec();
            packed.extend_from_slice(&odd);
            packed.extend_from_slice(&dense_tail);
            let mut plain = dense_head_plain;
            plain.extend_from_slice(&odd_plain);
            plain.extend_from_slice(&dense_tail_plain);

            let (mut rd, control) = reader_limited(Cursor::new(packed), 8, LIMIT);
            let (out, _peak, widest) = drain_watching(&mut rd, &control);
            assert_eq!(out, plain);
            assert!(rd.dense, "one compressible run should not have widened it");
            assert!(widest <= MT_DENSE_THREADS, "{widest} workers");
            const { assert!(MT_SHAPE_RUNS > 1, "one run in a row would flap the count") };
        }
    }
}
