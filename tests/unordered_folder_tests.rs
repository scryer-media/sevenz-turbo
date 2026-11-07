//! `ArchiveReader::for_each_entries_unordered`: the folders of a non-solid
//! archive decoded side by side whatever their size, each entry handed over as
//! it is decoded.
//!
//! Every case holds the unordered walk to the ordered one: the same entries,
//! byte for byte, every checksum verified, and the same error from the same
//! folder. Concurrency is shown by a source on which the first read of every
//! large folder waits until every one of them has been read, which a walk that
//! decodes them one after another can never get past.
#![cfg(feature = "compress")]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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

/// One member: a name, and its bytes or none for a directory.
type Member = (String, Option<Vec<u8>>);

/// A non-solid archive, one folder per member with bytes.
fn write(methods: &[EncoderConfiguration], members: &[Member]) -> Vec<u8> {
    write_with(methods, members, true)
}

/// As [`write`], with the header encrypted or in the clear.
fn write_with(
    methods: &[EncoderConfiguration],
    members: &[Member],
    encrypt_header: bool,
) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods.to_vec());
    writer.set_encrypt_header(encrypt_header);
    for (name, data) in members {
        let mut entry = match data {
            None => ArchiveEntry::new_directory(name),
            Some(_) => ArchiveEntry::new_file(name),
        };
        entry.size = data.as_ref().map_or(0, |d| d.len() as u64);
        writer
            .push_archive_entry(entry, data.as_deref().map(Cursor::new))
            .expect("push");
    }
    writer.finish().expect("finish").into_inner()
}

/// A solid archive of `members`, all in one folder.
fn write_solid(methods: &[EncoderConfiguration], members: &[Member]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods.to_vec());
    let entries: Vec<ArchiveEntry> = members
        .iter()
        .map(|(name, data)| {
            let mut entry = ArchiveEntry::new_file(name);
            entry.size = data.as_ref().map_or(0, |d| d.len() as u64);
            entry
        })
        .collect();
    let readers = members
        .iter()
        .map(|(_, data)| SourceReader::new(Cursor::new(data.clone().unwrap_or_default())))
        .collect();
    writer
        .push_archive_entries(entries, readers)
        .expect("push solid");
    writer.finish().expect("finish").into_inner()
}

/// A tree with directories, empty files, small files and `large` members of
/// `large_len` bytes spread through it.
fn tree(small: usize, large: usize, large_len: usize) -> Vec<Member> {
    let mut out = Vec::new();
    for i in 0..small {
        let name = format!("quartz_lane/part-{i:03}.bin");
        out.push(match i % 7 {
            2 => (format!("quartz_lane/dir-{i:03}"), None),
            4 => (name, Some(Vec::new())),
            _ => (name, Some(payload(300 + (i * 7919) % 60_000, i as u64))),
        });
        if i % 3 == 1 && out.iter().filter(|(n, _)| n.contains("large")).count() < large {
            out.push((
                format!("quartz_lane/large-{i:03}.bin"),
                Some(payload(large_len, 0xABC0 + i as u64)),
            ));
        }
    }
    out
}

/// What the ordered walk hands over: each entry's index and bytes, in order.
fn ordered(archive: &[u8], password: &Password) -> Result<Vec<(usize, Vec<u8>)>, Error> {
    let mut reader = ArchiveReader::new(Cursor::new(archive.to_vec()), password.clone())?;
    let files: Vec<String> = reader
        .archive()
        .files
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    let mut seen: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut taken = BTreeSet::new();
    let result = reader.for_each_entries(|entry, rd| {
        let mut bytes = Vec::new();
        rd.read_to_end(&mut bytes)?;
        // Names are unique in these trees.
        let index = files.iter().position(|n| n == entry.name()).unwrap();
        assert!(taken.insert(index));
        seen.push((index, bytes));
        Ok(true)
    });
    result.map(|()| seen)
}

/// What an unordered walk handed over, checked as it came: each entry begun
/// once, its bytes contiguous from offset zero, then ended once, and no event
/// for an entry outside that.
#[derive(Default, Debug)]
struct Heard {
    begun: Vec<usize>,
    bytes: BTreeMap<usize, Vec<u8>>,
    ended: Vec<usize>,
    /// Whether entries of different folders interleaved.
    interleaved: bool,
}

