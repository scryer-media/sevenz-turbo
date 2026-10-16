use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Checksum, ChecksumPlan, Lzma2Run, Lzma2RunDecoder, Lzma2RunScanner, LzmaError};

use crate::{
    Archive, ArchiveEntry, ArchiveLimits, CrcFolder, EncoderMethod, Error, Limit, PackStreamRange,
    ReadAt, SubStream, SubStreamCompletion,
};

const INPUT_BYTES: usize = 64 << 10;

/// A worker-computed CRC segment, in folder output coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lzma2RunCrc {
    /// Offset of the segment's first byte.
    pub offset: u64,
    /// Length of the segment.
    pub len: u64,
    /// CRC-32 of these bytes.
    pub crc32: u32,
}

/// Completed decoding of one run. Bytes remain provisional until the folder's
/// header checksums are verified and the caller accepts the current epoch.
#[derive(Debug, Clone)]
pub struct Lzma2RunCompletion {
    /// Run index in scanner order.
    pub index: u64,
    /// Absolute archive range consumed by this run.
    pub packed: PackStreamRange,
    /// First output byte within the folder.
    pub output_offset: u64,
    /// Exact decoded length.
    pub output_len: u64,
    /// CRC segments split at archive member boundaries.
    pub checks: Vec<Lzma2RunCrc>,
}

struct Folder {
    archive: Arc<Archive>,
    source: Arc<dyn ReadAt>,
    block: usize,
    packed: PackStreamRange,
    output_len: u64,
    file_indexes: Vec<usize>,
    streams: Vec<SubStream>,
    prop: u8,
    limit: u64,
    plan: ChecksumPlan,
}

/// A pure LZMA2 folder whose runs can write independently on a caller's pool.
///
/// This is an explicit positional-output mode. Ordered `Read` and `EntryRead`
/// retain their existing decoding and buffering behavior. AES, filters, BCJ2,
/// and other graphs return `None` from the constructor for normal fallback.
/// The caller owns coverage, aggregate memory permits, cancellation, guarded
/// destination creation, and repair epochs; source and sink must reject stale
/// work. No worker threads or destination files are created by this API.
#[derive(Clone)]
pub struct PositionalLzma2Folder(Arc<Folder>);

