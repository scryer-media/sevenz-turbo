use std::io::Read;
use std::sync::Arc;

#[cfg(feature = "bzip2")]
use bzip2::read::BzDecoder;
#[cfg(feature = "deflate")]
use flate2::bufread::DeflateDecoder;
use lzma_turbo::LzmaReader;
#[cfg(feature = "ppmd")]
use ppmd_turbo::{Params as PpmdParams, SevenZDecoder as PpmdDecoder, io::SevenZReader};

#[cfg(feature = "brotli")]
use crate::codec::brotli::BrotliDecoder;
#[cfg(feature = "lz4")]
use crate::codec::lz4::Lz4Decoder;
use crate::codec::{
    filter::{bcj::BcjReader, delta::DeltaReader},
    lzma_turbo::{
        Lzma2Coder, Lzma2Control, Lzma2Plan, lzma2_clamped_prop, lzma2_decoder,
        lzma2_dictionary_size, lzma2_memory_usage_kb,
    },
};
use crate::container::ArchiveLimits;
#[cfg(feature = "aes256")]
use crate::encryption::Aes256Sha256Decoder;
use crate::{Password, archive::EncoderMethod, block::Coder, error::Error};

/// Bytes a decoder that reads its input in small pieces is given at a time.
///
/// PPMd's range decoder takes its input one byte per call, Brotli and the
/// BCJ filters a few kilobytes, and straight off a pack stream every one of
/// those calls is a read system call: a PPMd block of 15 MB took 15 million
/// of them, and spent as long in the kernel as decoding. 7-Zip's filter
/// coders read at least this much at a time. A `BufReader` of this size hands
/// a read at least as large as itself straight through, so a coder that
/// already reads in large pieces underneath one costs no second copy.
pub(crate) const INPUT_BUF_SIZE: usize = 64 << 10;

/// Everything a coder needs that is not in the archive: the caller's limits,
/// how many threads they will allow, and the live link to the LZMA2 coder.
///
/// It replaces the two loose parameters (`max_mem_limit_kb`, `threads`) that
/// upstream threads through the decode-stack builders. Bundling them is what
/// lets the LZMA2 coder be handed a control block as well without every
/// function between here and the reader growing a ninth argument.
#[derive(Clone, Copy)]
pub(crate) struct DecodeOptions<'a> {
    /// What the caller will let the archive allocate.
    pub(crate) limits: &'a ArchiveLimits,
    /// Thread ceiling for coders that can use one. One means inline.
    pub(crate) threads: u32,
    /// Build the LZMA2 coder so it can widen later even at one thread.
    pub(crate) adaptive_lzma2: bool,
    /// Whether the header's checksums are to be checked at all. False only
    /// when the caller has said it verifies the bytes by other means.
    pub(crate) verify_checksums: bool,
    /// The live link back to the reader, when there is one. The header decode
    /// has none: it is one small block on the calling thread, before the
    /// caller has had any chance to ask for anything else.
    pub(crate) lzma2_control: Option<&'a Arc<Lzma2Control>>,
    /// Where the consumer's boundaries fall in this block's decoded stream —
    /// the offsets its files start at — so that a parallel LZMA2 coder can
    /// checksum each piece in the worker that produced it. Empty when nothing
    /// is to be checksummed there.
    pub(crate) checksum_splits: &'a [u64],
    /// Whether the caller checks each of this block's files against its own
    /// CRC-32 as it reads it. A block holding one file whose only checksum is
    /// that file's would otherwise be checked twice over the same bytes: once
    /// by the block's verifying reader, with the CRC borrowed from the file,
    /// and again by the caller's. False wherever the block's reader is the
    /// only check there is.
    pub(crate) files_verified: bool,
    /// Decoder memory, in kilobytes, that the other coders of this chain have
    /// already been granted out of `limits.memory_limit_bytes`. Set from
    /// [`check_chain_memory`]; a coder that fits itself to what is left
    /// (Zstandard's window) subtracts it, so that the chain stays within the
    /// limit as a whole rather than each coder within it alone.
    #[cfg_attr(not(feature = "zstd"), allow(dead_code))]
    pub(crate) reserved_kb: usize,
}

