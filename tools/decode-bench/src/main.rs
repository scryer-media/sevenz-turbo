//! Decode-throughput harness for the codec swap.
//!
//! ```text
//! decode-bench [--runs N] [--no-oracle] <archive.7z> ...
//! ```
//!
//! For each archive it times, in one session on one machine:
//!
//! - `7zz t` (the C reference, every thread) and `7zz t -mmt=1`,
//! - upstream `sevenz-rust2` 0.22.2 — the release weaver ships today — driven
//!   at 16 threads, which is its `Lzma2ReaderMt` path,
//! - this fork at 1, 2, 8 and every thread.
//!
//! extracting every entry to a discard sink, and reports the median wall time
//! and the MiB/s of *uncompressed* output. The numbers this produced are in
//! `docs/benchmarking.md`. The gate is the `vs 7zz` column on the
//! multi-threaded fixture: the container must not cost more than 5% over the
//! reference implementation doing the same work.
//!
//! Not a criterion benchmark on purpose: the inputs are ~900 MiB fixtures that
//! live outside the repository, the run takes minutes, and it is a thing an
//! operator runs deliberately rather than something CI measures.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HELP: &str = "\
usage: decode-bench [--runs N] [--no-oracle] [--threads LIST] [--password P] <archive.7z> ...

  --runs N        repetitions per decoder (default 3); the median is reported
  --no-oracle     skip the `7zz t` lanes
  --only NAME     run only `7zz`, `upstream` or `fork`
  --threads LIST  fork thread counts, comma separated (default 1,2,8,all)
  --password P    password for an AES-256 archive, passed to every lane
  --memory-limit MIB  decode the fork lanes under a caller memory limit
  --cipher-only   time AES-256-CBC alone over 1 GiB in memory and exit
  --cipher-chunk KIB  chunk size for --cipher-only, repeatable (default 1024)
  --floor         time reading <archive.7z> and digesting it, and exit
  --io-profile    decode once through a counting reader and report read sizes";

fn main() {
    let mut runs = 3usize;
    let mut oracle = true;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut only = String::new();
    let mut threads: Vec<u32> = Vec::new();
    let mut password: Option<String> = None;
    let mut cipher_only = false;
    let mut floor = false;
    let mut cipher_chunks: Vec<usize> = Vec::new();
    let mut io_profile = false;
    let mut memory_limit = u64::MAX;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                runs = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--runs needs a number"));
            }
            "--no-oracle" => oracle = false,
            "--only" => {
                only = args.next().unwrap_or_else(|| fail("--only needs a name"));
            }
            "--threads" => {
                let list = args
                    .next()
                    .unwrap_or_else(|| fail("--threads needs a list"));
                threads = list.split(',').map(parse_threads).collect();
            }
            "--cipher-only" => cipher_only = true,
            "--floor" => floor = true,
            "--io-profile" => io_profile = true,
            "--cipher-chunk" => {
                let kib: usize = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--cipher-chunk needs a size in KiB"));
                cipher_chunks.push(kib * 1024);
            }
            "--memory-limit" => {
                let mib: u64 = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--memory-limit needs a size in MiB"));
                memory_limit = mib * (1 << 20);
            }
            "--password" => {
                password = Some(
                    args.next()
                        .unwrap_or_else(|| fail("--password needs a value")),
                );
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return;
            }
            other if other.starts_with('-') => fail(&format!("unknown option {other}")),
            other => files.push(PathBuf::from(other)),
        }
    }

    if cipher_only {
        if cipher_chunks.is_empty() {
            cipher_chunks.push(1 << 20);
        }
        for chunk in cipher_chunks {
            bench_cipher_only(runs, chunk);
        }
        return;
    }

    if files.is_empty() {
        println!("{HELP}");
        std::process::exit(2);
    }

    if io_profile {
        for file in &files {
            io_profile_one(file, password.as_deref());
        }
        return;
    }

    if floor {
        for file in &files {
            bench_floor(file, runs);
        }
        return;
    }

    if threads.is_empty() {
        threads = vec![1, 2, 8, all_threads()];
    }
    threads.dedup();

    println!(
        "decode-bench, {runs} run(s) per decoder, median reported, crypto backend {}",
        sevenz_turbo::crypto_backend()
    );

    for file in &files {
        bench_one(
            file,
            runs,
            oracle,
            &only,
            &threads,
            password.as_deref(),
            memory_limit,
        );
    }
}

