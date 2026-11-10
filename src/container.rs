//! Container-level questions a streaming consumer asks before it decodes.
//!
//! Upstream's reader answers "give me the bytes of this entry". A consumer
//! that is feeding the reader from a network download, under a memory budget
//! it has to honour before it allocates, needs to ask a few more things first:
//! how much decoder memory this archive will want, which byte ranges of the
//! file each block lives in, what the per-file checksums are, and — once a
//! block has been decoded and checked — to be told so. This module is where
//! the fork adds those, as read-only accessors over the parsed [`Archive`];
//! nothing here changes how anything decodes.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use crate::WindowAllocator;
use crate::archive::{Archive, EncoderMethod};
use crate::block::Coder;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Limits a caller imposes on an archive *before* the reader allocates for it.
///
/// A 7z header is a list of numbers that describe an archive; nothing in the
/// format makes those numbers true. A file of a few hundred bytes can say it
/// has four billion entries, a 4 GiB dictionary, a header that unpacks to a
/// terabyte, or a key derivation that wants 2^63 SHA-256 rounds. This type is
/// the single place that says what this process is prepared to believe, and
/// every field is checked *before* the allocation or the work it bounds, not
/// after.
///
/// The defaults are chosen so that no archive a 7-Zip encoder would write
/// trips them — an honest 1 GiB archive is nowhere near any of them — while a
/// hostile one is refused with [`Error::LimitExceeded`], which names the bound
/// it hit. The two "what can this machine afford" fields,
/// `memory_limit_bytes` and `max_end_header_bytes`, keep defaulting to no
/// limit at all: only the caller knows what it can afford, and guessing on its
/// behalf would change what well-formed archives do.
///
/// [`Error::LimitExceeded`]: crate::Error::LimitExceeded
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct ArchiveLimits {
    /// Largest decoder footprint the caller will allow, in bytes.
    ///
    /// Default: no limit. Bounds [`Archive::decoder_memory_estimate`], which
    /// is dominated by the dictionary a block declares, and, when a header or
    /// block is decoded, the sum of its coder chain's dictionaries and models
    /// rather than each coder alone.
    ///
    /// A parallel LZMA2 decode keeps its buffers inside what is left of the
    /// limit once the chain's dictionaries and coder state are reserved: the
    /// decoder's held bytes (input pieces, runs out with workers, decoded
    /// runs waiting, parked buffers) plus the reader's queue of pieces read
    /// and not yet handed over. Dictionaries, coder state and allocator slack
    /// are additions to that, not bounded by it.
    pub memory_limit_bytes: u64,
    /// Largest end header the caller will allow to be buffered, in bytes.
    ///
    /// Default: no limit. The size comes out of the first 32 bytes of the
    /// file and the reader must buffer that many bytes to parse it, so an
    /// archive from an untrusted source can otherwise name any number.
    pub max_end_header_bytes: u64,
    /// Largest header a *compressed* header may decode to, in bytes.
    ///
    /// Default: 64 MiB. An encoded header is a block like any other, so its
    /// declared unpacked size is attacker-controlled and is buffered whole
    /// before it can be parsed: a kilobyte of input can claim to unpack to a
    /// terabyte. 64 MiB is two orders of magnitude above the largest header a
    /// real archive has been seen to have (a million files with long names is
    /// a few megabytes).
    pub max_header_unpacked_bytes: u64,
    /// How many times a header may be a compressed header containing another.
    ///
    /// Default: 2, which is one encoded header containing the real one — the
    /// only nesting 7-Zip writes. Without a bound, a header that decodes to
    /// another encoded header is an unbounded recursion driven by a few bytes
    /// of input.
    pub max_header_depth: u8,
    /// Largest count of anything the header may declare: files, blocks,
    /// coders, pack streams, sub-streams, bind pairs.
    ///
    /// Default: 1,000,000. Every count is also bounded by the bytes left in
    /// the header — a thing that is described must be described by at least
    /// one byte — but that bound has an amplification factor: a count is one
    /// header byte and the entry it reserves is tens or hundreds of bytes, so
    /// a 64 MiB header would otherwise be able to ask for gigabytes. The
    /// largest archives seen in the wild have a few hundred thousand entries.
    pub max_entries: u64,
    /// Largest a single stored file name may be, in bytes of UTF-16.
    ///
    /// Default: 64 KiB, which is four times the longest path any mainstream
    /// filesystem accepts.
    pub max_name_bytes: u64,
    /// Largest the whole names blob may be, in bytes.
    ///
    /// Default: 64 MiB, which is a million names of 64 characters.
    pub max_total_name_bytes: u64,
    /// Largest number of coders one block may chain.
    ///
    /// Default: 8. 7-Zip itself writes at most four (say AES over BCJ2 over
    /// LZMA2), and the chain is walked recursively when the decode stack is
    /// built. A block with more than 64 coders or 64 input streams, 7-Zip's
    /// own ceiling, is refused whatever this says, [`ArchiveLimits::unlimited`]
    /// included.
    pub max_coders_per_block: u64,
    /// Largest number of streams one coder may declare, in or out.
    ///
    /// Default: 8. Only BCJ2 takes more than one stream at all (four in, one
    /// out); everything else is one to one. The counts are unbounded varints,
    /// and the coder graph is walked with a linear search per stream, so a
    /// single coder claiming a million streams is a quadratic walk as well as
    /// a million-entry allocation. With this bound a block's whole graph is at
    /// most `max_coders_per_block * max_streams_per_coder` streams.
    pub max_streams_per_coder: u64,
    /// Largest number of coders in the archive, across every block.
    ///
    /// Default: 1,000,000, for the same reason as `max_entries`: a block is
    /// cheap to declare and a coder is not.
    pub max_total_coders: u64,
    /// Largest total output the caller will accept from a decode, in bytes.
    ///
    /// Default: no limit. This is the decompression-bomb bound: the sizes are
    /// in the header, so an archive whose declared output exceeds it is
    /// refused before a byte is decoded, and a stream that lies about its size
    /// is stopped when it passes the bound.
    pub max_unpack_bytes: u64,
    /// Largest ratio of output bytes to packed bytes the caller will accept.
    ///
    /// Default: no limit. LZMA legitimately reaches ratios in the thousands on
    /// repetitive data, so this is off unless a caller knows what it is
    /// feeding; `max_unpack_bytes` is the bound with a meaning that does not
    /// depend on the data.
    pub max_unpack_ratio: u64,
    /// Largest AES key-derivation work factor the caller will accept.
    ///
    /// Default: 24, which is 16.7 million SHA-256 rounds and what 7-Zip itself
    /// refuses to exceed. Values above 24 cannot raise the decoder ceiling.
    /// The special value 63 in an archive selects raw-key mode, without hashing.
    pub max_aes_cycles_power: u8,
    /// Maximum actual SHA-256 KDF rounds across the reader's password lifetime,
    /// including encoded headers and repeated block decodes. Cache hits and
    /// raw-key mode cost zero; cache eviction does not reset the counter.
    /// Default: 2^28. Checked before each derivation, so payload work can fail
    /// during extraction. A fresh password or clone starts a fresh budget; a
    /// clone shares the keys already derived, so a hit on one costs it nothing.
    pub max_aes_kdf_rounds: u64,
    /// Whether an entry whose stored name would escape the extraction
    /// directory makes the archive unreadable.
    ///
    /// Default: false — the names are reported as they are stored, and
    /// [`ArchiveEntry::is_unsafe_path`] says which ones are dangerous, because
    /// a reader is not always a writer and a consumer listing an archive
    /// should see what it actually contains. A consumer that extracts to a
    /// directory should set this, and then no entry it is handed can contain
    /// `..`, a root, a drive letter, a NUL or a backslash.
    ///
    /// [`ArchiveEntry::is_unsafe_path`]: crate::ArchiveEntry::is_unsafe_path
    pub reject_unsafe_paths: bool,
    /// Where LZMA and LZMA2 dictionaries and PPMd model arenas larger than
    /// `max_in_ram_window_bytes` come from.
    ///
    /// Default: none, and every window is allocated in RAM and counted
    /// against `memory_limit_bytes`, as without this field. With an
    /// allocator, the rule is per buffer: each decode buffer over
    /// `max_in_ram_window_bytes` is asked of it instead and counts nothing
    /// against `memory_limit_bytes`; each one at or under it is allocated in
    /// RAM and counted. The coder's other state is always RAM. See
    /// [`WindowAllocator`] for the contract and for how many windows can be
    /// alive at once.
    ///
    /// Every LZMA2 decoder takes the allocator. A threaded LZMA2 decode
    /// holds a buffer per worker, each up to `lzma-turbo`'s worker bound for
    /// the dictionary (four dictionaries, held to 1 MiB..=256 MiB, or the
    /// dictionary if larger) and to `max_lent_window_bytes`, on the caller's
    /// thread count: those over the in-RAM limit are lent whether or not the
    /// dictionary is. A positional LZMA2 run
    /// ([`crate::PositionalLzma2Folder`]) lends its ring, asked once per run.
    /// [`crate::Archive::decoder_memory`] bounds the windows out at once.
    /// The Zstandard window is never lent: it is fitted to what
    /// `memory_limit_bytes` leaves and stays in RAM.
    ///
    /// A panic in the allocator is caught and reported as
    /// [`Error::WindowAllocatorFailed`], with nothing left lent. The field is
    /// wrapped in [`AssertUnwindSafe`] so that `ArchiveLimits` stays
    /// [`UnwindSafe`](std::panic::UnwindSafe) and
    /// [`RefUnwindSafe`](std::panic::RefUnwindSafe) with any allocator: the
    /// decoder only holds it and calls it.
    ///
    /// On `wasm32` this field is skipped by `wasm-bindgen`: it exists, and
    /// [`ArchiveLimits::with_window_allocator`] sets it from Rust, but a
    /// JavaScript caller has no way to set it and decodes every window in
    /// RAM.
    ///
    /// [`WindowAllocator`]: crate::WindowAllocator
    /// [`Error::WindowAllocatorFailed`]: crate::Error::WindowAllocatorFailed
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(skip))]
    pub window_allocator: Option<AssertUnwindSafe<Arc<dyn WindowAllocator>>>,
    /// Largest decode buffer, in bytes, allocated in RAM when
    /// `window_allocator` is set. A larger one is asked of the allocator.
    ///
    /// Default: no limit, so an allocator is never asked until this is
    /// lowered. No effect without an allocator. Each buffer is judged alone,
    /// at the size the codec would allocate: an LZMA or LZMA2 dictionary,
    /// clamped to the coder's output and rounded up as the LZMA SDK rounds it
    /// (to a multiple of 4 KiB, 1 MiB or 4 MiB by size); a threaded LZMA2
    /// worker's run buffer; a positional LZMA2 run's ring; a PPMd model's
    /// declared size, rounded up to a multiple of four.
    pub max_in_ram_window_bytes: u64,
    /// Largest window, in bytes, the allocator may be asked for.
    ///
    /// Default: 4 GiB, the largest dictionary an LZMA or LZMA2 coder can
    /// declare. The window size is a header field, so an archive of a few
    /// hundred bytes can ask a caller's allocator for 4 GiB of disk; this
    /// bounds that. A window over it is refused with
    /// [`Error::LimitExceeded`] of [`Limit::LentWindowBytes`] before the
    /// allocator is asked. No effect without an allocator.
    ///
    /// A ceiling under `lzma-turbo`'s `Lzma2Decoder::max_worker_window_len`
    /// for an LZMA2 dictionary that fits it does not refuse a threaded
    /// decode: a run longer than the ceiling is not given to a worker but
    /// decoded on one thread, by the decoder that follows the stream, into
    /// the dictionary. Such a run completes and does not fail; it decodes no
    /// faster than a single-threaded decode would.
    ///
    /// [`Error::LimitExceeded`]: crate::Error::LimitExceeded
    /// [`Limit::LentWindowBytes`]: crate::Limit::LentWindowBytes
    pub max_lent_window_bytes: u64,
}

