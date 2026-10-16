//! Stored folders read, checksummed and decrypted off the calling thread.
//!
//! A stored folder of 8 MiB or more, decoded with two threads or more from a
//! complete positional source, is read ahead on a thread that checksums as it
//! reads, and an AES one is decrypted on a second. None of that may show in
//! what comes out: the same bytes, hook calls and errors as one thread gives,
//! a read error after every byte before it, and a caller that stops — or
//! whose sink fails — stops the reading too.
#![cfg(feature = "compress")]

use std::io::{self, Cursor, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sevenz_turbo::*;

/// SplitMix64 bytes, different for every seed.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// One folder's files: sizes that put file boundaries inside the 1 MiB
/// spans and across them, an empty file, a one-byte file, and an odd tail,
/// `scale` MiB or so in all.
fn members(scale: usize) -> Vec<(String, Vec<u8>)> {
    let sizes = [
        (scale / 4) * (1 << 20) + 17,
        1,
        0,
        (1 << 20) - 3,
        (scale / 2) * (1 << 20) + 4093,
        777,
        (scale / 4) * (1 << 20) + 12_345,
    ];
    sizes
        .iter()
        .enumerate()
        .map(|(i, &len)| (format!("dir/member-{i}.bin"), payload(len, i as u64 + 1)))
        .collect()
}

/// One solid folder of `files`, coded with `methods`.
fn solid(methods: Vec<EncoderConfiguration>, files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods);
    let entries = files
        .iter()
        .map(|(name, data)| {
            let mut entry = ArchiveEntry::new_file(name);
            entry.size = data.len() as u64;
            entry
        })
        .collect();
    let readers = files
        .iter()
        .map(|(_, data)| SourceReader::new(Cursor::new(data.as_slice())))
        .collect();
    writer.push_archive_entries(entries, readers).expect("push");
    writer.finish().expect("finish").into_inner()
}

fn stored(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    solid(vec![EncoderMethod::COPY.into()], files)
}

#[cfg(feature = "aes256")]
fn stored_aes(files: &[(String, Vec<u8>)], password: &str) -> Vec<u8> {
    solid(
        vec![
            encoder_options::AesEncoderOptions::new(Password::from(password)).into(),
            EncoderMethod::COPY.into(),
        ],
        files,
    )
}

/// What the positional source saw.
#[derive(Default)]
struct Log {
    /// Bytes read on a stored folder's reader thread.
    off_thread: AtomicU64,
    /// Bytes read in all.
    served: AtomicU64,
    /// Reads at an offset in this range fail; reads before it stop at it.
    fail: Mutex<Option<std::ops::Range<u64>>>,
}

/// An archive in memory, read positionally, that records who read it.
#[derive(Clone)]
struct Watched {
    bytes: Arc<Vec<u8>>,
    log: Arc<Log>,
}

impl ReadAt for Watched {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let mut end = self.bytes.len() as u64;
        if let Some(fail) = self.log.fail.lock().unwrap().clone() {
            if fail.contains(&offset) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the disk went away",
                ));
            }
            if offset < fail.start {
                end = end.min(fail.start);
            }
        }
        if offset >= end {
            return Ok(0);
        }
        let n = buf.len().min((end - offset) as usize);
        buf[..n].copy_from_slice(&self.bytes[offset as usize..offset as usize + n]);
        if std::thread::current().name() == Some("sevenz-store") {
            self.log.off_thread.fetch_add(n as u64, Ordering::Relaxed);
        }
        self.log.served.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
}

/// A sink that fails once it has taken `room` bytes.
struct Sink {
    out: Arc<Mutex<Vec<u8>>>,
    room: usize,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut out = self.out.lock().unwrap();
        if out.len() >= self.room {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "the sink is full",
            ));
        }
        let n = buf.len().min(self.room - out.len());
        out.extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What a decode showed its caller.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    entries: Vec<(String, Vec<u8>)>,
    sub_streams: Vec<(usize, usize, u64, u64, u32)>,
    blocks: Vec<(usize, u64, bool)>,
}

