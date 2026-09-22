//! Embedder-supplied delegation hook for the bulk AES-256-CBC decrypt.
//!
//! With `crypto-host` enabled, a wasm guest build of this crate does not run
//! the 7z `aes256` bulk decrypt itself: it calls a plain Rust function pointer
//! that the embedding program installs at start-up. Whatever sits behind the
//! pointer — a raw wasm import in some namespace, a `wit-bindgen`-generated
//! component import, a host SDK call — is the embedder's business.
//! `sevenz-turbo` takes no dependency on any runtime, SDK, or interface
//! definition; it only calls the `fn` pointer it was handed.
//!
//! This module exists whenever `crypto-host` is enabled. On native targets the
//! feature is accepted but the in-process backend (AWS-LC or RustCrypto) stays
//! active, so an installed hook is never called there.
//!
//! ## The seam
//!
//! ```ignore
//! use sevenz_turbo::hooks::{HostAesError, HostCryptoHooks, install_host_crypto_hooks};
//!
//! fn aes(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError> {
//!     // forward to the embedder's AES-256-CBC decrypt
//! }
//!
//! install_host_crypto_hooks(HostCryptoHooks { aes_cbc_decrypt: aes });
//! ```
//!
//! `examples/wasm_host_extract_conformance.rs` is a complete reference
//! embedding: a `wasm32-wasip1` guest that declares one raw import in a `host`
//! namespace and installs a hook that forwards to it, driven by the native
//! `wasmtime` harness in `tools/wasm-conformance`.
//!
//! ## Contract the hook must satisfy
//!
//! `aes_cbc_decrypt(key, iv, data)` returns the AES-CBC decryption of `data`
//! under `key`/`iv`, **no padding**, as a fresh buffer of exactly `data.len()`
//! bytes. `key` is 32 bytes (7z encrypts with AES-256 only), `iv` is exactly
//! 16, `data` is a whole number of 16-byte blocks and may be empty. The hook
//! is STATELESS per call: this crate threads the CBC IV across chunks itself
//! (see `crate::crypto_backend::HostAes256Cbc`).
//!
//! A hook that reports an error, returns the wrong length, or is missing
//! entirely is an embedder contract violation and panics: a guest that reaches
//! the bulk decrypt without a working host has no recoverable state, and a
//! silent in-guest fallback would quietly defeat the whole point of
//! delegation.
//!
//! Only the *decrypt* is delegated. The encoder (`compress` + `aes256`) keeps
//! RustCrypto's `cbc::Encryptor` in `encryption::aes`; writing archives is not
//! the path this seam exists for. SHA-256 (the 7z key derivation) and CRC-32
//! come from `lzma-turbo` and are not routed through this module.

use std::sync::RwLock;

/// Why the embedder's AES-CBC hook refused a call.
///
/// These mirror the length/alignment rejections a host may report; each one is
/// a contract violation on this crate's side, because the caller below only
/// ever passes a 32-byte key, a 16-byte IV, and a block-aligned buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAesError {
    /// `key` was not 32 bytes.
    BadKeyLength,
    /// `data` was not a whole number of 16-byte AES blocks.
    BadBlockLength,
    /// `iv` was not exactly 16 bytes.
    BadIvLength,
}

impl std::fmt::Display for HostAesError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::BadKeyLength => "host rejected the AES key length",
            Self::BadBlockLength => "host rejected the AES block alignment",
            Self::BadIvLength => "host rejected the AES IV length",
        })
    }
}

impl std::error::Error for HostAesError {}

/// Decrypt `data` (block-aligned, may be empty) under `key`/`iv`, returning a
/// buffer of the same length. Stateless per call.
pub type AesCbcDecryptHook =
    fn(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError>;

/// The embedder-supplied delegation hooks.
///
/// A plain `fn` pointer rather than a trait object or a closure: it carries no
/// state, is `Copy`, and can therefore be read on the hot path without
/// allocation. Any state the hook needs belongs to the embedding program.
///
/// It is a struct with one field rather than a bare pointer so that a later
/// delegated primitive is an added field, not a changed signature.
#[derive(Clone, Copy)]
pub struct HostCryptoHooks {
    /// Bulk AES-256-CBC decrypt (consumed when `crypto-host` is enabled on a
    /// `wasm32` target).
    pub aes_cbc_decrypt: AesCbcDecryptHook,
}

impl std::fmt::Debug for HostCryptoHooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HostCryptoHooks { .. }")
    }
}

/// Process-wide hook registry.
///
/// A wasm guest is single-threaded and short-lived, so this is written exactly
/// once per instantiation in the shipping configuration. The lock keeps the
/// seam sound in native builds (where this crate's own tests are
/// multi-threaded) without any `unsafe`; a read guard per bulk chunk is
/// negligible next to the AES work the chunk represents.
static HOOKS: RwLock<Option<HostCryptoHooks>> = RwLock::new(None);

/// Install (or replace) the embedder's crypto hooks.
///
/// Call this before opening an encrypted archive — in practice, once at the
/// top of the guest's entry point.
pub fn install_host_crypto_hooks(hooks: HostCryptoHooks) {
    let mut slot = HOOKS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(hooks);
}

