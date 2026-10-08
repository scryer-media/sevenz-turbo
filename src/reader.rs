use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs::File,
    io,
    io::{Read, Seek, SeekFrom},
    rc::Rc,
    sync::Arc,
};

use lzma_turbo::crc::{Crc32, crc32};

use crate::{
    ByteReader, Password,
    archive::*,
    bitset::BitSet,
    block::*,
    codec::filter::bcj2::Bcj2Reader,
    codec::lzma_turbo::{Lzma2Control, Lzma2Handle, Lzma2Progress},
    container::{ArchiveLimits, BlockCompletion, SubStreamCompletion},
    decoder::{DecodeOptions, add_decoder, check_chain_memory},
    error::{Error, Limit},
    pipeline::{Chain, Stage},
    positional::{ReadAt, ReadAtCursor},
};

/// Upper bound for eagerly pre-allocating an output buffer from an archive-declared
/// (untrusted) uncompressed size. The buffer still grows to the real size as data is
/// read; this only stops a tiny archive from forcing a huge up-front allocation.
const MAX_PREALLOC_BYTES: usize = 4 << 20;

pub struct BoundedReader<R: Read> {
    inner: R,
    remain: usize,
}

impl<R: Read> BoundedReader<R> {
    pub fn new(inner: R, max_size: usize) -> Self {
        Self {
            inner,
            remain: max_size,
        }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remain == 0 {
            return Ok(0);
        }
        let bound = buf.len().min(self.remain);
        let size = self.inner.read(&mut buf[..bound])?;
        self.remain -= size;
        Ok(size)
    }
}

/// A special reader that shares it's inner reader with other instances and
/// needs to re-seek every read operation.
#[derive(Debug)]
pub(crate) struct SharedBoundedReader<'a, R> {
    inner: Rc<RefCell<&'a mut R>>,
    cur: u64,
    bounds: (u64, u64),
}

impl<'a, R> Clone for SharedBoundedReader<'a, R> {
    fn clone(&self) -> Self {
        Self {
            inner: Rc::clone(&self.inner),
            cur: self.cur,
            bounds: self.bounds,
        }
    }
}

impl<'a, R: Read + Seek> Seek for SharedBoundedReader<'a, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(pos) => self.bounds.0 as i64 + pos as i64,
            SeekFrom::End(pos) => self.bounds.1 as i64 + pos,
            SeekFrom::Current(pos) => self.cur as i64 + pos,
        };
        if new_pos < 0 {
            return Err(io::Error::other("SeekBeforeStart"));
        }
        self.cur = new_pos as u64;
        self.inner.borrow_mut().seek(SeekFrom::Start(self.cur))
    }
}

impl<'a, R: Read + Seek> Read for SharedBoundedReader<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cur >= self.bounds.1 {
            return Ok(0);
        }

        let mut inner = self.inner.borrow_mut();

        inner.seek(SeekFrom::Start(self.cur))?;

        let bound = buf.len().min((self.bounds.1 - self.cur) as usize);
        let size = inner.read(&mut buf[..bound])?;
        self.cur += size as u64;
        Ok(size)
    }
}

impl<'a, R: Read + Seek> SharedBoundedReader<'a, R> {
    fn new(inner: Rc<RefCell<&'a mut R>>, bounds: (u64, u64)) -> Self {
        Self {
            inner,
            cur: bounds.0,
            bounds,
        }
    }
}

/// Wraps the decode chain so that a failure can be told apart from a failure
/// in the caller's own callback.
///
/// An entry is handed to the caller as a `&mut dyn Read`; when the caller then
/// returns an error there is otherwise no way to know whether the archive was
/// damaged or the caller's own sink failed, and only the first of those may be
/// reported as a block failure.
struct FaultRecordingReader<R> {
    inner: R,
    faulted: Rc<Cell<bool>>,
}

impl<R: Read> Read for FaultRecordingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf).inspect_err(|_| self.faulted.set(true))
    }
}

struct Crc32VerifyingReader<R> {
    inner: R,
    crc_digest: Crc32,
    expected_value: u64,
    remaining: i64,
    /// Where to leave the checksum once it is final, for a caller that wants
    /// the value and not only the verdict. See [`SubStreamCompletion`].
    report: Option<Rc<Cell<Option<u32>>>>,
}

impl<R: Read> Crc32VerifyingReader<R> {
    fn new(inner: R, remaining: usize, expected_value: u64) -> Self {
        Self {
            inner,
            crc_digest: Crc32::new(),
            expected_value,
            remaining: remaining as i64,
            report: None,
        }
    }

    /// Same, and hands the computed checksum to `report` when it matches.
    fn reporting(
        inner: R,
        remaining: usize,
        expected_value: u64,
        report: Rc<Cell<Option<u32>>>,
    ) -> Self {
        Self {
            report: Some(report),
            ..Self::new(inner, remaining, expected_value)
        }
    }
}

impl<R: Read> Read for Crc32VerifyingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining <= 0 {
            return Ok(0);
        }
        let size = self.inner.read(buf)?;
        if size > 0 {
            self.remaining -= size as i64;
            self.crc_digest.update(&buf[..size]);
        }
        if self.remaining <= 0 {
            let d = std::mem::replace(&mut self.crc_digest, Crc32::new()).finalize();
            if d as u64 != self.expected_value {
                return Err(std::io::Error::other(Error::ChecksumVerificationFailed));
            }
            if let Some(report) = self.report.as_ref() {
                report.set(Some(d));
            }
        }
        Ok(size)
    }
}

impl Archive {
    /// Open 7z file under specified `path`.
    #[inline]
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Archive, Error> {
        Self::open_with_password(path, &Password::empty())
    }

    /// Open an encrypted 7z file under specified `path` with `password`.
    ///
    /// # Parameters
    /// - `reader`   - the path to the 7z file
    /// - `password` - archive password encoded in utf16 little endian
    #[inline]
    pub fn open_with_password(
        path: impl AsRef<std::path::Path>,
        password: &Password,
    ) -> Result<Archive, Error> {
        let mut file = File::open(path)?;
        Self::read(&mut file, password)
    }

    /// Read 7z file archive info use the specified `reader`.
    ///
    /// # Parameters
    /// - `reader`   - the reader of the 7z filr archive
    /// - `password` - archive password encoded in utf16 little endian
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::{
    ///     fs::File,
    ///     io::{Read, Seek},
    /// };
    ///
    /// use sevenz_turbo::*;
    ///
    /// let mut reader = File::open("example.7z").unwrap();
    ///
    /// let password = Password::from("the password");
    /// let archive = Archive::read(&mut reader, &password).unwrap();
    ///
    /// for entry in &archive.files {
    ///     println!("{}", entry.name());
    /// }
    /// ```
    pub fn read<R: Read + Seek>(reader: &mut R, password: &Password) -> Result<Archive, Error> {
        Self::read_with_limits(reader, password, &ArchiveLimits::default())
    }

    /// Reads the archive's header under `limits`.
    ///
    /// The one limit that has to be applied here rather than by the caller is
    /// [`ArchiveLimits::max_end_header_bytes`]: the declared end-header size is
    /// read out of the archive's first 32 bytes and the header is buffered in
    /// full in order to parse it, so checking the size afterwards is checking
    /// nothing. An archive that declares more is refused with
    /// [`Error::EndHeaderTooLarge`] before the allocation.
    ///
    /// A compressed header is decoded under the rest of `limits`: its declared
    /// size against [`ArchiveLimits::max_header_unpacked_bytes`], and its coder
    /// chain, summed, against [`ArchiveLimits::memory_limit_bytes`], which is
    /// refused with [`Error::MaxMemLimited`] before any coder is built.
    ///
    /// # Errors
    ///
    /// Anything [`Archive::read`] raises, plus [`Error::EndHeaderTooLarge`].
    pub fn read_with_limits<R: Read + Seek>(
        reader: &mut R,
        password: &Password,
        limits: &ArchiveLimits,
    ) -> Result<Archive, Error> {
        let reader_len = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;

        let mut signature = [0; 6];
        reader.read_exact(&mut signature)?;
        if signature != SEVEN_Z_SIGNATURE {
            return Err(Error::BadSignature(signature));
        }
        let mut versions = [0; 2];
        reader.read_exact(&mut versions)?;
        let version_major = versions[0];
        let version_minor = versions[1];
        if version_major != 0 {
            return Err(Error::UnsupportedVersion {
                major: version_major,
                minor: version_minor,
            });
        }

        let start_header_crc = reader.read_u32()?;

        let header_valid = if start_header_crc == 0 {
            let current_position = reader.stream_position()?;
            let mut buf = [0; 20];
            reader.read_exact(&mut buf)?;
            reader.seek(SeekFrom::Start(current_position))?;
            buf.iter().any(|a| *a != 0)
        } else {
            true
        };
        if header_valid {
            let start_header = Self::read_start_header(reader, start_header_crc)?;
            Self::init_archive(
                reader,
                start_header,
                password,
                true,
                &DecodeOptions::header(limits),
            )
        } else {
            Self::try_to_locale_end_header(
                reader,
                reader_len,
                password,
                &DecodeOptions::header(limits),
            )
        }
    }

    fn read_start_header<R: Read>(
        reader: &mut R,
        start_header_crc: u32,
    ) -> Result<StartHeader, Error> {
        let mut buf = [0; 20];
        reader.read_exact(&mut buf)?;
        let crc32 = crc32(&buf);
        if crc32 != start_header_crc {
            return Err(Error::ChecksumVerificationFailed);
        }
        let mut buf_read = buf.as_slice();
        let offset = buf_read.read_u64()?;

        let size = buf_read.read_u64()?;
        let crc = buf_read.read_u32()?;
        Ok(StartHeader {
            next_header_offset: offset,
            next_header_size: size,
            next_header_crc: crc as u64,
        })
    }

    fn read_header<R: Read + Seek>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let mut nid = header.read_u8()?;
        if nid == K_ARCHIVE_PROPERTIES {
            Self::read_archive_properties(header, bounds)?;
            nid = header.read_u8()?;
        }

        if nid == K_ADDITIONAL_STREAMS_INFO {
            return Err(Error::other("Additional streams unsupported"));
        }
        if nid == K_MAIN_STREAMS_INFO {
            Self::read_streams_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        }
        if nid == K_FILES_INFO {
            Self::read_files_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        } else {
            // A header with blocks but no files info: legal enough to parse,
            // and `read_files_info` is where the stream map is normally built.
            // Without this the map stays empty while `blocks` is not, and the
            // first decode indexes a zero-length `block_first_pack_stream_index`
            // — a panic from a header of a dozen bytes.
            Self::calculate_stream_map(archive)?;
        }
        if nid != K_END {
            return Err(Error::BadTerminatedHeader(nid));
        }

        Ok(())
    }

    fn read_archive_properties<R: Read + Seek>(
        header: &mut R,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let mut nid = header.read_u8()?;
        while nid != K_END {
            // Bound the skip length against the buffer: an unbounded value cast to `i64`
            // could go negative and seek backwards, re-reading the same bytes forever.
            let property_size = bounds.size(read_variable_u64(header)?)?;
            header.seek(SeekFrom::Current(property_size as i64))?;
            nid = header.read_u8()?;
        }
        Ok(())
    }

