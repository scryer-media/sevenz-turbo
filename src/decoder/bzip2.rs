use std::io::{self, Read};

use bzip2_turbo::Decoder;

/// Verifies the codec trailer before a size-bounded archive reader can stop.
pub struct Bzip2Decoder<R: Read> {
    inner: Decoder<R>,
    remaining: u64,
    finished: bool,
    failed: bool,
}

impl<R: Read> Bzip2Decoder<R> {
    pub(super) fn new(mut inner: Decoder<R>, size: u64) -> Self {
        // A 7z coder contains one stream; AES can leave padding after it.
        // Match CDecoder::DecodeAllStreams = false while checking its trailer.
        inner.set_multistream(false);
        Self {
            inner,
            remaining: size,
            finished: false,
            failed: false,
        }
    }

    fn read_inner(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.finished {
            return Ok(0);
        }
        let want =
            usize::try_from(self.remaining).map_or(output.len(), |left| output.len().min(left));
        let size = if want == 0 {
            0
        } else {
            self.inner.read(&mut output[..want])?
        };
        if size == 0 && self.remaining != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "BZip2 output ended before its declared size",
            ));
        }
        self.remaining -= size as u64;
        if self.remaining == 0 {
            // Per-file CRC readers stop at their declared length. Reach the
            // stream CRC here, below those readers, even when all file CRCs
            // were already folded on the reconstruction worker.
            let mut probe = [0];
            loop {
                match self.inner.read(&mut probe) {
                    Ok(0) => {
                        self.finished = true;
                        break;
                    }
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "BZip2 output exceeds its declared size",
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(size)
    }
}

impl<R: Read> Read for Bzip2Decoder<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "BZip2 archive decoder previously failed",
            ));
        }
        let result = self.read_inner(output);
        self.failed = result
            .as_ref()
            .is_err_and(|error| error.kind() != io::ErrorKind::Interrupted);
        result
    }
}
