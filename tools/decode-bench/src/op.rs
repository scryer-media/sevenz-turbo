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
//!                        [--engine turbo|upstream]
//! decode-bench op encode --input DIR --out FILE [--level L] [--threads N]
//!                        [--non-solid] [--password P]
//! ```
//!
//! Exit status 0 with the JSON line on success; 1 with
//! `{"ok":false,"error":...}` when the operation itself failed; 2 on a usage
//! error.

use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;

use super::{Sink, drain, fork_password};

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

/// What the binary is: the crate version, the cryptography backend the build
/// selected, and the versions locked when it was built.
fn version() -> Fields {
    vec![
        ("decode_bench", str(env!("CARGO_PKG_VERSION"))),
        ("crypto_backend", str(sevenz_turbo::crypto_backend())),
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

/// A full decode through this crate, every entry into the digesting sink.
///
/// `--stream` is the streaming consumer's path rather than the convenience
/// one: the header is parsed once, each block's pack-stream ranges and
/// sub-stream sizes are asked of the parsed archive, every block is decoded
/// through `ArchiveReader::block_decoder` on the same source, and each file's
/// CRC-32 is taken from the sub-stream hook rather than recomputed.
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
    reader.set_threads(opts.threads.max(1));
    reader.set_verify_checksums(!opts.no_verify);
    let handle = reader.lzma2_handle();

    let mut sink = Sink::default();
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
    let reported = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    if opts.stream {
        let counter = std::sync::Arc::clone(&reported);
        reader.set_sub_stream_complete_hook(move |done| {
            // The CRC-32 the decoder already verified; a consumer reporting
            // per-file integrity takes it from here.
            std::hint::black_box(done.crc32);
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        for block in 0..reader.archive().blocks.len() {
            pack_ranges += reader.archive().block_pack_streams(block).len() as u64;
            sub_streams += reader.archive().block_sub_streams(block).len() as u64;
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
    let digest = sink.finish();
    let mut fields = vec![
        ("engine", str("sevenz-turbo")),
        ("crypto_backend", str(sevenz_turbo::crypto_backend())),
        ("threads", Json::Int(u64::from(opts.threads.max(1)))),
        ("entries", Json::Int(entries)),
        ("blocks", Json::Int(blocks)),
        ("bytes_out", Json::Int(sink.bytes)),
        ("digest", Json::Str(format!("{digest:016x}"))),
        ("parse_seconds", Json::Float(parse)),
        ("verify", Json::Bool(!opts.no_verify)),
        ("parallel_path", Json::Bool(parallel_seen)),
        ("max_spawned_threads", Json::Int(u64::from(max_spawned))),
    ];
    if let Some(limit) = opts.memory_limit {
        fields.push(("memory_limit", Json::Int(limit)));
    }
    if opts.stream {
        fields.push(("stream", Json::Bool(true)));
        fields.push(("pack_ranges", Json::Int(pack_ranges)));
        fields.push(("sub_streams", Json::Int(sub_streams)));
        fields.push((
            "crcs_reported",
            Json::Int(reported.load(std::sync::atomic::Ordering::Relaxed)),
        ));
    }
    Ok(fields)
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
    let mut sink = Sink::default();
    let mut buf = vec![0u8; 1 << 20];
    let mut entries = 0u64;
    reader
        .for_each_entries(|entry, rd| {
            if !entry.is_directory() {
                entries += 1;
                drain(rd, &mut sink, &mut buf)?;
            }
            Ok(true)
        })
        .map_err(|e| e.to_string())?;
    let digest = sink.finish();
    Ok(vec![
        ("engine", str("sevenz-rust2")),
        ("threads", Json::Int(u64::from(opts.threads.max(1)))),
        ("entries", Json::Int(entries)),
        ("bytes_out", Json::Int(sink.bytes)),
        ("digest", Json::Str(format!("{digest:016x}"))),
    ])
}

/// The block size `7zz` uses for a multi-threaded LZMA2 encode when none is
/// given: four dictionaries, between 1 MiB and 256 MiB. Using the same one
/// makes the size ratio against `7zz a -mmt=N` a like-for-like comparison.
fn mt_block_size(dict: u32) -> u64 {
    (u64::from(dict) * 4).clamp(1 << 20, 256 << 20)
}

/// Writes an archive of `--input` (a directory) with LZMA2 at `--level`,
/// solid unless `--non-solid`, AES-256 when `--password` is given.
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

    let mut writer = sevenz_turbo::ArchiveWriter::create(out).map_err(|e| e.to_string())?;
    writer.set_content_methods(methods);
    if opts.non_solid {
        writer
            .push_source_path_non_solid(input, |_| true)
            .map_err(|e| e.to_string())?;
    } else {
        writer
            .push_source_path(input, |_| true)
            .map_err(|e| e.to_string())?;
    }
    writer.finish().map_err(|e| e.to_string())?;

    let bytes_out = std::fs::metadata(out).map_err(|e| e.to_string())?.len();
    let mut bytes_in = 0u64;
    let mut files = 0u64;
    let mut stack = vec![input.clone()];
    while let Some(dir) = stack.pop() {
        if dir.is_file() {
            bytes_in += std::fs::metadata(&dir).map_err(|e| e.to_string())?.len();
            files += 1;
            continue;
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            stack.push(entry.map_err(|e| e.to_string())?.path());
        }
    }
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
