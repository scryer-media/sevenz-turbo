use super::*;
use std::path::Path;

use crate::util::decompress::{extract_in_root, open_extract_root, safe_join};

impl<R: Read + Seek> ArchiveReader<R> {
    /// Extracts into a confined directory using this reader's thread count and limits.
    ///
    /// Large, unfiltered LZMA2 files can decode independent runs straight into
    /// their destination when a positional source is available. Each worker
    /// holds a dictionary ring rather than a complete run's decoded output.
    /// Other archives use the ordinary ordered entry walk. Existing links are
    /// rejected and existing files are replaced without truncating their inode.
    /// Partial files can remain on failure, as with [`crate::decompress`].
    ///
    /// The initial positional path admits a single data member only. Multiple
    /// members retain ordered creation (including duplicate-name replacement),
    /// and readers with adaptive controls, a live LZMA2 handle, completion
    /// hooks or finite memory budgets retain their usual scheduler. Positional
    /// extraction has no ordered-reader progress or ledger to report.
    pub fn extract_to(&mut self, dest: impl AsRef<Path>) -> Result<(), Error> {
        let root = open_extract_root(dest.as_ref())?;
        #[cfg(any(unix, windows))]
        if let Some(plan) = self.ring_extraction_plan()? {
            self.lzma2.set_block_index(0);
            return plan.extract(&root, &self.archive.files[0], self.thread_count);
        }
        self.for_each_entries(|entry, reader| {
            let relative = safe_join(Path::new(""), entry.name())?;
            extract_in_root(&root, entry, reader, &relative)
        })
    }

    #[cfg(any(unix, windows))]
    fn ring_extraction_plan(&self) -> Result<Option<RingExtraction>, Error> {
        // Finite budgets, live adaptive controls and completion callbacks use
        // the existing scheduler and its full accounting/progress contract.
        // A single file also avoids changing duplicate-name and directory
        // creation order when different runs finish out of order.
        //
        // `strong_count(&self.lzma2) != 1` is the "a live `Lzma2Handle` may
        // retune this decode" test. It is sound here even though counts are
        // racy in general: `extract_to` holds `&mut self`, so no clone can be
        // taken from the reader while this runs, and a count of one means no
        // handle exists anywhere that could later be used. A count above one
        // that drops concurrently only sends this extraction down the
        // ordinary path, which is never wrong.
        if !self.input_complete
            || self.thread_count <= 1
            || self.adaptive_lzma2
            || Arc::strong_count(&self.lzma2) != 1
            || !self.verify_checksums
            || self.on_block_complete.is_some()
            || self.on_sub_stream_complete.is_some()
            || self.limits.memory_limit_bytes != u64::MAX
            || self.archive.blocks.len() != 1
            || self.archive.files.len() != 1
            || !self.archive.files[0].has_stream
            || self.archive.files[0].is_directory()
        {
            return Ok(None);
        }
        let Some(source) = self.positional.as_ref() else {
            return Ok(None);
        };
        let block = &self.archive.blocks[0];
        if block.coders.len() != 1
            || block.coders[0].encoder_method_id() != EncoderMethod::ID_LZMA2
            || u64::from(crate::codec::lzma_turbo::lzma2_dictionary_size(
                block.coders[0].properties(),
            )?) >= block.get_unpack_size()
        {
            return Ok(None);
        }
        let Some(folder) = crate::PositionalLzma2Folder::new(
            Arc::new(self.archive.clone()),
            Arc::clone(source),
            0,
            self.limits,
        )?
        else {
            return Ok(None);
        };
        // Bound planning metadata as well as decoder buffers. A stream with
        // pathological reset density falls back before creating any file.
        const MAX_RUNS: usize = 4096;
        let packed = folder.packed_range();
        if source.size()? < packed.end() {
            return Err(Error::from(io::Error::from(io::ErrorKind::UnexpectedEof))
                .in_block(0, packed.offset));
        }
        let mut scanner = folder.scanner();
        let mut input = [0u8; 64];
        let mut position = 0;
        let mut runs = Vec::new();
        while position < packed.size {
            position += scanner.skip_available_payload(packed.size - position);
            if position == packed.size {
                break;
            }
            let want = (packed.size - position).min(input.len() as u64) as usize;
            let size = loop {
                match source.read_at(&mut input[..want], packed.offset + position) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    result => {
                        break result.map_err(|e| Error::from(e).in_block(0, packed.offset))?;
                    }
                }
            };
            if size == 0 || size > want {
                return Err(Error::from(io::Error::from(io::ErrorKind::UnexpectedEof))
                    .in_block(0, packed.offset));
            }
            let mut used = 0;
            while used < size {
                let count = scanner
                    .feed(&input[used..size])
                    .map_err(|e| e.in_block(0, packed.offset))?;
                if let Some(run) = scanner.next_run() {
                    if runs.len() == MAX_RUNS {
                        return Ok(None);
                    }
                    runs.try_reserve(1)
                        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
                    runs.push(run);
                } else if count == 0 {
                    return Err(Error::corrupted("LZMA2 scanner made no progress")
                        .in_block(0, packed.offset));
                }
                used += count;
            }
            position += size as u64;
        }
        scanner.finish().map_err(|e| e.in_block(0, packed.offset))?;
        Ok(Some(RingExtraction { folder, runs }))
    }
}