/// A `Read + Seek` that records how big each read of the archive file was, so
/// a lane that looks slow can be shown to be asking the file for the payload
/// in small pieces rather than in the caller's buffer size.
struct CountingSource<R> {
    inner: R,
}

/// Reads bucketed by size, `BUCKETS[n]` counting reads of `2^n..2^(n+1)`, and
/// the totals. Statics because the reader is moved into the archive reader and
/// never handed back.
static BUCKETS: [std::sync::atomic::AtomicU64; 32] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 32];
static READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static READ_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl<R: Read> Read for CountingSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::atomic::Ordering::Relaxed;
        let read = self.inner.read(buf)?;
        READS.fetch_add(1, Relaxed);
        READ_BYTES.fetch_add(read as u64, Relaxed);
        let bucket = usize::BITS - 1 - read.max(1).leading_zeros();
        BUCKETS[(bucket as usize).min(31)].fetch_add(1, Relaxed);
        Ok(read)
    }
}

impl<R: std::io::Seek> std::io::Seek for CountingSource<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// One decode through `CountingSource`, reporting the read-size histogram.
fn io_profile_one(path: &Path, password: Option<&str>) {
    let file = std::fs::File::open(path).expect("open archive");
    let source = CountingSource { inner: file };
    let mut reader =
        sevenz_turbo::ArchiveReader::new(source, fork_password(password)).expect("read header");
    reader.set_threads(1);
    let mut sink = Sink::default();
    let mut buf = vec![0u8; 1 << 20];
    reader
        .for_each_entries(|entry, rd| {
            if !entry.is_directory() {
                drain(rd, &mut sink, &mut buf)?;
            }
            Ok(true)
        })
        .expect("extract");
    use std::sync::atomic::Ordering::Relaxed;
    let reads = READS.load(Relaxed);
    let bytes = READ_BYTES.load(Relaxed);
    println!(
        "io-profile {}: {} reads for {} MiB (mean {} bytes)",
        path.display(),
        reads,
        bytes / (1 << 20),
        bytes / reads.max(1),
    );
    for (bucket, count) in BUCKETS.iter().enumerate() {
        let count = count.load(Relaxed);
        if count > 0 {
            println!("  {:>10} bytes and up: {}", 1u64 << bucket, count);
        }
    }
}

/// What the machine costs to move the bytes at all: the packed stream read
/// from the file in 1 MiB chunks, once discarded and once fed to the bench's
/// own digest. No archive parsing, no cipher, no CRC. Every archive lane pays
/// both of these, and `7zz t` pays only the read, so the difference between
/// them is the part of a lane's time that is the harness rather than the
/// decoder.
fn bench_floor(path: &Path, runs: usize) {
    let mut read_only = Vec::new();
    let mut digested = Vec::new();
    let mut bytes = 0u64;
    for _ in 0..runs {
        for (times, digest) in [(&mut read_only, false), (&mut digested, true)] {
            let mut file = std::fs::File::open(path).expect("open archive");
            let mut buf = vec![0u8; 1 << 20];
            let mut sink = Sink::default();
            let start = Instant::now();
            loop {
                let read = file.read(&mut buf).expect("read archive");
                if read == 0 {
                    break;
                }
                if digest {
                    sink.update(&buf[..read]);
                }
            }
            sink.finish();
            times.push(start.elapsed());
            bytes = sink.bytes.max(bytes);
        }
    }
    read_only.sort_unstable();
    digested.sort_unstable();
    let read = read_only[read_only.len() / 2];
    let digest = digested[digested.len() / 2];
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "floor {}: read {:.3}s ({:.1} MiB/s), read+digest {:.3}s ({:.1} MiB/s), digest {:.3}s",
        path.display(),
        read.as_secs_f64(),
        mib / read.as_secs_f64().max(f64::MIN_POSITIVE),
        digest.as_secs_f64(),
        mib / digest.as_secs_f64().max(f64::MIN_POSITIVE),
        digest.as_secs_f64() - read.as_secs_f64(),
    );
}