    fn try_to_locale_end_header<R: Read + Seek>(
        reader: &mut R,
        reader_len: u64,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<Self, Error> {
        let search_limit = 1024 * 1024;
        let prev_data_size = reader.stream_position()? + 20;
        let size = reader_len;
        let min_pos = if reader.stream_position()? + search_limit > size {
            reader.stream_position()?
        } else {
            size - search_limit
        };
        let mut pos = reader_len - 1;
        while pos > min_pos {
            pos -= 1;

            reader.seek(SeekFrom::Start(pos))?;
            let nid = reader.read_u8()?;
            if nid == K_ENCODED_HEADER || nid == K_HEADER {
                // `pos` scans down and can fall below `prev_data_size`; skip such candidates
                // instead of underflowing the subtraction.
                let Some(next_header_offset) = pos.checked_sub(prev_data_size) else {
                    continue;
                };
                let start_header = StartHeader {
                    next_header_offset,
                    next_header_size: reader_len - pos,
                    next_header_crc: 0,
                };
                let result = Self::init_archive(reader, start_header, password, false, opts)?;

                if !result.files.is_empty() {
                    return Ok(result);
                }
            }
        }
        Err(Error::other(
            "Start header corrupt and unable to guess end header",
        ))
    }

    fn init_archive<R: Read + Seek>(
        reader: &mut R,
        start_header: StartHeader,
        password: &Password,
        verify_crc: bool,
        opts: &DecodeOptions<'_>,
    ) -> Result<Self, Error> {
        // The caller's own bound comes first: this number is read out of the
        // archive's first 32 bytes and the header below is buffered in full.
        if start_header.next_header_size > opts.limits.max_end_header_bytes {
            return Err(Error::EndHeaderTooLarge {
                limit_bytes: opts.limits.max_end_header_bytes,
                declared_bytes: start_header.next_header_size,
            });
        }

        // Bound the declared next-header size against the actual file length before allocating.
        let reader_len = reader.seek(SeekFrom::End(0))?;
        if start_header.next_header_size > usize::MAX as u64
            || start_header.next_header_size > reader_len
        {
            return Err(Error::other(format!(
                "Cannot handle next_header_size {}",
                start_header.next_header_size
            )));
        }

        let next_header_size_int = start_header.next_header_size as usize;

        // Bound the header position too: `next_header_offset` is an unbounded `u64`, so the
        // addition can overflow (a panic under overflow checks) and any value past the file
        // end is invalid anyway.
        let header_pos = SIGNATURE_HEADER_SIZE
            .checked_add(start_header.next_header_offset)
            .filter(|pos| *pos <= reader_len)
            .ok_or_else(|| Error::other("next header offset out of range"))?;
        reader.seek(SeekFrom::Start(header_pos))?;

        let mut buf = vec![0; next_header_size_int];
        reader.read_exact(&mut buf)?;
        if verify_crc && u64::from(crc32(&buf)) != start_header.next_header_crc {
            return Err(Error::NextHeaderCrcMismatch);
        }

        let mut archive = Archive::default();
        let mut buf_reader = buf.as_slice();
        let mut nid = buf_reader.read_u8()?;
        let mut header = if nid == K_ENCODED_HEADER {
            // A compressed header is one level of nesting: this header, plus the
            // one it decodes to. A caller that allows fewer refuses it outright.
            if opts.limits.max_header_depth < 2 {
                return Err(Error::limit(
                    Limit::HeaderDepth,
                    u64::from(opts.limits.max_header_depth),
                    2,
                ));
            }
            let (mut out_reader, buf_size) = Self::read_encoded_header(
                &mut buf_reader,
                reader,
                &mut archive,
                password,
                HeaderBounds::new(next_header_size_int, opts.limits),
                opts,
            )?;
            buf = read_decoded_header(&mut out_reader, buf_size, password)?;
            archive = Archive::default();
            buf_reader = buf.as_slice();
            nid = buf_reader.read_u8()?;
            buf_reader
        } else {
            buf_reader
        };
        // Upper bound for any header-declared count/size: it can never exceed the number
        // of bytes in the header buffer, since every counted element consumes at least
        // one header byte. This kills the "tiny file declares a huge count" OOM vector
        // without rejecting any legitimate archive.
        let header_len_bound = header.len();
        let mut header = std::io::Cursor::new(&mut header);
        if nid == K_HEADER {
            Self::read_header(
                &mut header,
                &mut archive,
                HeaderBounds::new(header_len_bound, opts.limits),
            )?;
        } else if nid == K_ENCODED_HEADER {
            // A compressed header that decodes to another one. Nothing writes
            // this, and following it would be a recursion driven by a few bytes
            // of input, so it is refused as the depth limit it is.
            return Err(Error::limit(
                Limit::HeaderDepth,
                u64::from(opts.limits.max_header_depth),
                3,
            ));
        } else {
            return Err(Error::other("Broken or unsupported archive: no Header"));
        }

        // Every packed byte the header describes has to be inside the file. The
        // pack streams are laid out end to end from `pack_pos`, so they cannot
        // overlap or run backwards by construction; what is not implied is that
        // the last one ends inside the archive. Without this, a header can send a
        // decode reading at an arbitrary offset, and a coder can be handed a
        // length that is only ever going to end in a short read part-way through.
        let mut packed_total: u64 = 0;
        for size in &archive.pack_sizes {
            packed_total = packed_total
                .checked_add(*size)
                .ok_or_else(|| Error::other("pack sizes overflow"))?;
        }
        let packed_end = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .and_then(|start| start.checked_add(packed_total))
            .ok_or_else(|| Error::other("pack stream range out of range"))?;
        if packed_end > reader_len {
            return Err(Error::other(format!(
                "pack streams end at {packed_end}, past the end of a {reader_len}-byte archive"
            )));
        }

        // The decompression-bomb bounds. Both are about what the archive says it
        // will produce, which is the only thing that can be known before any of
        // it is produced; the decode itself never writes more than these sizes,
        // because every read is driven by them.
        if opts.limits.max_unpack_bytes < u64::MAX || opts.limits.max_unpack_ratio < u64::MAX {
            let mut unpacked_total: u64 = 0;
            for block in &archive.blocks {
                unpacked_total = unpacked_total
                    .checked_add(block.get_unpack_size())
                    .ok_or_else(|| Error::other("unpacked sizes overflow"))?;
            }
            if unpacked_total > opts.limits.max_unpack_bytes {
                return Err(Error::limit(
                    Limit::UnpackBytes,
                    opts.limits.max_unpack_bytes,
                    unpacked_total,
                ));
            }
            // A ratio needs something to divide by: an archive of nothing but
            // empty files packs to nothing and is not a bomb.
            if opts.limits.max_unpack_ratio < u64::MAX && packed_total > 0 {
                let ratio = unpacked_total / packed_total;
                if ratio > opts.limits.max_unpack_ratio {
                    return Err(Error::limit(
                        Limit::UnpackRatio,
                        opts.limits.max_unpack_ratio,
                        ratio,
                    ));
                }
            }
        }

        // The caller extracts to a directory, so a name that would not stay
        // inside one makes the archive unreadable rather than being reported
        // per entry.
        if opts.limits.reject_unsafe_paths {
            for file in &archive.files {
                if let Some(reason) = crate::archive::unsafe_path_reason(&file.name) {
                    return Err(Error::UnsafeEntryName {
                        name: file.name.clone(),
                        reason,
                    });
                }
            }
        }

        archive.check_aes_work(opts.limits)?;
        archive.is_solid = archive
            .blocks
            .iter()
            .any(|block| block.num_unpack_sub_streams > 1);

        Ok(archive)
    }

    fn read_encoded_header<'r, R: Read, RI: 'r + Read + Seek>(
        header: &mut R,
        reader: &'r mut RI,
        archive: &mut Archive,
        password: &Password,
        bounds: HeaderBounds<'_>,
        opts: &DecodeOptions<'_>,
    ) -> Result<(Box<dyn Read + 'r>, usize), Error> {
        Self::read_streams_info(header, archive, bounds)?;
        archive.check_aes_work(opts.limits)?;
        let block = archive
            .blocks
            .first()
            .ok_or(Error::other("no blocks, can't read encoded header"))?;
        let first_pack_stream_index = 0;
        let block_offset = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .ok_or_else(|| Error::other("pack position out of range"))?;
        if archive.pack_sizes.is_empty() {
            return Err(Error::other("no packed streams, can't read encoded header"));
        }

        reader.seek(SeekFrom::Start(block_offset))?;
        let coder_len = block.coders.len();
        // The decoded header is buffered whole before it can be parsed, and this
        // size is a header-declared number multiplied by whatever the coder can
        // amplify: a kilobyte of packed input may claim to unpack to a terabyte.
        let declared_unpack = block.get_unpack_size();
        if declared_unpack > bounds.limits.max_header_unpacked_bytes {
            return Err(Error::limit(
                Limit::HeaderUnpackedBytes,
                bounds.limits.max_header_unpacked_bytes,
                declared_unpack,
            ));
        }
        let unpack_size = usize::try_from(declared_unpack)
            .map_err(|_| Error::other("encoded header unpack size out of range"))?;
        // Each coder is checked against the memory limit as it is built, which
        // alone lets a chain of them hold the limit several times over; the
        // chain is refused here, as a whole, before any of them allocates.
        let reserved_kb = check_chain_memory(
            block
                .ordered_coder_iter()
                .map(|(index, coder)| (coder, block.get_unpack_size_at_index(index))),
            opts.limits,
            0,
        )?;
        let opts = &opts.reserving(reserved_kb);
        let pack_size = archive.pack_sizes[first_pack_stream_index] as usize;
        let input_reader = BoundedReader::new(reader, pack_size);
        let mut decoder: Box<dyn Read> = Box::new(input_reader);
        let mut decoder = if coder_len > 0 {
            for (index, coder) in block.ordered_coder_iter() {
                if coder.num_in_streams != 1 || coder.num_out_streams != 1 {
                    return Err(Error::other(
                        "Multi input/output stream coders are not yet supported",
                    ));
                }
                let next = add_decoder(
                    decoder,
                    block.get_unpack_size_at_index(index) as usize,
                    coder,
                    password,
                    opts,
                )?;
                decoder = Box::new(next);
            }
            decoder
        } else {
            decoder
        };
        // The block checksum is verified by folding the workers' own segments
        // when the parallel coder computed them; wrapping the stream here as
        // well would put a CRC-32 back on the consuming thread, which is the
        // one place the multi-threaded path must not spend time. See
        // `BlockDecoder::for_each_entries`.
        if block.has_crc && opts.verify_checksums && !opts.folding_checksums() {
            decoder = Box::new(Crc32VerifyingReader::new(decoder, unpack_size, block.crc));
        }

        Ok((decoder, unpack_size))
    }

    fn read_streams_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let mut nid = header.read_u8()?;
        if nid == K_PACK_INFO {
            Self::read_pack_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        }

        if nid == K_UNPACK_INFO {
            Self::read_unpack_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        } else {
            archive.blocks.clear();
        }
        if nid == K_SUB_STREAMS_INFO {
            Self::read_sub_streams_info(header, archive, bounds)?;
            nid = header.read_u8()?;
        }
        if nid != K_END {
            return Err(Error::BadTerminatedStreamsInfo(nid));
        }

        Ok(())
    }

    fn read_files_info<R: Read + Seek>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let num_files = bounds.count(read_variable_u64(header)?, Limit::Entries)?;
        let mut files: Vec<ArchiveEntry> = vec![Default::default(); num_files];