impl std::fmt::Debug for ArchiveLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveLimits")
            .field("memory_limit_bytes", &self.memory_limit_bytes)
            .field("max_end_header_bytes", &self.max_end_header_bytes)
            .field("max_header_unpacked_bytes", &self.max_header_unpacked_bytes)
            .field("max_header_depth", &self.max_header_depth)
            .field("max_entries", &self.max_entries)
            .field("max_name_bytes", &self.max_name_bytes)
            .field("max_total_name_bytes", &self.max_total_name_bytes)
            .field("max_coders_per_block", &self.max_coders_per_block)
            .field("max_streams_per_coder", &self.max_streams_per_coder)
            .field("max_total_coders", &self.max_total_coders)
            .field("max_unpack_bytes", &self.max_unpack_bytes)
            .field("max_unpack_ratio", &self.max_unpack_ratio)
            .field("max_aes_cycles_power", &self.max_aes_cycles_power)
            .field("max_aes_kdf_rounds", &self.max_aes_kdf_rounds)
            .field("reject_unsafe_paths", &self.reject_unsafe_paths)
            .field(
                "window_allocator",
                &self.window_allocator.as_ref().map(|_| "WindowAllocator"),
            )
            .field("max_in_ram_window_bytes", &self.max_in_ram_window_bytes)
            .field("max_lent_window_bytes", &self.max_lent_window_bytes)
            .finish()
    }
}

