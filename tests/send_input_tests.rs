#![cfg(feature = "compress")]

use sevenz_turbo::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, Password, SourceReader,
    encoder_options::{DeltaOptions, Lzma2Options},
};
use std::{
    cell::RefCell,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    rc::Rc,
};

// Deliberately !Send: the additive API must not strengthen the output bound.
struct LocalOutput(Rc<RefCell<Cursor<Vec<u8>>>>);
impl Write for LocalOutput {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for LocalOutput {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.borrow_mut().seek(pos)
    }
}
struct ShortInput(Cursor<Vec<u8>>);
impl Read for ShortInput {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = out.len().min(997);
        self.0.read(&mut out[..n])
    }
}

fn methods(threads: u32) -> Vec<EncoderConfiguration> {
    let mut options = Lzma2Options::from_level_mt(5, threads, 1 << 17);
    options.set_dictionary_size(1 << 16);
    vec![options.into()]
}

fn write_archive(
    data: &[Vec<u8>],
    methods: Vec<EncoderConfiguration>,
    send: bool,
    solid: bool,
    known: bool,
) -> Vec<u8> {
    let sizes: Vec<_> = data
        .iter()
        .map(|bytes| if known { bytes.len() as u64 } else { 0 })
        .collect();
    write_archive_with_sizes(data, methods, send, solid, &sizes)
}

fn write_archive_with_sizes(
    data: &[Vec<u8>],
    methods: Vec<EncoderConfiguration>,
    send: bool,
    solid: bool,
    sizes: &[u64],
) -> Vec<u8> {
    assert_eq!(data.len(), sizes.len());
    let shared = Rc::new(RefCell::new(Cursor::new(Vec::new())));
    let mut writer = ArchiveWriter::new(LocalOutput(Rc::clone(&shared))).unwrap();
    writer.set_content_methods(methods);
    let entries: Vec<_> = data
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let mut entry = ArchiveEntry::new_file(&format!("part-{i}"));
            entry.size = sizes[i];
            entry
        })
        .collect();
    let sources = data
        .iter()
        .map(|bytes| ShortInput(Cursor::new(bytes.clone())));
    if solid {
        let readers = sources.map(SourceReader::new).collect();
        if send {
            writer.push_archive_entries_send(entries, readers).unwrap();
        } else {
            writer.push_archive_entries(entries, readers).unwrap();
        }
    } else {
        for (entry, reader) in entries.into_iter().zip(sources) {
            if send {
                writer.push_archive_entry_send(entry, Some(reader)).unwrap();
            } else {
                writer.push_archive_entry(entry, Some(reader)).unwrap();
            }
        }
    }
    writer.finish().unwrap();
    shared.borrow().get_ref().clone()
}

