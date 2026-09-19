//! Branch/call/jump and delta filters.
//!
//! # Provenance
//!
//! These files are copied from [`lzma-rust2`] 0.20.1 (`src/filter/`) by Nils
//! Hasenbanck, Apache-2.0, the same licence as this crate. Upstream
//! `sevenz-rust2` reaches them through the `lzma-rust2` dependency; this fork
//! decodes LZMA and LZMA2 with `lzma-turbo` instead, and vendoring the filters
//! is what lets `lzma-rust2` leave the runtime dependency graph entirely
//! rather than being carried for three filters.
//!
//! None of the three carries a converter of its own any more. `lzma-turbo`
//! ports the same branch converters, the same delta filter and, since its
//! 0.5.0, BCJ2 from the same public-domain C, and is tested against the SDK's
//! own harness for them, so `bcj` and `delta` keep only `lzma-rust2`'s readers
//! and writers and put `lzma-turbo`'s filters underneath, and `bcj2` is a
//! `Read` of this crate's own over `lzma_turbo::filters::bcj2`.
//!
//! The only changes are mechanical, so a future re-sync stays a diff:
//!
//! - `crate::Read` / `crate::Write` / `crate::Result` become the `std::io`
//!   items they alias there;
//! - the `encoder` feature becomes this crate's `compress`;
//! - `error_invalid_data` becomes the local helper below instead of a
//!   crate-wide one.
//!
//! Fixes belong upstream in `lzma-rust2` as well as here.
//!
//! [`lzma-rust2`]: https://github.com/hasenbanck/lzma-rust2

// Vendored verbatim, so they carry accessors this crate does not call
// (`into_inner`, `inner`, `inner_mut`). Keeping them keeps a future re-sync a
// diff rather than a merge.
#[allow(dead_code)]
pub(crate) mod bcj;
pub(crate) mod bcj2;
#[allow(dead_code)]
pub(crate) mod delta;

use std::io;

/// `lzma-rust2`'s crate-level helper of the same name.
#[inline(always)]
pub(crate) fn error_invalid_data(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
