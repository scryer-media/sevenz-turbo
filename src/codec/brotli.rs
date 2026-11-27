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
        let header_read = match Read::read(&mut input, &mut header) {
            Ok(n) if n >= 4 => n,
            Ok(_) => return Err(Error::corrupted("Input too short")),
            Err(e) => return Err(e.into()),
        };

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
                let mut comp = compressor.take().expect("no compressor set");
                comp.flush()?;
                let mut data = finish_encoder(comp)?;

                if !data.is_empty() {
                    Self::write_frame(writer, &data, *uncompressed_bytes_in_frame)?;
                    data.clear();
                    *uncompressed_bytes_in_frame = 0;
                }

                *compressor = Some(encoder(data, self.buffer_size, self.quality, self.window)?);

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
type RawEncoder<W> = brotli::CompressorWriter<W>;
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
        Ok(brotli::CompressorWriter::new(
            sink,
            buffer_size,
            quality,
            window,
        ))
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
    #[cfg(feature = "brotli")]
    {
        Ok(encoder.into_inner())
    }
    #[cfg(not(feature = "brotli"))]
    {
        encoder.finish()
    }
}
