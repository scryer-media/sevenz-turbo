use std::{cell::RefCell, io::Write, rc::Rc};

#[cfg(feature = "lzma-rust2-encoder")]
use lzma_rust2::{Lzma2Writer, Lzma2WriterMt, LzmaWriter};

#[cfg(not(feature = "lzma-rust2-encoder"))]
use crate::codec::lzma_turbo::writer::{Coder, LzmaTurboWriter, PullCoder, SideCoder};

use crate::codec::filter::{
    bcj::BcjWriter,
    bcj2::{Bcj2Finished, Bcj2Writer},
    delta::DeltaWriter,
};

#[cfg(feature = "brotli")]
use crate::codec::brotli::BrotliEncoder;
#[cfg(feature = "lz4")]
use crate::codec::lz4::Lz4Encoder;
#[cfg(feature = "brotli")]
use crate::encoder_options::BrotliOptions;
#[cfg(feature = "bzip2")]
use crate::encoder_options::Bzip2Options;
#[cfg(feature = "deflate")]
use crate::encoder_options::DeflateOptions;
#[cfg(feature = "lz4")]
use crate::encoder_options::Lz4Options;
#[cfg(feature = "ppmd")]
use crate::encoder_options::PpmdOptions;
#[cfg(feature = "zstd")]
use crate::encoder_options::ZstandardOptions;
#[cfg(feature = "aes256")]
use crate::encryption::Aes256Sha256Encoder;
use crate::{
    Error,
    archive::{EncoderConfiguration, EncoderMethod},
    encoder_options::{DeltaOptions, EncoderOptions, Lzma2Options, LzmaOptions, LzmaSettings},
    writer::CountingWriter,
};

/// A BCJ2 coder in a chain: its main stream goes on down the chain, and its
/// call and jump streams into coders of their own.
pub(crate) type Bcj2ChainWriter<W> = Bcj2Writer<CountingWriter<W>, Box<dyn Write>>;

