//! The multi-threaded LZMA2 path: same bytes, more threads.
//!
//! `lzma-turbo` decodes an LZMA2 stream in parallel by cutting it at the
//! dictionary resets that make a *run* independently decodable, and this fork
//! drives that decoder with a thread count the caller can change while the
//! decode is running. What has to be true of all of it is that none of it is
//! observable in the output: the same archive must produce the same bytes at
//! one thread, at eight, and when the count is moved from one to the other
//! part way through.
//!
//! The archives here are written by `7zz`, because whether a stream *can* be
//! decoded in parallel is a property of how the encoder chunked it and this
//! crate's own encoder does not chunk. The file skips itself when `7zz` is not
//! on `PATH`.

use std::path::{Path, PathBuf};
use std::process::Command;

use sevenz_turbo::{ArchiveLimits, ArchiveReader, Password};

fn have_7zz() -> bool {
    Command::new("7zz")
        .arg("i")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Compressible bytes with enough structure that `7zz` chunks them.
fn payload(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut words: Vec<Vec<u8>> = Vec::new();
    for _ in 0..512 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let n = 3 + (state % 9) as usize;
        words.push(vec![b'a' + (state >> 32) as u8 % 26; n]);
    }
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&words[(state >> 16) as usize % words.len()]);
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// A one-entry archive whose LZMA2 stream has several runs: a small dictionary
/// makes `7zz` cut its multi-threaded blocks small, so a few megabytes are
/// enough to get a backlog rather than the 128 MiB runs a default encode uses.
fn multi_run_archive(dir: &Path) -> PathBuf {
    let member = dir.join("payload.txt");
    std::fs::write(&member, payload(24 * 1024 * 1024)).expect("write member");
    let archive = dir.join("mt.7z");
    let output = Command::new("7zz")
        .args([
            "a",
            "-bso0",
            "-bsp0",
            "-t7z",
            "-m0=lzma2",
            "-md=256k",
            "-mmt=8",
            "-mx3",
        ])
        .arg(&archive)
        .arg(&member)
        .output()
        .expect("run 7zz");
    assert!(output.status.success(), "7zz a failed");
    archive
}

/// What `7zz x -so` makes of the archive: the oracle every lane is compared to.
fn oracle(archive: &Path) -> Vec<u8> {
    let output = Command::new("7zz")
        .args(["x", "-so", "-bso0", "-bsp0"])
        .arg(archive)
        .output()
        .expect("run 7zz");
    assert!(output.status.success(), "7zz x -so failed");
    output.stdout
}

/// Extracts every entry into one buffer.
fn extract(reader: &mut ArchiveReader<std::fs::File>) -> Vec<u8> {
    let mut out = Vec::new();
    reader
        .for_each_entries(|entry, rd| {
            if entry.is_directory() {
                return Ok(true);
            }
            rd.read_to_end(&mut out)?;
            Ok(true)
        })
        .expect("extract");
    out
}

fn open(archive: &Path, threads: u32) -> ArchiveReader<std::fs::File> {
    let file = std::fs::File::open(archive).expect("open archive");
    ArchiveReader::new(file, Password::empty())
        .expect("read archive")
        .with_threads(threads)
}

/// The headline: 1, 2, 8 and every thread this machine has, all byte-identical
/// to `7zz`'s own extraction.
#[test]
fn every_thread_count_decodes_what_7zz_decodes() {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let archive = multi_run_archive(tmp.path());
    let expected = oracle(&archive);

    let all = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    for threads in [1, 2, 8, all] {
        let mut reader = open(&archive, threads);
        let got = extract(&mut reader);
        assert_eq!(got.len(), expected.len(), "length differs at {threads}");
        assert!(got == expected, "bytes differ at {threads} threads");
    }
}

/// The parallel path is actually engaged, rather than every lane quietly
/// falling back to the single-threaded decoder: the run index of the block
/// being decoded advances past one, and workers exist.
#[test]
fn the_parallel_path_is_engaged_and_reports_its_backlog() {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let archive = multi_run_archive(tmp.path());

    let mut reader = open(&archive, 8);
    let handle = reader.lzma2_handle();
    let mut runs = 0;
    let mut spawned = 0;
    reader
        .for_each_entries(|_entry, rd| {
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = rd.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if let Some(progress) = handle.progress() {
                    runs = runs.max(progress.runs_claimed);
                    spawned = spawned.max(progress.spawned_threads);
                }
            }
            Ok(true)
        })
        .expect("extract");

    assert!(runs > 1, "the archive decoded as one run: {runs}");
    assert!(spawned > 0, "no worker thread was ever created");
}