/// AES-256-CBC on its own, always over AWS-LC whichever backend the crate was
/// built with: this lane calls `aws-lc-rs` directly, so it is the cipher's own
/// ceiling and not a backend comparison. 1 GiB already in memory, decrypted in 1 MiB chunks
/// with the IV carried between them, which is exactly what the reader does
/// minus the reading. It is the floor any archive lane can reach, and the
/// number the plumbing around the cipher is read against.
fn bench_cipher_only(runs: usize, chunk_len: usize) {
    use aws_lc_rs::cipher::{AES_256, DecryptingKey, DecryptionContext, UnboundCipherKey};
    use aws_lc_rs::iv::FixedLength;

    const TOTAL: usize = 1 << 30;
    let chunk_len = chunk_len.max(16) & !15;

    let key = [0x5au8; 32];
    let mut data = vec![0u8; TOTAL];
    // Something other than zeroes, so nothing can be optimised into a memset.
    let mut state = 0x243f_6a88_85a3_08d3u64;
    for byte in data.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state >> 24) as u8;
    }

    let mut times = Vec::new();
    for _ in 0..runs {
        let unbound = UnboundCipherKey::new(&AES_256, &key).expect("key");
        let cipher = DecryptingKey::cbc(unbound).expect("cbc");
        let mut iv = [0u8; 16];
        let start = Instant::now();
        for chunk in data.chunks_mut(chunk_len) {
            let mut next = [0u8; 16];
            next.copy_from_slice(&chunk[chunk.len() - 16..]);
            cipher
                .decrypt(chunk, DecryptionContext::Iv128(FixedLength::from(iv)))
                .expect("decrypt");
            iv = next;
        }
        times.push(start.elapsed());
    }

    let median = median(times);
    println!(
        "cipher-only (aws-lc, called directly; crate backend is {}): \
         {:.3}s for {} MiB in {} KiB chunks, {:.1} MiB/s",
        sevenz_turbo::crypto_backend(),
        median.as_secs_f64(),
        TOTAL / (1 << 20),
        chunk_len / 1024,
        (TOTAL / (1 << 20)) as f64 / median.as_secs_f64()
    );
}

/// Every thread this machine has, which is what "all" means in the tables.
fn all_threads() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

