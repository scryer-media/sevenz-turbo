//! Generated 64-bit archive boundaries. No fixture bytes are retained.
//!
//! The default tests use real sparse files with payloads and headers above
//! 4 GiB. The ignored test actually reads and verifies more than 4 GiB.

// NTFS requires an explicit sparse-file control operation; do not accidentally
// turn this inexpensive fixture into a physical 4 GiB allocation on Windows.
#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use sevenz_turbo::{ArchiveEntry, ArchiveReader, Error, Password, PositionalBlockDecoder};
use std::sync::Arc;

const FOUR_GIB: u64 = 1u64 << 32;
const MARKER: &[u8] = b"64-bit archive boundary\n";

// The nine-byte form is legal for every 7z NUMBER. Keeping the independent
// fixture generator simple avoids sharing the production variable-int code.
fn number(out: &mut Vec<u8>, value: u64) {
    out.push(0xff);
    out.extend_from_slice(&value.to_le_bytes());
}

fn copy_header(pack_position: u64, size: u64) -> Vec<u8> {
    let mut h = vec![1, 4, 6]; // Header, MainStreamsInfo, PackInfo
    number(&mut h, pack_position);
    number(&mut h, 1);
    h.push(9); // Size
    number(&mut h, size);
    h.extend_from_slice(&[0, 7, 11]); // End, UnpackInfo, Folder
    number(&mut h, 1);
    h.extend_from_slice(&[0, 1, 1, 0, 12]); // internal, 1 coder, COPY, UnpackSize
    number(&mut h, size);
    h.extend_from_slice(&[0, 8, 0, 0, 5]); // End UnpackInfo, empty SubStreamsInfo, End, FilesInfo
    number(&mut h, 1);
    h.push(17); // Name
    let name: Vec<u8> = "boundary.bin\0"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    number(&mut h, 1 + name.len() as u64);
    h.push(0);
    h.extend_from_slice(&name);
    h.extend_from_slice(&[0, 0]);
    h
}

fn sparse_copy(pack_position: u64, size: u64) -> File {
    let mut file = tempfile::tempfile().expect("temporary sparse archive");
    let header = copy_header(pack_position, size);
    let next_offset = pack_position + size;
    let mut start = [0u8; 20];
    start[..8].copy_from_slice(&next_offset.to_le_bytes());
    start[8..16].copy_from_slice(&(header.len() as u64).to_le_bytes());
    start[16..].copy_from_slice(&lzma_turbo::crc::crc32(&header).to_le_bytes());
    file.write_all(&[0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c, 0, 4])
        .unwrap();
    file.write_all(&lzma_turbo::crc::crc32(&start).to_le_bytes())
        .unwrap();
    file.write_all(&start).unwrap();
    // Seeking leaves holes, not a 4 GiB allocation or a 4 GiB fixture upload.
    file.seek(SeekFrom::Start(32 + next_offset - MARKER.len() as u64))
        .unwrap();
    file.write_all(MARKER).unwrap();
    file.write_all(&header).unwrap();
    file.rewind().unwrap();
    file
}

#[test]
fn real_sparse_archive_seeks_to_payload_and_header_above_four_gib() {
    for pack_position in [FOUR_GIB - 1, FOUR_GIB, FOUR_GIB + 1] {
        for positional in [false, true] {
            let file = sparse_copy(pack_position, MARKER.len() as u64);
            assert!(file.metadata().unwrap().len() > FOUR_GIB);
            let source = file.try_clone().unwrap();
            let mut reader = ArchiveReader::new(file, Password::empty()).unwrap();
            let packed = reader.archive().block_pack_streams(0)[0];
            assert_eq!(packed.offset, 32 + pack_position);
            assert_eq!(packed.size, MARKER.len() as u64);
            let mut entries = 0;
            let mut verify = |entry: &ArchiveEntry, input: &mut dyn Read| -> Result<bool, Error> {
                assert_eq!(entry.size(), MARKER.len() as u64);
                let mut bytes = Vec::new();
                input.read_to_end(&mut bytes)?;
                assert_eq!(bytes, MARKER);
                entries += 1;
                Ok(true)
            };
            if positional {
                // A single folder at T1 does not select ArchiveReader's
                // positional planner. Invoke the owned decoder explicitly.
                PositionalBlockDecoder::new(
                    Arc::new(reader.archive().clone()),
                    Arc::new(Password::empty()),
                    Arc::new(source),
                    0,
                )
                .unwrap()
                .for_each_entries(&mut verify)
                .unwrap();
            } else {
                reader.for_each_entries(&mut verify).unwrap();
            }
            assert_eq!(entries, 1);
        }
    }
}