// `ArchiveLimits` keeps the auto traits it had before it held an allocator:
// a caller can still send it across threads or hold it across a
// `catch_unwind`.
const _: () = {
    const fn auto_traits<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe + Send + Sync>() {}
    auto_traits::<ArchiveLimits>();
};

/// A clone shares the allocator: the same `Arc`.
impl Clone for ArchiveLimits {
    fn clone(&self) -> Self {
        Self {
            window_allocator: self
                .window_allocator
                .as_ref()
                .map(|allocator| AssertUnwindSafe(Arc::clone(&allocator.0))),
            ..*self
        }
    }
}

/// Two limits are equal when every bound is, and both have no allocator or
/// the same one (the same `Arc` allocation).
impl PartialEq for ArchiveLimits {
    fn eq(&self, other: &Self) -> bool {
        let same_allocator = match (&self.window_allocator, &other.window_allocator) {
            (None, None) => true,
            (Some(a), Some(b)) => std::ptr::addr_eq(Arc::as_ptr(&a.0), Arc::as_ptr(&b.0)),
            _ => false,
        };
        same_allocator
            && self.memory_limit_bytes == other.memory_limit_bytes
            && self.max_end_header_bytes == other.max_end_header_bytes
            && self.max_header_unpacked_bytes == other.max_header_unpacked_bytes
            && self.max_header_depth == other.max_header_depth
            && self.max_entries == other.max_entries
            && self.max_name_bytes == other.max_name_bytes
            && self.max_total_name_bytes == other.max_total_name_bytes
            && self.max_coders_per_block == other.max_coders_per_block
            && self.max_streams_per_coder == other.max_streams_per_coder
            && self.max_total_coders == other.max_total_coders
            && self.max_unpack_bytes == other.max_unpack_bytes
            && self.max_unpack_ratio == other.max_unpack_ratio
            && self.max_aes_cycles_power == other.max_aes_cycles_power
            && self.max_aes_kdf_rounds == other.max_aes_kdf_rounds
            && self.reject_unsafe_paths == other.reject_unsafe_paths
            && self.max_in_ram_window_bytes == other.max_in_ram_window_bytes
            && self.max_lent_window_bytes == other.max_lent_window_bytes
    }
}

impl Eq for ArchiveLimits {}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            memory_limit_bytes: u64::MAX,
            max_end_header_bytes: u64::MAX,
            max_header_unpacked_bytes: 64 * MIB,
            max_header_depth: 2,
            max_entries: 1_000_000,
            max_name_bytes: 64 * KIB,
            max_total_name_bytes: 64 * MIB,
            max_coders_per_block: 8,
            max_streams_per_coder: 8,
            max_total_coders: 1_000_000,
            max_unpack_bytes: u64::MAX,
            max_unpack_ratio: u64::MAX,
            max_aes_cycles_power: 24,
            max_aes_kdf_rounds: 1 << 28,
            reject_unsafe_paths: false,
            window_allocator: None,
            max_in_ram_window_bytes: u64::MAX,
            max_lent_window_bytes: DEFAULT_MAX_LENT_WINDOW_BYTES,
        }
    }
}

/// The default [`ArchiveLimits::max_lent_window_bytes`]: a 4 GiB - 1
/// dictionary, rounded up as the decoder rounds it.
const DEFAULT_MAX_LENT_WINDOW_BYTES: u64 = 1 << 32;

impl ArchiveLimits {
    /// Both of the caller-affordability limits at once, with every structural
    /// limit left at its default.
    #[must_use]
    pub fn new(memory_limit_bytes: u64, max_end_header_bytes: u64) -> Self {
        Self {
            memory_limit_bytes,
            max_end_header_bytes,
            ..Self::default()
        }
    }

    /// Only a decoder-memory limit.
    #[must_use]
    pub fn memory(memory_limit_bytes: u64) -> Self {
        Self {
            memory_limit_bytes,
            ..Self::default()
        }
    }

    /// Bounds the total decoded output, in bytes.
    #[must_use]
    pub fn with_max_unpack_bytes(mut self, bytes: u64) -> Self {
        self.max_unpack_bytes = bytes;
        self
    }

    /// Refuses entries whose stored names would escape an extraction
    /// directory.
    #[must_use]
    pub fn rejecting_unsafe_paths(mut self) -> Self {
        self.reject_unsafe_paths = true;
        self
    }

