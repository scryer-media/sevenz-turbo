//! Folder-parallel decode and encode of non-solid archives.
//!
//! Decoding several folders at once, from a positional source, and coding
//! several at once on the write side, must be invisible in what comes out:
//! the same entries, bytes, hook calls and errors, in the same order, as the
//! one-folder-at-a-time path, and on the write side the same archive bytes.
//! Every case here is run against that sequential path, which is also what a
//! thread count of one and a target without threads take.
#![cfg(feature = "compress")]

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
        out.push((x >> 8) as u8);
    }
    out.truncate(len);
    out
}

/// Bytes shaped like x86 code, so that BCJ2 has calls and jumps to split out.
fn code_like(len: usize, seed: u64) -> Vec<u8> {
    let mut out = payload(len, seed);
    let mut i = (seed % 7) as usize;
    while i + 5 <= out.len() {
        out[i] = if i.is_multiple_of(3) { 0xE9 } else { 0xE8 };
        out[i + 1..i + 5].copy_from_slice(&((i as u32 * 3) % 0x4000).to_le_bytes());
        i += 11;
    }
    out
}

/// One member of a test tree.
#[derive(Clone)]
struct Member {
    name: String,
    directory: bool,
    data: Option<Vec<u8>>,
}

/// A tree of small members with the shapes a real one has: empty files,
/// directories, one-byte files and a range of sizes, and - when `large` is
/// given - one member too large to stage, part-way through, so that a decode
/// switches between the parallel runs and the folder decoded alone.
fn members(count: usize, large: Option<usize>, code: bool) -> Vec<Member> {
    let mut out = Vec::new();
    for i in 0..count {
        let name = format!("granite_path/part-{i:04}.bin");
        let member = match i % 17 {
            3 => Member {
                name: format!("granite_path/dir-{i:04}"),
                directory: true,
                data: None,
            },
            5 => Member {
                name,
                directory: false,
                data: Some(Vec::new()),
            },
            7 => Member {
                name,
                directory: false,
                data: Some(vec![i as u8]),
            },
            _ => {
                let len = 200 + (i * 7919) % 90_000;
                let data = if code {
                    code_like(len, i as u64)
                } else {
                    payload(len, i as u64)
                };
                Member {
                    name,
                    directory: false,
                    data: Some(data),
                }
            }
        };
        out.push(member);
        if large == Some(i) {
            out.push(Member {
                name: format!("granite_path/large-{i:04}.bin"),
                directory: false,
                data: Some(payload(9 << 20, 0xC0FFEE)),
            });
        }
    }
    out
}

fn entry(member: &Member) -> ArchiveEntry {
    let mut entry = if member.directory {
        ArchiveEntry::new_directory(&member.name)
    } else {
        ArchiveEntry::new_file(&member.name)
    };
    // What `ArchiveEntry::from_path` records: the size the reader will yield.
    entry.size = member.data.as_ref().map_or(0, |d| d.len() as u64);
    entry
}

/// The archive a loop of `push_archive_entry` writes.
fn write_sequential(methods: &[EncoderConfiguration], members: &[Member]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods.to_vec());
    for member in members {
        let reader = member.data.as_deref().map(Cursor::new);
        writer
            .push_archive_entry(entry(member), reader)
            .expect("push");
    }
    writer.finish().expect("finish").into_inner()
}

/// The same archive, through the folder-parallel writer.
fn write_parallel(methods: &[EncoderConfiguration], members: &[Member], threads: u32) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods.to_vec());
    let entries = members.iter().map(entry).collect();
    writer
        .push_archive_entries_non_solid(
            entries,
            |index, _| Ok(members[index].data.as_deref().map(Cursor::new)),
            threads,
        )
        .expect("push");
    writer.finish().expect("finish").into_inner()
}

/// Everything a decode shows its caller, in the order it showed it.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Entry(String, Vec<u8>),
    SubStream(usize, usize, usize, u64, u64, u32),
    Block(usize, u64, bool),
}

/// How a decode reads its source.
#[derive(Debug, Clone, Copy)]
enum Source {
    /// A plain `Read + Seek`: every folder on the calling thread.
    Sequential,
    /// Bytes in memory, read positionally.
    Memory,
    /// A `Read + Seek` behind the serialising fallback.
    Serial,
    /// A file on disk, through `ArchiveReader::open`.
    File,
}

