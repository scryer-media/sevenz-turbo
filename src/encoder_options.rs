use std::{fmt::Debug, num::NonZeroU64};

#[cfg(feature = "ppmd")]
use ppmd_rust::{PPMD7_MAX_MEM_SIZE, PPMD7_MAX_ORDER, PPMD7_MIN_MEM_SIZE, PPMD7_MIN_ORDER};

#[cfg(feature = "compress")]
use crate::EncoderConfiguration;
#[cfg(feature = "aes256")]
use crate::Password;

/// The LZMA settings both option types carry, independent of which encoder
/// runs them. `None` is "the level's default".
///
/// The levels are the table `lzma-rust2` uses, which is xz's: the dictionary
/// doubles from 256 KiB at level 0 to 64 MiB at level 9, levels 0 to 3 are
/// the fast parser over a hash chain and the rest the optimal parser over a
/// binary tree. Both encoders are given these numbers outright, so a level
/// means the same dictionary, and the same archive memory, whichever one the
/// build compiles - the SDK's own level defaults, which reach 256 MiB, are
/// not used.
#[cfg(feature = "compress")]
#[derive(Debug, Clone, Copy)]
pub(crate) struct LzmaSettings {
    level: u32,
    dict_size: Option<u32>,
    nice_len: Option<u32>,
    /// How many bytes the coder is about to be given, when the writer knows.
    /// The dictionary never needs to be larger, and a folder that fits one
    /// block needs no block threads. `None` is "unknown": the full dictionary
    /// and the configured threads. The writer sets it per folder; the public
    /// option types never do.
    input_size: Option<u64>,
}

#[cfg(feature = "compress")]
impl LzmaSettings {
    /// The smallest dictionary either encoder accepts.
    pub(crate) const DICT_SIZE_MIN: u32 = 4096;
    /// The largest dictionary the option setters allow; the encoder's own
    /// limit, checked when the coder is built, is lower.
    const DICT_SIZE_MAX: u32 = 4_294_967_280;
    const NICE_LEN_MIN: u32 = 8;
    const NICE_LEN_MAX: u32 = 273;

    /// Dictionary size by level.
    const LEVEL_DICT_SIZE: [u32; 10] = [
        1 << 18,
        1 << 20,
        1 << 21,
        1 << 22,
        1 << 22,
        1 << 23,
        1 << 23,
        1 << 24,
        1 << 25,
        1 << 26,
    ];
    /// Nice match length by level.
    const LEVEL_NICE_LEN: [u32; 10] = [128, 128, 273, 273, 16, 32, 64, 64, 64, 64];
    /// Hash-chain depth for the fast levels; the optimal levels leave it to
    /// the encoder.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    const LEVEL_DEPTH: [u32; 4] = [4, 8, 24, 48];

    const fn from_level(level: u32) -> Self {
        LzmaSettings {
            level: if level > 9 { 9 } else { level },
            dict_size: None,
            nice_len: None,
            input_size: None,
        }
    }

    #[cfg(feature = "lzma-rust2-encoder")]
    pub(crate) const fn level(&self) -> u32 {
        self.level
    }

    #[cfg(not(feature = "lzma-rust2-encoder"))]
    const fn fast(&self) -> bool {
        self.level <= 3
    }

    fn set_dict_size(&mut self, dict_size: u32) {
        self.dict_size = Some(dict_size.clamp(Self::DICT_SIZE_MIN, Self::DICT_SIZE_MAX));
    }

    fn set_nice_len(&mut self, nice_len: u32) {
        self.nice_len = Some(nice_len.clamp(Self::NICE_LEN_MIN, Self::NICE_LEN_MAX));
    }

    /// The dictionary size the caller asked for: theirs, or the level's.
    pub(crate) fn requested_dict_size(&self) -> u32 {
        self.dict_size
            .unwrap_or(Self::LEVEL_DICT_SIZE[self.level as usize])
    }