    /// Removes caller-configurable limits on counts, names, coders and nesting.
    /// The decoder's hard AES power ceiling of 24 still applies; raw-key mode
    /// (63) is accepted because it performs no derivation.
    ///
    /// For reading archives this process wrote itself. Nothing else should use
    /// it, and nothing in this crate calls it.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            memory_limit_bytes: u64::MAX,
            max_end_header_bytes: u64::MAX,
            max_header_unpacked_bytes: u64::MAX,
            max_header_depth: u8::MAX,
            max_entries: u64::MAX,
            max_name_bytes: u64::MAX,
            max_total_name_bytes: u64::MAX,
            max_coders_per_block: u64::MAX,
            max_streams_per_coder: u64::MAX,
            max_total_coders: u64::MAX,
            max_unpack_bytes: u64::MAX,
            max_unpack_ratio: u64::MAX,
            max_aes_cycles_power: 63,
            max_aes_kdf_rounds: u64::MAX,
            reject_unsafe_paths: false,
            window_allocator: None,
            max_in_ram_window_bytes: u64::MAX,
            max_lent_window_bytes: u64::MAX,
        }
    }

    /// Takes every LZMA and LZMA2 dictionary and PPMd model arena over
    /// `max_in_ram_window_bytes` from `allocator`, up to
    /// [`max_lent_window_bytes`](Self::max_lent_window_bytes).
    #[must_use]
    pub fn with_window_allocator(
        mut self,
        allocator: Arc<dyn WindowAllocator>,
        max_in_ram_window_bytes: u64,
    ) -> Self {
        self.window_allocator = Some(AssertUnwindSafe(allocator));
        self.max_in_ram_window_bytes = max_in_ram_window_bytes;
        self
    }

    /// The decoder-memory limit in the kilobytes the internal decoder uses,
    /// saturating rather than wrapping on an unlimited budget.
    pub(crate) fn memory_limit_kb(&self) -> usize {
        usize::try_from(self.memory_limit_bytes.div_ceil(KIB)).unwrap_or(usize::MAX)
    }

    /// Whether a window of `bytes` is asked of the caller's allocator rather
    /// than allocated in RAM. The one place that choice is made.
    pub(crate) fn lends(&self, bytes: u64) -> bool {
        self.window_allocator.is_some() && bytes > self.max_in_ram_window_bytes
    }
}

/// A coder with no memory model, from [`Archive::decoder_memory_estimate`].
///
/// Sizing stops at the first one rather than skipping it: a budget built from
/// a chain with an unknown link in it would be a guess presented as a
/// measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsizedCoder {
    /// The coder's method id, as it appears in the archive.
    pub method_id: Vec<u8>,
    /// Why it could not be sized.
    pub reason: &'static str,
}

impl std::fmt::Display for UnsizedCoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "coder {:02x?}: {}", self.method_id, self.reason)
    }
}

impl std::error::Error for UnsizedCoder {}

/// Where one of a block's packed streams lives in the archive file.
///
/// Offsets are absolute: a caller can hand the range straight to a reader that
/// only knows about byte positions, with no 7z arithmetic of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackStreamRange {
    /// Index of the stream in [`Archive::pack_sizes`].
    ///
    /// [`Archive::pack_sizes`]: crate::Archive::pack_sizes
    pub index: usize,
    /// Absolute offset of the first packed byte in the archive.
    pub offset: u64,
    /// Number of packed bytes.
    pub size: u64,
}

impl PackStreamRange {
    /// Offset one past the last packed byte.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.size)
    }
}

/// One entry's worth of sub-stream metadata, as `SubStreamsInfo` records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubStream {
    /// Index into the archive's flat list of sub-streams.
    pub index: usize,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// CRC-32 of the uncompressed bytes, when the archive records one.
    pub crc: Option<u32>,
}

/// A block that finished decoding, reported to a completion hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCompletion {
    /// Index of the block in [`Archive::blocks`].
    ///
    /// [`Archive::blocks`]: crate::Archive::blocks
    pub block_index: usize,
    /// Uncompressed bytes the block produced.
    pub unpacked_size: u64,
    /// Whether a CRC-32 was checked against the header while decoding it. It
    /// is `false` when the archive records no checksum for the block or its
    /// sub-streams, not when a check failed — a failed check is an error, and
    /// the hook is never reached.
    pub crc_verified: bool,
}

/// One sub-stream — one file — whose CRC-32 has become final, reported to a
/// completion hook as the decode reaches the end of it.
///
/// A 7z block holds a run of files whose bytes are one uncompressed stream,
/// and `SubStreamsInfo` records a checksum per file rather than per block. A
/// consumer that wants to report per-file integrity has, without this, to read
/// the bytes a second time to checksum them. The decoder has already done that
/// work — the reader checks each file's CRC against the header as it goes —
/// so this hands the answer over instead of throwing it away.
///
/// The CRC reported here has already been checked against the header: a
/// mismatch is [`Error::BlockDecode`] of kind
/// [`BlockErrorKind::ChecksumMismatch`] and the hook is never reached. Files
/// the archive records no checksum for are not reported at all.
///
/// [`Error::BlockDecode`]: crate::Error::BlockDecode
/// [`BlockErrorKind::ChecksumMismatch`]: crate::BlockErrorKind::ChecksumMismatch
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubStreamCompletion {
    /// Index of the block in [`Archive::blocks`].
    ///
    /// [`Archive::blocks`]: crate::Archive::blocks
    pub block_index: usize,
    /// Index of the sub-stream in the archive's flat list, as
    /// [`SubStream::index`] numbers them.
    pub sub_stream_index: usize,
    /// Index of the entry in [`Archive::files`].
    ///
    /// [`Archive::files`]: crate::Archive::files
    pub file_index: usize,
    /// Offset of this file's first byte within the block's uncompressed
    /// stream. This is the coordinate the parallel decoder's output blocks
    /// carry, so segments and files are in the same space.
    pub unpacked_offset: u64,
    /// Length of the file in bytes.
    pub len: u64,
    /// The CRC-32 of those bytes.
    pub crc32: u32,
}

// ---------------------------------------------------------------------------
// CRC folding
// ---------------------------------------------------------------------------

