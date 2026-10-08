//! A CRC-32 that does not match is one error, whichever path caught it.
//!
//! The same mismatch can be caught by three different checks: the block's
//! verifying reader (a block holding one file, which is every block of a
//! non-solid store archive), a file's verifying reader on the consuming thread
//! (a solid block decoded on one thread), and the parallel LZMA2 workers'
//! folded checksums. A consumer that decides between "repair this" and "give
//! up" on the error's kind has to see the same answer from all three:
//! `Error::BlockDecode` with `BlockErrorKind::ChecksumMismatch`, located in the
//! block. And a caller that turned verification off gets none of them.
#![cfg(feature = "compress")]

use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// An archive of `files`, solid (one block) or one block per file.
fn archive(methods: Vec<EncoderConfiguration>, solid: bool, files: &[Vec<u8>]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods);
    if solid {
        let entries = (0..files.len())
            .map(|i| ArchiveEntry::new_file(&format!("f{i}")))
            .collect();
        let readers = files
            .iter()
            .map(|f| SourceReader::new(Cursor::new(f.as_slice())))
            .collect();
        writer.push_archive_entries(entries, readers).expect("push");
    } else {
        for (i, f) in files.iter().enumerate() {
            writer
                .push_archive_entry(
                    ArchiveEntry::new_file(&format!("f{i}")),
                    Some(Cursor::new(f.as_slice())),
                )
                .expect("push");
        }
    }
    writer.finish().expect("finish").into_inner()
}

fn read_all(rd: &mut dyn Read) -> Result<bool, Error> {
    let mut sink = Vec::new();
    rd.read_to_end(&mut sink)?;
    Ok(true)
}

#[track_caller]
fn assert_checksum_mismatch(result: Result<(), Error>, block: usize) {
    match result {
        Err(Error::BlockDecode {
            block_index,
            kind: BlockErrorKind::ChecksumMismatch,
            message,
            ..
        }) => {
            assert_eq!(block_index, block);
            assert_eq!(message, Error::ChecksumVerificationFailed.to_string());
        }
        other => panic!("expected a located checksum mismatch, got {other:?}"),
    }
}

/// A damaged byte in a store-mode archive is seen only as a CRC mismatch,
/// caught by the block's verifying reader. It used to surface as an I/O
/// error of kind `Other`.
#[test]
fn a_store_block_mismatch_is_a_located_checksum_mismatch() {
    let files = [payload(100_000, 1), payload(50_000, 2)];
    let mut bytes = archive(vec![EncoderMethod::COPY.into()], false, &files);
    // The first block's packed bytes start right after the signature
    // header, and are the first file as it stands.
    bytes[32 + 1000] ^= 0x55;

    for threads in [1, 4] {
        let mut reader =
            ArchiveReader::new(Cursor::new(bytes.clone()), Password::empty()).expect("open");
        reader.set_threads(threads);
        assert_checksum_mismatch(reader.for_each_entries(|_, rd| read_all(rd)), 0);
    }
    let mut reader =
        ArchiveReader::new(Cursor::new(bytes.clone()), Password::empty()).expect("open");
    match reader.read_file("f0") {
        Err(Error::BlockDecode {
            block_index: 0,
            kind: BlockErrorKind::ChecksumMismatch,
            ..
        }) => {}
        other => panic!("read_file: expected a checksum mismatch, got {other:?}"),
    }

    // Asked not to verify, the reader checks nothing: the damaged byte comes
    // out as it is.
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("open");
    reader.set_verify_checksums(false);
    let mut first = Vec::new();
    reader
        .for_each_entries(|entry, rd| {
            if entry.name() == "f0" {
                rd.read_to_end(&mut first)?;
                Ok(true)
            } else {
                read_all(rd)
            }
        })
        .expect("an unverified decode does not check");
    assert_eq!(first.len(), files[0].len());
    assert_eq!(first[1000], files[0][1000] ^ 0x55);
}

/// A solid block decoded on one thread checks each file on the consuming
/// thread, above the reader that records the chain's faults. Its mismatch
/// used to come out as a bare I/O error with no block context at all.
#[test]
fn a_solid_single_threaded_mismatch_is_a_located_checksum_mismatch() {
    let files = [payload(40_000, 3), payload(40_000, 4), payload(40_000, 5)];
    let bytes = archive(vec![Lzma2Options::from_level(1).into()], true, &files);
    let mut source = Cursor::new(bytes);
    let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
    assert_eq!(archive.blocks.len(), 1);
    archive.files[1].crc ^= 1;

    let password = Password::empty();
    let result = BlockDecoder::new(1, 0, &archive, &password, &mut source)
        .for_each_entries(&mut |_, rd| read_all(rd))
        .map(|_| ());
    assert_checksum_mismatch(result, 0);

    // Not verifying, the same block decodes.
    BlockDecoder::new(1, 0, &archive, &password, &mut source)
        .with_verify_checksums(false)
        .for_each_entries(&mut |_, rd| read_all(rd))
        .expect("an unverified decode does not check");
}

/// The parallel path folds the workers' checksums instead, and was already
/// typed; it must stay the same error as the other two.
#[test]
fn a_folded_mismatch_is_the_same_checksum_mismatch() {
    // Several LZMA2 blocks, each a run the workers can take, and large
    // enough that the block is decoded in parallel at all.
    let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
    options.set_dictionary_size(1 << 20);
    let files = [payload(5 << 20, 6), payload(3 << 20, 7)];
    let bytes = archive(vec![options.into()], true, &files);
    let mut source = Cursor::new(bytes);
    let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
    archive.files[1].crc ^= 1;

    let password = Password::empty();
    let decoder = BlockDecoder::new(4, 0, &archive, &password, &mut source);
    let handle = decoder.lzma2_handle();
    let engaged = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&engaged);
    let result = decoder
        .for_each_entries(&mut |_, rd| {
            let outcome = read_all(rd);
            if handle.progress().is_some() {
                seen.store(true, Ordering::Relaxed);
            }
            outcome
        })
        .map(|_| ());
    assert!(
        engaged.load(Ordering::Relaxed),
        "the parallel path was never engaged, so this proves nothing"
    );
    assert_checksum_mismatch(result, 0);
}
