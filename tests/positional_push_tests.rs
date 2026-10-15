#![cfg(all(feature = "compress", feature = "aes256"))]

use sevenz_turbo::{
    Archive, ArchiveEntry, ArchiveLimits, ArchiveReader, ArchiveWriter, EncoderConfiguration,
    EncoderMethod, Error, Password, PositionalBlockDecoder, ReadAt,
    encoder_options::AesEncoderOptions,
};
use std::io::{self, Cursor, Write};
use std::sync::{Arc, Barrier, Mutex};

type Fixture = (Arc<Archive>, Arc<Vec<u8>>, Arc<Password>, Vec<Vec<u8>>);

#[test]
fn positional_block_refuses_interspersed_no_stream_entries_before_reading() {
    let password = Arc::new(Password::empty());
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(vec![EncoderConfiguration::new(EncoderMethod::COPY)]);
    writer
        .push_archive_entries(
            vec![
                ArchiveEntry::new_file("first"),
                ArchiveEntry::new_file("last"),
            ],
            vec![
                sevenz_turbo::SourceReader::new(&b"first data"[..]),
                sevenz_turbo::SourceReader::new(&b"last data"[..]),
            ],
        )
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let mut archive = Archive::read(&mut Cursor::new(bytes), &password).unwrap();
    archive.files.insert(1, ArchiveEntry::default());
    archive.stream_map.file_block_index.insert(1, Some(0));
    struct Unread;
    impl ReadAt for Unread {
        fn read_at(&self, _: &mut [u8], _: u64) -> io::Result<usize> {
            panic!("invalid member mapping must fail before source access")
        }
        fn size(&self) -> io::Result<u64> {
            panic!("invalid member mapping must fail before source access")
        }
    }
    let result = PositionalBlockDecoder::new(Arc::new(archive), password, Arc::new(Unread), 0);
    assert!(result.is_err());
}

fn fixture(aes: bool, sizes: &[usize]) -> Fixture {
    let password = Password::new("fixture-key");
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    let mut methods = vec![EncoderConfiguration::new(EncoderMethod::COPY)];
    if aes {
        methods.insert(0, AesEncoderOptions::new(password.clone()).into());
    }
    writer.set_content_methods(methods);
    writer.set_encrypt_header(false);
    let payloads: Vec<_> = sizes
        .iter()
        .enumerate()
        .map(|(index, &size)| {
            (0..size)
                .map(|i| ((i * 29) ^ (i >> 7) ^ index) as u8)
                .collect::<Vec<_>>()
        })
        .collect();
    for (index, bytes) in payloads.iter().enumerate() {
        let mut entry = ArchiveEntry::new_file(&format!("member-{index}"));
        entry.size = bytes.len() as u64;
        writer
            .push_archive_entry(entry, Some(bytes.as_slice()))
            .unwrap();
    }
    let bytes = Arc::new(writer.finish().unwrap().into_inner());
    let archive = Arc::new(Archive::read(&mut Cursor::new(bytes.as_slice()), &password).unwrap());
    (archive, bytes, Arc::new(password), payloads)
}

#[derive(Default)]
struct Writes {
    bytes: Vec<u8>,
    sizes: Vec<usize>,
    partial: bool,
}
impl Write for Writes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let size = if self.partial {
            bytes.len().min(4093)
        } else {
            bytes.len()
        };
        self.sizes.push(size);
        self.bytes.extend_from_slice(&bytes[..size]);
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn copy_and_aes_push_share_the_entry_cursor_and_verified_completion() {
    for aes in [false, true] {
        for partial in [false, true] {
            let (archive, bytes, password, payloads) = fixture(aes, &[19, (2 << 20) + 31]);
            let mut reader =
                ArchiveReader::new(Cursor::new(bytes.as_slice()), (*password).clone()).unwrap();
            let completed = Arc::new(Mutex::new(Vec::new()));
            let reports = Arc::clone(&completed);
            reader.set_sub_stream_complete_hook(move |report| reports.lock().unwrap().push(report));
            let mut index = 0;
            reader
                .for_each_entries_direct(|entry, data| {
                    let mut prefix = [0; 13];
                    data.read_exact(&mut prefix)?;
                    let mut output = Writes {
                        partial,
                        ..Default::default()
                    };
                    let copied = data.write_rest(&mut output)?;
                    assert_eq!(copied + 13, entry.size);
                    assert_eq!(&prefix, &payloads[index][..13]);
                    assert_eq!(output.bytes, payloads[index][13..]);
                    if index == 1 && !partial {
                        assert!(output.sizes.iter().any(|&size| size > 256 << 10));
                        assert!(output.sizes.iter().all(|&size| size <= 1 << 20));
                    }
                    assert_eq!(data.write_rest(&mut io::sink())?, 0);
                    index += 1;
                    Ok(true)
                })
                .unwrap();
            assert_eq!(index, 2);
            let reports = completed.lock().unwrap();
            assert_eq!(reports.len(), 2);
            for report in reports.iter() {
                assert_eq!(report.len, archive.files[report.file_index].size);
                assert_eq!(
                    u64::from(report.crc32),
                    archive.files[report.file_index].crc
                );
            }
        }
    }
}