    /// The dictionary size the encoder will use: the requested one, shrunk
    /// to the input when the input is known to be smaller, but never below
    /// 4 KiB. This is `LzmaEncProps_Normalize`'s `reduceSize` rule, which
    /// `lzma-turbo` applies to the same numbers, so the size the folder's
    /// coder record names is the size the encoder ran with.
    pub(crate) fn dict_size(&self) -> u32 {
        let dict_size = self.requested_dict_size();
        match self.input_size {
            Some(size) if u64::from(dict_size) > size => {
                // `size` is below a `u32` here.
                dict_size.min((size as u32).max(Self::DICT_SIZE_MIN))
            }
            _ => dict_size,
        }
    }

    /// How many bytes the coder will be given, if the writer knows.
    pub(crate) const fn input_size(&self) -> Option<u64> {
        self.input_size
    }

    /// Records how many bytes the coder will be given. See `input_size`.
    pub(crate) fn set_input_size(&mut self, size: u64) {
        self.input_size = Some(size);
    }

    fn nice_len(&self) -> u32 {
        self.nice_len
            .unwrap_or(Self::LEVEL_NICE_LEN[self.level as usize])
    }

    /// The `lzma-turbo` setting.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    pub(crate) fn turbo_props(&self) -> lzma_turbo::LzmaEncProps {
        use lzma_turbo::MatchFinderKind;

        let fast = self.fast();
        let mut props = lzma_turbo::LzmaEncProps::new()
            .with_level(self.level)
            .with_dict_size(self.requested_dict_size())
            .with_fast_bytes(self.nice_len())
            .with_fast_mode(fast)
            .with_match_finder(if fast {
                MatchFinderKind::Hc4
            } else {
                MatchFinderKind::Bt4
            });
        if fast {
            props = props.with_match_cycles(Self::LEVEL_DEPTH[self.level as usize]);
        }
        // C: `props.reduceSize`. Only where it shrinks the dictionary, so an
        // input at least the dictionary's size is coded exactly as before.
        if let Some(size) = self.input_size
            && size < u64::from(self.requested_dict_size())
        {
            props = props.with_reduce_size(size);
        }
        props
    }

    /// The `lzma-rust2` setting: its preset for the level, which is this
    /// table, with the caller's overrides applied.
    #[cfg(feature = "lzma-rust2-encoder")]
    pub(crate) fn rust2_options(&self) -> lzma_rust2::LzmaOptions {
        let mut options = lzma_rust2::LzmaOptions::with_preset(self.level);
        options.dict_size = self.dict_size();
        options.nice_len = self.nice_len();
        options
    }

    /// The LZMA properties byte, `(pb * 5 + lp) * 9 + lc`. Neither option
    /// type exposes lc, lp or pb, so both encoders run the defaults of 3, 0
    /// and 2.
    pub(crate) const fn props_byte() -> u8 {
        const LC: u8 = 3;
        const LP: u8 = 0;
        const PB: u8 = 2;
        (PB * 5 + LP) * 9 + LC
    }
}

#[cfg(feature = "compress")]
#[derive(Debug, Clone)]
/// Options for LZMA compression.
pub struct LzmaOptions(pub(crate) LzmaSettings);

#[cfg(feature = "compress")]
impl Default for LzmaOptions {
    fn default() -> Self {
        Self(LzmaSettings::from_level(6))
    }
}

#[cfg(feature = "compress")]
impl LzmaOptions {
    /// Creates LZMA options with the specified compression level.
    ///
    /// # Arguments
    /// * `level` - Compression level (0-9, clamped to this range)
    pub fn from_level(level: u32) -> Self {
        Self(LzmaSettings::from_level(level))
    }

    /// Sets the dictionary size used when encoding.
    ///
    /// Will be clamped between 4096..=4294967280.
    ///
    /// Encoding returns an invalid-input error for dictionary sizes above 1073741823 bytes
    /// on 64-bit targets or 268435454 bytes on 32-bit targets.
    pub fn set_dictionary_size(&mut self, dict_size: u32) {
        self.0.set_dict_size(dict_size);
    }

    /// Sets the nice length of a match.
    ///
    /// Will be clamped between 8..=273.
    pub fn set_nice_len(&mut self, nice_len: u32) {
        self.0.set_nice_len(nice_len);
    }
}