#[test]
fn truncated_next_header_above_four_gib_is_rejected() {
    let file = sparse_copy(FOUR_GIB + 17, MARKER.len() as u64);
    let length = file.metadata().unwrap().len();
    file.set_len(length - 1).unwrap();
    assert!(ArchiveReader::new(file, Password::empty()).is_err());
}

#[test]
#[ignore = "reads 4 GiB of sparse COPY data; run explicitly with --release --ignored"]
fn copy_member_size_and_actual_delivery_cross_four_gib() {
    let size = FOUR_GIB + MARKER.len() as u64;
    let file = sparse_copy(0, size);
    let mut reader = ArchiveReader::new(file, Password::empty()).unwrap();
    let mut total = 0u64;
    let mut entries = 0;
    reader
        .for_each_entries(|entry, input| {
            assert_eq!(entry.size(), size);
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                let zeros = FOUR_GIB.saturating_sub(total).min(n as u64) as usize;
                assert!(buf[..zeros].iter().all(|&b| b == 0));
                if zeros < n {
                    let marker_offset = total.saturating_sub(FOUR_GIB) as usize;
                    assert_eq!(
                        &buf[zeros..n],
                        &MARKER[marker_offset..marker_offset + n - zeros]
                    );
                }
                total += n as u64;
            }
            entries += 1;
            Ok(true)
        })
        .unwrap();
    assert_eq!(entries, 1);
    assert_eq!(total, size);
}

#[test]
#[ignore = "7zz generates >4 GiB compressed member; verifies every decoded byte at T1 and T4"]
fn lzma2_member_actual_delivery_crosses_four_gib() {
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.7z");
    let size = FOUR_GIB + 65537;
    let mut oracle = Command::new("7zz")
        .args([
            "a",
            "-t7z",
            "-si",
            "-m0=lzma2",
            "-mx1",
            "-md64k",
            "-mmt1",
            "-mhc=off",
            "-bso0",
            "-bsp0",
        ])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("7zz must be installed");
    let mut pattern = [0u8; 64 * 1024];
    for (i, byte) in pattern.iter_mut().enumerate() {
        *byte = i as u8;
    }
    {
        let mut input = oracle.stdin.take().unwrap();
        let mut remaining = size;
        while remaining > 0 {
            let n = remaining.min(pattern.len() as u64) as usize;
            input.write_all(&pattern[..n]).unwrap();
            remaining -= n as u64;
        }
    }
    assert!(oracle.wait().unwrap().success());
    assert!(std::fs::metadata(&path).unwrap().len() < 16 * 1024 * 1024);
    for threads in [1, 4] {
        let mut reader = ArchiveReader::new(File::open(&path).unwrap(), Password::empty()).unwrap();
        reader.set_threads(threads);
        let mut total = 0u64;
        let mut entries = 0;
        reader
            .for_each_entries(|entry, input| {
                assert_eq!(entry.size(), size);
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = input.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    for (i, &byte) in buf[..n].iter().enumerate() {
                        assert_eq!(
                            byte,
                            (total + i as u64) as u8,
                            "offset {}",
                            total + i as u64
                        );
                    }
                    total += n as u64;
                }
                entries += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(total, size);
        assert_eq!(entries, 1);
    }
}