// One of these is built per coder in a chain and boxed there as a
// `dyn Write`; its variants range from a counting writer to a brotli state
// of several KiB, and the difference costs nothing.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Encoder<W: Write> {
    Copy(CountingWriter<W>),
    Bcj(Option<BcjWriter<CountingWriter<W>>>),
    Bcj2(Option<Box<Bcj2ChainWriter<W>>>, Bcj2Side),
    Delta(DeltaWriter<CountingWriter<W>>),
    // LZMA and LZMA2 are `lzma-turbo`'s encoders unless the build asked for
    // `lzma-rust2`'s; see `Cargo.toml`. Both fronts have the same shape here.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    Lzma(Option<LzmaTurboWriter<CountingWriter<W>>>),
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    Lzma2(Option<LzmaTurboWriter<CountingWriter<W>>>),
    #[cfg(feature = "lzma-rust2-encoder")]
    Lzma(Option<LzmaWriter<CountingWriter<W>>>),
    #[cfg(feature = "lzma-rust2-encoder")]
    Lzma2(Option<Lzma2Writer<CountingWriter<W>>>),
    #[cfg(feature = "lzma-rust2-encoder")]
    Lzma2Mt(Option<Lzma2WriterMt<CountingWriter<W>>>),
    #[cfg(feature = "ppmd")]
    Ppmd(Option<Box<ppmd_turbo::io::SevenZWriter<CountingWriter<W>>>>),
    #[cfg(feature = "brotli")]
    Brotli(BrotliEncoder<CountingWriter<W>>),
    #[cfg(feature = "bzip2")]
    Bzip2(Option<bzip2::write::BzEncoder<CountingWriter<W>>>),
    #[cfg(feature = "deflate")]
    Deflate(Option<flate2::write::DeflateEncoder<CountingWriter<W>>>),
    #[cfg(feature = "lz4")]
    Lz4(Option<Lz4Encoder<CountingWriter<W>>>),
    #[cfg(feature = "zstd")]
    Zstd(Option<zstd::Encoder<'static, CountingWriter<W>>>),
    #[cfg(feature = "aes256")]
    Aes(Aes256Sha256Encoder<CountingWriter<W>>),
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Some encoder need to finish the encoding process. Because of lifetime limitations on
        // dynamic dispatch, we need to implement an implicit contract, where empty writes with
        // "&[]" trigger the call to "finish()". We need to also make sure to propagate the empty
        // write into the inner writer, so that the whole chain of encoders can properly finish
        // their data stream. Not a great way to do it, but I couldn't get a proper dynamic
        // dispatch based approach to work.
        match self {
            Encoder::Copy(w) => w.write(buf),
            Encoder::Delta(w) => w.write(buf),
            Encoder::Bcj(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    inner.write(buf)?;
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            Encoder::Bcj2(w, side) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let Bcj2Finished {
                        mut main,
                        mut call,
                        mut jump,
                        rc,
                        call_len,
                        jump_len,
                    } = writer.finish()?;
                    // Finish the call and jump coders, whose output lands in
                    // the buffers `side` shares with them, before the main
                    // chain: the archive writer appends these streams after
                    // the main one, once the chain has unwound.
                    call.write(&[])?;
                    jump.write(&[])?;
                    drop((call, jump));
                    *side.tail.borrow_mut() = Some(Bcj2Tail {
                        rc,
                        call: side.call.take(),
                        jump: side.jump.take(),
                        call_size: call_len,
                        jump_size: jump_len,
                    });
                    main.write(buf)?;
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            Encoder::Lzma(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            Encoder::Lzma2(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            #[cfg(feature = "lzma-rust2-encoder")]
            Encoder::Lzma2Mt(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            #[cfg(feature = "ppmd")]
            Encoder::Ppmd(w) => match buf.is_empty() {
                true => {
                    // 7-Zip writes no end marker in a `.7z`; the writer's
                    // finish is the coder's flush, written once.
                    let mut writer = w.take().unwrap();
                    writer.finish()?;
                    let mut inner = writer.into_inner();
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            // TODO: Also add a proper "finish" method here.
            #[cfg(feature = "brotli")]
            Encoder::Brotli(w) => w.write(buf),
            #[cfg(feature = "bzip2")]
            Encoder::Bzip2(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            #[cfg(feature = "deflate")]
            Encoder::Deflate(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            #[cfg(feature = "lz4")]
            Encoder::Lz4(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            #[cfg(feature = "zstd")]
            Encoder::Zstd(w) => match buf.is_empty() {
                true => {
                    let writer = w.take().unwrap();
                    let mut inner = writer.finish()?;
                    let _ = inner.write(buf);
                    Ok(0)
                }
                false => w.as_mut().unwrap().write(buf),
            },
            #[cfg(feature = "aes256")]
            Encoder::Aes(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Encoder::Copy(w) => w.flush(),
            Encoder::Bcj(w) => w.as_mut().unwrap().flush(),
            Encoder::Bcj2(w, _) => w.as_mut().unwrap().flush(),
            Encoder::Delta(w) => w.flush(),
            Encoder::Lzma(w) => w.as_mut().unwrap().flush(),
            Encoder::Lzma2(w) => w.as_mut().unwrap().flush(),
            #[cfg(feature = "lzma-rust2-encoder")]
            Encoder::Lzma2Mt(w) => w.as_mut().unwrap().flush(),
            #[cfg(feature = "brotli")]
            Encoder::Brotli(w) => w.flush(),
            // Only passes down to the sink: the writer's flush never ends the
            // range coder, so the stream's tail is written once, by finish,
            // and `7zz t` accepts it.
            #[cfg(feature = "ppmd")]
            Encoder::Ppmd(w) => w.as_mut().unwrap().flush(),
            #[cfg(feature = "bzip2")]
            Encoder::Bzip2(w) => w.as_mut().unwrap().flush(),
            #[cfg(feature = "deflate")]
            Encoder::Deflate(w) => w.as_mut().unwrap().flush(),
            #[cfg(feature = "lz4")]
            Encoder::Lz4(w) => w.as_mut().unwrap().flush(),
            #[cfg(feature = "zstd")]
            Encoder::Zstd(w) => w.as_mut().unwrap().flush(),
            #[cfg(feature = "aes256")]
            Encoder::Aes(w) => w.flush(),
        }
    }
}

/// A byte buffer that outlives the writer it is handed to: the call and jump
/// coders of a BCJ2 block write into one each, and the block's tail is taken
/// out of them once those coders are finished.
#[derive(Clone, Default)]
pub(crate) struct SharedBuf(Rc<RefCell<Vec<u8>>>);

impl SharedBuf {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a BCJ2 block produces besides its main stream, available once the
/// coder chain has been finished.
#[derive(Debug, Default)]
pub(crate) struct Bcj2Tail {
    /// The range-coded stream, which 7-Zip stores without a coder.
    pub(crate) rc: Vec<u8>,
    /// The call stream, LZMA-coded.
    pub(crate) call: Vec<u8>,
    /// The jump stream, LZMA-coded.
    pub(crate) jump: Vec<u8>,
    /// The call stream's size before its coder.
    pub(crate) call_size: u64,
    /// The jump stream's size before its coder.
    pub(crate) jump_size: u64,
}

/// Where a BCJ2 coder leaves its [`Bcj2Tail`] when it is finished.
pub(crate) type Bcj2Slot = Rc<RefCell<Option<Bcj2Tail>>>;

/// The handles a BCJ2 coder in the chain keeps on the buffers its call and
/// jump coders write into, and on the slot its tail goes to.
pub(crate) struct Bcj2Side {
    call: SharedBuf,
    jump: SharedBuf,
    tail: Bcj2Slot,
}

/// The dictionary 7-Zip gives the call and jump streams' LZMA coders.
///
/// `AddBcj2Methods` in 7-Zip's `CPP/7zip/Archive/7z/7zUpdate.cpp` sets
/// `kDictionarySize = 1 << 20`, `kNumFastBytes = 128`, `kNumThreads = 1`,
/// `kLitPosBits = 2` and `kLitContextBits = 0` (pb stays at its 2): the two
/// streams are four-byte big-endian addresses, which a byte of literal
/// context does not predict and their position in the word does.
pub(crate) const BCJ2_SIDE_DICT_SIZE: u32 = 1 << 20;
const BCJ2_SIDE_FAST_BYTES: u32 = 128;
const BCJ2_SIDE_LC: u8 = 0;
const BCJ2_SIDE_LP: u8 = 2;
const BCJ2_SIDE_PB: u8 = 2;

/// The LZMA property bytes of the call and jump coders: `(pb * 5 + lp) * 9 +
/// lc`, then the dictionary.
pub(crate) fn bcj2_side_properties() -> [u8; 5] {
    let mut props = [0u8; 5];
    props[0] = (BCJ2_SIDE_PB * 5 + BCJ2_SIDE_LP) * 9 + BCJ2_SIDE_LC;
    props[1..].copy_from_slice(&BCJ2_SIDE_DICT_SIZE.to_le_bytes());
    props
}

/// Where the LZMA coders of a BCJ2 chain's call and jump streams run.
///
/// The streams are the same bytes either way: the choice is only whether the
/// two coders overlap the main stream's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bcj2Sides {
    /// Each on a thread of its own, beside the main stream's coder: what a
    /// folder coded by itself does.
    Threads,
    /// On the thread that writes into the chain. For folders coded several at
    /// once on a thread budget, where two more threads to a folder would be
    /// threads nobody asked for.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Inline,
}

/// An LZMA coder for a BCJ2 call or jump stream, writing into `sink`.
fn bcj2_side_encoder(sink: SharedBuf, sides: Bcj2Sides) -> Result<Box<dyn Write>, Error> {
    let input = CountingWriter::new(sink);
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    let lz = {
        // 7-Zip's side coders run its default level (5, the binary-tree
        // match finder) with the settings above.
        let coder = SideCoder {
            level: 5,
            dict_size: BCJ2_SIDE_DICT_SIZE,
            fast_bytes: BCJ2_SIDE_FAST_BYTES,
            lc: BCJ2_SIDE_LC,
            lp: BCJ2_SIDE_LP,
            pb: BCJ2_SIDE_PB,
        };
        match sides {
            Bcj2Sides::Threads => coder.writer(input)?,
            Bcj2Sides::Inline => coder.inline_writer(input)?,
        }
    };
    // This encoder never starts a thread of its own.
    #[cfg(feature = "lzma-rust2-encoder")]
    let _ = sides;
    #[cfg(feature = "lzma-rust2-encoder")]
    let lz = {
        let mut options = lzma_rust2::LzmaOptions::with_preset(5);
        options.dict_size = BCJ2_SIDE_DICT_SIZE;
        options.nice_len = BCJ2_SIDE_FAST_BYTES;
        options.lc = u32::from(BCJ2_SIDE_LC);
        options.lp = u32::from(BCJ2_SIDE_LP);
        options.pb = u32::from(BCJ2_SIDE_PB);
        LzmaWriter::new_no_header(input, &options, false)?
    };
    Ok(Box::new(Encoder::Lzma(Some(lz))))
}

/// A BCJ2 coder whose main stream goes on to `main`, with the call and jump
/// streams' LZMA coders built here, to run where `sides` says. The returned
/// slot is filled when the coder is finished.
pub(crate) fn add_bcj2_encoder<W: Write>(
    main: CountingWriter<W>,
    sides: Bcj2Sides,
) -> Result<(Encoder<W>, Bcj2Slot), Error> {
    let call = SharedBuf::default();
    let jump = SharedBuf::default();
    let tail = Bcj2Slot::default();
    let writer = Bcj2Writer::new(
        main,
        bcj2_side_encoder(call.clone(), sides)?,
        bcj2_side_encoder(jump.clone(), sides)?,
    );
    let side = Bcj2Side {
        call,
        jump,
        tail: Rc::clone(&tail),
    };
    Ok((Encoder::Bcj2(Some(Box::new(writer)), side), tail))
}

fn validate_lzma_dictionary_size(dict_size: u32) -> Result<(), Error> {
    // Keep the binary tree's two indices per dictionary position within i32,
    // and its Vec<i32> allocation within the platform's isize::MAX bytes.
    // This also leaves room for the encoder's lookahead and reserve buffers.
    let max_dict_size = ((1u64 << 30) - 1).min(isize::MAX as u64 / 8 - 1);
    if !(u64::from(LzmaSettings::DICT_SIZE_MIN)..=max_dict_size).contains(&u64::from(dict_size)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported LZMA dictionary size: {dict_size} (maximum {max_dict_size})"),
        )
        .into());
    }
    Ok(())
}

/// The LZMA2 property byte for `dict_size`: the smallest of the table's 41
/// dictionaries that is at least as large.
///
/// The encoder's window is exactly `dict_size`, so the byte must never name a
/// smaller dictionary, or a decoder allocates less than a match can reach
/// back over. A size that is not a power of two or three times one is rounded
/// up, as `Lzma2Enc_WriteProperties` does.
fn lzma2_property_for(dict_size: u32) -> u8 {
    (0..=40u8)
        .find(|&prop| {
            crate::codec::lzma_turbo::lzma2_dictionary_size(&[prop]).is_ok_and(|d| d >= dict_size)
        })
        .unwrap_or(40)
}

/// The `lzma-turbo` setting for a coder of `settings` that the caller allowed
/// `threads` threads.
///
/// More than one engages the threaded match finder, as 7-Zip does
/// (C++: `numThreads = (algo == 0 || btMode == 0) ? 1 : 2`): the
/// binary-tree finder of the normal mode gets a thread of its own, the
/// fast mode's hash chain does not. The output is the same bytes either
/// way.
///
/// Here and not with the settings: the option types name no encoder, so that
/// which one codes them is decided in this file alone.
#[cfg(not(feature = "lzma-rust2-encoder"))]
fn turbo_props(settings: &LzmaSettings, threads: u32) -> lzma_turbo::LzmaEncProps {
    use lzma_turbo::MatchFinderKind;

    let fast = settings.fast();
    let mut props = lzma_turbo::LzmaEncProps::new()
        .with_level(settings.level())
        .with_dict_size(settings.requested_dict_size())
        .with_fast_bytes(settings.nice_len())
        .with_fast_mode(fast)
        .with_match_finder(if fast {
            MatchFinderKind::Hc4
        } else {
            MatchFinderKind::Bt4
        });
    if let Some(depth) = settings.hash_chain_depth() {
        props = props.with_match_cycles(depth);
    } else if settings.match_finder_threads(threads) > 1 {
        props = props.with_num_threads(2);
    }
    // C: `props.reduceSize`. Only where it shrinks the dictionary, so an
    // input at least the dictionary's size is coded exactly as before.
    //
    // Raised to `LzmaEncProps_Normalize`'s floor, below which it shrinks
    // nothing further: the coder is the same, and so are the settings, which
    // is what lets one kept coder serve every folder under the floor (see
    // `FolderCoder`). The size is read nowhere else on this path.
    if let Some(size) = settings.input_size()
        && size < u64::from(settings.requested_dict_size())
    {
        props = props.with_reduce_size(size.max(REDUCE_SIZE_FLOOR));
    }
    props
}

/// C: `kReduceMin` in `LzmaEncProps_Normalize`: a dictionary is never shrunk
/// below 4 KiB to fit the input.
#[cfg(not(feature = "lzma-rust2-encoder"))]
const REDUCE_SIZE_FLOOR: u64 = 1 << 12;

/// What one thread coding folder after folder keeps between them: the LZMA
/// or LZMA2 coder the data meets first, when that coder runs on one thread,
/// with its window, tables and state.
///
/// [`FolderCoder::plan`] says whether a folder's first coder is one; if it
/// is, the caller hands that coder the folder's reader and the rest of the
/// chain, built as [`add_encoder`] builds it, as its output. Nothing is
/// started on another thread and nothing is copied on the way in: the
/// encoder reads the input into its own window. Folders whose settings are
/// the same - every folder under 4 KiB, and every folder at least the
/// dictionary's size - share one encoder; any other folder builds its own,
/// in place of the one kept.
///
/// With the `lzma-rust2-encoder` feature nothing is planned here: that
/// encoder already codes on the thread that writes into it.
#[derive(Default)]
pub(crate) struct FolderCoder {
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    pull: PullCoder,
}

/// A coder [`FolderCoder::plan`] accepted, ready for [`FolderCoder::encode`].
pub(crate) struct PulledCoder {
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    props: lzma_turbo::LzmaEncProps,
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    coder: Coder,
}

impl FolderCoder {
    /// The coder [`FolderCoder::encode`] would run for `mc`, if `mc` is LZMA
    /// or LZMA2 on one thread: one block thread and no match-finder thread.
    ///
    /// # Errors
    ///
    /// A dictionary size the encoder refuses, as [`add_encoder`] refuses it.
    pub(crate) fn plan(mc: &EncoderConfiguration) -> Result<Option<PulledCoder>, Error> {
        #[cfg(not(feature = "lzma-rust2-encoder"))]
        {
            let (props, coder) = match (mc.method.id(), &mc.options) {
                (EncoderMethod::ID_LZMA, options) => {
                    let options = match options {
                        Some(EncoderOptions::Lzma(options)) => options.clone(),
                        _ => LzmaOptions::default(),
                    };
                    validate_lzma_dictionary_size(options.0.dict_size())?;
                    (turbo_props(&options.0, 1), Coder::Lzma)
                }
                (EncoderMethod::ID_LZMA2, options) => {
                    let options = match options {
                        Some(EncoderOptions::Lzma2(options)) => options.clone(),
                        _ => Lzma2Options::default(),
                    };
                    validate_lzma_dictionary_size(options.settings.dict_size())?;
                    let (block_size, threads) = lzma2_block_plan(&options);
                    (
                        turbo_props(&options.settings, options.threads),
                        Coder::Lzma2 {
                            block_size,
                            threads,
                        },
                    )
                }
                _ => return Ok(None),
            };
            Ok(PullCoder::drives(&props, coder).then_some(PulledCoder { props, coder }))
        }
        #[cfg(feature = "lzma-rust2-encoder")]
        {
            let _ = mc;
            Ok(None)
        }
    }

    /// Codes everything `input` yields with `coder` into `out`, on this
    /// thread.
    ///
    /// # Errors
    ///
    /// What `input` or `out` returned, or the encoder's own failure.
    pub(crate) fn encode(
        &mut self,
        coder: &PulledCoder,
        input: &mut dyn std::io::Read,
        out: &mut dyn Write,
    ) -> std::io::Result<()> {
        #[cfg(not(feature = "lzma-rust2-encoder"))]
        {
            self.pull.encode(&coder.props, coder.coder, input, out)
        }
        #[cfg(feature = "lzma-rust2-encoder")]
        {
            // `plan` never hands one out with this feature.
            let _ = (coder, input, out);
            unreachable!("no coder is pulled with the lzma-rust2 encoder")
        }
    }
}

/// The `lzma-rust2` setting for `settings`: its preset for the level, which
/// is the settings' own table, with the caller's overrides applied.
#[cfg(feature = "lzma-rust2-encoder")]
fn rust2_options(settings: &LzmaSettings) -> lzma_rust2::LzmaOptions {
    let mut options = lzma_rust2::LzmaOptions::with_preset(settings.level());
    options.dict_size = settings.dict_size();
    options.nice_len = settings.nice_len();
    options
}

/// Whether the folder these options were sized for is known to fit one LZMA2
/// block.
///
/// Such a folder is that block on one thread, whichever encoder codes it: the
/// same bytes the block-parallel coder would produce, without starting its
/// pool and buffering the block per folder.
fn lzma2_fits_one_block(options: &Lzma2Options) -> bool {
    options.block_size().is_some_and(|block_size| {
        options
            .settings
            .input_size()
            .is_some_and(|size| size <= block_size)
    })
}

/// The block size and block-thread count `lzma-turbo`'s LZMA2 coder runs with.
///
/// One thread is the solid stream; block threads need a block size, and a
/// chunk size without threads changes nothing. A folder that fits one block
/// (see [`lzma2_fits_one_block`]) runs on one thread.
///
/// Each block thread's coder runs its match finder on a thread of its own
/// when `LzmaSettings::match_finder_threads` says two, so the thread count
/// is divided by that, as `Lzma2EncProps_Normalize` divides
/// `numTotalThreads` by `numThreads`: block threads times match-finder
/// threads stays within the caller's count.
#[cfg(not(feature = "lzma-rust2-encoder"))]
fn lzma2_block_plan(options: &Lzma2Options) -> (u64, usize) {
    match (options.threads, options.block_size()) {
        (0 | 1, _) | (_, None) => (lzma_turbo::BLOCK_SIZE_SOLID, 1),
        (_, Some(block_size)) if lzma2_fits_one_block(options) => (block_size, 1),
        (threads, Some(block_size)) => {
            let per_block = options.settings.match_finder_threads(threads);
            (block_size, (threads / per_block).max(1) as usize)
        }
    }
}

/// Whether `lzma-rust2`'s multi-threaded LZMA2 writer codes a folder: only
/// with more than one thread, and not for a folder that fits one block (see
/// [`lzma2_fits_one_block`]).
#[cfg(feature = "lzma-rust2-encoder")]
fn lzma2_rust2_uses_mt(options: &Lzma2Options) -> bool {
    options.threads > 1 && !lzma2_fits_one_block(options)
}

/// The threads of its own the coder of `mc` runs on: a block-parallel LZMA2
/// coder's block threads, and with `lzma-turbo`'s encoder every other LZMA
/// and LZMA2 coder's one. Any other coder has none: it runs on the thread
/// writing into the chain.
fn coder_threads(mc: &EncoderConfiguration) -> u32 {
    match (mc.method.id(), &mc.options) {
        #[cfg(not(feature = "lzma-rust2-encoder"))]
        (EncoderMethod::ID_LZMA2, Some(EncoderOptions::Lzma2(options))) => {
            lzma2_block_plan(options).1 as u32
        }
        #[cfg(not(feature = "lzma-rust2-encoder"))]
        (EncoderMethod::ID_LZMA | EncoderMethod::ID_LZMA2, _) => 1,
        // `lzma-rust2`'s single-threaded writers code inside `write`.
        #[cfg(feature = "lzma-rust2-encoder")]
        (EncoderMethod::ID_LZMA2, Some(EncoderOptions::Lzma2(options)))
            if lzma2_rust2_uses_mt(options) =>
        {
            options.threads
        }
        _ => 0,
    }
}

/// Whether `method` compresses, where a filter or the cipher only transforms
/// what passes through it.
fn compresses(method: EncoderMethod) -> bool {
    matches!(
        method.id(),
        EncoderMethod::ID_LZMA
            | EncoderMethod::ID_LZMA2
            | EncoderMethod::ID_PPMD
            | EncoderMethod::ID_BZIP2
            | EncoderMethod::ID_DEFLATE
            | EncoderMethod::ID_BROTLI
            | EncoderMethod::ID_ZSTD
            | EncoderMethod::ID_LZ4
    )
}

/// The threads coding one folder with `methods` keeps busy. `methods` are the
/// folder's, already sized for it.
///
/// The coders of a chain run at once, so the threads they run on add up (see
/// [`coder_threads`]). Every other coder runs on the thread writing into the
/// chain, which is the one thread a chain with no such coder keeps busy.
///
/// Beside such a coder that thread is counted only where a compressor codes
/// on it. A second compressor is at work while the coders' threads are: eight
/// folders of PPMd or BZip2 with LZMA2 measured at up to 12.7 cores. A filter
/// or the cipher is not counted: the thread passes the coders' bytes through
/// it and waits for them, and eight folders of LZMA2 measured at 7.7 to 7.9
/// cores with AES-256 over it and 7.7 to 8.0 without.
///
/// BCJ2's call and jump coders are not counted here: a caller that counts its
/// folders against a thread budget builds the chain with
/// [`Bcj2Sides::Inline`], which starts no thread for them.
pub(crate) fn folder_threads(methods: &[EncoderConfiguration]) -> u32 {
    let own = methods
        .iter()
        .map(coder_threads)
        .fold(0, u32::saturating_add);
    if own == 0 {
        return 1;
    }
    let writing_thread_compresses = methods
        .iter()
        .any(|mc| coder_threads(mc) == 0 && compresses(mc.method));
    own.saturating_add(u32::from(writing_thread_compresses))
}

/// `methods` for a folder coded on one worker of a parallel non-solid write:
/// every LZMA2 coder on one thread.
///
/// The workers are the thread budget, and such a folder already keeps busy
/// the threads [`folder_threads`] counts for it; left at more than one, a
/// coder would still start a match-finder thread of its own and double the
/// threads in use.
pub(crate) fn one_thread_each(methods: &[EncoderConfiguration]) -> Vec<EncoderConfiguration> {
    methods
        .iter()
        .map(|mc| match &mc.options {
            Some(EncoderOptions::Lzma2(options)) if options.threads > 1 => EncoderConfiguration {
                method: mc.method,
                options: Some(EncoderOptions::Lzma2(Lzma2Options {
                    threads: 1,
                    ..options.clone()
                })),
            },
            _ => mc.clone(),
        })
        .collect()
}

pub(crate) fn add_encoder<W: Write>(
    input: CountingWriter<W>,
    method_config: &EncoderConfiguration,
) -> Result<Encoder<W>, Error> {
    let method = method_config.method;

    match method.id() {
        EncoderMethod::ID_COPY => Ok(Encoder::Copy(input)),
        EncoderMethod::ID_DELTA => {
            let options = match method_config.options {
                Some(EncoderOptions::Delta(options)) => options,
                _ => DeltaOptions::default(),
            };
            let dw = DeltaWriter::new(input, options.0 as usize);
            Ok(Encoder::Delta(dw))
        }
        EncoderMethod::ID_BCJ_X86 => Ok(Encoder::Bcj(Some(BcjWriter::new_x86(input, 0)))),
        EncoderMethod::ID_BCJ_ARM => Ok(Encoder::Bcj(Some(BcjWriter::new_arm(input, 0)))),
        EncoderMethod::ID_BCJ_ARM_THUMB => {
            Ok(Encoder::Bcj(Some(BcjWriter::new_arm_thumb(input, 0))))
        }
        EncoderMethod::ID_BCJ_ARM64 => Ok(Encoder::Bcj(Some(BcjWriter::new_arm64(input, 0)))),
        EncoderMethod::ID_BCJ_IA64 => Ok(Encoder::Bcj(Some(BcjWriter::new_ia64(input, 0)))),
        EncoderMethod::ID_BCJ_SPARC => Ok(Encoder::Bcj(Some(BcjWriter::new_sparc(input, 0)))),
        EncoderMethod::ID_BCJ_PPC => Ok(Encoder::Bcj(Some(BcjWriter::new_ppc(input, 0)))),
        EncoderMethod::ID_BCJ_RISCV => Ok(Encoder::Bcj(Some(BcjWriter::new_riscv(input, 0)))),
        EncoderMethod::ID_LZMA => {
            let options = match &method_config.options {
                Some(EncoderOptions::Lzma(options)) => options.clone(),
                _ => LzmaOptions::default(),
            };
            validate_lzma_dictionary_size(options.0.dict_size())?;
            #[cfg(not(feature = "lzma-rust2-encoder"))]
            let lz = LzmaTurboWriter::new(input, &turbo_props(&options.0, 1), Coder::Lzma)?;
            #[cfg(feature = "lzma-rust2-encoder")]
            let lz = LzmaWriter::new_no_header(input, &rust2_options(&options.0), false)?;
            Ok(Encoder::Lzma(Some(lz)))
        }
        EncoderMethod::ID_LZMA2 => {
            let lzma2_options = match &method_config.options {
                Some(EncoderOptions::Lzma2(options)) => options.clone(),
                _ => Lzma2Options::default(),
            };

            validate_lzma_dictionary_size(lzma2_options.settings.dict_size())?;
            #[cfg(not(feature = "lzma-rust2-encoder"))]
            let encoder = {
                let (block_size, threads) = lzma2_block_plan(&lzma2_options);
                Encoder::Lzma2(Some(LzmaTurboWriter::new(
                    input,
                    &turbo_props(&lzma2_options.settings, lzma2_options.threads),
                    Coder::Lzma2 {
                        block_size,
                        threads,
                    },
                )?))
            };
            #[cfg(feature = "lzma-rust2-encoder")]
            let encoder = {
                let mut options =
                    lzma_rust2::Lzma2Options::with_preset(lzma2_options.settings.level());
                options.lzma_options = rust2_options(&lzma2_options.settings);
                options.set_chunk_size(
                    lzma2_options
                        .block_size()
                        .and_then(std::num::NonZeroU64::new),
                );
                if lzma2_rust2_uses_mt(&lzma2_options) {
                    Encoder::Lzma2Mt(Some(Lzma2WriterMt::new(
                        input,
                        options,
                        lzma2_options.threads,
                    )?))
                } else {
                    Encoder::Lzma2(Some(Lzma2Writer::new(input, options)))
                }
            };

            Ok(encoder)
        }
        #[cfg(feature = "ppmd")]
        EncoderMethod::ID_PPMD => {
            let options = match method_config.options {
                Some(EncoderOptions::Ppmd(options)) => options,
                _ => PpmdOptions::default(),
            };

            // The options are clamped into range when built, so these are the
            // parameters the coder's properties record.
            let params = ppmd_turbo::Params::clamped(options.order, options.memory_size);
            let ppmd_encoder = ppmd_turbo::io::SevenZWriter::new(input, params)
                .map_err(|err| Error::from(std::io::Error::from(err)))?;

            Ok(Encoder::Ppmd(Some(Box::new(ppmd_encoder))))
        }
        #[cfg(feature = "brotli")]
        EncoderMethod::ID_BROTLI => {
            let options = match method_config.options {
                Some(EncoderOptions::Brotli(options)) => options,
                _ => BrotliOptions::default(),
            };

            let brotli_encoder = BrotliEncoder::new(
                input,
                options.quality,
                options.window,
                options.skippable_frame_size as usize,
            )?;

            Ok(Encoder::Brotli(brotli_encoder))
        }
        #[cfg(feature = "bzip2")]
        EncoderMethod::ID_BZIP2 => {
            let options = match method_config.options {
                Some(EncoderOptions::Bzip2(options)) => options,
                _ => Bzip2Options::default(),
            };

            let bzip2_encoder =
                bzip2::write::BzEncoder::new(input, bzip2::Compression::new(options.0));

            Ok(Encoder::Bzip2(Some(bzip2_encoder)))
        }
        #[cfg(feature = "deflate")]
        EncoderMethod::ID_DEFLATE => {
            let options = match method_config.options {
                Some(EncoderOptions::Deflate(options)) => options,
                _ => DeflateOptions::default(),
            };

            let deflate_encoder =
                flate2::write::DeflateEncoder::new(input, flate2::Compression::new(options.0));
            Ok(Encoder::Deflate(Some(deflate_encoder)))
        }
        #[cfg(feature = "lz4")]
        EncoderMethod::ID_LZ4 => {
            let options = match method_config.options.as_ref() {
                Some(EncoderOptions::Lz4(options)) => *options,
                _ => Lz4Options::default(),
            };

            let lz4_encoder = Lz4Encoder::new(input, options.skippable_frame_size as usize)?;

            Ok(Encoder::Lz4(Some(lz4_encoder)))
        }
        #[cfg(feature = "zstd")]
        EncoderMethod::ID_ZSTD => {
            let options = match method_config.options.as_ref() {
                Some(EncoderOptions::Zstd(options)) => *options,
                _ => ZstandardOptions::default(),
            };

            let zstd_encoder = zstd::Encoder::new(input, options.0 as i32)?;

            Ok(Encoder::Zstd(Some(zstd_encoder)))
        }
        #[cfg(feature = "aes256")]
        EncoderMethod::ID_AES256_SHA256 => {
            let options = match method_config.options.as_ref() {
                Some(EncoderOptions::Aes(p)) => p,
                _ => return Err(Error::PasswordRequired),
            };
            Ok(Encoder::Aes(Aes256Sha256Encoder::new(input, options)?))
        }
        _ => Err(Error::UnsupportedCompressionMethod(
            method.name().to_string(),
        )),
    }
}

pub(crate) fn get_options_as_properties<'a>(
    method: EncoderMethod,
    options: Option<&EncoderOptions>,
    out: &'a mut [u8],
) -> &'a [u8] {
    match method.id() {
        EncoderMethod::ID_DELTA => {
            let options = match options {
                Some(EncoderOptions::Delta(options)) => *options,
                _ => DeltaOptions::default(),
            };

            out[0] = options.0.saturating_sub(1) as u8;
            &out[0..1]
        }
        EncoderMethod::ID_LZMA2 => {
            let options = match options {
                Some(EncoderOptions::Lzma2(options)) => options,
                _ => &Lzma2Options::default(),
            };
            out[0] = lzma2_property_for(options.settings.dict_size());
            &out[0..1]
        }
        EncoderMethod::ID_LZMA => {
            let options = match options {
                Some(EncoderOptions::Lzma(options)) => options,
                _ => &LzmaOptions::default(),
            };
            let dict_size = options.0.dict_size();
            out[0] = LzmaSettings::props_byte();
            out[1..5].copy_from_slice(dict_size.to_le_bytes().as_ref());
            &out[0..5]
        }
        #[cfg(feature = "ppmd")]
        EncoderMethod::ID_PPMD => {
            let options = match options {
                Some(EncoderOptions::Ppmd(options)) => *options,
                _ => PpmdOptions::default(),
            };

            out[0] = options.order as u8;
            out[1..5].copy_from_slice(&options.memory_size.to_le_bytes());
            &out[0..5]
        }
        #[cfg(feature = "brotli")]
        EncoderMethod::ID_BROTLI => {
            let version_major = brotli::VERSION;
            let version_minor = 0;
            let options = match options {
                Some(EncoderOptions::Brotli(options)) => *options,
                _ => BrotliOptions::default(),
            };

            out[0] = version_major;
            out[1] = version_minor;
            out[2] = options.quality as u8;
            &out[0..3]
        }
        #[cfg(feature = "lz4")]
        EncoderMethod::ID_LZ4 => {
            // Since we use lz4_flex, we only support one compression level
            // and set the version to 1.0 for best compatibility.
            out[0] = 1; // Major version
            out[1] = 0; // Minor version
            out[2] = 3; // Fast compression
            &out[0..3]
        }
        #[cfg(feature = "zstd")]
        EncoderMethod::ID_ZSTD => {
            let version_major = zstd::zstd_safe::VERSION_MAJOR;
            let version_minor = zstd::zstd_safe::VERSION_MINOR;
            let options = match options {
                Some(EncoderOptions::Zstd(options)) => *options,
                _ => ZstandardOptions::default(),
            };

            out[0] = version_major as u8;
            out[1] = version_minor as u8;
            out[2] = options.0 as u8;
            &out[0..3]
        }
        #[cfg(feature = "aes256")]
        EncoderMethod::ID_AES256_SHA256 => {
            let options = match options.as_ref() {
                Some(EncoderOptions::Aes(p)) => p,
                _ => return &[],
            };
            options.write_properties(out);
            &out[..34]
        }
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    use super::lzma2_block_plan;
    use super::lzma2_property_for;
    use crate::codec::lzma_turbo::lzma2_dictionary_size;

    #[test]
    fn the_lzma2_property_never_names_a_smaller_dictionary() {
        for dict in [
            4096u32,
            1 << 18,
            (1 << 20) + 1,
            3 << 20,
            5 << 20,
            (3 << 20) + 1,
            1 << 26,
            (1 << 30) - 1,
            u32::MAX,
        ] {
            let prop = lzma2_property_for(dict);
            let named = lzma2_dictionary_size(&[prop]).unwrap();
            assert!(named >= dict, "{dict}: property {prop} names {named}");
            if prop > 0 {
                let below = lzma2_dictionary_size(&[prop - 1]).unwrap();
                assert!(below < dict, "{dict}: property {} would do", prop - 1);
            }
        }
        assert_eq!(lzma2_property_for(1 << 20), 16);
        assert_eq!(lzma2_property_for(3 << 20), 19);
        assert_eq!(lzma2_property_for(5 << 20), 21);
    }

    /// A folder that fits one block skips the block-parallel coder; one that
    /// does not, or whose size is unknown, keeps the threads it was given.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    #[test]
    fn a_folder_that_fits_one_block_is_coded_on_one_thread() {
        use crate::{EncoderConfiguration, encoder_options::EncoderOptions};

        let options = crate::encoder_options::Lzma2Options::from_level_mt(5, 8, 32 << 20);
        let sized = |size: u64| {
            let config: EncoderConfiguration = options.clone().into();
            match config.sized_for(size).expect("LZMA2").options {
                Some(EncoderOptions::Lzma2(o)) => o,
                other => panic!("not LZMA2 options: {other:?}"),
            }
        };
        // Level 5's binary-tree finder takes a thread per block: 4 x 2.
        assert_eq!(lzma2_block_plan(&options), (32 << 20, 4));
        assert_eq!(lzma2_block_plan(&sized(16 << 20)), (32 << 20, 1));
        assert_eq!(lzma2_block_plan(&sized(32 << 20)), (32 << 20, 1));
        assert_eq!(lzma2_block_plan(&sized((32 << 20) + 1)), (32 << 20, 4));
        // A small folder is one block of its own dictionary's size.
        assert_eq!(lzma2_block_plan(&sized(1000)), (32 << 20, 1));
        let solid = crate::encoder_options::Lzma2Options::from_level(5);
        assert_eq!(lzma2_block_plan(&solid), (lzma_turbo::BLOCK_SIZE_SOLID, 1));
    }

    /// The LZMA2 options `config` holds once it is sized for a folder of
    /// `size` bytes.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    fn lzma2_sized_for(
        config: &crate::EncoderConfiguration,
        size: u64,
    ) -> crate::encoder_options::Lzma2Options {
        match config.sized_for(size).expect("LZMA2").options {
            Some(crate::encoder_options::EncoderOptions::Lzma2(o)) => o,
            other => panic!("not LZMA2 options: {other:?}"),
        }
    }

    /// The settings the encoder is built with agree with the coder record: a
    /// folder smaller than the dictionary is coded with one its size, and one
    /// no smaller with settings that do not move, so the same bytes come out.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    #[test]
    fn the_encoder_is_built_with_the_dictionary_the_coder_record_names() {
        use super::turbo_props;
        use crate::{EncoderConfiguration, encoder_options::Lzma2Options};

        let config: EncoderConfiguration = Lzma2Options::from_level(5).into();
        let sized = lzma2_sized_for(&config, 100_000);
        assert_eq!(turbo_props(&sized.settings, 1).dict_size(), 100_000);

        let mut options = Lzma2Options::from_level(5);
        options.set_dictionary_size(1 << 16);
        let config: EncoderConfiguration = options.clone().into();
        for size in [1 << 16, (1 << 16) + 1, u64::MAX] {
            let sized = lzma2_sized_for(&config, size);
            assert_eq!(
                turbo_props(&sized.settings, 1),
                turbo_props(&options.settings, 1),
                "size {size}"
            );
        }
    }

    #[cfg(not(feature = "lzma-rust2-encoder"))]
    #[test]
    fn more_than_one_thread_gives_the_binary_tree_finder_a_thread_of_its_own() {
        use crate::encoder_options::Lzma2Options;

        let mf_threads = |level: u32, threads: u32| {
            super::turbo_props(&Lzma2Options::from_level(level).settings, threads)
                .normalized()
                .num_threads
        };
        assert_eq!(mf_threads(5, 1), 1);
        assert_eq!(mf_threads(5, 2), 2);
        assert_eq!(mf_threads(9, 18), 2);
        // The fast levels' hash chain has no threaded finder, as in 7-Zip.
        assert_eq!(mf_threads(1, 18), 1);
    }

    /// Block threads times match-finder threads never exceeds the caller's
    /// count: halved for the binary-tree finder, as 7-Zip does, and whole for
    /// the fast levels' hash chain, which has no thread of its own.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    #[test]
    fn block_threads_leave_room_for_the_match_finder_threads() {
        use crate::encoder_options::Lzma2Options;

        for (level, threads, blocks) in [
            (6, 2, 1),
            (6, 3, 1),
            (6, 8, 4),
            (9, 9, 4),
            (1, 8, 8),
            (3, 5, 5),
        ] {
            let options = Lzma2Options::from_level_mt(level, threads, 1 << 20);
            let (_, got) = lzma2_block_plan(&options);
            assert_eq!(got, blocks, "level {level}, {threads} threads");
            let mf = options.settings.match_finder_threads(threads) as usize;
            assert!(
                got * mf <= threads as usize,
                "level {level}, {threads} threads"
            );
        }
    }

    /// A chain's LZMA and LZMA2 coders each run on a thread of their own, all
    /// at once, so a folder costs their sum; a filter beside them costs
    /// nothing more, and a chain with none of them costs the one thread
    /// writing into it.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    #[test]
    fn a_folder_costs_a_thread_for_each_lzma_coder_in_its_chain() {
        use super::{folder_threads, one_thread_each};
        use crate::encoder_options::{Lzma2Options, LzmaOptions};
        use crate::{EncoderConfiguration, EncoderMethod};

        let lzma2 = || EncoderConfiguration::from(Lzma2Options::from_level(5));
        let lzma = || EncoderConfiguration::from(LzmaOptions::from_level(5));
        // Named without options, each still starts its coder's thread.
        let bare = |method: EncoderMethod| EncoderConfiguration::from(method);
        assert_eq!(folder_threads(&[bare(EncoderMethod::COPY)]), 1);
        assert_eq!(folder_threads(&[lzma2()]), 1);
        assert_eq!(
            folder_threads(&[lzma2(), bare(EncoderMethod::BCJ_X86_FILTER)]),
            1
        );
        assert_eq!(folder_threads(&[lzma2(), lzma()]), 2);
        assert_eq!(folder_threads(&[lzma2(), lzma(), lzma2()]), 3);
        assert_eq!(
            folder_threads(&[bare(EncoderMethod::LZMA2), bare(EncoderMethod::LZMA)]),
            2
        );

        // Four block threads (see the block plan above) and the other coder's
        // one; on a worker of the folder-parallel writer, one each.
        let wide = [
            EncoderConfiguration::from(Lzma2Options::from_level_mt(5, 8, 32 << 20)),
            lzma(),
        ];
        assert_eq!(folder_threads(&wide), 5);
        assert_eq!(folder_threads(&one_thread_each(&wide)), 2);
    }

    /// A compressor with no thread of its own codes on the thread writing
    /// into the chain, and beside an LZMA or LZMA2 coder that thread is at
    /// work while the coder's is: it counts. A filter or the cipher there
    /// does not, and neither does a chain that runs on the one thread anyway.
    #[cfg(not(feature = "lzma-rust2-encoder"))]
    #[test]
    fn a_second_compressor_costs_the_thread_writing_into_the_chain() {
        use super::{folder_threads, one_thread_each};
        use crate::encoder_options::{Lzma2Options, LzmaOptions};
        use crate::{EncoderConfiguration, EncoderMethod};

        let lzma2 = || EncoderConfiguration::from(Lzma2Options::from_level(5));
        let lzma = || EncoderConfiguration::from(LzmaOptions::from_level(5));
        let bare = |method: EncoderMethod| EncoderConfiguration::from(method);

        // Either side of the coder: the one that takes the data, or the one
        // that takes the coder's output.
        for other in [
            EncoderMethod::PPMD,
            EncoderMethod::BZIP2,
            EncoderMethod::DEFLATE,
            EncoderMethod::BROTLI,
            EncoderMethod::ZSTD,
            EncoderMethod::LZ4,
        ] {
            assert_eq!(folder_threads(&[lzma2(), bare(other)]), 2, "{other:?}");
            assert_eq!(folder_threads(&[bare(other), lzma2()]), 2, "{other:?}");
            assert_eq!(folder_threads(&[bare(other), lzma()]), 2, "{other:?}");
            // Alone, or under the cipher, it is the one thread.
            assert_eq!(folder_threads(&[bare(other)]), 1, "{other:?}");
            assert_eq!(
                folder_threads(&[bare(EncoderMethod::AES256_SHA256), bare(other)]),
                1,
                "{other:?}"
            );
        }
        // The writing thread is one thread, whatever codes on it.
        assert_eq!(
            folder_threads(&[
                lzma2(),
                bare(EncoderMethod::PPMD),
                bare(EncoderMethod::BZIP2)
            ]),
            2
        );
        assert_eq!(
            folder_threads(&[lzma2(), lzma(), bare(EncoderMethod::PPMD)]),
            3
        );

        // The cipher and the filters pass the coder's bytes on: no thread.
        for passing in [
            EncoderMethod::AES256_SHA256,
            EncoderMethod::COPY,
            EncoderMethod::DELTA_FILTER,
            EncoderMethod::BCJ_X86_FILTER,
            EncoderMethod::BCJ_ARM64_FILTER,
            EncoderMethod::BCJ2_FILTER,
        ] {
            assert_eq!(folder_threads(&[bare(passing), lzma2()]), 1, "{passing:?}");
            assert_eq!(folder_threads(&[lzma2(), bare(passing)]), 1, "{passing:?}");
            assert_eq!(
                folder_threads(&[bare(passing), lzma2(), lzma()]),
                2,
                "{passing:?}"
            );
        }

        // On a worker of the folder-parallel writer: its coder's one thread
        // and the worker's own.
        let wide = [
            EncoderConfiguration::from(Lzma2Options::from_level_mt(5, 8, 32 << 20)),
            bare(EncoderMethod::PPMD),
        ];
        assert_eq!(folder_threads(&wide), 5);
        assert_eq!(folder_threads(&one_thread_each(&wide)), 2);
    }

    /// `lzma-rust2`'s single-threaded writers code on the thread writing into
    /// the chain, so only its multi-threaded LZMA2 writer costs threads of
    /// its own; a single-threaded writer beside it is a compressor on the
    /// writing thread, and costs that one.
    #[cfg(feature = "lzma-rust2-encoder")]
    #[test]
    fn only_the_rust2_mt_writer_costs_a_folder_threads() {
        use super::{folder_threads, one_thread_each};
        use crate::encoder_options::{Lzma2Options, LzmaOptions};
        use crate::{EncoderConfiguration, EncoderMethod};

        let lzma2 = || EncoderConfiguration::from(Lzma2Options::from_level(5));
        let lzma = || EncoderConfiguration::from(LzmaOptions::from_level(5));
        let mt = || EncoderConfiguration::from(Lzma2Options::from_level_mt(5, 8, 32 << 20));
        assert_eq!(folder_threads(&[lzma2()]), 1);
        assert_eq!(folder_threads(&[lzma2(), lzma()]), 1);
        assert_eq!(folder_threads(&[mt()]), 8);
        assert_eq!(
            folder_threads(&[
                EncoderConfiguration::from(EncoderMethod::AES256_SHA256),
                mt()
            ]),
            8
        );
        let wide = [mt(), lzma()];
        assert_eq!(folder_threads(&wide), 9);
        assert_eq!(folder_threads(&one_thread_each(&wide)), 1);
    }

    /// The `lzma-rust2` encoder makes the same one-block decision: a folder
    /// that fits one block is not handed to its multi-threaded writer.
    #[cfg(feature = "lzma-rust2-encoder")]
    #[test]
    fn a_folder_that_fits_one_block_skips_the_rust2_mt_writer() {
        use super::lzma2_rust2_uses_mt;
        use crate::{EncoderConfiguration, encoder_options::EncoderOptions};

        let options = crate::encoder_options::Lzma2Options::from_level_mt(5, 8, 32 << 20);
        let sized = |size: u64| {
            let config: EncoderConfiguration = options.clone().into();
            match config.sized_for(size).expect("LZMA2").options {
                Some(EncoderOptions::Lzma2(o)) => o,
                other => panic!("not LZMA2 options: {other:?}"),
            }
        };
        assert!(lzma2_rust2_uses_mt(&options));
        assert!(!lzma2_rust2_uses_mt(&sized(16 << 20)));
        assert!(!lzma2_rust2_uses_mt(&sized(32 << 20)));
        assert!(lzma2_rust2_uses_mt(&sized((32 << 20) + 1)));
        assert!(!lzma2_rust2_uses_mt(&sized(1000)));
        let solid = crate::encoder_options::Lzma2Options::from_level(5);
        assert!(!lzma2_rust2_uses_mt(&solid));
    }
}