#[cfg(feature = "compress")]
#[derive(Debug, Clone)]
/// Options for LZMA2 compression.
pub struct Lzma2Options {
    pub(crate) settings: LzmaSettings,
    pub(crate) threads: u32,
    /// How much input one independently compressed block covers when
    /// `threads` is above one; `None` is one solid stream.
    pub(crate) chunk_size: Option<NonZeroU64>,
}

#[cfg(feature = "compress")]
impl Default for Lzma2Options {
    fn default() -> Self {
        Self {
            settings: LzmaSettings::from_level(6),
            threads: 1,
            chunk_size: None,
        }
    }
}

#[cfg(feature = "compress")]
impl Lzma2Options {
    /// Creates LZMA2 options with the specified compression level.
    /// Encoded using a single thread.
    ///
    /// # Arguments
    /// * `level` - Compression level (0-9, clamped to this range)
    pub fn from_level(level: u32) -> Self {
        Self {
            settings: LzmaSettings::from_level(level),
            threads: 1,
            chunk_size: None,
        }
    }

    /// Creates LZMA2 options with the specified compression level.
    /// Encoded using a multi-threading.
    ///
    /// # Arguments
    /// * `level` - Compression level (0-9, clamped to this range)
    /// * `threads` - Count of threads used to compress the data
    /// * `chunk_size` - Size of each independent chunk of uncompressed data.
    ///   The more streams can be created, the more effective is
    ///   the multi threading, but the worse the compression ratio
    ///   will be (value will be clamped to have at least the size of the dictionary).
    pub fn from_level_mt(level: u32, threads: u32, chunk_size: u64) -> Self {
        Self {
            settings: LzmaSettings::from_level(level),
            threads,
            // Zero is "the dictionary's size", as it always was:
            // `block_size` raises anything smaller to the dictionary.
            chunk_size: NonZeroU64::new(chunk_size.max(1)),
        }
    }

    /// Sets the dictionary size used when encoding.
    ///
    /// Will be clamped between 4096..=4294967280.
    ///
    /// Encoding returns an invalid-input error for dictionary sizes above 1073741823 bytes
    /// on 64-bit targets or 268435454 bytes on 32-bit targets.
    pub fn set_dictionary_size(&mut self, dict_size: u32) {
        self.settings.set_dict_size(dict_size);
    }

    /// Sets the nice length of a match.
    ///
    /// Will be clamped between 8..=273.
    pub fn set_nice_len(&mut self, nice_len: u32) {
        self.settings.set_nice_len(nice_len);
    }

    /// The block size in force: the chunk size, but never below the
    /// dictionary, so that a block is never smaller than what its coder could
    /// look back over.
    pub(crate) fn block_size(&self) -> Option<u64> {
        self.chunk_size
            .map(|chunk| chunk.get().max(u64::from(self.settings.dict_size())))
    }
}

#[cfg(feature = "bzip2")]
#[derive(Debug, Copy, Clone)]
/// Options for BZIP2 compression.
pub struct Bzip2Options(pub(crate) u32);

#[cfg(feature = "bzip2")]
impl Bzip2Options {
    /// Creates BZIP2 options with the specified compression level.
    ///
    /// # Arguments
    /// * `level` - Compression level (typically 1-9)
    pub const fn from_level(level: u32) -> Self {
        Self(level)
    }
}

#[cfg(feature = "bzip2")]
impl Default for Bzip2Options {
    fn default() -> Self {
        Self(6)
    }
}

#[cfg(any(feature = "brotli", feature = "lz4"))]
const MINIMAL_SKIPPABLE_FRAME_SIZE: u32 = 64 * 1024;
#[cfg(feature = "brotli")]
const DEFAULT_SKIPPABLE_FRAME_SIZE: u32 = 128 * 1024;

#[cfg(feature = "brotli")]
#[derive(Debug, Copy, Clone)]
/// Options for Brotli compression.
pub struct BrotliOptions {
    pub(crate) quality: u32,
    pub(crate) window: u32,
    pub(crate) skippable_frame_size: u32,
}

