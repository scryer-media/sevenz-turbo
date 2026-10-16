//! Parallel decode at two and four threads, small enough for the sanitizers.
//!
//! The thread count must never be visible in what comes out, and every
//! decode that ends early — a refused checksum, a callback that stops, a
//! callback that fails — has to tear its workers down. These cases are the
//! ones the ThreadSanitizer and AddressSanitizer lanes run over the parallel
//! LZMA2 coder, the folder-parallel replay of a positional source, and a BCJ2
//! chain whose side coders run beside the main one. They are kept to a few
//! megabytes and a 64 KiB dictionary so that an instrumented build finishes
//! them in minutes.
#![cfg(feature = "compress")]

use std::io::{Cursor, Read};

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

/// Members with empty files between the streamed ones, including two empties
/// in a row and one at each end.
fn members() -> Vec<(String, Vec<u8>)> {
    let sizes = [
        0usize,
        700 << 10,
        0,
        0,
        300 << 10,
        1,
        0,
        900 << 10,
        (200 << 10) + 13,
        0,
    ];
    sizes
        .iter()
        .enumerate()
        .map(|(i, &len)| (format!("ember_vale/m{i:02}.bin"), payload(len, i as u64)))
        .collect()
}

/// LZMA2 written by two threads in 128 KiB chunks over a 64 KiB dictionary,
/// so the stream has many runs for the parallel decoder to share out.
fn lzma2_mt() -> Lzma2Options {
    let mut options = Lzma2Options::from_level_mt(1, 2, 128 << 10);
    options.set_dictionary_size(64 << 10);
    options
}

fn entries(members: &[(String, Vec<u8>)]) -> Vec<ArchiveEntry> {
    members
        .iter()
        .map(|(name, data)| {
            let mut entry = ArchiveEntry::new_file(name);
            entry.size = data.len() as u64;
            entry
        })
        .collect()
}

fn solid_archive(methods: Vec<EncoderConfiguration>, members: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(methods);
    let readers = members
        .iter()
        .map(|(_, data)| SourceReader::new(data.as_slice()))
        .collect();
    writer
        .push_archive_entries(entries(members), readers)
        .unwrap();
    writer.finish().unwrap().into_inner()
}

fn non_solid_archive(members: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(vec![lzma2_mt().into()]);
    writer
        .push_archive_entries_non_solid(
            entries(members),
            |index, _| Ok(Some(Cursor::new(members[index].1.as_slice()))),
            2,
        )
        .unwrap();
    writer.finish().unwrap().into_inner()
}

fn number(out: &mut Vec<u8>, value: u64) {
    out.push(0xff);
    out.extend_from_slice(&value.to_le_bytes());
}

/// A 7z bit vector, most significant bit first.
fn bits(set: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; set.len().div_ceil(8)];
    for (i, _) in set.iter().enumerate().filter(|(_, bit)| **bit) {
        out[i / 8] |= 0x80 >> (i % 8);
    }
    out
}

/// One solid LZMA2 block of `members`' streamed bytes, encoded as
/// independent 256 KiB runs, with every empty member an empty-stream file in
/// its place between them, as 7-Zip writes one. This crate's writer keeps an
/// empty member of a solid push as a zero-size sub-stream instead, so the
/// header is built here.
fn interleaved_archive(members: &[(String, Vec<u8>)]) -> Vec<u8> {
    use lzma_turbo::{LzmaEncProps, crc::crc32, encode_lzma2};
    let streamed: Vec<&[u8]> = members
        .iter()
        .map(|(_, data)| data.as_slice())
        .filter(|data| !data.is_empty())
        .collect();
    let input = streamed.concat();
    let props = LzmaEncProps::new().with_level(1).with_dict_size(64 << 10);
    let mut packed = Vec::new();
    let mut property = None;
    for chunk in input.chunks(256 << 10) {
        let (prop, mut bytes) = encode_lzma2(chunk, &props).unwrap();
        assert_eq!(*property.get_or_insert(prop), prop);
        // One end marker after all the independent runs.
        assert_eq!(bytes.pop(), Some(0));
        packed.extend_from_slice(&bytes);
    }
    packed.push(0);
    // Header, MainStreamsInfo, PackInfo: one pack stream at offset zero.
    let mut h = vec![1, 4, 6];
    number(&mut h, 0);
    number(&mut h, 1);
    h.push(9);
    number(&mut h, packed.len() as u64);
    // UnpackInfo: one folder, one LZMA2 coder with its dictionary byte.
    h.extend_from_slice(&[0, 7, 11, 1, 0, 1, 0x21, 0x21, 1, property.unwrap()]);
    h.push(12);
    number(&mut h, input.len() as u64);
    // SubStreamsInfo: a sub-stream per streamed member, each with its CRC.
    h.extend_from_slice(&[0, 8, 13]);
    number(&mut h, streamed.len() as u64);
    h.push(9);
    for data in &streamed[..streamed.len() - 1] {
        number(&mut h, data.len() as u64);
    }
    h.extend_from_slice(&[10, 1]);
    for data in &streamed {
        h.extend_from_slice(&crc32(data).to_le_bytes());
    }
    // End of SubStreamsInfo and MainStreamsInfo; FilesInfo.
    h.extend_from_slice(&[0, 0, 5]);
    number(&mut h, members.len() as u64);
    let empty: Vec<bool> = members.iter().map(|(_, data)| data.is_empty()).collect();
    let empty_stream = bits(&empty);
    h.push(14);
    number(&mut h, empty_stream.len() as u64);
    h.extend_from_slice(&empty_stream);
    // Every empty-stream member is a file, not a directory.
    let empty_file = bits(&vec![true; empty.iter().filter(|e| **e).count()]);
    h.push(15);
    number(&mut h, empty_file.len() as u64);
    h.extend_from_slice(&empty_file);
    let names: Vec<u8> = members
        .iter()
        .flat_map(|(name, _)| name.encode_utf16().chain([0]).flat_map(u16::to_le_bytes))
        .collect();
    h.push(17);
    number(&mut h, names.len() as u64 + 1);
    h.push(0);
    h.extend_from_slice(&names);
    h.extend_from_slice(&[0, 0]);
    let mut start = Vec::new();
    start.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    start.extend_from_slice(&(h.len() as u64).to_le_bytes());
    start.extend_from_slice(&crc32(&h).to_le_bytes());
    let mut archive = vec![0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0, 4];
    archive.extend_from_slice(&crc32(&start).to_le_bytes());
    archive.extend_from_slice(&start);
    archive.extend_from_slice(&packed);
    archive.extend_from_slice(&h);
    archive
}

