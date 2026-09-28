//! Native `wasmtime` driver for the encrypted-extraction CONFORMANCE test.
//!
//! This is the executable proof that a full AES-256 encrypted 7z extraction
//! runs correctly inside a wasm guest whose block cipher lives on the host:
//! the crate's `crypto-host` backend, the embedder hook it calls
//! (`sevenz_turbo::hooks`), and the example's own raw import
//! (`host::host_aes_cbc_decrypt`) that the hook forwards to. It DOUBLES as the
//! reference an embedding host must satisfy for that ABI — it implements
//! `host_aes_cbc_decrypt` exactly to contract (raw offsets into the guest's
//! linear memory, in-place AES-256-CBC, no padding, stateless per call) with a
//! RustCrypto reference.
//!
//! What is asserted:
//!
//! 1. The guest's extraction of a freshly generated encrypted archive is
//!    byte-identical, entry for entry, to the same archive decoded by the
//!    native decoder in this process. Nothing weaker: the guest prints the hex
//!    of every recovered byte.
//! 2. A guest that never installs a hook panics with the documented message
//!    rather than decoding wrongly or silently falling back in-guest.
//!
//! Flow:
//!   1. Write a fixture archive (AES-256 + LZMA2, invented file names) into
//!      `CARGO_TARGET_TMPDIR` with this crate's own encoder.
//!   2. Build `examples/wasm_host_extract_conformance.rs` for `wasm32-wasip1`
//!      with `--no-default-features --features aes256,crypto-host`, into a
//!      private target dir so the nested cargo does not fight the outer test's
//!      target lock.
//!   3. Instantiate with `wasmtime`, providing WASI preview1 (argv, stdio, and
//!      the fixture directory preopened read-only as `/fixture`) plus the one
//!      custom host import.
//!   4. Run `_start` and compare its stdout with the native extraction.
//!
//! Skipped automatically if the `wasm32-wasip1` target is not installed.

#![cfg(not(target_family = "wasm"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use wasmtime::{Caller, Engine, Extern, Linker, Module, Store};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

const AES_BLOCK: usize = 16;

/// AES ABI return codes (see the `embedding` module in the example).
const RC_OK: i64 = 0;
const RC_BAD_KEY_LEN: i64 = -1;
const RC_BAD_BUF_LEN: i64 = -2;
const RC_OOB: i64 = -3;

/// The password the fixture is written and read with. It is a test fixture,
/// not a secret.
const FIXTURE_PASSWORD: &str = "cormorant-bracket-9";

/// Reference AES-256-CBC decrypt in place with a FRESH context seeded by `iv`
/// (stateless per call, matching the host contract).
fn reference_cbc_decrypt(key: &[u8; 32], iv: &[u8; AES_BLOCK], data: &mut [u8]) {
    use aes::cipher::{BlockModeDecrypt, KeyIvInit};

    let mut decryptor = cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into());
    for block in data.chunks_exact_mut(AES_BLOCK) {
        let block: &mut [u8; AES_BLOCK] = block.try_into().expect("exact chunk");
        decryptor.decrypt_block(block.into());
    }
}

/// The reference host function, wired to the exact ABI the guest calls. Reads
/// `key`/`iv` and the block-aligned buffer from the guest's linear memory at
/// the passed offsets, decrypts in place, and writes the plaintext back.
/// Stateless per call. Returns the contract's status codes.
fn reference_host_aes_cbc_decrypt(
    mut caller: Caller<'_, WasiP1Ctx>,
    key_ptr: i64,
    key_len: i64,
    iv_ptr: i64,
    buf_ptr: i64,
    buf_len: i64,
) -> i64 {
    // Validate per the contract before touching memory. 7z is AES-256 only.
    if key_len != 32 {
        return RC_BAD_KEY_LEN;
    }
    if buf_len % (AES_BLOCK as i64) != 0 {
        return RC_BAD_BUF_LEN;
    }

    let memory = match caller.get_export("memory") {
        Some(Extern::Memory(memory)) => memory,
        _ => return RC_OOB, // no linear memory exported — cannot proceed
    };

    let (key_ptr, iv_ptr, buf_ptr, buf_len) = (
        key_ptr as u64 as usize,
        iv_ptr as u64 as usize,
        buf_ptr as u64 as usize,
        buf_len as u64 as usize,
    );

    let mut key = [0u8; 32];
    if memory.read(&caller, key_ptr, &mut key).is_err() {
        return RC_OOB;
    }
    let mut iv = [0u8; AES_BLOCK];
    if memory.read(&caller, iv_ptr, &mut iv).is_err() {
        return RC_OOB;
    }

    let mut buf = vec![0u8; buf_len];
    if memory.read(&caller, buf_ptr, &mut buf).is_err() {
        return RC_OOB;
    }
    reference_cbc_decrypt(&key, &iv, &mut buf);
    if memory.write(&mut caller, buf_ptr, &buf).is_err() {
        return RC_OOB;
    }

    RC_OK
}