/// Combines two CRC-32s: the checksum of `a ++ b`, given the checksum of each
/// piece and the length of the second.
///
/// A parallel decode computes a checksum per piece of output, on the thread
/// that produced the piece. Turning those into the checksum of a whole file —
/// or of a whole block, or of bytes spanning several blocks — is this, and it
/// costs microseconds regardless of how long the pieces are, because it works
/// on the polynomial rather than on the bytes.
///
/// This crate folds the pieces of a file itself; what is re-exported here is
/// for a consumer folding across boundaries this crate does not know about,
/// so that it does so with the same implementation the workers checksummed
/// with rather than a second copy of it.
pub use lzma_turbo::crc::crc32_combine;

/// Folds checksummed pieces into the checksum of any range they cover.
///
/// The pieces may be pushed in any order and are folded with
/// [`crc32_combine`]; a range is answered only when the pieces cover it
/// exactly. Use it to fold across blocks, or across whatever boundaries a
/// consumer has that a 7z archive does not.
pub use lzma_turbo::crc::CrcFolder;

// ---------------------------------------------------------------------------
// Decoder memory model
// ---------------------------------------------------------------------------

/// Range decoder input buffer plus the LZMA state tables. The LZMA2 reader's
/// own accounting names 40 KiB of state and a 64 KiB compressed-chunk buffer;
/// LZMA's is the same order. Rounded up to a full megabyte.
const LZ_STATE_BYTES: u64 = MIB;
/// The PPMd model is one allocation of exactly the declared size; this covers
/// the decoder's own tables around it.
const PPMD_STATE_BYTES: u64 = MIB;
/// What `bzip2-turbo` bounds its compact single-threaded decoder at: 6 MiB, or
/// 8 MiB on Linux x86-64 and AArch64, where its tables are whole 2 MiB pages.
/// A coder the pipeline gives decode workers charges the rest of their bound
/// at decode time: see [`bzip2_worker_bytes`].
pub(crate) const BZIP2_BYTES: u64 = 8 * MIB;
/// What a compact BZip2 decoder with `workers` decode workers holds beyond
/// [`BZIP2_BYTES`]. The bound grows with every worker.
#[cfg(feature = "bzip2")]
pub(crate) const fn bzip2_worker_bytes(workers: u32) -> u64 {
    bzip2_turbo::Decoder::<std::io::Empty>::low_memory_usage(workers as usize)
        .saturating_sub(BZIP2_BYTES)
}
// The estimate must cover the codec's own bound wherever it is built.
#[cfg(feature = "bzip2")]
const _: () = assert!(BZIP2_BYTES >= bzip2_turbo::Decoder::<std::io::Empty>::low_memory_usage(1));
/// Deflate's window is 32 KiB and the reader wraps its input in a buffer.
const DEFLATE_BYTES: u64 = MIB;
/// Brotli's largest standard window is 16 MiB.
const BROTLI_BYTES: u64 = 32 * MIB;
/// The zstd decoder refuses frames whose window exceeds 128 MiB unless told
/// otherwise, and nothing here tells it otherwise.
const ZSTD_BYTES: u64 = 160 * MIB;
/// An LZ4 frame block is at most 4 MiB, plus a 64 KiB dictionary.
const LZ4_BYTES: u64 = 16 * MIB;
/// Branch/call/jump filters and the delta filter keep a few hundred bytes of
/// state; AES keeps a block and, for a caller reading under 64 KiB at a time,
/// a 64 KiB plaintext buffer. One megabyte covers any of them with room.
const FILTER_BYTES: u64 = MIB;
/// BCJ2 reads four streams at once and keeps a range coder over one of them.
/// Its sub-streams' own decoders are separate coders in the same block and are
/// summed with it.
const BCJ2_BYTES: u64 = 16 * MIB;
/// A block reads its pack stream through one buffer under its coders,
/// whatever they are, so even a Copy block costs this much. Charged to every
/// block, BCJ2's included, which has none (its own estimate covers its
/// buffers): the estimate stays an upper bound on the reader's chain check.
pub(crate) const BLOCK_INPUT_BYTES: u64 = crate::decoder::INPUT_BUF_SIZE as u64;

pub(crate) fn check_aes_coders<'a>(
    coders: impl Iterator<Item = &'a Coder>,
    limits: &ArchiveLimits,
) -> Result<(), crate::Error> {
    for coder in coders {
        if coder.encoder_method_id() != EncoderMethod::ID_AES256_SHA256 {
            continue;
        }
        let power = coder
            .properties()
            .first()
            .ok_or_else(|| crate::Error::corrupted("AES256 properties too short"))?
            & 63;
        let cap = limits
            .max_aes_cycles_power
            .min(crate::encryption::MAX_AES_CYCLES_POWER);
        if power != 63 && power > cap {
            return Err(crate::Error::limit(
                crate::Limit::AesCyclesPower,
                cap.into(),
                power.into(),
            ));
        }
    }
    Ok(())
}

impl Archive {
    /// Bytes a single-threaded decode of this archive needs for its decoders.
    ///
    /// Blocks decode one after another, so the answer is the most expensive
    /// block, not the sum of them; within a block the coders are nested
    /// readers that are all live at once, so a block costs the sum of its
    /// chain. Nothing here is exact to the byte: each coder contributes its
    /// dominant allocation — the dictionary, the PPMd model — plus a margin
    /// that covers its state and buffers.
    ///
    /// | Coder | Estimate |
    /// | --- | --- |
    /// | every block, under its coders | 64 KiB (the pack stream's read buffer) |
    /// | Copy | 0 (owned push is disabled under finite memory limits) |
    /// | LZMA, LZMA2 | dictionary, clamped to the coder's output as the decoder clamps it, + 1 MiB |
    /// | PPMd | declared model size + 1 MiB |
    /// | BZip2 | 8 MiB |
    /// | Deflate | 1 MiB |
    /// | Brotli | 32 MiB |
    /// | Zstd | 160 MiB |
    /// | LZ4 | 16 MiB |
    /// | BCJ2 | 16 MiB |
    /// | BCJ, delta, AES-256 | 1 MiB |
    ///
    /// The model is for the **single-threaded** decoders, which is what this
    /// crate currently uses for every coder. A multi-threaded LZMA2 reader
    /// buffers a whole run of dependent chunks before decoding any of it, so
    /// its footprint scales with the block rather than with the dictionary,
    /// and this estimate would not describe it. Nor does it count the pipes
    /// of a block decoded as a pipeline on more than one thread, 1.25 MiB
    /// each and one or two for every coder given a thread of its own: those
    /// are charged to [`ArchiveLimits::memory_limit_bytes`] when the block
    /// is decoded, beside every coder of its chain at this table's figures,
    /// and a block they do not fit decodes without them.
    ///
    /// Every window is counted here as RAM. With a
    /// [`crate::WindowAllocator`], [`Archive::decoder_memory`] gives the
    /// figure held in RAM and the windows lent, apart.
    ///
    /// [`ArchiveLimits::memory_limit_bytes`]: crate::ArchiveLimits::memory_limit_bytes
    ///
    /// # Errors
    ///
    /// [`UnsizedCoder`] for the first coder with no model, including one whose
    /// properties are too short to read a size out of.
    pub fn decoder_memory_estimate(&self) -> Result<u64, UnsizedCoder> {
        self.resident_memory_estimate(&ArchiveLimits::default())
    }

