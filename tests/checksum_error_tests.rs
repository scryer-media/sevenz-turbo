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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

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

/// Under a wrong password a store-mode block decrypts to garbage, and the
/// file's CRC is the first check to fail. That is the password's fault, not
/// damage a repair could fix: `read_file` says `Password`, as
/// `for_each_entries` does, still located in the block.
#[cfg(feature = "aes256")]
#[test]
fn a_wrong_password_mismatch_is_a_password_error_on_every_path() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;

    let files = [payload(100_000, 3), payload(50_000, 4)];
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_encrypt_header(false);
    writer.set_content_methods(vec![
        AesEncoderOptions::new(Password::from("right")).into(),
        EncoderMethod::COPY.into(),
    ]);
    for (i, f) in files.iter().enumerate() {
        writer
            .push_archive_entry(
                ArchiveEntry::new_file(&format!("f{i}")),
                Some(Cursor::new(f.as_slice())),
            )
            .expect("push");
    }
    let bytes = writer.finish().expect("finish").into_inner();

    #[track_caller]
    fn assert_password(result: Result<(), Error>, what: &str) {
        match result {
            Err(Error::BlockDecode {
                block_index: 0,
                kind: BlockErrorKind::Password,
                ..
            }) => {}
            other => panic!("{what}: expected a located password error, got {other:?}"),
        }
    }

    let mut reader =
        ArchiveReader::new(Cursor::new(bytes.clone()), Password::from("wrong")).expect("open");
    assert_password(
        reader.for_each_entries(|_, rd| read_all(rd)),
        "for_each_entries",
    );
    let mut reader =
        ArchiveReader::new(Cursor::new(bytes.clone()), Password::from("wrong")).expect("open");
    assert_password(reader.read_file("f0").map(drop), "read_file");

    // The right password reads the same archive back.
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::from("right")).expect("open");
    assert_eq!(reader.read_file("f0").expect("decodes"), files[0]);
}

/// Bytes that shrink by about a quarter and no further, so that a member's
/// packed stream is nearly as long as the member is.
fn dense(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            b' ' + ((x >> 24) as u8 & 0x3F)
        })
        .collect()
}

/// LZMA2 as one thread writes it: a single run from the first byte of the
/// stream to the last, which is also what `7zz -mmt=1` writes.
///
/// A parallel decoder has no second run to give a second worker, so it
/// decodes such a stream on the calling thread as it arrives, once it has
/// seen that the first run outlasts the longest one an encoder writes for
/// the dictionary and that there is still packed input to come. The members
/// below are sized for both: several times this dictionary, and a packed
/// stream longer than the decoder reads at once.
fn one_run_lzma2() -> Vec<EncoderConfiguration> {
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(1 << 16);
    vec![options.into()]
}

/// What one decode showed its caller.
struct Shown {
    result: Result<(), Error>,
    /// Each completion the sub-stream hook heard: the file and its CRC-32.
    completions: Vec<(usize, u32)>,
    /// How many entries the callback was handed.
    entries: usize,
    /// The most worker threads a block's parallel LZMA2 coder was seen with,
    /// or `None` when no coder reported: a single-threaded decode, or small
    /// folders decoding together.
    workers: Option<u32>,
}

/// Decodes `bytes` as `archive` describes them, so that a test can say what
/// the header claims without rewriting it.
///
/// `positional` gives the reader the source that lets small folders decode
/// on workers. `stop_at` is the entry whose callback answers `false` once it
/// has read the entry to its end.
fn show(
    bytes: &[u8],
    archive: Archive,
    threads: u32,
    positional: bool,
    stop_at: Option<usize>,
) -> Shown {
    let mut reader =
        ArchiveReader::from_archive(archive, Cursor::new(bytes.to_vec()), Password::empty());
    if positional {
        reader.set_positional_source(bytes.to_vec());
    }
    reader.set_threads(threads);
    let handle = reader.lzma2_handle();
    let completions = Arc::new(Mutex::new(Vec::new()));
    let heard = Arc::clone(&completions);
    reader.set_sub_stream_complete_hook(move |done| {
        heard.lock().unwrap().push((done.file_index, done.crc32));
    });
    let mut entries = 0;
    let mut workers = None;
    let result = reader.for_each_entries(|_, rd| {
        let at = entries;
        entries += 1;
        let mut sink = Vec::new();
        rd.read_to_end(&mut sink)?;
        if let Some(progress) = handle.progress() {
            workers = Some(workers.unwrap_or(0).max(progress.spawned_threads));
        }
        Ok(stop_at != Some(at))
    });
    reader.clear_sub_stream_complete_hook();
    let completions = std::mem::take(&mut *completions.lock().unwrap());
    Shown {
        result,
        completions,
        entries,
        workers,
    }
}