        let mut is_empty_stream: Option<BitSet> = None;
        let mut is_empty_file: Option<BitSet> = None;
        let mut is_anti: Option<BitSet> = None;
        loop {
            let prop_type = header.read_u8()?;
            if prop_type == 0 {
                break;
            }
            let size = read_variable_u64(header)?;
            match prop_type {
                K_EMPTY_STREAM => {
                    is_empty_stream = Some(read_bits(header, num_files)?);
                }
                K_EMPTY_FILE => {
                    let n = if let Some(s) = &is_empty_stream {
                        s.len()
                    } else {
                        return Err(Error::other(
                            "Header format error: kEmptyStream must appear before kEmptyFile",
                        ));
                    };
                    is_empty_file = Some(read_bits(header, n)?);
                }
                K_ANTI => {
                    let n = if let Some(s) = is_empty_stream.as_ref() {
                        s.len()
                    } else {
                        return Err(Error::other(
                            "Header format error: kEmptyStream must appear before kEmptyFile",
                        ));
                    };
                    is_anti = Some(read_bits(header, n)?);
                }
                K_NAME => {
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::other("Not implemented:external != 0"));
                    }
                    // A zero `size` would underflow `size - 1`; reject it explicitly.
                    if size == 0 || (size - 1) & 1 != 0 {
                        return Err(Error::other("file names length invalid"));
                    }

                    let size = bounds.count(size, Limit::TotalNameBytes)?;
                    // let mut names = vec![0u8; size - 1];
                    // header.read_exact(&mut names)?;
                    let names_reader = NamesReader::new(
                        header,
                        size - 1,
                        usize::try_from(bounds.limits.max_name_bytes).unwrap_or(usize::MAX),
                    );

                    let mut next_file = 0;
                    for s in names_reader {
                        // The names blob is an independent length, so it can yield more
                        // names than `num_files`. Bail with an error instead of letting
                        // `files[next_file]` panic with an out-of-bounds index.
                        if next_file >= files.len() {
                            return Err(Error::other("Error parsing file names"));
                        }
                        files[next_file].name = s?;
                        next_file += 1;
                    }

                    if next_file != files.len() {
                        return Err(Error::other("Error parsing file names"));
                    }
                }
                K_C_TIME => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::other(format!(
                            "kCTime Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_creation_date = times_defined.contains(i);
                        if file.has_creation_date {
                            file.creation_date = header.read_u64()?.into();
                        }
                    }
                }
                K_A_TIME => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::other(format!(
                            "kATime Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_access_date = times_defined.contains(i);
                        if file.has_access_date {
                            file.access_date = header.read_u64()?.into();
                        }
                    }
                }
                K_M_TIME => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::other(format!(
                            "kMTime Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_last_modified_date = times_defined.contains(i);
                        if file.has_last_modified_date {
                            file.last_modified_date = header.read_u64()?.into();
                        }
                    }
                }
                K_WIN_ATTRIBUTES => {
                    let times_defined = read_all_or_bits(header, num_files)?;
                    let external = header.read_u8()?;
                    if external != 0 {
                        return Err(Error::other(format!(
                            "kWinAttributes Unimplemented:external={external}"
                        )));
                    }
                    for (i, file) in files.iter_mut().enumerate() {
                        file.has_windows_attributes = times_defined.contains(i);
                        if file.has_windows_attributes {
                            file.windows_attributes = header.read_u32()?;
                        }
                    }
                }
                K_START_POS => return Err(Error::other("kStartPos is unsupported, please report")),
                K_DUMMY => {
                    // Bound the skip against the buffer: an unbounded value cast to `i64`
                    // could go negative and seek backwards, re-reading the same bytes forever.
                    let skip = bounds.size(size)?;
                    header.seek(SeekFrom::Current(skip as i64))?;
                }
                _ => {
                    let skip = bounds.size(size)?;
                    header.seek(SeekFrom::Current(skip as i64))?;
                }
            };
        }

        let mut non_empty_file_counter = 0;
        let mut empty_file_counter = 0;
        for (i, file) in files.iter_mut().enumerate() {
            file.has_stream = is_empty_stream
                .as_ref()
                .map(|s| !s.contains(i))
                .unwrap_or(true);
            if file.has_stream {
                let sub_stream_info = if let Some(s) = archive.sub_streams_info.as_ref() {
                    s
                } else {
                    return Err(Error::other(
                        "Archive contains file with streams but no subStreamsInfo",
                    ));
                };
                file.is_directory = false;
                file.is_anti_item = false;
                // The count of streamed files and `total_unpack_streams` are independent
                // header quantities. Reject a mismatch instead of indexing out of bounds.
                let (Some(&crc), Some(&size)) = (
                    sub_stream_info.crcs.get(non_empty_file_counter),
                    sub_stream_info.unpack_sizes.get(non_empty_file_counter),
                ) else {
                    return Err(Error::other(
                        "Archive declares more streamed files than sub-streams",
                    ));
                };
                file.has_crc = sub_stream_info.has_crc.contains(non_empty_file_counter);
                file.crc = crc;
                file.size = size;
                non_empty_file_counter += 1;
            } else {
                file.is_directory = if let Some(s) = &is_empty_file {
                    !s.contains(empty_file_counter)
                } else {
                    true
                };
                file.is_anti_item = is_anti
                    .as_ref()
                    .map(|s| s.contains(empty_file_counter))
                    .unwrap_or(false);
                file.has_crc = false;
                file.size = 0;
                empty_file_counter += 1;
            }
        }
        archive.files = files;

        Self::calculate_stream_map(archive)?;
        Ok(())
    }

    fn calculate_stream_map(archive: &mut Archive) -> Result<(), Error> {
        let mut stream_map = StreamMap::default();

        let mut next_block_pack_stream_index = 0;
        let num_blocks = archive.blocks.len();
        stream_map.block_first_pack_stream_index = vec![0; num_blocks];
        for i in 0..num_blocks {
            stream_map.block_first_pack_stream_index[i] = next_block_pack_stream_index;
            // A block's pack-stream span `[first .. first + packed_streams.len())` is later
            // used to slice `pack_stream_offsets`/`pack_sizes` in `build_decode_stack{,2}`.
            // Reject a block whose span runs past the available pack streams instead of
            // panicking there.
            next_block_pack_stream_index = next_block_pack_stream_index
                .checked_add(archive.blocks[i].packed_streams.len())
                .ok_or_else(|| Error::other("pack stream index overflow"))?;
            if next_block_pack_stream_index > archive.pack_sizes.len() {
                return Err(Error::other(
                    "block references pack streams beyond the available pack sizes",
                ));
            }
        }

        // `SubStreamsInfo` is indexed by the running sub-stream count over all
        // blocks. Record each block's first index here so `build_decode_stack`
        // does not re-sum the preceding blocks every time it opens one, which
        // made extracting an archive of many non-solid blocks quadratic.
        let mut next_sub_stream_index: usize = 0;
        stream_map.block_first_sub_stream_index = vec![0; num_blocks];
        for i in 0..num_blocks {
            stream_map.block_first_sub_stream_index[i] = next_sub_stream_index;
            next_sub_stream_index = next_sub_stream_index
                .checked_add(archive.blocks[i].num_unpack_sub_streams)
                .ok_or_else(|| Error::other("sub-stream index overflow"))?;
        }

        let mut next_pack_stream_offset: u64 = 0;
        let num_pack_sizes = archive.pack_sizes.len();
        stream_map.pack_stream_offsets = vec![0; num_pack_sizes];
        for i in 0..num_pack_sizes {
            stream_map.pack_stream_offsets[i] = next_pack_stream_offset;
            next_pack_stream_offset = next_pack_stream_offset
                .checked_add(archive.pack_sizes[i])
                .ok_or_else(|| Error::other("pack stream offset overflow"))?;
        }

        stream_map.block_first_file_index = vec![0; num_blocks];
        stream_map.file_block_index = vec![None; archive.files.len()];
        let mut next_block_index = 0;
        let mut next_block_unpack_stream_index = 0;
        for i in 0..archive.files.len() {
            if !archive.files[i].has_stream && next_block_unpack_stream_index == 0 {
                stream_map.file_block_index[i] = None;
                continue;
            }
            if next_block_unpack_stream_index == 0 {
                while next_block_index < archive.blocks.len() {
                    stream_map.block_first_file_index[next_block_index] = i;
                    if archive.blocks[next_block_index].num_unpack_sub_streams > 0 {
                        break;
                    }
                    next_block_index += 1;
                }
                if next_block_index >= archive.blocks.len() {
                    return Err(Error::other("Too few blocks in archive"));
                }
            }
            stream_map.file_block_index[i] = Some(next_block_index);
            if !archive.files[i].has_stream {
                continue;
            }

            // The first file in a block carries the block's compressed size:
            // every pack stream it reads, which for BCJ2 is all four, as the
            // writer reports it when the entry is pushed.
            if stream_map.block_first_file_index[next_block_index] == i {
                let first_pack_stream_index =
                    stream_map.block_first_pack_stream_index[next_block_index];
                let pack_sizes = first_pack_stream_index
                    .checked_add(archive.blocks[next_block_index].packed_streams.len())
                    .and_then(|end| archive.pack_sizes.get(first_pack_stream_index..end))
                    .ok_or_else(|| {
                        Error::other("block references a pack stream index beyond pack_sizes")
                    })?;
                let pack_size = pack_sizes
                    .iter()
                    .try_fold(0u64, |total, &size| total.checked_add(size))
                    .ok_or_else(|| Error::other("block pack size overflow"))?;

                archive.files[i].compressed_size = pack_size;
            }

            next_block_unpack_stream_index += 1;
            if next_block_unpack_stream_index
                >= archive.blocks[next_block_index].num_unpack_sub_streams
            {
                next_block_index += 1;
                next_block_unpack_stream_index = 0;
            }
        }

        // Each block's file span `[first_file .. first_file + num_unpack_sub_streams)` is
        // later iterated directly against `archive.files` (in `BlockDecoder`). The
        // sub-stream count is an independent header quantity, so reject a block that
        // declares more sub-streams than there are files instead of indexing out of bounds.
        for (b, block) in archive.blocks.iter().enumerate() {
            let start = stream_map.block_first_file_index[b];
            let end = start
                .checked_add(block.num_unpack_sub_streams)
                .ok_or_else(|| Error::other("block file span overflow"))?;
            if end > archive.files.len() {
                return Err(Error::other(
                    "block declares more sub-streams than the archive has files",
                ));
            }
        }

        archive.stream_map = stream_map;
        Ok(())
    }

    fn read_pack_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        archive.pack_pos = read_variable_u64(header)?;
        let num_pack_streams = bounds.count(read_variable_u64(header)?, Limit::Entries)?;
        let mut nid = header.read_u8()?;
        if nid == K_SIZE {
            archive.pack_sizes = vec![0u64; num_pack_streams];
            for i in 0..archive.pack_sizes.len() {
                archive.pack_sizes[i] = read_variable_u64(header)?;
            }
            nid = header.read_u8()?;
        }

        if nid == K_CRC {
            archive.pack_crcs_defined = read_all_or_bits(header, num_pack_streams)?;
            archive.pack_crcs = vec![0; num_pack_streams];
            for i in 0..num_pack_streams {
                if archive.pack_crcs_defined.contains(i) {
                    archive.pack_crcs[i] = header.read_u32()? as u64;
                }
            }
            nid = header.read_u8()?;
        }

        if nid != K_END {
            return Err(Error::BadTerminatedPackInfo(nid));
        }

        Ok(())
    }
    fn read_unpack_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        let nid = header.read_u8()?;
        if nid != K_FOLDER {
            return Err(Error::other(format!("Expected kFolder, got {nid}")));
        }
        let num_blocks = bounds.count(read_variable_u64(header)?, Limit::Entries)?;

        // Grow into the count rather than reserving it: a `Block` is a hundred-odd
        // bytes and the count is one, so the reservation is the amplification the
        // byte bound alone does not cover.
        archive.blocks.reserve(num_blocks.min(1024));
        let external = header.read_u8()?;
        if external != 0 {
            return Err(Error::ExternalUnsupported);
        }

        // A block is cheap to declare and a coder is not, so the coders are counted
        // across the whole archive as well as per block.
        let mut total_coders: u64 = 0;
        for _ in 0..num_blocks {
            let block = Self::read_block(header, bounds)?;
            total_coders = total_coders.saturating_add(block.coders.len() as u64);
            if total_coders > bounds.limits.max_total_coders {
                return Err(Error::limit(
                    Limit::TotalCoders,
                    bounds.limits.max_total_coders,
                    total_coders,
                ));
            }
            archive.blocks.push(block);
        }

        let nid = header.read_u8()?;
        if nid != K_CODERS_UNPACK_SIZE {
            return Err(Error::other(format!(
                "Expected kCodersUnpackSize, got {nid}"
            )));
        }

        for block in archive.blocks.iter_mut() {
            // `total_output_streams` is bounded in `read_block`, but clamp the eager
            // reservation to `limit` as well so it can never over-allocate.
            let tos = block.total_output_streams;
            block.unpack_sizes.reserve_exact(tos.min(bounds.bytes));
            for _ in 0..tos {
                block.unpack_sizes.push(read_variable_u64(header)?);
            }
        }

        let mut nid = header.read_u8()?;
        if nid == K_CRC {
            let crcs_defined = read_all_or_bits(header, num_blocks)?;
            for i in 0..num_blocks {
                if crcs_defined.contains(i) {
                    archive.blocks[i].has_crc = true;
                    archive.blocks[i].crc = header.read_u32()? as u64;
                } else {
                    archive.blocks[i].has_crc = false;
                }
            }
            nid = header.read_u8()?;
        }
        if nid != K_END {
            return Err(Error::BadTerminatedUnpackInfo);
        }

        Ok(())
    }

    fn read_sub_streams_info<R: Read>(
        header: &mut R,
        archive: &mut Archive,
        bounds: HeaderBounds<'_>,
    ) -> Result<(), Error> {
        for block in archive.blocks.iter_mut() {
            block.num_unpack_sub_streams = 1;
        }
        let mut total_unpack_streams = archive.blocks.len();

        let mut nid = header.read_u8()?;
        if nid == K_NUM_UNPACK_STREAM {
            total_unpack_streams = 0;
            for block in archive.blocks.iter_mut() {
                let num_streams = bounds.count(read_variable_u64(header)?, Limit::Entries)?;
                block.num_unpack_sub_streams = num_streams;
                // Each sub-stream still consumes header bytes downstream, so the running
                // total stays bounded by `limit`; reject anything larger up front.
                // C: CInArchive::ReadSubStreamsInfo checks the running sum.
                total_unpack_streams = bounds.add_count(total_unpack_streams, num_streams)?;
            }
            nid = header.read_u8()?;
        }

        let mut sub_streams_info = SubStreamsInfo::default();
        sub_streams_info
            .unpack_sizes
            .resize(total_unpack_streams, Default::default());
        sub_streams_info
            .has_crc
            .reserve_len_exact(total_unpack_streams);
        sub_streams_info.crcs = vec![0; total_unpack_streams];

        let mut next_unpack_stream = 0;
        for block in archive.blocks.iter() {
            if block.num_unpack_sub_streams == 0 {
                continue;
            }
            let mut sum: u64 = 0;
            if nid == K_SIZE {
                for _i in 0..block.num_unpack_sub_streams - 1 {
                    let size = read_variable_u64(header)?;
                    sub_streams_info.unpack_sizes[next_unpack_stream] = size;
                    next_unpack_stream += 1;
                    sum = sum
                        .checked_add(size)
                        .ok_or_else(|| Error::other("sub-stream size sum overflow"))?;
                }
            }
            if sum > block.get_unpack_size() {
                return Err(Error::other(
                    "sum of unpack sizes of block exceeds total unpack size",
                ));
            }
            // Calculate the last size from the total minus the sum of N-1 sizes.
            sub_streams_info.unpack_sizes[next_unpack_stream] = block.get_unpack_size() - sum;
            next_unpack_stream += 1;
        }
        if nid == K_SIZE {
            nid = header.read_u8()?;
        }

        let mut num_digests = 0usize;
        for block in archive.blocks.iter() {
            if block.num_unpack_sub_streams != 1 || !block.has_crc {
                num_digests = num_digests
                    .checked_add(block.num_unpack_sub_streams)
                    .filter(|total| *total <= total_unpack_streams)
                    .ok_or_else(|| Error::other("sub-stream digest count out of range"))?;
            }
        }

        if nid == K_CRC {
            let has_missing_crc = read_all_or_bits(header, num_digests)?;
            let mut missing_crcs = vec![0; num_digests];
            for (i, missing_crc) in missing_crcs.iter_mut().enumerate() {
                if has_missing_crc.contains(i) {
                    *missing_crc = header.read_u32()? as u64;
                }
            }
            let mut next_crc = 0;
            let mut next_missing_crc = 0;
            for block in archive.blocks.iter() {
                if block.num_unpack_sub_streams == 1 && block.has_crc {
                    sub_streams_info.has_crc.insert(next_crc);
                    sub_streams_info.crcs[next_crc] = block.crc;
                    next_crc += 1;
                } else {
                    for _i in 0..block.num_unpack_sub_streams {
                        if has_missing_crc.contains(next_missing_crc) {
                            sub_streams_info.has_crc.insert(next_crc);
                        } else {
                            sub_streams_info.has_crc.remove(next_crc);
                        }
                        sub_streams_info.crcs[next_crc] = missing_crcs[next_missing_crc];
                        next_crc += 1;
                        next_missing_crc += 1;
                    }
                }
            }

            nid = header.read_u8()?;
        }

        if nid != K_END {
            return Err(Error::BadTerminatedSubStreamsInfo);
        }

        archive.sub_streams_info = Some(sub_streams_info);
        Ok(())
    }

    fn read_block<R: Read>(header: &mut R, bounds: HeaderBounds<'_>) -> Result<Block, Error> {
        let mut block = Block::default();

        let num_coders = bounds.count(read_variable_u64(header)?, Limit::CodersPerBlock)?;
        let mut coders = Vec::with_capacity(num_coders);
        let mut total_in_streams: u64 = 0;
        let mut total_out_streams: u64 = 0;
        for _i in 0..num_coders {
            let mut coder = Coder::default();
            let bits = header.read_u8()?;
            let id_size = bits & 0xF;
            let is_simple = (bits & 0x10) == 0;
            let has_attributes = (bits & 0x20) != 0;
            let more_alternative_methods = (bits & 0x80) != 0;

            coder.id_size = id_size as usize;

            header.read_exact(coder.decompression_method_id_mut())?;
            if is_simple {
                coder.num_in_streams = 1;
                coder.num_out_streams = 1;
            } else {
                coder.num_in_streams = read_variable_u64(header)?;
                coder.num_out_streams = read_variable_u64(header)?;
            }
            // Each stream is referenced by a bind-pair/packed-stream entry that consumes
            // header bytes, so the totals cannot legitimately exceed `limit`. The counts are
            // unbounded attacker-controlled varints, so the sums are checked: an overflowing
            // addition must be rejected rather than wrap past the bound below.
            total_in_streams = total_in_streams
                .checked_add(coder.num_in_streams)
                .ok_or_else(|| Error::other("coder stream counts exceed available input"))?;
            total_out_streams = total_out_streams
                .checked_add(coder.num_out_streams)
                .ok_or_else(|| Error::other("coder stream counts exceed available input"))?;
            if coder.num_in_streams > bounds.limits.max_streams_per_coder
                || coder.num_out_streams > bounds.limits.max_streams_per_coder
            {
                return Err(Error::limit(
                    Limit::StreamsPerCoder,
                    bounds.limits.max_streams_per_coder,
                    coder.num_in_streams.max(coder.num_out_streams),
                ));
            }
            bounds.count(total_in_streams, Limit::Entries)?;
            bounds.count(total_out_streams, Limit::Entries)?;
            if has_attributes {
                let properties_size = bounds.size(read_variable_u64(header)?)?;
                let mut props = vec![0u8; properties_size];
                header.read_exact(&mut props)?;
                coder.properties = props;
            }
            coders.push(coder);
            // would need to keep looping as above:
            if more_alternative_methods {
                return Err(Error::other(
                    "Alternative methods are unsupported, please report. The reference implementation doesn't support them either.",
                ));
            }
        }
        block.coders = coders;
        let total_in_streams = total_in_streams as usize;
        let total_out_streams = total_out_streams as usize;
        block.total_input_streams = total_in_streams;
        block.total_output_streams = total_out_streams;

        if total_out_streams == 0 {
            return Err(Error::other("Total output streams can't be 0"));
        }
        let num_bind_pairs = total_out_streams - 1;
        let mut bind_pairs = Vec::with_capacity(num_bind_pairs);
        for _ in 0..num_bind_pairs {
            let bp = BindPair {
                in_index: read_variable_u64(header)?,
                out_index: read_variable_u64(header)?,
            };
            // Bind-pair indices are later used to index fixed-size coder arrays and the
            // coder graph. Validate them at parse time so decoding cannot panic on an
            // out-of-range index.
            if bp.in_index >= total_in_streams as u64 || bp.out_index >= total_out_streams as u64 {
                return Err(Error::other("bind pair references an out-of-range stream"));
            }
            bind_pairs.push(bp);
        }
        block.bind_pairs = bind_pairs;

        if total_in_streams < num_bind_pairs {
            return Err(Error::other(
                "Total input streams can't be less than the number of bind pairs",
            ));
        }
        let num_packed_streams = total_in_streams - num_bind_pairs;
        let mut packed_streams = vec![0; num_packed_streams];
        if num_packed_streams == 1 {
            let mut index = u64::MAX;
            for i in 0..total_in_streams {
                if block.find_bind_pair_for_in_stream(i as u64).is_none() {
                    index = i as u64;
                    break;
                }
            }
            if index == u64::MAX {
                return Err(Error::other("Couldn't find stream's bind pair index"));
            }
            packed_streams[0] = index;
        } else {
            for packed_stream in packed_streams.iter_mut() {
                *packed_stream = read_variable_u64(header)?;
            }
        }
        block.packed_streams = packed_streams;

        Self::validate_coder_graph(&block)?;

        Ok(block)
    }

    /// Checks that a block's coders form a decodable graph before anything is
    /// built out of it.
    ///
    /// The bind pairs and packed-stream indices are attacker-controlled numbers
    /// that the decode stack walks as if they were a chain. Three things make
    /// that walk safe, and none of them are implied by the counts alone:
    ///
    /// - every stream index is in range, and no stream is bound or packed twice
    ///   (a stream bound twice leaves another one unbound, which is a second
    ///   "final" output);
    /// - exactly one output stream is unbound — the block's actual output;
    /// - the graph is acyclic. A cycle makes the ordered coder walk revisit
    ///   coders forever, which is an unbounded stack of decoders built from a
    ///   handful of header bytes.
    fn validate_coder_graph(block: &Block) -> Result<(), Error> {
        let total_in = block.total_input_streams;
        let total_out = block.total_output_streams;

        let mut in_bound = vec![false; total_in];
        let mut out_bound = vec![false; total_out];
        for bp in &block.bind_pairs {
            // Ranges were checked as the pairs were read; this is the duplicate check.
            let (i, o) = (bp.in_index as usize, bp.out_index as usize);
            if in_bound[i] || out_bound[o] {
                return Err(Error::other("bind pairs bind a stream twice"));
            }
            in_bound[i] = true;
            out_bound[o] = true;
        }

        for &ps in &block.packed_streams {
            let i = usize::try_from(ps)
                .ok()
                .filter(|i| *i < total_in)
                .ok_or_else(|| Error::other("packed stream references an out-of-range stream"))?;
            if in_bound[i] {
                return Err(Error::other("packed stream is also bound by a bind pair"));
            }
            in_bound[i] = true;
        }
        if in_bound.iter().any(|bound| !bound) {
            return Err(Error::other("block leaves an input stream unaccounted for"));
        }

        let unbound_outputs = out_bound.iter().filter(|bound| !**bound).count();
        if unbound_outputs != 1 {
            return Err(Error::other(
                "block must have exactly one unbound output stream",
            ));
        }

        // Which coder owns each stream index. Both totals are the sums that were
        // bounded as the coders were read.
        let mut coder_of_in = Vec::with_capacity(total_in);
        let mut coder_of_out = Vec::with_capacity(total_out);
        for (ci, coder) in block.coders.iter().enumerate() {
            for _ in 0..coder.num_in_streams {
                coder_of_in.push(ci);
            }
            for _ in 0..coder.num_out_streams {
                coder_of_out.push(ci);
            }
        }
        debug_assert_eq!(coder_of_in.len(), total_in);
        debug_assert_eq!(coder_of_out.len(), total_out);

        // Depth-first from the block's output back through the bind pairs, with
        // the coders on the current path marked: meeting one of them again is a
        // cycle.
        const UNSEEN: u8 = 0;
        const ON_PATH: u8 = 1;
        const DONE: u8 = 2;
        let mut state = vec![UNSEEN; block.coders.len()];
        let final_out = out_bound
            .iter()
            .position(|bound| !*bound)
            .expect("exactly one unbound output");
        let mut stack = vec![(coder_of_out[final_out], 0usize)];
        while let Some((coder_index, step)) = stack.pop() {
            if step == 0 {
                match state[coder_index] {
                    ON_PATH => return Err(Error::other("block's coders form a cycle")),
                    DONE => continue,
                    _ => state[coder_index] = ON_PATH,
                }
            }
            // This coder's input streams, in order; `step` is how many have been walked.
            let first_in = block.coders[..coder_index]
                .iter()
                .map(|c| c.num_in_streams as usize)
                .sum::<usize>();
            let num_in = block.coders[coder_index].num_in_streams as usize;
            if step < num_in {
                stack.push((coder_index, step + 1));
                if let Some(bp) = block.find_bind_pair_for_in_stream((first_in + step) as u64) {
                    stack.push((coder_of_out[bp.out_index as usize], 0));
                }
            } else {
                state[coder_index] = DONE;
            }
        }

        Ok(())
    }
}

