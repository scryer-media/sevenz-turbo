use std::io::Read;

use lzma_turbo::crc::Crc32;

/// A wrapper around a reader that tracks read count and CRC32.
///
/// Used during compression to track how much data has been read and compute
/// the CRC32 checksum of the data.
pub struct SourceReader<R> {
    reader: R,
    size: usize,
    crc: Crc32,
    crc_value: u32,
    finished: bool,
}

impl<R> From<R> for SourceReader<R> {
    fn from(value: R) -> Self {
        Self::new(value)
    }
}

impl<R: Read> Read for SourceReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.finished {
            return Ok(0);
        }
        let n = self.reader.read(buf)?;
        if n > 0 {
            self.size += n;
            self.crc.update(&buf[..n]);
        } else {
            let crc = std::mem::take(&mut self.crc);
            self.crc_value = crc.finalize();
            self.finished = true;
        }
        Ok(n)
    }
}

impl<R> SourceReader<R> {
    /// Creates a new source reader wrapper.
    ///
    /// # Arguments
    /// * `reader` - The underlying reader to wrap
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            size: 0,
            crc: Crc32::new(),
            crc_value: 0,
            finished: false,
        }
    }

    /// Returns the total number of bytes read so far.
    pub fn read_count(&self) -> usize {
        self.size
    }

    /// Returns the CRC32 value of all data read.
    ///
    /// The CRC is computed at the first nonempty read that reaches EOF.
    /// Subsequent reads remain at EOF; empty-buffer reads do not end a source.
    pub fn crc_value(&self) -> u32 {
        self.crc_value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::seq_reader::SeqReader;
    use lzma_turbo::crc::crc32;

    #[test]
    fn empty_reads_do_not_finish_or_skip_sources() {
        let parts: [&[u8]; 4] = [b"first", b"", b"second", b""];
        let mut readers =
            SeqReader::new(parts.iter().map(|part| SourceReader::new(*part)).collect());
        assert_eq!(readers.read(&mut []).unwrap(), 0);
        let mut actual = Vec::new();
        let mut buf = [0; 3];
        loop {
            let n = readers.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            actual.extend_from_slice(&buf[..n]);
            assert_eq!(readers.read(&mut []).unwrap(), 0);
        }
        assert_eq!(actual, b"firstsecond");
        for (reader, part) in readers.iter().zip(parts) {
            assert_eq!(reader.read_count(), part.len());
            assert_eq!(reader.crc_value(), crc32(part));
            assert!(reader.finished);
        }
    }

    #[test]
    fn a_zero_crc_is_a_finished_checksum_not_a_sentinel() {
        let data = [0x9d, 0x0a, 0xd9, 0x6d];
        assert_eq!(crc32(&data), 0);
        let mut reader = SourceReader::new(data.as_slice());
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        assert!(reader.finished);
        assert_eq!(reader.read(&mut [0; 8]).unwrap(), 0);
        assert_eq!(reader.crc_value(), 0);
        assert_eq!(reader.read_count(), data.len());
    }
}