#[cfg(feature = "brotli")]
impl BrotliOptions {
    /// Creates Brotli options with the specified quality and window size.
    ///
    /// # Arguments
    /// * `quality` - Compression quality (0-11, clamped to this range)
    /// * `window` - Window size (10-24, clamped to this range)
    pub const fn from_quality_window(quality: u32, window: u32) -> Self {
        let quality = if quality > 11 { 11 } else { quality };
        let window = if window > 24 { 24 } else { window };
        Self {
            quality,
            window,
            skippable_frame_size: DEFAULT_SKIPPABLE_FRAME_SIZE,
        }
    }

    /// Set's the skippable frame size. The size is defined as the size of uncompressed data a frame
    /// contains. A value of 0 deactivates skippable frames and uses the native brotli bitstream.
    /// If a value is set, then a similar skippable frame format used by LZ4 and ZSTD is used.
    ///
    /// Af value between 1..=64KiB will be set to 64KiB.
    ///
    /// This was first implemented by zstdmt. The default value is 128 KiB.
    pub fn with_skippable_frame_size(mut self, skippable_frame_size: u32) -> Self {
        if skippable_frame_size == 0 {
            self.skippable_frame_size = 0;
        } else {
            self.skippable_frame_size =
                u32::max(skippable_frame_size, MINIMAL_SKIPPABLE_FRAME_SIZE);
        }

        self
    }
}

#[cfg(feature = "brotli")]
impl Default for BrotliOptions {
    fn default() -> Self {
        Self {
            quality: 11,
            window: 22,
            skippable_frame_size: DEFAULT_SKIPPABLE_FRAME_SIZE,
        }
    }
}

#[cfg(feature = "compress")]
#[derive(Debug, Copy, Clone)]
/// Options for Delta filter compression.
pub struct DeltaOptions(pub(crate) u32);

#[cfg(feature = "compress")]
impl DeltaOptions {
    /// Creates Delta options with the specified distance.
    ///
    /// # Arguments
    /// * `distance` - Delta distance (1-256, clamped to this range, 0 becomes 1)
    pub const fn from_distance(distance: u32) -> Self {
        let distance = if distance == 0 {
            1
        } else if distance > 256 {
            256
        } else {
            distance
        };
        Self(distance)
    }
}

#[cfg(feature = "compress")]
impl Default for DeltaOptions {
    fn default() -> Self {
        Self(1)
    }
}

#[cfg(feature = "deflate")]
#[derive(Debug, Copy, Clone)]
/// Options for Deflate compression.
pub struct DeflateOptions(pub(crate) u32);

#[cfg(feature = "deflate")]
impl DeflateOptions {
    /// Creates Deflate options with the specified compression level.
    ///
    /// # Arguments
    /// * `level` - Compression level (0-9, clamped to this range)
    pub const fn from_level(level: u32) -> Self {
        let level = if level > 9 { 9 } else { level };
        Self(level)
    }
}

#[cfg(feature = "deflate")]
impl Default for DeflateOptions {
    fn default() -> Self {
        Self(6)
    }
}

#[cfg(feature = "lz4")]
#[derive(Debug, Copy, Clone, Default)]
/// Options for LZ4 compression.
pub struct Lz4Options {
    pub(crate) skippable_frame_size: u32,
}

#[cfg(feature = "lz4")]
impl Lz4Options {
    /// Set's the skippable frame size. The size is defined as the size of uncompressed data a frame
    /// contains. A value of 0 deactivates skippable frames and uses the native LZ4 bitstream.
    /// If a value is set, then the similar skippable frame format is used.
    ///
    /// Af value between 1..=64KiB will be set to 64KiB.
    ///
    /// This was first implemented by zstdmt.
    ///
    /// Defaults to not use the skippable frame format at all, since LZ4 is extremely fast and will
    /// most likely saturate IO even on a single thread.
    pub fn with_skippable_frame_size(mut self, skippable_frame_size: u32) -> Self {
        if skippable_frame_size == 0 {
            self.skippable_frame_size = 0;
        } else {
            self.skippable_frame_size =
                u32::max(skippable_frame_size, MINIMAL_SKIPPABLE_FRAME_SIZE);
        }

        self
    }
}