#[cfg(all(test, feature = "compress", any(unix, windows)))]
#[path = "../../tests/support/multirun.rs"]
mod multirun;

#[cfg(all(test, feature = "compress", any(unix, windows)))]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fixture() -> (ArchiveReader<Cursor<Vec<u8>>>, Vec<u8>) {
        let expected: Vec<_> = (0..(3 << 20) + 17)
            .map(|i| ((i * 29) ^ (i >> 7)) as u8)
            .collect();
        let bytes = multirun::archive(&[("nested/member", &expected)]);
        let source = bytes.clone();
        let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
        reader.set_positional_source(source);
        reader.set_input_complete(true);
        reader.set_threads(4);
        (reader, expected)
    }

    #[test]
    fn file_open_enables_complete_input_ring_extraction() {
        let (source, expected) = fixture();
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), source.source.get_ref()).unwrap();
        let mut reader = ArchiveReader::open(input.path(), Password::empty()).unwrap();
        reader.set_threads(4);
        assert!(reader.ring_extraction_plan().unwrap().is_some());
        let output = tempfile::tempdir().unwrap();
        reader.extract_to(output.path()).unwrap();
        assert_eq!(
            std::fs::read(output.path().join("nested/member")).unwrap(),
            expected
        );
    }

    #[test]
    fn ring_extract_uses_bounded_runs_and_matches_ordered_extraction() {
        let (mut reader, expected) = fixture();
        let plan = reader.ring_extraction_plan().unwrap().unwrap();
        assert!(plan.runs.len() >= 3);
        assert!(
            plan.runs
                .iter()
                .all(|run| run.memory_required().unwrap() < 1 << 20)
        );
        let ring = tempfile::tempdir().unwrap();
        reader.extract_to(ring.path()).unwrap();
        assert_eq!(
            std::fs::read(ring.path().join("nested/member")).unwrap(),
            expected
        );
        assert!(reader.lzma2_progress().is_none());
        reader.clear_positional_source();
        let ordered = tempfile::tempdir().unwrap();
        reader.extract_to(ordered.path()).unwrap();
        assert_eq!(
            std::fs::read(ordered.path().join("nested/member")).unwrap(),
            expected
        );
    }

    #[test]
    fn ring_extract_scans_headers_with_short_reads_without_reading_payload() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counted {
            bytes: Vec<u8>,
            step: usize,
            read: AtomicUsize,
        }
        impl ReadAt for Counted {
            fn size(&self) -> io::Result<u64> {
                Ok(self.bytes.len() as u64)
            }
            fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
                let count = bytes.len().min(self.step);
                let got = self.bytes.read_at(&mut bytes[..count], offset)?;
                self.read.fetch_add(got, Ordering::Relaxed);
                Ok(got)
            }
        }
        let (mut reader, _) = fixture();
        let original = reader.ring_extraction_plan().unwrap().unwrap();
        let ranges: Vec<_> = original.runs.iter().map(|run| run.packed_range()).collect();
        for step in [1, 3, 64] {
            let source = Arc::new(Counted {
                bytes: reader.source.get_ref().clone(),
                step,
                read: AtomicUsize::new(0),
            });
            reader.set_positional_source(Arc::clone(&source));
            let plan = reader.ring_extraction_plan().unwrap().unwrap();
            assert_eq!(
                plan.runs
                    .iter()
                    .map(|run| run.packed_range())
                    .collect::<Vec<_>>(),
                ranges
            );
            assert!(
                (source.read.load(Ordering::Relaxed) as u64) < plan.folder.packed_range().size / 2
            );
        }
        let mut short = reader.source.get_ref().clone();
        short.truncate(original.folder.packed_range().end() as usize - 1);
        reader.set_positional_source(short);
        assert!(reader.ring_extraction_plan().is_err());
    }

    #[test]
    fn ring_extract_preserves_control_and_limit_fallbacks() {
        let (mut reader, _) = fixture();
        reader.set_input_complete(false);
        assert!(reader.ring_extraction_plan().unwrap().is_none());
        reader.set_input_complete(true);
        let handle = reader.lzma2_handle();
        assert!(reader.ring_extraction_plan().unwrap().is_none());
        drop(handle);
        assert!(reader.ring_extraction_plan().unwrap().is_some());
        reader.set_adaptive_lzma2(true);
        assert!(reader.ring_extraction_plan().unwrap().is_none());
        reader.set_adaptive_lzma2(false);
        reader.limits.memory_limit_bytes = 64 << 20;
        assert!(reader.ring_extraction_plan().unwrap().is_none());
        reader.limits.memory_limit_bytes = u64::MAX;
        reader.set_block_complete_hook(|_| {});
        assert!(reader.ring_extraction_plan().unwrap().is_none());
        reader.clear_block_complete_hook();
        reader.set_verify_checksums(false);
        assert!(reader.ring_extraction_plan().unwrap().is_none());
    }

    #[test]
    fn ring_extract_rejects_crc_damage_and_unsafe_names() {
        let (mut reader, _) = fixture();
        reader.archive.blocks[0].has_crc = true;
        reader.archive.blocks[0].crc = 0;
        let dest = tempfile::tempdir().unwrap();
        let error = reader.extract_to(dest.path()).unwrap_err();
        assert!(
            matches!(
                error,
                Error::BlockDecode {
                    block_index: 0,
                    kind: crate::BlockErrorKind::ChecksumMismatch,
                    ..
                }
            ),
            "{error:?}"
        );
        reader.archive.files[0].name = "../outside".into();
        assert!(matches!(
            reader.extract_to(dest.path()),
            Err(Error::UnsafeEntryName { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ring_extract_reuses_link_guards_and_never_truncates_shared_inodes() {
        let (mut reader, expected) = fixture();
        let dest = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("member"), b"untouched").unwrap();
        std::os::unix::fs::symlink(outside.path(), dest.path().join("nested")).unwrap();
        assert!(reader.extract_to(dest.path()).is_err());
        std::fs::remove_file(dest.path().join("nested")).unwrap();
        std::fs::create_dir(dest.path().join("nested")).unwrap();
        std::fs::hard_link(
            outside.path().join("member"),
            dest.path().join("nested/member"),
        )
        .unwrap();
        reader.extract_to(dest.path()).unwrap();
        assert_eq!(
            std::fs::read(outside.path().join("member")).unwrap(),
            b"untouched"
        );
        assert_eq!(
            std::fs::read(dest.path().join("nested/member")).unwrap(),
            expected
        );
    }

    #[test]
    fn ring_extract_joins_workers_on_source_error_or_panic() {
        struct Source {
            bytes: Vec<u8>,
            scanner: std::thread::ThreadId,
            panic: bool,
        }
        impl ReadAt for Source {
            fn size(&self) -> io::Result<u64> {
                Ok(self.bytes.len() as u64)
            }

            fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
                if std::thread::current().id() != self.scanner {
                    assert!(!self.panic, "injected worker panic");
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected source error",
                    ));
                }
                assert!(bytes.len() <= 64 << 10);
                self.bytes.read_at(bytes, offset)
            }
        }
        let (mut reader, _) = fixture();
        for panic in [false, true] {
            let source = Arc::new(Source {
                bytes: reader.source.get_ref().clone(),
                scanner: std::thread::current().id(),
                panic,
            });
            reader.set_positional_source(Arc::clone(&source));
            let dest = tempfile::tempdir().unwrap();
            let error = reader.extract_to(dest.path()).unwrap_err();
            if panic {
                assert!(error.to_string().contains("worker panicked"));
            } else {
                assert!(matches!(
                    error,
                    Error::BlockDecode {
                        block_index: 0,
                        kind: crate::BlockErrorKind::Io,
                        ..
                    }
                ));
            }
            reader.clear_positional_source();
            assert_eq!(
                Arc::strong_count(&source),
                1,
                "a worker retained the source"
            );
        }
    }
}