fn decode(
    archive: &[u8],
    password: &Password,
    source: Source,
    threads: u32,
) -> Result<Vec<Seen>, (Error, Vec<Seen>)> {
    let tmp;
    let mut reader: Box<dyn DecodeAll> = match source {
        Source::Sequential => Box::new(
            ArchiveReader::new(Cursor::new(archive.to_vec()), password.clone()).expect("header"),
        ),
        Source::Memory => Box::new(
            ArchiveReader::from_read_at(
                archive.to_vec(),
                password.clone(),
                ArchiveLimits::default(),
            )
            .expect("header"),
        ),
        Source::Serial => Box::new(
            ArchiveReader::from_read_at(
                SerialReadAt::new(Cursor::new(archive.to_vec())),
                password.clone(),
                ArchiveLimits::default(),
            )
            .expect("header"),
        ),
        Source::File => {
            tmp = tempfile::NamedTempFile::new().expect("temp file");
            std::fs::write(tmp.path(), archive).expect("write archive");
            Box::new(ArchiveReader::open(tmp.path(), password.clone()).expect("header"))
        }
    };
    reader.decode_all(threads)
}

/// One decode loop over any reader type.
trait DecodeAll {
    fn decode_all(&mut self, threads: u32) -> Result<Vec<Seen>, (Error, Vec<Seen>)>;
}

impl<R: Read + std::io::Seek> DecodeAll for ArchiveReader<R> {
    fn decode_all(&mut self, threads: u32) -> Result<Vec<Seen>, (Error, Vec<Seen>)> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        self.set_threads(threads);
        let hook = Arc::clone(&seen);
        self.set_sub_stream_complete_hook(move |done| {
            hook.lock().unwrap().push(Seen::SubStream(
                done.block_index,
                done.sub_stream_index,
                done.file_index,
                done.unpacked_offset,
                done.len,
                done.crc32,
            ));
        });
        let hook = Arc::clone(&seen);
        self.set_block_complete_hook(move |done| {
            hook.lock().unwrap().push(Seen::Block(
                done.block_index,
                done.unpacked_size,
                done.crc_verified,
            ));
        });
        let entries = Arc::clone(&seen);
        let result = self.for_each_entries(|entry, rd| {
            let mut bytes = Vec::new();
            rd.read_to_end(&mut bytes)?;
            entries
                .lock()
                .unwrap()
                .push(Seen::Entry(entry.name().to_string(), bytes));
            Ok(true)
        });
        self.clear_sub_stream_complete_hook();
        self.clear_block_complete_hook();
        let seen = std::mem::take(&mut *seen.lock().unwrap());
        match result {
            Ok(()) => Ok(seen),
            Err(error) => Err((error, seen)),
        }
    }
}

const POSITIONAL: [Source; 3] = [Source::Memory, Source::Serial, Source::File];

/// Every positional source and thread count decodes `archive` to exactly what
/// the sequential path shows.
fn assert_decodes_like_sequential(archive: &[u8], password: &Password, expected_entries: usize) {
    let reference = decode(archive, password, Source::Sequential, 1).expect("sequential decode");
    let entries = reference
        .iter()
        .filter(|seen| matches!(seen, Seen::Entry(..)))
        .count();
    assert_eq!(
        entries, expected_entries,
        "the sequential decode saw every member"
    );
    for source in POSITIONAL {
        for threads in [1, 2, 3, 8] {
            let got = decode(archive, password, source, threads)
                .unwrap_or_else(|(e, _)| panic!("{source:?} at {threads} threads: {e}"));
            assert!(
                got == reference,
                "{source:?} at {threads} threads differs from the sequential decode"
            );
        }
    }
    // The sequential path itself, at a thread count that only its own LZMA2
    // coder can use.
    let got = decode(archive, password, Source::Sequential, 8).expect("sequential at 8");
    assert!(got == reference, "the sequential path at 8 threads differs");
}

#[test]
fn lzma2_members_decode_and_encode_the_same_in_parallel() {
    let methods = vec![Lzma2Options::from_level(5).into()];
    let tree = members(120, Some(60), false);
    let sequential = write_sequential(&methods, &tree);
    for threads in [1, 2, 4, 8] {
        let parallel = write_parallel(&methods, &tree, threads);
        assert!(
            parallel == sequential,
            "the parallel writer at {threads} threads wrote different bytes"
        );
    }
    assert_decodes_like_sequential(&sequential, &Password::empty(), tree.len());
}

