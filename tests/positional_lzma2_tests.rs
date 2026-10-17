#![cfg(feature = "compress")]

use std::io::{self, Cursor};
use std::sync::{Arc, Barrier, Mutex, atomic::AtomicBool};

use sevenz_turbo::{
    Archive, ArchiveEntry, ArchiveLimits, ArchiveWriter, EncoderConfiguration, EncoderMethod,
    Error, Password, PositionalLzma2Folder, PositionalLzma2Run, ReadAt, SourceReader,
    encoder_options::Lzma2Options,
};

#[path = "support/multirun.rs"]
mod multirun;

fn fixture(methods: Option<Vec<EncoderConfiguration>>) -> (Arc<Archive>, Arc<Vec<u8>>, Vec<u8>) {
    let parts: Vec<Vec<u8>> = [1_048_593, 1_048_607, 1_048_623]
        .into_iter()
        .enumerate()
        .map(|(part, len)| {
            (0..len)
                .map(|i| ((i * 29) ^ (i >> 7) ^ part) as u8)
                .collect()
        })
        .collect();
    if methods.is_none() {
        let bytes = Arc::new(multirun::archive(&[
            ("member-0", &parts[0]),
            ("member-1", &parts[1]),
            ("member-2", &parts[2]),
        ]));
        let archive = Arc::new(
            Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap(),
        );
        return (archive, bytes, parts.concat());
    }
    let mut options = Lzma2Options::from_level_mt(1, 2, 1 << 20);
    options.set_dictionary_size(1 << 16);
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(methods.unwrap_or_else(|| vec![options.into()]));
    writer.set_encrypt_header(false);
    let entries = parts
        .iter()
        .enumerate()
        .map(|(index, bytes)| {
            let mut entry = ArchiveEntry::new_file(&format!("member-{index}"));
            entry.size = bytes.len() as u64;
            entry
        })
        .collect();
    let readers = parts
        .iter()
        .map(|bytes| SourceReader::new(bytes.as_slice()))
        .collect();
    writer.push_archive_entries(entries, readers).unwrap();
    let bytes = Arc::new(writer.finish().unwrap().into_inner());
    let archive =
        Arc::new(Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap());
    (archive, bytes, parts.concat())
}

fn jobs(folder: &PositionalLzma2Folder, bytes: &[u8]) -> Vec<PositionalLzma2Run> {
    let range = folder.packed_range();
    let packed = &bytes[range.offset as usize..range.end() as usize];
    let mut scanner = folder.scanner();
    let mut position = 0;
    let mut jobs = Vec::new();
    while position < packed.len() {
        let end = (position + 4093).min(packed.len());
        let used = scanner.feed(&packed[position..end]).unwrap();
        assert_ne!(used, 0);
        position += used;
        if let Some(job) = scanner.next_run() {
            assert!(job.packed_range().end() <= range.offset + position as u64);
            jobs.push(job);
        }
    }
    scanner.finish().unwrap();
    jobs
}

#[test]
fn positional_runs_overlap_workers_and_verify_file_split_checksums() {
    let (archive, bytes, expected) = fixture(None);
    let source: Arc<dyn ReadAt> = bytes.clone();
    let folder = PositionalLzma2Folder::new(archive, source, 0, ArchiveLimits::default())
        .unwrap()
        .unwrap();
    let runs = jobs(&folder, &bytes);
    assert!(runs.len() >= 3);
    let barrier = Arc::new(Barrier::new(runs.len()));
    let output = Arc::new(Mutex::new(vec![0; expected.len()]));
    let workers: Vec<_> = runs
        .into_iter()
        .map(|run| {
            assert!(run.memory_required().unwrap() < (1 << 20));
            let barrier = Arc::clone(&barrier);
            let output = Arc::clone(&output);
            std::thread::spawn(move || {
                let mut first = true;
                run.decode(&AtomicBool::new(false), &mut |offset, bytes: &[u8]| {
                    if first {
                        first = false;
                        barrier.wait();
                    }
                    let size = bytes.len().min(4093);
                    output.lock().unwrap()[offset as usize..offset as usize + size]
                        .copy_from_slice(&bytes[..size]);
                    Ok(size)
                })
                .unwrap()
            })
        })
        .collect();
    let mut completions: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    completions.reverse();
    let verified = folder.verify_runs(&completions).unwrap();
    assert_eq!(verified.len(), 3);
    assert_eq!(*output.lock().unwrap(), expected);
    let mut changed = completions.clone();
    changed[0].checks[0].crc32 ^= 1;
    assert!(folder.verify_runs(&changed).is_err());
    assert!(
        folder
            .verify_runs(&completions[..completions.len() - 1])
            .is_err()
    );
    changed = completions.clone();
    changed.push(completions[0].clone());
    assert!(folder.verify_runs(&changed).is_err());
    for case in 0..5 {
        let mut changed = completions.clone();
        match case {
            0 => changed[0].output_offset -= 1,
            1 => changed[0].packed.offset -= 1,
            2 => changed[0].output_len += 1,
            3 => changed[0].checks[0].offset += 1,
            4 => changed[0].checks[0].len += 1,
            _ => unreachable!(),
        }
        assert!(folder.verify_runs(&changed).is_err(), "case {case}");
    }
}