    /// What decoding this archive under `limits` on up to `threads` threads
    /// holds, in RAM and on the caller's [`crate::WindowAllocator`].
    ///
    /// `resident_bytes` is [`Archive::decoder_memory_estimate`] with the
    /// windows `limits` would lend left out, for one folder: what is checked
    /// against [`ArchiveLimits::memory_limit_bytes`] as each folder is
    /// decoded. `lent_windows` and `lent_bytes` are the most windows, and the
    /// most bytes of them, the allocator can have out at once over the whole
    /// decode, so a caller can reserve scratch space before decoding. Without
    /// an allocator, nothing is lent and `resident_bytes` is
    /// [`Archive::decoder_memory_estimate`].
    ///
    /// `threads` is the most threads the reader decodes on: its own thread
    /// count, or a higher ceiling set through
    /// [`crate::Lzma2Handle::set_threads`] while it decodes; for a
    /// [`crate::PositionalLzma2Folder`], the most runs decoded at once. The
    /// lent figures cover every path the reader takes on that many threads:
    /// one folder with a threaded LZMA2 coder (one window per thread, each up
    /// to `lzma-turbo`'s `max_worker_window_len`, and one for the decoder
    /// that follows the stream), folders decoded side by side
    /// ([`crate::ArchiveReader::for_each_entries_unordered`], or small folders
    /// on a positional source), and runs of one folder decoded on their own.
    ///
    /// Each window is the one the decoder allocates: the dictionary clamped
    /// to its coder's output, judged against `max_in_ram_window_bytes` by
    /// the decision the decoder makes. Every figure is an upper bound: each
    /// coder's state is counted at its margin, and threads are counted as if
    /// each could hold the largest buffer a worker can be lent.
    ///
    /// # Errors
    ///
    /// [`UnsizedCoder`], as for [`Archive::decoder_memory_estimate`].
    pub fn decoder_memory(
        &self,
        limits: &ArchiveLimits,
        threads: u32,
    ) -> Result<DecoderMemory, UnsizedCoder> {
        let threads = threads.clamp(1, 256);
        // A threaded LZMA2 decoder runs on at least two workers where it can
        // start them, and holds one more window than it has workers.
        let lzma2_threads = if cfg!(target_arch = "wasm32") {
            u64::from(threads)
        } else {
            u64::from(threads.max(2))
        };
        let mut memory = DecoderMemory::default();
        // Per folder: its windows and their bytes, each coder lent once.
        let mut folders: Vec<(usize, u64)> = Vec::with_capacity(self.blocks.len());
        let mut worker_window = None::<u64>;
        for block in &self.blocks {
            let mut resident = BLOCK_INPUT_BYTES;
            let (mut windows, mut lent) = (0usize, 0u64);
            for (index, coder) in block.coders.iter().enumerate() {
                let split =
                    coder_memory_split(coder, block.get_unpack_size_at_index(index), limits)?;
                resident = resident.saturating_add(split.in_ram);
                if let Some(window) = split.lent {
                    windows += 1;
                    lent = lent.saturating_add(window);
                }
                if let Some(worker) = split.worker {
                    worker_window = Some(worker_window.map_or(worker, |w| w.max(worker)));
                }
            }
            memory.resident_bytes = memory.resident_bytes.max(resident);
            folders.push((windows, lent));
        }
        // As many folders as threads can be live at once, each with its
        // coders' windows.
        let live = (threads as usize).min(folders.len());
        let mut by_windows: Vec<usize> = folders.iter().map(|f| f.0).collect();
        by_windows.sort_unstable_by(|a, b| b.cmp(a));
        let mut by_bytes: Vec<u64> = folders.iter().map(|f| f.1).collect();
        by_bytes.sort_unstable_by(|a, b| b.cmp(a));
        memory.lent_windows = by_windows[..live].iter().sum();
        memory.lent_bytes = by_bytes[..live]
            .iter()
            .fold(0u64, |sum, b| sum.saturating_add(*b));
        // And every thread may hold one lent buffer of a threaded LZMA2
        // coder's workers, or one run's ring, each at most the largest.
        if let Some(worker) = worker_window {
            memory.lent_windows = memory.lent_windows.saturating_add(lzma2_threads as usize);
            memory.lent_bytes = memory
                .lent_bytes
                .saturating_add(worker.saturating_mul(lzma2_threads));
        }
        Ok(memory)
    }

    /// [`Archive::decoder_memory`]'s `resident_bytes`.
    pub(crate) fn resident_memory_estimate(
        &self,
        limits: &ArchiveLimits,
    ) -> Result<u64, UnsizedCoder> {
        Ok(self.decoder_memory(limits, 1)?.resident_bytes)
    }
}

/// What [`Archive::decoder_memory`] reports: the decoder memory held in RAM
/// for one folder, and the most a whole decode asks of the caller's
/// allocator at once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecoderMemory {
    /// Bytes held in RAM and checked against
    /// [`ArchiveLimits::memory_limit_bytes`].
    pub resident_bytes: u64,
    /// The most windows the caller's allocator can have lent at once.
    pub lent_windows: usize,
    /// The most bytes the caller's allocator can have lent at once, its
    /// windows summed.
    pub lent_bytes: u64,
}