/// Whether hooks have been installed. Embedders can assert this in their own
/// start-up tests rather than discovering the gap inside a decrypt.
pub fn host_crypto_hooks_installed() -> bool {
    HOOKS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some()
}

/// Remove any installed hooks. Intended for embedder tests that need to prove
/// their wiring is what makes delegation work.
pub fn clear_host_crypto_hooks() {
    let mut slot = HOOKS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = None;
}

/// The installed hooks, or a panic naming the missing wiring.
///
/// Only a wasm guest build calls this outside `#[cfg(test)]`: on native targets
/// the in-process backends stay active (see `crate::crypto_backend`), so the
/// delegating seam there is exercised by tests alone.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn hooks() -> HostCryptoHooks {
    HOOKS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .expect(
            "sevenz-turbo: no host crypto hooks installed; the embedding program must call \
             sevenz_turbo::hooks::install_host_crypto_hooks before reading an encrypted archive",
        )
}

#[cfg(all(test, feature = "native-crypto"))]
pub(crate) mod test_reference {
    use super::{HostAesError, HostCryptoHooks, install_host_crypto_hooks};

    /// A reference hook backed by RustCrypto's `aes`/`cbc`.
    ///
    /// It is what a correct host does, so the backend tests can drive the real
    /// delegation path — registry lookup, buffer round trip, IV threading — on
    /// a native target with no wasm runtime in sight. Every test installs this
    /// same hook, which keeps a parallel `cargo test` deterministic.
    pub(crate) fn aes_cbc_decrypt(
        key: &[u8],
        iv: &[u8],
        data: &[u8],
    ) -> Result<Vec<u8>, HostAesError> {
        use aes::cipher::{BlockModeDecrypt, KeyIvInit};

        let key: &[u8; 32] = key.try_into().map_err(|_| HostAesError::BadKeyLength)?;
        let iv: &[u8; 16] = iv.try_into().map_err(|_| HostAesError::BadIvLength)?;
        if !data.len().is_multiple_of(16) {
            return Err(HostAesError::BadBlockLength);
        }

        let mut out = data.to_vec();
        let mut decryptor = cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into());
        for block in out.chunks_exact_mut(16) {
            let block: &mut [u8; 16] = block.try_into().expect("exact chunk");
            decryptor.decrypt_block(block.into());
        }
        Ok(out)
    }

    /// Install the reference hook for the duration of a test.
    pub(crate) fn install() {
        install_host_crypto_hooks(HostCryptoHooks { aes_cbc_decrypt });
    }
}

#[cfg(all(test, feature = "native-crypto"))]
mod tests {
    use super::*;

    /// The registry hands back exactly what was installed, and reports its own
    /// state honestly — the two facts an embedder's wiring test depends on.
    #[test]
    fn installed_hooks_are_visible_and_dispatch() {
        test_reference::install();
        assert!(host_crypto_hooks_installed());

        // NIST SP 800-38A, F.2.6 (CBC-AES256.Decrypt), first block.
        let key = [
            0x60, 0x3d, 0xeb, 0x10, 0x15, 0xca, 0x71, 0xbe, 0x2b, 0x73, 0xae, 0xf0, 0x85, 0x7d,
            0x77, 0x81, 0x1f, 0x35, 0x2c, 0x07, 0x3b, 0x61, 0x08, 0xd7, 0x2d, 0x98, 0x10, 0xa3,
            0x09, 0x14, 0xdf, 0xf4,
        ];
        let iv = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let ciphertext = [
            0xf5, 0x8c, 0x4c, 0x04, 0xd6, 0xe5, 0xf1, 0xba, 0x77, 0x9e, 0xab, 0xfb, 0x5f, 0x7b,
            0xfb, 0xd6,
        ];
        let plaintext = (hooks().aes_cbc_decrypt)(&key, &iv, &ciphertext).expect("reference");
        assert_eq!(
            plaintext,
            vec![
                0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
                0x17, 0x2a,
            ]
        );
    }

    /// An empty buffer is legal and decrypts to nothing, so a zero-length
    /// chunk never trips the length assertions on the calling side.
    #[test]
    fn empty_data_decrypts_to_empty() {
        test_reference::install();
        assert_eq!(
            (hooks().aes_cbc_decrypt)(&[0u8; 32], &[0u8; 16], &[]),
            Ok(Vec::new())
        );
    }

    /// The contract's rejections are reported, not papered over.
    #[test]
    fn the_reference_hook_reports_contract_violations() {
        assert_eq!(
            test_reference::aes_cbc_decrypt(&[0u8; 16], &[0u8; 16], &[]),
            Err(HostAesError::BadKeyLength)
        );
        assert_eq!(
            test_reference::aes_cbc_decrypt(&[0u8; 32], &[0u8; 8], &[]),
            Err(HostAesError::BadIvLength)
        );
        assert_eq!(
            test_reference::aes_cbc_decrypt(&[0u8; 32], &[0u8; 16], &[0u8; 17]),
            Err(HostAesError::BadBlockLength)
        );
    }
}