#[test]
fn positional_run_limits_and_sink_errors_preserve_their_identity() {
    let (archive, bytes, _) = fixture(None);
    let source: Arc<dyn ReadAt> = bytes.clone();
    let folder =
        PositionalLzma2Folder::new(archive.clone(), source.clone(), 0, ArchiveLimits::memory(1))
            .unwrap()
            .unwrap();
    let run = jobs(&folder, &bytes).remove(0);
    assert!(matches!(
        run.decode(&AtomicBool::new(false), &mut |_, _| panic!("over budget")),
        Err(Error::MemoryLimited { .. })
    ));
    let folder = PositionalLzma2Folder::new(archive, source, 0, ArchiveLimits::default())
        .unwrap()
        .unwrap();
    let run = jobs(&folder, &bytes).remove(0);
    let error = run
        .decode(&AtomicBool::new(false), &mut |_, _| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sink refused",
            ))
        })
        .unwrap_err();
    assert!(!matches!(error, Error::BlockDecode { .. }));
    assert!(error.to_string().contains("sink refused"));
    for zero in [true, false] {
        let run = jobs(&folder, &bytes).remove(0);
        let error = run
            .decode(&AtomicBool::new(false), &mut |_, bytes: &[u8]| {
                Ok(if zero { 0 } else { bytes.len() + 1 })
            })
            .unwrap_err();
        assert!(!matches!(error, Error::BlockDecode { .. }));
    }
    let cancel = AtomicBool::new(false);
    let run = jobs(&folder, &bytes).remove(0);
    let error = run
        .decode(&cancel, &mut |_, data: &[u8]| {
            cancel.store(true, std::sync::atomic::Ordering::Release);
            Ok(data.len())
        })
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error:?}");
}

#[test]
fn positional_scanning_rejects_truncation_extra_bytes_and_unsupported_graphs() {
    let (archive, bytes, _) = fixture(None);
    let source: Arc<dyn ReadAt> = bytes.clone();
    let folder = PositionalLzma2Folder::new(archive, source, 0, ArchiveLimits::default())
        .unwrap()
        .unwrap();
    let range = folder.packed_range();
    let packed = &bytes[range.offset as usize..range.end() as usize];
    let mut scanner = folder.scanner();
    let mut pos = 0;
    while pos < packed.len() - 1 {
        pos += scanner.feed(&packed[pos..packed.len() - 1]).unwrap();
        scanner.next_run();
    }
    assert!(scanner.finish().is_err());
    let mut scanner = folder.scanner();
    let mut extra = packed.to_vec();
    extra.push(0);
    assert!(scanner.feed(&extra).is_err());
    assert!(scanner.next_run().is_none());
    let (archive, bytes, _) = fixture(Some(vec![EncoderConfiguration::new(EncoderMethod::COPY)]));
    assert!(
        PositionalLzma2Folder::new(archive, bytes, 0, ArchiveLimits::default())
            .unwrap()
            .is_none()
    );
}