#[cfg(any(unix, windows))]
struct RingExtraction {
    folder: crate::PositionalLzma2Folder,
    runs: Vec<crate::PositionalLzma2Run>,
}

#[cfg(any(unix, windows))]
impl RingExtraction {
    fn extract(
        self,
        root: &cap_std::fs::Dir,
        entry: &ArchiveEntry,
        threads: u32,
    ) -> Result<(), Error> {
        use crate::util::decompress::{create_extract_entry, set_extract_times};
        use std::sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };

        let relative = safe_join(Path::new(""), entry.name())?;
        let file = create_extract_entry(root, entry, &relative)?
            .ok_or_else(|| Error::invalid_argument("ring destination must be a file"))?;
        let workers = (threads as usize).min(self.runs.len());
        let cancel = AtomicBool::new(false);
        let next = AtomicUsize::new(0);
        let mut completions = Vec::new();
        completions
            .try_reserve_exact(self.runs.len())
            .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
        let completions = Mutex::new(completions);
        let first_error = Mutex::new(None);
        std::thread::scope(|scope| -> Result<(), Error> {
            let mut handles = Vec::new();
            handles
                .try_reserve_exact(workers)
                .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
            for _ in 0..workers {
                let spawned = std::thread::Builder::new().spawn_scoped(scope, || {
                    while !cancel.load(Ordering::Acquire) {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(run) = self.runs.get(index) else {
                            break;
                        };
                        let result = run.clone().decode(&cancel, &mut |offset, bytes: &[u8]| {
                            #[cfg(unix)]
                            {
                                std::os::unix::fs::FileExt::write_at(&file, bytes, offset)
                            }
                            #[cfg(windows)]
                            {
                                std::os::windows::fs::FileExt::seek_write(&file, bytes, offset)
                            }
                        });
                        match result {
                            Ok(done) => completions.lock().unwrap().push(done),
                            Err(error) => {
                                let mut first = first_error.lock().unwrap();
                                if first.is_none() {
                                    *first = Some(error);
                                }
                                cancel.store(true, Ordering::Release);
                                break;
                            }
                        }
                    }
                });
                match spawned {
                    Ok(handle) => handles.push(handle),
                    Err(error) => {
                        cancel.store(true, Ordering::Release);
                        let mut first = first_error.lock().unwrap();
                        if first.is_none() {
                            *first = Some(error.into());
                        }
                        break;
                    }
                }
            }
            // Every worker is joined before the destination handle is released,
            // including spawn failure, codec failure and an unwinding worker.
            for handle in handles {
                if handle.join().is_err() {
                    cancel.store(true, Ordering::Release);
                    let mut first = first_error.lock().unwrap();
                    if first.is_none() {
                        *first = Some(Error::internal("ring worker panicked"));
                    }
                }
            }
            Ok(())
        })?;
        if let Some(error) = first_error.into_inner().unwrap() {
            return Err(error);
        }
        self.folder
            .verify_runs(&completions.into_inner().unwrap())?;
        set_extract_times(&file, entry);
        Ok(())
    }
}
