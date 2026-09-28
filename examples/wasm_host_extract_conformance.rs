//! WASM conformance guest for the host-delegated AES backend (`crypto-host`).
//!
//! Built for `wasm32-wasip1` with `--no-default-features --features
//! aes256,crypto-host`, this extracts an AES-256 encrypted 7z archive with
//! every block cipher call leaving the guest: `crate::crypto_backend` selects
//! the host-delegated lane, which calls the `aes_cbc_decrypt` hook installed
//! below (see [`sevenz_turbo::hooks`]). Everything else — the SHA-256 key
//! derivation, the LZMA2 decode, the CRC checks — runs in the guest.
//!
//! This example is also the reference embedding of that seam for a **core**
//! wasm module: it declares one raw import, `host_aes_cbc_decrypt`, in a
//! `host` namespace and installs a hook that forwards to it. The import takes
//! guest pointers because a core module's linear memory is addressable by the
//! host, so the host decrypts in place with no marshalling — that ABI belongs
//! to this example, not to `sevenz-turbo`, which only ever sees the hook.
//!
//! It prints one `name<TAB>hex(contents)` line per archive entry, so the
//! native harness (`tools/wasm-conformance/tests/wasm_host_extract_conformance.rs`) can compare the
//! guest's extraction byte-for-byte against the same archive decoded by the
//! native decoder. With `--skip-hook-install` it deliberately does NOT install
//! the hook, which is how the harness proves that a guest missing its wiring
//! panics with the documented message instead of silently decoding wrongly.
//!
//! Arguments (supplied by the harness):
//!   argv[1] = the archive path inside the guest (e.g. `/fixture/archive.7z`)
//!   argv[2] = the password
//!   argv[3] = `--skip-hook-install`, optionally
//!
//! Build & run (from the repository root):
//!   cargo build --release --example wasm_host_extract_conformance \
//!     --no-default-features --features aes256,crypto-host \
//!     --target wasm32-wasip1
//!   # then run under the harness, which provides the host function:
//!   cargo test -p wasm-conformance
//!
//! Running the raw module under a plain `wasmtime` CLI traps at instantiation
//! because the `host_aes_cbc_decrypt` import is unsatisfied — that is
//! expected; the module is only meaningful with a host that provides it.

use std::fs::File;

use sevenz_turbo::{ArchiveReader, Password};

/// The example's own raw import and the hook that forwards to it.
///
/// ABI (fixed contract, shared with the harness):
///
/// ```text
/// host_aes_cbc_decrypt(key_ptr, key_len, iv_ptr, buf_ptr, buf_len) -> i64
/// ```
///
/// Every `*_ptr` is a byte offset into this module's linear memory, which the
/// host slices in place. AES-256-CBC, no padding, decrypt IN PLACE. `key_len`
/// is 32; `iv` is 16 bytes at `iv_ptr`; `buf_len` is a multiple of 16 and may
/// be 0. The host is stateless per call — `sevenz-turbo` threads the CBC IV
/// across chunks itself. Returns `0` ok, `-1` bad `key_len`, `-2`
/// `buf_len % 16 != 0`, `-3` out-of-bounds.
#[cfg(target_arch = "wasm32")]
mod embedding {
    use sevenz_turbo::hooks::{HostAesError, HostCryptoHooks, install_host_crypto_hooks};

    #[link(wasm_import_module = "host")]
    unsafe extern "C" {
        fn host_aes_cbc_decrypt(
            key_ptr: u64,
            key_len: u64,
            iv_ptr: u64,
            buf_ptr: u64,
            buf_len: u64,
        ) -> i64;
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

    pub(super) fn install() {
        install_host_crypto_hooks(HostCryptoHooks { aes_cbc_decrypt });
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let archive = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!(
            "usage: wasm_host_extract_conformance <archive> <password> [--skip-hook-install]"
        );
        std::process::exit(2);
    });
    let password = args.get(2).cloned().unwrap_or_default();
    let skip_install = args.iter().any(|arg| arg == "--skip-hook-install");

    #[cfg(target_arch = "wasm32")]
    if !skip_install {
        embedding::install();
        assert!(
            sevenz_turbo::hooks::host_crypto_hooks_installed(),
            "the hook must be visible to the crate right after installation"
        );
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = skip_install;

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
