//! The one place that decides which implementation of SHA-256 the 7z `aes256`
//! coder uses, and the crate's own AES-256-CBC.
//!
//! Two SHA-256 backends are available, both of them `lzma-turbo`'s:
//!
//! - **`aws-lc-crypto`** (on by default) — `aws-lc-rs` over AWS-LC. It is the
//!   scryer-media house default, shared with `lzma-turbo` and `rarpar`, and it
//!   is what the numbers in `docs/benchmarking.md` were taken with.
//! - **`native-crypto`** — RustCrypto's `sha2`, for consumers who cannot have
//!   a C toolchain in their build.
//!
//! Cargo features are additive, so `native-crypto` cannot be expressed as
//! "turns `aws-lc-crypto` off". Instead **`native-crypto` takes precedence**:
//! whenever it is enabled this module selects RustCrypto, whether or not
//! AWS-LC is also compiled in. A build that wants no AWS-LC at all therefore
//! asks for `default-features = false` plus `native-crypto`, and a build that
//! enables both gets RustCrypto plus a test that the two agree.
//!
//! Enabling `aes256` with neither is a compile error rather than a silent
//! choice, because "which cryptography is in my binary" is not something a
//! crate should decide behind a consumer's back.
//!
//! # AES-256-CBC is this crate's own, and it follows the same switch
//!
//! `lzma-turbo` used to expose AES-256-CBC and the 7z key derivation; it
//! dropped both on purpose, because 7z cryptography is this crate's job and
//! that crate is LZMA, LZMA2 and the xz container. So the block cipher lives
//! here — but it is *not* pinned to one implementation: it follows the same
//! backend choice SHA-256 does.
//!
//! - **`aws-lc-crypto`** — `aws_lc_rs::cipher::DecryptingKey::cbc`, which is
//!   AWS-LC's *unpadded* CBC mode (no PKCS7; `StreamingDecryptingKey` is the
//!   padded one and is deliberately not used here). The key schedule is built
//!   once, in `new`, and every call reuses it.
//! - **`native-crypto`** — RustCrypto's `aes`/`cbc`, which compiles to AES-NI
//!   on x86-64 and to the ARMv8 cryptography extensions on aarch64.
//!
//! Driving CBC incrementally needs no streaming API on either lane: a chunk is
//! decrypted with the current IV, and that chunk's last ciphertext block —
//! copied out *before* the in-place decrypt — is the next chunk's IV. Both
//! lanes require whole blocks, which costs nothing here because 7z hands over
//! AES streams in 16-byte multiples and the caller in `encryption::aes`
//! buffers a partial tail block either way.
//!
//! The encoder (`compress` + `aes256`) stays on RustCrypto's `cbc::Encryptor`
//! in `encryption::aes`: writing archives is not the hot path this fork
//! exists for, and one encryptor is simpler than two.
//!
//! # The host-delegated backend (`crypto-host`, wasm only)
//!
//! A third choice exists for wasm embeddings: with `crypto-host` enabled on a
//! `wasm32` target, the bulk AES-256-CBC **decrypt** leaves the guest and runs
//! on the embedder's AES through a plain `fn` pointer hook (see
//! [`crate::hooks`]). A wasm guest has neither AES-NI nor the ARMv8
//! cryptography extensions, so the block cipher is the one part of 7z decoding
//! a host can do several times faster; everything else — the key derivation,
//! the LZMA/LZMA2 decode, the CRCs — stays in the guest where it belongs.
//!
//! Precedence is **host (wasm + `crypto-host`) > `native-crypto` > AWS-LC**.
//! On a native target `crypto-host` is accepted but inert: the in-process
//! backend stays selected and no hook is ever called, so feature unification
//! in a mixed workspace cannot silently turn a native build into a delegating
//! one. `crypto-host` pulls in neither `aes` nor `cbc` — a delegating wasm
//! build carries no in-guest AES at all — but it does forward
//! `lzma-turbo/native-crypto`, because SHA-256 for the 7z key derivation is
//! still computed in the guest (see the feature comment in `Cargo.toml`).
//!
//! The hook is stateless per call, so [`HostAes256Cbc`] threads the CBC IV
//! across chunks itself: before each in-place decrypt it copies out the
//! chunk's last *ciphertext* block, which is the next chunk's IV. That is
//! exactly what the stateful AWS-LC and RustCrypto contexts do internally,
//! only written out — and the differential test at the bottom of this file
//! proves it against the RustCrypto lane natively, with no wasm runtime in
//! sight.

