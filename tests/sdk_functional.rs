//! Archive contracts from 7-Zip 26.03, commit
//! 0766b733fe3e06dd2a7f9a3cfbf2108ac73abd17, C/7zArcIn.c and C/7zDec.c.
//! Fixtures are constructed from the format; the independent SDK decoder
//! validates them in CI. No generated archive bytes are committed.

use std::{io::Cursor, path::PathBuf, process::Command};

use sevenz_turbo::{ArchiveReader, Password};

#[derive(Clone, Copy)]
struct Member<'a> {
    name: &'a str,
    data: Option<&'a [u8]>,
    directory: bool,
}

fn number(out: &mut Vec<u8>, value: u64) {
    out.push(0xff);
    out.extend_from_slice(&value.to_le_bytes());
}

fn bits(values: impl IntoIterator<Item = bool>) -> Vec<u8> {
    let values: Vec<_> = values.into_iter().collect();
    let mut bytes = vec![0; values.len().div_ceil(8)];
    for (index, value) in values.into_iter().enumerate() {
        if value {
            bytes[index / 8] |= 0x80 >> (index % 8);
        }
    }
    bytes
}

fn property(header: &mut Vec<u8>, id: u8, data: &[u8]) {
    header.push(id);
    number(header, data.len() as u64);
    header.extend_from_slice(data);
}

fn raw(packed: &[u8], header: &[u8]) -> Vec<u8> {
    let mut start = Vec::new();
    start.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    start.extend_from_slice(&(header.len() as u64).to_le_bytes());
    start.extend_from_slice(&lzma_turbo::crc::crc32(header).to_le_bytes());
    let mut bytes = vec![0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0, 4];
    bytes.extend_from_slice(&lzma_turbo::crc::crc32(&start).to_le_bytes());
    bytes.extend_from_slice(&start);
    bytes.extend_from_slice(packed);
    bytes.extend_from_slice(header);
    bytes
}