/// How a decode is driven.
#[derive(Clone, Copy)]
struct Drive {
    threads: u32,
    /// The callback answers `false` after this many entries.
    stop_after: Option<usize>,
    /// The sink fails after this many bytes in all.
    sink_room: usize,
}

impl Drive {
    fn at(threads: u32) -> Self {
        Self {
            threads,
            stop_after: None,
            sink_room: usize::MAX,
        }
    }
}

/// Decodes `archive` through a watched positional source: what the caller
/// saw, every byte the sinks took in order, and the error if there was one.
fn decode(
    archive: &[u8],
    password: &str,
    drive: Drive,
    log: &Arc<Log>,
) -> (Seen, Vec<u8>, Option<Error>) {
    let source = Watched {
        bytes: Arc::new(archive.to_vec()),
        log: Arc::clone(log),
    };
    let password = if password.is_empty() {
        Password::empty()
    } else {
        Password::from(password)
    };
    let mut reader =
        ArchiveReader::from_read_at(source, password, ArchiveLimits::default()).expect("header");
    reader.set_input_complete(true);
    reader.set_threads(drive.threads);
    let seen = Arc::new(Mutex::new(Seen::default()));
    {
        let seen = Arc::clone(&seen);
        reader.set_sub_stream_complete_hook(move |c| {
            seen.lock().unwrap().sub_streams.push((
                c.block_index,
                c.file_index,
                c.unpacked_offset,
                c.len,
                c.crc32,
            ));
        });
    }
    {
        let seen = Arc::clone(&seen);
        reader.set_block_complete_hook(move |c| {
            seen.lock()
                .unwrap()
                .blocks
                .push((c.block_index, c.unpacked_size, c.crc_verified));
        });
    }
    let all = Arc::new(Mutex::new(Vec::new()));
    let mut count = 0;
    let result = reader.for_each_entries_direct(|entry, read| {
        let before = all.lock().unwrap().len();
        let mut sink = Sink {
            out: Arc::clone(&all),
            room: drive.sink_room,
        };
        let outcome = read.write_rest(&mut sink);
        let bytes = all.lock().unwrap()[before..].to_vec();
        seen.lock()
            .unwrap()
            .entries
            .push((entry.name().to_string(), bytes));
        outcome?;
        count += 1;
        Ok(drive.stop_after != Some(count))
    });
    drop(reader);
    let seen = std::mem::take(&mut *seen.lock().unwrap());
    let all = std::mem::take(&mut *all.lock().unwrap());
    (seen, all, result.err())
}

fn expected(files: &[(String, Vec<u8>)]) -> Vec<(String, Vec<u8>)> {
    files.to_vec()
}

/// One thread, then two, four and eight: the same bytes, hook calls and
/// order every time, and the reading done off the calling thread from two.
fn same_at_every_width(archive: &[u8], password: &str, files: &[(String, Vec<u8>)]) {
    let log = Arc::new(Log::default());
    let (reference, _, error) = decode(archive, password, Drive::at(1), &log);
    assert!(error.is_none(), "one thread: {error:?}");
    assert_eq!(log.off_thread.load(Ordering::Relaxed), 0);
    assert!(
        reference.entries == expected(files),
        "one thread decoded wrong bytes"
    );
    assert_eq!(reference.blocks.len(), 1);
    assert_eq!(
        reference.sub_streams.len(),
        files.iter().filter(|f| !f.1.is_empty()).count()
    );
    for threads in [2, 4, 8] {
        let log = Arc::new(Log::default());
        let (seen, _, error) = decode(archive, password, Drive::at(threads), &log);
        assert!(error.is_none(), "{threads} threads: {error:?}");
        assert!(
            seen == reference,
            "{threads} threads differ from one thread"
        );
        let folder: u64 = files.iter().map(|f| f.1.len() as u64).sum();
        assert!(
            log.off_thread.load(Ordering::Relaxed) >= folder,
            "{threads} threads: the folder was not read on its own thread"
        );
    }
}