fn parse(bytes: &[u8]) -> Archive {
    Archive::read(&mut Cursor::new(bytes), &Password::empty()).expect("parse")
}

fn crcs(files: &[Vec<u8>]) -> Vec<(usize, u32)> {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| (index, lzma_turbo::crc::crc32(file)))
        .collect()
}

/// A folder of one file, written as one run.
static ONE_FILE: LazyLock<(Vec<u8>, Vec<Vec<u8>>)> = LazyLock::new(|| {
    let files = vec![dense(7 << 20, 11)];
    (archive(one_run_lzma2(), false, &files), files)
});

/// Two files sharing one folder, and one run.
static TWO_FILES: LazyLock<(Vec<u8>, Vec<Vec<u8>>)> = LazyLock::new(|| {
    let files = vec![dense(4 << 20, 12), dense(3 << 20, 13)];
    (archive(one_run_lzma2(), true, &files), files)
});

/// A file inside a run the parallel LZMA2 coder decodes as it arrives is
/// handed over before the coder has reported its checksum, because the run
/// is one open piece until the coder reaches the end of the stream. The file
/// is compared with its CRC all the same, at every thread count. With two or
/// more threads it used to be passed as checked without being compared.
#[test]
fn a_file_decoded_as_one_run_is_compared_with_its_crc() {
    let (bytes, files) = &*ONE_FILE;
    let parsed = parse(bytes);
    assert_eq!(parsed.blocks.len(), 1);
    for threads in [1, 2, 8] {
        let shown = show(bytes, parsed.clone(), threads, false, None);
        shown
            .result
            .unwrap_or_else(|e| panic!("at {threads} threads: {e}"));
        assert_eq!(shown.completions, crcs(files), "at {threads} threads");
        if threads > 1 {
            assert_eq!(
                shown.workers,
                Some(0),
                "at {threads} threads a run went to a worker, so this is not \
                 the decode this test is here for"
            );
        }

        let mut wrong = parsed.clone();
        wrong.files[0].crc ^= 1;
        let shown = show(bytes, wrong, threads, false, None);
        assert_checksum_mismatch(shown.result, 0);
        assert!(shown.completions.is_empty(), "at {threads} threads");
    }
}

/// The same for a block whose only checksum is its own.
#[test]
fn a_block_decoded_as_one_run_is_compared_with_its_crc() {
    let (bytes, files) = &*ONE_FILE;
    let mut parsed = parse(bytes);
    let crc = u64::from(lzma_turbo::crc::crc32(&files[0]));
    parsed.files[0].has_crc = false;
    parsed.blocks[0].has_crc = true;
    parsed.blocks[0].crc = crc;
    for threads in [1, 2, 8] {
        show(bytes, parsed.clone(), threads, false, None)
            .result
            .unwrap_or_else(|e| panic!("at {threads} threads: {e}"));

        let mut wrong = parsed.clone();
        wrong.blocks[0].crc ^= 1;
        assert_checksum_mismatch(show(bytes, wrong, threads, false, None).result, 0);
    }
}

