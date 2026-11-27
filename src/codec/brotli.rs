#[cfg(feature = "compress")]
use std::io::Write;
use std::io::{self, Cursor, Read};

use crate::{ByteReader, Error};

/// Magic bytes of a skippable frame format as used in brotli by zstdmt.
const SKIPPABLE_FRAME_MAGIC: u32 = 0x184D2A50;
/// "BR" in little-endian
const BROTLI_MAGIC: u16 = 0x5242;
#[cfg(feature = "compress")]
const HINT_UNIT_SIZE: usize = 65536;

/// Custom decoder to support the custom format first implemented by zstdmt, which allows to have
/// optional skippable frames.
pub(crate) struct BrotliDecoder<R: Read> {
    inner: Option<RawDecoder<InnerReader<R>>>,
    buffer_size: usize,
}

impl<R: Read> BrotliDecoder<R> {
    pub(crate) fn new(mut input: R, buffer_size: usize) -> Result<Self, Error> {
        let mut header = [0u8; 16];
        let header_read = fill(&mut input, &mut header)?;
        if header_read < 4 {
            return Err(Error::corrupted("Input too short"));
        }

        let magic_value = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);

        let inner_reader = if magic_value == SKIPPABLE_FRAME_MAGIC && header_read >= 16 {
            let skippable_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            if skippable_size != 8 {
                return Err(Error::corrupted("Invalid brotli skippable frame size"));
            }

            let compressed_size =
                u32::from_le_bytes([header[8], header[9], header[10], header[11]]);

            let brotli_magic_value = u16::from_le_bytes([header[12], header[13]]);
            if brotli_magic_value != BROTLI_MAGIC {
                return Err(Error::corrupted("Invalid brotli magic value"));
            }

            InnerReader::new_skippable(input, compressed_size)
        } else {
            InnerReader::new_standard(input, header[..header_read].to_vec())
        };

        let decompressor = decoder(inner_reader, buffer_size)?;

        Ok(BrotliDecoder {
            inner: Some(decompressor),
            buffer_size,
        })
    }
}

/// Reads into `buf` until it is full or `input` ends, retrying an
/// interrupted read, and returns how many bytes it holds. A source may hand
/// out fewer bytes per read than asked, so one read can stop short of a
/// header that is there.
fn fill<R: Read>(input: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n.min(buf.len() - filled),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

impl<R: Read> Read for BrotliDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if let Some(inner) = &mut self.inner {
            match inner.read(buf) {
                Ok(0) => {
                    let inner_reader = inner.get_mut();

                    if inner_reader.read_next_frame_header()? {
                        let reader = std::mem::replace(inner_reader, InnerReader::empty());
                        let mut decompressor = decoder(reader, self.buffer_size)?;
                        let result = decompressor.read(buf);
                        self.inner = Some(decompressor);
                        result
                    } else {
                        self.inner = None;
                        Ok(0)
                    }
                }
                result => result,
            }
        } else {
            Ok(0)
        }
    }
}

enum InnerReader<R: Read> {
    Empty,
    Standard {
        reader: R,
        header_buffer: Cursor<Vec<u8>>,
        header_finished: bool,
    },
    Skippable {
        reader: R,
        remaining_in_frame: u32,
        frame_finished: bool,
    },
}

impl<R: Read> InnerReader<R> {
    fn empty() -> Self {
        InnerReader::Empty
    }

    fn new_standard(reader: R, header: Vec<u8>) -> Self {
        InnerReader::Standard {
            reader,
            header_buffer: Cursor::new(header),
            header_finished: false,
        }
    }

    fn new_skippable(reader: R, remaining_in_frame: u32) -> Self {
        InnerReader::Skippable {
            reader,
            remaining_in_frame,
            frame_finished: false,
        }
    }