#[test]
fn positional_file_mapping_skips_interspersed_empty_entries() {
    let (archive, bytes, expected) = fixture(None);
    let mut archive = Arc::try_unwrap(archive).unwrap();
    // This is the stream map emitted by the parser when FilesInfo contains
    // an empty file or directory between a solid folder's data members.
    let empty = ArchiveEntry {
        name: "empty".into(),
        has_stream: false,
        ..ArchiveEntry::default()
    };
    let directory = ArchiveEntry {
        name: "directory".into(),
        has_stream: false,
        is_directory: true,
        ..ArchiveEntry::default()
    };
    archive.files.insert(1, empty);
    archive.files.insert(2, directory);
    archive.stream_map.file_block_index.insert(1, Some(0));
    archive.stream_map.file_block_index.insert(2, Some(0));
    let folder = PositionalLzma2Folder::new(
        Arc::new(archive),
        bytes.clone(),
        0,
        ArchiveLimits::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        folder.entries().map(|(index, _)| index).collect::<Vec<_>>(),
        [0, 3, 4]
    );
    let mut output = vec![0; expected.len()];
    let completions: Vec<_> = jobs(&folder, &bytes)
        .into_iter()
        .map(|run| {
            run.decode(&AtomicBool::new(false), &mut |offset, bytes: &[u8]| {
                output[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            })
            .unwrap()
        })
        .collect();
    assert_eq!(
        folder
            .verify_runs(&completions)
            .unwrap()
            .iter()
            .map(|done| done.file_index)
            .collect::<Vec<_>>(),
        [0, 3, 4]
    );
    assert_eq!(output, expected);
}

#[test]
fn positional_constructor_honors_metadata_and_output_limits() {
    let (archive, bytes, _) = fixture(None);
    for (extra_block, limit, requested) in [(false, 0, 1), (true, 1, 2)] {
        let mut metadata = (*archive).clone();
        if extra_block {
            metadata.blocks.push(metadata.blocks[0].clone());
        }
        assert!(matches!(
            PositionalLzma2Folder::new(
                Arc::new(metadata), bytes.clone(), 0,
                ArchiveLimits { max_total_coders: limit, ..ArchiveLimits::default() },
            ),
            Err(Error::LimitExceeded { what: sevenz_turbo::Limit::TotalCoders, requested: count, limit: cap })
                if count == requested && cap == limit
        ));
    }
    assert!(
        PositionalLzma2Folder::new(
            archive.clone(),
            bytes.clone(),
            0,
            ArchiveLimits {
                max_total_coders: 1,
                ..ArchiveLimits::default()
            },
        )
        .unwrap()
        .is_some()
    );
    for limits in [
        ArchiveLimits {
            max_entries: 2,
            ..ArchiveLimits::default()
        },
        ArchiveLimits {
            max_unpack_bytes: 1,
            ..ArchiveLimits::default()
        },
        ArchiveLimits {
            max_unpack_ratio: 1,
            ..ArchiveLimits::default()
        },
        ArchiveLimits {
            max_coders_per_block: 0,
            ..ArchiveLimits::default()
        },
        ArchiveLimits {
            max_streams_per_coder: 0,
            ..ArchiveLimits::default()
        },
    ] {
        assert!(matches!(
            PositionalLzma2Folder::new(archive.clone(), bytes.clone(), 0, limits),
            Err(Error::LimitExceeded { .. })
        ));
    }
}

#[test]
fn positional_ring_mode_leaves_filters_and_encryption_on_the_existing_path() {
    for method in [EncoderMethod::BCJ_X86_FILTER, EncoderMethod::BCJ2_FILTER] {
        let (archive, bytes, _) = fixture(Some(vec![
            Lzma2Options::from_level(1).into(),
            EncoderConfiguration::new(method),
        ]));
        assert!(
            PositionalLzma2Folder::new(archive, bytes, 0, ArchiveLimits::default())
                .unwrap()
                .is_none()
        );
    }
    #[cfg(feature = "aes256")]
    {
        let (archive, bytes, _) = fixture(Some(vec![
            sevenz_turbo::encoder_options::AesEncoderOptions::new(Password::new("fixture-key"))
                .into(),
            Lzma2Options::from_level(1).into(),
        ]));
        assert!(
            PositionalLzma2Folder::new(archive, bytes, 0, ArchiveLimits::default())
                .unwrap()
                .is_none()
        );
    }
}