impl PositionalLzma2Folder {
    /// Admits a parsed folder with one unfiltered LZMA2 coder and one pack stream.
    /// Header encryption does not matter once metadata has been parsed.
    /// Entry/coder counts, output size/ratio and per-run memory are bounded
    /// here. Header/name/path limits must already have been applied while
    /// parsing metadata; destination path guards are still required.
    ///
    /// # Errors
    /// Invalid block indexes, properties, or inconsistent stream bounds fail.
    /// A fallback checksum outside the CRC-32 wire range fails with block context.
    /// Unsupported graphs return `Ok(None)` without reading packed data.
    pub fn new(
        archive: Arc<Archive>,
        source: Arc<dyn ReadAt>,
        block_index: usize,
        limits: ArchiveLimits,
    ) -> Result<Option<Self>, Error> {
        let block = archive
            .blocks
            .get(block_index)
            .ok_or_else(|| Error::invalid_argument("block index out of range"))?;
        if block.coders.len() != 1
            || block.coders[0].encoder_method_id() != EncoderMethod::ID_LZMA2
            || block.total_input_streams() != 1
            || block.total_output_streams() != 1
            || block.coders[0].num_in_streams() != 1
            || block.coders[0].num_out_streams() != 1
            || !block.bind_pairs().is_empty()
            || block.packed_streams().len() != 1
        {
            return Ok(None);
        }
        let properties = block.coders[0].properties();
        if properties.len() != 1 || properties[0] > 40 {
            return Err(Error::corrupted("invalid LZMA2 dictionary property"));
        }
        let first_pack = *archive
            .stream_map
            .block_first_pack_stream_index
            .get(block_index)
            .ok_or_else(|| Error::corrupted("missing block pack mapping"))?;
        let relative = *archive
            .stream_map
            .pack_stream_offsets
            .get(first_pack)
            .ok_or_else(|| Error::corrupted("missing pack offset"))?;
        let size = *archive
            .pack_sizes
            .get(first_pack)
            .ok_or_else(|| Error::corrupted("missing pack size"))?;
        let offset = 32u64
            .checked_add(archive.pack_pos)
            .and_then(|v| v.checked_add(relative))
            .filter(|v| v.checked_add(size).is_some())
            .ok_or_else(|| Error::corrupted("pack range overflow"))?;
        let first_file = *archive
            .stream_map
            .block_first_file_index
            .get(block_index)
            .ok_or_else(|| Error::corrupted("missing block file mapping"))?;
        let file_count = block.num_unpack_sub_streams;
        // Recheck finite aggregate limits against the caller's public metadata.
        // An unlimited caller needs no archive-wide scan on each folder admission.
        if limits.max_total_coders != u64::MAX {
            let total_coders = archive.blocks.iter().fold(0u64, |total, block| {
                total.saturating_add(block.coders.len() as u64)
            });
            if total_coders > limits.max_total_coders {
                return Err(Error::LimitExceeded {
                    what: Limit::TotalCoders,
                    requested: total_coders,
                    limit: limits.max_total_coders,
                });
            }
        }
        for (what, requested, limit) in [
            (
                Limit::Entries,
                archive.files.len() as u64,
                limits.max_entries,
            ),
            (Limit::CodersPerBlock, 1, limits.max_coders_per_block),
            (Limit::StreamsPerCoder, 1, limits.max_streams_per_coder),
        ] {
            if requested > limit {
                return Err(Error::LimitExceeded {
                    what,
                    requested,
                    limit,
                });
            }
        }
        if file_count > archive.files.len().saturating_sub(first_file) {
            return Err(Error::corrupted("sub-stream count exceeds file mapping"));
        }
        let mut file_indexes = Vec::new();
        file_indexes
            .try_reserve_exact(file_count)
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
        for (index, entry) in archive.files.iter().enumerate().skip(first_file) {
            if archive.stream_map.file_block_index.get(index) != Some(&Some(block_index)) {
                break;
            }
            if entry.has_stream {
                if file_indexes.len() == file_count {
                    return Err(Error::corrupted("invalid block file mapping"));
                }
                file_indexes.push(index);
            }
        }
        if file_indexes.len() != file_count {
            return Err(Error::corrupted("invalid block file mapping"));
        }
        let output_len = block.get_unpack_size();
        if output_len > limits.max_unpack_bytes {
            return Err(Error::LimitExceeded {
                what: Limit::UnpackBytes,
                requested: output_len,
                limit: limits.max_unpack_bytes,
            });
        }
        if size != 0 && output_len.div_ceil(size) > limits.max_unpack_ratio {
            return Err(Error::LimitExceeded {
                what: Limit::UnpackRatio,
                requested: output_len.div_ceil(size),
                limit: limits.max_unpack_ratio,
            });
        }
        let mut boundary = 0u64;
        let mut splits = Vec::new();
        splits
            .try_reserve_exact(file_indexes.len())
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
        for &index in &file_indexes {
            let entry = &archive.files[index];
            boundary = boundary
                .checked_add(entry.size)
                .ok_or_else(|| Error::corrupted("file size overflow"))?;
            splits.push(boundary);
        }
        if boundary != output_len || size == 0 {
            return Err(Error::corrupted("inconsistent LZMA2 folder bounds"));
        }
        let mut streams = archive.block_sub_streams(block_index);
        if streams.is_empty() && archive.sub_streams_info.is_none() && file_count == 1 {
            let entry = &archive.files[file_indexes[0]];
            let index = archive
                .stream_map
                .block_first_sub_stream_index
                .get(block_index)
                .ok_or_else(|| Error::corrupted("missing sub-stream mapping"))?;
            let crc = if block.has_crc {
                Some(block.crc)
            } else {
                entry.has_crc.then_some(entry.crc)
            }
            .map(u32::try_from)
            .transpose()
            .map_err(|_| Error::ChecksumVerificationFailed.in_block(block_index, offset))?;
            streams.push(SubStream {
                index,
                size: output_len,
                crc,
            });
        }
        if streams.len() != file_count
            || streams
                .iter()
                .zip(&file_indexes)
                .any(|(stream, &index)| stream.size != archive.files[index].size)
        {
            return Err(Error::corrupted("inconsistent folder sub-stream metadata"));
        }
        let prop = properties[0];
        Ok(Some(Self(Arc::new(Folder {
            archive,
            source,
            block: block_index,
            packed: PackStreamRange {
                index: first_pack,
                offset,
                size,
            },
            output_len,
            file_indexes,
            streams,
            prop,
            limit: limits.memory_limit_bytes,
            plan: ChecksumPlan::new(Checksum::Crc32).with_split_points(splits),
        }))))
    }