    fn read_next_frame_header(&mut self) -> io::Result<bool> {
        match self {
            InnerReader::Empty => Ok(false),
            InnerReader::Standard { .. } => Ok(false),
            InnerReader::Skippable {
                reader,
                remaining_in_frame,
                frame_finished,
            } => {
                if !*frame_finished {
                    return Ok(false);
                }

                match reader.read_u32() {
                    Ok(magic) => {
                        if magic != SKIPPABLE_FRAME_MAGIC {
                            return Ok(false);
                        }

                        let skippable_size = reader.read_u32()?;
                        if skippable_size != 8 {
                            return Ok(false);
                        }

                        let compressed_size = reader.read_u32()?;

                        let brotli_magic = reader.read_u16()?;
                        if brotli_magic != BROTLI_MAGIC {
                            return Ok(false);
                        }

                        let _uncompressed_hint = reader.read_u16()?;

                        *remaining_in_frame = compressed_size;
                        *frame_finished = false;

                        Ok(true)
                    }
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
                    Err(e) => Err(e),
                }
            }
        }
    }
}

impl<R: Read> Read for InnerReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            InnerReader::Empty => Ok(0),
            InnerReader::Standard {
                reader,
                header_buffer,
                header_finished,
            } => {
                if !*header_finished {
                    let bytes_read = header_buffer.read(buf)?;
                    if bytes_read > 0 {
                        return Ok(bytes_read);
                    }
                    *header_finished = true;
                }
                reader.read(buf)
            }
            InnerReader::Skippable {
                reader,
                remaining_in_frame,
                frame_finished,
            } => {
                if *frame_finished || *remaining_in_frame == 0 {
                    return Ok(0);
                }

                let bytes_to_read = std::cmp::min(*remaining_in_frame as usize, buf.len());
                let bytes_read = reader.read(&mut buf[..bytes_to_read])?;

                if bytes_read == 0 {
                    *frame_finished = true;
                    return Ok(0);
                }

                *remaining_in_frame -= bytes_read as u32;
                if *remaining_in_frame == 0 {
                    *frame_finished = true;
                }

                Ok(bytes_read)
            }
        }
    }
}

/// Custom encoder to support the custom format first implemented by zstdmt, which allows to have
/// optional skippable frames.
#[cfg(feature = "compress")]
pub(crate) struct BrotliEncoder<W: Write> {
    inner: InnerWriter<W>,
    quality: u32,
    window: u32,
    buffer_size: usize,
}

#[cfg(feature = "compress")]
enum InnerWriter<W: Write> {
    Standard(RawEncoder<W>),
    Framed {
        writer: W,
        compressor: Option<RawEncoder<Vec<u8>>>,
        frame_size: usize,
        uncompressed_bytes_in_frame: usize,
    },
}

#[cfg(feature = "compress")]
impl<W: Write> BrotliEncoder<W> {
    pub(crate) fn new(
        writer: W,
        quality: u32,
        window: u32,
        frame_size: usize,
    ) -> Result<Self, Error> {
        let buffer_size = 8192;

        let inner = if frame_size == 0 {
            let compressor = encoder(writer, buffer_size, quality, window)?;
            InnerWriter::Standard(compressor)
        } else {
            let compressor = Some(encoder(
                Vec::with_capacity(frame_size),
                buffer_size,
                quality,
                window,
            )?);
            InnerWriter::Framed {
                writer,
                compressor,
                frame_size,
                uncompressed_bytes_in_frame: 0,
            }
        };

        Ok(Self {
            inner,
            quality,
            window,
            buffer_size,
        })
    }

    pub(crate) fn finish(self) -> io::Result<W> {
        match self.inner {
            InnerWriter::Standard(encoder) => finish_encoder(encoder),
            InnerWriter::Framed {
                mut writer,
                compressor,
                uncompressed_bytes_in_frame,
                ..
            } => {
                if uncompressed_bytes_in_frame != 0 {
                    let data = finish_encoder(compressor.expect("open frame"))?;
                    Self::write_frame(&mut writer, &data, uncompressed_bytes_in_frame)?;
                }
                Ok(writer)
            }
        }
    }