#[cfg(feature = "native-crypto")]
use aes::Aes256;
#[cfg(feature = "native-crypto")]
use aes::cipher::{BlockModeDecrypt, KeyIvInit};
#[cfg(feature = "aws-lc-crypto")]
use aws_lc_rs::cipher::{AES_256, DecryptingKey, DecryptionContext, UnboundCipherKey};
#[cfg(feature = "aws-lc-crypto")]
use aws_lc_rs::iv::FixedLength;

// SHA-256 stays in-process on every lane, including the host-delegated one:
// `crypto-host` forwards `lzma-turbo/native-crypto`, so a delegating wasm
// guest has RustCrypto's SHA-256 without needing this crate's `native-crypto`
// (which would also drag `aes`/`cbc` in). Delegating SHA-256 as well is a
// follow-up that waits on `lzma-turbo`'s own host hooks.
#[cfg(all(
    feature = "aws-lc-crypto",
    not(feature = "native-crypto"),
    not(all(target_arch = "wasm32", feature = "crypto-host"))
))]
pub(crate) use lzma_turbo::crypto::awslc::Sha256;
#[cfg(any(
    feature = "native-crypto",
    all(target_arch = "wasm32", feature = "crypto-host")
))]
pub(crate) use lzma_turbo::crypto::rustcrypto::Sha256;

#[cfg(not(any(
    feature = "aws-lc-crypto",
    feature = "native-crypto",
    all(target_arch = "wasm32", feature = "crypto-host")
)))]
compile_error!(
    "the `aes256` feature needs a SHA-256 backend: enable `aws-lc-crypto` \
     (the default, AWS-LC) or `native-crypto` (RustCrypto, no C toolchain). \
     On wasm32, `crypto-host` also satisfies this: it delegates AES to the \
     embedder and keeps RustCrypto's SHA-256 in the guest"
);

/// Which cryptography backend this build selected, for SHA-256 and for
/// AES-256-CBC alike. Only used by tests and
/// diagnostics, but a consumer wondering what is in their binary should be
/// able to ask.
pub(crate) const BACKEND: &str = if cfg!(all(target_arch = "wasm32", feature = "crypto-host")) {
    "host"
} else if cfg!(feature = "native-crypto") {
    "rustcrypto"
} else {
    "aws-lc"
};

/// Size of an AES block, for callers that chunk their input.
pub(crate) const AES_BLOCK_LEN: usize = 16;

/// Size of an AES-256 key.
pub(crate) const AES256_KEY_LEN: usize = 32;

/// What can go wrong in the block cipher. Nothing here is a decode error of
/// the archive's payload, so it is a type of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AesError {
    /// The key was not 32 bytes.
    KeyLength,
    /// The initialisation vector was not 16 bytes.
    IvLength,
    /// The ciphertext was not a whole number of blocks.
    BlockAlignment,
    /// The backend refused the key or the buffer. AWS-LC reports failures
    /// without a reason, so there is nothing more specific to say. The
    /// RustCrypto lane cannot produce it.
    #[cfg_attr(
        any(feature = "native-crypto", not(feature = "aws-lc-crypto")),
        allow(dead_code)
    )]
    Backend,
}

impl std::fmt::Display for AesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::KeyLength => "an AES-256 key is 32 bytes",
            Self::IvLength => "an AES initialisation vector is 16 bytes",
            Self::BlockAlignment => "AES-CBC ciphertext is a whole number of 16-byte blocks",
            Self::Backend => "the AES backend refused the key or the ciphertext",
        })
    }
}

impl std::error::Error for AesError {}