/// Does this `rustc` actually have the `wasm32-wasip1` standard library?
///
/// `--print target-list` is NOT a usable probe: it lists every target rustc
/// can name, installed or not, so it answers `true` even for a toolchain that
/// cannot build a single wasm crate. The target libdir existing is the real
/// signal.
fn has_wasip1_std(rustc: &Path) -> bool {
    Command::new(rustc)
        .args(["--print", "target-libdir", "--target", "wasm32-wasip1"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .is_some_and(|out| Path::new(String::from_utf8_lossy(&out.stdout).trim()).is_dir())
}

/// The workspace root: where the nested cargo runs from, and where the guest
/// example lives. This crate sits two directories below it.
fn workspace_root() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();
    root
}

/// Resolve a `(cargo, rustc)` pair that can actually build `wasm32-wasip1`.
///
/// The ambient `cargo`/`rustc` are not necessarily the toolchain pinned by
/// `rust-toolchain.toml`. A Homebrew `rust` install, for instance, puts real
/// `cargo`/`rustc` binaries on PATH ahead of rustup's proxies; those carry no
/// wasm std, and they are a *different build* of the same version number, so
/// their artifacts cannot be mixed with rustup's (E0514). Candidates are tried
/// in order: an explicit `RUSTC` override, the `rustc` beside the outer
/// `CARGO`, whatever `rustup which rustc` resolves for this manifest (which
/// honours `rust-toolchain.toml`), then PATH. The first one with a real wasm
/// std wins, and the nested build is pinned to it.
fn wasm_toolchain() -> Option<(PathBuf, PathBuf)> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(rustc) = std::env::var_os("RUSTC") {
        candidates.push(PathBuf::from(rustc));
    }
    if let Some(dir) = std::env::var_os("CARGO")
        .map(PathBuf::from)
        .and_then(|cargo| cargo.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join("rustc"));
    }
    if let Some(out) = Command::new("rustup")
        .args(["which", "rustc"])
        .current_dir(workspace_root())
        .output()
        .ok()
        .filter(|out| out.status.success())
    {
        candidates.push(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()));
    }
    candidates.push(PathBuf::from("rustc"));

    let rustc = candidates.into_iter().find(|rustc| has_wasip1_std(rustc))?;
    let cargo = rustc
        .parent()
        .map(|dir| dir.join("cargo"))
        .filter(|cargo| cargo.is_file())
        .or_else(|| std::env::var_os("CARGO").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("cargo"));
    Some((cargo, rustc))
}

/// Build the conformance example for `wasm32-wasip1` with `crypto-host` and
/// return the path to the produced `.wasm`.
///
/// Built once per test binary: the tests below run in parallel and would
/// otherwise queue behind each other on cargo's package-cache lock, or race
/// each other's output file.
static CONFORMANCE_WASM: LazyLock<PathBuf> = LazyLock::new(build_conformance_wasm);

fn build_conformance_wasm() -> PathBuf {
    let manifest_dir = workspace_root();
    let target_dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wasm-host-conformance-target");
    let (cargo, rustc) =
        wasm_toolchain().expect("no toolchain with a wasm32-wasip1 std (checked by the caller)");

    let status = Command::new(&cargo)
        .current_dir(&manifest_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        // Pin the nested build to the resolved wasm-capable toolchain instead
        // of letting cargo pick whichever `rustc` is first on PATH.
        .env("RUSTC", &rustc)
        .env("RUSTDOC", rustc.with_file_name("rustdoc"))
        // Do not inherit the outer test's RUSTFLAGS/target selection.
        .env_remove("RUSTFLAGS")
        .args([
            "build",
            "--locked",
            "--release",
            "--example",
            "wasm_host_extract_conformance",
            "--no-default-features",
            "--features",
            "aes256,crypto-host",
            "--target",
            "wasm32-wasip1",
        ])
        .status()
        .expect("failed to spawn cargo to build the wasm conformance example");
    assert!(
        status.success(),
        "cargo build of the wasm conformance example failed"
    );

    let wasm = target_dir
        .join("wasm32-wasip1")
        .join("release")
        .join("examples")
        .join("wasm_host_extract_conformance.wasm");
    assert!(
        wasm.is_file(),
        "expected built wasm at {}, but it is missing",
        wasm.display()
    );
    wasm
}

/// Deterministic sample bytes — reproducible, and compressible enough that the
/// fixture stays small while still spanning many AES blocks.
fn sample(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|index| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // A small alphabet keeps LZMA2 busy and the archive small.
            b"abcdefghijklmnop"[((state >> 24) as usize ^ index) & 0xf]
        })
        .collect()
}

