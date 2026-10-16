//! Valid-archive contract found by the upstream coverage audit.
//! C/7zArcIn.c: SzReadStreamsInfo defaults to one substream per folder
//! when the entire optional SubStreamsInfo section is absent.

use sevenz_turbo::{Archive, ArchiveReader, Block, Error, Password};
use std::io::Cursor;

fn number(out: &mut Vec<u8>, value: u64) {
    out.push(0xff);
    out.extend_from_slice(&value.to_le_bytes());
}

fn fixture(payloads: &[&[u8]], crc: bool, section: &[u8]) -> Vec<u8> {
    let mut h = vec![1, 4, 6];
    number(&mut h, 0);
    number(&mut h, payloads.len() as u64);
    h.push(9);
    for payload in payloads {
        number(&mut h, payload.len() as u64);
    }
    h.extend_from_slice(&[0, 7, 11]);
    number(&mut h, payloads.len() as u64);
    h.push(0);
    for _ in payloads {
        h.extend_from_slice(&[1, 1, 0]);
    }
    h.push(12);
    for payload in payloads {
        number(&mut h, payload.len() as u64);
    }
    if crc {
        h.extend_from_slice(&[10, 1]);
        for payload in payloads {
            h.extend_from_slice(&lzma_turbo::crc::crc32(payload).to_le_bytes());
        }
    }
    h.push(0);
    // Empty means the entire optional section is absent, not an empty section.
    h.extend_from_slice(section);
    h.extend_from_slice(&[0, 5]);
    number(&mut h, payloads.len() as u64);
    h.push(17);
    let name: Vec<_> = (0..payloads.len())
        .flat_map(|i| {
            format!("member-{i}\0")
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        })
        .collect();
    number(&mut h, name.len() as u64 + 1);
    h.push(0);
    h.extend_from_slice(&name);
    h.extend_from_slice(&[0, 0]);
    let size: u64 = payloads.iter().map(|p| p.len() as u64).sum();
    let mut start = Vec::new();
    start.extend_from_slice(&size.to_le_bytes());
    start.extend_from_slice(&(h.len() as u64).to_le_bytes());
    start.extend_from_slice(&lzma_turbo::crc::crc32(&h).to_le_bytes());
    let mut archive = vec![0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0, 4];
    archive.extend_from_slice(&lzma_turbo::crc::crc32(&start).to_le_bytes());
    archive.extend_from_slice(&start);
    for payload in payloads {
        archive.extend_from_slice(payload);
    }
    archive.extend_from_slice(&h);
    archive
}

#[test]
fn omitted_substreams_info_inherits_single_folder_size() {
    let payload = b"omitted substreams header\n";
    let archive = fixture(&[payload], false, &[]);
    let mut reader = ArchiveReader::new(Cursor::new(archive), Password::empty())
        .expect("SDK-valid archive with an omitted optional section");
    let mut count = 0;
    reader
        .for_each_entries(|entry, input| {
            assert_eq!(entry.size(), payload.len() as u64);
            let mut output = Vec::new();
            input.read_to_end(&mut output)?;
            assert_eq!(output, payload);
            count += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn omitted_substreams_info_preserves_sizes_crcs_and_folder_mapping() {
    let payloads: &[&[u8]] = &[b"first", b"second folder with different length", b"third"];
    for crc in [false, true] {
        let mut reader =
            ArchiveReader::new(Cursor::new(fixture(payloads, crc, &[])), Password::empty())
                .unwrap();
        assert_eq!(reader.archive().blocks.len(), payloads.len());
        for (i, entry) in reader.archive().files.iter().enumerate() {
            assert_eq!(entry.size(), payloads[i].len() as u64);
            assert_eq!(entry.has_crc, crc);
            if crc {
                assert_eq!(entry.crc, u64::from(lzma_turbo::crc::crc32(payloads[i])));
            }
            assert_eq!(reader.archive().stream_map.file_block_index[i], Some(i));
        }
        let mut count = 0;
        reader
            .for_each_entries(|_, input| {
                let mut output = Vec::new();
                input.read_to_end(&mut output)?;
                assert_eq!(output, payloads[count]);
                count += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(count, payloads.len());
    }
}

#[test]
fn explicit_invalid_substreams_section_never_uses_missing_section_defaults() {
    for (section, unknown) in [(&[8, 13, 0, 0][..], false), (&[8, 0xff][..], true)] {
        let error = match ArchiveReader::new(
            Cursor::new(fixture(&[b"payload"], true, section)),
            Password::empty(),
        ) {
            Ok(_) => panic!("invalid explicit section was accepted"),
            Err(error) => error,
        };
        if unknown {
            assert!(matches!(error, Error::BadTerminatedSubStreamsInfo));
        } else {
            assert!(matches!(error, Error::Corrupted(ref text)
                if text == "Archive declares more streamed files than sub-streams"));
        }
    }
}

#[test]
fn inferred_substreams_are_owned_snapshots_after_public_metadata_mutation() {
    let payloads: &[&[u8]] = &[b"first snapshot", b"second snapshot"];
    for crc in [false, true] {
        let mut archive = Archive::read(
            &mut Cursor::new(fixture(payloads, crc, &[])),
            &Password::empty(),
        )
        .unwrap();
        let original = archive.clone();
        for (i, payload) in payloads.iter().enumerate() {
            archive.files[i].size = u64::MAX;
            archive.files[i].crc = u64::MAX;
            archive.files[i].has_crc = !crc;
            archive.blocks[i].crc = u64::MAX;
            archive.blocks[i].has_crc = !crc;
            let snapshot = archive.sub_stream(i).unwrap();
            assert_eq!(snapshot.size, payload.len() as u64);
            assert_eq!(snapshot.crc, crc.then(|| lzma_turbo::crc::crc32(payload)));
            assert_eq!(
                archive.block_sub_streams(i)[0],
                original.block_sub_streams(i)[0]
            );
            // Replacing a public folder changes the folder itself, not the
            // independently parsed flat substream snapshot or its clone.
            archive.blocks[i] = Block::default();
            assert_eq!(archive.sub_stream(i), original.sub_stream(i));
        }
    }
}