fn fixture(members: &[Member<'_>], solid: bool, extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
    fixture_with_crcs(members, solid, extra, None)
}

fn fixture_with_crcs(
    members: &[Member<'_>],
    solid: bool,
    extra: &[(u8, Vec<u8>)],
    defined: Option<&[bool]>,
) -> Vec<u8> {
    let streams: Vec<_> = members.iter().filter_map(|m| m.data).collect();
    let packed: Vec<_> = streams.iter().flat_map(|s| s.iter().copied()).collect();
    let mut header = vec![1];
    if !streams.is_empty() {
        let folders: Vec<_> = if solid {
            vec![packed.as_slice()]
        } else {
            streams.clone()
        };
        header.extend_from_slice(&[4, 6]); // MainStreamsInfo, PackInfo
        number(&mut header, 0);
        number(&mut header, folders.len() as u64);
        header.push(9); // Size
        for data in &folders {
            number(&mut header, data.len() as u64);
        }
        header.extend_from_slice(&[0, 7, 11]); // End, UnpackInfo, Folder
        number(&mut header, folders.len() as u64);
        header.push(0); // inline folders
        for _ in &folders {
            header.extend_from_slice(&[1, 1, 0]); // one Copy coder
        }
        header.push(12); // CodersUnpackSize
        for data in &folders {
            number(&mut header, data.len() as u64);
        }
        let defined = defined
            .map(|mask| mask.to_vec())
            .unwrap_or_else(|| vec![true; folders.len()]);
        assert_eq!(defined.len(), folders.len());
        header.push(10); // CRC
        if defined.iter().all(|&bit| bit) {
            header.push(1);
        } else {
            header.push(0);
            header.extend_from_slice(&bits(defined.iter().copied()));
        }
        for (data, &defined) in folders.iter().zip(&defined) {
            if defined {
                header.extend_from_slice(&lzma_turbo::crc::crc32(data).to_le_bytes());
            }
        }
        header.push(0);
        if solid && streams.len() > 1 {
            header.extend_from_slice(&[8, 13]); // SubStreamsInfo, NumUnpackStream
            number(&mut header, streams.len() as u64);
            header.push(9);
            for data in &streams[..streams.len() - 1] {
                number(&mut header, data.len() as u64);
            }
            header.extend_from_slice(&[10, 1]);
            for data in &streams {
                header.extend_from_slice(&lzma_turbo::crc::crc32(data).to_le_bytes());
            }
            header.push(0);
        }
        header.push(0); // End StreamsInfo
    }
    header.push(5); // FilesInfo
    number(&mut header, members.len() as u64);
    if members.iter().any(|m| m.data.is_none()) {
        property(
            &mut header,
            14,
            &bits(members.iter().map(|m| m.data.is_none())),
        );
        property(
            &mut header,
            15,
            &bits(
                members
                    .iter()
                    .filter(|m| m.data.is_none())
                    .map(|m| !m.directory),
            ),
        );
    }
    let mut names = vec![0];
    for member in members {
        for unit in member.name.encode_utf16().chain([0]) {
            names.extend_from_slice(&unit.to_le_bytes());
        }
    }
    property(&mut header, 17, &names);
    for (id, value) in extra {
        property(&mut header, *id, value);
    }
    header.extend_from_slice(&[0, 0]);
    raw(&packed, &header)
}

fn sdk(bytes: &[u8], accepted: bool) {
    let Some(binary) = std::env::var_os("SEVENZ_TURBO_SDK_7ZDEC") else {
        assert!(
            std::env::var_os("SEVENZ_TURBO_SDK_REQUIRED").is_none(),
            "SDK oracle required; run bash .github/scripts/build-sdk-oracle.sh"
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("case.7z");
    std::fs::write(&archive, bytes).unwrap();
    let output = Command::new(binary)
        .arg("t")
        .arg(&archive)
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        accepted,
        "SDK verdict: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn accept(bytes: &[u8], members: &[Member<'_>]) {
    sdk(bytes, true);
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
    assert_eq!(reader.archive().files.len(), members.len());
    for (entry, member) in reader.archive().files.iter().zip(members) {
        assert_eq!(entry.name(), member.name);
        assert_eq!(entry.is_directory(), member.directory);
        assert_eq!(
            entry.size(),
            member.data.map_or(0, |data| data.len() as u64)
        );
    }
    for block in 0..reader.archive().blocks.len() {
        let expected: Vec<_> = reader
            .archive()
            .files
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                reader.archive().stream_map.file_block_index[*index] == Some(block)
            })
            .map(|(_, entry)| entry.name().to_owned())
            .collect();
        let decoder = reader.block_decoder(block).unwrap();
        assert_eq!(decoder.entry_count(), expected.len());
        assert_eq!(
            decoder
                .entries()
                .iter()
                .map(|entry| entry.name().to_owned())
                .collect::<Vec<_>>(),
            expected
        );
    }
    let mut actual = Vec::new();
    reader
        .for_each_entries(|entry, input| {
            let mut decoded = Vec::new();
            input.read_to_end(&mut decoded)?;
            actual.push((entry.name().to_owned(), decoded));
            Ok(true)
        })
        .unwrap();
    // The public walker emits folders first, then entries without streams.
    // Compare the complete multiset so duplicate names cannot hide a member.
    let mut expected: Vec<_> = members
        .iter()
        .map(|m| (m.name.to_owned(), m.data.unwrap_or_default().to_vec()))
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

fn reject(bytes: &[u8]) {
    sdk(bytes, false);
    let result =
        ArchiveReader::new(Cursor::new(bytes), Password::empty()).and_then(|mut reader| {
            reader.for_each_entries(|_, input| {
                std::io::copy(input, &mut std::io::sink())?;
                Ok(true)
            })
        });
    assert!(result.is_err(), "damaged archive was accepted");
}

const ONE: &[Member<'_>] = &[Member {
    name: "member",
    data: Some(b"SDK archive contract"),
    directory: false,
}];

#[test]
fn sdk_sparse_folder_crcs_inherit_only_defined_digests() {
    // ReadBitUi32s and ReadSubStreamsInfo preserve undefined CRCs; a folder
    // with one substream inherits its digest only when that digest is defined.
    let members: Vec<_> = (0..10).map(|_| ONE[0]).collect();
    let defined: Vec<_> = (0..10).map(|i| [0, 7, 9].contains(&i)).collect();
    let bytes = fixture_with_crcs(&members, false, &[], Some(&defined));
    accept(&bytes, &members);
    let reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
    for (entry, defined) in reader.archive().files.iter().zip(defined) {
        assert_eq!(entry.has_crc, defined);
        if defined {
            assert_eq!(
                entry.crc as u32,
                lzma_turbo::crc::crc32(ONE[0].data.unwrap())
            );
        }
    }
}

#[test]
fn sdk_zero_crc_is_a_defined_digest_for_an_empty_stream() {
    let members = [Member {
        name: "zero-crc",
        data: Some(&[]),
        directory: false,
    }];
    let bytes = fixture(&members, false, &[]);
    accept(&bytes, &members);
    let reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
    assert!(reader.archive().files[0].has_crc);
    assert_eq!(reader.archive().files[0].crc, 0);
}

#[test]
fn sdk_major_version_is_strict_and_minor_version_is_forward_compatible() {
    // C/7zArcIn.c: SzArEx_Open2 checks only the major version.
    let mut bytes = fixture(ONE, false, &[]);
    for minor in [0, 1, 4, 255] {
        bytes[7] = minor;
        accept(&bytes, ONE);
    }
    for major in [1, 255] {
        bytes[6] = major;
        reject(&bytes);
    }
}

#[test]
fn sdk_empty_archive_forms_have_no_members() {
    // SzArEx_Open2 accepts an absent next header; SzReadHeader2 accepts End.
    for bytes in [raw(&[], &[]), raw(&[], &[1, 0]), fixture(&[], false, &[])] {
        accept(&bytes, &[]);
    }
}

#[test]
fn sdk_empty_stream_bits_and_utf16_names_preserve_order() {
    // SzReadHeader2 uses a separate EmptyFile bitmap over empty streams only.
    let members = [
        Member {
            name: "folder",
            data: None,
            directory: true,
        },
        Member {
            name: "folder/empty",
            data: None,
            directory: false,
        },
        Member {
            name: "folder/α-🚀",
            data: Some(b"unicode"),
            directory: false,
        },
        Member {
            name: "empty-2",
            data: None,
            directory: false,
        },
        Member {
            name: "dir-2",
            data: None,
            directory: true,
        },
        Member {
            name: "five",
            data: Some(b"five"),
            directory: false,
        },
        Member {
            name: "six",
            data: None,
            directory: false,
        },
        Member {
            name: "seven",
            data: Some(b"seven"),
            directory: false,
        },
        Member {
            name: "eight",
            data: None,
            directory: false,
        },
        Member {
            name: "nine",
            data: Some(b"nine"),
            directory: false,
        },
    ];
    for solid in [false, true] {
        accept(&fixture(&members, solid, &[]), &members);
    }
}

#[test]
fn sdk_duplicate_names_keep_independent_substreams() {
    // SzArEx_Extract addresses files by index, not by name.
    let members = [
        Member {
            name: "same",
            data: Some(b"first"),
            directory: false,
        },
        Member {
            name: "same",
            data: Some(b"second, longer"),
            directory: false,
        },
    ];
    for solid in [false, true] {
        accept(&fixture(&members, solid, &[]), &members);
    }
}

#[test]
fn sdk_sparse_times_and_attributes_follow_defined_bits() {
    // ReadTime / ReadUi32s consume values only for set definition bits.
    let members: Vec<_> = (0..10).map(|_| ONE[0]).collect();
    let mut time = vec![0, 0x81, 0x40, 0]; // entries 0, 7, 9; inline
    let mut attributes = time.clone();
    for value in [
        132_000_000_000_000_000u64,
        132_000_000_000_000_001,
        132_000_000_000_000_002,
    ] {
        time.extend_from_slice(&value.to_le_bytes());
    }
    for value in [0x20u32, 0x01, 0x21] {
        attributes.extend_from_slice(&value.to_le_bytes());
    }
    let bytes = fixture(
        &members,
        true,
        &[(18, time.clone()), (20, time), (21, attributes)],
    );
    accept(&bytes, &members);
    let reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
    for (index, entry) in reader.archive().files.iter().enumerate() {
        let defined = [0, 7, 9].contains(&index);
        assert_eq!(entry.has_creation_date, defined);
        assert_eq!(entry.has_last_modified_date, defined);
        assert_eq!(entry.has_windows_attributes, defined);
        if let Some(slot) = [0, 7, 9].iter().position(|&n| n == index) {
            assert_eq!(
                u64::from(entry.creation_date),
                132_000_000_000_000_000 + slot as u64
            );
            assert_eq!(entry.windows_attributes, [0x20, 0x01, 0x21][slot]);
        }
    }
}

#[test]
fn sdk_unknown_file_and_archive_properties_are_skipped() {
    // SzReadHeader2 skips ArchiveProperties and unknown FilesInfo IDs by size.
    let bytes = fixture(ONE, false, &[(0x40, vec![0, 1, 0xff, 0])]);
    accept(&bytes, ONE);
    let header_at = 32 + u64::from_le_bytes(bytes[12..20].try_into().unwrap()) as usize;
    let mut header = vec![1, 2, 0x40];
    number(&mut header, 4);
    header.extend_from_slice(&[0, 1, 0xff, 0, 0]);
    header.extend_from_slice(&bytes[header_at + 1..]);
    accept(&raw(&bytes[32..header_at], &header), ONE);
}

#[test]
fn sdk_start_header_crc_covers_every_field() {
    let bytes = fixture(ONE, false, &[]);
    for index in 12..32 {
        let mut damaged = bytes.clone();
        damaged[index] ^= 0x80;
        reject(&damaged);
    }
}

#[test]
fn sdk_next_header_and_member_crcs_are_independent() {
    let bytes = fixture(ONE, false, &[]);
    for index in [32, bytes.len() - 1] {
        let mut damaged = bytes.clone();
        damaged[index] ^= 1;
        reject(&damaged);
    }
}

#[test]
fn sdk_every_truncated_archive_is_refused() {
    let bytes = fixture(ONE, false, &[]);
    for len in 0..bytes.len() {
        reject(&bytes[..len]);
    }
}

#[test]
fn sdk_reference_archives_extract_identical_bytes() {
    let Some(binary) = std::env::var_os("SEVENZ_TURBO_SDK_7ZDEC") else {
        assert!(std::env::var_os("SEVENZ_TURBO_SDK_REQUIRED").is_none());
        return;
    };
    let mut names = vec![
        "single_file_with_content_lzma.7z",
        "solid.7z",
        "non_solid.7z",
        "delta.7z",
        "7za433_7zip_lzma2_bcj2.7z",
    ];
    if cfg!(feature = "ppmd") {
        names.push("ppmd.7z");
    }
    for name in names {
        let archive = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/resources")
            .join(name);
        let tmp = tempfile::tempdir().unwrap();
        let output = Command::new(&binary)
            .arg("x")
            .arg(&archive)
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "SDK failed on {name}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let mut reader = ArchiveReader::new(
            Cursor::new(std::fs::read(archive).unwrap()),
            Password::empty(),
        )
        .unwrap();
        reader
            .for_each_entries(|entry, input| {
                if !entry.is_directory() {
                    let expected = std::fs::read(tmp.path().join(entry.name()))?;
                    let mut actual = Vec::new();
                    input.read_to_end(&mut actual)?;
                    assert_eq!(actual, expected, "{name}: {}", entry.name());
                }
                Ok(true)
            })
            .unwrap();
    }
}