#[cfg(feature = "ppmd")]
#[derive(Debug, Copy, Clone)]
/// Options for PPMD compression.
pub struct PpmdOptions {
    pub(crate) order: u32,
    pub(crate) memory_size: u32,
}

#[cfg(feature = "ppmd")]
impl PpmdOptions {
    /// Creates PPMD options with the specified compression level.
    ///
    /// # Arguments
    /// * `level` - Compression level (0-9, clamped to this range)
    pub const fn from_level(level: u32) -> Self {
        const ORDERS: [u32; 10] = [3, 4, 4, 5, 5, 6, 8, 16, 24, 32];

        let level = if level > 9 { 9 } else { level };
        let order = ORDERS[level as usize];
        let memory_size = 1 << (level + 19);

        Self { order, memory_size }
    }

    /// Creates PPMD options with specific order and memory size parameters.
    ///
    /// # Arguments
    /// * `order` - Model order (clamped to valid PPMD range)
    /// * `memory_size` - Memory size in bytes (clamped to valid PPMD range)
    pub const fn from_order_memory_size(order: u32, memory_size: u32) -> Self {
        let order = if order > PPMD7_MAX_ORDER {
            PPMD7_MAX_ORDER
        } else if order < PPMD7_MIN_ORDER {
            PPMD7_MIN_ORDER
        } else {
            order
        };
        let memory_size = if memory_size > PPMD7_MAX_MEM_SIZE {
            PPMD7_MAX_MEM_SIZE
        } else if memory_size < PPMD7_MIN_MEM_SIZE {
            PPMD7_MIN_MEM_SIZE
        } else {
            memory_size
        };
        Self { order, memory_size }
    }
}

#[cfg(feature = "ppmd")]
impl Default for PpmdOptions {
    fn default() -> Self {
        Self::from_level(6)
    }
}

#[cfg(feature = "zstd")]
#[derive(Debug, Copy, Clone)]
/// Options for Zstandard compression.
pub struct ZstandardOptions(pub(crate) u32);

#[cfg(feature = "zstd")]
impl ZstandardOptions {
    /// Creates Zstandard options with the specified compression level.
    ///
    /// # Arguments
    /// * `level` - Compression level (typically 1-22)
    pub const fn from_level(level: u32) -> Self {
        let level = if level > 22 { 22 } else { level };
        Self(level)
    }
}

#[cfg(feature = "zstd")]
impl Default for ZstandardOptions {
    fn default() -> Self {
        Self(3)
    }
}

#[cfg(feature = "aes256")]
#[derive(Debug, Clone)]
/// Options for AES256 encryption.
pub struct AesEncoderOptions {
    /// Password for encryption.
    pub password: Password,
    /// Initialization vector for encryption.
    pub iv: [u8; 16],
    /// Salt for key derivation.
    pub salt: [u8; 16],
    /// Number of cycles power for key derivation.
    pub num_cycles_power: u8,
}

#[cfg(feature = "aes256")]
impl AesEncoderOptions {
    /// Creates new AES encoder options with the specified password.
    ///
    /// Generates random IV and salt values automatically.
    ///
    /// # Arguments
    /// * `password` - Password for encryption
    pub fn new(password: Password) -> Self {
        let mut iv = [0; 16];
        getrandom::fill(&mut iv).expect("Can't generate IV");

        let mut salt = [0; 16];
        getrandom::fill(&mut salt).expect("Can't generate salt");

        Self {
            password,
            iv,
            salt,
            num_cycles_power: 8,
        }
    }

    pub(crate) fn properties(&self) -> [u8; 34] {
        let mut props = [0u8; 34];
        self.write_properties(&mut props);
        props
    }

