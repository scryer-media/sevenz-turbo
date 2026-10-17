//! Reader regressions found after 0.27.1: what a callback leaves unread, what
//! the block hook may claim, and the checksum of a file with no bytes.
//!
//! Every archive here is written by `ArchiveWriter` with LZMA2, so the tests
//! run under every feature set that can write one.
#![cfg(feature = "compress")]

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex};

use sevenz_turbo::encoder_options::Lzma2Options;
use sevenz_turbo::*;

/// Compressible bytes, different for every seed.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let word = b'a' + (x >> 40) as u8 % 26;
        out.extend(std::iter::repeat_n(word, 1 + (x % 7) as usize));
        out.push(b' ');
    }
    out.truncate(len);
    out
}

fn crc32(data: &[u8]) -> u32 {
    lzma_turbo::crc::crc32(data)
}

/// One solid LZMA2 block per entry of `blocks`; file `f` of block `b` is
/// named `b{b}f{f}`.
fn solid_archive(options: Lzma2Options, blocks: &[Vec<Vec<u8>>]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(vec![options.into()]);
    for (b, files) in blocks.iter().enumerate() {
        let entries = (0..files.len())
            .map(|f| ArchiveEntry::new_file(&format!("b{b}f{f}")))
            .collect();
        let readers = files
            .iter()
            .map(|data| SourceReader::new(Cursor::new(data.as_slice())))
            .collect();
        writer.push_archive_entries(entries, readers).expect("push");
    }
    writer.finish().expect("finish").into_inner()
}

/// The ways a whole archive is walked.
#[derive(Debug, Clone, Copy)]
enum Path {
    /// `for_each_entries_direct`, every folder on the calling thread.
    Direct,
    /// `for_each_entries_direct` over a positional source at two threads: the
    /// small folders are decoded by workers and replayed to the callback.
    Replay,
    /// `for_each_entries`, the `Read`-only entry point.
    Plain,
}

const PATHS: [Path; 3] = [Path::Direct, Path::Replay, Path::Plain];

/// A reader over `bytes` for `path`, its parsed archive first passed to
/// `tamper`.
fn open(
    bytes: &[u8],
    path: Path,
    tamper: impl FnOnce(&mut Archive),
) -> ArchiveReader<Cursor<Vec<u8>>> {
    let mut source = Cursor::new(bytes.to_vec());
    let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
    tamper(&mut archive);
    let mut reader = ArchiveReader::from_archive(archive, source, Password::empty());
    if let Path::Replay = path {
        reader.set_threads(2);
        reader.set_positional_source(bytes.to_vec());
    }
    reader
}

type Callback<'a> = dyn FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error> + 'a;

fn walk(
    reader: &mut ArchiveReader<Cursor<Vec<u8>>>,
    path: Path,
    each: &mut Callback<'_>,
) -> Result<(), Error> {
    match path {
        Path::Direct | Path::Replay => reader.for_each_entries_direct(|entry, rd| each(entry, rd)),
        Path::Plain => reader.for_each_entries(|entry, rd| each(entry, rd)),
    }
}

#[track_caller]
fn assert_checksum_mismatch<T>(result: Result<T, Error>, block: usize, what: &str) {
    match result.map(drop) {
        Err(Error::BlockDecode {
            block_index,
            kind: BlockErrorKind::ChecksumMismatch,
            ..
        }) if block_index == block => {}
        other => panic!("{what}: expected a checksum mismatch in block {block}, got {other:?}"),
    }
}

/// Two small solid blocks, so that two threads over a positional source
/// replay them from workers: three files, then two.
fn two_blocks() -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![payload(40_000, 1), payload(30_000, 2), payload(20_000, 3)],
        vec![payload(10_000, 4), payload(5_000, 5)],
    ]
}