fn parse_threads(value: &str) -> u32 {
    match value.trim() {
        "all" => all_threads(),
        other => other
            .parse()
            .unwrap_or_else(|_| fail("--threads takes numbers or `all`")),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("decode-bench: {msg}");
    std::process::exit(2);
}

/// A sink that counts bytes and keeps the CRC-32 of everything written, so two
/// decoders can be shown to have produced the same output rather than assumed
/// to have.
struct Sink {
    bytes: u64,
    /// Four independent accumulators, folded together at the end. One would be
    /// a chain of dependent multiplies — about 5 cycles per 8 bytes, which is
    /// 0.2 s per GiB and a tenth of a fast parallel decode. Four run in the
    /// pipeline at once and cost a quarter of that.
    lanes: [u64; 4],
    /// Whole words folded so far. The lane a word goes to is a function of its
    /// position in the stream and nothing else — pick the lane by where the
    /// word falls in *this call* and the digest depends on how the decoder
    /// happened to chunk its reads, which is exactly what it must not do.
    words: u64,
    /// The four folded into one, once `finish` has been called.
    digest: u64,
    /// Bytes left over from the last update, so the digest depends on the byte
    /// stream and not on where each decoder happened to end its reads.
    tail: [u8; 8],
    tail_len: usize,
}

impl Default for Sink {
    fn default() -> Self {
        Self {
            bytes: 0,
            lanes: [
                0xcbf2_9ce4_8422_2325,
                0x9e37_79b9_7f4a_7c15,
                0xff51_afd7_ed55_8ccd,
                0xc4ce_b9fe_1a85_ec53,
            ],
            words: 0,
            digest: 0,
            tail: [0; 8],
            tail_len: 0,
        }
    }
}

const DIGEST_K: u64 = 0x100_0000_01b3;

impl Sink {
    fn update(&mut self, mut buf: &[u8]) {
        self.bytes += buf.len() as u64;

        if self.tail_len > 0 {
            let take = (8 - self.tail_len).min(buf.len());
            self.tail[self.tail_len..self.tail_len + take].copy_from_slice(&buf[..take]);
            self.tail_len += take;
            buf = &buf[take..];
            if self.tail_len < 8 {
                return;
            }
            let lane = (self.words % 4) as usize;
            self.mix(lane, u64::from_le_bytes(self.tail));
            self.words += 1;
            self.tail_len = 0;
        }

        // Walk up to a lane boundary one word at a time, then four at a time
        // so the four multiplies are in the pipeline together.
        let mut chunks = buf.chunks_exact(8);
        while !self.words.is_multiple_of(4) {
            let Some(chunk) = chunks.next() else { break };
            let lane = (self.words % 4) as usize;
            self.mix(lane, u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
            self.words += 1;
        }
        let aligned = chunks.remainder().len() + chunks.len() * 8;
        let from = buf.len() - aligned;
        let mut wide = buf[from..].chunks_exact(32);
        for chunk in &mut wide {
            for lane in 0..4 {
                let word =
                    u64::from_le_bytes(chunk[lane * 8..lane * 8 + 8].try_into().expect("8 bytes"));
                self.mix(lane, word);
            }
            self.words += 4;
        }
        let mut chunks = wide.remainder().chunks_exact(8);
        for chunk in &mut chunks {
            let lane = (self.words % 4) as usize;
            self.mix(lane, u64::from_le_bytes(chunk.try_into().expect("8 bytes")));
            self.words += 1;
        }
        let rest = chunks.remainder();
        self.tail[..rest.len()].copy_from_slice(rest);
        self.tail_len = rest.len();
    }

    /// An order-sensitive 64-bit digest, not a CRC: its only job is to show
    /// that two decoders produced the same bytes, and it has to cost far less
    /// than the decode it is measuring. (A byte-at-a-time CRC-32 here cost a
    /// quarter of the fork's measured time and made the fork look 30% slower
    /// than it is.)
    #[inline]
    fn mix(&mut self, lane: usize, word: u64) {
        self.lanes[lane] = (self.lanes[lane] ^ word)
            .wrapping_mul(DIGEST_K)
            .rotate_left(23);
    }

    /// Folds in whatever is left in the carry buffer. Call once per stream.
    fn finish(&mut self) -> u64 {
        for index in 0..self.tail_len {
            let lane = (self.words % 4) as usize;
            self.lanes[lane] =
                (self.lanes[lane] ^ u64::from(self.tail[index])).wrapping_mul(DIGEST_K);
        }
        self.tail_len = 0;
        let mut folded = self.bytes;
        for lane in self.lanes {
            folded = (folded ^ lane).wrapping_mul(DIGEST_K).rotate_left(23);
        }
        self.digest = folded;
        folded
    }
}

fn drain<R: Read + ?Sized>(reader: &mut R, sink: &mut Sink, buf: &mut [u8]) -> std::io::Result<()> {
    loop {
        let read = reader.read(buf)?;
        if read == 0 {
            return Ok(());
        }
        sink.update(&buf[..read]);
    }
}

fn extract_fork(path: &Path, threads: u32, password: Option<&str>, memory_limit: u64) -> Sink {
    extract_fork_with(path, threads, true, password, memory_limit)
}

/// The password each lane gets, or an empty one for an unencrypted archive.
fn fork_password(password: Option<&str>) -> sevenz_turbo::Password {
    password.map_or_else(sevenz_turbo::Password::empty, sevenz_turbo::Password::from)
}

/// The fork, with the header's checksums either checked or not. The unchecked
/// lane is there to price the checking: under the parallel path it is done by
/// the workers and folded, so the two should differ by noise.
fn extract_fork_with(
    path: &Path,
    threads: u32,
    verify: bool,
    password: Option<&str>,
    memory_limit: u64,
) -> Sink {
    let file = std::fs::File::open(path).expect("open archive");
    // A limit is what the consumer this is measured for always sets, so the
    // lane that sets one is the lane that matters; with none the reader is
    // built exactly as it was before the option existed.
    let mut reader = if memory_limit == u64::MAX {
        sevenz_turbo::ArchiveReader::new(file, fork_password(password)).expect("fork: read header")
    } else {
        let limits = sevenz_turbo::ArchiveLimits {
            memory_limit_bytes: memory_limit,
            ..sevenz_turbo::ArchiveLimits::default()
        };
        sevenz_turbo::ArchiveReader::with_limits(file, fork_password(password), limits)
            .expect("fork: read header")
    };
    reader.set_threads(threads);
    reader.set_verify_checksums(verify);
    let mut sink = Sink::default();
    let mut buf = vec![0u8; 1 << 20];
    reader
        .for_each_entries(|entry, rd| {
            if !entry.is_directory() {
                drain(rd, &mut sink, &mut buf)?;
            }
            Ok(true)
        })
        .expect("fork: extract");
    sink.finish();
    sink
}

/// Upstream at a given thread count. Above one this is its `Lzma2ReaderMt`,
/// the multi-threaded LZMA2 reader in `lzma-rust2` that this fork replaced.
fn extract_upstream(path: &Path, threads: u32, password: Option<&str>) -> Sink {
    let file = std::fs::File::open(path).expect("open archive");
    let upstream_password =
        password.map_or_else(sevenz_rust2::Password::empty, sevenz_rust2::Password::from);
    let mut reader =
        sevenz_rust2::ArchiveReader::new(file, upstream_password).expect("upstream: read header");
    reader.set_thread_count(threads);
    let mut sink = Sink::default();
    let mut buf = vec![0u8; 1 << 20];
    reader
        .for_each_entries(|entry, rd| {
            if !entry.is_directory() {
                drain(rd, &mut sink, &mut buf)?;
            }
            Ok(true)
        })
        .expect("upstream: extract");
    sink.finish();
    sink
}

/// `7zz t`: a full decode plus CRC check, with no output file, which is the
/// closest the CLI offers to what the library paths above do. `mmt` is the
/// thread count to pass, or `None` for the CLI's own default (every thread).
fn oracle_7zz(path: &Path, mmt: Option<u32>, password: Option<&str>) -> Option<Duration> {
    let mmt = mmt.map(|n| format!("-mmt={n}"));
    let start = Instant::now();
    let mut command = Command::new("7zz");
    command.args(["t", "-bso0", "-bsp0"]);
    if let Some(flag) = &mmt {
        command.arg(flag);
    }
    // `7zz` prompts when an encrypted archive gets no password, which in a
    // benchmark would hang rather than fail.
    command.arg(match password {
        Some(p) => format!("-p{p}"),
        None => "-p".to_string(),
    });
    let status = command
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    status.success().then(|| start.elapsed())
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

fn bench_one(
    path: &Path,
    runs: usize,
    oracle: bool,
    only: &str,
    threads: &[u32],
    password: Option<&str>,
    memory_limit: u64,
) {
    let wanted = |name: &str| only.is_empty() || only == name;
    let compressed = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    println!(
        "\n{} ({:.1} MiB packed)",
        path.display(),
        compressed as f64 / (1024.0 * 1024.0)
    );

    let mut rows: Vec<(String, Duration, Option<Sink>)> = Vec::new();

    // Times one library lane `runs` times and keeps the last sink, so that
    // every row can be shown to have produced the same bytes.
    let time_it = |name: String, mut once: Box<dyn FnMut() -> Sink + '_>| {
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..runs {
            print!("  {name} …");
            let _ = std::io::stdout().flush();
            let start = Instant::now();
            let sink = once();
            times.push(start.elapsed());
            last = Some(sink);
            print!("\r");
        }
        (name, median(times), last)
    };

    if oracle && wanted("7zz") {
        // `7zz t` with no `-mmt` is the CLI's own default: every thread. It is
        // the gate the fork's all-thread row is read against.
        for (label, mmt) in [("7zz t (all)", None), ("7zz t -mmt=1", Some(1))] {
            let mut times = Vec::new();
            for _ in 0..runs {
                print!("  {label} …");
                let _ = std::io::stdout().flush();
                match oracle_7zz(path, mmt, password) {
                    Some(elapsed) => times.push(elapsed),
                    None => {
                        println!("\r  {label}: unavailable          ");
                        times.clear();
                        break;
                    }
                }
                print!("\r");
            }
            if !times.is_empty() {
                rows.push((label.to_string(), median(times), None));
            }
        }
    }

    if wanted("upstream") {
        // Upstream's own multi-threaded path, which is `lzma-rust2`'s
        // `Lzma2ReaderMt`. Sixteen threads: it is what weaver would be asking
        // for today, and the crate does not scale past it on these fixtures.
        rows.push(time_it(
            "sevenz-rust2 0.22.2 @16".to_string(),
            Box::new(|| extract_upstream(path, 16, password)),
        ));
    }

    if wanted("fork") {
        for &count in threads {
            rows.push(time_it(
                format!("sevenz-turbo @{count}"),
                Box::new(move || extract_fork(path, count, password, memory_limit)),
            ));
        }
        if let Some(&count) = threads.last() {
            rows.push(time_it(
                format!("sevenz-turbo @{count} no crc"),
                Box::new(move || extract_fork_with(path, count, false, password, memory_limit)),
            ));
        }
    }

    let unpacked = rows
        .iter()
        .find_map(|(_, _, sink)| sink.as_ref().map(|s| s.bytes))
        .unwrap_or(0);
    println!("  {:.1} MiB unpacked", unpacked as f64 / (1024.0 * 1024.0));

    let baseline = rows
        .iter()
        .find(|(name, _, _)| name.starts_with("sevenz-rust2"))
        .map(|(_, time, _)| *time);
    let gate = rows
        .iter()
        .find(|(name, _, _)| name == "7zz t (all)")
        .map(|(_, time, _)| *time);

    println!(
        "  {:<24} {:>9}  {:>7}  {:>9}  {:>7}  digest",
        "decoder", "median", "MiB/s", "vs 0.22.2", "vs 7zz"
    );
    for (name, time, sink) in &rows {
        let secs = time.as_secs_f64();
        let throughput = if unpacked > 0 && secs > 0.0 {
            format!("{:.1}", unpacked as f64 / (1024.0 * 1024.0) / secs)
        } else {
            "-".to_string()
        };
        let ratio = match baseline {
            Some(base) => format!("{:.2}x", base.as_secs_f64() / secs),
            None => "-".to_string(),
        };
        let vs_gate = match gate {
            Some(base) => format!("{:.3}", secs / base.as_secs_f64()),
            None => "-".to_string(),
        };
        let crc = sink
            .as_ref()
            .map_or_else(|| "-".to_string(), |s| format!("{:016x}", s.digest));
        println!("  {name:<24} {secs:>8.3}s  {throughput:>7}  {ratio:>9}  {vs_gate:>7}  {crc}");
    }

    let digests: Vec<u64> = rows
        .iter()
        .filter_map(|(_, _, s)| s.as_ref().map(|s| s.digest))
        .collect();
    if digests.windows(2).any(|w| w[0] != w[1]) {
        println!("  WARNING: the decoders disagree on the output bytes");
    }
}

#[cfg(test)]
mod tests {
    use super::Sink;

    /// The digest must depend on the bytes and not on how they arrive: two
    /// decoders that chunk their reads differently have to agree, which is the
    /// entire point of comparing digests at all.
    #[test]
    fn the_digest_does_not_depend_on_how_the_bytes_are_chunked() {
        let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();

        let mut whole = Sink::default();
        whole.update(&bytes);
        let expected = whole.finish();

        for step in [1usize, 3, 7, 8, 9, 32, 33, 1000] {
            let mut piecemeal = Sink::default();
            for chunk in bytes.chunks(step) {
                piecemeal.update(chunk);
            }
            assert_eq!(
                piecemeal.finish(),
                expected,
                "reads of {step} bytes disagree with one read"
            );
        }
    }
}