/// Moving the thread count while the archive is decoding changes nothing about
/// the bytes. The switch lands at the next run boundary, which is a dictionary
/// reset, so a run decoded inline and the same run decoded on a worker are the
/// same decode.
#[test]
fn switching_mode_mid_archive_produces_identical_bytes() {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let archive = multi_run_archive(tmp.path());
    let expected = oracle(&archive);

    // Start inline but ask for a coder that can widen, then flap the ceiling
    // every 64 KiB of output.
    let file = std::fs::File::open(&archive).expect("open archive");
    let mut reader = ArchiveReader::new(file, Password::empty())
        .expect("read archive")
        .with_adaptive_lzma2();
    let handle = reader.lzma2_handle();

    let mut got = Vec::new();
    let mut switches = 0u32;
    reader
        .for_each_entries(|entry, rd| {
            if entry.is_directory() {
                return Ok(true);
            }
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = rd.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                switches += 1;
                handle.set_threads(if switches.is_multiple_of(2) { 1 } else { 8 });
            }
            Ok(true)
        })
        .expect("extract");

    assert!(switches > 4, "not enough reads to switch mode on");
    assert_eq!(got.len(), expected.len(), "length differs");
    assert!(got == expected, "bytes differ after switching mode");
}

/// A memory limit too small for a parallel decode is not an error: the coder
/// decodes single-threaded, which needs nothing beyond its dictionary. A limit
/// says what the caller can afford, not that the archive must be refused.
#[test]
fn a_memory_limit_too_small_for_threads_degrades_to_single_threaded() {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let archive = multi_run_archive(tmp.path());
    let expected = oracle(&archive);

    // Room for the 256 KiB dictionary and the decoder's own state, and nothing
    // like enough to hold runs in flight.
    let file = std::fs::File::open(&archive).expect("open archive");
    let mut reader = ArchiveReader::with_limits(
        file,
        Password::empty(),
        ArchiveLimits::memory(4 * 1024 * 1024),
    )
    .expect("read archive");
    reader.set_threads(8);
    let handle = reader.lzma2_handle();

    let mut got = Vec::new();
    let mut ever_engaged = false;
    reader
        .for_each_entries(|entry, rd| {
            if entry.is_directory() {
                return Ok(true);
            }
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = rd.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                ever_engaged |= handle.progress().is_some();
            }
            Ok(true)
        })
        .expect("extract");

    assert!(got == expected, "bytes differ under a small memory limit");
    assert!(
        !ever_engaged,
        "the parallel coder was engaged under a limit that cannot hold a run"
    );
}

/// The two fixtures the throughput numbers are taken on, at every thread count
/// in the bench table, against `7zz x -so`.
///
/// They are a gigabyte each and live outside the repository, so this runs only
/// when `SEVENZ_TURBO_FIXTURES` points at the directory holding them (in
/// practice `lzma-turbo/bench/fixtures`). Run it with `--release`: in a debug
/// build it decodes eight gigabytes through an unoptimised decoder.
#[test]
fn the_bench_fixtures_decode_identically_at_every_thread_count() {
    let Ok(dir) = std::env::var("SEVENZ_TURBO_FIXTURES") else {
        eprintln!("skipping: set SEVENZ_TURBO_FIXTURES to the fixture directory");
        return;
    };
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let all = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    for name in ["mt.7z", "st.7z"] {
        let archive = Path::new(&dir).join(name);
        assert!(archive.exists(), "{} is missing", archive.display());
        let expected = oracle(&archive);
        for threads in [1, 2, 8, all] {
            let mut reader = open(&archive, threads);
            let got = extract(&mut reader);
            assert_eq!(
                got.len(),
                expected.len(),
                "{name}: length differs at {threads} threads"
            );
            assert!(got == expected, "{name}: bytes differ at {threads} threads");
        }
    }
}

/// The completion hook is a container-level promise, and the parallel decoder
/// must not change when or how often it fires.
#[test]
fn the_completion_hook_still_fires_once_per_block() {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let archive = multi_run_archive(tmp.path());

    let file = std::fs::File::open(&archive).expect("open archive");
    let mut reader = ArchiveReader::new(file, Password::empty()).expect("read archive");
    reader.set_threads(8);
    let block_count = reader.archive().blocks.len();

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&seen);
    reader.set_block_complete_hook(move |completion| {
        sink.lock().expect("lock").push(completion);
    });
    let _ = extract(&mut reader);

    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), block_count, "one completion per block");
    for completion in seen.iter() {
        assert!(completion.crc_verified, "block CRC was not verified");
        assert!(completion.unpacked_size > 0);
    }
}

// ---------------------------------------------------------------------------
// Per-file checksums
// ---------------------------------------------------------------------------

