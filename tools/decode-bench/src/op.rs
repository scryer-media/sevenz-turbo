//! `decode-bench op <kind> ...`: exactly one operation per process, and one
//! JSON object on the last line of stdout.
//!
//! This is the lane `bench/sevenz-turbo-bench` drives. One operation per
//! process is the point: the harness reads wall time, CPU time and the peak
//! resident set from the kernel's accounting of the exited child, so each
//! row's peak RSS is that operation's and nothing else's, measured the same
//! way `7zz` is measured. The human-oriented modes above stay as they were.
//!
//! ```text
//! decode-bench op version
//! decode-bench op list   --archive A [--password P]
//! decode-bench op decode --archive A [--threads N] [--password P]
//!                        [--memory-limit BYTES] [--no-verify] [--stream]
//!                        [--adaptive [--poll-ms MS]]
//!                        [--engine turbo|upstream] [--digest]
//! decode-bench op encode --input DIR --out FILE [--level L] [--threads N]
//!                        [--non-solid] [--password P]
//! ```
//!
//! A decode counts the bytes it drains and does nothing else with them, as
//! `7zz t` does: the timed rows are run without `--digest`. The harness asks
//! for the order-sensitive digest of the output in a separate, untimed run,
//! to hold every engine to the same bytes.
//!
//! Exit status 0 with the JSON line on success; 1 with
//! `{"ok":false,"error":...}` when the operation itself failed; 2 on a usage
//! error.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{Sink, fork_password};

const USAGE: &str = "usage: decode-bench op version|list|decode|encode [options]";

#[derive(Default)]
struct Opts {
    archive: Option<PathBuf>,
    input: Option<PathBuf>,
    out: Option<PathBuf>,
    threads: u32,
    password: Option<String>,
    memory_limit: Option<u64>,
    no_verify: bool,
    stream: bool,
    adaptive: bool,
    poll_ms: u64,
    digest: bool,
    engine: String,
    level: u32,
    non_solid: bool,
}

/// Runs one operation and exits.
pub fn main(args: Vec<String>) -> ! {
    let Some((kind, rest)) = args.split_first() else {
        usage("op needs a kind")
    };
    let opts = parse(rest);
    let started = Instant::now();
    let result = match kind.as_str() {
        "version" => Ok(version()),
        "list" => list(&opts),
        "decode" => match opts.engine.as_str() {
            "" | "turbo" => decode_turbo(&opts),
            "upstream" => decode_upstream(&opts),
            other => usage(&format!("unknown engine {other}")),
        },
        "encode" => encode(&opts),
        other => usage(&format!("unknown op {other}")),
    };
    match result {
        Ok(mut fields) => {
            fields.push(("ok", Json::Bool(true)));
            fields.push(("op", Json::Str(kind.clone())));
            fields.push((
                "inner_seconds",
                Json::Float(started.elapsed().as_secs_f64()),
            ));
            println!("{}", render(&fields));
            std::process::exit(0);
        }
        Err(error) => {
            let fields = vec![
                ("ok", Json::Bool(false)),
                ("op", Json::Str(kind.clone())),
                ("error", Json::Str(error)),
            ];
            println!("{}", render(&fields));
            std::process::exit(1);
        }
    }
}

fn usage(message: &str) -> ! {
    eprintln!("decode-bench op: {message}\n{USAGE}");
    std::process::exit(2);
}