fn verify(bytes: Vec<u8>, password: Password, data: &[Vec<u8>]) {
    let mut reader = ArchiveReader::new(Cursor::new(bytes), password).unwrap();
    let mut index = 0;
    reader
        .for_each_entries(|entry, input| {
            let mut actual = Vec::new();
            input.read_to_end(&mut actual)?;
            assert_eq!(actual, data[index]);
            assert_eq!(entry.size, actual.len() as u64);
            index += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(index, data.len());
}

#[test]
fn send_archives_preserve_bytes_crcs_headers_and_fallbacks() {
    let data = vec![
        (0..17003).map(|i| (i % 251) as u8).collect(),
        Vec::new(),
        vec![71; (1 << 17) + 53],
    ];
    for threads in [2, 4] {
        for solid in [false, true] {
            for known in [false, true] {
                let expected = write_archive(&data, methods(threads), false, solid, known);
                let actual = write_archive(&data, methods(threads), true, solid, known);
                assert_eq!(
                    actual, expected,
                    "threads={threads} solid={solid} known={known}"
                );
                verify(actual, Password::empty(), &data);
            }
        }
    }
    let mut filtered = methods(2);
    filtered.push(DeltaOptions::from_distance(1).into());
    let expected = write_archive(&data, filtered.clone(), false, true, true);
    let actual = write_archive(&data, filtered, true, true, true);
    assert_eq!(actual, expected);
    verify(actual, Password::empty(), &data);
}

#[test]
fn send_size_hints_do_not_limit_or_extend_the_actual_stream() {
    let data = vec![
        (0..(2 << 17) + 53).map(|i| (i % 251) as u8).collect(),
        Vec::new(),
        vec![37; 1939],
    ];
    for solid in [false, true] {
        // The nonzero empty-file hint also verifies that metadata is derived
        // from EOF, even when every declaration claims a nonempty source.
        for threads in [2, 4] {
            for sizes in [
                [1, 1, 1],
                [1 << 22, 1 << 20, 1 << 20],
                [1, 1 << 20, 1 << 22],
            ] {
                let expected =
                    write_archive_with_sizes(&data, methods(threads), false, solid, &sizes);
                let actual = write_archive_with_sizes(&data, methods(threads), true, solid, &sizes);
                assert_eq!(
                    actual, expected,
                    "threads={threads}, solid={solid}, sizes={sizes:?}"
                );
                verify(actual, Password::empty(), &data);
            }
        }
    }
}

#[test]
#[cfg(not(any(sevenz_turbo_unthreaded, target_family = "wasm")))]
fn send_archive_reader_panic_after_read_ahead_returns_to_the_caller() {
    use std::sync::{Arc, Mutex};
    const BEFORE_PANIC: usize = (2 << 20) + 31;
    struct PartialPanic {
        read: usize,
        calls: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    }
    impl Read for PartialPanic {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.calls.lock().unwrap().push(std::thread::current().id());
            if out.is_empty() {
                return Ok(0);
            }
            if self.read == BEFORE_PANIC {
                std::panic::panic_any(967usize);
            }
            let n = out.len().min(8191).min(BEFORE_PANIC - self.read);
            out[..n].fill(71);
            self.read += n;
            Ok(n)
        }
    }
    for solid in [false, true] {
        for known in [false, true] {
            let caller = std::thread::current().id();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let shared = Rc::new(RefCell::new(Cursor::new(Vec::new())));
            let mut writer = ArchiveWriter::new(LocalOutput(Rc::clone(&shared))).unwrap();
            writer.set_content_methods(methods(2));
            let mut entry = ArchiveEntry::new_file("partial");
            if known {
                entry.size = (BEFORE_PANIC + 1) as u64;
            }
            let source = PartialPanic {
                read: 0,
                calls: Arc::clone(&calls),
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if solid {
                    writer
                        .push_archive_entries_send(vec![entry], vec![SourceReader::new(source)])
                        .map(|_| ())
                } else {
                    writer
                        .push_archive_entry_send(entry, Some(source))
                        .map(|_| ())
                }
            }));
            assert_eq!(*result.unwrap_err().downcast::<usize>().unwrap(), 967);
            let calls = calls.lock().unwrap();
            assert!(calls.iter().any(|id| *id != caller));
            assert_eq!(calls.contains(&caller), !known);
            assert!(
                shared.borrow().get_ref().len() > 32,
                "earlier output was written"
            );
            // A failed archive is not finalized or reused; lower-level tests
            // separately prove that its discarded coder can be rebuilt.
        }
    }
}

#[cfg(feature = "aes256")]
#[test]
fn send_archives_keep_aes_and_encrypted_headers_on_the_output_side() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;
    let data = vec![vec![31; (1 << 17) + 71], Vec::new(), vec![93; 9001]];
    let password = Password::from("test-only");
    let mut encrypted = vec![
        AesEncoderOptions::new(password.clone())
            .with_num_cycles_power(4)
            .into(),
    ];
    for threads in [2, 4] {
        encrypted.truncate(1);
        encrypted.extend(methods(threads));
        for solid in [false, true] {
            for send in [false, true] {
                // Each folder and header gets a fresh IV: compare decoded bytes
                // and the checked metadata rather than randomized ciphertext.
                verify(
                    write_archive(&data, encrypted.clone(), send, solid, true),
                    password.clone(),
                    &data,
                );
            }
        }
    }
}

#[test]
fn send_multiblock_fast_and_normal_archives_preserve_crc_and_sizes() {
    let data = vec![
        (0..(1 << 20) + 53).map(|i| (i % 251) as u8).collect(),
        Vec::new(),
    ];
    for level in [1, 5] {
        let mut options = Lzma2Options::from_level_mt(level, 4, 1 << 17);
        options.set_dictionary_size(1 << 16);
        let methods: Vec<EncoderConfiguration> = vec![options.into()];
        for solid in [false, true] {
            for known in [false, true] {
                let expected = write_archive(&data, methods.clone(), false, solid, known);
                let actual = write_archive(&data, methods.clone(), true, solid, known);
                assert_eq!(
                    actual, expected,
                    "level={level}, solid={solid}, known={known}"
                );
                verify(actual, Password::empty(), &data);
            }
        }
    }
}

#[test]
fn generic_archive_api_keeps_non_send_readers_and_writers() {
    struct LocalInput(Rc<RefCell<Cursor<Vec<u8>>>>);
    impl Read for LocalInput {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.0.borrow_mut().read(out)
        }
    }
    let bytes = b"local reader".to_vec();
    let shared = Rc::new(RefCell::new(Cursor::new(Vec::new())));
    let mut writer = ArchiveWriter::new(LocalOutput(Rc::clone(&shared))).unwrap();
    writer.set_content_methods(methods(2));
    writer
        .push_archive_entry(
            ArchiveEntry::new_file("local"),
            Some(LocalInput(Rc::new(RefCell::new(Cursor::new(
                bytes.clone(),
            ))))),
        )
        .unwrap();
    writer.finish().unwrap();
    let archive = shared.borrow().get_ref().clone();
    verify(archive, Password::empty(), &[bytes]);
}