impl<'a> DecodeOptions<'a> {
    /// The options the header decode runs with: the caller's limits, one
    /// thread, no live control.
    pub(crate) fn header(limits: &'a ArchiveLimits) -> Self {
        Self {
            limits,
            threads: 1,
            adaptive_lzma2: false,
            verify_checksums: true,
            lzma2_control: None,
            checksum_splits: &[],
            files_verified: false,
            reserved_kb: 0,
        }
    }

    /// The same options, with `reserved_kb` already granted to the chain's
    /// sized coders.
    pub(crate) fn reserving(self, reserved_kb: usize) -> Self {
        Self {
            reserved_kb,
            ..self
        }
    }

    /// Whether the checksums of this block are being computed by the LZMA2
    /// workers rather than by whoever consumes the output.
    ///
    /// True only once the coder has actually been built and engaged the
    /// parallel path: a plan can always degrade to the single-threaded
    /// decoder, which computes no checksums, so the answer is read from the
    /// live coder and not from what was asked for.
    pub(crate) fn folding_checksums(&self) -> bool {
        !self.checksum_splits.is_empty()
            && self
                .lzma2_control
                .is_some_and(|control| control.progress().is_some())
    }
}

pub enum Decoder<R: Read> {
    Copy(R),
    Lzma(Box<LzmaReader<R>>),
    Lzma2(Box<Lzma2Coder<R>>),
    #[cfg(feature = "ppmd")]
    Ppmd(Box<SevenZReader<std::io::BufReader<R>>>),
    Bcj(BcjReader<R>),
    Delta(DeltaReader<R>),
    #[cfg(feature = "brotli")]
    Brotli(Box<BrotliDecoder<R>>),
    #[cfg(feature = "bzip2")]
    Bzip2(BzDecoder<R>),
    #[cfg(feature = "deflate")]
    Deflate(DeflateDecoder<std::io::BufReader<R>>),
    #[cfg(feature = "lz4")]
    Lz4(Lz4Decoder<R>),
    #[cfg(feature = "zstd")]
    Zstd(zstd::Decoder<'static, std::io::BufReader<R>>),
    #[cfg(feature = "aes256")]
    Aes256Sha256(Box<Aes256Sha256Decoder<R>>),
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Decoder::Copy(r) => r.read(buf),
            Decoder::Lzma(r) => r.read(buf),
            Decoder::Lzma2(r) => r.read(buf),
            #[cfg(feature = "ppmd")]
            Decoder::Ppmd(r) => r.read(buf),
            Decoder::Bcj(r) => r.read(buf),
            Decoder::Delta(r) => r.read(buf),
            #[cfg(feature = "brotli")]
            Decoder::Brotli(r) => r.read(buf),
            #[cfg(feature = "bzip2")]
            Decoder::Bzip2(r) => r.read(buf),
            #[cfg(feature = "deflate")]
            Decoder::Deflate(r) => r.read(buf),
            #[cfg(feature = "lz4")]
            Decoder::Lz4(r) => r.read(buf),
            #[cfg(feature = "zstd")]
            Decoder::Zstd(r) => r.read(buf),
            #[cfg(feature = "aes256")]
            Decoder::Aes256Sha256(r) => r.read(buf),
        }
    }
}