    fn write_frame(
        writer: &mut W,
        compressed_data: &[u8],
        uncompressed_bytes: usize,
    ) -> io::Result<()> {
        use crate::ByteWriter;

        if compressed_data.is_empty() {
            return Ok(());
        }

        let packed_size = super::packed_frame_size(compressed_data.len())?;
        writer.write_u32(SKIPPABLE_FRAME_MAGIC)?;
        writer.write_u32(8)?;
        writer.write_u32(packed_size)?;
        writer.write_u16(BROTLI_MAGIC)?;

        let hint_value = uncompressed_bytes.div_ceil(HINT_UNIT_SIZE);
        let hint_value = if hint_value > usize::from(u16::MAX) {
            u16::MAX
        } else {
            hint_value as u16
        };
        writer.write_u16(hint_value)?;

        writer.write_all(compressed_data)?;

        Ok(())
    }
}

#[cfg(feature = "compress")]
impl<W: Write> Write for BrotliEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            InnerWriter::Standard(compressor) => compressor.write(buf),
            InnerWriter::Framed {
                writer,
                compressor,
                frame_size,
                uncompressed_bytes_in_frame,
            } => {
                let mut bytes_consumed = 0;
                let total_bytes = buf.len();

                while bytes_consumed < total_bytes {
                    let comp = compressor.as_mut().expect("no compressor set");

                    let end = std::cmp::min(
                        total_bytes,
                        bytes_consumed + (*frame_size - *uncompressed_bytes_in_frame),
                    );
                    let chunk = &buf[bytes_consumed..end];
                    let bytes_written = comp.write(chunk)?;

                    if bytes_written == 0 && !chunk.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write any bytes",
                        ));
                    }

                    bytes_consumed += bytes_written;
                    *uncompressed_bytes_in_frame += bytes_written;

                    if *uncompressed_bytes_in_frame >= *frame_size {
                        let mut comp = compressor.take().expect("no compressor set");
                        comp.flush()?;
                        let mut data = finish_encoder(comp)?;

                        Self::write_frame(writer, &data, *uncompressed_bytes_in_frame)?;
                        data.clear();

                        *compressor =
                            Some(encoder(data, self.buffer_size, self.quality, self.window)?);

                        *uncompressed_bytes_in_frame = 0;
                    }
                }

                Ok(total_bytes)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            InnerWriter::Standard(compressor) => compressor.flush(),
            InnerWriter::Framed {
                writer,
                compressor,
                uncompressed_bytes_in_frame,
                ..
            } => {
                // As in `finish`: a frame with no input is not written.
                if *uncompressed_bytes_in_frame != 0 {
                    let mut comp = compressor.take().expect("no compressor set");
                    comp.flush()?;
                    let mut data = finish_encoder(comp)?;
                    Self::write_frame(writer, &data, *uncompressed_bytes_in_frame)?;
                    data.clear();
                    *uncompressed_bytes_in_frame = 0;
                    *compressor = Some(encoder(data, self.buffer_size, self.quality, self.window)?);
                }

                writer.flush()
            }
        }
    }
}

#[cfg(feature = "brotli")]
type RawDecoder<R> = brotli::Decompressor<R>;
#[cfg(not(feature = "brotli"))]
type RawDecoder<R> = super::owned_io::Reader<R, mbrotli::DecoderSessionOwned>;

fn decoder<R: Read>(reader: R, buffer_size: usize) -> io::Result<RawDecoder<R>> {
    #[cfg(feature = "brotli")]
    {
        Ok(brotli::Decompressor::new(reader, buffer_size))
    }
    #[cfg(not(feature = "brotli"))]
    {
        let _ = buffer_size;
        let limit = mbrotli::WindowLimit::standard(24).map_err(io::Error::other)?;
        let config = mbrotli::DecoderConfig::new().with_window_limit(limit);
        let session = mbrotli::Decompressor::new(config)
            .map_err(io::Error::other)?
            .into_session(Default::default())
            .map_err(io::Error::other)?;
        Ok(RawDecoder::new(reader, session))
    }
}

#[cfg(all(feature = "compress", feature = "brotli"))]
type RawEncoder<W> = StreamEncoder<W>;