struct ConcurrentSource {
    bytes: Arc<Vec<u8>>,
    starts: Vec<u64>,
    seen: Mutex<Vec<u64>>,
    barrier: Barrier,
}
impl ReadAt for ConcurrentSource {
    fn read_at(&self, output: &mut [u8], offset: u64) -> io::Result<usize> {
        let first = {
            let mut seen = self.seen.lock().unwrap();
            if self.starts.contains(&offset) && !seen.contains(&offset) {
                seen.push(offset);
                true
            } else {
                false
            }
        };
        if first {
            self.barrier.wait();
        }
        self.bytes.read_at(output, offset)
    }
    fn size(&self) -> io::Result<u64> {
        self.bytes.size()
    }
}

#[test]
fn owned_blocks_move_to_independent_workers_without_a_shared_cursor() {
    fn is_send<T: Send + 'static>() {}
    is_send::<PositionalBlockDecoder>();
    for aes in [false, true] {
        let (archive, bytes, password, payloads) = fixture(aes, &[100_003, 77_777, 33_333]);
        let source: Arc<dyn ReadAt> = Arc::new(ConcurrentSource {
            bytes,
            starts: (0..archive.blocks.len())
                .map(|block| archive.block_pack_streams(block)[0].offset)
                .collect(),
            seen: Mutex::new(Vec::new()),
            barrier: Barrier::new(3),
        });
        let workers: Vec<_> = (0..3)
            .map(|block| {
                let decoder = PositionalBlockDecoder::new(
                    Arc::clone(&archive),
                    Arc::clone(&password),
                    Arc::clone(&source),
                    block,
                )
                .unwrap();
                std::thread::spawn(move || {
                    assert_eq!(decoder.entries().len(), 1);
                    let mut output = Vec::new();
                    assert!(
                        decoder
                            .for_each_entries_direct(&mut |_, data| {
                                data.write_rest(&mut output)?;
                                Ok(true)
                            })
                            .unwrap()
                    );
                    output
                })
            })
            .collect();
        for (worker, expected) in workers.into_iter().zip(payloads) {
            assert_eq!(worker.join().unwrap(), expected);
        }
    }
}

#[test]
fn positional_decode_preserves_early_stop_sink_errors_and_limits() {
    let (archive, bytes, password, _) = fixture(true, &[100_003]);
    let source: Arc<dyn ReadAt> = bytes;
    let make = || {
        PositionalBlockDecoder::new(
            Arc::clone(&archive),
            Arc::clone(&password),
            Arc::clone(&source),
            0,
        )
        .unwrap()
    };
    let completed = Arc::new(Mutex::new(0));
    let mut decoder = make();
    let count = Arc::clone(&completed);
    decoder.set_sub_stream_complete_hook(move |_| *count.lock().unwrap() += 1);
    assert!(
        !decoder
            .for_each_entries_direct(&mut |_, data| {
                data.read_exact(&mut [0; 7])?;
                Ok(false)
            })
            .unwrap()
    );
    assert_eq!(*completed.lock().unwrap(), 0);
    struct Failed;
    impl Write for Failed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "destination refused",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let error = make()
        .for_each_entries_direct(&mut |_, data| {
            data.write_rest(&mut Failed)?;
            Ok(true)
        })
        .unwrap_err();
    assert!(
        !matches!(error, Error::BlockDecode { .. }),
        "sink errors are not source damage: {error}"
    );
    assert!(error.to_string().contains("destination refused"));
    let limits = ArchiveLimits {
        memory_limit_bytes: 1,
        ..ArchiveLimits::default()
    };
    let decoder = PositionalBlockDecoder::with_limits(
        Arc::clone(&archive),
        Arc::clone(&password),
        Arc::clone(&source),
        0,
        limits,
    )
    .unwrap();
    assert!(
        decoder
            .for_each_entries_direct(&mut |_, _| panic!("memory refusal precedes delivery"))
            .is_err()
    );
    assert!(PositionalBlockDecoder::new(archive, password, source, 1).is_err());
}