/// Files sharing one such run are each compared, and each completion is
/// heard, in file order: the first file's checksum is no more to hand when
/// its last byte goes out than the last file's is.
#[test]
fn files_sharing_one_run_are_each_compared_with_their_crcs() {
    let (bytes, files) = &*TWO_FILES;
    let parsed = parse(bytes);
    assert_eq!(parsed.blocks.len(), 1);
    for threads in [1, 2, 8] {
        let shown = show(bytes, parsed.clone(), threads, false, None);
        shown
            .result
            .unwrap_or_else(|e| panic!("at {threads} threads: {e}"));
        assert_eq!(shown.completions, crcs(files), "at {threads} threads");
        if threads > 1 {
            assert_eq!(shown.workers, Some(0), "at {threads} threads");
        }

        for damaged in 0..files.len() {
            let mut wrong = parsed.clone();
            wrong.files[damaged].crc ^= 1;
            assert_checksum_mismatch(show(bytes, wrong, threads, false, None).result, 0);
        }
    }
}

/// A callback that reads a file to its end and then stops the block has
/// still been handed every byte of it, so the file is compared before the
/// stop is honoured: by `for_each_entries`, and by `read_file`, which stops
/// a solid block the same way once it has its file.
#[test]
fn a_callback_that_stops_inside_a_run_still_has_its_file_compared() {
    let (bytes, files) = &*TWO_FILES;
    let parsed = parse(bytes);
    let mut wrong = parsed.clone();
    wrong.files[0].crc ^= 1;
    for threads in [1, 2, 8] {
        let shown = show(bytes, parsed.clone(), threads, false, Some(0));
        shown
            .result
            .unwrap_or_else(|e| panic!("at {threads} threads: {e}"));
        assert_eq!(shown.entries, 1);
        assert_eq!(shown.completions, crcs(files)[..1], "at {threads} threads");

        let shown = show(bytes, wrong.clone(), threads, false, Some(0));
        assert_checksum_mismatch(shown.result, 0);
        assert_eq!(shown.entries, 1);

        let open = |archive: &Archive| {
            let mut reader = ArchiveReader::from_archive(
                archive.clone(),
                Cursor::new(bytes.clone()),
                Password::empty(),
            );
            reader.set_threads(threads);
            reader
        };
        assert_eq!(open(&parsed).read_file("f0").expect("decodes"), files[0]);
        assert_checksum_mismatch(open(&wrong).read_file("f0").map(drop), 0);
    }
}

/// Small folders decoded together each have a worker, and with threads to
/// spare each worker's own coder is the parallel one, so a folder that is
/// one run is decoded the same way there. Its file is compared before the
/// folder is reported, and before a callback that stops the folder is taken
/// at its word.
#[test]
fn small_folders_decoded_as_one_run_each_are_compared_on_their_workers() {
    let files = vec![dense(7 << 20, 14), dense(7 << 20, 15)];
    let bytes = archive(one_run_lzma2(), false, &files);
    let parsed = parse(&bytes);
    assert_eq!(parsed.blocks.len(), 2);
    // Eight threads over two folders: a worker each, and four threads for
    // each folder's coder.
    let shown = show(&bytes, parsed.clone(), 8, true, None);
    shown.result.expect("decodes");
    assert_eq!(shown.completions, crcs(&files));
    assert_eq!(
        shown.workers, None,
        "a coder reported, so the folders did not decode together"
    );

    for damaged in 0..files.len() {
        let mut wrong = parsed.clone();
        wrong.files[damaged].crc ^= 1;
        let shown = show(&bytes, wrong.clone(), 8, true, None);
        assert_checksum_mismatch(shown.result, damaged);

        let shown = show(&bytes, wrong, 8, true, Some(damaged));
        assert_checksum_mismatch(shown.result, damaged);
    }
}

/// A block whose only checksum is its own is owed that comparison once every
/// byte of it has been handed over, whether or not the callback handed the
/// last byte then stops: one thread compares it on the read that hands that
/// byte over, before the callback can answer. With two or more threads the
/// stop used to be honoured first, and the block passed without being
/// compared.
#[test]
fn a_callback_that_stops_at_a_blocks_last_byte_still_has_the_block_compared() {
    for (bytes, files) in [&*ONE_FILE, &*TWO_FILES] {
        let mut parsed = parse(bytes);
        assert_eq!(parsed.blocks.len(), 1);
        for file in &mut parsed.files {
            file.has_crc = false;
        }
        parsed.blocks[0].has_crc = true;
        parsed.blocks[0].crc = u64::from(lzma_turbo::crc::crc32(&files.concat()));
        let mut wrong = parsed.clone();
        wrong.blocks[0].crc ^= 1;
        let last = files.len() - 1;
        for threads in [1, 2, 8] {
            let shown = show(bytes, parsed.clone(), threads, false, Some(last));
            shown
                .result
                .unwrap_or_else(|e| panic!("at {threads} threads: {e}"));
            assert_eq!(shown.entries, files.len(), "at {threads} threads");

            let shown = show(bytes, wrong.clone(), threads, false, Some(last));
            assert_checksum_mismatch(shown.result, 0);
            assert_eq!(shown.entries, files.len(), "at {threads} threads");
        }
    }
}

