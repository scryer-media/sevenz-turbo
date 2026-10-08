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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// Largest decoder footprint the caller will allow, in bytes.
    ///
    /// Default: no limit. Bounds [`Archive::decoder_memory_estimate`], which
    /// is dominated by the dictionary a block declares, and, when a header or
    /// block is decoded, the sum of its coder chain's dictionaries and models
    /// rather than each coder alone.
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
    /// built.
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
}

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
        }
    }
}

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
        }
    }

    /// The decoder-memory limit in the kilobytes the internal decoder uses,
    /// saturating rather than wrapping on an unlimited budget.
    pub(crate) fn memory_limit_kb(&self) -> usize {
        usize::try_from(self.memory_limit_bytes.div_ceil(KIB)).unwrap_or(usize::MAX)
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
/// A bzip2 block is at most 900 KiB, and the decoder holds a few times that.
const BZIP2_BYTES: u64 = 8 * MIB;
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
            .properties
            .first()
            .ok_or_else(|| crate::Error::other("AES256 properties too short"))?
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
    /// | Copy | 0 |
    /// | LZMA, LZMA2 | declared dictionary + 1 MiB |
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
    /// and this estimate would not describe it.
    ///
    /// # Errors
    ///
    /// [`UnsizedCoder`] for the first coder with no model, including one whose
    /// properties are too short to read a size out of.
    pub fn decoder_memory_estimate(&self) -> Result<u64, UnsizedCoder> {
        let mut largest_block = 0u64;
        for block in &self.blocks {
            let mut chain = BLOCK_INPUT_BYTES;
            for coder in &block.coders {
                chain = chain.saturating_add(coder_memory_estimate(coder)?);
            }
            largest_block = largest_block.max(chain);
        }
        Ok(largest_block)
    }
}

/// Bytes one coder's decoder needs.
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
                .then(|| info.crcs.get(index).map(|crc| *crc as u32))
                .flatten(),
        })
    }

    /// Every sub-stream of one block, in order.
    ///
    /// Empty when `block_index` is out of range, or when the archive carries
    /// no `SubStreamsInfo` at all (an archive of one entry per block need not).
    #[must_use]
    pub fn block_sub_streams(&self, block_index: usize) -> Vec<SubStream> {
        let Some(block) = self.blocks.get(block_index) else {
            return Vec::new();
        };
        let Some(&first) = self
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
        let count = block.packed_streams.len().max(1);
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
        coder.id_size = 4;
        coder
            .decompression_method_id_mut()
            .copy_from_slice(EncoderMethod::ID_AES256_SHA256);
        coder.properties = vec![63, 0];
        check_aes_coders([&coder].into_iter(), &ArchiveLimits::unlimited()).unwrap();
        coder.properties[0] = crate::encryption::MAX_AES_CYCLES_POWER + 1;
        assert_eq!(
            check_aes_coders([&coder].into_iter(), &ArchiveLimits::unlimited())
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesCyclesPower)
        );
    }
}
