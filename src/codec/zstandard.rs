#[cfg(feature = "compress")]
use std::io::Write;
use std::io::{self, Read};

#[cfg(feature = "zstd")]
pub(crate) type Decoder<R> = zstd::Decoder<'static, io::BufReader<R>>;
#[cfg(not(feature = "zstd"))]
pub(crate) type Decoder<R> = Box<super::owned_io::Reader<R, mzstd::DecoderSessionOwned>>;

pub(crate) fn decoder<R: Read>(source: R, window_log: u32) -> io::Result<Decoder<R>> {
    #[cfg(feature = "zstd")]
    {
        let mut decoder = zstd::Decoder::new(source)?;
        decoder.window_log_max(window_log)?;
        Ok(decoder)
    }
    #[cfg(not(feature = "zstd"))]
    {
        let limits = mzstd::DecodeLimits::DEFAULT
            .with_max_window_log(window_log as u8)
            .map_err(io::Error::other)?;
        let session = mzstd::Decompressor::new(limits.into()).into_session();
        Ok(Box::new(super::owned_io::Reader::new(source, session)))
    }
}

#[cfg(all(feature = "compress", feature = "zstd"))]
pub(crate) type Encoder<W> = zstd::Encoder<'static, W>;
#[cfg(all(feature = "compress", not(feature = "zstd")))]
pub(crate) type Encoder<W> = super::owned_io::Writer<W, mzstd::EncoderSessionOwned>;

#[cfg(feature = "compress")]
pub(crate) fn encoder<W: Write>(sink: W, level: i32) -> io::Result<Encoder<W>> {
    #[cfg(feature = "zstd")]
    {
        zstd::Encoder::new(sink, level)
    }
    #[cfg(not(feature = "zstd"))]
    {
        let level = mzstd::Level::try_from(level).map_err(io::Error::other)?;
        let config = mzstd::EncoderConfig::default().with_level(level);
        let session = mzstd::Compressor::new(config)
            .map_err(io::Error::other)?
            .into_session(Default::default());
        Ok(Encoder::new(sink, session))
    }
}