fn parse(args: &[String]) -> Opts {
    let mut opts = Opts {
        threads: 1,
        level: 5,
        poll_ms: ADAPTIVE_POLL_MS,
        ..Opts::default()
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .unwrap_or_else(|| usage(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--archive" => opts.archive = Some(PathBuf::from(value())),
            "--input" => opts.input = Some(PathBuf::from(value())),
            "--out" => opts.out = Some(PathBuf::from(value())),
            "--threads" => {
                let text = value();
                opts.threads = if text == "all" {
                    super::all_threads()
                } else {
                    text.parse()
                        .unwrap_or_else(|_| usage("--threads takes a number or `all`"))
                };
            }
            "--password" => opts.password = Some(value()),
            "--memory-limit" => {
                opts.memory_limit = Some(
                    value()
                        .parse()
                        .unwrap_or_else(|_| usage("--memory-limit takes bytes")),
                );
            }
            "--no-verify" => opts.no_verify = true,
            "--stream" => opts.stream = true,
            "--adaptive" => opts.adaptive = true,
            "--poll-ms" => {
                opts.poll_ms = value()
                    .parse()
                    .unwrap_or_else(|_| usage("--poll-ms takes milliseconds"));
            }
            "--digest" => opts.digest = true,
            "--engine" => opts.engine = value(),
            "--level" => {
                opts.level = value()
                    .parse()
                    .unwrap_or_else(|_| usage("--level takes 0-9"));
            }
            "--non-solid" => opts.non_solid = true,
            other => usage(&format!("unknown option {other}")),
        }
    }
    opts
}

type Fields = Vec<(&'static str, Json)>;

enum Json {
    Bool(bool),
    Int(u64),
    Float(f64),
    Str(String),
}

fn render(fields: &Fields) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&quote(key));
        out.push(':');
        match value {
            Json::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Json::Int(value) => out.push_str(&value.to_string()),
            Json::Float(value) => out.push_str(&format!("{value:.6}")),
            Json::Str(value) => out.push_str(&quote(value)),
        }
    }
    out.push('}');
    out
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn str(value: &str) -> Json {
    Json::Str(value.to_string())
}

/// The `Cargo.lock` this binary was built against.
const CARGO_LOCK: &[u8] = include_bytes!("../../../Cargo.lock");

