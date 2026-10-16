//! Branch/call/jump and delta filters.
//!
//! # Provenance
//!
//! The `Read` and `Write` wrappers in `bcj.rs` and `delta.rs` come from
//! [`lzma-rust2`] 0.20.1 (`src/filter/`) by Nils Hasenbanck, Apache-2.0, the
//! same licence as this crate; that crate is itself a port of XZ for Java
//! (public domain). The converters underneath them are not `lzma-rust2`'s:
//! they are `lzma-turbo`'s port of the public-domain LZMA SDK filters
//! (`lzma_turbo::filters`), tested against the SDK's own harness. `bcj2.rs`
//! is this crate's own `Read` and `Write` over `lzma_turbo::filters::bcj2`.
//!
//! Each of `bcj.rs` and `delta.rs` lists, in its own header, how it has been
//! modified from the `lzma-rust2` file (Apache-2.0 section 4(b)). Changes
//! common to both:
//!
//! - `crate::Read` / `crate::Write` / `crate::Result` became the `std::io`
//!   items they alias there;
//! - the `encoder` feature became this crate's `compress`, and the tests'
//!   `encoder` + `std` gate became `compress`;
//! - the filter state each wrapper drives was replaced by a handle on
//!   `lzma-turbo`'s converter.
//!
//! [`lzma-rust2`]: https://github.com/hasenbanck/lzma-rust2

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
