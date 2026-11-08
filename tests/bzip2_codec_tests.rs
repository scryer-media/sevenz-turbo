#![cfg(all(
    any(feature = "bzip2", feature = "bzip2-permissive"),
    feature = "compress"
))]

use std::io::Cursor;
use std::process::Command;

use sevenz_turbo::encoder_options::Bzip2Options;
use sevenz_turbo::{ArchiveEntry, ArchiveLimits, ArchiveReader, ArchiveWriter, Password};

fn payload() -> Vec<u8> {
    let mut state = 0x81c4_705du32;
    (0..350_017)
        .map(|position| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if position % 251 < 100 {
                (position % 19) as u8
            } else {
                state as u8
            }
        })
        .collect()
}

fn encode(input: &[u8], threads: u32) -> Vec<u8> {
    let mut options = Bzip2Options::from_level_mt(7, threads);
    options.set_block_size(100_000);
    options.set_passes(2);
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(vec![options.into()]);
    writer
        .push_archive_entry(ArchiveEntry::new_file("payload.bin"), Some(input))
        .unwrap();
    writer.finish().unwrap().into_inner()
}

#[test]
fn bzip2_archive_threads_preserve_bytes_and_memory_limited_reads() {
    let input = payload();
    let expected = encode(&input, 1);
    for threads in [1, 2, 4, 8] {
        let archive = encode(&input, threads);
        assert_eq!(archive, expected, "encoding threads={threads}");
        for limits in [ArchiveLimits::default(), ArchiveLimits::memory(16 << 20)] {
            let mut reader =
                ArchiveReader::with_limits(Cursor::new(&archive), Password::empty(), limits)
                    .unwrap()
                    .with_threads(threads);
            let mut actual = Vec::new();
            reader
                .for_each_entries(|entry, source| {
                    assert_eq!(entry.name(), "payload.bin");
                    source.read_to_end(&mut actual)?;
                    Ok(true)
                })
                .unwrap();
            assert_eq!(actual, input, "decoding threads={threads}");
        }
    }
}

#[test]
fn maximum_bzip2_blocks_preserve_existing_archive_memory_admission() {
    let input: Vec<_> = (0..900_017).map(|i| (i % 251) as u8).collect();
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(vec![Bzip2Options::from_level_mt(5, 2).into()]);
    writer
        .push_archive_entry(
            ArchiveEntry::new_file("bounded.bin"),
            Some(input.as_slice()),
        )
        .unwrap();
    let archive = writer.finish().unwrap().into_inner();
    // The existing archive model charges 8 MiB for BZip2 plus its 64 KiB
    // compressed-input buffer. Compact decoding must keep that admission.
    let budget = (8 << 20) + (64 << 10);
    let error = ArchiveReader::with_limits(
        Cursor::new(&archive),
        Password::empty(),
        ArchiveLimits::memory(8 << 20),
    )
    .err()
    .expect("the input buffer must remain part of the archive budget");
    assert!(matches!(
        error,
        sevenz_turbo::Error::MemoryLimited { limit_bytes, required_bytes }
            if limit_bytes == 8 << 20 && required_bytes == budget
    ));
    for threads in [1, 8] {
        let mut reader = ArchiveReader::with_limits(
            Cursor::new(&archive),
            Password::empty(),
            ArchiveLimits::memory(budget),
        )
        .unwrap()
        .with_threads(threads);
        let mut actual = Vec::new();
        reader
            .for_each_entries(|_, source| {
                source.read_to_end(&mut actual)?;
                Ok(true)
            })
            .unwrap();
        assert_eq!(actual, input);
    }
}

