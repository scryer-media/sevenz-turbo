//! WASM conformance guest for the host-delegated backends (`crypto-host` and
//! `crc-host`).
//!
//! Built for `wasm32-wasip1` with `--no-default-features --features
//! aes256,crc-host,crypto-host`, this extracts an AES-256 encrypted 7z archive
//! with its bulk cryptography and checksums leaving the guest: every block
//! cipher call goes to the `aes_cbc_decrypt` hook installed below, the key
//! derivation's SHA-256 to the `sha256_*` hooks, and every CRC-32 the archive
//! carries - start header, next header, members - to the `crc32` hook (see
//! [`sevenz_turbo::hooks`]). The LZMA2 decode and the container parsing run
//! in the guest.
//!
//! This example is also the reference embedding of those seams for a **core**
//! wasm module: it declares its raw imports in a `host` namespace and installs
//! hooks that forward to them. The imports take guest pointers because a core
//! module's linear memory is addressable by the host, so the host works in
//! place with no marshalling - that ABI belongs to this example, not to
//! `sevenz-turbo`, which only ever sees the hooks.
//!
//! It prints one `name<TAB>hex(contents)` line per archive entry, so the
//! native harness (`tools/wasm-conformance/tests/wasm_host_extract_conformance.rs`) can compare the
//! guest's extraction byte-for-byte against the same archive decoded by the
//! native decoder. With `--skip-aes-hook` or `--skip-hash-hooks` it
//! deliberately does NOT install that set of hooks, which is how the harness
//! proves that a guest missing its wiring panics with the documented message
//! instead of silently decoding wrongly.
//!
//! Arguments (supplied by the harness):
//!   argv[1] = the archive path inside the guest (e.g. `/fixture/archive.7z`)
//!   argv[2] = the password
//!   argv[3..] = `--skip-aes-hook` and/or `--skip-hash-hooks`, optionally
//!
//! Build & run (from the repository root):
//!   cargo build --release --example wasm_host_extract_conformance \
//!     --no-default-features --features aes256,crc-host,crypto-host \
//!     --target wasm32-wasip1
//!   # then run under the harness, which provides the host functions:
//!   cargo test -p wasm-conformance
//!
//! Running the raw module under a plain `wasmtime` CLI traps at instantiation
//! because the `host` imports are unsatisfied - that is expected; the module
//! is only meaningful with a host that provides them.

use std::fs::File;

use sevenz_turbo::{ArchiveReader, Password};

/// The example's own raw imports and the hooks that forward to them.
///
/// ABI (fixed contract, shared with the harness):
///
/// ```text
/// host_aes_cbc_decrypt(key_ptr, key_len, iv_ptr, buf_ptr, buf_len) -> i64
/// host_crc32(seed: i32, ptr: i32, len: i32) -> i32
/// host_crc64_xz(seed: i64, ptr: i32, len: i32) -> i64
/// host_sha256_init() -> i64
/// host_sha256_clone(handle: i64) -> i64
/// host_sha256_update(handle: i64, ptr: i32, len: i32)
/// host_sha256_finalize(handle: i64, out_ptr: i32)
/// host_sha256_drop(handle: i64)
/// ```
///
/// Every `*_ptr` is a byte offset into this module's linear memory, which the
/// host slices in place.
///
/// `host_aes_cbc_decrypt` is AES-256-CBC, no padding, decrypt IN PLACE.
/// `key_len` is 32; `iv` is 16 bytes at `iv_ptr`; `buf_len` is a multiple of
/// 16 and may be 0. The host is stateless per call - `sevenz-turbo` threads
/// the CBC IV across chunks itself. Returns `0` ok, `-1` bad `key_len`, `-2`
/// `buf_len % 16 != 0`, `-3` out-of-bounds.
///
/// The CRC and SHA-256 imports are the shape `lzma-turbo`'s own reference
/// embedding uses: the CRCs resume from a seed in the finalized domain, and
/// `host_sha256_finalize` writes exactly 32 bytes at `out_ptr`. A bad offset
/// or a stale handle is a contract violation and the host traps. `sevenz-turbo`
/// never calls `host_crc64_xz` (7z has no CRC-64), but the hook set needs one.
#[cfg(target_arch = "wasm32")]
mod embedding {
    use sevenz_turbo::hooks::{
        HostAesError, HostCryptoHooks, HostHashHooks, HostSha256Handle, install_host_crypto_hooks,
        install_host_hash_hooks,
    };

    #[link(wasm_import_module = "host")]
    unsafe extern "C" {
        fn host_aes_cbc_decrypt(
            key_ptr: u64,
            key_len: u64,
            iv_ptr: u64,
            buf_ptr: u64,
            buf_len: u64,
        ) -> i64;
        fn host_crc32(seed: i32, ptr: i32, len: i32) -> i32;
        fn host_crc64_xz(seed: i64, ptr: i32, len: i32) -> i64;
        fn host_sha256_init() -> i64;
        fn host_sha256_clone(handle: i64) -> i64;
        fn host_sha256_update(handle: i64, ptr: i32, len: i32);
        fn host_sha256_finalize(handle: i64, out_ptr: i32);
        fn host_sha256_drop(handle: i64);
    }