/// The fixture's entries: invented names, deterministic contents, sizes chosen
/// so the encrypted stream spans many blocks and does not end on a tidy
/// boundary.
fn fixture_entries() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("notes/alpha-index.txt", sample(64 * 1024 + 7, 0x51)),
        ("notes/beta-log.txt", sample(9_001, 0x52)),
        ("tiny.txt", sample(3, 0x53)),
    ]
}

/// Write the AES-256 encrypted fixture archive with this crate's own encoder,
/// and return the directory holding it (which the guest gets preopened).
fn write_fixture() -> tempfile::TempDir {
    use sevenz_turbo::encoder_options::{AesEncoderOptions, Lzma2Options};
    use sevenz_turbo::{ArchiveEntry, ArchiveWriter, Password};

    let dir = tempfile::tempdir().expect("create a private fixture directory");
    let path = dir.path().join("archive.7z");

    let file = std::fs::File::create(&path).expect("create the fixture archive");
    let mut writer = ArchiveWriter::new(file).expect("open an archive writer");
    writer.set_content_methods(vec![
        AesEncoderOptions::new(Password::from(FIXTURE_PASSWORD)).into(),
        Lzma2Options::default().into(),
    ]);
    for (name, content) in fixture_entries() {
        writer
            .push_archive_entry(ArchiveEntry::new_file(name), Some(content.as_slice()))
            .expect("write an entry");
    }
    writer.finish().expect("finish the archive");

    dir
}

/// Extract the fixture with the NATIVE decoder, in the guest's output format:
/// one `name\thex(contents)` line per entry.
fn native_extraction(archive: &Path) -> Vec<String> {
    use sevenz_turbo::{ArchiveReader, Password};

    let file = std::fs::File::open(archive).expect("open the fixture archive");
    let mut reader =
        ArchiveReader::new(file, Password::from(FIXTURE_PASSWORD)).expect("read the header");
    let mut lines = Vec::new();
    reader
        .for_each_entries(|entry, entry_reader| {
            let mut contents = Vec::new();
            entry_reader.read_to_end(&mut contents)?;
            let hex: String = contents.iter().map(|byte| format!("{byte:02x}")).collect();
            lines.push(format!("{}\t{hex}", entry.name));
            Ok(true)
        })
        .expect("native extraction");
    lines
}

/// What a guest run produced.
struct GuestRun {
    stdout: String,
    stderr: String,
    outcome: Result<(), String>,
}

/// Run the wasm guest over the preopened `fixture_dir`, with the reference host
/// AES import, capturing stdout and stderr.
fn run_guest(wasm: &Path, fixture_dir: &Path, extra_arg: Option<&str>) -> GuestRun {
    let engine = Engine::default();
    let module = Module::from_file(&engine, wasm).expect("load wasm module");

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |ctx: &mut WasiP1Ctx| ctx)
        .expect("add wasi preview1 to linker");
    // The one custom import, in the fixed namespace, satisfying the example's
    // raw `#[link(wasm_import_module = "host")]` extern.
    linker
        .func_wrap(
            "host",
            "host_aes_cbc_decrypt",
            reference_host_aes_cbc_decrypt,
        )
        .expect("define host host_aes_cbc_decrypt");

    let stdout = MemoryOutputPipe::new(16 * 1024 * 1024);
    let stderr = MemoryOutputPipe::new(1024 * 1024);
    let mut builder = WasiCtxBuilder::new();
    builder
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .arg("wasm_host_extract_conformance")
        .arg("/fixture/archive.7z")
        .arg(FIXTURE_PASSWORD)
        .preopened_dir(fixture_dir, "/fixture", FsPerms::ReadOnly)
        .expect("preopen the fixture directory");
    if let Some(arg) = extra_arg {
        builder.arg(arg);
    }
    let mut store = Store::new(&engine, builder.build_p1());

    let instance = linker
        .instantiate(&mut store, &module)
        .expect("instantiate wasm module (all imports, incl. the host fn, must be satisfied)");
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .expect("wasip1 command must export _start");

    // A wasip1 command signals success by returning cleanly OR by
    // `proc_exit(0)`, which surfaces here as an `I32Exit(0)` error.
    let outcome = match start.call(&mut store, ()) {
        Ok(()) => Ok(()),
        Err(err) => match err.downcast_ref::<wasmtime_wasi::I32Exit>() {
            Some(exit) if exit.0 == 0 => Ok(()),
            Some(exit) => Err(format!("guest exited with code {}", exit.0)),
            None => Err(format!("guest trapped: {err:?}")),
        },
    };

    drop(store);
    GuestRun {
        stdout: String::from_utf8_lossy(&stdout.contents()).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.contents()).into_owned(),
        outcome,
    }
}