/// What the header is allowed to claim, and what it is being read out of.
///
/// Two independent bounds, and every count is checked against both.
///
/// - `bytes` is the header buffer's length. Everything a header describes — a
///   file, a coder, a pack stream, a name — costs at least one byte to
///   describe, so no legitimate count can exceed it. This bound comes from the
///   archive itself and needs no caller.
/// - [`ArchiveLimits`] is what the caller will believe. The byte bound alone
///   is not enough: a count is one byte and the entry it reserves is tens or
///   hundreds, so a 64 MiB header could still ask for gigabytes of `Vec`.
#[derive(Clone, Copy)]
struct HeaderBounds<'a> {
    bytes: usize,
    limits: &'a ArchiveLimits,
}

impl<'a> HeaderBounds<'a> {
    fn new(bytes: usize, limits: &'a ArchiveLimits) -> Self {
        Self { bytes, limits }
    }

    /// Checks a count of things the header goes on to describe.
    fn count(self, value: u64, what: Limit) -> Result<usize, Error> {
        if value > self.bytes as u64 {
            return Err(Error::limit(Limit::ArchiveBytes, self.bytes as u64, value));
        }
        let cap = match what {
            Limit::CodersPerBlock => self.limits.max_coders_per_block,
            Limit::TotalCoders => self.limits.max_total_coders,
            Limit::TotalNameBytes => self.limits.max_total_name_bytes,
            _ => self.limits.max_entries,
        };
        if value > cap {
            return Err(Error::limit(what, cap, value));
        }
        // Bounded by `self.bytes`, which is a `usize`.
        Ok(value as usize)
    }

    fn add_count(self, total: usize, additional: usize) -> Result<usize, Error> {
        let total = total
            .checked_add(additional)
            .ok_or_else(|| Error::other("total unpack streams overflow"))?;
        self.count(total as u64, Limit::Entries)
    }

    /// Checks a size in bytes that the header goes on to spend on itself.
    fn size(self, value: u64) -> Result<usize, Error> {
        if value > self.bytes as u64 {
            return Err(Error::limit(Limit::ArchiveBytes, self.bytes as u64, value));
        }
        Ok(value as usize)
    }
}

fn read_variable_u64<R: Read>(reader: &mut R) -> io::Result<u64> {
    let first = reader.read_u8()? as u64;
    let mut mask = 0x80_u64;
    let mut value = 0;
    for i in 0..8 {
        if (first & mask) == 0 {
            return Ok(value | ((first & (mask - 1)) << (8 * i)));
        }
        let b = reader.read_u8()? as u64;
        value |= b << (8 * i);
        mask >>= 1;
    }
    Ok(value)
}

fn read_all_or_bits<R: Read>(header: &mut R, size: usize) -> io::Result<BitSet> {
    let all = header.read_u8()?;
    if all != 0 {
        let mut bits = BitSet::with_capacity(size);
        for i in 0..size {
            bits.insert(i);
        }
        Ok(bits)
    } else {
        read_bits(header, size)
    }
}

fn read_bits<R: Read>(header: &mut R, size: usize) -> io::Result<BitSet> {
    let mut bits = BitSet::with_capacity(size);
    let mut mask = 0u32;
    let mut cache = 0u32;
    for i in 0..size {
        if mask == 0 {
            mask = 0x80;
            cache = header.read_u8()? as u32;
        }
        if (cache & mask) != 0 {
            bits.insert(i);
        }
        mask >>= 1;
    }
    Ok(bits)
}

struct NamesReader<'a, R: Read> {
    max_bytes: usize,
    read_bytes: usize,
    /// Longest one name may be, in bytes of UTF-16. A names blob is one
    /// length for all of the names in it, so without this a single name may
    /// be the whole blob — which is not an allocation problem here, but is one
    /// for everything downstream that treats a name as a path.
    max_name_bytes: usize,
    cache: Vec<u16>,
    reader: &'a mut R,
}

impl<'a, R: Read> NamesReader<'a, R> {
    fn new(reader: &'a mut R, max_bytes: usize, max_name_bytes: usize) -> Self {
        Self {
            max_bytes,
            max_name_bytes,
            reader,
            read_bytes: 0,
            cache: Vec::with_capacity(16),
        }
    }
}

impl<R: Read> Iterator for NamesReader<'_, R> {
    type Item = Result<String, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.max_bytes <= self.read_bytes {
            return None;
        }
        self.cache.clear();
        let mut buf = [0; 2];
        while self.read_bytes < self.max_bytes {
            let r = self.reader.read_exact(&mut buf);
            self.read_bytes += 2;
            if let Err(e) = r {
                return Some(Err(e.into()));
            }
            let u = u16::from_le_bytes(buf);
            if u == 0 {
                break;
            }
            if self.cache.len() * 2 >= self.max_name_bytes {
                return Some(Err(Error::limit(
                    Limit::NameBytes,
                    self.max_name_bytes as u64,
                    self.max_name_bytes as u64 + 2,
                )));
            }
            self.cache.push(u);
        }

        Some(String::from_utf16(&self.cache).map_err(|e| Error::other(e.to_string())))
    }
}

#[derive(Copy, Clone)]
struct IndexEntry {
    block_index: Option<usize>,
    file_index: usize,
}

/// Reads a 7z archive file.
pub struct ArchiveReader<R: Read + Seek> {
    source: R,
    archive: Archive,
    password: Password,
    thread_count: u32,
    adaptive_lzma2: bool,
    verify_checksums: bool,
    lzma2: Arc<Lzma2Control>,
    index: HashMap<String, IndexEntry>,
    limits: ArchiveLimits,
    #[allow(clippy::type_complexity)]
    on_block_complete: Option<Box<dyn FnMut(BlockCompletion) + Send>>,
    #[allow(clippy::type_complexity)]
    on_sub_stream_complete: Option<Box<dyn FnMut(SubStreamCompletion) + Send>>,
    /// The same bytes as `source`, read at an offset, for decoding several
    /// folders at once. See [`ArchiveReader::set_positional_source`].
    positional: Option<Arc<dyn ReadAt>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ArchiveReader<File> {
    /// Opens a 7z archive file at the given `path` and creates a [`ArchiveReader`] to read it.
    ///
    /// The reader keeps a second handle on the file as its positional source
    /// (see [`ArchiveReader::set_positional_source`]), so with more than one
    /// thread the folders of a non-solid archive decode in parallel. With the
    /// default of one thread that handle is never read.
    #[inline]
    pub fn open(path: impl AsRef<std::path::Path>, password: Password) -> Result<Self, Error> {
        let file = File::open(path.as_ref())
            .map_err(|e| Error::file_open(e, path.as_ref().to_string_lossy().to_string()))?;
        let positional = file.try_clone().ok();
        let mut reader = Self::new(file, password)?;
        #[cfg(any(unix, windows))]
        if let Some(positional) = positional {
            reader.set_positional_source(positional);
        }
        #[cfg(not(any(unix, windows)))]
        drop(positional);
        Ok(reader)
    }
}

impl<S: ReadAt + 'static> ArchiveReader<ReadAtCursor<S>> {
    /// A reader over a [`ReadAt`] source: a file, bytes in memory, or any
    /// `Read + Seek` behind [`crate::SerialReadAt`].
    ///
    /// The header is parsed through a cursor of the reader's own, and the same
    /// source is the reader's positional source, so with more than one thread
    /// ([`ArchiveReader::set_threads`]) the folders of a non-solid archive
    /// decode in parallel. See [`ArchiveReader::set_positional_source`] for
    /// what that changes and what it does not.
    ///
    /// # Errors
    ///
    /// As [`ArchiveReader::with_limits`].
    pub fn from_read_at(
        source: S,
        password: Password,
        limits: ArchiveLimits,
    ) -> Result<Self, Error> {
        let source = Arc::new(source);
        let mut reader =
            Self::with_limits(ReadAtCursor::new(Arc::clone(&source)), password, limits)?;
        reader.positional = Some(source);
        Ok(reader)
    }
}

impl<R: Read + Seek> ArchiveReader<R> {
    /// Creates a [`ArchiveReader`] to read a 7z archive file from the given `source` reader.
    #[inline]
    pub fn new(source: R, password: Password) -> Result<Self, Error> {
        Self::with_limits(source, password, ArchiveLimits::default())
    }

    /// Creates an [`ArchiveReader`] that refuses an archive exceeding `limits`
    /// *before* allocating for it.
    ///
    /// Two checks, both of which have to happen before an allocation rather
    /// than after one, because both numbers come out of the archive:
    ///
    /// - the declared end-header size, against
    ///   [`ArchiveLimits::max_end_header_bytes`], before the header is
    ///   buffered ([`Error::EndHeaderTooLarge`]);
    /// - [`Archive::decoder_memory_estimate`], against
    ///   [`ArchiveLimits::memory_limit_bytes`], before any decoder is built
    ///   ([`Error::MemoryLimited`]). The same limit then bounds each block's
    ///   coder chain, summed, before it is constructed.
    ///
    /// An archive whose coder chain has no memory model is refused too when a
    /// memory limit is set: a budget that cannot be computed has not been met.
    ///
    /// # Errors
    ///
    /// Anything [`ArchiveReader::new`] raises, plus the two above.
    pub fn with_limits(
        mut source: R,
        password: Password,
        limits: ArchiveLimits,
    ) -> Result<Self, Error> {
        let archive = Archive::read_with_limits(&mut source, &password, &limits)?;

        if limits.memory_limit_bytes < u64::MAX {
            match archive.decoder_memory_estimate() {
                Ok(required_bytes) if required_bytes > limits.memory_limit_bytes => {
                    return Err(Error::MemoryLimited {
                        limit_bytes: limits.memory_limit_bytes,
                        required_bytes,
                    });
                }
                Ok(_) => {}
                Err(unsized_coder) => {
                    return Err(Error::UnsupportedCompressionMethod(
                        unsized_coder.to_string(),
                    ));
                }
            }
        }

        let mut reader = Self {
            source,
            archive,
            password,
            // One thread unless the caller asks for more. A reader that is
            // handed to something with its own idea of how many threads it
            // may use — which is every consumer this fork exists for — must
            // not quietly spawn `available_parallelism()` of them, and must
            // not quietly hold the memory that decoding in parallel needs.
            thread_count: 1,
            adaptive_lzma2: false,
            verify_checksums: true,
            lzma2: Arc::new(Lzma2Control::new(1)),
            index: HashMap::default(),
            limits,
            on_block_complete: None,
            on_sub_stream_complete: None,
            positional: None,
        };

        reader.fill_index();

        Ok(reader)
    }

    /// Gives the reader a positional view of its source, so that the folders
    /// of a non-solid archive can be decoded concurrently.
    ///
    /// `source` must hold the same bytes as the reader's own source; the reader
    /// cannot check that, and decodes whatever it reads. [`ArchiveReader::open`]
    /// and [`ArchiveReader::from_read_at`] set it themselves.
    ///
    /// It changes nothing until the thread count is above one. Then
    /// [`ArchiveReader::for_each_entries`] decodes each run of consecutive
    /// small folders — none larger than 8 MiB unpacked — on worker threads,
    /// as many at once as the thread count allows, each folder with its share
    /// of the threads, while a folder too large to stage is decoded alone on
    /// the calling thread with the whole count, exactly as without a
    /// positional source. Nothing else changes:
    ///
    /// - **Order.** The callback, the sub-stream hook and the block hook are
    ///   all called on the calling thread, in archive order, as on the
    ///   sequential path; only the decoding happens elsewhere.
    /// - **Memory.** At most two staged folders per worker, each at most
    ///   8 MiB, are held between the workers and the callback. Under a memory
    ///   limit the worker count is cut until every worker's decoder plus its
    ///   staging fits the limit, and each folder then decodes on one thread.
    /// - **Checks.** Every member's CRC is verified on the worker that decoded
    ///   it, and a failure is the same located [`Error::BlockDecode`] naming
    ///   the same folder. A worker reads each member in full, so a damaged
    ///   member is reported even if the callback returned without reading it.
    pub fn set_positional_source(&mut self, source: impl ReadAt + 'static) {
        self.positional = Some(Arc::new(source));
    }

    /// [`ArchiveReader::set_positional_source`], for a builder chain.
    #[must_use]
    pub fn with_positional_source(mut self, source: impl ReadAt + 'static) -> Self {
        self.set_positional_source(source);
        self
    }

    /// Drops the positional source: every folder decodes on the calling
    /// thread again, one at a time.
    pub fn clear_positional_source(&mut self) {
        self.positional = None;
    }

    /// Calls `hook` each time a block has been decoded in full and its
    /// checksum, if the archive records one, has been verified.
    ///
    /// A consumer that is streaming output somewhere else uses this to learn
    /// that a region is settled without waiting for the whole archive, and
    /// without inferring it from entry callbacks (an entry boundary is not a
    /// block boundary, and in a solid archive a block's last entry is checked
    /// only when the block's own CRC is).
    ///
    /// The hook never fires for a block that failed: a failure is an error
    /// from the call that was decoding it.
    pub fn set_block_complete_hook(&mut self, hook: impl FnMut(BlockCompletion) + Send + 'static) {
        self.on_block_complete = Some(Box::new(hook));
    }

    /// Drops any hook set by [`ArchiveReader::set_block_complete_hook`].
    pub fn clear_block_complete_hook(&mut self) {
        self.on_block_complete = None;
    }

    /// Calls `hook` as each file's CRC-32 becomes final, with the value.
    ///
    /// The reader already checksums every file it decodes, to check it against
    /// the header; this hands the number over rather than discarding it, so a
    /// consumer can report per-file integrity without reading the bytes a
    /// second time. The hook fires after the entry callback returns for that
    /// file, once per file the archive records a checksum for, in order.
    ///
    /// A failed check never reaches the hook: it is [`Error::BlockDecode`] of
    /// kind [`crate::BlockErrorKind::ChecksumMismatch`] out of
    /// [`ArchiveReader::for_each_entries`].
    pub fn set_sub_stream_complete_hook(
        &mut self,
        hook: impl FnMut(SubStreamCompletion) + Send + 'static,
    ) {
        self.on_sub_stream_complete = Some(Box::new(hook));
    }

    /// Drops any hook set by [`ArchiveReader::set_sub_stream_complete_hook`].
    pub fn clear_sub_stream_complete_hook(&mut self) {
        self.on_sub_stream_complete = None;
    }

    /// The limits this reader was built with.
    #[must_use]
    pub fn limits(&self) -> ArchiveLimits {
        self.limits
    }

    /// A decoder for one block, borrowing this reader rather than consuming it.
    ///
    /// This is what lets a consumer parse the header once and then decode
    /// blocks from the same source: the header pass and the decode pass share
    /// one [`Read`] + [`Seek`], instead of the caller having to hand out a
    /// freshly opened reader per pass because the constructor took ownership.
    /// The decoder carries this reader's thread count and limits.
    ///
    /// # Errors
    ///
    /// [`Error::FileNotFound`] if `block_index` is past the last block.
    pub fn block_decoder(&mut self, block_index: usize) -> Result<BlockDecoder<'_, R>, Error> {
        if block_index >= self.archive.blocks.len() {
            return Err(Error::FileNotFound);
        }
        Ok(BlockDecoder {
            thread_count: self.thread_count,
            adaptive_lzma2: self.adaptive_lzma2,
            verify_checksums: self.verify_checksums,
            lzma2: Arc::clone(&self.lzma2),
            block_index,
            archive: &self.archive,
            password: &self.password,
            source: &mut self.source,
            limits: self.limits,
            on_sub_stream_complete: self
                .on_sub_stream_complete
                .as_mut()
                .map(|hook| &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send + '_)),
        })
    }

    /// The source this reader is reading from.
    ///
    /// Seeking it moves the reader's own cursor; every decode seeks to the
    /// block it wants first, so that is safe between calls, and is how a
    /// caller shares one open file between the header pass and the decode
    /// pass.
    pub fn source_mut(&mut self) -> &mut R {
        &mut self.source
    }