#[cfg(all(feature = "compress", feature = "brotli"))]
use brotli::enc::{
    StandardAlloc,
    encode::{
        BrotliEncoderDestroyInstance, BrotliEncoderOperation, BrotliEncoderParameter,
        BrotliEncoderStateStruct,
    },
};

/// The `brotli` provider's encoder state, released when it is dropped.
#[cfg(all(feature = "compress", feature = "brotli"))]
struct EncoderState(BrotliEncoderStateStruct<StandardAlloc>);

#[cfg(all(feature = "compress", feature = "brotli"))]
impl Drop for EncoderState {
    fn drop(&mut self) {
        BrotliEncoderDestroyInstance(&mut self.0);
    }
}

/// A `brotli` stream written through the encoder state's own
/// `compress_stream`, as `brotli::CompressorWriter` writes one, except that
/// finishing the stream returns what failed. `CompressorWriter::into_inner`
/// drops both a sink error and the encoder's own failure.
#[cfg(all(feature = "compress", feature = "brotli"))]
struct StreamEncoder<W> {
    sink: W,
    state: EncoderState,
    out: Vec<u8>,
    total_out: Option<usize>,
}

#[cfg(all(feature = "compress", feature = "brotli"))]
impl<W: Write> StreamEncoder<W> {
    fn new(sink: W, buffer_size: usize, quality: u32, window: u32) -> Self {
        let mut state = BrotliEncoderStateStruct::new(StandardAlloc::default());
        state.set_parameter(BrotliEncoderParameter::BROTLI_PARAM_QUALITY, quality);
        state.set_parameter(BrotliEncoderParameter::BROTLI_PARAM_LGWIN, window);
        Self {
            sink,
            state: EncoderState(state),
            out: vec![0; if buffer_size == 0 { 4096 } else { buffer_size }],
            total_out: Some(0),
        }
    }

    /// Runs `op` over `input`, writing what it produces to the sink, until
    /// the input is taken (process), the output is drained (flush) or the
    /// stream is finished (finish).
    fn run(&mut self, op: BrotliEncoderOperation, input: &[u8]) -> io::Result<()> {
        let mut no_callback = |_: &mut brotli::enc::interface::PredictionModeContextMap<
            brotli::InputReferenceMut,
        >,
                               _: &mut [brotli::enc::interface::StaticCommand],
                               _: brotli::InputPair,
                               _: &mut StandardAlloc| ();
        let mut avail_in = input.len();
        let mut in_offset = 0;
        loop {
            let mut avail_out = self.out.len();
            let mut out_offset = 0;
            let done = self.state.0.compress_stream(
                op,
                &mut avail_in,
                input,
                &mut in_offset,
                &mut avail_out,
                &mut self.out,
                &mut out_offset,
                &mut self.total_out,
                &mut no_callback,
            );
            write_all(&mut self.sink, &self.out[..out_offset])?;
            if !done {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the Brotli encoder failed",
                ));
            }
            let settled = match op {
                BrotliEncoderOperation::BROTLI_OPERATION_PROCESS => avail_in == 0,
                BrotliEncoderOperation::BROTLI_OPERATION_FLUSH => !self.state.0.has_more_output(),
                _ => self.state.0.is_finished(),
            };
            if settled {
                return Ok(());
            }
        }
    }

    fn finish(mut self) -> io::Result<W> {
        self.run(BrotliEncoderOperation::BROTLI_OPERATION_FINISH, &[])?;
        Ok(self.sink)
    }
}

/// Writes all of `bytes` to `sink`, retrying an interrupted write. A sink
/// that takes nothing is a [`io::ErrorKind::WriteZero`] error.
#[cfg(all(feature = "compress", feature = "brotli"))]
fn write_all<W: Write>(sink: &mut W, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match sink.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => bytes = bytes.get(written..).unwrap_or_default(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(all(feature = "compress", feature = "brotli"))]
impl<W: Write> Write for StreamEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.run(BrotliEncoderOperation::BROTLI_OPERATION_PROCESS, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.run(BrotliEncoderOperation::BROTLI_OPERATION_FLUSH, &[])?;
        self.sink.flush()
    }
}