/// Kilobytes of decoder state one coder's decoder holds, by the same model
/// [`add_decoder`] checks it against the limit with: the dictionary, clamped to
/// the coder's output the way the decoder clamps it, plus the LZ state for
/// LZMA and LZMA2; the declared model for PPMd.
///
/// `None` for every other coder: filters and the fixed-size codecs are not
/// sized against the limit, and Zstandard's window is fitted to whatever the
/// limit leaves (see [`DecodeOptions::reserved_kb`]). `None` too for
/// properties too short to read a size out of, so that [`add_decoder`] still
/// reports them in its own words when it reaches the coder.
fn sized_coder_memory_kb(coder: &Coder, uncompressed_len: usize) -> Option<usize> {
    let method_id = coder.encoder_method_id();
    if method_id == EncoderMethod::ID_LZMA {
        let dict_size = crate::codec::lzma_turbo::clamp_dictionary(
            crate::codec::lzma_turbo::lzma_dictionary_size(&coder.properties).ok()?,
            uncompressed_len as u64,
        );
        Some(lzma2_memory_usage_kb(dict_size))
    } else if method_id == EncoderMethod::ID_LZMA2 {
        let dict_prop = lzma2_clamped_prop(*coder.properties.first()?, uncompressed_len as u64);
        Some(lzma2_memory_usage_kb(
            lzma2_dictionary_size(&[dict_prop]).ok()?,
        ))
    } else if cfg!(feature = "ppmd") && method_id == EncoderMethod::ID_PPMD {
        let size = coder.properties.get(1..5)?;
        let memory_size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]);
        Some(memory_size.div_ceil(1024) as usize)
    } else {
        None
    }
}

/// Refuses a coder chain whose sized coders need more decoder memory
/// *together* than `limits.memory_limit_bytes`, before any of them is built.
///
/// [`add_decoder`] checks each coder alone, which bounds nothing about a chain
/// of them: a block can declare up to `max_coders_per_block` coders, each just
/// under the limit. `coders` is each coder the decode will build with the
/// output length it will be built for, exactly as [`add_decoder`] will be
/// handed them, so a chain that fits here fits there. The refusal is the same
/// [`Error::MaxMemLimited`] the per-coder check raises, with `actaul_kb` the
/// chain's total.
///
/// `base_kb` is what the chain's caller holds under it whatever its coders
/// are: a block's pack-stream buffer ([`INPUT_BUF_SIZE`]), or nothing.
///
/// Returns the kilobytes the chain's sized coders take, with `base_kb`, for
/// [`DecodeOptions::reserving`].
pub(crate) fn check_chain_memory<'c>(
    coders: impl IntoIterator<Item = (&'c Coder, u64)>,
    limits: &ArchiveLimits,
    base_kb: usize,
) -> Result<usize, Error> {
    let max_kb = limits.memory_limit_kb();
    let total_kb = coders
        .into_iter()
        .filter_map(|(coder, len)| sized_coder_memory_kb(coder, len as usize))
        .fold(base_kb, usize::saturating_add);
    if total_kb > max_kb {
        return Err(Error::MaxMemLimited {
            max_kb,
            actaul_kb: total_kb,
        });
    }
    Ok(total_kb)
}

