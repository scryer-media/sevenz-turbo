//! Bounded I/O for codec sessions owned by a 7z coder chain.
#[cfg(feature = "compress")]
use std::io::Write;
use std::io::{self, BufRead, BufReader, Read};

pub(crate) struct Reader<R, S> {
    source: BufReader<R>,
    session: S,
    pending: bool,
    finished: bool,
}

impl<R: Read, S> Reader<R, S> {
    pub(crate) fn new(source: R, session: S) -> Self {
        Self {
            source: BufReader::new(source),
            session,
            pending: false,
            finished: false,
        }
    }

    #[cfg(all(feature = "mbrotli", not(feature = "brotli")))]
    pub(crate) fn get_mut(&mut self) -> &mut R {
        self.source.get_mut()
    }
}

pub(crate) struct Decoded {
    consumed: usize,
    produced: usize,
    pending: bool,
    finished: bool,
}

pub(crate) trait Decode {
    fn decode(&mut self, input: &[u8], output: &mut [u8], eof: bool) -> io::Result<Decoded>;
}

impl<R: Read, S: Decode> Read for Reader<R, S> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.finished {
            return Ok(0);
        }
        loop {
            let input = if self.pending {
                &[]
            } else {
                self.source.fill_buf()?
            };
            let eof = !self.pending && input.is_empty();
            let step = self.session.decode(input, output, eof)?;
            self.source.consume(step.consumed);
            self.pending = step.pending;
            self.finished = step.finished;
            if step.produced != 0 || self.finished {
                return Ok(step.produced);
            }
        }
    }
}

#[cfg(feature = "compress")]
pub(crate) struct Writer<W, S> {
    sink: W,
    session: S,
    scratch: Box<[u8]>,
}

#[cfg(feature = "compress")]
pub(crate) trait Encode {
    fn process(&mut self, input: &[u8], output: &mut [u8]) -> io::Result<(usize, usize)>;
    fn drain(&mut self, output: &mut [u8], finish: bool) -> io::Result<(usize, bool)>;
}

#[cfg(feature = "compress")]
impl<W: Write, S: Encode> Writer<W, S> {
    pub(crate) fn new(sink: W, session: S) -> Self {
        Self {
            sink,
            session,
            scratch: vec![0; 32 * 1024].into_boxed_slice(),
        }
    }

    fn drain(&mut self, finish: bool) -> io::Result<()> {
        loop {
            let (produced, done) = self.session.drain(&mut self.scratch, finish)?;
            self.sink.write_all(&self.scratch[..produced])?;
            if done {
                return Ok(());
            }
        }
    }

    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.drain(true)?;
        Ok(self.sink)
    }
}

#[cfg(feature = "compress")]
impl<W: Write, S: Encode> Write for Writer<W, S> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        loop {
            let (consumed, produced) = self.session.process(input, &mut self.scratch)?;
            self.sink.write_all(&self.scratch[..produced])?;
            if consumed != 0 {
                return Ok(consumed);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain(false)?;
        self.sink.flush()
    }
}

// Both codec crates expose the same owned-session progress contract.
macro_rules! sessions {
    ($codec:ident, $process:ident) => {
        impl Decode for $codec::DecoderSessionOwned {
            fn decode(
                &mut self,
                input: &[u8],
                output: &mut [u8],
                eof: bool,
            ) -> io::Result<Decoded> {
                let op = if eof {
                    $codec::DecodeOperation::Finish
                } else {
                    $codec::DecodeOperation::Process
                };
                let p = self
                    .process(input, output, op)
                    .map_err(|failure| io::Error::new(io::ErrorKind::InvalidData, failure.error))?;
                Ok(Decoded {
                    consumed: p.consumed,
                    produced: p.produced,
                    pending: p.status == $codec::DecoderStatus::NeedsOutput,
                    finished: p.status == $codec::DecoderStatus::Finished,
                })
            }
        }

        #[cfg(feature = "compress")]
        impl Encode for $codec::EncoderSessionOwned {
            fn process(&mut self, input: &[u8], output: &mut [u8]) -> io::Result<(usize, usize)> {
                let p = self
                    .process(input, output, $codec::Operation::$process)
                    .map_err(io::Error::other)?;
                Ok((p.consumed, p.produced))
            }

            fn drain(&mut self, output: &mut [u8], finish: bool) -> io::Result<(usize, bool)> {
                let p = if finish {
                    self.finish(output)
                } else {
                    self.flush(output)
                }
                .map_err(io::Error::other)?;
                Ok((p.produced, p.status != $codec::EncoderStatus::NeedsOutput))
            }
        }
    };
}

#[cfg(all(feature = "mzstd", not(feature = "zstd")))]
sessions!(mzstd, Continue);
#[cfg(all(feature = "mbrotli", not(feature = "brotli")))]
sessions!(mbrotli, Process);