#[test]
fn a_stored_folder_decodes_the_same_at_every_width() {
    let files = members(12);
    same_at_every_width(&stored(&files), "", &files);
}

#[cfg(feature = "aes256")]
#[test]
fn an_encrypted_stored_folder_decodes_the_same_at_every_width() {
    let files = members(12);
    same_at_every_width(
        &stored_aes(&files, "correct horse"),
        "correct horse",
        &files,
    );
}

/// Below 8 MiB the folder stays on the calling thread.
#[test]
fn a_small_stored_folder_is_not_read_ahead() {
    let files = vec![("small.bin".to_string(), payload((8 << 20) - 1, 9))];
    let log = Arc::new(Log::default());
    let (seen, _, error) = decode(&stored(&files), "", Drive::at(4), &log);
    assert!(error.is_none());
    assert!(seen.entries == files);
    assert_eq!(log.off_thread.load(Ordering::Relaxed), 0);
}

/// The archive's offset of byte `at` of its first folder's packed stream.
fn packed(archive: &[u8], password: &str, at: u64) -> usize {
    let password = if password.is_empty() {
        Password::empty()
    } else {
        Password::from(password)
    };
    let parsed = Archive::read(&mut Cursor::new(archive), &password).expect("header");
    (parsed.block_pack_streams(0)[0].offset + at) as usize
}

/// A flipped byte fails the folder with the error one thread gives.
fn corrupt_fails_the_same(archive: &[u8], password: &str) {
    let mut damaged = archive.to_vec();
    let at = packed(archive, password, 5 << 20);
    damaged[at] ^= 0x40;
    let log = Arc::new(Log::default());
    let (_, _, one) = decode(&damaged, password, Drive::at(1), &log);
    let one = one.expect("one thread accepted a damaged folder");
    for threads in [2, 4] {
        let log = Arc::new(Log::default());
        let (_, _, error) = decode(&damaged, password, Drive::at(threads), &log);
        let error = error.unwrap_or_else(|| panic!("{threads} threads accepted it"));
        assert_eq!(
            format!("{error:?}"),
            format!("{one:?}"),
            "{threads} threads"
        );
        assert!(log.off_thread.load(Ordering::Relaxed) > 0);
    }
    match one {
        Error::BlockDecode {
            block_index: 0,
            kind,
            ..
        } => assert!(
            if password.is_empty() {
                kind == BlockErrorKind::ChecksumMismatch
            } else {
                kind == BlockErrorKind::Password
            },
            "{kind:?}"
        ),
        other => panic!("not located: {other:?}"),
    }
}

#[test]
fn a_damaged_stored_folder_fails_its_checksum() {
    corrupt_fails_the_same(&stored(&members(12)), "");
}

#[cfg(feature = "aes256")]
#[test]
fn a_damaged_encrypted_stored_folder_fails_its_checksum() {
    corrupt_fails_the_same(&stored_aes(&members(12), "pw-9"), "pw-9");
}

/// A source that fails partway: the error, after every byte before it.
fn read_error_after_every_byte_before_it(archive: &[u8], password: &str, plain: &[u8]) {
    let fail_at = packed(archive, password, (6 << 20) + 4321) as u64;
    let lost_at = packed(archive, password, 0) as u64;
    let mut reference = None;
    for threads in [1, 4] {
        let log = Arc::new(Log::default());
        let (_, all, error) = decode_failing(archive, password, threads, &log, fail_at);
        let error = error.unwrap_or_else(|| panic!("{threads} threads read through a failure"));
        let delivered = (fail_at - lost_at) as usize;
        if password.is_empty() {
            assert_eq!(all.len(), delivered, "{threads} threads");
            assert!(all[..] == plain[..delivered], "{threads} threads");
        } else {
            // Whole cipher blocks only: what could be decrypted.
            assert!(all.len() <= delivered && delivered - all.len() < 16);
            assert!(all[..] == plain[..all.len()], "{threads} threads");
        }
        let shown = format!("{error:?}");
        assert!(shown.contains("the disk went away"), "{shown}");
        match &reference {
            None => reference = Some((shown, all.len())),
            Some(one) => assert_eq!(one, &(shown, all.len())),
        }
    }
}

