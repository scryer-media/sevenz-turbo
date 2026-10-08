//! Differential extraction against the 7-Zip CLI.
//!
//! The fork swapped the LZMA and LZMA2 decoders. The test that swap has to pass
//! is not "the crate's own archives still round-trip" — upstream's tests cover
//! that — but "an archive 7-Zip wrote extracts to exactly what 7-Zip extracts
//! it to". So every case here builds an archive with `7zz a`, extracts it twice
//! (once with `7zz x`, once with this crate) and compares the bytes.
//!
//! Every case is extracted three times by this crate — single-threaded, with
//! eight threads, and through the adaptive coder while the ceiling is one —
//! and all three are compared against `7zz`'s own extraction. The thread count
//! must not be able to change a byte: a run boundary is a dictionary reset, so
//! which decoder took which run is not observable in the output.
//!
//! `7zz` is not installed on the CI runners, so the whole file skips itself
//! when `7zz` is not on `PATH`. Run it locally before proposing a decoder
//! change; `docs/benchmarking.md` records the matrix it covers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use sevenz_turbo::{ArchiveReader, Password};

/// Is the oracle available?
fn have_7zz() -> bool {
    Command::new("7zz")
        .arg("i")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Deterministic, barely compressible bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Deterministic, very compressible bytes.
fn text(len: usize) -> Vec<u8> {
    const LINE: &[u8] = b"the quick brown fox jumps over the lazy dog, again and again\n";
    LINE.iter().copied().cycle().take(len).collect()
}

/// Bytes with the branch opcodes the BCJ filters look for, so a filtered
/// archive actually exercises the filter.
fn pseudo_x86(len: usize) -> Vec<u8> {
    let mut out = noise(len, 99);
    let mut i = 0;
    while i + 5 <= out.len() {
        out[i] = if i % 10 == 0 { 0xE8 } else { 0xE9 };
        i += 37;
    }
    out
}

/// The members every archive in the matrix holds.
///
/// The text member alone is larger than the smallest block the reader decodes
/// in parallel, so the eight-thread and adaptive lanes take the parallel path
/// for every block that holds it, solid or not, rather than all being decoded
/// single-threaded as too small to be worth it.
fn members() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("silver_horizon/noise.bin", noise(300 * 1024, 7)),
        ("silver_horizon/text.txt", text(1280 * 1024)),
        ("silver_horizon/inner/code.bin", pseudo_x86(128 * 1024)),
        ("silver_horizon/tiny.txt", b"one line\n".to_vec()),
    ]
}

fn write_members(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for (name, bytes) in members() {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("member has a parent")).expect("mkdir");
        std::fs::write(&path, &bytes).expect("write member");
        names.push(name.to_string());
    }
    names
}