/// A plain bit-at-a-time CRC-32, as the oracle for the folded values. Slow on
/// purpose: it shares no code with the decoder's.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// An archive of many small files, solid, so a single LZMA2 run holds many
/// sub-streams and every file's checksum has to be cut out of it. There are
/// enough of them that the block is over the smallest one the reader decodes
/// in parallel, so the workers are the ones cutting.
fn many_small_files_archive(dir: &Path) -> (PathBuf, Vec<Vec<u8>>) {
    let source = dir.join("many");
    std::fs::create_dir_all(&source).expect("mkdir");
    let mut members = Vec::new();
    for index in 0..256 {
        let bytes = payload(4096 + index * 37);
        std::fs::write(source.join(format!("member{index:03}.txt")), &bytes).expect("write");
        members.push(bytes);
    }
    let archive = dir.join("many.7z");
    let output = Command::new("7zz")
        .args(["a", "-bso0", "-bsp0", "-t7z", "-m0=lzma2", "-mx3", "-ms=on"])
        .arg(&archive)
        .arg(source.join("*"))
        .output()
        .expect("run 7zz");
    assert!(output.status.success(), "7zz a failed");
    (archive, members)
}

/// Every file's checksum, as the decode computed it, at every thread count —
/// against the checksums the header records and against a checksum taken over
/// the extracted bytes by an implementation that shares nothing with the
/// decoder's.
#[test]
fn per_file_checksums_match_the_header_at_every_thread_count() {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let spanning = multi_run_archive(tmp.path());
    let (many, _) = many_small_files_archive(tmp.path());
    let all = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);

    for archive in [&spanning, &many] {
        for threads in [1, 2, 8, all] {
            let mut reader = open(archive, threads);
            let header: Vec<Option<u32>> = (0..reader.archive().num_unpack_sub_streams())
                .map(|index| reader.archive().sub_stream(index).expect("sub-stream").crc)
                .collect();

            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = std::sync::Arc::clone(&seen);
            reader.set_sub_stream_complete_hook(move |completion| {
                sink.lock().expect("lock").push(completion);
            });

            let mut bytes: Vec<Vec<u8>> = Vec::new();
            reader
                .for_each_entries(|entry, rd| {
                    if entry.is_directory() {
                        return Ok(true);
                    }
                    let mut buf = Vec::new();
                    rd.read_to_end(&mut buf)?;
                    bytes.push(buf);
                    Ok(true)
                })
                .expect("extract");

            let seen = seen.lock().expect("lock");
            assert_eq!(
                seen.len(),
                header.iter().filter(|c| c.is_some()).count(),
                "one completion per checksummed file at {threads} threads"
            );
            for (position, completion) in seen.iter().enumerate() {
                assert_eq!(
                    Some(completion.crc32),
                    header[completion.sub_stream_index],
                    "file {position} disagrees with the header at {threads} threads"
                );
                assert_eq!(
                    completion.crc32,
                    crc32(&bytes[position]),
                    "file {position} disagrees with a checksum over its bytes"
                );
                assert_eq!(completion.len, bytes[position].len() as u64);
            }
        }
    }
}

/// Folding is what turns per-piece checksums into per-file ones, so the
/// helper has to be right on the boundaries the decoder will hand over.
#[test]
fn folding_checksums_equals_checksumming_the_whole() {
    let whole = payload(300 * 1024);
    for cut in [
        0usize,
        1,
        15,
        16,
        4096,
        100_000,
        whole.len() - 1,
        whole.len(),
    ] {
        let (head, tail) = whole.split_at(cut);
        assert_eq!(
            sevenz_turbo::crc32_combine(crc32(head), crc32(tail), tail.len() as u64),
            crc32(&whole),
            "folding at {cut} differs"
        );
    }

    // Three pieces, folded left to right, as a caller crossing block
    // boundaries would.
    let (a, rest) = whole.split_at(1000);
    let (b, c) = rest.split_at(5000);
    let ab = sevenz_turbo::crc32_combine(crc32(a), crc32(b), b.len() as u64);
    let abc = sevenz_turbo::crc32_combine(ab, crc32(c), c.len() as u64);
    assert_eq!(abc, crc32(&whole), "three-piece fold differs");
}

/// Verification still happens when the workers do the checksumming. The
/// multi-threaded path skips the verifying reader on the consuming thread —
/// that is the whole point of folding the workers' segments — so a corrupt
/// block must still be refused, or the skip would have quietly turned
/// verification off.
#[test]
fn a_corrupt_block_is_still_refused_when_the_workers_checksum() {
    if !have_7zz() {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let archive = multi_run_archive(tmp.path());
    let mut bytes = std::fs::read(&archive).expect("read archive");
    // A byte in the middle of the packed stream, well past the signature
    // header and well before the footer.
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    let corrupt = tmp.path().join("corrupt.7z");
    std::fs::write(&corrupt, &bytes).expect("write corrupt archive");

    let mut reader = open(&corrupt, 8);
    let engaged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&engaged);
    let handle = reader.lzma2_handle();
    let outcome = reader.for_each_entries(|_entry, rd| {
        let mut sink = Vec::new();
        let read = rd.read_to_end(&mut sink);
        if handle.progress().is_some() {
            seen.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        read?;
        Ok(true)
    });

    assert!(
        engaged.load(std::sync::atomic::Ordering::Relaxed),
        "the parallel path was never engaged, so this proves nothing"
    );
    assert!(
        outcome.is_err(),
        "a corrupt block decoded without complaint"
    );
}