    #[inline]
    pub(crate) fn write_properties(&self, props: &mut [u8]) {
        assert!(props.len() >= 34);
        props[0] = (self.num_cycles_power & 0x3F) | 0xC0;
        props[1] = 0xFF;
        props[2..18].copy_from_slice(&self.salt);
        props[18..34].copy_from_slice(&self.iv);
    }
}

/// Encoder-specific options for various compression and encryption methods.
#[derive(Debug, Clone)]
pub enum EncoderOptions {
    #[cfg(feature = "compress")]
    /// Delta filter options.
    Delta(DeltaOptions),
    #[cfg(feature = "compress")]
    /// LZMA compression options.
    Lzma(LzmaOptions),
    #[cfg(feature = "compress")]
    /// LZMA2 compression options.
    Lzma2(Lzma2Options),
    #[cfg(feature = "brotli")]
    /// Brotli compression options.
    Brotli(BrotliOptions),
    #[cfg(feature = "bzip2")]
    /// BZIP2 compression options.
    Bzip2(Bzip2Options),
    #[cfg(feature = "deflate")]
    /// Deflate compression options.
    Deflate(DeflateOptions),
    #[cfg(feature = "lz4")]
    /// LZ4 compression options.
    Lz4(Lz4Options),
    #[cfg(feature = "ppmd")]
    /// PPMD compression options.
    Ppmd(PpmdOptions),
    #[cfg(feature = "zstd")]
    /// Zstandard compression options.
    Zstd(ZstandardOptions),
    #[cfg(feature = "aes256")]
    /// AES256 encryption options.
    Aes(AesEncoderOptions),
}

#[cfg(feature = "aes256")]
impl From<AesEncoderOptions> for EncoderOptions {
    fn from(value: AesEncoderOptions) -> Self {
        Self::Aes(value)
    }
}

#[cfg(all(feature = "aes256", feature = "compress"))]
impl From<AesEncoderOptions> for EncoderConfiguration {
    fn from(value: AesEncoderOptions) -> Self {
        Self::new(crate::EncoderMethod::AES256_SHA256).with_options(EncoderOptions::Aes(value))
    }
}

#[cfg(feature = "compress")]
impl From<DeltaOptions> for EncoderConfiguration {
    fn from(options: DeltaOptions) -> Self {
        Self::new(crate::EncoderMethod::DELTA_FILTER).with_options(EncoderOptions::Delta(options))
    }
}

#[cfg(feature = "compress")]
impl From<LzmaOptions> for EncoderConfiguration {
    fn from(options: LzmaOptions) -> Self {
        Self::new(crate::EncoderMethod::LZMA).with_options(EncoderOptions::Lzma(options))
    }
}

#[cfg(feature = "compress")]
impl From<Lzma2Options> for EncoderConfiguration {
    fn from(options: Lzma2Options) -> Self {
        Self::new(crate::EncoderMethod::LZMA2).with_options(EncoderOptions::Lzma2(options))
    }
}

#[cfg(feature = "bzip2")]
impl From<Bzip2Options> for EncoderConfiguration {
    fn from(options: Bzip2Options) -> Self {
        Self::new(crate::EncoderMethod::BZIP2).with_options(EncoderOptions::Bzip2(options))
    }
}

#[cfg(feature = "brotli")]
impl From<BrotliOptions> for EncoderConfiguration {
    fn from(options: BrotliOptions) -> Self {
        Self::new(crate::EncoderMethod::BROTLI).with_options(EncoderOptions::Brotli(options))
    }
}

#[cfg(feature = "deflate")]
impl From<DeflateOptions> for EncoderConfiguration {
    fn from(options: DeflateOptions) -> Self {
        Self::new(crate::EncoderMethod::DEFLATE).with_options(EncoderOptions::Deflate(options))
    }
}

#[cfg(feature = "lz4")]
impl From<Lz4Options> for EncoderConfiguration {
    fn from(options: Lz4Options) -> Self {
        Self::new(crate::EncoderMethod::LZ4).with_options(EncoderOptions::Lz4(options))
    }
}

#[cfg(feature = "ppmd")]
impl From<PpmdOptions> for EncoderConfiguration {
    fn from(options: PpmdOptions) -> Self {
        Self::new(crate::EncoderMethod::PPMD).with_options(EncoderOptions::Ppmd(options))
    }
}