/// What the coders of `block` hold in RAM under `limits`, summed: each at
/// [`coder_memory_estimate`]'s margin for its state, plus its dictionary or
/// model, clamped to the coder's output as the decoder clamps it, unless
/// `limits` lend it. The decoder makes the same decision, on the same
/// window, when it builds each coder.
pub(crate) fn block_coders_resident(
    block: &crate::block::Block,
    limits: &ArchiveLimits,
) -> Result<u64, UnsizedCoder> {
    block
        .coders
        .iter()
        .enumerate()
        .try_fold(0u64, |sum, (index, coder)| {
            let split = coder_memory_split(coder, block.get_unpack_size_at_index(index), limits)?;
            Ok(sum.saturating_add(split.in_ram))
        })
}

/// One coder's memory split by `limits`.
struct CoderSplit {
    /// The bytes held in RAM.
    in_ram: u64,
    /// The window asked of the caller's allocator, if one is.
    lent: Option<u64>,
    /// For LZMA2, the largest buffer one worker of a threaded decoder of it
    /// can be lent, if any can.
    worker: Option<u64>,
}

/// `coder`, decoding to `unpack_len` bytes, split by `limits`: its window by
/// [`crate::decoder::coder_window_split`], the decision the decoder makes,
/// and its state at [`coder_memory_estimate`]'s margin. A coder with no
/// window is held in RAM at [`coder_memory_estimate`].
fn coder_memory_split(
    coder: &Coder,
    unpack_len: u64,
    limits: &ArchiveLimits,
) -> Result<CoderSplit, UnsizedCoder> {
    let full = coder_memory_estimate(coder)?;
    let method_id = coder.encoder_method_id();
    let margin = if method_id == EncoderMethod::ID_LZMA || method_id == EncoderMethod::ID_LZMA2 {
        LZ_STATE_BYTES
    } else if method_id == EncoderMethod::ID_PPMD {
        PPMD_STATE_BYTES
    } else {
        0
    };
    Ok(
        match crate::decoder::coder_window_split(coder, unpack_len, limits) {
            Some(window) if margin > 0 => CoderSplit {
                in_ram: margin.saturating_add(window.in_ram),
                lent: window.lent,
                worker: window.worker,
            },
            _ => CoderSplit {
                in_ram: full,
                lent: None,
                worker: None,
            },
        },
    )
}

/// The arena a PPMd model of `memory_size` bytes is laid out in, and so the
/// window asked of a caller's allocator: the size rounded up to a multiple of
/// four, as `ppmd-turbo` lays it out.
#[cfg(feature = "ppmd")]
pub(crate) fn ppmd_arena_bytes(memory_size: u32) -> u64 {
    u64::from(memory_size).next_multiple_of(4)
}

/// Bytes one coder's decoder needs, its window counted as RAM; see
/// [`Archive::decoder_memory`] for the split under a window allocator.
///
/// # Errors
///
/// [`UnsizedCoder`] if there is no model for this method id.
pub fn coder_memory_estimate(coder: &Coder) -> Result<u64, UnsizedCoder> {
    let method_id = coder.encoder_method_id();
    let properties = coder.properties();

    if method_id == EncoderMethod::ID_COPY {
        Ok(0)
    } else if method_id == EncoderMethod::ID_LZMA {
        Ok(lzma_dictionary_bytes(method_id, properties)?.saturating_add(LZ_STATE_BYTES))
    } else if method_id == EncoderMethod::ID_LZMA2 {
        Ok(lzma2_dictionary_bytes(method_id, properties)?.saturating_add(LZ_STATE_BYTES))
    } else if method_id == EncoderMethod::ID_PPMD {
        Ok(ppmd_model_bytes(method_id, properties)?.saturating_add(PPMD_STATE_BYTES))
    } else if method_id == EncoderMethod::ID_BZIP2 {
        Ok(BZIP2_BYTES)
    } else if method_id == EncoderMethod::ID_DEFLATE {
        Ok(DEFLATE_BYTES)
    } else if method_id == EncoderMethod::ID_BROTLI {
        Ok(BROTLI_BYTES)
    } else if method_id == EncoderMethod::ID_ZSTD {
        Ok(ZSTD_BYTES)
    } else if method_id == EncoderMethod::ID_LZ4 {
        Ok(LZ4_BYTES)
    } else if method_id == EncoderMethod::ID_BCJ2 {
        Ok(BCJ2_BYTES)
    } else if method_id == EncoderMethod::ID_AES256_SHA256
        || method_id == EncoderMethod::ID_DELTA
        || method_id == EncoderMethod::ID_BCJ_X86
        || method_id == EncoderMethod::ID_BCJ_ARM
        || method_id == EncoderMethod::ID_BCJ_ARM64
        || method_id == EncoderMethod::ID_BCJ_ARM_THUMB
        || method_id == EncoderMethod::ID_BCJ_PPC
        || method_id == EncoderMethod::ID_BCJ_IA64
        || method_id == EncoderMethod::ID_BCJ_SPARC
        || method_id == EncoderMethod::ID_BCJ_RISCV
    {
        Ok(FILTER_BYTES)
    } else {
        Err(UnsizedCoder {
            method_id: method_id.to_vec(),
            reason: "no memory model for this coder",
        })
    }
}

/// LZMA properties are five bytes: lc/lp/pb, then the dictionary size as a
/// little-endian `u32`.
fn lzma_dictionary_bytes(method_id: &[u8], properties: &[u8]) -> Result<u64, UnsizedCoder> {
    if properties.len() < 5 {
        return Err(UnsizedCoder {
            method_id: method_id.to_vec(),
            reason: "LZMA properties shorter than five bytes",
        });
    }
    let dict = u32::from_le_bytes([properties[1], properties[2], properties[3], properties[4]]);
    Ok(u64::from(dict))
}