/// Runs `7zz` and fails the test with its output if it did not succeed.
fn run_7zz(cwd: &Path, args: &[&str]) {
    let output = Command::new("7zz")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run 7zz");
    assert!(
        output.status.success(),
        "7zz {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every regular file under `root`, keyed by its path relative to `root`.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let key = path
                    .strip_prefix(base)
                    .expect("under base")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(key, std::fs::read(&path).expect("read member"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// How this crate should decode a case.
#[derive(Debug, Clone, Copy)]
struct Lane {
    name: &'static str,
    threads: u32,
    adaptive: bool,
}

const LANES: [Lane; 3] = [
    Lane {
        name: "threads=1",
        threads: 1,
        adaptive: false,
    },
    Lane {
        name: "threads=8",
        threads: 8,
        adaptive: false,
    },
    Lane {
        name: "adaptive, threads=1",
        threads: 1,
        adaptive: true,
    },
];

/// Extracts `archive` with this crate, into the same shape `tree` returns.
fn extract_with_crate(
    archive: &Path,
    password: &Password,
    lane: Lane,
) -> BTreeMap<String, Vec<u8>> {
    let file = std::fs::File::open(archive).expect("open archive");
    let mut reader = ArchiveReader::new(file, password.clone()).expect("read archive");
    reader.set_threads(lane.threads);
    reader.set_adaptive_lzma2(lane.adaptive);
    let mut out = BTreeMap::new();
    reader
        .for_each_entries(|entry, rd| {
            if entry.is_directory() {
                return Ok(true);
            }
            let mut bytes = Vec::new();
            rd.read_to_end(&mut bytes)?;
            out.insert(entry.name().replace('\\', "/"), bytes);
            Ok(true)
        })
        .expect("extract");
    out
}

/// Concatenates `name.001`, `name.002`, … into one buffer, the way a consumer
/// that streams a split set presents it.
fn join_volumes(dir: &Path, stem: &str) -> PathBuf {
    let mut joined = Vec::new();
    for index in 1.. {
        let part = dir.join(format!("{stem}.{index:03}"));
        if !part.exists() {
            assert!(index > 1, "no volumes found for {stem}");
            break;
        }
        joined.extend_from_slice(&std::fs::read(&part).expect("read volume"));
    }
    let path = dir.join(format!("{stem}-joined.7z"));
    std::fs::write(&path, joined).expect("write joined archive");
    path
}

/// Build with `7zz a`, extract with `7zz x` and with this crate, compare.
fn differential(case: &str, archive_args: &[&str], password: Option<&str>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let source = root.join("src");
    std::fs::create_dir_all(&source).expect("mkdir src");
    let names = write_members(&source);

    let mut args: Vec<String> = vec!["a".into(), "-bso0".into(), "-bsp0".into()];
    args.extend(archive_args.iter().map(|a| (*a).to_string()));
    if let Some(p) = password {
        args.push(format!("-p{p}"));
    }
    args.push("../archive.7z".into());
    for name in &names {
        args.push(name.clone());
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_7zz(&source, &borrowed);

    let mut archive = root.join("archive.7z");
    if archive_args.iter().any(|a| a.starts_with("-v")) {
        archive = join_volumes(root, "archive.7z");
    }

    // The oracle.
    let oracle_dir = root.join("oracle");
    let mut x: Vec<String> = vec![
        "x".into(),
        "-bso0".into(),
        "-bsp0".into(),
        "-y".into(),
        format!("-o{}", oracle_dir.display()),
    ];
    if let Some(p) = password {
        x.push(format!("-p{p}"));
    }
    x.push(archive.display().to_string());
    let borrowed: Vec<&str> = x.iter().map(String::as_str).collect();
    run_7zz(root, &borrowed);
    let oracle = tree(&oracle_dir);
    assert!(!oracle.is_empty(), "{case}: 7zz extracted nothing");

    let password = password.map_or_else(Password::empty, Password::from);
    for lane in LANES {
        let ours = extract_with_crate(&archive, &password, lane);
        let case = format!("{case} [{}]", lane.name);

        assert_eq!(
            ours.keys().collect::<Vec<_>>(),
            oracle.keys().collect::<Vec<_>>(),
            "{case}: member list differs"
        );
        for (name, expected) in &oracle {
            let got = &ours[name];
            assert_eq!(
                got.len(),
                expected.len(),
                "{case}: {name} length differs ({} vs {})",
                got.len(),
                expected.len()
            );
            assert!(got == expected, "{case}: {name} bytes differ");
        }
    }
}

macro_rules! differential_case {
    ($name:ident, $args:expr) => {
        differential_case!($name, $args, None);
    };
    ($name:ident, $args:expr, $password:expr) => {
        #[test]
        fn $name() {
            if !have_7zz() {
                eprintln!("skipping {}: 7zz is not on PATH", stringify!($name));
                return;
            }
            differential(stringify!($name), &$args, $password);
        }
    };
}

differential_case!(lzma_level1, ["-m0=lzma", "-mx1"]);
differential_case!(lzma_level5, ["-m0=lzma", "-mx5"]);
differential_case!(lzma_level9, ["-m0=lzma", "-mx9"]);
differential_case!(lzma2_single_threaded, ["-m0=lzma2", "-mmt=1", "-mx5"]);
differential_case!(lzma2_chunked, ["-m0=lzma2", "-mmt=on", "-mx5"]);
differential_case!(lzma2_level1, ["-m0=lzma2", "-mx1"]);
differential_case!(lzma2_level9_solid, ["-m0=lzma2", "-mx9", "-ms=on"]);
differential_case!(lzma2_level5_non_solid, ["-m0=lzma2", "-mx5", "-ms=off"]);
differential_case!(lzma2_bcj_x86, ["-m0=BCJ", "-m1=lzma2", "-mx5"]);
differential_case!(
    lzma2_bcj2,
    ["-m0=BCJ2", "-m1=lzma2", "-m2=lzma", "-m3=lzma"]
);
differential_case!(lzma2_delta, ["-m0=delta:4", "-m1=lzma2", "-mx5"]);
differential_case!(lzma_solid_bcj, ["-m0=BCJ", "-m1=lzma", "-mx9", "-ms=on"]);
differential_case!(multi_volume, ["-m0=lzma2", "-mx1", "-v256k"]);
#[cfg(feature = "aes256")]
differential_case!(
    aes_encrypted,
    ["-m0=lzma2", "-mx5"],
    Some("silver-horizon-passphrase")
);
#[cfg(feature = "aes256")]
differential_case!(
    aes_encrypted_header,
    ["-m0=lzma2", "-mx5", "-mhe=on"],
    Some("silver-horizon-passphrase")
);
#[cfg(feature = "aes256")]
differential_case!(
    aes_encrypted_solid_lzma,
    ["-m0=lzma", "-mx9", "-ms=on"],
    Some("silver-horizon-passphrase")
);

/// PPMd decodes through `ppmd-turbo`; these hold it to 7-Zip on archives
/// 7-Zip wrote, alone, behind the single-stream filters, as BCJ2's main
/// stream, solid and encrypted.
#[cfg(feature = "ppmd")]
mod ppmd {
    use std::io::{BufReader, Read};

    use super::*;

    differential_case!(ppmd_level5, ["-m0=PPMd", "-mx5"]);
    differential_case!(ppmd_level9_solid, ["-m0=PPMd", "-mx9", "-ms=on"]);
    differential_case!(ppmd_order2_small_model, ["-m0=PPMd:o=2:mem=1m", "-ms=off"]);
    differential_case!(ppmd_order32, ["-m0=PPMd:o=32:mem=64m"]);
    differential_case!(ppmd_bcj_x86, ["-m0=BCJ", "-m1=PPMd"]);
    differential_case!(ppmd_delta, ["-m0=delta:4", "-m1=PPMd"]);
    differential_case!(
        ppmd_bcj2_main_stream,
        ["-m0=BCJ2", "-m1=PPMd", "-m2=lzma", "-m3=lzma"]
    );
    #[cfg(feature = "aes256")]
    differential_case!(
        ppmd_aes_encrypted_header,
        ["-m0=PPMd", "-mhe=on"],
        Some("silver-horizon-passphrase")
    );

    /// The bench corpus's `ppmd.7z` (`bench/fixtures/<profile>/`, generated by
    /// `sevenz-turbo-bench fixtures`), extracted by this crate and by `7zz x`,
    /// compared byte for byte as they stream. Skips a profile that has not
    /// been generated on this host, and the whole test without `7zz`.
    #[test]
    fn bench_corpus_ppmd_matches_7zz() {
        if !have_7zz() {
            eprintln!("skipping bench_corpus_ppmd_matches_7zz: 7zz is not on PATH");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/fixtures");
        let mut decoded = 0;
        for profile in ["quick", "full"] {
            let archive = corpus.join(profile).join("ppmd.7z");
            if !archive.is_file() {
                eprintln!(
                    "skipping the {profile} corpus: {} is not generated",
                    archive.display()
                );
                continue;
            }
            let tmp = tempfile::tempdir().expect("tempdir");
            let oracle_dir = tmp.path().join("oracle");
            run_7zz(
                tmp.path(),
                &[
                    "x",
                    "-bso0",
                    "-bsp0",
                    "-y",
                    &format!("-o{}", oracle_dir.display()),
                    &archive.display().to_string(),
                ],
            );

            let file = std::fs::File::open(&archive).expect("open archive");
            let mut reader = ArchiveReader::new(file, Password::empty()).expect("read archive");
            let mut members = 0;
            let mut ours = vec![0u8; 1 << 20];
            let mut theirs = vec![0u8; 1 << 20];
            reader
                .for_each_entries(|entry, rd| {
                    if entry.is_directory() {
                        return Ok(true);
                    }
                    let name = entry.name().replace('\\', "/");
                    let mut expected =
                        BufReader::new(std::fs::File::open(oracle_dir.join(&name)).expect(&name));
                    let mut offset = 0u64;
                    loop {
                        let n = rd.read(&mut ours)?;
                        if n == 0 {
                            break;
                        }
                        expected.read_exact(&mut theirs[..n]).unwrap_or_else(|e| {
                            panic!("{profile}/{name}: longer than 7zz's at {offset}: {e}")
                        });
                        assert!(
                            ours[..n] == theirs[..n],
                            "{profile}/{name}: bytes differ in [{offset}, {})",
                            offset + n as u64
                        );
                        offset += n as u64;
                    }
                    assert_eq!(
                        expected.read(&mut theirs[..1]).expect("read oracle"),
                        0,
                        "{profile}/{name}: shorter than 7zz's ({offset} bytes)"
                    );
                    members += 1;
                    Ok(true)
                })
                .expect("extract");
            assert_eq!(members, tree_files(&oracle_dir), "{profile}: member count");
            decoded += 1;
        }
        if decoded == 0 {
            eprintln!("skipping bench_corpus_ppmd_matches_7zz: no bench corpus generated");
        }
    }

    /// Regular files under `root`, without reading them.
    fn tree_files(root: &Path) -> usize {
        std::fs::read_dir(root)
            .expect("read_dir")
            .map(|entry| {
                let path = entry.expect("dir entry").path();
                if path.is_dir() { tree_files(&path) } else { 1 }
            })
            .sum()
    }
}