fn unordered_with(
    reader: &mut ArchiveReader<impl Read + std::io::Seek>,
    stop_after: Option<usize>,
) -> (Result<(), Error>, Heard) {
    let mut heard = Heard::default();
    let mut open: BTreeSet<usize> = BTreeSet::new();
    let mut events = 0usize;
    let result = reader.for_each_entries_unordered(|event| {
        events += 1;
        match event {
            UnorderedEntry::Begin { file_index, .. } => {
                assert!(!heard.begun.contains(&file_index), "begun twice");
                if !open.is_empty() {
                    heard.interleaved = true;
                }
                open.insert(file_index);
                heard.begun.push(file_index);
                heard.bytes.insert(file_index, Vec::new());
            }
            UnorderedEntry::Data {
                file_index,
                offset,
                bytes,
            } => {
                assert!(open.contains(&file_index), "bytes of an entry not open");
                let got = heard.bytes.get_mut(&file_index).unwrap();
                assert_eq!(offset, got.len() as u64, "bytes out of order");
                got.extend_from_slice(bytes);
            }
            UnorderedEntry::End { file_index } => {
                assert!(open.remove(&file_index), "ended an entry not open");
                heard.ended.push(file_index);
            }
            _ => unreachable!(),
        }
        Ok(stop_after.is_none_or(|at| events < at))
    });
    (result, heard)
}

fn unordered(
    archive: &[u8],
    password: &Password,
    threads: u32,
    limits: ArchiveLimits,
) -> (Result<(), Error>, Heard) {
    let mut reader =
        ArchiveReader::from_read_at(archive.to_vec(), password.clone(), limits).expect("header");
    reader.set_threads(threads);
    unordered_with(&mut reader, None)
}

/// Every thread count and limit decodes `archive` to what the ordered walk
/// decodes it to, entry for entry.
fn assert_like_ordered(archive: &[u8], password: &Password) -> Heard {
    let reference = ordered(archive, password).expect("ordered decode");
    let order: Vec<usize> = reference.iter().map(|(index, _)| *index).collect();
    let reader =
        ArchiveReader::from_read_at(archive.to_vec(), password.clone(), ArchiveLimits::default())
            .expect("header");
    assert_eq!(reader.entry_order(), order, "entry order");
    let mut last = Heard::default();
    for threads in [1, 2, 3, 8] {
        for limits in [ArchiveLimits::default(), ArchiveLimits::memory(256 << 20)] {
            let (result, heard) = unordered(archive, password, threads, limits);
            result.unwrap_or_else(|e| panic!("{threads} threads: {e}"));
            let mut begun = heard.begun.clone();
            begun.sort_unstable();
            let mut expected = order.clone();
            expected.sort_unstable();
            assert_eq!(begun, expected, "{threads} threads: entries");
            assert_eq!(heard.ended.len(), order.len(), "{threads} threads: ends");
            for (index, bytes) in &reference {
                assert!(
                    heard.bytes[index] == *bytes,
                    "{threads} threads: entry {index} differs"
                );
            }
            if threads == 1 {
                assert_eq!(heard.begun, order, "one thread walks in order");
                assert!(!heard.interleaved);
            }
            last = heard;
        }
    }
    last
}

#[test]
fn small_and_large_members_decode_like_the_ordered_walk() {
    let methods = vec![Lzma2Options::from_level(3).into()];
    let members = tree(40, 3, 9 << 20);
    let archive = write(&methods, &members);
    assert_like_ordered(&archive, &Password::empty());
}

#[test]
fn members_of_several_lzma2_runs_decode_like_the_ordered_walk() {
    // A 1 MiB dictionary cut into 1 MiB runs: each 9 MiB folder is longer
    // than one encoder run, so its coder keeps the threads the plan gives it.
    let mut options = Lzma2Options::from_level_mt(3, 2, 1 << 20);
    options.set_dictionary_size(1 << 20);
    let members = tree(9, 3, 9 << 20);
    let archive = write(&[options.into()], &members);
    assert_like_ordered(&archive, &Password::empty());
}

#[test]
fn bcj2_and_filtered_members_decode_like_the_ordered_walk() {
    for methods in [
        vec![
            Lzma2Options::from_level(3).into(),
            EncoderMethod::BCJ2_FILTER.into(),
        ],
        vec![
            Lzma2Options::from_level(3).into(),
            EncoderMethod::BCJ_X86_FILTER.into(),
        ],
        vec![EncoderMethod::COPY.into()],
    ] {
        let archive = write(&methods, &tree(30, 0, 0));
        assert_like_ordered(&archive, &Password::empty());
    }
}

