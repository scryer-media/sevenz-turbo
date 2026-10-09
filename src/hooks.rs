//! Embedder-supplied delegation hooks for the bulk cryptography and checksums.
//!
//! With `crypto-host` and/or `crc-host` enabled, a wasm guest build of this
//! crate does not run those primitives itself: it calls plain Rust function
//! pointers that the embedding program installs at start-up. Whatever sits
//! behind the pointers — a raw wasm import in some namespace, a
//! `wit-bindgen`-generated component import, a host SDK call — is the
//! embedder's business. `sevenz-turbo` takes no dependency on any runtime,
//! SDK, or interface definition; it only calls the `fn` pointers it was handed.
//!
//! There are two seams, because the work is split between two crates:
//!
//! - **This crate's**, `install_host_crypto_hooks`: the bulk AES-256-CBC
//!   decrypt of the 7z `aes256` coder. Present with `aes256` and
//!   `crypto-host`.
//! - **`lzma-turbo`'s**, re-exported here as [`install_host_hash_hooks`](crate::hooks::install_host_hash_hooks): the
//!   CRC-32 of every 7z header, folder and member (`crc-host`), and the
//!   SHA-256 of the 7z key derivation (`crypto-host`). This crate computes both
//!   through `lzma-turbo`'s `crc` and `crypto` modules, so forwarding the
//!   features there is what delegates them. The re-export means an embedder
//!   needs no direct dependency on `lzma-turbo` to install them.
//!
//! This module exists whenever either feature is enabled. On native targets
//! the features are accepted but the in-process backends (AWS-LC or
//! RustCrypto, `crc-fast`) stay active, so an installed hook is never called
//! there and feature unification in a mixed workspace cannot silently turn a
//! native build into a delegating one.
//!
//! ## The seams
//!
//! ```ignore
//! use sevenz_turbo::hooks::{
//!     HostAesError, HostCryptoHooks, HostHashHooks, install_host_crypto_hooks,
//!     install_host_hash_hooks,
//! };
//!
//! fn aes(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError> {
//!     // forward to the embedder's AES-256-CBC decrypt
//! }
//!
//! install_host_crypto_hooks(HostCryptoHooks { aes_cbc_decrypt: aes });
//! install_host_hash_hooks(HostHashHooks::new(
//!     crc32, crc64_xz, sha256_init, sha256_clone, sha256_update, sha256_finalize, sha256_drop,
//! ));
//! ```
//!
//! `HostHashHooks` takes every field whichever of the two features is on; only
//! the hooks a feature consumes are ever called. This crate never calls
//! `crc64_xz` (7z has no CRC-64), so an embedder may give it a body that
//! panics.
//!
//! `examples/wasm_host_extract_conformance.rs` is a complete reference
//! embedding: a `wasm32-wasip1` guest that declares its raw imports in a
//! `host` namespace and installs hooks that forward to them, driven by the
//! native `wasmtime` harness in `tools/wasm-conformance`.
//!
//! ## Contract the AES hook must satisfy
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
//! Only the *decrypt* is delegated. The encoder (`compress` + `aes256`) uses
//! RustCrypto's `cbc::Encryptor` in the guest; writing archives is not the
//! path this seam exists for.
//!
//! ## Contract the hash hooks must satisfy
//!
//! The CRC and SHA-256 hooks are `lzma-turbo`'s, and so is their contract: the
//! CRCs are resumable one-shots in the finalized domain, SHA-256 is a
//! streaming state behind an opaque [`HostSha256Handle`](crate::hooks::HostSha256Handle), and a missing hook
//! panics naming `install_host_hash_hooks`. See `lzma_turbo::hooks` for the
//! full text.

#[cfg(all(feature = "aes256", feature = "crypto-host"))]
mod aes;

#[cfg(all(feature = "aes256", feature = "crypto-host"))]
pub(crate) use aes::hooks;
#[cfg(all(
    test,
    feature = "aes256",
    feature = "crypto-host",
    feature = "native-crypto"
))]
pub(crate) use aes::test_reference;
#[cfg(all(feature = "aes256", feature = "crypto-host"))]
pub use aes::{
    AesCbcDecryptHook, HostAesError, HostCryptoHooks, clear_host_crypto_hooks,
    host_crypto_hooks_installed, install_host_crypto_hooks,
};

pub use lzma_turbo::hooks::{
    Crc32Hook, Crc64XzHook, HostHashHooks, HostSha256Handle, Sha256CloneHook, Sha256DropHook,
    Sha256FinalizeHook, Sha256InitHook, Sha256UpdateHook, clear_host_hash_hooks,
    host_hash_hooks_installed, install_host_hash_hooks,
};