    /// The folder's absolute archive range, including the final EOS byte.
    pub fn packed_range(&self) -> PackStreamRange {
        self.0.packed
    }

    /// The folder's decoded length, used to clip destination writes.
    pub fn output_len(&self) -> u64 {
        self.0.output_len
    }

    /// Data-bearing members in output order, with their archive file indexes.
    /// Empty files/directories still require the caller's metadata pass.
    pub fn entries(&self) -> impl Iterator<Item = (usize, &ArchiveEntry)> {
        self.0
            .file_indexes
            .iter()
            .map(|&index| (index, &self.0.archive.files[index]))
    }

    /// Starts a scanner over consecutively covered packed bytes.
    pub fn scanner(&self) -> PositionalLzma2Scanner {
        PositionalLzma2Scanner {
            folder: self.clone(),
            scanner: Lzma2RunScanner::new(),
            next: 0,
            failed: false,
        }
    }

    /// Folds completed current-epoch runs and verifies all header CRCs.
    /// Completion records must cover the folder exactly, without gaps or
    /// overlap. This does not publish files or validate the caller's epoch.
    ///
    /// # Errors
    /// Missing/inconsistent output or a CRC mismatch fails with block context.
    pub fn verify_runs(
        &self,
        runs: &[Lzma2RunCompletion],
    ) -> Result<Vec<SubStreamCompletion>, Error> {
        let failure =
            || Error::ChecksumVerificationFailed.in_block(self.0.block, self.0.packed.offset);
        let mut ordered: Vec<_> = runs.iter().collect();
        ordered.sort_unstable_by_key(|run| run.output_offset);
        let mut folded = CrcFolder::new();
        let mut output = 0u64;
        let mut packed = self.0.packed.offset;
        for (index, run) in ordered.into_iter().enumerate() {
            if run.index != index as u64
                || run.output_offset != output
                || run.packed.offset != packed
                || run.output_len == 0
                || run.packed.size == 0
            {
                return Err(failure());
            }
            output = output
                .checked_add(run.output_len)
                .filter(|&v| v <= self.0.output_len)
                .ok_or_else(failure)?;
            packed = packed.checked_add(run.packed.size).ok_or_else(failure)?;
            let mut checked = run.output_offset;
            for check in &run.checks {
                if check.offset != checked || check.len == 0 {
                    return Err(failure());
                }
                checked = checked
                    .checked_add(check.len)
                    .filter(|&v| v <= output)
                    .ok_or_else(failure)?;
                folded.push(check.offset, check.len, check.crc32);
            }
            if checked != output {
                return Err(failure());
            }
        }
        if output != self.0.output_len || packed.checked_add(1) != Some(self.0.packed.end()) {
            return Err(failure());
        }
        let block = &self.0.archive.blocks[self.0.block];
        if block.has_crc && folded.range(0, output).map(u64::from) != Some(block.crc) {
            return Err(failure());
        }
        let mut verified = Vec::new();
        let mut offset = 0u64;
        for (i, stream) in self.0.streams.iter().enumerate() {
            if let Some(expected) = stream.crc {
                if folded.range(offset, stream.size) != Some(expected) {
                    return Err(failure());
                }
                verified.push(SubStreamCompletion {
                    block_index: self.0.block,
                    sub_stream_index: stream.index,
                    file_index: self.0.file_indexes[i],
                    unpacked_offset: offset,
                    len: stream.size,
                    crc32: expected,
                });
            }
            offset += stream.size;
        }
        Ok(verified)
    }
}