    /// Takes the source back, dropping the reader.
    pub fn into_source(self) -> R {
        self.source
    }

    /// Creates an [`ArchiveReader`] from an existing [`Archive`] instance.
    ///
    /// This is useful when you already have a parsed archive and want to create a reader
    /// without re-parsing the archive structure.
    ///
    /// # Arguments
    /// * `archive` - An existing parsed archive instance
    /// * `source` - The reader providing access to the archive data
    /// * `password` - Password for encrypted archives
    #[inline]
    pub fn from_archive(archive: Archive, source: R, password: Password) -> Self {
        let mut reader = Self {
            source,
            archive,
            password,
            thread_count: 1,
            adaptive_lzma2: false,
            verify_checksums: true,
            lzma2: Arc::new(Lzma2Control::new(1)),
            index: HashMap::default(),
            limits: ArchiveLimits::default(),
            on_block_complete: None,
            on_sub_stream_complete: None,
            positional: None,
        };

        reader.fill_index();

        reader
    }

    /// Sets how many threads one block's LZMA2 coder may decode on, clamped
    /// to `1..=256`.
    ///
    /// **The default is one**, which is the behaviour of every other coder in
    /// this crate and the behaviour this fork had before the parallel path
    /// existed: nothing is spawned, nothing is buffered, and the memory a
    /// decode needs is the dictionary. Upstream `sevenz-rust2` defaults this
    /// to `available_parallelism()`; a library that decides on its own to
    /// occupy every core and to hold a gigabyte while doing it is not a
    /// default a consumer can build a memory budget on, so the fork asks.
    ///
    /// The count takes effect at the next LZMA2 **run boundary**, so changing
    /// it during a decode is allowed and lossless — a run begins with a
    /// dictionary reset, which is exactly where one decoder can hand over to
    /// another. A count of `1` decodes one run at a time: on the calling
    /// thread for a coder built single-threaded, and on one worker for a
    /// coder that can widen, so that a widening is not held up behind a run
    /// decoded inline. Already-spawned workers park on their channel and cost
    /// nothing until it goes back up.
    ///
    /// A block only decodes in parallel at all if its coder was built for it:
    /// see [`ArchiveReader::set_adaptive_lzma2`] for starting at one thread
    /// and widening later.
    pub fn set_threads(&mut self, threads: u32) {
        self.thread_count = threads.clamp(1, 256);
        self.lzma2.set_threads(self.thread_count);
    }

    /// [`ArchiveReader::set_threads`], for a builder chain.
    #[must_use]
    pub fn with_threads(mut self, threads: u32) -> Self {
        self.set_threads(threads);
        self
    }

    /// The thread ceiling currently in force.
    pub fn threads(&self) -> u32 {
        self.thread_count
    }

    /// Upstream's name for [`ArchiveReader::set_threads`].
    pub fn set_thread_count(&mut self, thread_count: u32) {
        self.set_threads(thread_count);
    }

    /// Builds each block's LZMA2 coder so that it *can* widen later, even
    /// while the thread count is one.
    ///
    /// The choice between the plain decoder and the adaptive one is made when
    /// a block's coder is built, because the plain one is what costs nothing
    /// and a caller who never asked for threads must keep paying nothing. A
    /// consumer that intends to chase a download — decode with one thread
    /// while the tail is arriving, widen once a backlog of complete runs has
    /// built up, narrow again — asks for this once, leaves the count at one,
    /// and then drives [`Lzma2Handle::set_threads`] as it goes.
    pub fn set_adaptive_lzma2(&mut self, adaptive: bool) {
        self.adaptive_lzma2 = adaptive;
    }

    /// [`ArchiveReader::set_adaptive_lzma2`], for a builder chain.
    #[must_use]
    pub fn with_adaptive_lzma2(mut self) -> Self {
        self.set_adaptive_lzma2(true);
        self
    }

    /// Whether the header's CRC-32s are checked as the archive is decoded.
    /// On by default, which is what almost every caller wants.
    ///
    /// Turning it off is for a consumer that verifies the bytes by other
    /// means — a PAR2 set over the extracted files, say — and does not want to
    /// pay for the same assurance twice. With it off, a corrupt archive is
    /// decoded into corrupt bytes without complaint: this crate then reports
    /// only what the decoder itself notices, which is far less than a
    /// checksum notices. It exists to be measured against, too: a decode with
    /// it off is the floor the checked decode is compared with.
    pub fn set_verify_checksums(&mut self, verify: bool) {
        self.verify_checksums = verify;
    }

    /// [`ArchiveReader::set_verify_checksums`], for a builder chain.
    #[must_use]
    pub fn with_verify_checksums(mut self, verify: bool) -> Self {
        self.verify_checksums = verify;
        self
    }

    /// A handle on the LZMA2 coder of whichever block is decoding, which can
    /// be held and used while this reader is borrowed by a decode.
    pub fn lzma2_handle(&self) -> Lzma2Handle {
        Lzma2Handle {
            control: Arc::clone(&self.lzma2),
        }
    }

    /// What the LZMA2 coder of the block being decoded is doing right now, or
    /// `None` when no block is decoding through the adaptive path.
    pub fn lzma2_progress(&self) -> Option<Lzma2Progress> {
        self.lzma2.progress()
    }

    fn fill_index(&mut self) {
        for (file_index, file) in self.archive.files.iter().enumerate() {
            let block_index = self.archive.stream_map.file_block_index[file_index];

            self.index.insert(
                file.name.clone(),
                IndexEntry {
                    block_index,
                    file_index,
                },
            );
        }
    }

    /// Returns a reference to the underlying [`Archive`] structure.
    ///
    /// This provides access to the archive metadata including files, blocks,
    /// and compression information.
    #[inline]
    pub fn archive(&self) -> &Archive {
        &self.archive
    }

    fn build_decode_stack<'r>(
        source: &'r mut R,
        archive: &Archive,
        block_index: usize,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<(Box<dyn Read + 'r>, usize), Error> {
        let block = &archive.blocks[block_index];
        crate::container::check_aes_coders(block.coders.iter(), opts.limits)?;
        // The chain as a whole against the memory limit, before any coder is
        // built; see `read_encoded_header`. Under `ArchiveReader::with_limits`
        // this never refuses anything, because `decoder_memory_estimate` has
        // already bounded the same sum from above; it is what bounds a
        // `BlockDecoder` built with limits of its own.
        let reserved_kb = if block.total_input_streams > block.total_output_streams {
            // Every coder of a multi-stream graph is built.
            check_chain_memory(
                block
                    .coders
                    .iter()
                    .enumerate()
                    .map(|(index, coder)| (coder, block.get_unpack_size_at_index(index))),
                opts.limits,
                0,
            )?
        } else {
            // Under the chain, the pack stream's read buffer (below).
            check_chain_memory(
                block
                    .ordered_coder_iter()
                    .map(|(index, coder)| (coder, block.get_unpack_size_at_index(index))),
                opts.limits,
                crate::decoder::INPUT_BUF_SIZE / 1024,
            )?
        };
        let opts = &opts.reserving(reserved_kb);
        if block.total_input_streams > block.total_output_streams {
            return Self::build_decode_stack2(source, archive, block_index, password, opts);
        }
        let first_pack_stream_index = archive.stream_map.block_first_pack_stream_index[block_index];
        let block_offset = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .and_then(|v| {
                v.checked_add(archive.stream_map.pack_stream_offsets[first_pack_stream_index])
            })
            .ok_or_else(|| Error::other("block offset out of range"))?;

        let (mut has_crc, mut crc) = (block.has_crc, block.crc);

        // Single stream blocks might have it's CRC stored in the single substream information.
        // A caller that checks the file itself already checks exactly that
        // CRC over exactly these bytes, so it is borrowed only for the callers
        // whose one check this is.
        if !has_crc
            && !opts.files_verified
            && block.num_unpack_sub_streams == 1
            && let Some(sub_streams_info) = archive.sub_streams_info.as_ref()
        {
            let substream_index = archive.stream_map.block_first_sub_stream_index[block_index];

            // Only when there is a single stream, we can use it's CRC to verify the compressed block data.
            // Multiple streams would contain the CRC of the compressed data for each file in the block.
            if sub_streams_info.has_crc.contains(substream_index) {
                has_crc = true;
                crc = sub_streams_info.crcs[substream_index];
            }
        }

        source.seek(SeekFrom::Start(block_offset))?;
        let pack_size = archive.pack_sizes[first_pack_stream_index] as usize;

        // Buffered at the bottom too, for a chain whose first coder reads
        // whatever its caller asks for: Copy, or delta straight off the pack
        // stream, would otherwise make one read call per caller read, however
        // small. The block re-seeks the source before this, and the bounded
        // reader stops the read-ahead at the pack stream's end, so nothing
        // beyond the block is consumed.
        let block = &archive.blocks[block_index];
        let mut chain = Chain::new(block, opts);
        let mut stage = Stage::Leaf(Box::new(BoundedReader::new(source, pack_size)));
        for (index, coder) in block.ordered_coder_iter() {
            if coder.num_in_streams != 1 || coder.num_out_streams != 1 {
                return Err(Error::unsupported(
                    "Multi input/output stream coders are not supported",
                ));
            }
            stage = chain.add(block, stage, index, password, opts)?;
        }
        let mut decoder = chain.pipeline.here(stage);
        // Read after the coders are built: the LZMA2 coder decides there
        // whether its workers fold the checksums, and when they do, the
        // block's is folded with them rather than taken again here, on the
        // thread delivering the bytes. A caller that said not to verify gets
        // no verifying reader at all.
        if has_crc && opts.verify_checksums && !opts.folding_checksums() {
            decoder = Box::new(Crc32VerifyingReader::new(
                decoder,
                block.get_unpack_size() as usize,
                crc,
            ));
        }

        Ok((decoder, pack_size))
    }