#[test]
fn push_damage_and_truncated_ciphertext_keep_the_block_location() {
    for aes in [false, true] {
        let (archive, bytes, password, _) = fixture(aes, &[100_003]);
        let pack = archive.block_pack_streams(0)[0];
        for truncate in [false, true] {
            let mut damaged = (*bytes).clone();
            if truncate {
                damaged.truncate((pack.offset + pack.size - 1) as usize);
            } else {
                damaged[pack.offset as usize + 10] ^= 0x80;
            }
            let source: Arc<dyn ReadAt> = Arc::new(damaged);
            let decoder =
                PositionalBlockDecoder::new(Arc::clone(&archive), Arc::clone(&password), source, 0)
                    .unwrap();
            let error = decoder
                .for_each_entries_direct(&mut |_, data| {
                    data.write_rest(&mut io::sink())?;
                    Ok(true)
                })
                .unwrap_err();
            assert!(
                matches!(error, Error::BlockDecode { block_index: 0, .. }),
                "{error}"
            );
        }
    }
}

#[test]
fn finite_copy_budgets_preserve_admission_and_charged_buffer_delivery() {
    let (_, bytes, _, payloads) = fixture(false, &[2 * 1024 * 1024 + 31]);
    for budget in [64 * 1024, 2 * 1024 * 1024] {
        let limits = ArchiveLimits {
            memory_limit_bytes: budget,
            ..ArchiveLimits::default()
        };
        let mut reader =
            ArchiveReader::with_limits(Cursor::new(bytes.as_slice()), Password::empty(), limits)
                .unwrap();
        let mut output = Writes::default();
        reader
            .for_each_entries_direct(|_, entry| {
                entry.write_rest(&mut output)?;
                Ok(true)
            })
            .unwrap();
        assert_eq!(output.bytes, payloads[0]);
        assert!(output.sizes.iter().all(|&size| size <= 64 * 1024));
    }
}

#[test]
fn finite_aes_copy_push_reuses_plaintext_with_mixed_reads_and_short_writes() {
    let (archive, bytes, password, payloads) = fixture(true, &[19, (2 << 20) + 31]);
    let memory = archive.decoder_memory_estimate().unwrap();
    assert_eq!(memory, (1 << 20) + (64 << 10));
    for threads in [1, 4] {
        for partial in [false, true] {
            let mut reader = ArchiveReader::with_limits(
                Cursor::new(bytes.as_slice()),
                (*password).clone(),
                ArchiveLimits {
                    memory_limit_bytes: memory,
                    ..ArchiveLimits::default()
                },
            )
            .unwrap();
            reader.set_threads(threads);
            let mut index = 0;
            reader
                .for_each_entries_direct(|entry, data| {
                    let mut prefix = [0; 13];
                    data.read_exact(&mut prefix)?;
                    let mut output = Writes {
                        partial,
                        ..Default::default()
                    };
                    assert_eq!(data.write_rest(&mut output)? + 13, entry.size);
                    assert_eq!(prefix, payloads[index][..13]);
                    assert_eq!(output.bytes, payloads[index][13..]);
                    if index == 1 && !partial {
                        assert!(output.sizes.contains(&(64 << 10)));
                        assert!(output.sizes.iter().all(|&size| size <= 64 << 10));
                    }
                    assert_eq!(data.write_rest(&mut io::sink())?, 0);
                    index += 1;
                    Ok(true)
                })
                .unwrap();
            assert_eq!(index, 2);
        }
    }
}

#[test]
fn finite_copy_push_keeps_crc_damage_and_sink_failure_separate() {
    for aes in [false, true] {
        let (archive, bytes, password, _) = fixture(aes, &[100_003]);
        let limits = ArchiveLimits {
            memory_limit_bytes: archive.decoder_memory_estimate().unwrap(),
            ..ArchiveLimits::default()
        };
        let mut damaged = (*bytes).clone();
        damaged[archive.block_pack_streams(0)[0].offset as usize + 10] ^= 0x80;
        let mut reader =
            ArchiveReader::with_limits(Cursor::new(damaged), (*password).clone(), limits).unwrap();
        let error = reader
            .for_each_entries_direct(|_, data| {
                data.write_rest(&mut io::sink())?;
                Ok(true)
            })
            .unwrap_err();
        assert!(matches!(error, Error::BlockDecode { block_index: 0, .. }));

        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "destination refused",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut reader =
            ArchiveReader::with_limits(Cursor::new(bytes.as_slice()), (*password).clone(), limits)
                .unwrap();
        let completed = Arc::new(Mutex::new(0));
        let reports = Arc::clone(&completed);
        reader.set_sub_stream_complete_hook(move |_| *reports.lock().unwrap() += 1);
        let error = reader
            .for_each_entries_direct(|_, data| {
                data.write_rest(&mut Failed)?;
                Ok(true)
            })
            .unwrap_err();
        assert!(!matches!(error, Error::BlockDecode { .. }));
        assert!(error.to_string().contains("destination refused"));
        assert_eq!(*completed.lock().unwrap(), 0);
    }
}