#[cfg(feature = "aes256")]
#[test]
fn encrypted_members_decode_like_the_ordered_walk() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;
    let password = Password::from("cobalt-ridge-passphrase");
    let methods = vec![
        AesEncoderOptions {
            password: password.clone(),
            iv: [5; 16],
            salt: [9; 16],
            num_cycles_power: 10,
        }
        .into(),
        Lzma2Options::from_level(3).into(),
    ];
    let archive = write(&methods, &tree(24, 0, 0));
    assert_like_ordered(&archive, &password);
    // With the header in the clear, a wrong password fails as the ordered
    // walk fails, in the same folder.
    let archive = write_with(&methods, &tree(24, 0, 0), false);
    assert_like_ordered(&archive, &password);
    let wrong = Password::from("not-the-passphrase");
    let expected = ordered(&archive, &wrong).expect_err("wrong password");
    let (result, _) = unordered(&archive, &wrong, 8, ArchiveLimits::default());
    let error = result.expect_err("wrong password");
    assert_eq!(error.to_string(), expected.to_string());
    assert_eq!(
        error.is_password_required(),
        expected.is_password_required()
    );
}

#[test]
fn a_solid_archive_decodes_as_the_ordered_walk_does() {
    let methods = vec![Lzma2Options::from_level(3).into()];
    let members: Vec<Member> = (0..12)
        .map(|i| (format!("basalt/solid-{i:02}.bin"), Some(payload(70_000, i))))
        .collect();
    let archive = write_solid(&methods, &members);
    let heard = assert_like_ordered(&archive, &Password::empty());
    // One folder: nothing to interleave with.
    assert!(!heard.interleaved);
}

/// A source on which the first read inside each of `ranges` waits until
/// every range has been read once.
struct Rendezvous {
    bytes: Vec<u8>,
    ranges: Vec<(u64, u64)>,
    armed: AtomicBool,
    arrived: Mutex<BTreeSet<usize>>,
    all: Condvar,
}

impl ReadAt for Rendezvous {
    fn read_at(&self, into: &mut [u8], offset: u64) -> std::io::Result<usize> {
        if self.armed.load(Ordering::Acquire)
            && let Some(range) = self
                .ranges
                .iter()
                .position(|&(start, len)| offset >= start && offset < start + len)
        {
            let mut arrived = self.arrived.lock().unwrap();
            if arrived.insert(range) {
                self.all.notify_all();
            }
            while arrived.len() < self.ranges.len() {
                arrived = self.all.wait(arrived).unwrap();
            }
        }
        self.bytes.as_slice().read_at(into, offset)
    }

    fn size(&self) -> std::io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
}

#[test]
fn large_folders_decode_side_by_side() {
    // Four members far over the 8 MiB the ordered walk stages, one LZMA2
    // block each. Each one's first read waits for the other three's.
    let methods = vec![Lzma2Options::from_level(1).into()];
    let members: Vec<Member> = (0..4)
        .map(|i| {
            (
                format!("onyx/large-{i}.bin"),
                Some(payload(10 << 20, 77 + i)),
            )
        })
        .collect();
    let archive = write(&methods, &members);
    let parsed = Archive::read(&mut Cursor::new(&archive), &Password::empty()).expect("header");
    let ranges = (0..parsed.blocks.len())
        .map(|block| {
            let range = parsed.block_pack_streams(block)[0];
            (range.offset, range.size)
        })
        .collect();
    let source = Arc::new(Rendezvous {
        bytes: archive.clone(),
        ranges,
        armed: AtomicBool::new(false),
        arrived: Mutex::default(),
        all: Condvar::new(),
    });
    let mut reader = ArchiveReader::from_read_at(
        Arc::clone(&source),
        Password::empty(),
        ArchiveLimits::memory(1 << 30),
    )
    .expect("header");
    reader.set_threads(4);
    source.armed.store(true, Ordering::Release);
    let (result, heard) = unordered_with(&mut reader, None);
    result.expect("unordered decode");
    assert!(heard.interleaved, "the four folders interleave");
    let reference = ordered(&archive, &Password::empty()).expect("ordered decode");
    for (index, bytes) in &reference {
        assert!(heard.bytes[index] == *bytes, "entry {index} differs");
    }
    assert_eq!(heard.ended.len(), 4);
}