pub fn add_decoder<I: Read>(
    input: I,
    uncompressed_len: usize,
    coder: &Coder,
    #[allow(unused)] password: &Password,
    opts: &DecodeOptions<'_>,
) -> Result<Decoder<I>, Error> {
    let max_mem_limit_kb = opts.limits.memory_limit_kb();
    let method = EncoderMethod::by_id(coder.encoder_method_id());
    let method = if let Some(m) = method {
        m
    } else {
        return Err(Error::UnsupportedCompressionMethod(format!(
            "{:?}",
            coder.encoder_method_id()
        )));
    };
    match method.id() {
        EncoderMethod::ID_COPY => Ok(Decoder::Copy(input)),
        EncoderMethod::ID_LZMA => {
            // Validate the length before touching the properties: the decoder
            // slices `[1..5]`, which would panic on an attacker-supplied short field.
            if coder.properties.len() < 5 {
                return Err(Error::Other("LZMA properties too short".into()));
            }
            // Clamp before the budget check, so a coder that declares a huge
            // dictionary for a small stream is decoded rather than refused.
            let dict_size = crate::codec::lzma_turbo::clamp_dictionary(
                crate::codec::lzma_turbo::lzma_dictionary_size(&coder.properties)?,
                uncompressed_len as u64,
            );
            let mem_size = lzma2_memory_usage_kb(dict_size);
            if mem_size > max_mem_limit_kb {
                return Err(Error::MaxMemLimited {
                    max_kb: max_mem_limit_kb,
                    actaul_kb: mem_size,
                });
            }
            let lz = crate::codec::lzma_turbo::lzma_decoder(
                input,
                uncompressed_len,
                &coder.properties,
                dict_size,
            )
            .map_err(|e| Error::bad_password(e, !password.is_empty()))?;
            Ok(Decoder::Lzma(Box::new(lz)))
        }
        EncoderMethod::ID_LZMA2 => {
            // The dictionary is a table index rather than a number here, so the
            // clamp comes back as the property byte the decoders are built from.
            let dict_prop = coder
                .properties
                .first()
                .copied()
                .ok_or_else(|| Error::other("LZMA2 properties too short"))?;
            let dict_prop = lzma2_clamped_prop(dict_prop, uncompressed_len as u64);
            let dic_size = lzma2_dictionary_size(&[dict_prop])?;
            let mem_size = lzma2_memory_usage_kb(dic_size);
            if mem_size > max_mem_limit_kb {
                return Err(Error::MaxMemLimited {
                    max_kb: max_mem_limit_kb,
                    actaul_kb: mem_size,
                });
            }

            let plan = match opts.lzma2_control {
                Some(control) => Lzma2Plan::for_block(
                    opts.threads,
                    opts.adaptive_lzma2,
                    opts.limits.memory_limit_bytes,
                    dic_size,
                    uncompressed_len as u64,
                    control,
                    opts.checksum_splits,
                ),
                None => Lzma2Plan::SingleThreaded,
            };
            let lz = lzma2_decoder(input, dict_prop, plan)
                .map_err(|e| Error::bad_password(e, !password.is_empty()))?;
            Ok(Decoder::Lzma2(Box::new(lz)))
        }
        #[cfg(feature = "ppmd")]
        EncoderMethod::ID_PPMD => {
            let params = get_ppmd_params(coder, max_mem_limit_kb)?;
            // Buffered here rather than at the bottom of the chain only, so a
            // PPMd coder anywhere - under AES, inside a BCJ2 graph, in the
            // header - reads its input in large pieces, and an AES coder
            // under it decrypts them in bulk instead of a block per call. The
            // step decoder reads straight out of this buffer.
            let input = std::io::BufReader::with_capacity(INPUT_BUF_SIZE, input);
            // 7-Zip writes no end marker: the coder's unpacked size ends the
            // stream. A stream that is corrupt or cut short fails its read as
            // an I/O error of kind `InvalidData` or `UnexpectedEof`, the same
            // classes the LZMA decoders report.
            let decoder = PpmdDecoder::new(params, Some(uncompressed_len as u64))
                .map_err(|err| Error::from(std::io::Error::from(err)))?;
            Ok(Decoder::Ppmd(Box::new(SevenZReader::from_decoder(
                input, decoder,
            ))))
        }
        #[cfg(feature = "brotli")]
        EncoderMethod::ID_BROTLI => {
            let de = BrotliDecoder::new(input, INPUT_BUF_SIZE)?;
            Ok(Decoder::Brotli(Box::new(de)))
        }
        #[cfg(feature = "bzip2")]
        EncoderMethod::ID_BZIP2 => {
            let de = BzDecoder::new(input);
            Ok(Decoder::Bzip2(de))
        }
        #[cfg(feature = "deflate")]
        EncoderMethod::ID_DEFLATE => {
            let buf_read = std::io::BufReader::new(input);
            let de = DeflateDecoder::new(buf_read);
            Ok(Decoder::Deflate(de))
        }
        #[cfg(feature = "lz4")]
        EncoderMethod::ID_LZ4 => {
            let de = Lz4Decoder::new(input)?;
            Ok(Decoder::Lz4(de))
        }
        #[cfg(feature = "zstd")]
        EncoderMethod::ID_ZSTD => {
            let mut zs = zstd::Decoder::new(input)?;
            // A zstd frame declares its own back-reference window, and the
            // decoder allocates it. The format allows windows far larger than
            // any 7z encoder writes, so the window is bounded here: by the
            // caller's memory limit when there is one, and otherwise by the
            // 128 MiB the reference decoder itself refuses to exceed. The
            // budget is what the chain's sized coders have left of the limit,
            // so a window beside an LZMA dictionary does not double it.
            const ZSTD_DEFAULT_WINDOW_LOG: u32 = 27;
            let window_log = if max_mem_limit_kb == usize::MAX {
                ZSTD_DEFAULT_WINDOW_LOG
            } else {
                let budget_kb = max_mem_limit_kb.saturating_sub(opts.reserved_kb);
                let bytes = (budget_kb as u64).saturating_mul(1024).max(1024);
                // The largest power of two that fits in the budget, never above
                // the default and never below the 1 KiB floor the format has.
                (63 - bytes.leading_zeros()).clamp(10, ZSTD_DEFAULT_WINDOW_LOG)
            };
            zs.window_log_max(window_log)?;
            Ok(Decoder::Zstd(zs))
        }
        EncoderMethod::ID_BCJ_X86 => {
            let de = BcjReader::new_x86(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_BCJ_ARM => {
            let de = BcjReader::new_arm(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_BCJ_ARM64 => {
            let de = BcjReader::new_arm64(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_BCJ_ARM_THUMB => {
            let de = BcjReader::new_arm_thumb(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_BCJ_PPC => {
            let de = BcjReader::new_ppc(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_BCJ_IA64 => {
            let de = BcjReader::new_ia64(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_BCJ_SPARC => {
            let de = BcjReader::new_sparc(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_BCJ_RISCV => {
            let de = BcjReader::new_riscv(input, 0);
            Ok(Decoder::Bcj(de))
        }
        EncoderMethod::ID_DELTA => {
            // The distance is `properties[0] + 1` in the range 1..=256. Widen to `usize`
            // before the `+1` so a property byte of `0xFF` yields 256, not 0 (a `u8`
            // `wrapping_add` would wrap to a zero distance and mis-decode / divide by zero).
            let d = coder.properties.first().map_or(1, |b| *b as usize + 1);
            let de = DeltaReader::new(input, d);
            Ok(Decoder::Delta(de))
        }
        #[cfg(feature = "aes256")]
        EncoderMethod::ID_AES256_SHA256 => {
            if password.is_empty() {
                return Err(Error::PasswordRequired);
            }
            let de = Aes256Sha256Decoder::new(
                input,
                &coder.properties,
                password,
                opts.limits.max_aes_cycles_power,
                opts.limits.max_aes_kdf_rounds,
            )?;
            Ok(Decoder::Aes256Sha256(Box::new(de)))
        }
        _ => Err(Error::UnsupportedCompressionMethod(
            method.name().to_string(),
        )),
    }
}

#[cfg(feature = "ppmd")]
fn get_ppmd_params(coder: &Coder, max_mem_limit_kb: usize) -> Result<PpmdParams, Error> {
    // 7-Zip reads the first five property bytes and ignores any after them.
    let props = coder
        .properties
        .get(..5)
        .ok_or_else(|| Error::other("PPMD properties too short"))?;
    let params = PpmdParams::from_7z_props(props).map_err(|_| {
        Error::other(format!(
            "PPMD order {} or memory size {} out of range",
            props[0],
            u32::from_le_bytes([props[1], props[2], props[3], props[4]])
        ))
    })?;

    // Checked before the decoder allocates its model.
    let memory_size_kb = params.mem_size().div_ceil(1024) as usize;
    if memory_size_kb > max_mem_limit_kb {
        return Err(Error::MaxMemLimited {
            max_kb: max_mem_limit_kb,
            actaul_kb: memory_size_kb,
        });
    }

    Ok(params)
}