/// A bounded scanner that holds at most one completed run awaiting admission.
pub struct PositionalLzma2Scanner {
    folder: PositionalLzma2Folder,
    scanner: Lzma2RunScanner,
    next: u64,
    failed: bool,
}

impl PositionalLzma2Scanner {
    /// Used only by internal extraction from a complete positional source.
    /// The caller has checked the complete pack range against source length;
    /// streaming callers must still feed only their covered prefix normally.
    #[cfg(all(feature = "util", any(unix, windows), not(target_arch = "wasm32")))]
    pub(crate) fn skip_available_payload(&mut self, available: u64) -> u64 {
        if self.failed || self.scanner.pending_runs() != 0 {
            return 0;
        }
        self.scanner.skip_payload(
            available.min(
                self.folder
                    .0
                    .packed
                    .size
                    .saturating_sub(self.scanner.in_position()),
            ),
        )
    }

    /// Feeds a covered prefix and returns its consumed length. Drain `next_run`
    /// before resubmitting unconsumed bytes. Payload is skipped only within the
    /// supplied slice; the scanner cannot jump beyond download coverage.
    ///
    /// # Errors
    /// Invalid framing, extra EOS bytes, and output beyond the folder fail.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        let result = self.feed_inner(bytes);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn feed_inner(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        if self.failed
            || bytes.len() as u64
                > self
                    .folder
                    .0
                    .packed
                    .size
                    .saturating_sub(self.scanner.in_position())
        {
            return Err(Error::corrupted("LZMA2 scan exceeds packed range"));
        }
        let mut used = 0;
        while used < bytes.len() && self.scanner.pending_runs() == 0 {
            if self.scanner.finished() {
                return Err(Error::corrupted("bytes follow LZMA2 EOS"));
            }
            let skipped = self.scanner.skip_payload((bytes.len() - used) as u64) as usize;
            if skipped != 0 {
                used += skipped;
                continue;
            }
            used += self
                .scanner
                .feed(&bytes[used..used + 1])
                .map_err(super::scan_error)?;
            if self.scanner.out_position() > self.folder.0.output_len {
                return Err(Error::corrupted("LZMA2 scan exceeds folder output"));
            }
        }
        Ok(used)
    }

    /// Takes the next fully covered run as an owned `Send` worker job.
    pub fn next_run(&mut self) -> Option<PositionalLzma2Run> {
        if self.failed {
            return None;
        }
        let run = self.scanner.next_run()?;
        let index = self.next;
        self.next += 1;
        Some(PositionalLzma2Run {
            folder: self.folder.clone(),
            run,
            index,
        })
    }

    /// Confirms exact packed/output totals and EOS after the final feed.
    ///
    /// # Errors
    /// Incomplete, failed or incorrectly sized streams are refused.
    pub fn finish(self) -> Result<(), Error> {
        if self.failed
            || !self.scanner.finished()
            || self.scanner.in_position() != self.folder.0.packed.size
            || self.scanner.out_position() != self.folder.0.output_len
            || self.scanner.pending_runs() != 0
        {
            return Err(Error::corrupted("incomplete LZMA2 folder scan"));
        }
        Ok(())
    }
}

/// One independently decodable, fully scanned run. Cloning permits a repair
/// replay, but coverage and epoch validity must be re-established by the caller.
#[derive(Clone)]
pub struct PositionalLzma2Run {
    folder: PositionalLzma2Folder,
    run: Lzma2Run,
    index: u64,
}