#[cfg(feature = "zstd")]
impl From<ZstandardOptions> for EncoderConfiguration {
    fn from(options: ZstandardOptions) -> Self {
        Self::new(crate::EncoderMethod::ZSTD).with_options(EncoderOptions::Zstd(options))
    }
}

#[cfg(feature = "compress")]
impl From<DeltaOptions> for EncoderOptions {
    fn from(o: DeltaOptions) -> Self {
        Self::Delta(o)
    }
}

#[cfg(feature = "compress")]
impl From<Lzma2Options> for EncoderOptions {
    fn from(o: Lzma2Options) -> Self {
        Self::Lzma2(o)
    }
}

#[cfg(feature = "bzip2")]
impl From<Bzip2Options> for EncoderOptions {
    fn from(o: Bzip2Options) -> Self {
        Self::Bzip2(o)
    }
}

#[cfg(feature = "brotli")]
impl From<BrotliOptions> for EncoderOptions {
    fn from(o: BrotliOptions) -> Self {
        Self::Brotli(o)
    }
}

#[cfg(feature = "deflate")]
impl From<DeflateOptions> for EncoderOptions {
    fn from(o: DeflateOptions) -> Self {
        Self::Deflate(o)
    }
}

#[cfg(feature = "lz4")]
impl From<Lz4Options> for EncoderOptions {
    fn from(o: Lz4Options) -> Self {
        Self::Lz4(o)
    }
}

#[cfg(feature = "ppmd")]
impl From<PpmdOptions> for EncoderOptions {
    fn from(o: PpmdOptions) -> Self {
        Self::Ppmd(o)
    }
}

#[cfg(feature = "zstd")]
impl From<ZstandardOptions> for EncoderOptions {
    fn from(o: ZstandardOptions) -> Self {
        Self::Zstd(o)
    }
}

impl EncoderOptions {
    /// Gets the LZMA & LZMA2 dictionary size for this encoder option.
    ///
    /// Returns the dictionary size if this is an LZMA & LZMA2 option, or a default value otherwise.
    pub fn get_lzma_dict_size(&self) -> u32 {
        match self {
            #[cfg(feature = "compress")]
            EncoderOptions::Lzma(o) => o.0.dict_size(),
            #[cfg(feature = "compress")]
            EncoderOptions::Lzma2(o) => o.settings.dict_size(),
            #[allow(unused)]
            _ => 0,
        }
    }
}