/// Every entry, in archive order, with its bytes.
fn extract<R: Read + std::io::Seek>(
    reader: &mut ArchiveReader<R>,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut out = Vec::new();
    reader.for_each_entries(|entry, input| {
        let mut data = Vec::new();
        input.read_to_end(&mut data)?;
        out.push((entry.name().to_owned(), data));
        Ok(true)
    })?;
    Ok(out)
}

/// The same archive through a seekable cursor, a [`ReadAt`] source and a file
/// on disk, at two and four threads, with and without the adaptive coder.
fn every_path(bytes: &[u8], password: &Password, expected: &[(String, Vec<u8>)]) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive.7z");
    std::fs::write(&path, bytes).unwrap();
    for threads in [2, 4] {
        for adaptive in [false, true] {
            let label = format!("{threads} threads, adaptive {adaptive}");
            let mut cursor = ArchiveReader::new(Cursor::new(bytes), password.clone())
                .unwrap()
                .with_threads(threads);
            cursor.set_adaptive_lzma2(adaptive);
            assert!(extract(&mut cursor).unwrap() == expected, "cursor, {label}");

            let mut positional =
                ArchiveReader::from_read_at(bytes.to_vec(), password.clone(), Default::default())
                    .unwrap()
                    .with_threads(threads);
            positional.set_adaptive_lzma2(adaptive);
            assert!(
                extract(&mut positional).unwrap() == expected,
                "read_at, {label}"
            );

            let mut file = ArchiveReader::open(&path, password.clone())
                .unwrap()
                .with_threads(threads);
            file.set_adaptive_lzma2(adaptive);
            assert!(extract(&mut file).unwrap() == expected, "file, {label}");
        }
    }
}

#[test]
fn a_solid_lzma2_block_with_zero_size_sub_streams_decodes_the_same_at_two_and_four_threads() {
    // The writer keeps an empty member of a solid push as a zero-size sub-
    // stream of the block, so the empties sit inside the block's file range.
    let members = members();
    let bytes = solid_archive(vec![lzma2_mt().into()], &members);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    assert_eq!(archive.blocks.len(), 1);
    let alone =
        extract(&mut ArchiveReader::new(Cursor::new(&bytes), Password::empty()).unwrap()).unwrap();
    assert!(alone == members);
    every_path(&bytes, &Password::empty(), &members);
}

#[test]
fn a_solid_multi_run_block_with_interleaved_empty_streams_decodes_the_same_at_two_and_four_threads()
{
    // Empty-stream files between, before and after the block's streamed
    // ones, as 7-Zip writes them: no sub-stream at all, only a bit in the
    // empty-stream vector.
    let members = members();
    let bytes = interleaved_archive(&members);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    assert_eq!(archive.blocks.len(), 1);
    let empties: Vec<_> = archive.files.iter().map(|file| !file.has_stream).collect();
    assert_eq!(
        empties,
        members
            .iter()
            .map(|(_, d)| d.is_empty())
            .collect::<Vec<_>>()
    );
    // An empty member before the block's first streamed one belongs to no
    // block and is handed out after the blocks; the order is the reader's,
    // and every thread count has to keep it.
    let alone =
        extract(&mut ArchiveReader::new(Cursor::new(&bytes), Password::empty()).unwrap()).unwrap();
    let mut sorted = alone.clone();
    sorted.sort();
    assert!(sorted == members);
    every_path(&bytes, &Password::empty(), &alone);
}

