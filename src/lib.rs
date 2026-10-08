//! This project is a 7z compressor/decompressor written in pure Rust.
//!
//! `sevenz-turbo` is a fork of [sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2)
//! (itself a fork of the unmaintained `sevenz-rust`). It differs from upstream
//! in two ways, and the module paths and public API are otherwise upstream's:
//!
//! 1. LZMA and LZMA2 decode - and, with `compress`, encode - through
//!    [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo), a port of the
//!    7-Zip reference coders, instead of `lzma-rust2`.
//! 2. It adds the container API a streaming consumer needs: memory limits
//!    enforced before allocation, per-member CRCs, folder-to-pack-stream byte
//!    ranges, a borrowing reader, typed corruption errors carrying a block
//!    index and packed offset, and a per-block completion hook.
//!
//! The `CHANGELOG.md` section "Fork" is the exhaustive divergence list.
//!
//! ## Supported Codecs & filters
//!
//! | Codec          | Decompression | Compression |
//! |----------------|---------------|-------------|
//! | COPY           | ✓             | ✓           |
//! | LZMA           | ✓             | ✓           |
//! | LZMA2          | ✓             | ✓           |
//! | BROTLI (*)     | ✓             | ✓           |
//! | BZIP2          | ✓             | ✓           |
//! | DEFLATE (*)    | ✓             | ✓           |
//! | PPMD           | ✓             | ✓           |
//! | LZ4 (*)        | ✓             | ✓           |
//! | ZSTD (*)       | ✓             | ✓           |
//!
//! (*) Require optional cargo feature.
//!
//! | Filter        | Decompression | Compression |
//! |---------------|---------------|-------------|
//! | BCJ X86       | ✓             | ✓           |
//! | BCJ ARM       | ✓             | ✓           |
//! | BCJ ARM64     | ✓             | ✓           |
//! | BCJ ARM_THUMB | ✓             | ✓           |
//! | BCJ RISC_V    | ✓             | ✓           |
//! | BCJ PPC       | ✓             | ✓           |
//! | BCJ SPARC     | ✓             | ✓           |
//! | BCJ IA64      | ✓             | ✓           |
//! | BCJ2          | ✓             | ✓           |
//! | DELTA         | ✓             | ✓           |
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

#[cfg(target_arch = "wasm32")]
extern crate wasm_bindgen;

pub mod container;
#[cfg(feature = "aes256")]
mod crypto_backend;
#[cfg(feature = "compress")]
mod encoder;
/// Encoding options when compressing.
#[cfg(feature = "compress")]
pub mod encoder_options;
mod encryption;

/// Names the cryptography backend this build of the crate selected for the 7z
/// `aes256` coder: `"aws-lc"` or `"rustcrypto"`.
///
/// Which cryptography ends up in a binary is decided by Cargo features that a
/// dependency can turn on without the top-level crate noticing, so this is
/// here to be asserted on in a consumer's own tests. See the `aws-lc-crypto`
/// and `native-crypto` features.
#[cfg(feature = "aes256")]
#[must_use]
pub fn crypto_backend() -> &'static str {
    crypto_backend::BACKEND
}

/// The SHA-256 of `data`, computed by the backend [`crypto_backend`] names.
///
/// For a consumer that wants one digest routine in its binary and wants it to
/// be the one this crate's `aes256` coder uses, rather than linking a second
/// cryptography library (or a second configuration of the same one) beside it.
#[cfg(feature = "aes256")]
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut sha = crypto_backend::Sha256::new();
    sha.update(data);
    sha.finalize()
}

/// Names the LZMA and LZMA2 encoder this build of the crate selected:
/// `"lzma-turbo"`, or `"lzma-rust2"` when the `lzma-rust2-encoder` feature is
/// on.
///
/// Like [`crypto_backend`], a dependency can turn that feature on without the
/// top-level crate noticing; this is here to be asserted on.
#[cfg(feature = "compress")]
#[must_use]
pub fn lzma_encoder() -> &'static str {
    if cfg!(feature = "lzma-rust2-encoder") {
        "lzma-rust2"
    } else {
        "lzma-turbo"
    }
}

/// Embedder-supplied delegation hooks for the bulk AES-256-CBC decrypt
/// (the `crypto-host` feature). See the module documentation for the contract
/// a host must satisfy.
#[cfg(all(feature = "aes256", feature = "crypto-host"))]
pub mod hooks;

mod error;
mod reader;