/// A callback that reads a 7-byte prefix of the first file of a solid block
/// and asks to go on used to leave the rest of that file in the stream, so
/// the second file began inside the first: wrong bytes, and a CRC mismatch
/// blamed on a file that was fine. The rest is now read for it, through the
/// reader it was handed, so the files after it arrive whole and checked, and
/// a bad CRC on the partly read file is still caught.
#[test]
fn a_partly_read_file_does_not_shift_the_files_after_it() {
    let blocks = two_blocks();
    let bytes = solid_archive(Lzma2Options::from_level(1), &blocks);
    for path in PATHS {
        let mut reader = open(&bytes, path, |_| {});
        let reports = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::clone(&reports);
        reader.set_sub_stream_complete_hook(move |event| {
            hook.lock().unwrap().push((event.file_index, event.crc32));
        });
        let mut got = BTreeMap::new();
        walk(&mut reader, path, &mut |entry, rd| {
            let mut data = Vec::new();
            if entry.name() == "b0f0" {
                let mut prefix = [0u8; 7];
                rd.read_exact(&mut prefix)?;
                data.extend_from_slice(&prefix);
            } else {
                rd.read_to_end(&mut data)?;
            }
            got.insert(entry.name().to_string(), data);
            Ok(true)
        })
        .unwrap_or_else(|e| panic!("{path:?}: {e}"));

        assert_eq!(got["b0f0"], blocks[0][0][..7], "{path:?}");
        for (b, files) in blocks.iter().enumerate() {
            for (f, data) in files.iter().enumerate().filter(|&(f, _)| b + f > 0) {
                assert!(
                    got[&format!("b{b}f{f}")] == *data,
                    "{path:?}: b{b}f{f} is not its own bytes"
                );
            }
        }
        let reports = reports.lock().unwrap().clone();
        for (file_index, data) in [(1, &blocks[0][1]), (2, &blocks[0][2])] {
            assert!(
                reports.contains(&(file_index, crc32(data))),
                "{path:?}: file {file_index} was not reported checked: {reports:?}"
            );
        }

        // The bytes the callback skipped are still checked.
        let mut reader = open(&bytes, path, |archive| archive.files[0].crc ^= 1);
        let result = walk(&mut reader, path, &mut |entry, rd| {
            if entry.name() == "b0f0" {
                let mut prefix = [0u8; 7];
                rd.read_exact(&mut prefix)?;
            } else {
                rd.read_to_end(&mut Vec::new())?;
            }
            Ok(true)
        });
        assert_checksum_mismatch(result, 0, &format!("{path:?}"));
    }
}

/// How a callback treats each file.
#[derive(Debug, Clone, Copy)]
enum Reading {
    /// Reads every file and goes on.
    Everything,
    /// Reads a prefix of the first file and asks to stop.
    PrefixThenStop,
    /// Reads nothing at all, and goes on.
    NothingAndGoOn,
}

/// The `BlockCompletion` hook says a block's checksums were compared. That is
/// only true of a block whose every byte went through the callbacks' readers;
/// it used to fire with `crc_verified` for a block whose callback went on
/// without reading the last file, whose bytes nobody checked.
#[test]
fn block_completion_is_claimed_only_for_a_block_read_in_full() {
    let blocks = two_blocks();
    let bytes = solid_archive(Lzma2Options::from_level(1), &blocks);
    for path in PATHS {
        for reading in [
            Reading::Everything,
            Reading::PrefixThenStop,
            Reading::NothingAndGoOn,
        ] {
            let mut reader = open(&bytes, path, |_| {});
            let completions = Arc::new(Mutex::new(Vec::new()));
            let hook = Arc::clone(&completions);
            reader.set_block_complete_hook(move |event| {
                hook.lock()
                    .unwrap()
                    .push((event.block_index, event.crc_verified));
            });
            walk(&mut reader, path, &mut |_, rd| match reading {
                Reading::Everything => {
                    rd.read_to_end(&mut Vec::new())?;
                    Ok(true)
                }
                Reading::PrefixThenStop => {
                    rd.read_exact(&mut [0u8; 7])?;
                    Ok(false)
                }
                Reading::NothingAndGoOn => Ok(true),
            })
            .unwrap_or_else(|e| panic!("{path:?} {reading:?}: {e}"));
            let completions = completions.lock().unwrap().clone();
            match reading {
                Reading::Everything => {
                    assert_eq!(completions, [(0, true), (1, true)], "{path:?} {reading:?}");
                }
                _ => assert!(
                    !completions.iter().any(|&(_, verified)| verified),
                    "{path:?} {reading:?}: a block was claimed checked: {completions:?}"
                ),
            }
        }
    }
}