/// The shape both AES-256-CBC backends share, so `encryption::aes` is written
/// once against one name and the cross-backend test can drive either one.
///
/// Unpadded: 7z streams carry no padding, the coder's declared unpacked size
/// is what ends the decode. The chaining state carries across calls, so a
/// caller may decrypt a stream in whatever block-aligned pieces it has.
pub(crate) trait Aes256CbcLike: Sized {
    /// A decryptor for `key` starting from `iv`.
    fn new(key: &[u8], iv: &[u8]) -> Result<Self, AesError>;
    /// Decrypts `data` in place and advances the chaining state.
    fn decrypt(&mut self, data: &mut [u8]) -> Result<(), AesError>;
}

/// AES-256-CBC over AWS-LC.
///
/// `DecryptingKey::cbc` is the unpadded mode (`OperatingMode::CBC`, no PKCS7),
/// and `decrypt` takes `&self`, so the key schedule built here is reused by
/// every chunk. The IV for the next chunk is this chunk's last ciphertext
/// block, copied out before the in-place decrypt destroys it.
#[cfg(feature = "aws-lc-crypto")]
// Built even when `native-crypto` wins the selection, so a build with both
// features can compare the two lanes against each other.
#[cfg_attr(feature = "native-crypto", allow(dead_code))]
pub(crate) struct AwsLcAes256Cbc {
    key: DecryptingKey,
    iv: [u8; AES_BLOCK_LEN],
}

#[cfg(feature = "aws-lc-crypto")]
impl std::fmt::Debug for AwsLcAes256Cbc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key or chaining state.
        f.write_str("AwsLcAes256Cbc(..)")
    }
}

#[cfg(feature = "aws-lc-crypto")]
impl Aes256CbcLike for AwsLcAes256Cbc {
    fn new(key: &[u8], iv: &[u8]) -> Result<Self, AesError> {
        let key: &[u8; AES256_KEY_LEN] = key.try_into().map_err(|_| AesError::KeyLength)?;
        let iv: [u8; AES_BLOCK_LEN] = iv.try_into().map_err(|_| AesError::IvLength)?;
        let unbound = UnboundCipherKey::new(&AES_256, key).map_err(|_| AesError::Backend)?;
        let key = DecryptingKey::cbc(unbound).map_err(|_| AesError::Backend)?;
        Ok(Self { key, iv })
    }

    fn decrypt(&mut self, data: &mut [u8]) -> Result<(), AesError> {
        if !data.len().is_multiple_of(AES_BLOCK_LEN) {
            return Err(AesError::BlockAlignment);
        }
        if data.is_empty() {
            return Ok(());
        }
        let mut next_iv = [0u8; AES_BLOCK_LEN];
        next_iv.copy_from_slice(&data[data.len() - AES_BLOCK_LEN..]);
        self.key
            .decrypt(data, DecryptionContext::Iv128(FixedLength::from(self.iv)))
            .map_err(|_| AesError::Backend)?;
        self.iv = next_iv;
        Ok(())
    }
}

/// AES-256-CBC over RustCrypto's `aes`/`cbc`, for builds without a C
/// toolchain. `cbc::Decryptor` carries the chaining state itself.
#[cfg(feature = "native-crypto")]
pub(crate) struct RustCryptoAes256Cbc(cbc::Decryptor<Aes256>);

#[cfg(feature = "native-crypto")]
impl std::fmt::Debug for RustCryptoAes256Cbc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key or chaining state.
        f.write_str("RustCryptoAes256Cbc(..)")
    }
}

#[cfg(feature = "native-crypto")]
impl Aes256CbcLike for RustCryptoAes256Cbc {
    fn new(key: &[u8], iv: &[u8]) -> Result<Self, AesError> {
        let key: &[u8; AES256_KEY_LEN] = key.try_into().map_err(|_| AesError::KeyLength)?;
        let iv: &[u8; AES_BLOCK_LEN] = iv.try_into().map_err(|_| AesError::IvLength)?;
        Ok(Self(cbc::Decryptor::<Aes256>::new(key.into(), iv.into())))
    }