#[cfg(feature = "compress")]
mod writer;

pub(crate) mod archive;
pub(crate) mod bitset;
pub(crate) mod block;
pub(crate) mod codec;
pub(crate) mod decoder;

mod time;
#[cfg(feature = "util")]
mod util;

use std::{
    io::{Read, Write},
    ops::{Deref, DerefMut},
};

pub use archive::*;
pub use block::*;
pub use codec::lzma_turbo::{Lzma2Handle, Lzma2Progress};
pub use container::{
    ArchiveLimits, BlockCompletion, CrcFolder, PackStreamRange, SubStream, SubStreamCompletion,
    UnsizedCoder, coder_memory_estimate, crc32_combine,
};
pub use encryption::Password;
pub use error::{BlockErrorKind, Error, Limit};
pub use reader::{ArchiveReader, BlockDecoder};
pub use time::NtTime;
#[cfg(all(feature = "compress", feature = "util", not(target_arch = "wasm32")))]
pub use util::compress::*;
#[cfg(all(feature = "util", not(target_arch = "wasm32")))]
pub use util::decompress::*;
#[cfg(all(feature = "util", target_arch = "wasm32"))]
pub use util::wasm::*;
#[cfg(feature = "compress")]
pub use writer::*;

trait ByteReader {
    fn read_u8(&mut self) -> std::io::Result<u8>;

    #[cfg(feature = "brotli")]
    fn read_u16(&mut self) -> std::io::Result<u16>;

    fn read_u32(&mut self) -> std::io::Result<u32>;

    fn read_u64(&mut self) -> std::io::Result<u64>;
}

trait ByteWriter {
    #[cfg(feature = "compress")]
    fn write_u8(&mut self, value: u8) -> std::io::Result<()>;

    fn write_u16(&mut self, value: u16) -> std::io::Result<()>;

    #[cfg(feature = "compress")]
    fn write_u32(&mut self, value: u32) -> std::io::Result<()>;

    #[cfg(feature = "compress")]
    fn write_u64(&mut self, value: u64) -> std::io::Result<()>;
}

impl<T: Read> ByteReader for T {
    #[inline(always)]
    fn read_u8(&mut self) -> std::io::Result<u8> {
        let mut buf = [0; 1];
        self.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    #[cfg(feature = "brotli")]
    #[inline(always)]
    fn read_u16(&mut self) -> std::io::Result<u16> {
        let mut buf = [0; 2];
        self.read_exact(buf.as_mut())?;
        Ok(u16::from_le_bytes(buf))
    }

    #[inline(always)]
    fn read_u32(&mut self) -> std::io::Result<u32> {
        let mut buf = [0; 4];
        self.read_exact(buf.as_mut())?;
        Ok(u32::from_le_bytes(buf))
    }

    #[inline(always)]
    fn read_u64(&mut self) -> std::io::Result<u64> {
        let mut buf = [0; 8];
        self.read_exact(buf.as_mut())?;
        Ok(u64::from_le_bytes(buf))
    }
}

impl<T: Write> ByteWriter for T {
    #[cfg(feature = "compress")]
    #[inline(always)]
    fn write_u8(&mut self, value: u8) -> std::io::Result<()> {
        self.write_all(&[value])
    }

    #[inline(always)]
    fn write_u16(&mut self, value: u16) -> std::io::Result<()> {
        self.write_all(&value.to_le_bytes())
    }

    #[cfg(feature = "compress")]
    #[inline(always)]
    fn write_u32(&mut self, value: u32) -> std::io::Result<()> {
        self.write_all(&value.to_le_bytes())
    }

    #[cfg(feature = "compress")]
    #[inline(always)]
    fn write_u64(&mut self, value: u64) -> std::io::Result<()> {
        self.write_all(&value.to_le_bytes())
    }
}

/// A trait for writers that finishes the stream on drop.
trait AutoFinish {
    /// Finish writing the stream without error handling.
    fn finish_ignore_error(self);
}

/// A wrapper around a writer that finishes the stream on drop.
#[allow(private_bounds)]
pub struct AutoFinisher<T: AutoFinish>(Option<T>);

impl<T: AutoFinish> Drop for AutoFinisher<T> {
    fn drop(&mut self) {
        if let Some(writer) = self.0.take() {
            writer.finish_ignore_error();
        }
    }
}

impl<T: AutoFinish> Deref for AutoFinisher<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl<T: AutoFinish> DerefMut for AutoFinisher<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}