    fn build_decode_stack2<'r>(
        source: &'r mut R,
        archive: &Archive,
        block_index: usize,
        password: &Password,
        opts: &DecodeOptions<'_>,
    ) -> Result<(Box<dyn Read + 'r>, usize), Error> {
        const MAX_CODER_COUNT: usize = 32;
        let block = &archive.blocks[block_index];
        if block.coders.len() > MAX_CODER_COUNT {
            return Err(Error::unsupported(format!(
                "Too many coders: {}",
                block.coders.len()
            )));
        }

        assert!(block.total_input_streams > block.total_output_streams);
        let shared_source = Rc::new(RefCell::new(source));
        let first_pack_stream_index = archive.stream_map.block_first_pack_stream_index[block_index];
        let start_pos = SIGNATURE_HEADER_SIZE
            .checked_add(archive.pack_pos)
            .ok_or_else(|| Error::other("pack position out of range"))?;
        let offsets = &archive.stream_map.pack_stream_offsets[first_pack_stream_index..];

        let mut sources = Vec::with_capacity(block.packed_streams.len());

        for (i, offset) in offsets[..block.packed_streams.len()].iter().enumerate() {
            let pack_pos = start_pos
                .checked_add(*offset)
                .ok_or_else(|| Error::other("pack stream offset out of range"))?;
            let pack_size = archive.pack_sizes[first_pack_stream_index + i];
            let pack_end = pack_pos
                .checked_add(pack_size)
                .ok_or_else(|| Error::other("pack stream size out of range"))?;

            let pack_reader =
                SharedBoundedReader::new(Rc::clone(&shared_source), (pack_pos, pack_end));

            sources.push(pack_reader);
        }

        let mut coder_to_stream_map = [usize::MAX; MAX_CODER_COUNT];

        let mut si = 0;
        for (i, coder) in block.coders.iter().enumerate() {
            coder_to_stream_map[i] = si;
            si += coder.num_in_streams as usize;
        }

        let main_coder_index = {
            let mut coder_used = [false; MAX_CODER_COUNT];
            for bp in block.bind_pairs.iter() {
                // `out_index` is validated `< total_output_streams` at parse time, but the
                // `coder_used` array indexes coders (max `MAX_CODER_COUNT`); reject an
                // index that would fall outside it instead of panicking.
                let out_index = bp.out_index as usize;
                if out_index >= block.coders.len() {
                    return Err(Error::other("bind pair out index exceeds coder count"));
                }
                coder_used[out_index] = true;
            }
            let mut mci = 0;
            for (i, used) in coder_used[..block.coders.len()].iter().enumerate() {
                if !used {
                    mci = i;
                    break;
                }
            }
            mci
        };

        // Build the decoder for the folder's final output by resolving the main coder's
        // output stream. `get_in_stream2` recursively wires up the whole coder graph,
        // so this also handles single-input filters (e.g. Delta) layered on top of a
        // BCJ2 coder's output, not just a bare BCJ2 main coder.
        let mut chain = Chain::new(block, opts);
        let stage = Self::get_in_stream2(
            block,
            &sources,
            &coder_to_stream_map,
            password,
            main_coder_index,
            0,
            opts,
            &mut chain,
        )?;
        let mut decoder = chain.pipeline.here(stage);
        if block.has_crc && opts.verify_checksums {
            decoder = Box::new(Crc32VerifyingReader::new(
                decoder,
                block.get_unpack_size() as usize,
                block.crc,
            ));
        }
        Ok((
            decoder,
            archive.pack_sizes[first_pack_stream_index] as usize,
        ))
    }

    // One more parameter than clippy likes, because the memory limit has to
    // reach `add_decoder` at the bottom of a BCJ2 chain. Bundling upstream's
    // five into a context struct would widen the diff against upstream for no
    // gain.
    #[allow(clippy::too_many_arguments)]
    fn get_in_stream<'r>(
        block: &Block,
        sources: &[SharedBoundedReader<'r, R>],
        coder_to_stream_map: &[usize],
        password: &Password,
        in_stream_index: usize,
        depth: usize,
        opts: &DecodeOptions<'_>,
        chain: &mut Chain<'r>,
    ) -> Result<Stage<'r>, Error>
    where
        R: 'r,
    {
        let index = block
            .packed_streams
            .iter()
            .position(|&i| i == in_stream_index as u64);
        if let Some(index) = index {
            return Ok(Stage::Leaf(Box::new(sources[index].clone())));
        }

        let bp = block
            .find_bind_pair_for_in_stream(in_stream_index as u64)
            .ok_or_else(|| {
                Error::other(format!(
                    "Couldn't find bind pair for stream {in_stream_index}"
                ))
            })?;
        let index = bp.out_index as usize;

        Self::get_in_stream2(
            block,
            sources,
            coder_to_stream_map,
            password,
            index,
            depth,
            opts,
            chain,
        )
    }

    // One more parameter than clippy likes, because the memory limit has to
    // reach `add_decoder` at the bottom of a BCJ2 chain. Bundling upstream's
    // five into a context struct would widen the diff against upstream for no
    // gain.
    #[allow(clippy::too_many_arguments)]
    fn get_in_stream2<'r>(
        block: &Block,
        sources: &[SharedBoundedReader<'r, R>],
        coder_to_stream_map: &[usize],
        password: &Password,
        in_stream_index: usize,
        depth: usize,
        opts: &DecodeOptions<'_>,
        chain: &mut Chain<'r>,
    ) -> Result<Stage<'r>, Error>
    where
        R: 'r,
    {
        // Each coder is visited at most once in an acyclic graph, so a traversal deeper
        // than the coder count means the bind pairs form a cycle. Bail out instead of
        // recursing until the stack overflows (an uncatchable abort).
        if depth > block.coders.len() {
            return Err(Error::other("cyclic coder bind-pair graph"));
        }
        let (Some(coder), Some(&start_index)) = (
            block.coders.get(in_stream_index),
            coder_to_stream_map.get(in_stream_index),
        ) else {
            return Err(Error::other("in_stream_index out of range"));
        };
        if start_index == usize::MAX {
            return Err(Error::other("in_stream_index out of range"));
        }
        let uncompressed_len = *block
            .unpack_sizes
            .get(in_stream_index)
            .ok_or_else(|| Error::other("in_stream_index out of range"))?
            as usize;
        if coder.num_in_streams == 1 {
            let input = Self::get_in_stream(
                block,
                sources,
                coder_to_stream_map,
                password,
                start_index,
                depth + 1,
                opts,
                chain,
            )?;
            return chain.add(block, input, in_stream_index, password, opts);
        }

        // BCJ2 is the only multi-input coder we support. It takes four input streams
        // (main, call, jump and range-coder) and produces a single output stream.
        if coder.encoder_method_id() == EncoderMethod::ID_BCJ2 {
            let num_in_streams = coder.num_in_streams as usize;
            // The BCJ2 decoder indexes exactly four input streams; reject a malformed count
            // up front instead of handing a short/long input list to the upstream decoder.
            if num_in_streams != 4 {
                return Err(Error::other(
                    "BCJ2 coder must declare exactly four input streams",
                ));
            }
            let mut inputs: Vec<Box<dyn Read>> = Vec::with_capacity(num_in_streams);
            for i in start_index..start_index + num_in_streams {
                let input = Self::get_in_stream(
                    block,
                    sources,
                    coder_to_stream_map,
                    password,
                    i,
                    depth + 1,
                    opts,
                    chain,
                )?;
                inputs.push(chain.pipeline.here(input));
            }
            return Ok(Stage::Here(Box::new(Bcj2Reader::new(
                inputs,
                uncompressed_len as u64,
            ))));
        }

        Err(Error::unsupported(format!(
            "Unsupported multi-input coder: {:?}",
            coder.encoder_method_id()
        )))
    }

    /// Takes a closure to decode each files in the archive.
    ///
    /// Attention about solid archive:
    /// When decoding a solid archive, the data to be decompressed depends on the data in front of it,
    /// you cannot simply skip the previous data and only decompress the data in the back.
    pub fn for_each_entries<F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>>(
        &mut self,
        mut each: F,
    ) -> Result<(), Error> {
        for phase in self.folder_phases() {
            match phase {
                FolderPhase::Alone { block } => self.decode_block_alone(block, &mut each)?,
                #[cfg(not(target_arch = "wasm32"))]
                FolderPhase::Parallel {
                    blocks,
                    workers,
                    threads_per_folder,
                } => self.decode_blocks_parallel(blocks, workers, threads_per_folder, &mut each)?,
                // Never planned without threads; walked in order if it were.
                #[cfg(target_arch = "wasm32")]
                FolderPhase::Parallel { blocks, .. } => {
                    for block in blocks {
                        self.decode_block_alone(block, &mut each)?;
                    }
                }
            }
        }
        // decode empty files
        for file_index in 0..self.archive.files.len() {
            let block_index = self.archive.stream_map.file_block_index[file_index];
            if block_index.is_none() {
                let file = &self.archive.files[file_index];
                let empty_reader: &mut dyn Read = &mut ([0u8; 0].as_slice());
                if !each(file, empty_reader)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// How [`ArchiveReader::for_each_entries`] walks the folders: every one
    /// alone on the calling thread, unless there is a positional source and
    /// more than one thread to share among runs of small folders.
    fn folder_phases(&self) -> Vec<FolderPhase> {
        #[cfg(not(target_arch = "wasm32"))]
        if self.positional.is_some() && self.thread_count > 1 {
            let folders = self.archive.blocks.iter().map(|block| {
                let estimate = block.coders.iter().try_fold(0u64, |sum, coder| {
                    crate::container::coder_memory_estimate(coder)
                        .ok()
                        .map(|bytes| sum.saturating_add(bytes))
                });
                (block.get_unpack_size(), estimate.unwrap_or(u64::MAX))
            });
            return plan_folders(folders, self.thread_count, self.limits.memory_limit_bytes);
        }
        (0..self.archive.blocks.len())
            .map(|block| FolderPhase::Alone { block })
            .collect()
    }

    /// One folder, decoded on the calling thread with the reader's own thread
    /// count: the sequential path, unchanged.
    fn decode_block_alone<F>(&mut self, block_index: usize, each: &mut F) -> Result<(), Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>,
    {
        let block_decoder = BlockDecoder {
            thread_count: self.thread_count,
            adaptive_lzma2: self.adaptive_lzma2,
            verify_checksums: self.verify_checksums,
            lzma2: Arc::clone(&self.lzma2),
            block_index,
            archive: &self.archive,
            password: &self.password,
            source: &mut self.source,
            limits: self.limits,
            on_sub_stream_complete: self
                .on_sub_stream_complete
                .as_mut()
                .map(|hook| &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send + '_)),
        };
        let finished = block_decoder.for_each_entries(each)?;
        // Upstream moves on to the next block when a callback returns
        // `false`, and consumers rely on that; only the hook treats it as
        // "this block was not decoded in full".
        Self::block_completed(
            &self.archive,
            self.on_block_complete
                .as_mut()
                .map(|hook| &mut **hook as &mut (dyn FnMut(BlockCompletion) + Send)),
            block_index,
            finished,
        );
        Ok(())
    }

    /// Fires the block hook for a block the caller let run to the end.
    fn block_completed(
        archive: &Archive,
        hook: Option<&mut (dyn FnMut(BlockCompletion) + Send)>,
        block_index: usize,
        finished: bool,
    ) {
        // Only a block the caller let run to the end has been decoded and
        // checked in full; a callback that stopped early leaves the rest of
        // the block unread, and saying otherwise would be a completion claim
        // nobody verified.
        let (Some(hook), true) = (hook, finished) else {
            return;
        };
        let block = &archive.blocks[block_index];
        let sub_streams = archive.block_sub_streams(block_index);
        let crc_verified = block.has_crc
            || (!sub_streams.is_empty()
                && sub_streams
                    .iter()
                    .all(|sub_stream| sub_stream.crc.is_some()));
        hook(BlockCompletion {
            block_index,
            unpacked_size: block.get_unpack_size(),
            crc_verified,
        });
    }

    /// A run of small folders, decoded `workers` at a time from the
    /// positional source and handed to `each` in archive order.
    #[cfg(not(target_arch = "wasm32"))]
    fn decode_blocks_parallel<F>(
        &mut self,
        blocks: std::ops::Range<usize>,
        workers: u32,
        threads_per_folder: u32,
        each: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>,
    {
        let Some(source) = self.positional.clone() else {
            for block in blocks {
                self.decode_block_alone(block, each)?;
            }
            return Ok(());
        };
        let first = blocks.start;
        let workers = workers as usize;
        let worker_side = FolderWorker {
            archive: &self.archive,
            password: &self.password,
            limits: self.limits,
            verify_checksums: self.verify_checksums,
            threads: threads_per_folder,
            source: &source,
        };
        let archive = &self.archive;
        let encrypted = !self.password.is_empty();
        let on_block = &mut self.on_block_complete;
        let on_sub = &mut self.on_sub_stream_complete;
        let outcome = crate::ordered::run(
            blocks.len(),
            workers,
            FOLDER_WINDOW_PER_WORKER * workers,
            FOLDER_STAGE_BYTES as usize,
            |job, tx| worker_side.decode(first + job, tx),
            |job, rx| {
                let block_index = first + job;
                let finished = replay_folder(
                    archive,
                    encrypted,
                    block_index,
                    rx,
                    each,
                    on_sub
                        .as_mut()
                        .map(|hook| &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send)),
                )?;
                Self::block_completed(
                    archive,
                    on_block
                        .as_mut()
                        .map(|hook| &mut **hook as &mut (dyn FnMut(BlockCompletion) + Send)),
                    block_index,
                    finished,
                );
                Ok(())
            },
        );
        match outcome {
            Some(result) => result,
            // Not one worker could be started: decode the run here instead.
            None => {
                for block in blocks {
                    self.decode_block_alone(block, each)?;
                }
                Ok(())
            }
        }
    }

    /// Returns the data of a file with the given path inside the archive.
    ///
    /// # Notice
    /// This function is very inefficient when used with solid archives, since
    /// it needs to decode all data before the actual file.
    pub fn read_file(&mut self, name: &str) -> Result<Vec<u8>, Error> {
        let index_entry = *self.index.get(name).ok_or(Error::FileNotFound)?;
        let file = &self.archive.files[index_entry.file_index];

        if !file.has_stream {
            return Ok(Vec::new());
        }

        let block_index = index_entry
            .block_index
            .ok_or_else(|| Error::other("File has no associated block"))?;

        match self.archive.is_solid {
            true => {
                let mut result = None;
                let target_file_ptr = file as *const _;

                BlockDecoder {
                    thread_count: self.thread_count,
                    adaptive_lzma2: self.adaptive_lzma2,
                    verify_checksums: self.verify_checksums,
                    lzma2: Arc::clone(&self.lzma2),
                    block_index,
                    archive: &self.archive,
                    password: &self.password,
                    source: &mut self.source,
                    limits: self.limits,
                    on_sub_stream_complete: self.on_sub_stream_complete.as_mut().map(|hook| {
                        &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send + '_)
                    }),
                }
                .for_each_entries(&mut |archive_entry, reader| {
                    let mut data =
                        Vec::with_capacity((archive_entry.size as usize).min(MAX_PREALLOC_BYTES));
                    reader.read_to_end(&mut data)?;

                    if std::ptr::eq(archive_entry, target_file_ptr) {
                        result = Some(data);
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                })?;

                result.ok_or(Error::FileNotFound)
            }
            false => {
                let pack_index = self.archive.stream_map.block_first_pack_stream_index[block_index];
                let pack_offset = self.archive.stream_map.pack_stream_offsets[pack_index];
                let block_offset = SIGNATURE_HEADER_SIZE
                    .checked_add(self.archive.pack_pos)
                    .and_then(|v| v.checked_add(pack_offset))
                    .ok_or_else(|| Error::other("block offset out of range"))?;

                self.source.seek(SeekFrom::Start(block_offset))?;

                let opts = DecodeOptions {
                    limits: &self.limits,
                    threads: self.thread_count,
                    adaptive_lzma2: self.adaptive_lzma2,
                    verify_checksums: self.verify_checksums,
                    lzma2_control: Some(&self.lzma2),
                    // One file is read here and its checksum is verified as it
                    // streams past; there are no other boundaries to declare.
                    checksum_splits: &[],
                    // Below, against the file's own CRC.
                    files_verified: true,
                    reserved_kb: 0,
                };
                self.lzma2.set_block_index(block_index);
                let (mut block_reader, _size) = Self::build_decode_stack(
                    &mut self.source,
                    &self.archive,
                    block_index,
                    &self.password,
                    &opts,
                )?;

                let mut data = Vec::with_capacity((file.size as usize).min(MAX_PREALLOC_BYTES));
                let mut decoder: Box<dyn Read> =
                    Box::new(BoundedReader::new(&mut block_reader, file.size as usize));

                if file.has_crc {
                    decoder = Box::new(Crc32VerifyingReader::new(
                        decoder,
                        file.size as usize,
                        file.crc,
                    ));
                }

                decoder.read_to_end(&mut data).map_err(|e| {
                    // A checksum that did not match is reported located and
                    // typed, as `for_each_entries` reports it: under a
                    // password it may be the wrong key decrypting to garbage,
                    // so it stays `Password` there, never repairable damage.
                    let e = Error::from(e);
                    let checksum = e.is_checksum_failure();
                    let e = e.maybe_bad_password(!self.password.is_empty());
                    if checksum {
                        let packed_offset = self
                            .archive
                            .block_pack_streams(block_index)
                            .first()
                            .map_or(0, |range| range.offset);
                        e.in_block(block_index, packed_offset)
                    } else {
                        e
                    }
                })?;

                Ok(data)
            }
        }
    }

    /// Get the compression method(s) used for a specific file in the archive.
    pub fn file_compression_methods(
        &self,
        file_name: &str,
        methods: &mut Vec<EncoderMethod>,
    ) -> Result<(), Error> {
        let index_entry = self.index.get(file_name).ok_or(Error::FileNotFound)?;
        let file = &self.archive.files[index_entry.file_index];

        if !file.has_stream {
            return Ok(());
        }

        let block_index = index_entry
            .block_index
            .ok_or_else(|| Error::other("File has no associated block"))?;

        let block = self
            .archive
            .blocks
            .get(block_index)
            .ok_or_else(|| Error::other("Block not found"))?;

        block
            .coders
            .iter()
            .filter_map(|coder| EncoderMethod::by_id(coder.encoder_method_id()))
            .for_each(|method| {
                methods.push(method);
            });

        Ok(())
    }
}

/// Decoder for a specific block within a 7z archive.
///
/// Provides access to entries within a single compression block and allows
/// decoding files from that block.
pub struct BlockDecoder<'a, R: Read + Seek> {
    thread_count: u32,
    adaptive_lzma2: bool,
    verify_checksums: bool,
    lzma2: Arc<Lzma2Control>,
    block_index: usize,
    archive: &'a Archive,
    password: &'a Password,
    source: &'a mut R,
    limits: ArchiveLimits,
    #[allow(clippy::type_complexity)]
    on_sub_stream_complete: Option<&'a mut (dyn FnMut(SubStreamCompletion) + Send + 'a)>,
}

impl<'a, R: Read + Seek> BlockDecoder<'a, R> {
    /// Creates a new [`BlockDecoder`] for decoding a specific block in the archive.
    ///
    /// # Arguments
    /// * `thread_count` - Number of threads to use for multi-threaded decompression (if supported
    ///   by the codec)
    /// * `block_index` - Index of the block to decode within the archive
    /// * `archive` - Reference to the archive containing the block
    /// * `password` - Password for encrypted blocks
    /// * `source` - Mutable reference to the reader providing archive data
    pub fn new(
        thread_count: u32,
        block_index: usize,
        archive: &'a Archive,
        password: &'a Password,
        source: &'a mut R,
    ) -> Self {
        Self::with_limits(
            thread_count,
            block_index,
            archive,
            password,
            source,
            ArchiveLimits::default(),
        )
    }

    /// Same as [`BlockDecoder::new`], with a bound on what the block's coders
    /// may allocate.
    ///
    /// The header has already been parsed by the time a caller holds a
    /// [`BlockDecoder`], so only the decoder-memory half of [`ArchiveLimits`]
    /// applies here; it bounds the block's coder chain, summed, before any of
    /// it is built ([`Error::MaxMemLimited`], located in the block).
    pub fn with_limits(
        thread_count: u32,
        block_index: usize,
        archive: &'a Archive,
        password: &'a Password,
        source: &'a mut R,
        limits: ArchiveLimits,
    ) -> Self {
        let thread_count = thread_count.clamp(1, 256);
        Self {
            thread_count,
            adaptive_lzma2: false,
            verify_checksums: true,
            lzma2: Arc::new(Lzma2Control::new(thread_count)),
            block_index,
            archive,
            password,
            source,
            limits,
            on_sub_stream_complete: None,
        }
    }

    /// Sets how many threads this block's LZMA2 coder may decode on, clamped
    /// to `1..=256`. See [`ArchiveReader::set_threads`].
    pub fn set_threads(&mut self, threads: u32) {
        self.thread_count = threads.clamp(1, 256);
        self.lzma2.set_threads(self.thread_count);
    }

    /// [`BlockDecoder::set_threads`], for a builder chain.
    #[must_use]
    pub fn with_threads(mut self, threads: u32) -> Self {
        self.set_threads(threads);
        self
    }

    /// Upstream's name for [`BlockDecoder::set_threads`].
    pub fn set_thread_count(&mut self, thread_count: u32) {
        self.set_threads(thread_count);
    }

    /// Builds this block's LZMA2 coder so that it can widen later even while
    /// the thread count is one. See [`ArchiveReader::set_adaptive_lzma2`].
    #[must_use]
    pub fn with_adaptive_lzma2(mut self) -> Self {
        self.adaptive_lzma2 = true;
        self
    }

    /// Whether this block's checksums are checked. See
    /// [`ArchiveReader::set_verify_checksums`].
    #[must_use]
    pub fn with_verify_checksums(mut self, verify: bool) -> Self {
        self.verify_checksums = verify;
        self
    }

    /// A handle on this block's LZMA2 coder, which can be held and used while
    /// the decode is running.
    pub fn lzma2_handle(&self) -> Lzma2Handle {
        Lzma2Handle {
            control: Arc::clone(&self.lzma2),
        }
    }

    /// Returns a slice of archive entries contained in this block.
    ///
    /// The entries are returned in the order they appear in the block.
    pub fn entries(&self) -> &[ArchiveEntry] {
        let start = self.archive.stream_map.block_first_file_index[self.block_index];
        let file_count = self.archive.blocks[self.block_index].num_unpack_sub_streams;
        &self.archive.files[start..(file_count + start)]
    }

    /// Returns the number of entries contained in this block.
    pub fn entry_count(&self) -> usize {
        self.archive.blocks[self.block_index].num_unpack_sub_streams
    }

    /// Takes a closure to decode each files in this block.
    ///
    /// When decoding files in a block, the data to be decompressed depends on the data in front of
    /// it, you cannot simply skip the previous data and only decompress the data in the back.
    ///
    /// Non-solid archives use one block per file and allow more effective decoding of single files.
    pub fn for_each_entries<F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>>(
        self,
        each: &mut F,
    ) -> Result<bool, Error> {
        let Self {
            thread_count,
            adaptive_lzma2,
            verify_checksums,
            lzma2,
            block_index,
            archive,
            password,
            source,
            limits,
            mut on_sub_stream_complete,
        } = self;
        // Where each of this block's files starts in its decoded stream. Handed
        // to the LZMA2 coder so that, if it decodes in parallel, each worker
        // checksums the pieces of its own output between those points, and no
        // CRC-32 is ever computed on the thread delivering the bytes.
        let splits = if verify_checksums {
            Self::file_boundaries(archive, block_index)
        } else {
            Vec::new()
        };
        let opts = DecodeOptions {
            limits: &limits,
            threads: thread_count,
            adaptive_lzma2,
            verify_checksums,
            lzma2_control: Some(&lzma2),
            checksum_splits: &splits,
            // Every file below is checked against its CRC as it is read, on
            // this thread or folded from the workers'.
            files_verified: verify_checksums,
            reserved_kb: 0,
        };
        lzma2.set_block_index(block_index);
        // Where this block's packed bytes start, so a failure below can say
        // which region of the file it was reading.
        let packed_offset = archive
            .block_pack_streams(block_index)
            .first()
            .map_or(0, |range| range.offset);
        let (block_reader, _size) =
            ArchiveReader::build_decode_stack(source, archive, block_index, password, &opts)
                .map_err(|error| error.in_block(block_index, packed_offset))?;
        // Read once, here: the coder is built and has either engaged the
        // parallel path or degraded to the single-threaded one, and it stops
        // reporting as soon as the block finishes.
        let folding = opts.folding_checksums();
        let faulted = Rc::new(Cell::new(false));
        let mut block_reader = FaultRecordingReader {
            inner: block_reader,
            faulted: Rc::clone(&faulted),
        };
        let start = archive.stream_map.block_first_file_index[block_index];
        let file_count = archive.blocks[block_index].num_unpack_sub_streams;

        let first_sub_stream = archive.stream_map.block_first_sub_stream_index[block_index];
        // Where each file starts inside the block's uncompressed stream: the
        // coordinate the parallel decoder's output blocks carry, so a folded
        // checksum and a file are in the same space.
        let mut unpacked_offset = 0u64;
        let mut sub_stream = 0usize;
        let crc_report = Rc::new(Cell::new(None));

        for file_index in start..(file_count + start) {
            let file = &archive.files[file_index];
            if file.has_stream && file.size > 0 {
                let mut decoder: Box<dyn Read> =
                    Box::new(BoundedReader::new(&mut block_reader, file.size as usize));
                if file.has_crc && verify_checksums && !folding {
                    crc_report.set(None);
                    // A file that does not match its CRC is the block's
                    // fault, although the check sits above the recording
                    // reader: it is recorded too, so the failure leaves this
                    // block located and typed as the folded check's does.
                    decoder = Box::new(FaultRecordingReader {
                        inner: Crc32VerifyingReader::reporting(
                            decoder,
                            file.size as usize,
                            file.crc,
                            Rc::clone(&crc_report),
                        ),
                        faulted: Rc::clone(&faulted),
                    });
                }
                let outcome = each(file, &mut decoder)
                    .map_err(|e| e.maybe_bad_password(!self.password.is_empty()))
                    .map_err(|e| {
                        // Only a failure that came out of the decode chain is
                        // this block's fault; the caller's own errors pass
                        // through as they were raised.
                        if faulted.get() {
                            e.in_block(block_index, packed_offset)
                        } else {
                            e
                        }
                    })?;
                if folding {
                    // The workers checksummed this file's bytes as they
                    // produced them; folding the pieces is a few multiplies
                    // over GF(2), and reads none of them back. A range that is
                    // not covered means the callback left bytes unread, which
                    // is not a checksum failure and is not reported as one.
                    if let Some(crc32) = lzma2.folded(unpacked_offset, file.size) {
                        if file.has_crc && u64::from(crc32) != file.crc {
                            return Err(Error::ChecksumVerificationFailed
                                .in_block(block_index, packed_offset));
                        }
                        crc_report.set(Some(crc32));
                    }
                }
                if let (Some(hook), Some(crc32)) =
                    (on_sub_stream_complete.as_deref_mut(), crc_report.take())
                {
                    hook(SubStreamCompletion {
                        block_index,
                        sub_stream_index: first_sub_stream + sub_stream,
                        file_index,
                        unpacked_offset,
                        len: file.size,
                        crc32,
                    });
                }
                unpacked_offset += file.size;
                sub_stream += 1;
                if !outcome {
                    return Ok(false);
                }
            } else {
                if file.has_stream {
                    sub_stream += 1;
                }
                let empty_reader: &mut dyn Read = &mut ([0u8; 0].as_slice());
                if !each(file, empty_reader)? {
                    return Ok(false);
                }
            }
        }
        if folding
            && archive.blocks[block_index].has_crc
            && let Some(crc32) = lzma2.folded(0, archive.blocks[block_index].get_unpack_size())
            && u64::from(crc32) != archive.blocks[block_index].crc
        {
            // The block's own checksum, folded from the same segments: the
            // stream was not wrapped in a verifying reader, because that
            // reader runs on the thread delivering the bytes.
            return Err(Error::ChecksumVerificationFailed.in_block(block_index, packed_offset));
        }
        Ok(true)
    }

    /// The offsets this block's files start at in its decoded stream, without
    /// the leading zero: the split points a parallel LZMA2 coder checksums
    /// between.
    ///
    /// Empty when there is nothing to gain — a block whose only checksum is
    /// its own needs no interior boundary, and one with no checksums at all
    /// needs none either.
    fn file_boundaries(archive: &Archive, block_index: usize) -> Vec<u64> {
        let block = &archive.blocks[block_index];
        // The workers checksum what the LZMA2 coder produces. That is the
        // block's output only when LZMA2 *is* the block's coder: a filter
        // above it — BCJ, delta, BCJ2 — rewrites those bytes on the way out,
        // and a checksum taken underneath it would be of bytes nobody ever
        // sees. Such a block keeps the streaming checksum on the consuming
        // thread, which is where it has to happen, because the filter runs
        // there too.
        if block.coders.len() != 1 {
            return Vec::new();
        }
        let start = archive.stream_map.block_first_file_index[block_index];
        let files = archive
            .files
            .iter()
            .skip(start)
            .take(block.num_unpack_sub_streams);
        let mut offsets = Vec::new();
        let mut at = 0u64;
        let mut wanted = block.has_crc;
        for file in files {
            if file.has_stream && file.size > 0 {
                wanted |= file.has_crc;
                if at != 0 {
                    offsets.push(at);
                }
                at += file.size;
            }
        }
        if !wanted {
            // Nothing in this block is checked, so there is nothing to compute
            // and no reason to make the workers do it.
            return Vec::new();
        }
        if offsets.is_empty() {
            // One stream: its range is the whole block, which a worker
            // checksums without being given any interior point. The plan still
            // has to be asked for, and an offset past the end of the stream is
            // how it is asked for with no point inside it.
            return vec![u64::MAX];
        }
        offsets
    }
}

/// The largest folder, unpacked, that is decoded on a worker and staged for
/// the callback rather than decoded alone on the calling thread.
///
/// Staging is what lets folders decode out of order and still reach the caller
/// in order, and it is bounded by this per folder. A larger folder is decoded
/// alone, with the reader's whole thread count for its own LZMA2 coder, which
/// is where a large folder's parallelism is.
pub(crate) const FOLDER_STAGE_BYTES: u64 = 8 << 20;

/// How many folders each worker may have decoded ahead of the callback: one
/// being decoded and one waiting, so a worker that finishes a folder while the
/// callback is still on an earlier one starts the next instead of idling.
pub(crate) const FOLDER_WINDOW_PER_WORKER: usize = 2;

/// How much of a member a folder worker reads at once and hands over.
#[cfg(not(target_arch = "wasm32"))]
const FOLDER_CHUNK_BYTES: usize = 1 << 20;

/// What a message costs in the stage beyond its bytes, so that a folder of
/// many empty members still fills its stage and waits.
#[cfg(not(target_arch = "wasm32"))]
const FOLDER_MESSAGE_WEIGHT: usize = 64;

/// One step of [`ArchiveReader::for_each_entries`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FolderPhase {
    /// One folder, on the calling thread, with the reader's whole thread count
    /// for its own coder.
    Alone { block: usize },
    /// Consecutive small folders, decoded `workers` at a time, each with
    /// `threads_per_folder` threads for its own coder. `workers *
    /// threads_per_folder` never exceeds the reader's thread count.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Parallel {
        blocks: std::ops::Range<usize>,
        workers: u32,
        threads_per_folder: u32,
    },
}

