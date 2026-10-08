//! Records the versions of the crates this binary is linked against, read from
//! the workspace `Cargo.lock` it was built from, so `decode-bench op version`
//! can report which `lzma-turbo` a measured binary actually carried rather
//! than which one somebody expected it to. It also records the commit the
//! binary was built from, and whether the sources compiled into it had
//! uncommitted changes then, so the harness can tell whether the checkout it
//! runs next to is the one the binary came from (the lock's digest is taken at
//! run time, over the lock the binary embeds).

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = manifest_dir.join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    println!(
        "cargo:rustc-env=DECODE_BENCH_GIT_COMMIT={}",
        git_commit(manifest_dir)
    );
    println!(
        "cargo:rustc-env=DECODE_BENCH_GIT_DIRTY={}",
        git_dirty(manifest_dir)
    );
    // The Cargo profile the binary is built under ("release" or "debug"), so
    // the harness can refuse to time an unoptimised candidate. Cargo sets
    // PROFILE for build scripts; a custom profile reports the one it inherits.
    println!(
        "cargo:rustc-env=DECODE_BENCH_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string())
    );
    let text = std::fs::read_to_string(&lock).unwrap_or_default();
    for (name, env) in [
        ("sevenz-turbo", "DECODE_BENCH_SEVENZ_TURBO_VERSION"),
        ("lzma-turbo", "DECODE_BENCH_LZMA_TURBO_VERSION"),
        ("sevenz-rust2", "DECODE_BENCH_SEVENZ_RUST2_VERSION"),
        ("aws-lc-rs", "DECODE_BENCH_AWS_LC_RS_VERSION"),
        ("crc-fast", "DECODE_BENCH_CRC_FAST_VERSION"),
    ] {
        println!(
            "cargo:rustc-env={env}={}",
            locked_version(&text, name).unwrap_or_else(|| "unknown".to_string())
        );
    }
}

/// The commit `HEAD` names, or `unknown` outside a git checkout. A build from
/// a source tree without git sets `DECODE_BENCH_GIT_COMMIT` itself. The build
/// script reruns when `HEAD` moves: a commit or checkout in this worktree
/// rewrites its `HEAD` reflog.
fn git_commit(dir: &Path) -> String {
    println!("cargo:rerun-if-env-changed=DECODE_BENCH_GIT_COMMIT");
    if let Ok(commit) = std::env::var("DECODE_BENCH_GIT_COMMIT")
        && !commit.is_empty()
    {
        return commit;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|text| !text.is_empty())
    };
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let git_dir = Path::new(&git_dir);
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("logs/HEAD").display()
        );
    }
    git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
}

/// The sources compiled into this binary, relative to the workspace root:
/// the crate and this tool. An edit to any of them reruns the build script, so
/// the dirty state below is the one the binary was compiled from.
const BUILD_PATHS: [&str; 4] = ["src", "Cargo.toml", "Cargo.lock", "tools/decode-bench"];

/// `true` when [`BUILD_PATHS`] differ from `HEAD` (staged or not), `false`
/// when they match it, `unknown` outside a git checkout. A build from a source
/// tree without git sets `DECODE_BENCH_GIT_DIRTY` itself.
fn git_dirty(dir: &Path) -> String {
    println!("cargo:rerun-if-env-changed=DECODE_BENCH_GIT_DIRTY");
    if let Ok(dirty) = std::env::var("DECODE_BENCH_GIT_DIRTY")
        && !dirty.is_empty()
    {
        return dirty;
    }
    let root = dir.join("../..");
    for path in BUILD_PATHS {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no", "--"])
        .args(BUILD_PATHS)
        .current_dir(&root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |output| (!output.stdout.is_empty()).to_string(),
        )
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