/// The SHA-256 of [`CARGO_LOCK`], lower-case hex: the same digest the harness
/// takes of a checkout's `Cargo.lock`, to tell whether that checkout built
/// this binary. Taken by the crate's own backend, so the `native-crypto`
/// build does not carry AWS-LC's SHA-256 beside RustCrypto's for this one
/// digest.
fn cargo_lock_sha256() -> String {
    sevenz_turbo::sha256(CARGO_LOCK)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What the binary is: the crate version, the cryptography backend and LZMA
/// encoder the build selected, the Cargo profile it was built under, the
/// commit and `Cargo.lock` it was built from and whether its sources had
/// uncommitted changes then, and the versions locked when it was built.
fn version() -> Fields {
    vec![
        ("decode_bench", str(env!("CARGO_PKG_VERSION"))),
        ("crypto_backend", str(sevenz_turbo::crypto_backend())),
        ("lzma_encoder", str(sevenz_turbo::lzma_encoder())),
        ("build_profile", str(env!("DECODE_BENCH_PROFILE"))),
        ("git_commit", str(env!("DECODE_BENCH_GIT_COMMIT"))),
        ("git_dirty", str(env!("DECODE_BENCH_GIT_DIRTY"))),
        ("cargo_lock_sha256", Json::Str(cargo_lock_sha256())),
        (
            "sevenz_turbo",
            str(env!("DECODE_BENCH_SEVENZ_TURBO_VERSION")),
        ),
        ("lzma_turbo", str(env!("DECODE_BENCH_LZMA_TURBO_VERSION"))),
        (
            "sevenz_rust2",
            str(env!("DECODE_BENCH_SEVENZ_RUST2_VERSION")),
        ),
        ("aws_lc_rs", str(env!("DECODE_BENCH_AWS_LC_RS_VERSION"))),
        ("crc_fast", str(env!("DECODE_BENCH_CRC_FAST_VERSION"))),
        ("ppmd_rust", str(env!("DECODE_BENCH_PPMD_RUST_VERSION"))),
        (
            "available_parallelism",
            Json::Int(u64::from(super::all_threads())),
        ),
    ]
}

/// Where a decode's bytes go: counted, and digested only when `--digest`
/// asked, so a timed row does no work on the output that `7zz t` does not.
struct Output {
    bytes: u64,
    digest: Option<Sink>,
}

impl Output {
    fn new(digest: bool) -> Self {
        Self {
            bytes: 0,
            digest: digest.then(Sink::default),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len() as u64;
        if let Some(sink) = &mut self.digest {
            sink.update(bytes);
        }
    }

    fn drain<R: Read + ?Sized>(&mut self, reader: &mut R, buf: &mut [u8]) -> std::io::Result<()> {
        loop {
            let read = reader.read(buf)?;
            if read == 0 {
                return Ok(());
            }
            self.update(&buf[..read]);
        }
    }

    /// `bytes_out`, and `digest` when one was taken.
    fn fields(self, fields: &mut Fields) {
        fields.push(("bytes_out", Json::Int(self.bytes)));
        if let Some(mut sink) = self.digest {
            fields.push(("digest", Json::Str(format!("{:016x}", sink.finish()))));
        }
    }
}

fn archive_path(opts: &Opts) -> &PathBuf {
    opts.archive
        .as_ref()
        .unwrap_or_else(|| usage("--archive is required"))
}

fn limits(opts: &Opts) -> sevenz_turbo::ArchiveLimits {
    match opts.memory_limit {
        Some(bytes) => sevenz_turbo::ArchiveLimits::memory(bytes),
        None => sevenz_turbo::ArchiveLimits::default(),
    }
}

/// The header pass alone: open, parse, and walk every entry's name, size and
/// CRC, which is what `7zz l` does.
fn list(opts: &Opts) -> Result<Fields, String> {
    let path = archive_path(opts);
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let archive = sevenz_turbo::Archive::read_with_limits(
        &mut file,
        &fork_password(opts.password.as_deref()),
        &limits(opts),
    )
    .map_err(|e| e.to_string())?;
    let mut names = 0u64;
    let mut bytes = 0u64;
    let mut crcs = 0u64;
    for entry in &archive.files {
        names += entry.name.len() as u64;
        bytes += entry.size;
        if entry.has_crc {
            crcs += 1;
        }
    }
    let parse = started.elapsed().as_secs_f64();
    let memory = archive.decoder_memory_estimate().unwrap_or(0);
    Ok(vec![
        ("entries", Json::Int(archive.files.len() as u64)),
        ("blocks", Json::Int(archive.blocks.len() as u64)),
        ("bytes_out", Json::Int(bytes)),
        ("name_bytes", Json::Int(names)),
        ("crcs", Json::Int(crcs)),
        ("parse_seconds", Json::Float(parse)),
        ("decoder_memory_estimate", Json::Int(memory)),
    ])
}

/// A full decode through this crate, every entry drained into [`Output`].
///
/// `--stream` is the streaming consumer's path rather than the convenience
/// one: the header is parsed once, each block's pack-stream ranges and
/// sub-stream sizes are asked of the parsed archive, every block is decoded
/// through `ArchiveReader::block_decoder` on the same source, and each file's
/// CRC-32 is taken from the sub-stream hook rather than recomputed.
/// How often the adaptive lane's governor looks at the decode: the interval
/// weaver's direct-unpack chase polls at.
const ADAPTIVE_POLL_MS: u64 = 100;

/// The adaptive lane's governor: weaver's chase decode. The reader starts at
/// one thread with the adaptive coder engaged, and a thread beside it polls
/// the decode and sets the ceiling to one more than the complete runs waiting,
/// up to `--threads`. Weaver also pays for each widening from a shared memory
/// budget first; this lane leaves that out, so it shows the decoder's side of
/// the chase on an archive that is all there.
struct Governor {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    widest: std::sync::Arc<std::sync::atomic::AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Governor {
    fn start(handle: sevenz_turbo::Lzma2Handle, ceiling: u32, poll: std::time::Duration) -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let widest = Arc::new(AtomicU32::new(1));
        let thread = (ceiling > 1).then(|| {
            let (stop, widest) = (Arc::clone(&stop), Arc::clone(&widest));
            std::thread::spawn(move || {
                let mut applied = 1u32;
                while !stop.load(Ordering::Acquire) {
                    std::thread::park_timeout(poll);
                    let Some(progress) = handle.progress() else {
                        continue;
                    };
                    let target = u32::try_from(progress.pending_runs)
                        .unwrap_or(u32::MAX)
                        .saturating_add(1)
                        .clamp(1, ceiling);
                    if target != applied {
                        handle.set_threads(target);
                        applied = target;
                        widest.fetch_max(target, Ordering::AcqRel);
                    }
                }
            })
        });
        Self {
            stop,
            widest,
            thread,
        }
    }

    /// Stops the governor and returns the widest ceiling it set.
    fn finish(mut self) -> u32 {
        self.stop_thread();
        self.widest.load(std::sync::atomic::Ordering::Acquire)
    }

    fn stop_thread(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl Drop for Governor {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

fn decode_turbo(opts: &Opts) -> Result<Fields, String> {
    let path = archive_path(opts);
    let file = File::open(path).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut reader = sevenz_turbo::ArchiveReader::with_limits(
        file,
        fork_password(opts.password.as_deref()),
        limits(opts),
    )
    .map_err(|e| e.to_string())?;
    let parse = started.elapsed().as_secs_f64();
    let ceiling = opts.threads.max(1);
    if opts.adaptive {
        reader.set_adaptive_lzma2(ceiling > 1);
        reader.set_threads(1);
    } else {
        reader.set_threads(ceiling);
    }
    reader.set_verify_checksums(!opts.no_verify);
    let handle = reader.lzma2_handle();
    let governor = opts.adaptive.then(|| {
        Governor::start(
            handle.clone(),
            ceiling,
            std::time::Duration::from_millis(opts.poll_ms),
        )
    });

    let mut sink = Output::new(opts.digest);
    let mut buf = vec![0u8; 1 << 20];
    let mut entries = 0u64;
    let mut max_spawned = 0u32;
    let mut parallel_seen = false;
    let mut each = |entry: &sevenz_turbo::ArchiveEntry,
                    rd: &mut dyn std::io::Read|
     -> Result<bool, sevenz_turbo::Error> {
        if !entry.is_directory() {
            entries += 1;
            loop {
                let read = rd.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                sink.update(&buf[..read]);
                if let Some(progress) = handle.progress() {
                    parallel_seen = true;
                    max_spawned = max_spawned.max(progress.spawned_threads);
                }
            }
        }
        Ok(true)
    };

    let blocks = reader.archive().blocks.len() as u64;
    let mut pack_ranges = 0u64;
    let mut sub_streams = 0u64;
    let reported = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(usize, u32)>::new()));
    let mut expected = std::collections::BTreeMap::new();
    if opts.stream {
        let sink = std::sync::Arc::clone(&reported);
        reader.set_sub_stream_complete_hook(move |done| {
            // The CRC-32 the decoder already verified; a consumer reporting
            // per-file integrity takes it from here. The pair is checked
            // against the header once the decode is done.
            if let Ok(mut reported) = sink.lock() {
                reported.push((done.sub_stream_index, done.crc32));
            }
        });
        for block in 0..reader.archive().blocks.len() {
            pack_ranges += reader.archive().block_pack_streams(block).len() as u64;
            let streams = reader.archive().block_sub_streams(block);
            sub_streams += streams.len() as u64;
            for stream in streams {
                if let Some(crc) = stream.crc {
                    expected.insert(stream.index, crc);
                }
            }
            let decoder = reader.block_decoder(block).map_err(|e| e.to_string())?;
            decoder
                .for_each_entries(&mut each)
                .map_err(|e| e.to_string())?;
        }
    } else {
        reader
            .for_each_entries(&mut each)
            .map_err(|e| e.to_string())?;
    }
    let widest = governor.map(Governor::finish);
    let crcs_reported = if opts.stream {
        let reported = reported
            .lock()
            .map_err(|_| "the sub-stream hook panicked".to_string())?;
        check_reported_crcs(&expected, &reported, !opts.no_verify)?;
        reported.len() as u64
    } else {
        0
    };
    let mut fields = vec![
        ("engine", str("sevenz-turbo")),
        ("crypto_backend", str(sevenz_turbo::crypto_backend())),
        ("threads", Json::Int(u64::from(opts.threads.max(1)))),
        ("entries", Json::Int(entries)),
        ("blocks", Json::Int(blocks)),
    ];
    sink.fields(&mut fields);
    fields.extend([
        ("parse_seconds", Json::Float(parse)),
        ("verify", Json::Bool(!opts.no_verify)),
        ("parallel_path", Json::Bool(parallel_seen)),
        ("max_spawned_threads", Json::Int(u64::from(max_spawned))),
    ]);
    if let Some(limit) = opts.memory_limit {
        fields.push(("memory_limit", Json::Int(limit)));
    }
    if let Some(widest) = widest {
        fields.push(("adaptive", Json::Bool(true)));
        fields.push(("widest_threads", Json::Int(u64::from(widest))));
    }
    if opts.stream {
        fields.push(("stream", Json::Bool(true)));
        fields.push(("pack_ranges", Json::Int(pack_ranges)));
        fields.push(("sub_streams", Json::Int(sub_streams)));
        fields.push(("crcs_reported", Json::Int(crcs_reported)));
    }
    Ok(fields)
}

/// The streaming lane's check of the sub-stream hook: with verification on,
/// it fired exactly once for every sub-stream the header records a CRC-32 for,
/// with that CRC. A hook that stops firing, fires twice, or hands over a
/// different number fails the row instead of passing it as healthy.
fn check_reported_crcs(
    expected: &std::collections::BTreeMap<usize, u32>,
    reported: &[(usize, u32)],
    verify: bool,
) -> Result<(), String> {
    if !verify {
        return Ok(());
    }
    let mut seen = std::collections::BTreeSet::new();
    for &(index, crc) in reported {
        match expected.get(&index) {
            None => {
                return Err(format!(
                    "sub-stream hook reported unexpected sub-stream {index}"
                ));
            }
            Some(&want) if want != crc => {
                return Err(format!(
                    "sub-stream hook reported CRC {crc:08x} for sub-stream {index}, the header records {want:08x}"
                ));
            }
            Some(_) => {}
        }
        if !seen.insert(index) {
            return Err(format!("sub-stream hook reported sub-stream {index} twice"));
        }
    }
    if seen.len() != expected.len() {
        return Err(format!(
            "sub-stream hook reported {} of the {} sub-streams with a CRC",
            seen.len(),
            expected.len()
        ));
    }
    Ok(())
}

/// The same decode through upstream `sevenz-rust2`, the secondary reference.
fn decode_upstream(opts: &Opts) -> Result<Fields, String> {
    let path = archive_path(opts);
    let file = File::open(path).map_err(|e| e.to_string())?;
    let password = opts
        .password
        .as_deref()
        .map_or_else(sevenz_rust2::Password::empty, sevenz_rust2::Password::from);
    let mut reader = sevenz_rust2::ArchiveReader::new(file, password).map_err(|e| e.to_string())?;
    reader.set_thread_count(opts.threads.max(1));
    let mut sink = Output::new(opts.digest);
    let mut buf = vec![0u8; 1 << 20];
    let mut entries = 0u64;
    reader
        .for_each_entries(|entry, rd| {
            if !entry.is_directory() {
                entries += 1;
                sink.drain(rd, &mut buf)?;
            }
            Ok(true)
        })
        .map_err(|e| e.to_string())?;
    let mut fields = vec![
        ("engine", str("sevenz-rust2")),
        ("threads", Json::Int(u64::from(opts.threads.max(1)))),
        ("entries", Json::Int(entries)),
    ];
    sink.fields(&mut fields);
    Ok(fields)
}

/// The block size `7zz` uses for a multi-threaded LZMA2 encode when none is
/// given: four dictionaries, between 1 MiB and 256 MiB. Using the same one
/// makes the size ratio against `7zz a -mmt=N` a like-for-like comparison.
fn mt_block_size(dict: u32) -> u64 {
    (u64::from(dict) * 4).clamp(1 << 20, 256 << 20)
}

/// One regular file under an encode's `--input`.
struct Member {
    path: PathBuf,
    /// The archive name: the path relative to `--input`.
    name: String,
    size: u64,
}

/// The files `7zz a` is given for the same source: every regular file under
/// `input`, skipping top-level names that start with `.` (the fixture
/// generator's `.complete` marker; the harness leaves them off `7zz`'s
/// command line too), sorted by relative path so the order of a solid stream
/// does not depend on how the filesystem enumerates a directory.
fn source_members(input: &Path) -> Result<Vec<Member>, String> {
    let mut members = Vec::new();
    if input.is_file() {
        let size = std::fs::metadata(input).map_err(|e| e.to_string())?.len();
        let name = input
            .file_name()
            .ok_or_else(|| format!("{} has no file name", input.display()))?
            .to_string_lossy()
            .to_string();
        members.push(Member {
            path: input.to_path_buf(),
            name,
            size,
        });
        return Ok(members);
    }
    let mut stack = vec![input.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            if dir == input && entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                let path = entry.path();
                let size = entry.metadata().map_err(|e| e.to_string())?.len();
                let name = path
                    .strip_prefix(input)
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .to_string();
                members.push(Member { path, name, size });
            }
        }
    }
    members.sort_by(|a, b| {
        Path::new(&a.name)
            .components()
            .cmp(Path::new(&b.name).components())
    });
    Ok(members)
}

/// A member's file, opened on its first read and closed at its end, so a
/// solid block of thousands of members does not hold thousands of handles.
struct LazyFile {
    path: PathBuf,
    file: Option<File>,
    done: bool,
}

impl Read for LazyFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        let file = match &mut self.file {
            Some(file) => file,
            None => self.file.insert(File::open(&self.path)?),
        };
        let read = file.read(buf)?;
        if read == 0 && !buf.is_empty() {
            self.file = None;
            self.done = true;
        }
        Ok(read)
    }
}

