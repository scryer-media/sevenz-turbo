//! Branch/call/jump filters, over `lzma-turbo`'s converters.
//!
//! The readers and writers below are this crate's, ported from the SDK's.
//! What sat under them was a second port of the same
//! eight converters from the same public-domain C, and `lzma-turbo` already
//! carries one that is tested byte for byte against the SDK's own harness and
//! is where this crate's LZMA comes from. Two ports of one filter is one too
//! many, so `BcjFilter` is now a handle on that one.
//!
//! The contract lines up exactly: both convert in place, both return how many
//! leading bytes they converted, and both expect the caller to carry the tail
//! to the next call. The x86 converter's three bits of carried state live
//! inside the handle, as they did here.

use std::io::Read;
#[cfg(feature = "compress")]
use std::io::Write;

use lzma_turbo::filters::bcj::{Bcj, BcjKind};

pub(crate) struct BcjFilter {
    is_encoder: bool,
    bcj: Bcj,
}

impl BcjFilter {
    fn new(kind: BcjKind, start_pos: usize, is_encoder: bool) -> Self {
        // Every call site starts at zero, and the constructor only refuses a
        // start offset that is not a multiple of the converter's alignment,
        // which zero always is. A start offset that does not fit a `u32` is
        // the same kind of nonsense, and the filter is simply off for it.
        let start = u32::try_from(start_pos).unwrap_or(0);
        let bcj = Bcj::new(kind, start - (start % kind.alignment()))
            .expect("the start offset was just rounded to the converter's alignment");
        Self { is_encoder, bcj }
    }

    pub(crate) fn new_x86(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::X86, start_pos, encoder)
    }

    pub(crate) fn new_arm(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::Arm, start_pos, encoder)
    }

    pub(crate) fn new_arm_thumb(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::ArmThumb, start_pos, encoder)
    }

    pub(crate) fn new_arm64(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::Arm64, start_pos, encoder)
    }

    pub(crate) fn new_power_pc(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::Ppc, start_pos, encoder)
    }

    pub(crate) fn new_sparc(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::Sparc, start_pos, encoder)
    }

    pub(crate) fn new_ia64(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::Ia64, start_pos, encoder)
    }

    pub(crate) fn new_riscv(start_pos: usize, encoder: bool) -> Self {
        Self::new(BcjKind::RiscV, start_pos, encoder)
    }

    #[inline]
    pub(crate) fn code(&mut self, buf: &mut [u8]) -> usize {
        if self.is_encoder {
            self.bcj.encode(buf)
        } else {
            self.bcj.decode(buf)
        }
    }
}

/// How much of its input the reader filters at a time, which off a pack
/// stream is also how much it reads per call: 7-Zip's filter coders use at
/// least this much.
const FILTER_BUF_SIZE: usize = crate::decoder::INPUT_BUF_SIZE;

/// Reader that applies BCJ (Branch/Call/Jump) filtering to compressed data.
pub struct BcjReader<R> {
    inner: R,
    filter: BcjFilter,
    state: State,
}

#[derive(Debug, Default)]
struct State {
    filter_buf: Vec<u8>,
    pos: usize,
    filtered: usize,
    unfiltered: usize,
    end_reached: bool,
}

impl<R> BcjReader<R> {
    fn new(inner: R, filter: BcjFilter) -> Self {
        Self {
            inner,
            filter,
            state: State {
                filter_buf: vec![0; FILTER_BUF_SIZE],
                ..Default::default()
            },
        }
    }

    /// Unwraps the reader, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Returns a reference to the inner reader.
    pub fn inner(&self) -> &R {
        &self.inner
    }

    /// Returns a mutable reference to the inner reader.
    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Creates a new BCJ reader for x86 instruction filtering.
    #[inline]
    pub fn new_x86(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_x86(start_pos, false))
    }

    /// Creates a new BCJ reader for ARM instruction filtering.
    #[inline]
    pub fn new_arm(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_arm(start_pos, false))
    }

    /// Creates a new BCJ reader for ARM64 instruction filtering.
    #[inline]
    pub fn new_arm64(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_arm64(start_pos, false))
    }

    /// Creates a new BCJ reader for ARM Thumb instruction filtering.
    #[inline]
    pub fn new_arm_thumb(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_arm_thumb(start_pos, false))
    }

    /// Creates a new BCJ reader for PowerPC instruction filtering.
    #[inline]
    pub fn new_ppc(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_power_pc(start_pos, false))
    }

    /// Creates a new BCJ reader for SPARC instruction filtering.
    #[inline]
    pub fn new_sparc(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_sparc(start_pos, false))
    }

    /// Creates a new BCJ reader for IA-64 instruction filtering.
    #[inline]
    pub fn new_ia64(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_ia64(start_pos, false))
    }

    /// Creates a new BCJ reader for RISC-V instruction filtering.
    #[inline]
    pub fn new_riscv(inner: R, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_riscv(start_pos, false))
    }
}