/// A file with no bytes inside a solid block has a CRC too, of no bytes,
/// which is zero; 7-Zip compares it. On one thread, and on two where the
/// workers' checksums are folded, a non-zero one is a located mismatch, and
/// a well-formed archive of interleaved empty files decodes and checks.
#[test]
fn zero_length_files_in_a_solid_block_are_checked() {
    let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
    options.set_dictionary_size(1 << 20);
    let files = vec![
        Vec::new(),
        payload(3 << 20, 11),
        Vec::new(),
        payload(3 << 20, 12),
    ];
    let bytes = solid_archive(options, std::slice::from_ref(&files));

    for threads in [1, 2] {
        let decode = |tamper: &dyn Fn(&mut Archive)| {
            let mut source = Cursor::new(bytes.clone());
            let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
            assert_eq!(archive.blocks.len(), 1);
            tamper(&mut archive);
            let mut reader = ArchiveReader::from_archive(archive, source, Password::empty());
            reader.set_threads(threads);
            let handle = reader.lzma2_handle();
            let mut engaged = false;
            let mut got = Vec::new();
            let result = reader.for_each_entries(|_, rd| {
                let mut data = Vec::new();
                rd.read_to_end(&mut data)?;
                engaged |= handle.progress().is_some();
                got.push(data);
                Ok(true)
            });
            (result.map(|()| got), engaged)
        };

        let (result, engaged) = decode(&|_| {});
        assert_eq!(
            result.expect("a well-formed archive decodes"),
            files,
            "at {threads} threads"
        );
        if threads > 1 {
            assert!(
                engaged,
                "the parallel path was never engaged, so this proves nothing"
            );
        }

        // The empty file between the two that have bytes: inside the block's
        // span whether the writer gave it a stream or not.
        let (result, _) = decode(&|archive| {
            let file = &mut archive.files[2];
            assert_eq!(file.size, 0);
            file.has_crc = true;
            file.crc = 0x1234_5678;
        });
        assert_checksum_mismatch(result, 0, &format!("at {threads} threads"));

        // Verification off: not compared.
        let mut source = Cursor::new(bytes.clone());
        let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
        archive.files[2].has_crc = true;
        archive.files[2].crc = 0x1234_5678;
        let mut reader = ArchiveReader::from_archive(archive, source, Password::empty());
        reader.set_threads(threads);
        reader.set_verify_checksums(false);
        reader
            .for_each_entries(|_, rd| {
                rd.read_to_end(&mut Vec::new())?;
                Ok(true)
            })
            .unwrap_or_else(|e| panic!("at {threads} threads with verification off: {e}"));
    }
}

/// A callback handed a file's failure that goes on as if nothing happened
/// gets it back as the block's error, on every path: no file after it is
/// handed out, and the block is never reported checked. The sequential paths
/// used to forget the failure, hand out the rest and finish `Ok`, with the
/// block hook claiming a verified block; a replayed folder's worker stops at
/// the failure. The failure keeps its payload, so a checksum mismatch comes
/// back as one and not as a bare I/O error.
#[test]
fn a_failure_the_callback_goes_on_from_fails_the_block_on_every_path() {
    let blocks = two_blocks();
    let bytes = solid_archive(Lzma2Options::from_level(1), &blocks);
    for path in PATHS {
        let mut reader = open(&bytes, path, |archive| archive.files[1].crc ^= 1);
        let completed = Arc::new(Mutex::new(Vec::new()));
        let hook = Arc::clone(&completed);
        reader.set_block_complete_hook(move |event| {
            hook.lock().unwrap().push(event.block_index);
        });
        let mut handed = None;
        let mut after = Vec::new();
        let result = walk(&mut reader, path, &mut |entry, rd| {
            if handed.is_some() {
                after.push(entry.name().to_string());
            }
            let mut data = Vec::new();
            if let Err(error) = rd.read_to_end(&mut data) {
                handed.get_or_insert(error);
            }
            Ok(true)
        });
        let handed =
            handed.unwrap_or_else(|| panic!("{path:?}: the callback was handed the failure"));
        assert!(
            handed
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<Error>())
                .is_some_and(Error::has_checksum_mismatch),
            "{path:?}: {handed:?}"
        );
        assert_checksum_mismatch(result, 0, &format!("{path:?}"));
        assert_eq!(after, Vec::<String>::new(), "{path:?}: handed out after");
        assert_eq!(
            *completed.lock().unwrap(),
            Vec::<usize>::new(),
            "{path:?}: a block was reported checked"
        );
    }
}