#[test]
#[ignore = "requires the pinned 7-Zip oracle; run by bzip2-conformance CI"]
fn seven_zip_accepts_the_internal_bzip2_archive_encoder() {
    let input = payload();
    let archive = encode(&input, 4);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bzip2.7z");
    std::fs::write(&path, archive).unwrap();
    let oracle = std::env::var_os("BZIP2_TURBO_7ZZ").unwrap_or_else(|| "7zz".into());
    let result = Command::new(&oracle)
        .args(["t", "-bso0", "-bsp0"])
        .arg(&path)
        .output()
        .expect("BZip2 integration requires 7zz or BZIP2_TURBO_7ZZ");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = Command::new(oracle)
        .args(["x", "-so", "-bsp0"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, input);
}

#[test]
fn damaged_bzip2_blocks_fail_in_both_archive_decode_modes() {
    let mut archive = encode(&payload(), 1);
    assert_eq!(&archive[32..35], b"BZh");
    archive[42] ^= 0x80; // First BZip2 block's checksum.
    for threads in [1, 2, 8] {
        let mut reader = ArchiveReader::new(Cursor::new(&archive), Password::empty())
            .unwrap()
            .with_threads(threads);
        let result = reader.for_each_entries(|_, source| {
            std::io::copy(source, &mut std::io::sink())?;
            Ok(true)
        });
        assert!(result.is_err(), "threads={threads}");
    }
}

#[test]
fn parallel_bzip2_rejects_a_wrong_container_file_checksum() {
    let bytes = encode(&payload(), 2);
    let password = Password::empty();
    let mut archive = sevenz_turbo::Archive::read(&mut Cursor::new(&bytes), &password).unwrap();
    archive.files[0].has_crc = true;
    archive.files[0].crc ^= 1;
    let mut input = Cursor::new(&bytes);
    let result = sevenz_turbo::BlockDecoder::new(2, 0, &archive, &password, &mut input)
        .for_each_entries(&mut |_, source: &mut dyn std::io::Read| {
            std::io::copy(source, &mut std::io::sink())?;
            Ok(true)
        });
    assert!(result.is_err());
}

#[cfg(feature = "aes256")]
#[test]
fn encrypted_bzip2_workers_verify_multiple_file_boundaries() {
    use sevenz_turbo::SourceReader;
    use sevenz_turbo::encoder_options::AesEncoderOptions;

    let input = payload();
    let parts = [&input[..10_003], &input[10_003..70_001], &input[70_001..]];
    let password = Password::from("codec-test-password");
    let mut options = Bzip2Options::from_level_mt(5, 4);
    options.set_block_size(100_000);
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(vec![
        AesEncoderOptions::new(password.clone()).into(),
        options.into(),
    ]);
    let entries = (0..parts.len())
        .map(|i| ArchiveEntry::new_file(&format!("part-{i}.bin")))
        .collect();
    let readers = parts
        .iter()
        .map(|bytes| SourceReader::new(Cursor::new(*bytes)))
        .collect();
    writer.push_archive_entries(entries, readers).unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    for threads in [1, 2, 3, 4, 8] {
        let mut reader = ArchiveReader::new(Cursor::new(&bytes), password.clone())
            .unwrap()
            .with_threads(threads);
        let mut index = 0;
        reader
            .for_each_entries(|_, source| {
                let mut actual = Vec::new();
                source.read_to_end(&mut actual)?;
                assert_eq!(actual, parts[index]);
                index += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(index, parts.len());
    }
    let mut archive = sevenz_turbo::Archive::read(&mut Cursor::new(&bytes), &password).unwrap();
    assert!(archive.files[1].has_crc);
    archive.files[1].crc ^= 1;
    let mut source = Cursor::new(&bytes);
    let result = sevenz_turbo::BlockDecoder::new(4, 0, &archive, &password, &mut source)
        .for_each_entries(&mut |_, source: &mut dyn std::io::Read| {
            std::io::copy(source, &mut std::io::sink())?;
            Ok(true)
        });
    assert!(result.is_err());
}

fn packed_bits(bytes: &[u8], position: usize, count: usize) -> u32 {
    (position..position + count).fold(0, |value, bit| {
        (value << 1) | u32::from((bytes[bit / 8] >> (7 - bit % 8)) & 1)
    })
}

#[test]
fn bzip2_trailers_are_validated_after_the_declared_file_bytes() {
    let data = payload();
    let bytes = encode(&data, 1);
    let password = Password::empty();
    // Parse the intact archive once and retain valid output sizes/file CRCs.
    // Direct block decoding avoids header parsing masking a trailer failure.
    let archive = sevenz_turbo::Archive::read(&mut Cursor::new(&bytes), &password).unwrap();
    let range = archive.block_pack_streams(0)[0];
    let start = range.offset as usize;
    let stop = range.end() as usize;
    let packed = &bytes[start..stop];
    let trailer = (packed.len() * 8 - 87..=packed.len() * 8 - 80)
        .find(|&position| {
            packed_bits(packed, position, 24) == 0x177245
                && packed_bits(packed, position + 24, 24) == 0x385090
        })
        .expect("the final BZip2 marker precedes its CRC and byte padding");
    let mut bad_crc = bytes[..stop].to_vec();
    let crc_bit = start * 8 + trailer + 48;
    bad_crc[crc_bit / 8] ^= 1 << (7 - crc_bit % 8);
    let mut bad_marker = bytes[..stop].to_vec();
    let marker_bit = start * 8 + trailer;
    bad_marker[marker_bit / 8] ^= 1 << (7 - marker_bit % 8);
    let cases = [
        ("combined CRC", bad_crc),
        ("trailer marker", bad_marker),
        ("missing trailer", bytes[..start + trailer / 8].to_vec()),
        ("truncated combined CRC", bytes[..stop - 2].to_vec()),
    ];
    for (case, damaged) in cases {
        for threads in [1, 2, 8] {
            let mut source = Cursor::new(&damaged);
            let result =
                sevenz_turbo::BlockDecoder::new(threads, 0, &archive, &password, &mut source)
                    .for_each_entries(&mut |_, reader: &mut dyn std::io::Read| {
                        std::io::copy(reader, &mut std::io::sink())?;
                        Ok(true)
                    });
            assert!(result.is_err(), "accepted {case} with {threads} threads");
        }
    }
}