impl<R: Read> Read for BcjReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut len = buf.len();
        let mut state = core::mem::take(&mut self.state);
        let mut off = 0;
        let mut size = 0;

        loop {
            // Copy filtered data into the caller-provided buffer.
            if state.filtered > 0 {
                let copy_size = state.filtered.min(len);
                let pos = state.pos;
                buf[off..(off + copy_size)]
                    .copy_from_slice(&state.filter_buf[pos..(pos + copy_size)]);
                state.pos += copy_size;
                state.filtered -= copy_size;
                off += copy_size;
                len -= copy_size;
                size += copy_size;
            }

            // If end of filterBuf was reached, move the pending data to
            // the beginning of the buffer so that more data can be
            // copied into filterBuf on the next loop iteration.
            if state.pos + state.filtered + state.unfiltered == FILTER_BUF_SIZE {
                // state.filter_buf.copy_from_slice(src);
                state.filter_buf.rotate_left(state.pos);
                state.pos = 0;
            }

            if len == 0 || state.end_reached {
                self.state = state;
                return Ok(if size > 0 { size } else { 0 });
            }

            assert_eq!(state.filtered, 0);
            // Get more data into the temporary buffer.
            let mut in_size = FILTER_BUF_SIZE - (state.pos + state.filtered + state.unfiltered);
            let start = state.pos + state.filtered + state.unfiltered;
            let temp = &mut state.filter_buf[start..(start + in_size)];
            in_size = match self.inner.read(temp) {
                Ok(s) => s,
                Err(error) => {
                    self.state = state;
                    return Err(error);
                }
            };

            if in_size == 0 {
                // Mark the remaining unfiltered bytes to be ready
                // to be copied out.
                state.end_reached = true;
                state.filtered = state.unfiltered;
                state.unfiltered = 0;
            } else {
                // Filter the data in filterBuf.
                state.unfiltered += in_size;
                state.filtered = self
                    .filter
                    .code(&mut state.filter_buf[state.pos..(state.pos + state.unfiltered)]);
                assert!(state.filtered <= state.unfiltered);
                state.unfiltered -= state.filtered;
            }
        }
    }
}

/// Writer that applies BCJ (Branch/Call/Jump) filtering to data before compression.
#[cfg(feature = "compress")]
pub struct BcjWriter<W> {
    inner: W,
    filter: BcjFilter,
    buffer: Vec<u8>,
}

#[cfg(feature = "compress")]
impl<W> BcjWriter<W> {
    fn new(inner: W, filter: BcjFilter) -> Self {
        Self {
            inner,
            filter,
            buffer: Vec::with_capacity(FILTER_BUF_SIZE),
        }
    }

    /// Unwraps the writer, returning the underlying writer.
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Returns a reference to the inner writer.
    pub fn inner(&self) -> &W {
        &self.inner
    }

    /// Returns a mutable reference to the inner writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Creates a new BCJ writer for x86 instruction filtering.
    #[inline]
    pub fn new_x86(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_x86(start_pos, true))
    }

    /// Creates a new BCJ writer for ARM instruction filtering.
    #[inline]
    pub fn new_arm(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_arm(start_pos, true))
    }

    /// Creates a new BCJ writer for ARM64 instruction filtering.
    #[inline]
    pub fn new_arm64(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_arm64(start_pos, true))
    }

    /// Creates a new BCJ writer for ARM Thumb instruction filtering.
    #[inline]
    pub fn new_arm_thumb(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_arm_thumb(start_pos, true))
    }

    /// Creates a new BCJ writer for PowerPC instruction filtering.
    #[inline]
    pub fn new_ppc(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_power_pc(start_pos, true))
    }

    /// Creates a new BCJ writer for SPARC instruction filtering.
    #[inline]
    pub fn new_sparc(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_sparc(start_pos, true))
    }

    /// Creates a new BCJ writer for IA-64 instruction filtering.
    #[inline]
    pub fn new_ia64(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_ia64(start_pos, true))
    }

    /// Creates a new BCJ writer for RISC-V instruction filtering.
    #[inline]
    pub fn new_riscv(inner: W, start_pos: usize) -> Self {
        Self::new(inner, BcjFilter::new_riscv(start_pos, true))
    }

    /// Finishes writing by flushing any remaining unprocessed data.
    /// This should be called when no more data will be written.
    pub fn finish(mut self) -> std::io::Result<W>
    where
        W: Write,
    {
        if !self.buffer.is_empty() {
            // Write any remaining unprocessed data.
            self.inner.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        self.inner.flush()?;
        Ok(self.inner)
    }
}

