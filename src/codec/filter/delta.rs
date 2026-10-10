//! The delta filter, over `lzma-turbo`'s.
//!
//! The readers and writers below are this crate's, ported from the SDK's.
//! What sat under them was a 256-byte ring with a
//! moving index, walked one byte at a time in both directions: per byte, two
//! masked index computations, a load, an add and a store. The filter it
//! implements is `out[i] = in[i] + out[i - distance]`, and the ring exists
//! only to carry the last `distance` bytes from one call to the next.
//!
//! `lzma-turbo` carries the C's own shape instead - the history as a plain
//! prefix that is shifted rather than rotated, so the body is a straight walk
//! over the buffer, and at distances of sixteen and up a block of `distance`
//! bytes at a time, which is legal because any `distance` consecutive outputs
//! depend on bytes that are already final. It is checked byte for byte against
//! the SDK's own delta filter.

use std::io::Read;
#[cfg(feature = "compress")]
use std::io::Write;

use lzma_turbo::filters::delta::Delta as TurboDelta;

pub(crate) struct Delta(TurboDelta);

impl Delta {
    pub(crate) fn new(distance: usize) -> Self {
        // Spec: the property byte is the distance minus one, so distances run
        // from 1 to 256. The 7z method properties are one byte, so a distance
        // outside that range cannot have come off a disk; clamping rather than
        // failing keeps the signature, which cannot fail.
        let props = u8::try_from(distance.clamp(1, 256) - 1).expect("clamped to 0..=255");
        Self(TurboDelta::new(props).expect("every one-byte property is a valid distance"))
    }

    pub(crate) fn decode(&mut self, buf: &mut [u8]) {
        self.0.decode(buf);
    }

    #[cfg(feature = "compress")]
    fn encode(&mut self, buf: &mut [u8]) {
        self.0.encode(buf);
    }
}

/// Reader that applies delta filtering to decompress data.
pub struct DeltaReader<R> {
    inner: R,
    delta: Delta,
}

impl<R> DeltaReader<R> {
    /// Creates a new delta reader with the specified distance.
    pub fn new(inner: R, distance: usize) -> Self {
        Self {
            inner,
            delta: Delta::new(distance),
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
}

impl<R: Read> Read for DeltaReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            return Ok(n);
        }
        self.delta.decode(&mut buf[..n]);
        Ok(n)
    }
}

#[cfg(feature = "compress")]
/// Writer that applies delta filtering before compression.
pub struct DeltaWriter<W> {
    inner: W,
    delta: Delta,
    buffer: Vec<u8>,
}

#[cfg(feature = "compress")]
impl<W> DeltaWriter<W> {
    /// Creates a new delta writer with the specified distance.
    pub fn new(inner: W, distance: usize) -> Self {
        Self {
            inner,
            delta: Delta::new(distance),
            buffer: Vec::with_capacity(4096),
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
}

#[cfg(feature = "compress")]
impl<W: Write> Write for DeltaWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let data_size = buf.len();

        if data_size > self.buffer.len() {
            self.buffer.resize(data_size, 0);
        }

        self.buffer[..data_size].copy_from_slice(buf);
        self.delta.encode(&mut self.buffer[..data_size]);
        self.inner.write(&self.buffer[..data_size])
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(feature = "compress")]
#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn test_delta_roundtrip() {
        let test_cases = [
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            vec![1, 2, 3, 1, 2, 3, 1, 2, 3],
            vec![42, 13, 255, 0, 128, 64, 32, 99, 200, 150],
            vec![100; 20],
            vec![0, 255, 0, 255, 0, 255, 0, 255],
            (0..300).map(|i| (i % 256) as u8).collect(),
        ];

        let distances = vec![1, 2, 4, 8, 16, 32, 64, 128, 256];

        for distance in distances {
            for (i, original_data) in test_cases.iter().enumerate() {
                let mut encoded_buffer = Vec::new();
                let mut writer = DeltaWriter::new(Cursor::new(&mut encoded_buffer), distance);
                std::io::copy(&mut original_data.as_slice(), &mut writer)
                    .expect("Failed to encode data");

                let mut decoded_data = Vec::new();
                let mut reader = DeltaReader::new(Cursor::new(&encoded_buffer), distance);
                std::io::copy(&mut reader, &mut decoded_data).expect("Failed to decode data");

                assert_eq!(
                    original_data, &decoded_data,
                    "Roundtrip failed for distance {distance} with data set {i}",
                );
            }
        }
    }
}