/// LZMA2 properties are one byte encoding the dictionary size: values up to 39
/// map to `(2 | (p & 1)) << (p / 2 + 11)`, and 40 means the 4 GiB maximum. The
/// decoder rounds the dictionary up to a multiple of sixteen before allocating
/// it, so this does too.
fn lzma2_dictionary_bytes(method_id: &[u8], properties: &[u8]) -> Result<u64, UnsizedCoder> {
    let Some(&bits) = properties.first() else {
        return Err(UnsizedCoder {
            method_id: method_id.to_vec(),
            reason: "LZMA2 properties empty",
        });
    };
    let bits = u64::from(bits);
    if bits & !0x3F != 0 {
        return Err(UnsizedCoder {
            method_id: method_id.to_vec(),
            reason: "LZMA2 property byte has reserved bits set",
        });
    }
    if bits > 40 {
        return Err(UnsizedCoder {
            method_id: method_id.to_vec(),
            reason: "LZMA2 dictionary larger than the 4 GiB maximum",
        });
    }
    let dict = if bits == 40 {
        u64::from(u32::MAX)
    } else {
        (2 | (bits & 1)) << (bits / 2 + 11)
    };
    Ok((dict + 15) & !15)
}

/// PPMd properties are five bytes: the model order, then the model memory size
/// as a little-endian `u32`. The decoder allocates exactly that.
fn ppmd_model_bytes(method_id: &[u8], properties: &[u8]) -> Result<u64, UnsizedCoder> {
    if properties.len() < 5 {
        return Err(UnsizedCoder {
            method_id: method_id.to_vec(),
            reason: "PPMd properties shorter than five bytes",
        });
    }
    let memory = u32::from_le_bytes([properties[1], properties[2], properties[3], properties[4]]);
    Ok(u64::from(memory))
}

// ---------------------------------------------------------------------------
// Read-only views a streaming consumer needs
// ---------------------------------------------------------------------------

impl Archive {
    /// Validate per-coder powers before constructing decoders.
    pub(crate) fn check_aes_work(&self, limits: &ArchiveLimits) -> Result<(), crate::Error> {
        check_aes_coders(self.blocks.iter().flat_map(|block| &block.coders), limits)
    }

    /// Total number of sub-streams across every block.
    ///
    /// A sub-stream is one entry's worth of a block's output. In a non-solid
    /// archive every block has exactly one; in a solid one a block holds many,
    /// and the per-entry sizes and checksums live in `SubStreamsInfo` rather
    /// than on the block.
    #[must_use]
    pub fn num_unpack_sub_streams(&self) -> usize {
        self.blocks
            .iter()
            .map(|block| block.num_unpack_sub_streams)
            .sum()
    }

    /// The sub-stream at `index` in the archive's flat sub-stream order.
    ///
    /// Returns `None` past the end. The CRC is `None` when the archive records
    /// none for that sub-stream, which is legal and not a defect.
    #[must_use]
    pub fn sub_stream(&self, index: usize) -> Option<SubStream> {
        let info = self.sub_streams_info.as_ref()?;
        let size = *info.unpack_sizes.get(index)?;
        Some(SubStream {
            index,
            size,
            crc: info
                .has_crc
                .contains(index)
                .then(|| info.crcs.get(index).copied())
                .flatten(),
        })
    }

    /// Every sub-stream of one block, in order.
    ///
    /// Empty when `block_index` is out of range. When the archive omits
    /// `SubStreamsInfo`, parsing infers one sub-stream per block with the
    /// block's unpacked size and any defined block checksum.
    #[must_use]
    pub fn block_sub_streams(&self, block_index: usize) -> Vec<SubStream> {
        let Some(block) = self.blocks.get(block_index) else {
            return Vec::new();
        };
        let Some(first) = self
            .stream_map
            .block_first_sub_stream_index
            .get(block_index)
        else {
            return Vec::new();
        };
        (first..first + block.num_unpack_sub_streams)
            .filter_map(|index| self.sub_stream(index))
            .collect()
    }

    /// Absolute byte ranges of a block's packed streams, in the order the
    /// block binds them.
    ///
    /// Most blocks have exactly one; a BCJ2 block has four. Empty when
    /// `block_index` is out of range. Offsets are from the start of the
    /// archive file, so a caller can prefetch or bound a read without doing
    /// any 7z arithmetic of its own.
    #[must_use]
    pub fn block_pack_streams(&self, block_index: usize) -> Vec<PackStreamRange> {
        let Some(block) = self.blocks.get(block_index) else {
            return Vec::new();
        };
        let Some(&first) = self
            .stream_map
            .block_first_pack_stream_index
            .get(block_index)
        else {
            return Vec::new();
        };
        let count = block.packed_streams().len().max(1);
        (first..first + count)
            .filter_map(|index| {
                let offset = *self.stream_map.pack_stream_offsets.get(index)?;
                let size = *self.pack_sizes.get(index)?;
                Some(PackStreamRange {
                    index,
                    offset: crate::archive::SIGNATURE_HEADER_SIZE
                        .saturating_add(self.pack_pos)
                        .saturating_add(offset),
                    size,
                })
            })
            .collect()
    }

    /// The coder chain of one block, in the archive's own order.
    ///
    /// Same slice as `archive.blocks[i].coders`, as a checked lookup.
    #[must_use]
    pub fn block_coders(&self, block_index: usize) -> &[Coder] {
        self.blocks
            .get(block_index)
            .map_or(&[], |block| block.coders.as_slice())
    }
}

#[cfg(test)]
mod aes_budget_tests {
    use super::*;

    #[test]
    fn raw_key_mode_is_allowed_but_the_hard_power_ceiling_remains() {
        let mut coder = Coder::default();
        coder.set_id_size(4);
        coder
            .decompression_method_id_mut()
            .copy_from_slice(EncoderMethod::ID_AES256_SHA256);
        coder.set_properties(vec![63, 0].into());
        check_aes_coders([&coder].into_iter(), &ArchiveLimits::unlimited()).unwrap();
        coder.properties_mut()[0] = crate::encryption::MAX_AES_CYCLES_POWER + 1;
        assert_eq!(
            check_aes_coders([&coder].into_iter(), &ArchiveLimits::unlimited())
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesCyclesPower)
        );
    }
}