#[cfg(feature = "compress")]
impl<W: Write> Write for BcjWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let original_len = buf.len();

        self.buffer.extend_from_slice(buf);

        let filtered_size = self.filter.code(&mut self.buffer);

        if filtered_size > 0 {
            self.inner.write_all(&self.buffer[..filtered_size])?;
        }

        if filtered_size < self.buffer.len() {
            self.buffer.copy_within(filtered_size.., 0);
            self.buffer.truncate(self.buffer.len() - filtered_size);
        } else {
            self.buffer.clear();
        }

        Ok(original_len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(feature = "compress")]
#[cfg(test)]
mod tests {
    use std::io::{Cursor, copy};

    use super::*;

    /// Upstream's round-trip tests read real `wget` binaries from
    /// `tests/data/`. This fork does not carry executables as fixtures, so the
    /// same round trips run over a deterministic pseudo-random buffer, which is
    /// dense enough in every architecture's branch opcodes to exercise the
    /// filters (the `x86` case asserts that explicitly).
    fn sample(len: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    type MakeWriter = fn(Cursor<Vec<u8>>, usize) -> BcjWriter<Cursor<Vec<u8>>>;
    type MakeReader = fn(Cursor<Vec<u8>>, usize) -> BcjReader<Cursor<Vec<u8>>>;

    fn round_trip(make_writer: MakeWriter, make_reader: MakeReader) -> (Vec<u8>, Vec<u8>) {
        let test_data = sample(64 * 1024);

        let mut writer = make_writer(Cursor::new(Vec::new()), 0);
        copy(&mut test_data.as_slice(), &mut writer).expect("Failed to encode data");
        let encoded_buffer = writer
            .finish()
            .expect("Failed to finish encoding")
            .into_inner();

        let mut decoded_data = Vec::new();
        let mut reader = make_reader(Cursor::new(encoded_buffer.clone()), 0);
        copy(&mut reader, &mut decoded_data).expect("Failed to decode data");

        assert_eq!(test_data, decoded_data);
        (test_data, encoded_buffer)
    }

    #[test]
    fn large_start_pos_does_not_panic() {
        let data = [0u8; 64];
        for make in [
            BcjReader::new_x86,
            BcjReader::new_arm,
            BcjReader::new_arm_thumb,
        ] {
            let mut reader = make(Cursor::new(data), usize::MAX);
            let mut out = Vec::new();
            copy(&mut reader, &mut out).unwrap();
        }
    }

    #[test]
    fn test_bcj_x86_roundtrip() {
        let (plain, encoded) = round_trip(BcjWriter::new_x86, BcjReader::new_x86);
        assert_ne!(plain, encoded, "the x86 filter must have changed something");
    }

    #[test]
    fn test_bcj_arm_roundtrip() {
        round_trip(BcjWriter::new_arm, BcjReader::new_arm);
    }

    #[test]
    fn test_bcj_arm64_roundtrip() {
        round_trip(BcjWriter::new_arm64, BcjReader::new_arm64);
    }

    #[test]
    fn test_bcj_arm_thumb_roundtrip() {
        round_trip(BcjWriter::new_arm_thumb, BcjReader::new_arm_thumb);
    }

    #[test]
    fn test_bcj_ppc_roundtrip() {
        round_trip(BcjWriter::new_ppc, BcjReader::new_ppc);
    }

    #[test]
    fn test_bcj_sparc_roundtrip() {
        round_trip(BcjWriter::new_sparc, BcjReader::new_sparc);
    }

    #[test]
    fn test_bcj_ia64_roundtrip() {
        round_trip(BcjWriter::new_ia64, BcjReader::new_ia64);
    }

    #[test]
    fn test_bcj_riscv_roundtrip() {
        round_trip(BcjWriter::new_riscv, BcjReader::new_riscv);
    }
}