#[cfg(all(feature = "compress", not(feature = "brotli")))]
type RawEncoder<W> = super::owned_io::Writer<W, mbrotli::EncoderSessionOwned>;

#[cfg(feature = "compress")]
fn encoder<W: Write>(
    sink: W,
    buffer_size: usize,
    quality: u32,
    window: u32,
) -> io::Result<RawEncoder<W>> {
    #[cfg(feature = "brotli")]
    {
        Ok(StreamEncoder::new(sink, buffer_size, quality, window))
    }
    #[cfg(not(feature = "brotli"))]
    {
        let _ = buffer_size;
        let quality = mbrotli::Quality::try_from(quality as u8).map_err(io::Error::other)?;
        let window = mbrotli::Window::standard(window.max(10) as u8).map_err(io::Error::other)?;
        let config = mbrotli::EncoderConfig::default()
            .with_quality(quality)
            .with_window(window);
        let session = mbrotli::Compressor::new(config)
            .map_err(io::Error::other)?
            .into_session(Default::default())
            .map_err(io::Error::other)?;
        Ok(RawEncoder::new(sink, session))
    }
}

#[cfg(feature = "compress")]
fn finish_encoder<W: Write>(encoder: RawEncoder<W>) -> io::Result<W> {
    encoder.finish()
}

#[cfg(all(test, feature = "compress"))]
mod tests {
    use std::{
        cell::Cell,
        io::{self, Write},
        rc::Rc,
    };

    use super::BrotliEncoder;

    /// Records what reaches the archive and fails every write once told to.
    struct Sink {
        bytes: Vec<u8>,
        fail: Rc<Cell<bool>>,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail.get() {
                return Err(io::Error::other("sink is full"));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn sink() -> (Sink, Rc<Cell<bool>>) {
        let fail = Rc::new(Cell::new(false));
        let sink = Sink {
            bytes: Vec::new(),
            fail: fail.clone(),
        };
        (sink, fail)
    }

    #[test]
    fn flushing_an_empty_framed_stream_writes_nothing() {
        let (sink, _) = sink();
        let mut encoder = BrotliEncoder::new(sink, 11, 22, 1 << 16).unwrap();
        encoder.flush().unwrap();
        encoder.flush().unwrap();
        assert!(encoder.finish().unwrap().bytes.is_empty());
    }

    #[test]
    fn flushing_a_framed_stream_writes_its_open_frame_once() {
        let (sink, _) = sink();
        let mut encoder = BrotliEncoder::new(sink, 11, 22, 1 << 16).unwrap();
        encoder.write_all(b"hello").unwrap();
        encoder.flush().unwrap();
        let framed = match &encoder.inner {
            super::InnerWriter::Framed { writer, .. } => writer.bytes.len(),
            super::InnerWriter::Standard(_) => unreachable!("frame size is set"),
        };
        assert_ne!(framed, 0);
        encoder.flush().unwrap();
        assert_eq!(encoder.finish().unwrap().bytes.len(), framed);
    }

    #[test]
    fn a_sink_failure_while_finishing_is_returned() {
        for frame_size in [0, 1 << 16] {
            let (sink, fail) = sink();
            let mut encoder = BrotliEncoder::new(sink, 11, 22, frame_size).unwrap();
            encoder.write_all(b"hello").unwrap();
            fail.set(true);
            let error = encoder
                .finish()
                .err()
                .expect("the stream end was not written");
            assert_eq!(error.to_string(), "sink is full", "frame size {frame_size}");
        }
    }

    /// The stream is the one `brotli::CompressorWriter` writes for the same
    /// writes, flushes and settings.
    #[cfg(feature = "brotli")]
    #[test]
    fn the_stream_matches_the_brotli_writer() {
        let input: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        for (quality, window) in [(0, 18), (1, 20), (5, 22), (11, 22)] {
            let mut ours = super::StreamEncoder::new(Vec::new(), 8192, quality, window);
            let mut theirs = brotli::CompressorWriter::new(Vec::new(), 8192, quality, window);
            for piece in input.chunks(70_000) {
                ours.write_all(piece).unwrap();
                theirs.write_all(piece).unwrap();
                ours.flush().unwrap();
                theirs.flush().unwrap();
            }
            assert!(
                ours.finish().unwrap() == theirs.into_inner(),
                "quality {quality}, window {window}"
            );
        }
    }

    /// The encoder's own failure while finishing is returned, not dropped.
    /// A metadata block left open puts the state where `compress_stream`
    /// refuses to finish.
    #[cfg(feature = "brotli")]
    #[test]
    fn an_encoder_failure_while_finishing_is_returned() {
        use brotli::enc::encode::BrotliEncoderOperation;
        let (sink, _) = sink();
        let mut encoder = super::StreamEncoder::new(sink, 4096, 11, 22);
        let (mut avail_in, mut in_offset, mut avail_out, mut out_offset) = (4, 0, 0, 0);
        let mut no_out = [0u8; 0];
        let mut no_callback = |_: &mut brotli::enc::interface::PredictionModeContextMap<
            brotli::InputReferenceMut,
        >,
                               _: &mut [brotli::enc::interface::StaticCommand],
                               _: brotli::InputPair,
                               _: &mut brotli::enc::StandardAlloc| ();
        assert!(encoder.state.0.compress_stream(
            BrotliEncoderOperation::BROTLI_OPERATION_EMIT_METADATA,
            &mut avail_in,
            b"meta",
            &mut in_offset,
            &mut avail_out,
            &mut no_out,
            &mut out_offset,
            &mut encoder.total_out,
            &mut no_callback,
        ));
        let error = encoder.finish().err().expect("the stream did not finish");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "the Brotli encoder failed");
    }