/// [`decode`], with reads of the folder from `fail_at` on failing.
fn decode_failing(
    archive: &[u8],
    password: &str,
    threads: u32,
    log: &Arc<Log>,
    fail_at: u64,
) -> (Seen, Vec<u8>, Option<Error>) {
    // The header is at the end of the archive, past the failing range.
    let end = {
        let password = if password.is_empty() {
            Password::empty()
        } else {
            Password::from(password)
        };
        let parsed = Archive::read(&mut Cursor::new(archive), &password).expect("header");
        let range = parsed.block_pack_streams(0)[0];
        range.offset + range.size
    };
    *log.fail.lock().unwrap() = Some(fail_at..end);
    decode(archive, password, Drive::at(threads), log)
}

#[test]
fn a_read_error_mid_folder_follows_every_byte_before_it() {
    let files = members(12);
    let plain: Vec<u8> = files.iter().flat_map(|f| f.1.clone()).collect();
    read_error_after_every_byte_before_it(&stored(&files), "", &plain);
}

#[cfg(feature = "aes256")]
#[test]
fn a_read_error_mid_encrypted_folder_follows_every_byte_before_it() {
    let files = members(12);
    let plain: Vec<u8> = files.iter().flat_map(|f| f.1.clone()).collect();
    read_error_after_every_byte_before_it(&stored_aes(&files, "pw"), "pw", &plain);
}

/// Reading stops with the caller: what was read past the bytes the caller
/// took is no more than the spans the stages may be ahead by.
fn stops_with_the_caller(archive: &[u8], password: &str, drive: Drive, taken: u64, stages: u64) {
    let log = Arc::new(Log::default());
    let (_, _, _) = decode(archive, password, drive, &log);
    let served = log.served.load(Ordering::Relaxed);
    let off_thread = log.off_thread.load(Ordering::Relaxed);
    assert!(off_thread > 0, "the folder was not read ahead");
    // Four 1 MiB spans a stage, and a span's worth of slack.
    let bound = taken + stages * (4 << 20) + (1 << 20);
    assert!(
        off_thread <= bound,
        "read {off_thread} bytes off the calling thread for {taken} taken"
    );
    // And nothing more once the decode has returned.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(log.served.load(Ordering::Relaxed), served);
}

#[test]
fn stopping_early_stops_the_reader_thread() {
    let files = members(40);
    let drive = Drive {
        stop_after: Some(1),
        ..Drive::at(4)
    };
    stops_with_the_caller(&stored(&files), "", drive, files[0].1.len() as u64, 1);
}

#[test]
fn a_failing_sink_stops_the_reader_thread() {
    let files = members(40);
    let room = 3 << 20;
    let drive = Drive {
        sink_room: room,
        ..Drive::at(4)
    };
    let log = Arc::new(Log::default());
    let (_, all, error) = decode(&stored(&files), "", drive, &log);
    assert_eq!(all.len(), room);
    match error {
        Some(Error::Io(error, _)) => assert_eq!(error.kind(), io::ErrorKind::StorageFull),
        other => panic!("the sink's error was not passed on: {other:?}"),
    }
    stops_with_the_caller(&stored(&files), "", drive, room as u64, 1);
}

#[cfg(feature = "aes256")]
#[test]
fn a_failing_sink_stops_both_encrypted_stages() {
    let files = members(40);
    let room = 3 << 20;
    let drive = Drive {
        sink_room: room,
        ..Drive::at(4)
    };
    stops_with_the_caller(&stored_aes(&files, "pw"), "pw", drive, room as u64, 2);
}