    /// Forward the hook to the raw import: copy `data` into a fresh buffer, let
    /// the host decrypt that buffer in place, and hand it back.
    fn aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError> {
        if iv.len() != 16 {
            return Err(HostAesError::BadIvLength);
        }
        let mut out = data.to_vec();
        // SAFETY: all pointers are valid offsets into this module's own linear
        // memory for the stated lengths; the host slices them in place and
        // never retains them past the call. `key`/`iv` are read-only to the
        // host; `out` is written in place and is uniquely owned here.
        let rc = unsafe {
            host_aes_cbc_decrypt(
                key.as_ptr() as u64,
                key.len() as u64,
                iv.as_ptr() as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
            )
        };
        match rc {
            0 => Ok(out),
            -1 => Err(HostAesError::BadKeyLength),
            -2 => Err(HostAesError::BadBlockLength),
            other => panic!("host_aes_cbc_decrypt returned {other} (contract violation)"),
        }
    }

    fn crc32(seed: u32, data: &[u8]) -> u32 {
        // SAFETY: a read-only offset+length into this module's own linear
        // memory, borrowed by the host only for the duration of the call.
        unsafe { host_crc32(seed as i32, data.as_ptr() as i32, data.len() as i32) as u32 }
    }

    fn crc64_xz(seed: u64, data: &[u8]) -> u64 {
        // SAFETY: as in `crc32`.
        unsafe { host_crc64_xz(seed as i64, data.as_ptr() as i32, data.len() as i32) as u64 }
    }

    fn sha256_init() -> HostSha256Handle {
        // SAFETY: no memory is shared; the host allocates its own state.
        HostSha256Handle(unsafe { host_sha256_init() } as u64)
    }

    fn sha256_clone(handle: HostSha256Handle) -> HostSha256Handle {
        // SAFETY: `handle` is live, by the hook contract.
        HostSha256Handle(unsafe { host_sha256_clone(handle.0 as i64) } as u64)
    }

    fn sha256_update(handle: HostSha256Handle, data: &[u8]) {
        // SAFETY: a live handle, plus a read-only slice of this module's own
        // memory borrowed only for the call.
        unsafe { host_sha256_update(handle.0 as i64, data.as_ptr() as i32, data.len() as i32) };
    }

    fn sha256_finalize(handle: HostSha256Handle) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: a live handle, and `out` is 32 writable bytes in this
        // module's memory, which is exactly what the host writes.
        unsafe { host_sha256_finalize(handle.0 as i64, out.as_mut_ptr() as i32) };
        out
    }

    fn sha256_drop(handle: HostSha256Handle) {
        // SAFETY: a live handle, consumed here and never passed again.
        unsafe { host_sha256_drop(handle.0 as i64) };
    }

    pub(super) fn install_aes() {
        install_host_crypto_hooks(HostCryptoHooks { aes_cbc_decrypt });
    }

    pub(super) fn install_hash() {
        install_host_hash_hooks(HostHashHooks::new(
            crc32,
            crc64_xz,
            sha256_init,
            sha256_clone,
            sha256_update,
            sha256_finalize,
            sha256_drop,
        ));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let archive = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!(
            "usage: wasm_host_extract_conformance <archive> <password> \
             [--skip-aes-hook] [--skip-hash-hooks]"
        );
        std::process::exit(2);
    });
    let password = args.get(2).cloned().unwrap_or_default();
    let skip_aes = args.iter().any(|arg| arg == "--skip-aes-hook");
    let skip_hash = args.iter().any(|arg| arg == "--skip-hash-hooks");

    #[cfg(target_arch = "wasm32")]
    {
        if !skip_aes {
            embedding::install_aes();
            assert!(
                sevenz_turbo::hooks::host_crypto_hooks_installed(),
                "the AES hook must be visible to the crate right after installation"
            );
        }
        if !skip_hash {
            embedding::install_hash();
            assert!(
                sevenz_turbo::hooks::host_hash_hooks_installed(),
                "the hash hooks must be visible to the crate right after installation"
            );
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (skip_aes, skip_hash);

    let file = File::open(&archive)
        .unwrap_or_else(|error| panic!("cannot open the fixture at {archive}: {error}"));
    let mut reader = ArchiveReader::new(file, Password::from(password.as_str()))
        .unwrap_or_else(|error| panic!("cannot read the archive header: {error}"));

    let mut lines = Vec::new();
    reader
        .for_each_entries(|entry, entry_reader| {
            let mut contents = Vec::new();
            entry_reader.read_to_end(&mut contents)?;
            let hex: String = contents.iter().map(|byte| format!("{byte:02x}")).collect();
            lines.push(format!("{}\t{hex}", entry.name));
            Ok(true)
        })
        .unwrap_or_else(|error| panic!("extraction failed: {error}"));

    for line in &lines {
        println!("{line}");
    }
}