impl PositionalLzma2Run {
    /// The run's order within its folder.
    pub fn index(&self) -> u64 {
        self.index
    }
    /// The run's packed image, excluding EOS and the next run's control byte.
    pub fn packed_range(&self) -> PackStreamRange {
        PackStreamRange {
            index: self.folder.0.packed.index,
            offset: self.folder.0.packed.offset + self.run.in_offset,
            size: self.run.packed_len,
        }
    }
    /// First decoded byte within the folder.
    pub fn output_offset(&self) -> u64 {
        self.run.out_offset
    }
    /// Exact run output length.
    pub fn output_len(&self) -> u64 {
        self.run.unpacked_len
    }
    /// Ring, coder state and positional input buffer reservation. The caller
    /// charges shared archive metadata and destination buffers separately.
    ///
    /// # Errors
    /// Invalid dictionary properties fail before allocation.
    pub fn memory_required(&self) -> Result<u64, Error> {
        let ring = Lzma2RunDecoder::memory_required(self.folder.0.prop, self.run.unpacked_len)
            .map_err(|e| Error::from(std::io::Error::from(e)))?;
        let checks = Lzma2RunDecoder::checksum_memory_required(
            &self.folder.0.plan,
            self.run.out_offset,
            self.run.unpacked_len,
        )
        .map_err(|e| Error::from(std::io::Error::from(e)))?;
        // At completion the codec records and public records may briefly
        // coexist. Both layouts fit in this doubled codec-record reservation.
        Ok(ring + self.run.packed_len.min(INPUT_BYTES as u64) + checks * 2)
    }

    /// Decodes on the calling worker and writes at folder-relative offsets.
    /// Short writes and `Interrupted` are handled without copying output.
    /// Writes are provisional until completion, folder verification and the
    /// caller's epoch check. Guarded destination creation is the caller's job.
    ///
    /// # Errors
    /// Refuses insufficient memory before allocation. Source and codec errors
    /// have block context; sink errors pass through unchanged. Cancellation
    /// stops between bounded steps and writes; a blocking source/sink must
    /// implement its own wakeup policy.
    pub fn decode<F>(self, cancel: &AtomicBool, sink: &mut F) -> Result<Lzma2RunCompletion, Error>
    where
        F: FnMut(u64, &[u8]) -> io::Result<usize> + ?Sized,
    {
        let required = self.memory_required()?;
        if required > self.folder.0.limit {
            return Err(Error::MemoryLimited {
                limit_bytes: self.folder.0.limit,
                required_bytes: required,
            });
        }
        let packed = self.packed_range();
        let locate = |error: Error| error.in_block(self.folder.0.block, packed.offset);
        let input_size = self.run.packed_len.min(INPUT_BYTES as u64) as usize;
        let mut decoder = Lzma2RunDecoder::new(
            self.index,
            self.folder.0.prop,
            self.run,
            self.folder.0.limit - input_size as u64,
        )
        .map_err(|e| locate(Error::from(std::io::Error::from(e))))?;
        decoder
            .set_checksum_plan(&self.folder.0.plan)
            .map_err(|e| locate(Error::from(std::io::Error::from(e))))?;
        let mut input = Vec::new();
        input
            .try_reserve_exact(input_size)
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
        input.resize(input_size, 0);
        let mut position = 0u64;
        let mut failed_sink = false;
        let mut deliver = |offset, bytes: &[u8]| {
            let result = sink(offset, bytes);
            if result
                .as_ref()
                .is_err_and(|e| e.kind() != io::ErrorKind::Interrupted)
                || result
                    .as_ref()
                    .is_ok_and(|&size| size == 0 || size > bytes.len())
            {
                failed_sink = true;
            }
            result
        };
        while position < packed.size {
            if cancel.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            let want = (packed.size - position).min(input.len() as u64) as usize;
            let size = loop {
                match self
                    .folder
                    .0
                    .source
                    .read_at(&mut input[..want], packed.offset + position)
                {
                    // A source woken by the cancellation returns
                    // `Interrupted`, and the cancellation is what is reported.
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        if cancel.load(Ordering::Acquire) {
                            return Err(Error::Cancelled);
                        }
                        continue;
                    }
                    result => break result.map_err(|e| locate(e.into()))?,
                }
            };
            if size == 0 || size > want {
                return Err(locate(io::Error::from(io::ErrorKind::UnexpectedEof).into()));
            }
            let mut used = 0;
            while used < size {
                match decoder.decode(&input[used..size], usize::MAX, cancel, &mut deliver) {
                    Ok(progress) if progress.read != 0 || progress.written != 0 => {
                        used += progress.read
                    }
                    Ok(_) => return Err(locate(Error::corrupted("LZMA2 run made no progress"))),
                    Err(error) => {
                        return Err(if failed_sink {
                            error.into()
                        } else {
                            locate(error.into())
                        });
                    }
                }
            }
            position += size as u64;
        }
        loop {
            match decoder.decode(&[], usize::MAX, cancel, &mut deliver) {
                Ok(progress) if progress.written != 0 => {}
                Ok(_) => break,
                Err(error) => {
                    return Err(if failed_sink {
                        error.into()
                    } else {
                        locate(error.into())
                    });
                }
            }
        }
        let complete = decoder.finish(cancel).map_err(|error| match error {
            LzmaError::Cancelled => locate(Error::Cancelled),
            other => locate(std::io::Error::from(other).into()),
        })?;
        let mut checks = Vec::new();
        checks
            .try_reserve_exact(complete.checks.segments.len())
            .map_err(|error| locate(io::Error::new(io::ErrorKind::OutOfMemory, error).into()))?;
        checks.extend(
            complete
                .checks
                .segments
                .into_iter()
                .map(|check| Lzma2RunCrc {
                    offset: check.offset,
                    len: check.len,
                    crc32: check.check.crc32().expect("CRC32 plan"),
                }),
        );
        Ok(Lzma2RunCompletion {
            index: self.index,
            packed,
            output_offset: self.run.out_offset,
            output_len: self.run.unpacked_len,
            checks,
        })
    }
}