/// The conformance assertion: everything the guest recovered through the host
/// AES equals, byte for byte, what the native decoder recovers.
#[test]
fn wasm_guest_extraction_matches_the_native_decoder() {
    if wasm_toolchain().is_none() {
        eprintln!("skipping: wasm32-wasip1 target not installed");
        return;
    }

    let fixture = write_fixture();
    let fixture_dir = fixture.path();
    let expected = native_extraction(&fixture_dir.join("archive.7z"));
    assert_eq!(
        expected.len(),
        fixture_entries().len(),
        "the native decoder must see every entry the fixture was written with"
    );

    let run = run_guest(&CONFORMANCE_WASM, fixture_dir, None);
    run.outcome.unwrap_or_else(|why| {
        panic!("{why}\n--- guest stderr ---\n{}", run.stderr);
    });

    let got: Vec<String> = run.stdout.lines().map(str::to_owned).collect();
    assert_eq!(
        got.len(),
        expected.len(),
        "guest printed {} entries, native decoder produced {}\n--- guest stderr ---\n{}",
        got.len(),
        expected.len(),
        run.stderr
    );
    for (guest, native) in got.iter().zip(expected.iter()) {
        let name = guest.split('\t').next().unwrap_or("<unnamed>");
        assert_eq!(
            guest, native,
            "guest extraction of {name} differs from the native decoder's"
        );
    }
}

/// A guest that never installs a hook must stop with the documented message.
/// Silently decoding wrongly, or falling back to an in-guest cipher, would
/// defeat the whole point of delegation — so the absence of wiring is a panic
/// an embedder cannot miss.
#[test]
fn a_guest_without_hooks_panics_with_the_documented_message() {
    if wasm_toolchain().is_none() {
        eprintln!("skipping: wasm32-wasip1 target not installed");
        return;
    }

    let fixture = write_fixture();
    let run = run_guest(
        &CONFORMANCE_WASM,
        fixture.path(),
        Some("--skip-hook-install"),
    );

    assert!(
        run.outcome.is_err(),
        "a guest with no hooks installed must not complete an extraction\n\
         --- guest stdout ---\n{}",
        run.stdout
    );
    assert!(
        run.stderr.contains("no host crypto hooks installed"),
        "the panic must name the missing wiring; stderr was:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("install_host_crypto_hooks"),
        "the panic must name the call that fixes it; stderr was:\n{}",
        run.stderr
    );
}

/// Unit-level check of the reference host function's cipher, so a regression in
/// the reference (which an embedding host mirrors) is caught even when the wasm
/// round-trip is skipped.
#[test]
fn the_reference_host_cipher_round_trips() {
    use aes::cipher::{BlockModeEncrypt, KeyIvInit};

    assert_eq!(
        (RC_OK, RC_BAD_KEY_LEN, RC_BAD_BUF_LEN, RC_OOB),
        (0, -1, -2, -3)
    );

    let key = [0x24u8; 32];
    let iv = [0x42u8; AES_BLOCK];
    let plaintext = [0x7fu8; 3 * AES_BLOCK];

    let mut ciphertext = plaintext.to_vec();
    let mut encryptor = cbc::Encryptor::<aes::Aes256>::new((&key).into(), (&iv).into());
    for block in ciphertext.chunks_exact_mut(AES_BLOCK) {
        let block: &mut [u8; AES_BLOCK] = block.try_into().expect("exact chunk");
        encryptor.encrypt_block(block.into());
    }

    reference_cbc_decrypt(&key, &iv, &mut ciphertext);
    assert_eq!(
        ciphertext, plaintext,
        "reference AES-256-CBC must round-trip"
    );
}
