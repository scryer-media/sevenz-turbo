//! Records the versions of the crates this binary is linked against, read from
//! the workspace `Cargo.lock` it was built from, so `decode-bench op version`
//! can report which `lzma-turbo` a measured binary actually carried rather
//! than which one somebody expected it to.

use std::path::Path;

fn main() {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = std::fs::read_to_string(&lock).unwrap_or_default();
    for (name, env) in [
        ("lzma-turbo", "DECODE_BENCH_LZMA_TURBO_VERSION"),
        ("sevenz-rust2", "DECODE_BENCH_SEVENZ_RUST2_VERSION"),
        ("aws-lc-rs", "DECODE_BENCH_AWS_LC_RS_VERSION"),
        ("crc-fast", "DECODE_BENCH_CRC_FAST_VERSION"),
        ("ppmd-rust", "DECODE_BENCH_PPMD_RUST_VERSION"),
    ] {
        println!(
            "cargo:rustc-env={env}={}",
            locked_version(&text, name).unwrap_or_else(|| "unknown".to_string())
        );
    }
}

/// The version of the first `[[package]]` named `name`. A lock that carries
/// two versions of one crate lists both; the first is reported, and the
/// harness records the lock's digest beside it.
fn locked_version(lock: &str, name: &str) -> Option<String> {
    let wanted = format!("name = \"{name}\"");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() == wanted {
            let version = lines.next()?.trim();
            let version = version.strip_prefix("version = \"")?.strip_suffix('"')?;
            return Some(version.to_string());
        }
    }
    None
}