#[cfg(all(test, feature = "compress"))]
mod tests {
    use super::*;
    use crate::{ArchiveWriter, Password, encoder_options::Lzma2Options};
    use std::io::Cursor;

    #[test]
    fn public_crc_high_bits_fail_with_block_context() {
        let expected = b"public checksum fields keep their complete values";
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(vec![Lzma2Options::from_level(1).into()]);
        writer.set_encrypt_header(false);
        let mut entry = ArchiveEntry::new_file("member");
        entry.size = expected.len() as u64;
        writer
            .push_archive_entry(entry, Some(expected.as_slice()))
            .unwrap();
        let bytes = Arc::new(writer.finish().unwrap().into_inner());
        let mut archive =
            Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap();
        archive.blocks[0].has_crc = true;
        archive.blocks[0].crc = archive.files[0].crc;
        let make = |archive| {
            PositionalLzma2Folder::new(
                Arc::new(archive),
                bytes.clone(),
                0,
                ArchiveLimits::default(),
            )
        };
        let folder = make(archive.clone()).unwrap().unwrap();
        let pack = folder.packed_range();
        let located = |error: Error| {
            assert!(matches!(
                error,
                Error::BlockDecode {
                    block_index: 0,
                    packed_offset,
                    kind: crate::BlockErrorKind::ChecksumMismatch,
                    ..
                }
                    if packed_offset == pack.offset
            ));
        };
        let mut scanner = folder.scanner();
        assert_eq!(
            scanner
                .feed(&bytes[pack.offset as usize..pack.end() as usize])
                .unwrap(),
            pack.size as usize
        );
        let run = scanner.next_run().unwrap();
        scanner.finish().unwrap();
        let mut decoded = Vec::new();
        let completion = run
            .decode(&AtomicBool::new(false), &mut |_, data: &[u8]| {
                decoded.extend_from_slice(data);
                Ok(data.len())
            })
            .unwrap();
        assert_eq!(decoded.as_slice(), expected);
        folder
            .verify_runs(std::slice::from_ref(&completion))
            .unwrap();
        let mut high_block = archive.clone();
        high_block.blocks[0].crc |= 1u64 << 32;
        let bad = make(high_block).unwrap().unwrap();
        located(
            bad.verify_runs(std::slice::from_ref(&completion))
                .unwrap_err(),
        );
        for block_crc in [false, true] {
            let mut fallback = archive.clone();
            fallback.sub_streams_info = None;
            fallback.blocks[0].has_crc = block_crc;
            // Both valid omitted forms remain accepted before mutation.
            make(fallback.clone())
                .unwrap()
                .unwrap()
                .verify_runs(std::slice::from_ref(&completion))
                .unwrap();
            if block_crc {
                fallback.blocks[0].crc |= 1u64 << 32;
            } else {
                fallback.files[0].crc |= 1u64 << 32;
            }
            match make(fallback) {
                Err(error) => located(error),
                Ok(_) => panic!("out-of-range public fallback CRC was accepted"),
            }
        }
    }