/// Plans the folders of an archive, given each folder's unpacked size and
/// decoder memory estimate (`u64::MAX` when it has none), for a thread budget
/// of `threads` and a memory limit of `memory_limit_bytes`.
///
/// A folder of at most [`FOLDER_STAGE_BYTES`] can be staged; a run of at
/// least two of them is decoded in parallel, by as many workers as the budget,
/// the run's length and the memory limit allow. Every other folder is decoded
/// alone. With no memory limit, a run that has fewer folders than threads
/// gives each folder an equal share of the rest; with one, each folder decodes
/// on one thread, because a parallel LZMA2 coder sizes what it holds from the
/// limit and several of them would each take the whole of it.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn plan_folders(
    folders: impl Iterator<Item = (u64, u64)>,
    threads: u32,
    memory_limit_bytes: u64,
) -> Vec<FolderPhase> {
    let threads = threads.max(1);
    let mut phases = Vec::new();
    // The open run: where it starts, and the largest decoder in it.
    let mut run: Option<(usize, u64)> = None;
    let close = |phases: &mut Vec<FolderPhase>, start: usize, end: usize, largest: u64| {
        let len = end - start;
        let by_memory = if memory_limit_bytes == u64::MAX {
            u64::MAX
        } else {
            let per_worker = largest
                .saturating_add(FOLDER_STAGE_BYTES.saturating_mul(FOLDER_WINDOW_PER_WORKER as u64));
            memory_limit_bytes / per_worker.max(1)
        };
        let workers = u64::from(threads).min(len as u64).min(by_memory).max(1) as u32;
        if len < 2 || workers < 2 {
            phases.extend((start..end).map(|block| FolderPhase::Alone { block }));
            return;
        }
        let threads_per_folder = if memory_limit_bytes == u64::MAX {
            (threads / workers).max(1)
        } else {
            1
        };
        phases.push(FolderPhase::Parallel {
            blocks: start..end,
            workers,
            threads_per_folder,
        });
    };
    let mut count = 0;
    for (index, (unpacked, estimate)) in folders.enumerate() {
        count = index + 1;
        if unpacked <= FOLDER_STAGE_BYTES {
            let largest = run.map_or(estimate, |(_, largest)| largest.max(estimate));
            run = Some((run.map_or(index, |(start, _)| start), largest));
            continue;
        }
        if let Some((start, largest)) = run.take() {
            close(&mut phases, start, index, largest);
        }
        phases.push(FolderPhase::Alone { block: index });
    }
    if let Some((start, largest)) = run {
        close(&mut phases, start, count, largest);
    }
    phases
}

/// What a folder worker hands the calling thread, in order.
#[cfg(not(target_arch = "wasm32"))]
enum FolderMessage {
    /// The callback is about to be given this file.
    Begin(usize),
    /// The next bytes of the file begun last.
    Data(Vec<u8>),
    /// The file begun last ended cleanly.
    End,
    /// Reading the file begun last failed here, after the bytes before it.
    Fault(io::Error),
    /// A file's checksum became final, for the sub-stream hook.
    Completion(SubStreamCompletion),
    /// The folder is over.
    Done(FolderEnd),
}

/// How a folder worker's decode ended.
#[cfg(not(target_arch = "wasm32"))]
enum FolderEnd {
    /// Every file was decoded and checked.
    Finished,
    /// A file failed partway, and its [`FolderMessage::Fault`] says how.
    Stopped,
    /// The folder failed outside any file: its coders could not be built, or
    /// its own checksum did not match. Already located.
    Failed(Error),
}

/// What every folder worker of one run shares.
#[cfg(not(target_arch = "wasm32"))]
struct FolderWorker<'a> {
    archive: &'a Archive,
    password: &'a Password,
    limits: ArchiveLimits,
    verify_checksums: bool,
    threads: u32,
    source: &'a Arc<dyn ReadAt>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FolderWorker<'_> {
    /// Decodes one folder through the ordinary [`BlockDecoder`] over a cursor
    /// of its own, sending what the callback would have been given.
    fn decode(&self, block_index: usize, tx: &mut crate::ordered::Sender<'_, FolderMessage>) {
        let mut cursor = ReadAtCursor::new(Arc::clone(self.source));
        let completions = std::sync::Mutex::new(Vec::new());
        let mut hook = |completion: SubStreamCompletion| {
            if let Ok(mut pending) = completions.lock() {
                pending.push(completion);
            }
        };
        let mut decoder = BlockDecoder::with_limits(
            self.threads,
            block_index,
            self.archive,
            self.password,
            &mut cursor,
            self.limits,
        )
        .with_verify_checksums(self.verify_checksums);
        decoder.on_sub_stream_complete = Some(&mut hook);

        let first_file = self.archive.stream_map.block_first_file_index[block_index];
        let mut next_file = first_file;
        let mut stopped = false;
        let mut cancelled = false;
        // The hook fires after the callback returns for a file, so what it
        // left is sent before the next file begins: the order the caller's
        // own hook would have seen.
        let flush = |tx: &mut crate::ordered::Sender<'_, FolderMessage>| {
            let pending = std::mem::take(&mut *completions.lock().map_err(|_| ())?);
            for completion in pending {
                tx.send(FolderMessage::Completion(completion), FOLDER_MESSAGE_WEIGHT)
                    .map_err(|_| ())?;
            }
            Ok::<(), ()>(())
        };
        let result = decoder.for_each_entries(&mut |entry, rd| {
            let file_index = next_file;
            next_file += 1;
            let send = |tx: &mut crate::ordered::Sender<'_, FolderMessage>, message, weight| {
                tx.send(message, weight)
                    .map_err(|_| Error::other("folder decode cancelled"))
            };
            if flush(tx).is_err() {
                cancelled = true;
                return Err(Error::other("folder decode cancelled"));
            }
            send(tx, FolderMessage::Begin(file_index), FOLDER_MESSAGE_WEIGHT)
                .inspect_err(|_| cancelled = true)?;
            let mut left = if entry.has_stream { entry.size } else { 0 };
            while left > 0 {
                let mut chunk = vec![0u8; left.min(FOLDER_CHUNK_BYTES as u64) as usize];
                let mut filled = 0;
                let mut fault = None;
                while filled < chunk.len() {
                    match rd.read(&mut chunk[filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            fault = Some(e);
                            break;
                        }
                    }
                }
                chunk.truncate(filled);
                left -= filled as u64;
                if !chunk.is_empty() {
                    let weight = chunk.len() + FOLDER_MESSAGE_WEIGHT;
                    send(tx, FolderMessage::Data(chunk), weight)
                        .inspect_err(|_| cancelled = true)?;
                }
                if let Some(e) = fault {
                    stopped = true;
                    send(tx, FolderMessage::Fault(e), FOLDER_MESSAGE_WEIGHT)
                        .inspect_err(|_| cancelled = true)?;
                    return Err(Error::other("folder decode stopped"));
                }
                if filled == 0 {
                    // The stream ended early: the callback would have seen the
                    // end here too.
                    break;
                }
            }
            send(tx, FolderMessage::End, FOLDER_MESSAGE_WEIGHT)
                .inspect_err(|_| cancelled = true)?;
            Ok(true)
        });
        if cancelled || flush(tx).is_err() {
            return;
        }
        let end = match result {
            _ if stopped => FolderEnd::Stopped,
            Ok(_) => FolderEnd::Finished,
            Err(error) => FolderEnd::Failed(error),
        };
        let _ = tx.send(FolderMessage::Done(end), FOLDER_MESSAGE_WEIGHT);
    }
}