    fn decrypt(&mut self, data: &mut [u8]) -> Result<(), AesError> {
        if !data.len().is_multiple_of(AES_BLOCK_LEN) {
            return Err(AesError::BlockAlignment);
        }
        for block in data.chunks_exact_mut(AES_BLOCK_LEN) {
            let block: &mut [u8; AES_BLOCK_LEN] = block.try_into().expect("exact chunk");
            self.0.decrypt_block(block.into());
        }
        Ok(())
    }
}

/// AES-256-CBC delegated to the embedding host (see [`crate::hooks`]).
///
/// Compiled whenever `crypto-host` is enabled — on native targets too, where
/// it is not the selected backend but is driven by the differential test below
/// through the very same hook a wasm embedder installs. `fn` pointers link on
/// any target, so that test is the real delegation path, not a stand-in.
///
/// The key is held raw because the hook takes it by slice on every call; there
/// is no key schedule to build once, since the host owns the cipher.
#[cfg(feature = "crypto-host")]
#[cfg_attr(
    not(all(target_arch = "wasm32", feature = "crypto-host")),
    allow(dead_code)
)]
pub(crate) struct HostAes256Cbc {
    key: zeroize::Zeroizing<[u8; AES256_KEY_LEN]>,
    iv: [u8; AES_BLOCK_LEN],
}

#[cfg(feature = "crypto-host")]
impl std::fmt::Debug for HostAes256Cbc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key or chaining state.
        f.write_str("HostAes256Cbc(..)")
    }
}

#[cfg(feature = "crypto-host")]
impl Aes256CbcLike for HostAes256Cbc {
    fn new(key: &[u8], iv: &[u8]) -> Result<Self, AesError> {
        let key = zeroize::Zeroizing::new(
            <[u8; AES256_KEY_LEN]>::try_from(key).map_err(|_| AesError::KeyLength)?,
        );
        let iv: [u8; AES_BLOCK_LEN] = iv.try_into().map_err(|_| AesError::IvLength)?;
        Ok(Self { key, iv })
    }

    fn decrypt(&mut self, data: &mut [u8]) -> Result<(), AesError> {
        if !data.len().is_multiple_of(AES_BLOCK_LEN) {
            return Err(AesError::BlockAlignment);
        }
        if data.is_empty() {
            return Ok(());
        }
        // The hook is stateless, so the IV for the next chunk has to be saved
        // here — and *before* the decrypt, which overwrites the ciphertext.
        let mut next_iv = [0u8; AES_BLOCK_LEN];
        next_iv.copy_from_slice(&data[data.len() - AES_BLOCK_LEN..]);

        // A hook that refuses a call this crate made, or answers with the
        // wrong number of bytes, is an embedder contract violation: the guest
        // asked for whole blocks under a 32-byte key and a 16-byte IV. There
        // is nothing to fall back to and nothing to report up the stack that a
        // caller could act on, so it panics rather than corrupting a decode.
        let hooks = crate::hooks::hooks();
        let plaintext = match (hooks.aes_cbc_decrypt)(self.key.as_ref(), &self.iv, data) {
            Ok(plaintext) => zeroize::Zeroizing::new(plaintext),
            Err(err) => {
                panic!("sevenz-turbo: host aes-cbc-decrypt failed: {err} (contract violation)")
            }
        };
        assert_eq!(
            plaintext.len(),
            data.len(),
            "sevenz-turbo: host aes-cbc-decrypt returned {} bytes for a {}-byte input \
             (contract violation)",
            plaintext.len(),
            data.len(),
        );
        data.copy_from_slice(&plaintext);

        self.iv = next_iv;
        Ok(())
    }
}

/// The cipher this build selected, under one name. The host-delegated backend
/// wins on a `wasm32` build with `crypto-host`; otherwise `native-crypto`
/// takes precedence exactly as it does for SHA-256.
#[cfg(all(target_arch = "wasm32", feature = "crypto-host"))]
pub(crate) type Aes256Cbc = HostAes256Cbc;
#[cfg(all(
    feature = "native-crypto",
    not(all(target_arch = "wasm32", feature = "crypto-host"))
))]
pub(crate) type Aes256Cbc = RustCryptoAes256Cbc;
#[cfg(all(
    feature = "aws-lc-crypto",
    not(feature = "native-crypto"),
    not(all(target_arch = "wasm32", feature = "crypto-host"))
))]
pub(crate) type Aes256Cbc = AwsLcAes256Cbc;