impl EncoderConfiguration {
    /// This coder told that its input is `size` bytes; `None` when the coder
    /// is not LZMA or LZMA2, which have no use for it.
    ///
    /// 7-Zip does the same per folder through `reduceSize`: a dictionary
    /// larger than the data costs its whole match-finder allocation and hash
    /// initialisation and can never be reached into, so a small folder is
    /// coded, and its coder record written, with a dictionary its own size.
    /// A folder that fits one LZMA2 block is coded on one thread rather than
    /// through the block-parallel coder, which would start a thread pool and
    /// buffer the whole block to code it in the same single block. An input
    /// at least the dictionary's size keeps the configured dictionary, and so
    /// codes to the same bytes as without the size. An unset option is the
    /// default the coder would have been built with.
    pub(crate) fn sized_for(&self, size: u64) -> Option<EncoderConfiguration> {
        let settings = match (self.method.id(), &self.options) {
            (crate::EncoderMethod::ID_LZMA, Some(EncoderOptions::Lzma(o))) => o.0,
            (crate::EncoderMethod::ID_LZMA, _) => LzmaOptions::default().0,
            (crate::EncoderMethod::ID_LZMA2, Some(EncoderOptions::Lzma2(o))) => o.settings,
            (crate::EncoderMethod::ID_LZMA2, _) => Lzma2Options::default().settings,
            _ => return None,
        };
        let mut settings = settings;
        settings.set_input_size(size);
        let options = match (self.method.id(), &self.options) {
            (crate::EncoderMethod::ID_LZMA, _) => EncoderOptions::Lzma(LzmaOptions(settings)),
            (_, Some(EncoderOptions::Lzma2(o))) => EncoderOptions::Lzma2(Lzma2Options {
                settings,
                ..o.clone()
            }),
            _ => EncoderOptions::Lzma2(Lzma2Options {
                settings,
                ..Lzma2Options::default()
            }),
        };
        Some(EncoderConfiguration {
            method: self.method,
            options: Some(options),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EncoderMethod;

    fn dict(config: &EncoderConfiguration) -> u32 {
        config
            .options
            .as_ref()
            .expect("options")
            .get_lzma_dict_size()
    }

    #[test]
    fn a_folder_smaller_than_the_dictionary_gets_one_its_size() {
        let config: EncoderConfiguration = Lzma2Options::from_level(5).into();
        let sized = config.sized_for(100_000).expect("shrunk");
        assert_eq!(dict(&sized), 100_000);
        assert_eq!(sized.method, EncoderMethod::LZMA2);
        // The settings the encoder is built with agree with the coder record.
        #[cfg(not(feature = "lzma-rust2-encoder"))]
        match &sized.options {
            Some(EncoderOptions::Lzma2(o)) => {
                assert_eq!(o.settings.turbo_props().dict_size(), 100_000);
            }
            other => panic!("not LZMA2 options: {other:?}"),
        }
    }

    #[test]
    fn the_dictionary_never_drops_below_4_kib() {
        let config: EncoderConfiguration = LzmaOptions::from_level(9).into();
        for size in [0, 1, 4095, 4096] {
            let sized = config.sized_for(size).expect("shrunk");
            assert_eq!(dict(&sized), 4096, "size {size}");
            assert_eq!(sized.method, EncoderMethod::LZMA);
        }
    }

    #[test]
    fn a_folder_no_smaller_than_the_dictionary_keeps_it() {
        let mut options = Lzma2Options::from_level(5);
        options.set_dictionary_size(1 << 16);
        let config: EncoderConfiguration = options.clone().into();
        for size in [1 << 16, (1 << 16) + 1, u64::MAX] {
            let sized = config.sized_for(size).expect("LZMA2");
            assert_eq!(dict(&sized), 1 << 16, "size {size}");
            // Not even the encoder's settings move: the same bytes come out.
            #[cfg(not(feature = "lzma-rust2-encoder"))]
            match &sized.options {
                Some(EncoderOptions::Lzma2(o)) => {
                    assert_eq!(o.settings.turbo_props(), options.settings.turbo_props());
                }
                other => panic!("not LZMA2 options: {other:?}"),
            }
        }
        assert_eq!(
            dict(&config.sized_for((1 << 16) - 1).unwrap()),
            (1 << 16) - 1
        );
    }

    #[test]
    fn an_unset_option_is_shrunk_from_the_default() {
        for method in [EncoderMethod::LZMA, EncoderMethod::LZMA2] {
            let sized = EncoderConfiguration::new(method)
                .sized_for(5000)
                .expect("shrunk");
            assert_eq!(dict(&sized), 5000);
        }
    }

    #[test]
    fn lzma2_threads_and_chunk_survive_the_shrink() {
        let config: EncoderConfiguration = Lzma2Options::from_level_mt(5, 4, 1 << 22).into();
        let sized = config.sized_for(1 << 20).expect("shrunk");
        match sized.options {
            Some(EncoderOptions::Lzma2(o)) => {
                assert_eq!(o.threads, 4);
                assert_eq!(o.chunk_size.map(NonZeroU64::get), Some(1 << 22));
                assert_eq!(o.settings.dict_size(), 1 << 20);
            }
            other => panic!("not LZMA2 options: {other:?}"),
        }
    }

    #[test]
    fn other_coders_are_left_alone() {
        assert!(
            EncoderConfiguration::new(EncoderMethod::COPY)
                .sized_for(1)
                .is_none()
        );
        let delta: EncoderConfiguration = DeltaOptions::from_distance(4).into();
        assert!(delta.sized_for(1).is_none());
    }
}