/// Hands one folder a worker decoded to `each`, in order, as
/// [`BlockDecoder::for_each_entries`] would have: the same files, the same
/// bytes, the same errors in the same places, and the sub-stream hook after
/// each file. Returns whether the callback let the folder run to its end.
#[cfg(not(target_arch = "wasm32"))]
fn replay_folder<F>(
    archive: &Archive,
    encrypted: bool,
    block_index: usize,
    rx: &mut crate::ordered::Receiver<'_, FolderMessage>,
    each: &mut F,
    mut on_sub_stream_complete: Option<&mut (dyn FnMut(SubStreamCompletion) + Send)>,
) -> Result<bool, Error>
where
    F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>,
{
    let packed_offset = archive
        .block_pack_streams(block_index)
        .first()
        .map_or(0, |range| range.offset);
    let lost = || {
        Error::other("the worker decoding this folder stopped before finishing it")
            .in_block(block_index, packed_offset)
    };
    // A file's failure the callback was handed and did not return, kept so
    // that it is still the folder's error.
    let mut swallowed: Option<(io::ErrorKind, String)> = None;
    loop {
        match rx.recv() {
            None => return Err(lost()),
            Some(FolderMessage::Begin(file_index)) => {
                let file = &archive.files[file_index];
                if !(file.has_stream && file.size > 0) {
                    // An empty file has no bytes to fail on: its errors are the
                    // caller's own, passed through.
                    match rx.recv() {
                        Some(FolderMessage::End) => {}
                        _ => return Err(lost()),
                    }
                    let empty_reader: &mut dyn Read = &mut ([0u8; 0].as_slice());
                    if !each(file, empty_reader)? {
                        return Ok(false);
                    }
                    continue;
                }
                let mut entry = ReplayEntry {
                    rx: &mut *rx,
                    chunk: Vec::new(),
                    pos: 0,
                    state: ReplayState::Open,
                    fault: None,
                };
                let outcome = each(file, &mut entry)
                    .map_err(|e| e.maybe_bad_password(encrypted))
                    .map_err(|e| {
                        // As on the sequential path: only a failure that came
                        // out of the decode is this folder's fault.
                        if entry.state.faulted() {
                            e.in_block(block_index, packed_offset)
                        } else {
                            e
                        }
                    })?;
                let unseen = entry.finish();
                if !outcome {
                    return Ok(false);
                }
                if let Some(error) = unseen {
                    // The worker read the whole file; the callback stopped
                    // short of where it failed. The member is damaged all
                    // the same.
                    return Err(Error::from(error)
                        .maybe_bad_password(encrypted)
                        .in_block(block_index, packed_offset));
                }
                if let Some(fault) = entry.fault.take() {
                    swallowed = Some(fault);
                }
            }
            Some(FolderMessage::Completion(completion)) => {
                if let Some(hook) = on_sub_stream_complete.as_deref_mut() {
                    hook(completion);
                }
            }
            Some(FolderMessage::Done(FolderEnd::Finished)) => return Ok(true),
            Some(FolderMessage::Done(FolderEnd::Failed(error))) => return Err(error),
            Some(FolderMessage::Done(FolderEnd::Stopped)) => {
                let (kind, message) = swallowed
                    .take()
                    .unwrap_or((io::ErrorKind::Other, "folder decode stopped".to_string()));
                return Err(Error::from(io::Error::new(kind, message))
                    .maybe_bad_password(encrypted)
                    .in_block(block_index, packed_offset));
            }
            Some(_) => return Err(lost()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayState {
    Open,
    Ended,
    Faulted,
    Lost,
}

#[cfg(not(target_arch = "wasm32"))]
impl ReplayState {
    fn faulted(self) -> bool {
        matches!(self, Self::Faulted | Self::Lost)
    }
}

/// The `Read` the callback is handed for one file a worker decoded.
#[cfg(not(target_arch = "wasm32"))]
struct ReplayEntry<'r, 'a> {
    rx: &'r mut crate::ordered::Receiver<'a, FolderMessage>,
    chunk: Vec<u8>,
    pos: usize,
    state: ReplayState,
    /// The failure the callback was handed, described, for a later read and
    /// for a callback that does not return it.
    fault: Option<(io::ErrorKind, String)>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ReplayEntry<'_, '_> {
    /// Takes the rest of the file's messages, returning a failure among them
    /// that the callback never read as far as.
    fn finish(&mut self) -> Option<io::Error> {
        if self.state != ReplayState::Open {
            return None;
        }
        loop {
            match self.rx.recv() {
                Some(FolderMessage::Data(_)) => {}
                Some(FolderMessage::End) => {
                    self.state = ReplayState::Ended;
                    return None;
                }
                Some(FolderMessage::Fault(error)) => {
                    self.state = ReplayState::Faulted;
                    return Some(error);
                }
                _ => {
                    self.state = ReplayState::Lost;
                    return Some(io::Error::other(
                        "the worker decoding this folder stopped before finishing it",
                    ));
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Read for ReplayEntry<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.chunk.len() {
                let n = buf.len().min(self.chunk.len() - self.pos);
                buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.state {
                ReplayState::Open => {}
                ReplayState::Ended => return Ok(0),
                ReplayState::Faulted | ReplayState::Lost => {
                    let (kind, message) = self
                        .fault
                        .clone()
                        .unwrap_or((io::ErrorKind::Other, "folder decode failed".to_string()));
                    return Err(io::Error::new(kind, message));
                }
            }
            if buf.is_empty() {
                return Ok(0);
            }
            match self.rx.recv() {
                Some(FolderMessage::Data(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Some(FolderMessage::End) => {
                    self.state = ReplayState::Ended;
                    return Ok(0);
                }
                Some(FolderMessage::Fault(error)) => {
                    self.state = ReplayState::Faulted;
                    self.fault = Some((error.kind(), error.to_string()));
                    return Err(error);
                }
                _ => {
                    self.state = ReplayState::Lost;
                    let error = io::Error::other(
                        "the worker decoding this folder stopped before finishing it",
                    );
                    self.fault = Some((error.kind(), error.to_string()));
                    return Err(error);
                }
            }
        }
    }
}

/// Reads a decoded header of exactly `declared` bytes into a buffer that never
/// grows past it.
///
/// `declared` has already been bounded by
/// [`ArchiveLimits::max_header_unpacked_bytes`], so the buffer is reserved at
/// that size once, up front, rather than grown by doubling as `read_to_end`
/// would: doubling can leave the buffer's capacity at nearly twice the declared
/// size, which is memory the limit was meant to rule out. The reservation is
/// fallible, so a caller who lifted that limit and was handed a header claiming
/// more than the process can have is refused rather than aborted. Only the
/// pages the decoder actually writes are touched.
fn read_decoded_header(
    decoded: &mut dyn Read,
    declared: usize,
    password: &Password,
) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(declared)
        .map_err(|_| Error::other("encoded header unpack size cannot be allocated"))?;
    // `take` stops the read at the declared size, and a buffer whose length
    // reaches its capacity is probed on the stack before `read_to_end` grows
    // it, so an exact-sized stream ends with the capacity unchanged.
    decoded
        .take(declared as u64)
        .read_to_end(&mut buf)
        .map_err(|e| Error::bad_password(e, !password.is_empty()))?;
    if buf.len() != declared {
        return Err(Error::bad_password(
            io::Error::from(io::ErrorKind::UnexpectedEof),
            !password.is_empty(),
        ));
    }
    Ok(buf)
}

#[cfg(test)]
mod decoded_header_buffer_tests {
    use super::*;

    /// Yields its bytes a few at a time, the way a decoder hands out output.
    struct Trickle<'a>(&'a [u8]);

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.0.len()).min(7);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn capacity_stays_at_the_declared_size() {
        // Sizes either side of the points where doubling from `read_to_end`'s
        // first reservation would have overshot.
        for declared in [1usize, 31, 32, 33, 4096, 8191, 8193, 100_000] {
            let data: Vec<u8> = (0..declared).map(|i| i as u8).collect();
            // A decoder with more to give than the header declares is cut off
            // at the declared size, and still does not grow the buffer.
            let mut longer = data.clone();
            longer.extend_from_slice(&[0xAA; 64]);
            for source in [&data, &longer] {
                let buf = read_decoded_header(&mut Trickle(source), declared, &Password::empty())
                    .unwrap();
                assert_eq!(buf, data);
                assert_eq!(
                    buf.capacity(),
                    declared,
                    "the decoded header buffer must not grow past its declared size"
                );
            }
        }
    }

    #[test]
    fn a_short_decode_is_refused() {
        let err = read_decoded_header(&mut Trickle(&[1, 2, 3]), 4, &Password::empty()).unwrap_err();
        assert!(matches!(err, Error::Io(ref e, _) if e.kind() == io::ErrorKind::UnexpectedEof));
    }
}

#[cfg(test)]
mod count_limit_tests {
    use super::*;

    #[test]
    fn aggregate_counts_check_overflow_and_both_limits() {
        let limits = ArchiveLimits {
            max_entries: 10,
            ..ArchiveLimits::default()
        };
        let bounds = HeaderBounds::new(20, &limits);
        assert_eq!(bounds.add_count(4, 6).unwrap(), 10);
        assert_eq!(
            bounds.add_count(5, 6).unwrap_err().limit_hit(),
            Some(Limit::Entries)
        );
        assert_eq!(
            HeaderBounds::new(8, &limits)
                .add_count(4, 5)
                .unwrap_err()
                .limit_hit(),
            Some(Limit::ArchiveBytes)
        );
        assert!(
            HeaderBounds::new(usize::MAX, &ArchiveLimits::unlimited())
                .add_count(usize::MAX, 1)
                .is_err()
        );
    }
}

#[cfg(all(test, feature = "compress"))]
mod block_checksum_tests {
    use std::io::Cursor;

    use super::*;
    use crate::ArchiveWriter;

    /// A one-file block whose only block checksum is the one it borrows from
    /// its file, with that borrowed copy made wrong while the file's own
    /// stays right. Which of the two a decode trips over says which check
    /// it ran.
    fn one_file_with_a_wrong_borrowed_crc() -> (Cursor<Vec<u8>>, Archive) {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
        writer
            .push_archive_entry(ArchiveEntry::new_file("one"), Some(data.as_slice()))
            .expect("push");
        let mut source = writer.finish().expect("finish");
        let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
        let block = &archive.blocks[0];
        assert!(!block.has_crc && block.num_unpack_sub_streams == 1);
        let info = archive.sub_streams_info.as_mut().expect("sub-streams");
        info.crcs[0] ^= 1;
        (source, archive)
    }

    /// `for_each_entries` checks the file itself, so the block's reader does
    /// not check the same bytes against the same CRC a second time.
    #[test]
    fn a_one_file_block_is_checked_once() {
        let (mut source, archive) = one_file_with_a_wrong_borrowed_crc();
        let password = Password::empty();
        BlockDecoder::new(1, 0, &archive, &password, &mut source)
            .for_each_entries(&mut |_, rd| {
                io::copy(rd, &mut io::sink())?;
                Ok(true)
            })
            .expect("only the file's own checksum is checked, and it is right");
    }

    /// A caller that does not check the file keeps the block's check, which
    /// is then the only one there is.
    #[test]
    fn the_block_check_stays_where_it_is_the_only_one() {
        let (mut source, archive) = one_file_with_a_wrong_borrowed_crc();
        let limits = ArchiveLimits::default();
        let opts = DecodeOptions::header(&limits);
        let (mut rd, _) = ArchiveReader::<Cursor<Vec<u8>>>::build_decode_stack(
            &mut source,
            &archive,
            0,
            &Password::empty(),
            &opts,
        )
        .expect("stack");
        let err = io::copy(&mut rd, &mut io::sink()).expect_err("the borrowed CRC is checked");
        assert!(Error::from(err).is_checksum_failure());
    }
}

#[cfg(test)]
mod folder_plan_tests {
    use super::*;

    /// Archives of varied shapes: unpacked sizes either side of the staging
    /// bound, with and without decoder estimates.
    fn shapes() -> Vec<Vec<(u64, u64)>> {
        let small = |n: usize, estimate: u64| vec![(32 << 10, estimate); n];
        let mut mixed = Vec::new();
        for i in 0..200u64 {
            let unpacked = if i % 37 == 0 {
                64 << 20
            } else {
                (i * 7919) % (9 << 20)
            };
            mixed.push((unpacked, if i % 5 == 0 { u64::MAX } else { 1 << 20 }));
        }
        vec![
            Vec::new(),
            small(1, 1 << 20),
            small(2, 1 << 20),
            small(3, 64 << 20),
            small(8192, 2 << 20),
            vec![(FOLDER_STAGE_BYTES, 0), (FOLDER_STAGE_BYTES + 1, 0), (1, 0)],
            mixed,
        ]
    }

    #[test]
    fn the_plan_never_exceeds_the_thread_budget() {
        let limits = [u64::MAX, 0, 1, 16 << 20, 40 << 20, 1 << 30, 64 << 30];
        for folders in shapes() {
            for threads in [0, 1, 2, 3, 4, 5, 7, 8, 13, 18, 64, 255] {
                for limit in limits {
                    let phases = plan_folders(folders.iter().copied(), threads, limit);
                    let mut next = 0;
                    for phase in &phases {
                        match phase {
                            FolderPhase::Alone { block } => {
                                assert_eq!(*block, next);
                                next += 1;
                            }
                            FolderPhase::Parallel {
                                blocks,
                                workers,
                                threads_per_folder,
                            } => {
                                assert_eq!(blocks.start, next);
                                next = blocks.end;
                                assert!(blocks.len() >= 2);
                                assert!(*workers >= 2 && *threads_per_folder >= 1);
                                assert!(*workers as usize <= blocks.len());
                                assert!(
                                    u64::from(*workers) * u64::from(*threads_per_folder)
                                        <= u64::from(threads.max(1)),
                                    "{workers} x {threads_per_folder} over {threads} threads"
                                );
                                if limit != u64::MAX {
                                    assert_eq!(*threads_per_folder, 1);
                                    let largest =
                                        folders[blocks.clone()].iter().map(|f| f.1).max().unwrap();
                                    let per_worker = largest.saturating_add(
                                        FOLDER_STAGE_BYTES * FOLDER_WINDOW_PER_WORKER as u64,
                                    );
                                    assert!(
                                        u64::from(*workers).saturating_mul(per_worker) <= limit
                                    );
                                }
                                for &(unpacked, _) in &folders[blocks.clone()] {
                                    assert!(unpacked <= FOLDER_STAGE_BYTES);
                                }
                            }
                        }
                    }
                    assert_eq!(next, folders.len(), "every folder planned once, in order");
                    if threads <= 1 {
                        assert!(
                            phases
                                .iter()
                                .all(|p| matches!(p, FolderPhase::Alone { .. })),
                            "one thread decodes every folder alone"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_large_folder_keeps_the_whole_budget() {
        let phases = plan_folders(
            [
                (1, 0),
                (2, 0),
                (FOLDER_STAGE_BYTES + 1, 0),
                (3, 0),
                (4, 0),
                (5, 0),
            ]
            .into_iter(),
            8,
            u64::MAX,
        );
        assert!(matches!(
            phases[..],
            [
                FolderPhase::Parallel { ref blocks, workers: 2, threads_per_folder: 4 },
                FolderPhase::Alone { block: 2 },
                FolderPhase::Parallel { blocks: ref rest, workers: 3, threads_per_folder: 2 },
            ] if *blocks == (0..2) && *rest == (3..6)
        ));
    }
}