/// Is the `7zz` oracle available? With `SEVENZ_TURBO_7ZZ_REQUIRED` set, a
/// missing one fails the test rather than skipping it.
fn have_7zz() -> bool {
    let found = std::process::Command::new("7zz")
        .arg("i")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        found || std::env::var_os("SEVENZ_TURBO_7ZZ_REQUIRED").is_none(),
        "SEVENZ_TURBO_7ZZ_REQUIRED is set but 7zz does not run"
    );
    found
}

/// The files [`sevenzip_archive`] stores.
fn sevenzip_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("big.bin".to_string(), payload((13 << 20) + 333, 41)),
        ("small.bin".to_string(), payload(4097, 42)),
    ]
}

/// What `7zz a -mx0` (with `-p` when given a password) makes of
/// [`sevenzip_files`], or `None` when there is no `7zz`.
fn sevenzip_archive(password: &str) -> Option<Vec<u8>> {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return None;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let files = sevenzip_files();
    for (name, data) in &files {
        std::fs::write(dir.path().join(name), data).expect("write member");
    }
    let mut args = vec!["a".to_string(), "-mx0".to_string(), "out.7z".to_string()];
    if !password.is_empty() {
        args.push(format!("-p{password}"));
    }
    args.extend(files.iter().map(|f| f.0.clone()));
    let status = std::process::Command::new("7zz")
        .args(&args)
        .current_dir(dir.path())
        .output()
        .expect("7zz runs");
    assert!(status.status.success(), "7zz a failed");
    Some(std::fs::read(dir.path().join("out.7z")).expect("archive"))
}

/// `7zz a -mx0` (and `-mx0 -p`) archives decode to the files 7-Zip was given,
/// read ahead off the calling thread.
fn sevenzip_stored(password: &str) {
    let Some(archive) = sevenzip_archive(password) else {
        return;
    };
    let files = sevenzip_files();
    for threads in [1, 4] {
        let log = Arc::new(Log::default());
        let (seen, _, error) = decode(&archive, password, Drive::at(threads), &log);
        assert!(error.is_none(), "{threads} threads: {error:?}");
        let mut got = seen.entries;
        got.sort();
        assert!(got == files, "{threads} threads decoded other bytes");
        assert_eq!(
            log.off_thread.load(Ordering::Relaxed) > 0,
            threads > 1,
            "{threads} threads"
        );
    }
}

#[test]
fn sevenzip_stored_archives_decode_the_same() {
    sevenzip_stored("");
}

#[cfg(feature = "aes256")]
#[test]
fn sevenzip_encrypted_stored_archives_decode_the_same() {
    sevenzip_stored("7zz-pass");
}

/// A wrong password is the error one thread gives: the plaintext fails its
/// checksum, and the folder says the password may be wrong. 7-Zip's archive,
/// because it leaves the header in the clear and the crate's writer does not.
#[cfg(feature = "aes256")]
#[test]
fn a_wrong_password_fails_the_same_at_every_width() {
    let Some(archive) = sevenzip_archive("right") else {
        return;
    };
    let log = Arc::new(Log::default());
    let (_, _, one) = decode(&archive, "wrong", Drive::at(1), &log);
    let one = one.expect("one thread accepted a wrong password");
    let log = Arc::new(Log::default());
    let (_, _, four) = decode(&archive, "wrong", Drive::at(4), &log);
    let four = four.expect("four threads accepted a wrong password");
    assert_eq!(format!("{four:?}"), format!("{one:?}"));
    assert!(log.off_thread.load(Ordering::Relaxed) > 0);
    assert!(
        matches!(
            one,
            Error::BlockDecode {
                kind: BlockErrorKind::Password,
                ..
            }
        ),
        "{one:?}"
    );
}