#[test]
fn a_corrupt_folder_fails_where_the_ordered_walk_fails() {
    let methods = vec![Lzma2Options::from_level(3).into()];
    let members: Vec<Member> = (0..24)
        .map(|i| {
            (
                format!("jade/record-{i:02}.dat"),
                Some(payload(40_000 + i * 97, i as u64)),
            )
        })
        .collect();
    let mut archive = write(&methods, &members);
    let parsed = Archive::read(&mut Cursor::new(&archive), &Password::empty()).expect("header");
    let damaged = 13;
    let range = parsed.block_pack_streams(damaged)[0];
    for at in [range.offset + 2, range.offset + range.size / 2] {
        archive[at as usize] ^= 0xFF;
    }
    let expected = ordered(&archive, &Password::empty()).expect_err("damaged");
    let Error::BlockDecode { block_index, .. } = &expected else {
        panic!("not located: {expected:?}");
    };
    assert_eq!(*block_index, damaged);
    for threads in [2, 3, 8] {
        let (result, heard) = unordered(
            &archive,
            &Password::empty(),
            threads,
            ArchiveLimits::default(),
        );
        let error = result.expect_err("damaged");
        assert_eq!(error.to_string(), expected.to_string(), "{threads} threads");
        // Every folder before the damaged one ran to its end; the damaged
        // one never ended.
        for block in 0..damaged {
            let file = parsed.stream_map.block_first_file_index[block];
            assert!(
                heard.ended.contains(&file),
                "{threads} threads: {block} ended"
            );
        }
        let file = parsed.stream_map.block_first_file_index[damaged];
        assert!(!heard.ended.contains(&file));
    }
}

#[test]
fn a_callback_that_stops_ends_the_decode() {
    let methods = vec![Lzma2Options::from_level(3).into()];
    let archive = write(&methods, &tree(40, 2, 9 << 20));
    for stop_after in [1, 2, 5, 40] {
        let mut reader = ArchiveReader::from_read_at(
            archive.clone(),
            Password::empty(),
            ArchiveLimits::default(),
        )
        .expect("header");
        reader.set_threads(4);
        let (result, heard) = unordered_with(&mut reader, Some(stop_after));
        result.expect("a stop is not an error");
        let events = heard.begun.len()
            + heard.ended.len()
            + heard.bytes.values().filter(|b| !b.is_empty()).count();
        assert!(events <= stop_after, "{stop_after}: nothing after the stop");
    }
}

#[test]
fn a_callback_error_is_returned_as_it_is() {
    let methods = vec![Lzma2Options::from_level(3).into()];
    let archive = write(&methods, &tree(20, 1, 9 << 20));
    let mut reader =
        ArchiveReader::from_read_at(archive, Password::empty(), ArchiveLimits::default())
            .expect("header");
    reader.set_threads(4);
    let mut calls = 0;
    let result = reader.for_each_entries_unordered(|_| {
        calls += 1;
        if calls == 3 {
            Err(Error::Other("granite callback refused".into()))
        } else {
            Ok(true)
        }
    });
    let error = result.expect_err("the callback's error");
    assert!(
        error.to_string().contains("granite callback refused"),
        "{error}"
    );
}

#[test]
fn hooks_hear_every_folder_and_member() {
    let methods = vec![Lzma2Options::from_level(3).into()];
    let archive = write(&methods, &tree(30, 2, 9 << 20));
    let collect = |threads: u32, unordered_walk: bool| {
        let mut reader = ArchiveReader::from_read_at(
            archive.clone(),
            Password::empty(),
            ArchiveLimits::default(),
        )
        .expect("header");
        reader.set_threads(threads);
        let blocks = Arc::new(Mutex::new(BTreeSet::new()));
        let subs = Arc::new(Mutex::new(BTreeSet::new()));
        let (b, s) = (Arc::clone(&blocks), Arc::clone(&subs));
        reader.set_block_complete_hook(move |done| {
            b.lock()
                .unwrap()
                .insert((done.block_index, done.crc_verified));
        });
        reader.set_sub_stream_complete_hook(move |done| {
            s.lock().unwrap().insert((done.file_index, done.crc32));
        });
        if unordered_walk {
            reader
                .for_each_entries_unordered(|_| Ok(true))
                .expect("unordered");
        } else {
            reader
                .for_each_entries(|_, rd| {
                    std::io::copy(rd, &mut std::io::sink())?;
                    Ok(true)
                })
                .expect("ordered");
        }
        let blocks = std::mem::take(&mut *blocks.lock().unwrap());
        let subs = std::mem::take(&mut *subs.lock().unwrap());
        (blocks, subs)
    };
    let reference = collect(1, false);
    assert!(!reference.0.is_empty() && !reference.1.is_empty());
    for threads in [1, 4, 8] {
        assert_eq!(collect(threads, true), reference, "{threads} threads");
    }
}