    /// The encoder's own failure while taking input is returned.
    #[cfg(feature = "brotli")]
    #[test]
    fn an_encoder_failure_while_writing_is_returned() {
        use brotli::enc::encode::BrotliEncoderOperation;
        let (sink, _) = sink();
        let mut encoder = super::StreamEncoder::new(sink, 4096, 11, 22);
        encoder
            .run(BrotliEncoderOperation::BROTLI_OPERATION_FINISH, &[])
            .unwrap();
        let error = encoder
            .write(b"after the end")
            .expect_err("input after the end");
        assert_eq!(error.to_string(), "the Brotli encoder failed");
    }

    /// Hands out one byte per read, after an interrupted read before each.
    struct Trickle {
        bytes: Vec<u8>,
        at: usize,
        interrupt: bool,
    }

    impl io::Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let Some(&byte) = self.bytes.get(self.at) else {
                return Ok(0);
            };
            let Some(first) = buf.first_mut() else {
                return Ok(0);
            };
            *first = byte;
            self.at += 1;
            Ok(1)
        }
    }

    fn encoded(frame_size: usize) -> Vec<u8> {
        let (sink, _) = sink();
        let mut encoder = BrotliEncoder::new(sink, 11, 22, frame_size).unwrap();
        encoder.write_all(b"one byte at a time").unwrap();
        encoder.finish().unwrap().bytes
    }

    #[test]
    fn a_source_that_reads_one_byte_at_a_time_decodes() {
        for frame_size in [0, 1 << 16] {
            let source = Trickle {
                bytes: encoded(frame_size),
                at: 0,
                interrupt: false,
            };
            let mut decoder = super::BrotliDecoder::new(source, 4096).unwrap();
            let mut out = Vec::new();
            io::Read::read_to_end(&mut decoder, &mut out).unwrap();
            assert_eq!(out, b"one byte at a time", "frame size {frame_size}");
        }
    }

    #[test]
    fn a_source_that_ends_inside_the_header_is_too_short() {
        let bytes = encoded(1 << 16);
        for len in 0..4 {
            let source = Trickle {
                bytes: bytes[..len].to_vec(),
                at: 0,
                interrupt: false,
            };
            let error = super::BrotliDecoder::new(source, 4096)
                .err()
                .expect("the header is cut short");
            assert!(
                matches!(error, crate::Error::Corrupted(ref why) if why.contains("too short")),
                "{len} bytes: {error:?}"
            );
        }
    }
}