/// The same for small folders decoded on workers, where a folder's own
/// verdict is the last thing its worker sends: a callback handed the folder's
/// last byte has that verdict waited for before its stop is taken at its
/// word.
#[test]
fn a_callback_that_stops_at_a_folders_last_byte_waits_for_the_folders_verdict() {
    let files = vec![dense(7 << 20, 16), dense(7 << 20, 17)];
    let bytes = archive(one_run_lzma2(), false, &files);
    let mut parsed = parse(&bytes);
    assert_eq!(parsed.blocks.len(), 2);
    for (index, file) in files.iter().enumerate() {
        parsed.files[index].has_crc = false;
        parsed.blocks[index].has_crc = true;
        parsed.blocks[index].crc = u64::from(lzma_turbo::crc::crc32(file));
    }
    for threads in [2, 8] {
        for stopped in 0..files.len() {
            let shown = show(&bytes, parsed.clone(), threads, true, Some(stopped));
            shown
                .result
                .unwrap_or_else(|e| panic!("at {threads} threads: {e}"));
            // A stop ends its own folder; the folders after it are still
            // handed over.
            assert_eq!(shown.entries, files.len(), "at {threads} threads");
            if threads == 8 {
                assert_eq!(
                    shown.workers, None,
                    "a coder reported, so the folders did not decode together"
                );
            }

            let mut wrong = parsed.clone();
            wrong.blocks[stopped].crc ^= 1;
            let shown = show(&bytes, wrong, threads, true, Some(stopped));
            assert_checksum_mismatch(shown.result, stopped);
        }
    }
}

/// The same again where the workers have the block's checksum to hand as
/// soon as its last byte is out: a stream of several runs, decoded on
/// workers. The stop was honoured first there too.
#[test]
fn a_folded_block_check_is_made_when_the_callback_stops_at_the_last_byte() {
    let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
    options.set_dictionary_size(1 << 20);
    let files = [payload(5 << 20, 8), payload(3 << 20, 9)];
    let bytes = archive(vec![options.into()], true, &files);
    let mut source = Cursor::new(bytes);
    let mut archive = Archive::read(&mut source, &Password::empty()).expect("parse");
    assert_eq!(archive.blocks.len(), 1);
    for file in &mut archive.files {
        file.has_crc = false;
    }
    archive.blocks[0].has_crc = true;
    archive.blocks[0].crc = u64::from(lzma_turbo::crc::crc32(&files.concat()));
    let mut wrong = archive.clone();
    wrong.blocks[0].crc ^= 1;

    let password = Password::empty();
    for threads in [1, 4] {
        let mut decode = |archive: &Archive| {
            let decoder = BlockDecoder::new(threads, 0, archive, &password, &mut source);
            let handle = decoder.lzma2_handle();
            let mut engaged = false;
            let result = decoder.for_each_entries(&mut |entry, rd| {
                read_all(rd)?;
                engaged |= handle.progress().is_some();
                Ok(entry.name() != "f1")
            });
            (result, engaged)
        };
        let (result, engaged) = decode(&archive);
        assert!(
            !result.unwrap_or_else(|e| panic!("at {threads} threads: {e}")),
            "at {threads} threads the callback stopped the block"
        );
        if threads > 1 {
            assert!(
                engaged,
                "the parallel path was never engaged, so this proves nothing"
            );
        }
        let (result, _) = decode(&wrong);
        assert_checksum_mismatch(result.map(drop), 0);
    }
}