/// The largest solid block the crate's own `push_source_path` builds.
const MAX_SOLID_BLOCK: u64 = 4 << 30;

/// Writes an archive of `--input` (a directory) with LZMA2 at `--level`,
/// solid unless `--non-solid`, AES-256 when `--password` is given.
///
/// The members are walked once, before encoding, and that walk is where the
/// reported file and byte counts come from: there is no second pass over the
/// source after the archive is finished.
fn encode(opts: &Opts) -> Result<Fields, String> {
    use sevenz_turbo::encoder_options::{AesEncoderOptions, EncoderOptions, Lzma2Options};

    let input = opts
        .input
        .as_ref()
        .unwrap_or_else(|| usage("--input is required"));
    let out = opts
        .out
        .as_ref()
        .unwrap_or_else(|| usage("--out is required"));
    let threads = opts.threads.max(1);
    let dict = EncoderOptions::from(Lzma2Options::from_level(opts.level)).get_lzma_dict_size();
    let block = mt_block_size(dict);
    let lzma2 = if threads > 1 {
        Lzma2Options::from_level_mt(opts.level, threads, block)
    } else {
        Lzma2Options::from_level(opts.level)
    };
    let mut methods = Vec::new();
    if let Some(password) = opts.password.as_deref() {
        methods.push(AesEncoderOptions::new(sevenz_turbo::Password::from(password)).into());
    }
    methods.push(lzma2.into());

    let members = source_members(input)?;
    let files = members.len() as u64;
    let bytes_in: u64 = members.iter().map(|m| m.size).sum();

    let mut writer = sevenz_turbo::ArchiveWriter::create(out).map_err(|e| e.to_string())?;
    writer.set_content_methods(methods);
    let entry = |m: &Member| sevenz_turbo::ArchiveEntry::from_path(&m.path, m.name.clone());
    if opts.non_solid {
        for member in &members {
            let file = File::open(&member.path).map_err(|e| e.to_string())?;
            writer
                .push_archive_entry(entry(member), Some(file))
                .map_err(|e| e.to_string())?;
        }
    } else {
        // The crate's `push_source_path` rule: a block closes before it would
        // reach 4 GiB, and a member that size or larger is a block of its own.
        let mut entries = Vec::new();
        let mut sources = Vec::new();
        let mut block_bytes = 0u64;
        for member in &members {
            let lazy = || LazyFile {
                path: member.path.clone(),
                file: None,
                done: false,
            };
            if member.size >= MAX_SOLID_BLOCK {
                writer
                    .push_archive_entry(entry(member), Some(lazy()))
                    .map_err(|e| e.to_string())?;
                continue;
            }
            if block_bytes + member.size >= MAX_SOLID_BLOCK {
                writer
                    .push_archive_entries(
                        std::mem::take(&mut entries),
                        std::mem::take(&mut sources),
                    )
                    .map_err(|e| e.to_string())?;
                block_bytes = 0;
            }
            block_bytes += member.size;
            entries.push(entry(member));
            sources.push(sevenz_turbo::SourceReader::new(lazy()));
        }
        if !entries.is_empty() {
            writer
                .push_archive_entries(entries, sources)
                .map_err(|e| e.to_string())?;
        }
    }
    writer.finish().map_err(|e| e.to_string())?;

    let bytes_out = std::fs::metadata(out).map_err(|e| e.to_string())?.len();
    Ok(vec![
        ("engine", str("sevenz-turbo")),
        ("crypto_backend", str(sevenz_turbo::crypto_backend())),
        ("threads", Json::Int(u64::from(threads))),
        ("level", Json::Int(u64::from(opts.level))),
        ("dictionary", Json::Int(u64::from(dict))),
        ("block_size", Json::Int(if threads > 1 { block } else { 0 })),
        ("solid", Json::Bool(!opts.non_solid)),
        ("encrypted", Json::Bool(opts.password.is_some())),
        ("files", Json::Int(files)),
        ("bytes_in", Json::Int(bytes_in)),
        ("bytes_out", Json::Int(bytes_out)),
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::{Opts, check_reported_crcs, encode, source_members};

    /// A scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("decode-bench-op-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree(root: &std::path::Path) {
        for (name, bytes) in [
            ("zeta.txt", &b"zeta"[..]),
            ("alpha/b.bin", b"bb"),
            ("alpha/a.bin", b"a"),
            ("mid/.nested-dot", b"kept"),
            (".complete", b"marker"),
            (".hidden/inner.txt", b"skipped"),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }

    #[test]
    fn members_are_sorted_and_skip_top_level_dot_names() {
        let scratch = Scratch::new("members");
        tree(&scratch.0);
        let members = source_members(&scratch.0).unwrap();
        let names: Vec<_> = members.iter().map(|m| m.name.replace('\\', "/")).collect();
        assert_eq!(
            names,
            ["alpha/a.bin", "alpha/b.bin", "mid/.nested-dot", "zeta.txt"]
        );
        assert_eq!(members.iter().map(|m| m.size).sum::<u64>(), 1 + 2 + 4 + 4);
    }

    #[test]
    fn an_encode_writes_the_sorted_members_without_the_marker() {
        let scratch = Scratch::new("encode");
        let input = scratch.0.join("src");
        tree(&input);
        for non_solid in [false, true] {
            let out = scratch.0.join(format!("out-{non_solid}.7z"));
            let opts = Opts {
                input: Some(input.clone()),
                out: Some(out.clone()),
                threads: 1,
                level: 1,
                non_solid,
                ..Opts::default()
            };
            encode(&opts).unwrap();
            let archive = sevenz_turbo::Archive::open(&out).unwrap();
            let names: Vec<_> = archive
                .files
                .iter()
                .map(|f| f.name.replace('\\', "/"))
                .collect();
            assert_eq!(
                names,
                ["alpha/a.bin", "alpha/b.bin", "mid/.nested-dot", "zeta.txt"],
                "non_solid {non_solid}"
            );
        }
    }

    #[test]
    fn the_stream_lane_refuses_a_hook_that_misses_or_misreports() {
        let expected: BTreeMap<usize, u32> = [(0, 0xAA), (1, 0xBB), (2, 0xCC)].into();
        assert!(check_reported_crcs(&expected, &[(0, 0xAA), (1, 0xBB), (2, 0xCC)], true).is_ok());
        // One file never reported.
        assert!(check_reported_crcs(&expected, &[(0, 0xAA), (2, 0xCC)], true).is_err());
        // Nothing reported at all.
        assert!(check_reported_crcs(&expected, &[], true).is_err());
        // A wrong CRC, a duplicate and a stranger.
        assert!(check_reported_crcs(&expected, &[(0, 0xAA), (1, 0x00), (2, 0xCC)], true).is_err());
        assert!(
            check_reported_crcs(
                &expected,
                &[(0, 0xAA), (0, 0xAA), (1, 0xBB), (2, 0xCC)],
                true
            )
            .is_err()
        );
        assert!(
            check_reported_crcs(&expected, &[(0, 0xAA), (1, 0xBB), (2, 0xCC), (9, 1)], true)
                .is_err()
        );
        // Without verification the hook is not held to anything.
        assert!(check_reported_crcs(&expected, &[], false).is_ok());
    }
}