#[test]
fn block_parallel_lzma2_settings_keep_large_folders_on_the_calling_thread() {
    // A multi-threaded LZMA2 configuration: the small folders each fit a
    // block and go to the workers; the large one codes with block threads of
    // its own, alone. The archive is still the sequential loop's.
    let mut options = Lzma2Options::from_level_mt(3, 4, 1 << 20);
    options.set_dictionary_size(1 << 20);
    let methods = vec![options.into()];
    let tree = members(60, Some(30), false);
    let sequential = write_sequential(&methods, &tree);
    let parallel = write_parallel(&methods, &tree, 8);
    assert!(
        parallel == sequential,
        "the parallel writer wrote different bytes"
    );
    assert_decodes_like_sequential(&sequential, &Password::empty(), tree.len());
}

#[cfg(feature = "aes256")]
#[test]
fn aes_members_decode_and_encode_the_same_in_parallel() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;
    let password = Password::from("amber-quarry-passphrase");
    let methods = vec![
        AesEncoderOptions {
            password: password.clone(),
            iv: [7; 16],
            salt: [3; 16],
            num_cycles_power: 10,
        }
        .into(),
        Lzma2Options::from_level(3).into(),
    ];
    let tree = members(80, Some(40), false);
    let sequential = write_sequential(&methods, &tree);
    let parallel = write_parallel(&methods, &tree, 6);
    // Every folder draws its own random IV, so two writes never match byte
    // for byte (even in length: the IVs in the header do not compress alike);
    // what must match is what they decode to. `sevenzip_encode_tests` checks
    // the IVs themselves.
    let reference =
        decode(&sequential, &password, Source::Sequential, 1).expect("sequential decode");
    let got = decode(&parallel, &password, Source::Sequential, 1).expect("parallel write");
    assert!(
        got == reference,
        "the parallel writer's archive decodes differently"
    );
    assert_decodes_like_sequential(&parallel, &password, tree.len());
}

#[test]
fn bcj2_members_decode_and_encode_the_same_in_parallel() {
    let methods = vec![
        Lzma2Options::from_level(5).into(),
        EncoderMethod::BCJ2_FILTER.into(),
    ];
    let tree = members(70, None, true);
    let sequential = write_sequential(&methods, &tree);
    let parallel = write_parallel(&methods, &tree, 8);
    assert!(
        parallel == sequential,
        "the parallel writer wrote different bytes"
    );
    assert_decodes_like_sequential(&sequential, &Password::empty(), tree.len());
}

#[test]
fn filtered_members_decode_the_same_in_parallel() {
    // A filter over LZMA2 checks each member on the consuming side of the
    // chain; delta and BCJ each take that path.
    for filter in [
        EncoderConfiguration::from(sevenz_turbo::encoder_options::DeltaOptions::from_distance(
            4,
        )),
        EncoderMethod::BCJ_X86_FILTER.into(),
    ] {
        let methods = vec![Lzma2Options::from_level(3).into(), filter];
        let tree = members(50, None, true);
        let archive = write_parallel(&methods, &tree, 4);
        assert!(archive == write_sequential(&methods, &tree));
        assert_decodes_like_sequential(&archive, &Password::empty(), tree.len());
    }
}

/// An archive whose members are stored, so that flipping a byte of one
/// member's packed data damages exactly that member's checksum and nothing
/// else.
fn stored_archive(count: usize) -> (Vec<u8>, Vec<Member>) {
    let methods = vec![EncoderMethod::COPY.into()];
    let tree: Vec<Member> = (0..count)
        .map(|i| Member {
            name: format!("slate_ledger/record-{i:03}.dat"),
            directory: false,
            data: Some(payload(3000 + i * 13, i as u64)),
        })
        .collect();
    (write_parallel(&methods, &tree, 4), tree)
}

