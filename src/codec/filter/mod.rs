//! Branch/call/jump and delta filters.
//!
//! This crate's BCJ and delta filters, ported from the LZMA SDK's, over
//! `lzma-turbo`'s converters; `bcj2` is a `Read` and a `Write` over
//! `lzma_turbo::filters::bcj2`.

// The readers and writers carry accessors this crate does not call
// (`into_inner`, `inner`, `inner_mut`).
#[allow(dead_code)]
pub(crate) mod bcj;
pub(crate) mod bcj2;
#[allow(dead_code)]
pub(crate) mod delta;

use std::io;

/// An `InvalidData` error carrying `msg`.
#[inline(always)]
pub(crate) fn error_invalid_data(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