    /// A source woken by the cancellation while a run waits on it returns
    /// `Interrupted`; the run reports the cancellation, not a source failure.
    #[test]
    fn an_interrupted_read_after_cancel_is_cancelled() {
        struct WakesOnCancel {
            bytes: Arc<Vec<u8>>,
            cancel: Arc<AtomicBool>,
        }
        impl ReadAt for WakesOnCancel {
            fn read_at(&self, _: &mut [u8], _: u64) -> io::Result<usize> {
                self.cancel.store(true, Ordering::Release);
                Err(io::ErrorKind::Interrupted.into())
            }
            fn size(&self) -> io::Result<u64> {
                Ok(self.bytes.len() as u64)
            }
        }

        let expected = b"a run waiting on its source when it is cancelled";
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(vec![Lzma2Options::from_level(1).into()]);
        writer.set_encrypt_header(false);
        let mut entry = ArchiveEntry::new_file("member");
        entry.size = expected.len() as u64;
        writer
            .push_archive_entry(entry, Some(expected.as_slice()))
            .unwrap();
        let bytes = Arc::new(writer.finish().unwrap().into_inner());
        let archive =
            Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let source = Arc::new(WakesOnCancel {
            bytes: bytes.clone(),
            cancel: cancel.clone(),
        });
        let folder =
            PositionalLzma2Folder::new(Arc::new(archive), source, 0, ArchiveLimits::default())
                .unwrap()
                .unwrap();
        let pack = folder.packed_range();
        let mut scanner = folder.scanner();
        scanner
            .feed(&bytes[pack.offset as usize..pack.end() as usize])
            .unwrap();
        let run = scanner.next_run().unwrap();
        let error = run
            .decode(&cancel, &mut |_, data: &[u8]| Ok(data.len()))
            .unwrap_err();
        assert!(matches!(error, Error::Cancelled), "{error:?}");
    }

    #[test]
    fn one_stream_without_substreams_info_uses_its_folder_crc() {
        let expected = b"single member without optional substreams metadata";
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(vec![Lzma2Options::from_level(1).into()]);
        writer.set_encrypt_header(false);
        let mut entry = ArchiveEntry::new_file("member");
        entry.size = expected.len() as u64;
        writer
            .push_archive_entry(entry, Some(expected.as_slice()))
            .unwrap();
        let bytes = Arc::new(writer.finish().unwrap().into_inner());
        let mut archive =
            Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap();
        // The writer records the member checksum in SubStreamsInfo. Move that
        // same checksum to the folder to exercise its valid omitted form.
        assert!(archive.files[0].has_crc);
        archive.blocks[0].has_crc = true;
        archive.blocks[0].crc = archive.files[0].crc;
        archive.sub_streams_info = None;
        let folder = PositionalLzma2Folder::new(
            Arc::new(archive),
            bytes.clone(),
            0,
            ArchiveLimits::default(),
        )
        .unwrap()
        .unwrap();
        let pack = folder.packed_range();
        let mut scanner = folder.scanner();
        assert_eq!(
            scanner
                .feed(&bytes[pack.offset as usize..pack.end() as usize])
                .unwrap(),
            pack.size as usize
        );
        let run = scanner.next_run().unwrap();
        scanner.finish().unwrap();
        let completion = run
            .decode(&AtomicBool::new(false), &mut |_, bytes: &[u8]| {
                Ok(bytes.len())
            })
            .unwrap();
        assert_eq!(
            folder.verify_runs(&[completion]).unwrap()[0].len,
            expected.len() as u64
        );
    }
}