#[test]
fn non_solid_folders_decode_the_same_in_parallel_from_a_positional_source() {
    let members: Vec<_> = (0..24)
        .map(|i| {
            let len = match i % 5 {
                0 => 0,
                1 => 1,
                _ => (i * 37 + 11) << 10,
            };
            (
                format!("ember_vale/f{i:02}.bin"),
                payload(len, 100 + i as u64),
            )
        })
        .collect();
    let bytes = non_solid_archive(&members);
    every_path(&bytes, &Password::empty(), &members);
}

#[test]
fn a_corrupt_block_is_refused_at_every_thread_count_without_hanging() {
    let members = members();
    let mut bytes = solid_archive(vec![lzma2_mt().into()], &members);
    // Well inside the packed stream, past the signature header.
    let at = 32 + (bytes.len() - 32) / 2;
    bytes[at] ^= 0x55;
    for threads in [1, 2, 4] {
        let mut cursor = ArchiveReader::new(Cursor::new(&bytes), Password::empty())
            .unwrap()
            .with_threads(threads);
        assert!(extract(&mut cursor).is_err(), "cursor, {threads} threads");
        let mut positional =
            ArchiveReader::from_read_at(bytes.clone(), Password::empty(), Default::default())
                .unwrap()
                .with_threads(threads);
        assert!(
            extract(&mut positional).is_err(),
            "read_at, {threads} threads"
        );
    }
}

#[test]
fn a_decode_stopped_early_tears_its_workers_down() {
    let members = members();
    let solid = solid_archive(vec![lzma2_mt().into()], &members);
    let non_solid = non_solid_archive(&members);
    for bytes in [&solid, &non_solid, &interleaved_archive(&members)] {
        for threads in [2, 4] {
            // A callback that stops after the first streamed entry, having
            // read only part of it.
            let mut reader =
                ArchiveReader::from_read_at(bytes.clone(), Password::empty(), Default::default())
                    .unwrap()
                    .with_threads(threads);
            let mut seen = 0;
            reader
                .for_each_entries(|entry, input| {
                    seen += 1;
                    if entry.size < 4096 {
                        std::io::copy(input, &mut std::io::sink())?;
                        return Ok(true);
                    }
                    let mut some = [0u8; 4096];
                    input.read_exact(&mut some)?;
                    Ok(false)
                })
                .unwrap();
            // `Ok(false)` ends the block it was returned in: a solid archive
            // stops there, and a non-solid one goes on to the next folder.
            assert!(seen >= 2);

            // A callback that fails part way through a streamed entry.
            let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty())
                .unwrap()
                .with_threads(threads);
            let failed = reader.for_each_entries(|entry, input| {
                if entry.size > (500 << 10) {
                    let mut some = vec![0u8; 100 << 10];
                    input.read_exact(&mut some)?;
                    return Err(std::io::Error::other("stop here").into());
                }
                std::io::copy(input, &mut std::io::sink())?;
                Ok(true)
            });
            assert!(failed.is_err());
        }
    }
}

#[test]
fn a_written_bcj2_chain_over_parallel_lzma2_decodes_the_same_at_two_threads() {
    let members: Vec<_> = [(300 << 10) + 7, 0, 600 << 10, 1]
        .into_iter()
        .enumerate()
        .map(|(i, len)| (format!("ember_vale/t{i}.exe"), code_like(len, i as u64)))
        .collect();
    let bytes = solid_archive(
        vec![lzma2_mt().into(), EncoderMethod::BCJ2_FILTER.into()],
        &members,
    );
    let alone =
        extract(&mut ArchiveReader::new(Cursor::new(&bytes), Password::empty()).unwrap()).unwrap();
    assert!(alone == members);
    for threads in [2, 4] {
        let mut cursor = ArchiveReader::new(Cursor::new(&bytes), Password::empty())
            .unwrap()
            .with_threads(threads);
        assert!(
            extract(&mut cursor).unwrap() == members,
            "{threads} threads"
        );
        let mut positional =
            ArchiveReader::from_read_at(bytes.clone(), Password::empty(), Default::default())
                .unwrap()
                .with_threads(threads);
        assert!(
            extract(&mut positional).unwrap() == members,
            "read_at, {threads} threads"
        );
    }
}

#[test]
fn the_committed_bcj2_archives_decode_the_same_at_two_threads() {
    for name in ["7za433_7zip_lzma2_bcj2.7z", "delta_bcj2.7z"] {
        let bytes = std::fs::read(format!("tests/resources/{name}")).unwrap();
        let alone =
            extract(&mut ArchiveReader::new(Cursor::new(&bytes), Password::empty()).unwrap())
                .unwrap();
        assert!(!alone.is_empty(), "{name}");
        let mut cursor = ArchiveReader::new(Cursor::new(&bytes), Password::empty())
            .unwrap()
            .with_threads(2);
        assert!(extract(&mut cursor).unwrap() == alone, "{name}: cursor");
        let mut positional =
            ArchiveReader::from_read_at(bytes.clone(), Password::empty(), Default::default())
                .unwrap()
                .with_threads(2);
        assert!(
            extract(&mut positional).unwrap() == alone,
            "{name}: read_at"
        );
    }
}