/// The shape both backends' SHA-256 share, so the key derivation can be
/// written once and run against either one. `lzma-turbo` exposes two concrete
/// types rather than a trait, and a trait defined here can be implemented for
/// both of them.
pub(crate) trait Sha256Like: Sized {
    /// A hash over no bytes yet.
    fn new() -> Self;
    /// Feeds the next bytes of the message.
    fn update(&mut self, data: &[u8]);
    /// Consumes the hash and returns the digest.
    fn finalize(self) -> [u8; 32];
}

macro_rules! impl_sha256_like {
    ($ty:path) => {
        impl Sha256Like for $ty {
            fn new() -> Self {
                <$ty>::new()
            }
            fn update(&mut self, data: &[u8]) {
                <$ty>::update(self, data);
            }
            fn finalize(self) -> [u8; 32] {
                <$ty>::finalize(self)
            }
        }
    };
}

#[cfg(all(
    feature = "aws-lc-crypto",
    not(all(target_arch = "wasm32", feature = "crypto-host"))
))]
impl_sha256_like!(lzma_turbo::crypto::awslc::Sha256);
// `crypto-host` forwards `lzma-turbo/native-crypto`, so the RustCrypto hash is
// present on a delegating wasm build even without this crate's own
// `native-crypto`.
#[cfg(any(
    feature = "native-crypto",
    all(target_arch = "wasm32", feature = "crypto-host")
))]
impl_sha256_like!(lzma_turbo::crypto::rustcrypto::Sha256);

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(feature = "native-crypto", feature = "compress"))]
    #[test]
    fn aes_key_schedules_are_cleared_on_drop() {
        fn requires_clearing<T: zeroize::ZeroizeOnDrop>() {}
        requires_clearing::<aes::Aes256>();
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// NIST SP 800-38A, F.2.6 (CBC-AES256.Decrypt): the four-block vector,
    /// which is also F.2.5's ciphertext.
    const NIST_KEY: &str = "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4";
    const NIST_IV: &str = "000102030405060708090a0b0c0d0e0f";
    const NIST_CIPHERTEXT: &str = concat!(
        "f58c4c04d6e5f1ba779eabfb5f7bfbd6",
        "9cfc4e967edb808d679f777bc6702c7d",
        "39f23369a9d9bacfa530e26304231461",
        "b2eb05e2c39be9fcda6c19078c6a9d1b",
    );
    const NIST_PLAINTEXT: &str = concat!(
        "6bc1bee22e409f96e93d7e117393172a",
        "ae2d8a571e03ac9c9eb76fac45af8e51",
        "30c81c46a35ce411e5fbc1191a0a52ef",
        "f69f2445df4f9b17ad2b417be66c3710",
    );

    #[test]
    fn nist_cbc_aes256_decrypt() {
        let mut data = unhex(NIST_CIPHERTEXT);
        let mut cipher =
            Aes256Cbc::new(&unhex(NIST_KEY), &unhex(NIST_IV)).expect("key and iv are sized");
        cipher.decrypt(&mut data).expect("aligned ciphertext");
        assert_eq!(hex(&data), NIST_PLAINTEXT, "backend {BACKEND}");
    }

    /// The same vector fed one block at a time: CBC chaining has to survive
    /// being driven incrementally, which is exactly how the 7z reader drives
    /// it (it decrypts whatever the packed stream hands it).
    #[test]
    fn cbc_chaining_is_incremental() {
        let whole = unhex(NIST_CIPHERTEXT);
        let mut cipher =
            Aes256Cbc::new(&unhex(NIST_KEY), &unhex(NIST_IV)).expect("key and iv are sized");
        let mut out = Vec::new();
        for block in whole.chunks(AES_BLOCK_LEN) {
            let mut block = block.to_vec();
            cipher.decrypt(&mut block).expect("aligned ciphertext");
            out.extend_from_slice(&block);
        }
        assert_eq!(hex(&out), NIST_PLAINTEXT, "backend {BACKEND}");
    }

    #[test]
    fn sha256_known_vectors() {
        let mut sha = Sha256::new();
        sha.update(b"abc");
        assert_eq!(
            hex(&sha.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "backend {BACKEND}"
        );

        let mut sha = Sha256::new();
        for _ in 0..1000 {
            sha.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&sha.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            "backend {BACKEND}"
        );
    }

    /// When both backends are compiled in, they must agree — on raw AES-CBC,
    /// on SHA-256, and on the 7z key derivation that sits on top of it,
    /// including the two cycle counts `7zAes.c` treats specially.
    #[cfg(all(feature = "aws-lc-crypto", feature = "native-crypto"))]
    mod differential {
        use lzma_turbo::crypto::{awslc, rustcrypto};

        use crate::encryption::derive_key_with;

        fn sample(len: usize, seed: u64) -> Vec<u8> {
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

        use super::{
            AES_BLOCK_LEN, Aes256CbcLike, AwsLcAes256Cbc, RustCryptoAes256Cbc,
            tests::{NIST_CIPHERTEXT, NIST_IV, NIST_KEY, NIST_PLAINTEXT, hex, unhex},
        };

        /// The NIST vector on *both* lanes, not only the selected one.
        #[test]
        fn both_lanes_match_the_nist_vector() {
            let mut a = unhex(NIST_CIPHERTEXT);
            AwsLcAes256Cbc::new(&unhex(NIST_KEY), &unhex(NIST_IV))
                .expect("key and iv are sized")
                .decrypt(&mut a)
                .expect("aligned ciphertext");
            assert_eq!(hex(&a), NIST_PLAINTEXT, "aws-lc");

            let mut b = unhex(NIST_CIPHERTEXT);
            RustCryptoAes256Cbc::new(&unhex(NIST_KEY), &unhex(NIST_IV))
                .expect("key and iv are sized")
                .decrypt(&mut b)
                .expect("aligned ciphertext");
            assert_eq!(hex(&b), NIST_PLAINTEXT, "rustcrypto");
        }

        /// Whole-buffer decryption, several sizes, both lanes byte-identical.
        #[test]
        fn backends_agree_on_aes256_cbc() {
            let key = sample(32, 11);
            let iv = sample(AES_BLOCK_LEN, 12);
            for blocks in [1usize, 2, 3, 7, 64] {
                let data = sample(blocks * AES_BLOCK_LEN, 100 + blocks as u64);

                let mut a = data.clone();
                AwsLcAes256Cbc::new(&key, &iv)
                    .expect("sized")
                    .decrypt(&mut a)
                    .expect("aligned");
                let mut b = data;
                RustCryptoAes256Cbc::new(&key, &iv)
                    .expect("sized")
                    .decrypt(&mut b)
                    .expect("aligned");

                assert_eq!(a, b, "backends disagree on {blocks} blocks");
            }
        }

        /// The chaining state has to survive being driven in pieces, and the
        /// pieces a 7z reader hands over are whatever the layer below produced
        /// — so the chunk sizes here are deliberately uneven. Each lane is
        /// compared both against the other and against its own one-shot
        /// result, which is what catches an IV carried wrongly.
        #[test]
        fn backends_agree_when_driven_in_chunks() {
            let key = sample(32, 21);
            let iv = sample(AES_BLOCK_LEN, 22);
            let data = sample(64 * AES_BLOCK_LEN, 23);

            let mut oneshot = data.clone();
            AwsLcAes256Cbc::new(&key, &iv)
                .expect("sized")
                .decrypt(&mut oneshot)
                .expect("aligned");

            for chunk_blocks in [1usize, 2, 3, 5, 13] {
                let chunk = chunk_blocks * AES_BLOCK_LEN;

                let mut aws = AwsLcAes256Cbc::new(&key, &iv).expect("sized");
                let mut rc = RustCryptoAes256Cbc::new(&key, &iv).expect("sized");
                let (mut out_a, mut out_b) = (Vec::new(), Vec::new());
                for piece in data.chunks(chunk) {
                    let mut a = piece.to_vec();
                    aws.decrypt(&mut a).expect("aligned");
                    out_a.extend_from_slice(&a);

                    let mut b = piece.to_vec();
                    rc.decrypt(&mut b).expect("aligned");
                    out_b.extend_from_slice(&b);
                }

                assert_eq!(out_a, oneshot, "aws-lc differs in {chunk}-byte chunks");
                assert_eq!(out_b, oneshot, "rustcrypto differs in {chunk}-byte chunks");
            }
        }

        /// An empty call must be a no-op on both lanes rather than an error or
        /// a disturbed IV: a coder below can hand over zero bytes.
        #[test]
        fn an_empty_chunk_changes_nothing() {
            let key = sample(32, 31);
            let iv = sample(AES_BLOCK_LEN, 32);
            let data = sample(4 * AES_BLOCK_LEN, 33);

            for empty_first in [true, false] {
                let mut aws = AwsLcAes256Cbc::new(&key, &iv).expect("sized");
                let mut rc = RustCryptoAes256Cbc::new(&key, &iv).expect("sized");
                if empty_first {
                    aws.decrypt(&mut []).expect("empty is fine");
                    rc.decrypt(&mut []).expect("empty is fine");
                }
                let mut a = data.clone();
                aws.decrypt(&mut a).expect("aligned");
                let mut b = data.clone();
                rc.decrypt(&mut b).expect("aligned");
                assert_eq!(a, b);
            }
        }

        /// A half block is refused by both, with the same error.
        #[test]
        fn backends_agree_on_refusing_a_partial_block() {
            let key = sample(32, 41);
            let iv = sample(AES_BLOCK_LEN, 42);
            let mut data = sample(AES_BLOCK_LEN + 1, 43);

            let a = AwsLcAes256Cbc::new(&key, &iv)
                .expect("sized")
                .decrypt(&mut data.clone());
            let b = RustCryptoAes256Cbc::new(&key, &iv)
                .expect("sized")
                .decrypt(&mut data);
            assert_eq!(a, Err(super::AesError::BlockAlignment));
            assert_eq!(b, Err(super::AesError::BlockAlignment));
        }

        #[test]
        fn backends_agree_on_sha256() {
            for len in [0usize, 1, 55, 56, 64, 65, 1000] {
                let message = sample(len, 77 + len as u64);

                let mut a = awslc::Sha256::new();
                a.update(&message);
                let mut b = rustcrypto::Sha256::new();
                b.update(&message);

                assert_eq!(
                    a.finalize(),
                    b.finalize(),
                    "backends disagree on {len} bytes"
                );
            }
        }

        /// `7zAes.c`'s derivation, both backends. The two special cycle counts
        /// never reach a backend — `0x3F` means "the key is the salt and the
        /// password themselves" and `>= 0x40` is rejected outright, both in
        /// `encryption::aes::get_aes_key` before any hashing — so they are
        /// covered by that module's own tests instead.
        #[test]
        fn backends_agree_on_the_7z_key_derivation() {
            let salt = b"\x01\x02\x03\x04\x05\x06\x07\x08";
            let password = b"p\0a\0s\0s\0w\0o\0r\0d\0";
            for cycles in [0u8, 1, 4, 8, 12] {
                let a = derive_key_with::<awslc::Sha256>(cycles, salt, password);
                let b = derive_key_with::<rustcrypto::Sha256>(cycles, salt, password);
                assert_eq!(a, b, "backends disagree at cycles {cycles}");
            }
        }
    }

    /// The host-delegated lane, driven NATIVELY through the real hook.
    ///
    /// `crypto-host` is inert on a native target — `Aes256Cbc` is still the
    /// in-process backend there — but [`HostAes256Cbc`] itself compiles and
    /// the hook is a plain `fn` pointer, so the whole delegation path can be
    /// exercised here without a wasm runtime: registry lookup, the fresh
    /// buffer the hook returns, the copy back, and above all the guest-tracked
    /// CBC IV threading across chunk boundaries. The wasm harness in
    /// `tools/wasm-conformance/tests/wasm_host_extract_conformance.rs` separately proves the same
    /// path links and extracts a real archive inside a guest.
    ///
    /// Needs `native-crypto` only for the reference hook's own AES.
    #[cfg(all(feature = "crypto-host", feature = "native-crypto"))]
    mod host_delegation {
        use super::{
            AES_BLOCK_LEN, AES256_KEY_LEN, Aes256CbcLike, AesError, HostAes256Cbc,
            RustCryptoAes256Cbc,
            tests::{NIST_CIPHERTEXT, NIST_IV, NIST_KEY, NIST_PLAINTEXT, hex, unhex},
        };
        use crate::hooks::test_reference;

        fn sample(len: usize, seed: u64) -> Vec<u8> {
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

        /// The NIST vector through the hook, one shot.
        #[test]
        fn the_host_lane_matches_the_nist_vector() {
            test_reference::install();
            let mut data = unhex(NIST_CIPHERTEXT);
            HostAes256Cbc::new(&unhex(NIST_KEY), &unhex(NIST_IV))
                .expect("key and iv are sized")
                .decrypt(&mut data)
                .expect("aligned ciphertext");
            assert_eq!(hex(&data), NIST_PLAINTEXT);
        }

        /// The load-bearing one: a stateless hook means the IV is threaded by
        /// this crate, so every block-aligned way of slicing the same stream
        /// has to come out identical to the in-process backend's one-shot
        /// answer. Uneven chunk sizes are what a 7z reader actually produces.
        #[test]
        fn the_host_lane_chains_the_iv_like_the_in_process_backend() {
            test_reference::install();
            let key = sample(32, 51);
            let iv = sample(AES_BLOCK_LEN, 52);
            let data = sample(64 * AES_BLOCK_LEN, 53);

            let mut oneshot = data.clone();
            RustCryptoAes256Cbc::new(&key, &iv)
                .expect("sized")
                .decrypt(&mut oneshot)
                .expect("aligned");

            for chunk_blocks in [1usize, 2, 3, 5, 13, 64] {
                let mut host = HostAes256Cbc::new(&key, &iv).expect("sized");
                let mut out = Vec::new();
                for piece in data.chunks(chunk_blocks * AES_BLOCK_LEN) {
                    let mut piece = piece.to_vec();
                    host.decrypt(&mut piece).expect("aligned");
                    out.extend_from_slice(&piece);
                }
                assert_eq!(
                    out, oneshot,
                    "host lane diverged in {chunk_blocks}-block chunks"
                );
            }
        }

        /// An empty call must not disturb the chaining state — a coder below
        /// can hand over zero bytes — and a partial block is refused before
        /// the hook is ever reached, so a host never sees a call this crate's
        /// own contract forbids.
        #[test]
        fn empty_and_misaligned_chunks_behave_like_the_other_lanes() {
            test_reference::install();
            let key = sample(32, 61);
            let iv = sample(AES_BLOCK_LEN, 62);
            let data = sample(4 * AES_BLOCK_LEN, 63);

            let mut expected = data.clone();
            RustCryptoAes256Cbc::new(&key, &iv)
                .expect("sized")
                .decrypt(&mut expected)
                .expect("aligned");

            let mut host = HostAes256Cbc::new(&key, &iv).expect("sized");
            host.decrypt(&mut []).expect("empty is fine");
            let mut got = data.clone();
            host.decrypt(&mut got).expect("aligned");
            assert_eq!(got, expected);

            let mut half = sample(AES_BLOCK_LEN + 1, 64);
            assert_eq!(
                HostAes256Cbc::new(&key, &iv)
                    .expect("sized")
                    .decrypt(&mut half),
                Err(AesError::BlockAlignment)
            );
        }

        /// A wrong-sized key or IV is refused where the other lanes refuse it:
        /// in `new`, with the same error, so the hook only ever sees the sizes
        /// its contract names.
        #[test]
        fn the_host_lane_refuses_a_wrong_sized_key_or_iv() {
            assert_eq!(
                HostAes256Cbc::new(&[0u8; 16], &[0u8; AES_BLOCK_LEN]).err(),
                Some(AesError::KeyLength)
            );
            assert_eq!(
                HostAes256Cbc::new(&[0u8; AES256_KEY_LEN], &[0u8; 8]).err(),
                Some(AesError::IvLength)
            );
        }
    }
}