#[test]
fn a_corrupt_member_is_reported_in_its_own_folder() {
    let (mut archive, tree) = stored_archive(64);
    let damaged = 41;
    let parsed = Archive::read(&mut Cursor::new(&archive), &Password::empty()).expect("header");
    let range = parsed.block_pack_streams(damaged)[0];
    archive[(range.offset + range.size / 2) as usize] ^= 0x5A;

    let (expected, before) = decode(&archive, &Password::empty(), Source::Sequential, 1)
        .expect_err("the sequential decode refuses the damaged member");
    for source in POSITIONAL {
        for threads in [2, 8] {
            let (error, seen) = decode(&archive, &Password::empty(), source, threads)
                .expect_err("the parallel decode refuses the damaged member");
            match &error {
                Error::BlockDecode {
                    block_index,
                    packed_offset,
                    kind: BlockErrorKind::ChecksumMismatch,
                    ..
                } => {
                    assert_eq!(*block_index, damaged, "{source:?} at {threads} threads");
                    assert_eq!(*packed_offset, range.offset);
                }
                other => panic!("{source:?} at {threads} threads: {other:?}"),
            }
            assert_eq!(error.to_string(), expected.to_string());
            // Everything before the damaged member reached the caller, in
            // order, and nothing after it did.
            assert!(seen == before, "{source:?} at {threads} threads");
        }
    }
    let delivered = before
        .iter()
        .filter(|seen| matches!(seen, Seen::Entry(..)))
        .count();
    assert_eq!(delivered, damaged);
    assert!(tree.len() > damaged);
}

#[test]
fn a_corrupt_lzma2_member_fails_where_the_sequential_decode_fails() {
    let methods = vec![Lzma2Options::from_level(5).into()];
    let tree = members(40, None, false);
    let mut archive = write_sequential(&methods, &tree);
    let parsed = Archive::read(&mut Cursor::new(&archive), &Password::empty()).expect("header");
    let damaged = 23;
    let range = parsed.block_pack_streams(damaged)[0];
    for at in [range.offset + 2, range.offset + range.size / 2] {
        archive[at as usize] ^= 0xFF;
    }
    let (expected, before) =
        decode(&archive, &Password::empty(), Source::Sequential, 1).expect_err("damaged");
    let Error::BlockDecode { block_index, .. } = expected else {
        panic!("not located: {expected:?}");
    };
    assert_eq!(block_index, damaged);
    let (error, seen) =
        decode(&archive, &Password::empty(), Source::Memory, 8).expect_err("damaged");
    assert_eq!(error.to_string(), expected.to_string());
    assert!(seen == before);
}

#[test]
fn a_callback_error_is_the_callers_own_and_stops_the_decode() {
    let (archive, _) = stored_archive(32);
    for source in [Source::Sequential, Source::Memory] {
        let mut reader = ArchiveReader::from_read_at(
            archive.clone(),
            Password::empty(),
            ArchiveLimits::default(),
        )
        .expect("header");
        if matches!(source, Source::Sequential) {
            reader.clear_positional_source();
        }
        reader.set_threads(8);
        let mut calls = 0;
        let result = reader.for_each_entries(|_, rd| {
            calls += 1;
            let mut sink = Vec::new();
            rd.read_to_end(&mut sink)?;
            if calls == 10 {
                return Err(Error::Other("caller gave up".into()));
            }
            Ok(true)
        });
        assert!(matches!(result, Err(Error::Other(ref m)) if m == "caller gave up"));
        assert_eq!(calls, 10);
    }
}

#[test]
fn a_callback_that_stops_a_folder_moves_on_to_the_next() {
    // `Ok(false)` ends the folder, not the archive, on every path.
    let (archive, tree) = stored_archive(24);
    let mut reader =
        ArchiveReader::from_read_at(archive, Password::empty(), ArchiveLimits::default())
            .expect("header");
    reader.set_threads(4);
    let mut names = Vec::new();
    reader
        .for_each_entries(|entry, _| {
            names.push(entry.name().to_string());
            Ok(false)
        })
        .expect("decode");
    let expected: Vec<String> = tree.iter().map(|m| m.name.clone()).collect();
    assert_eq!(names, expected);
}

#[test]
fn a_memory_limit_still_decodes_in_order() {
    let methods = vec![Lzma2Options::from_level(3).into()];
    let tree = members(40, None, false);
    let archive = write_sequential(&methods, &tree);
    let reference = decode(&archive, &Password::empty(), Source::Sequential, 1).expect("decode");
    for limit in [24 << 20, 64 << 20, 512 << 20] {
        let mut reader = ArchiveReader::from_read_at(
            archive.clone(),
            Password::empty(),
            ArchiveLimits::memory(limit),
        )
        .expect("header");
        let got = reader.decode_all(8).expect("decode under a limit");
        assert!(got == reference, "limit {limit}");
    }
}
