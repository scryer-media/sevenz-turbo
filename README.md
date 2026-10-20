# sevenz-turbo

[![ci](https://github.com/scryer-media/sevenz-turbo/actions/workflows/ci.yml/badge.svg)](https://github.com/scryer-media/sevenz-turbo/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sevenz-turbo.svg)](https://crates.io/crates/sevenz-turbo)
[![docs.rs](https://docs.rs/sevenz-turbo/badge.svg)](https://docs.rs/sevenz-turbo)

Fast 7z archives for Rust: read them, write them, encrypt and decrypt them.
LZMA and LZMA2 go through [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo),
a port of 7-Zip's own code and assembly, so decoding keeps pace with 7-Zip
and can use several threads.

```toml
[dependencies]
sevenz-turbo = "0.29"
```

It needs Rust 1.97.1 or newer. The default build uses AWS-LC for encryption,
which needs a C compiler; [Encryption](#encryption) shows the pure-Rust build.

## Why a fork?

`sevenz-turbo` is a fork of
**[sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2)** by Nils
Hasenbanck (Apache-2.0), taken at upstream `12ed7c8`, just after v0.22.2. It
is a hard fork: it is not rebased onto sevenz-rust2, nothing is sent back, and
the two have diverged for good.

Outreach was made to Hasenbanck to collaborate, but the core ethos of the `-turbo` line of crates was not something he was interested in engaging with. His crates focus more on pure Rust implementations with no unsafe and more readable code where the `-turbo` crates are performance driven.

Given that `sevenz-turbo` uses unsafe Rust and assembly, the CI pipeline is incredibly robust. Every bit of unsafe code is tested with every possible method of ensuring it is safe and secure. This ranges from valgrind to fuzz testing, miri and ASan. All assembly receives provenance checks with its sourced upstream code, which means any later fixes/changes to upstream will be found quickly in CI. Also the repo is checked every morning against upstream CVE reported to `7z` so that those fixes can be ported quickly if they apply to `sevenz-turbo`.

All reasonable methods of creating `7z` archives are also differentially tested against the upstream tooling to ensure interoperability.

`sevenz-turbo` does have pure Rust fallbacks for every algorithm and the crate can be compiled as pure Rust. The performance degrades but is still fantastic. However optimization time and benchmarking efforts go toward the default features rather than pure Rust. 

### Moving from sevenz-rust2

The API started as sevenz-rust2's, so most code moves over by renaming
`sevenz_rust2::` to `sevenz_turbo::`. Every difference, such as the default
of one decode thread, is listed in [CHANGELOG.md](CHANGELOG.md) under
`## Fork`.

## Reading an archive

```rust
sevenz_turbo::decompress_file("data/sample.7z", "data/sample").expect("complete");
```

With a password:

```rust
sevenz_turbo::decompress_file_with_password("path/to/encrypted.7z", "data/sample", "password".into()).expect("complete");
```

### Using several threads

LZMA2 can decode on several threads. It uses one unless asked, because a
library should not take every core on its own:

```rust
let mut reader = ArchiveReader::new(file, Password::empty())?.with_threads(8);
```

Only an archive written in independent pieces can be split, which is what
7-Zip writes when it compresses on several threads. One written on a single
thread (`7zz -mmt=1`) decodes on one thread whatever you ask for, and asking
costs nothing. Each extra thread holds a piece of the archive in memory,
about as much as 7-Zip holds for the same archive. `ArchiveLimits::memory`
caps it.

The thread count can also change while a block is decoding, from any thread:

```rust
let mut reader = ArchiveReader::new(file, Password::empty())?.with_adaptive_lzma2();
let lzma2 = reader.lzma2_handle();
// … on whatever thread is watching the download:
if let Some(progress) = lzma2.progress() {
    // `pending_runs` is how many pieces have arrived and wait to be decoded.
    lzma2.set_threads(if progress.pending_runs > 1 { 8 } else { 1 });
}
```

The change takes effect at the next piece, and the output is the same either
way. `set_threads(1)` decodes on the calling thread and starts no others.

### Archives from strangers

Every number in a 7z header is chosen by whoever wrote the file. This crate
checks each one before it allocates or works on it: counts, names,
dictionaries, nesting, the password's key-derivation cost, the output size.
The default limits apply even when you pass none, and no archive a normal
7-Zip writes reaches them.

```rust
use sevenz_turbo::{ArchiveLimits, ArchiveReader, Password};

let limits = ArchiveLimits::memory(512 << 20)   // what a decode may allocate
    .with_max_unpack_bytes(8 << 30)             // refuse a bomb at open
    .rejecting_unsafe_paths();                  // and a name that would escape
let reader = ArchiveReader::with_limits(file, Password::empty(), limits)?;
```

`Error::LimitExceeded { what, limit, requested }` says which limit stopped
the read, so you can tell "too big for us" from "corrupt". Before anything is
written, `ArchiveEntry::is_unsafe_path()` and `is_symlink()` say what an
entry's name would do.

[docs/security.md](docs/security.md) lists every limit, its default and what
happens when it is hit.

### Reading from a download

A program that streams an archive in, under a memory budget, can ask it what
it needs first:

- `ArchiveReader::with_limits` refuses an archive over its limits before
  allocating for it, and `Archive::decoder_memory_estimate()` says what a
  decode will allocate.
- `Archive::block_pack_streams(block)` gives the byte ranges a block is
  stored in, and `Archive::block_sub_streams(block)` the size and CRC-32 of
  each file in it.
- `ArchiveReader::block_decoder(block)` decodes one block without using up
  the reader, so the header is read once.
- `set_block_complete_hook` reports each block once it is decoded and
  checked, and `set_sub_stream_complete_hook` each file with its CRC-32, so
  nothing has to be read twice to report integrity.
- `crc32_combine` and `CrcFolder` join the CRC-32s of adjacent pieces, for a
  check that crosses blocks.
- `Error::BlockDecode` names the block and offset of corrupt data, apart from
  I/O errors and unsupported methods.

When an LZMA2 block decodes on several threads, each thread checksums the
part it decoded and the parts are joined, so checking costs the reading
thread nothing. A block decoded on one thread, or passed through a filter, is
checked as it streams.

## Writing an archive

```rust
sevenz_turbo::compress_to_path("examples/data/sample", "examples/data/sample.7z").expect("compress ok");
```

With a password:

```rust
sevenz_turbo::compress_to_path_encrypted("examples/data/sample", "examples/data/sample.7z", "password".into()).expect("compress ok");
```

LZMA and LZMA2 are compressed by `lzma-turbo`'s port of 7-Zip's encoder.
Level 0 uses a 256 KiB dictionary and level 9 a 64 MiB one.

### Solid archives

A solid archive compresses better, but reading one file means decompressing
everything stored before it.

```rust
use sevenz_turbo::*;

let mut writer = ArchiveWriter::create("dest.7z").expect("create writer ok");
writer.push_source_path("path/to/compress", |_| true).expect("pack ok");
writer.finish().expect("compress ok");
```

### Choosing the methods

```rust
use sevenz_turbo::*;

let mut writer = ArchiveWriter::create("dest.7z").expect("create writer ok");
writer.set_content_methods(vec![
    encoder_options::AesEncoderOptions::new("password".into()).into(),
    encoder_options::Lzma2Options::from_level(9).into(),
]);
writer.push_source_path("path/to/compress", |_| true).expect("pack ok");
writer.finish().expect("compress ok");
```

## Codecs and filters

| Codec       | Decompression | Compression | Implemented by |
|-------------|---------------|-------------|----------------|
| COPY        | ✓            | ✓          | this crate |
| LZMA        | ✓            | ✓          | `lzma-turbo` |
| LZMA2       | ✓            | ✓          | `lzma-turbo`, on one thread or several |
| BROTLI (*)  | ✓            | ✓          | `brotli` crate |
| BZIP2       | ✓            | ✓          | `bzip2` crate |
| DEFLATE (*) | ✓            | ✓          | `flate2` crate (`zlib-rs`) |
| PPMD        | ✓            | ✓          | `ppmd-turbo` crate |
| LZ4 (*)     | ✓            | ✓          | `lz4_flex` crate |
| ZSTD (*)    | ✓            | ✓          | `zstd` crate, which builds C code |

(*) Needs its cargo feature.

| Filter        | Decompression | Compression |
|---------------|---------------|-------------|
| BCJ X86       | ✓            | ✓          |
| BCJ ARM       | ✓            | ✓          |
| BCJ ARM64     | ✓            | ✓          |
| BCJ ARM_THUMB | ✓            | ✓          |
| BCJ RISC_V    | ✓            | ✓          |
| BCJ PPC       | ✓            | ✓          |
| BCJ SPARC     | ✓            | ✓          |
| BCJ IA64      | ✓            | ✓          |
| BCJ2          | ✓            | ✓          |
| DELTA         | ✓            | ✓          |

The filters themselves are `lzma-turbo`'s; this crate wraps them as readers
and writers. CRC-32 comes from `crc-turbo`, through `lzma-turbo`.

## Speed

Measured with sevenz-turbo 0.27.0 against 7-Zip 26.03 and sevenz-rust2
0.22.2 on five idle machines, 2026-10-13. Each number is the reference's
time or memory divided by sevenz-turbo's, averaged (geometric mean) over the
tests, so above 1.00 means sevenz-turbo was faster or used less memory.
Decoding covers one thread to all of them, solid and non-solid, encrypted
and filtered archives; encoding covers levels 1 to 9.

| CPU | threads | vs 7-Zip: decode | encode | peak memory | vs sevenz-rust2: decode | peak memory |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Zen 4 | 16 | 1.16 | 1.54 | 0.97 | 2.42 | 1.16 |
| Graviton4 | 16 | 1.12 | 1.45 | 0.97 | 1.95 | 1.16 |
| Alder Lake | 16 | 1.13 | 1.42 | 0.96 | 1.88 | 1.11 |
| Zen 2 | 12 | 1.10 | 1.33 | 0.98 | 2.51 | 1.19 |
| M5 Max | 18 | 1.13 | 1.38 | 0.93 | 2.44 | 1.24 |

- LZMA2 decoding matches 7-Zip at every thread count on every machine. The
  decode average is above 1.00 because of encrypted, PPMd and non-solid
  archives.
- Slower than 7-Zip: nearly incompressible archives decoded on every thread
  (0.88 to 0.99), single-thread decoding through the ARM64 filter on Zen 4,
  Graviton4 and M5 Max (0.91 to 0.94), the peak memory of single-thread
  decoding with a large dictionary, and the encoder's peak memory.

Every test on every machine is in [docs/benchmarks.md](docs/benchmarks.md),
and how to measure your own is in [docs/benchmarking.md](docs/benchmarking.md).

## Encryption

7z encryption is AES-256, with a SHA-256 key derivation. Both come from one
backend, chosen by feature:

- `aws-lc-crypto`, the default: AWS-LC, which needs a C compiler.
- `native-crypto`: RustCrypto, pure Rust, using AES-NI on x86-64 and the ARMv8
  crypto instructions on aarch64. It wins when both are on.

A build with no C at all:

```toml
sevenz-turbo = { version = "0.29", default-features = false, features = [
    "aes256", "native-crypto", "bzip2", "compress", "ppmd", "util",
] }
```

`aes256` with neither backend is a compile error rather than a silent
choice, and `sevenz_turbo::crypto_backend()` says which one a build uses.

## WebAssembly

The `default_wasm` feature set builds for `wasm32-unknown-unknown`, with
RustCrypto in place of AWS-LC:

```bash
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build --target wasm32-unknown-unknown --no-default-features --features=default_wasm
```

With `util`, the module exports `decompress`, and `compress` as well when
the `compress` feature is on.

The program that embeds the module can usually decrypt and checksum much
faster than code inside it. Two features hand that work over, on `wasm32`
only; on native targets they change nothing:

- `crypto-host`: AES-256 decryption and the key derivation's SHA-256.
- `crc-host`: every CRC-32 in the archive.

```bash
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build --target wasm32-unknown-unknown --no-default-features --features=aes256_wasm,crc-host,crypto-host,bzip2,ppmd
```

LZMA and LZMA2 still decode inside the module, and encrypting a new archive
still happens there too. The host installs its functions before opening an
archive, all through `sevenz_turbo::hooks`:

```rust,ignore
use sevenz_turbo::hooks::{
    HostAesError, HostCryptoHooks, HostHashHooks, install_host_crypto_hooks,
    install_host_hash_hooks,
};

fn aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError> {
    // forward to the embedder's AES-256-CBC
}

install_host_crypto_hooks(HostCryptoHooks { aes_cbc_decrypt });
install_host_hash_hooks(HostHashHooks::new(
    crc32, crc64_xz, sha256_init, sha256_clone, sha256_update, sha256_finalize, sha256_drop,
));
```

- `aes_cbc_decrypt` returns `data` decrypted with AES-256-CBC under `key`
  and `iv`, with no padding, as exactly `data.len()` bytes. `key` is 32
  bytes, `iv` 16, and `data` a whole number of 16-byte blocks, possibly none.
- Each call stands alone: this crate carries the IV from one call to the
  next.
- The hash functions follow `lzma-turbo`'s contract, in `lzma_turbo::hooks`.
  7z has no CRC-64, so `crc64_xz` is never called.
- A function that is missing, fails or returns the wrong length panics. There
  is no fallback inside the module.

[`examples/wasm_host_extract_conformance.rs`](examples/wasm_host_extract_conformance.rs)
is a complete guest, and [`tools/wasm-conformance`](tools/wasm-conformance) is
the host that runs it in CI.

## Acknowledgements

Almost none of this crate is ours, and the people it belongs to should be
named before the licence is.

- **Nils Hasenbanck** wrote and maintains
  [`sevenz-rust2`](https://github.com/hasenbanck/sevenz-rust2), which is
  where the code in this repository comes from: the archive reader and
  writer, the coders, the encryption, the tests and the examples all started
  as his. This fork is built on his work, and if you are not sure you need
  what it changes, his crate is the one to use.
- **dyz1990** wrote the original
  [`sevenz-rust`](https://github.com/dyz1990/sevenz-rust) that `sevenz-rust2`
  continued, and with it the first 7z implementation in pure Rust.
- **Igor Pavlov** designed the 7z format, LZMA and LZMA2, and wrote
  [7-Zip](https://www.7-zip.org/), whose public-domain code is what
  [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo) ports and whose
  `7zz` this crate's differential tests compare against.
- Everyone in [CONTRIBUTORS.md](CONTRIBUTORS.md), which lists the upstream
  authors by name.

## Licence

This crate is licensed under the
[Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0),
the same as upstream, and upstream's copyright notices are unchanged.
`lzma-turbo` is Apache-2.0 too, from its 0.7.0 release on; its earlier
releases were published under GPL-3.0-or-later.
