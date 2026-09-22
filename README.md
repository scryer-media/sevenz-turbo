# sevenz-turbo

[![ci](https://github.com/scryer-media/sevenz-turbo/actions/workflows/ci.yml/badge.svg)](https://github.com/scryer-media/sevenz-turbo/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sevenz-turbo.svg)](https://crates.io/crates/sevenz-turbo)
[![docs.rs](https://docs.rs/sevenz-turbo/badge.svg)](https://docs.rs/sevenz-turbo)

A 7z compressor/decompressor in pure Rust.

## This is a fork

`sevenz-turbo` is a fork of
**[sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2)** by Nils
Hasenbanck (Apache-2.0), taken at upstream `12ed7c8`, just after v0.22.2.
Nearly all of the code here is upstream's, and it stays that way: the public
API and the Rust module paths are upstream's, so migrating is
`sevenz_rust2::` → `sevenz_turbo::` plus whatever new calls you want.

Two things differ, and they are the whole reason the fork exists.

### 1. LZMA and LZMA2 decode with `lzma-turbo`

Upstream decodes LZMA/LZMA2 with
[`lzma-rust2`](https://github.com/hasenbanck/lzma-rust2), which is the fastest
pure-Rust LZMA decoder published but still about 1.3x slower than 7-Zip
single-threaded. This fork routes those two coders to
[`lzma-turbo`](https://github.com/scryer-media/lzma-turbo), a port of Igor
Pavlov's reference decoder (including the LZMA SDK's assembly loops) that is at
parity with `7zz` single-threaded. Archives are written with `lzma-turbo`'s
encoder too, a port of the SDK's, so `lzma-rust2` is not in the dependency
graph at all unless the `lzma-rust2-encoder` feature asks for its encoders
instead.

LZMA2 also decodes on several threads, by cutting the stream at the dictionary
resets that make a run independently decodable. **The default is one thread**,
unlike upstream, which defaults to `available_parallelism()`: a library should
not decide on its own to occupy every core, or to hold the memory that costs.

```rust
let mut reader = ArchiveReader::new(file, Password::empty())?.with_threads(8);
```

An archive written that way decodes about six times faster on eight threads
than on one, and within a fifth of what `7zz t` takes with every thread on the
same machine. An archive written `-mmt=1` is one run from beginning to end and
cannot be split at all, so a thread count above one neither helps it nor — as
of the read-ahead rule — hurts it: see [docs/benchmarking.md](docs/benchmarking.md).

The count can also be changed *while* a block is decoding, from the decoding
thread or from another one, through a handle taken before the decode starts:

```rust
let mut reader = ArchiveReader::new(file, Password::empty())?.with_adaptive_lzma2();
let lzma2 = reader.lzma2_handle();
// … on whatever thread is watching the download:
if let Some(progress) = lzma2.progress() {
    // `pending_runs` is the backlog of complete, not-yet-claimed runs, and
    // `runs_claimed` the run index of the block being decoded.
    lzma2.set_threads(if progress.pending_runs > 1 { 8 } else { 1 });
}
```

A change lands at the next run boundary, which is a dictionary reset, so
switching is lossless: a run decoded inline and the same run decoded on a
worker are the same decode, and the output bytes cannot tell you which
happened. `set_threads(1)` decodes inline on the calling thread, spawning
nothing.

A thread count above one is a request, not a promise. A stream with no
dictionary resets — what `7zz -mmt=1` writes — decodes single-threaded because
there is nothing to cut, and so does a block whose memory budget has no room
for runs in flight: a limit says what the caller can afford, not that the
archive must be refused. Measured numbers live in
[docs/benchmarking.md](docs/benchmarking.md).

Parallel decoding buys its speed with memory, and this crate spends more of it
than the runs in flight alone would need: reading a long way ahead is what
keeps the decoder from finishing a run on the delivering thread, which is worth
about 1.5x at low thread counts and is explained in
[docs/lzma-turbo-requests.md](docs/lzma-turbo-requests.md). Decoding a 900 MiB
block peaks around 2.5 GiB. A caller who would rather have the memory than the
speed says so with `ArchiveLimits::memory`, which bounds the read-ahead along
with everything else; a caller who wants neither leaves the thread count at
one, where a block costs its dictionary and nothing else.

### 2. Container API a streaming consumer needs

A consumer that feeds this reader from a download, under a memory budget it has
to honour before it allocates, needs to ask the archive a few things first:

- `ArchiveReader::with_limits` refuses an archive over an `ArchiveLimits`
  before the allocation it bounds — the declared end-header size before the
  header is buffered, and `Archive::decoder_memory_estimate()` before a
  decoder is built.
- `Archive::block_pack_streams(block)` gives absolute `(offset, size)` ranges,
  `Archive::block_sub_streams(block)` the per-entry sizes and CRC-32s.
- `ArchiveReader::block_decoder(block)` borrows the reader rather than
  consuming it, so the header is parsed once and blocks are decoded from the
  same source.
- `ArchiveReader::set_block_complete_hook` reports each block once it is
  decoded and verified, and `set_sub_stream_complete_hook` reports each *file*
  as its CRC-32 becomes final, with the value — the decoder computed it to
  check the header, so a consumer reporting per-file integrity never has to
  read the bytes a second time.
- `crc32_combine(a, b, len_b)` folds two checksums into the checksum of the
  two pieces joined, and `CrcFolder` folds a heap of `(offset, len, crc32)`
  pieces into any range they cover — for a consumer stitching across
  boundaries this crate does not know about, such as across blocks. Both are
  re-exported from `lzma-turbo`, so a consumer folds with the same
  implementation the decoder's workers checksummed with.

### Where the checksums come from

When a block's coder is LZMA2 and it is decoding in parallel, no CRC-32 is
computed on the thread delivering the bytes. The block's file boundaries go to
the decoder as split points, each worker checksums the pieces of the block it
produced before handing it on, and this crate folds those pieces into each
file's CRC-32 and compares it with the header. Verification is unchanged — a
corrupt block is refused exactly as before — but the cost has moved off the
one section that does not get faster with more cores.

Two cases keep the streaming checksum, because for them it is the right place:
a block decoded single-threaded, where the consuming thread is the decoding
thread; and a block whose LZMA2 output passes through a filter (BCJ, delta,
BCJ2), where the bytes a worker saw are not the bytes the file is made of and
the filter runs on the consuming thread anyway.
- `Error::BlockDecode` names the block and the packed offset for a corrupt
  archive, distinctly from I/O and unsupported-method failures.

Added rather than altered so far — every sevenz-rust2 signature still means
what it did, which keeps the migration a rename. See
[CHANGELOG.md](CHANGELOG.md), section `## Fork`, for the record of
divergences, and [AGENTS.md](AGENTS.md) for the rules of a crate that has
left its origin behind.

### Encoders

Writing archives (`compress`) encodes LZMA and LZMA2 with `lzma-turbo`'s port
of the SDK encoder. The alternative is `lzma-rust2`'s pure-Rust encoders,
behind the `lzma-rust2-encoder` feature (off by default), which is what every
version before 0.25.0 used; the option types and the archives are the same
either way, only the compressed bytes differ. A compression level means the
same dictionary under both: 256 KiB at level 0, doubling to 64 MiB at level 9.

### Crypto backends

The 7z `aes256` coder needs AES-256-CBC and SHA-256, and **both** follow the
backend feature. The default is `aws-lc-rs` — `DecryptingKey::cbc`, AWS-LC's
unpadded CBC mode, plus its SHA-256. Enabling `native-crypto` switches both to
RustCrypto (`aes`/`cbc` and `sha2`) and takes precedence, so a consumer that
cannot build C can use `default-features = false` with `aes256, native-crypto`;
that lane compiles to AES-NI on x86-64 and to the ARMv8 cryptography extensions
on aarch64. Decrypting a 7z stream in pieces needs no streaming API on either
lane: each chunk is decrypted with the current IV and its last ciphertext block
becomes the next chunk's. Writing archives (`compress`) keeps RustCrypto's
`cbc::Encryptor`. CRC-32 is `crc-fast`.

Because Cargo features are additive, `native-crypto` cannot mean "turn AWS-LC
off"; it means "win when both are compiled". So `aes256` does not pull a
backend in by itself: with `default-features = false` you pick one explicitly,
and asking for `aes256` with neither is a compile error rather than a silent
choice about what cryptography is in your binary. `sevenz_turbo::crypto_backend()`
reports which one a build selected.

## Speed

Two 7z archives of the same 1 GiB, decoded and CRC-checked in full, on
Linux x86_64 (Intel Arrow Lake-H, 16 threads), median of three runs.
`mt.7z` was written by `7zz -mmt=on`, so its LZMA2 stream can be decoded in
parallel; `st.7z` by `7zz -mmt=1`, so it cannot, by anyone.

| decoder | `mt.7z` | `st.7z` |
| --- | --- | --- |
| `7zz t`, all threads | 3.86 s | 20.1 s |
| `7zz t -mmt=1` | 20.2 s | 20.2 s |
| `sevenz-rust2` 0.22.2 | 25.6 s | 28.7 s |
| `sevenz-turbo`, 1 thread | 19.9 s | 20.0 s |
| `sevenz-turbo`, 8 threads | 4.59 s | 20.0 s |

On an Apple M5 Max the same `mt.7z` takes 2.28 s at 8 threads against
2.13 s for `7zz`.

Encrypted archives are where the fork is clearly ahead. The same 1 GiB,
stored (`-mx0`) and AES-256 encrypted, so that decrypting it is nearly all
of the work:

| `aes_store.7z` | `sevenz-turbo` | `7zz t` | `sevenz-rust2` |
| --- | --- | --- | --- |
| Linux x86_64 | 0.28 s | 0.63 s | 0.94 s |
| Windows x86_64 | 0.48 s | 0.65 s | 3.46 s |
| macOS arm64 | 0.18 s | 0.23 s | 0.97 s |

The AES decoder reads the packed stream a megabyte at a time and decrypts
it in the caller's buffer; measured layer by layer, the cipher, the file read
and the CRC add up to the whole decode, with nothing left in the plumbing.
What each phase of a decode costs, and the rest of the fixtures, are in
[docs/benchmarking.md](docs/benchmarking.md).

## Usage

```toml
[dependencies]
sevenz-turbo = "0.26"
```

Decompress "data/sample.7z" to "data/sample":

```rust
sevenz_turbo::decompress_file("data/sample.7z", "data/sample").expect("complete");
```

### Decompress an encrypted 7z file

```rust
sevenz_turbo::decompress_file_with_password("path/to/encrypted.7z", "data/sample", "password".into()).expect("complete");
```

### Archives from strangers

Every number in a 7z header is chosen by whoever wrote the file, and this crate
bounds each one before the allocation or the work it sizes — counts, names,
dictionaries, nesting, the key-derivation factor, the declared output. The
defaults are in force whether or not a caller passes limits, and no archive a
mainstream 7-Zip writes reaches them.

```rust
use sevenz_turbo::{ArchiveLimits, ArchiveReader, Password};

let limits = ArchiveLimits::memory(512 << 20)   // what a decode may allocate
    .with_max_unpack_bytes(8 << 30)             // refuse a bomb at open
    .rejecting_unsafe_paths();                  // and a name that would escape
let reader = ArchiveReader::with_limits(file, Password::empty(), limits)?;
```

`Error::LimitExceeded { what, limit, requested }` says which bound stopped a
read, so a consumer can report "this archive wants more memory than we give it"
rather than "corrupt archive". Per entry, `ArchiveEntry::is_unsafe_path()` and
`is_symlink()` say what a name would do before anything is written.

[`docs/security.md`](docs/security.md) is the threat model, every limit with its
default and rationale, and what happens when each is hit.

## Compression

```rust
sevenz_turbo::compress_to_path("examples/data/sample", "examples/data/sample.7z").expect("compress ok");
```

### Compress with AES encryption

```rust
sevenz_turbo::compress_to_path_encrypted("examples/data/sample", "examples/data/sample.7z", "password".into()).expect("compress ok");
```

### Advanced usage

#### Solid compression

Solid archives can compress better, but decompressing one file needs all the
data in front of it decompressed too.

```rust
use sevenz_turbo::*;

let mut writer = ArchiveWriter::create("dest.7z").expect("create writer ok");
writer.push_source_path("path/to/compress", |_| true).expect("pack ok");
writer.finish().expect("compress ok");
```

#### Configure the compression methods

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

### Supported codecs and filters

| Codec       | Decompression | Compression |
|-------------|---------------|-------------|
| COPY        | ✓            | ✓          |
| LZMA        | ✓ (lzma-turbo) | ✓          |
| LZMA2       | ✓ (lzma-turbo) | ✓          |
| BROTLI (*)  | ✓            | ✓          |
| BZIP2       | ✓            | ✓          |
| DEFLATE (*) | ✓            | ✓          |
| PPMD        | ✓            | ✓          |
| LZ4 (*)     | ✓            | ✓          |
| ZSTD (*)    | ✓            | ✓          |

(*) Require optional cargo feature.

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
| BCJ2          | ✓            |             |
| DELTA         | ✓            | ✓          |

### WASM support

WASM is supported, but not with the default features: `aws-lc-rs` does not
build for `wasm32`, so the WASM feature set uses the RustCrypto backend.

```bash
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build --target wasm32-unknown-unknown --no-default-features --features=default_wasm
```

The `util` feature's `wasm-bindgen` exports follow the feature set: `decompress`
is there whenever `util` is, and `compress` only when the `compress` feature is
on as well, so a decode-only guest builds without the writer half of the crate.

#### Letting the host do the AES (`crypto-host`)

A wasm guest has neither AES-NI nor the ARMv8 cryptography extensions, so the
block cipher is the one part of decoding an encrypted 7z archive that the
embedding program can do several times faster than the guest. The `crypto-host`
feature moves it there: on a `wasm32` target the bulk AES-256-CBC **decrypt**
leaves the guest through a plain `fn` pointer the embedder installs.

```bash
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' cargo build --target wasm32-unknown-unknown --no-default-features --features=aes256_wasm,crypto-host,bzip2,ppmd
```

Nothing else changes: the SHA-256 key derivation, the LZMA/LZMA2 decode and the
CRCs stay in the guest, the public API is untouched, and the encoder keeps its
own in-guest AES. The feature adds no AES dependency at all — a delegating build
that does not also ask for `compress` carries no block cipher of its own.

The embedder installs one hook before opening an encrypted archive:

```rust,ignore
use sevenz_turbo::hooks::{HostAesError, HostCryptoHooks, install_host_crypto_hooks};

fn aes_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, HostAesError> {
    // forward to the embedder's AES-256-CBC (a raw wasm import, a component
    // import, a host SDK call — `sevenz-turbo` never sees which)
}

install_host_crypto_hooks(HostCryptoHooks { aes_cbc_decrypt });
```

The contract the hook must satisfy:

- It returns the AES-256-CBC decryption of `data` under `key`/`iv`, **no
  padding**, as a fresh buffer of exactly `data.len()` bytes.
- `key` is 32 bytes, `iv` is 16, `data` is a whole number of 16-byte blocks and
  may be empty.
- It is **stateless per call**: this crate threads the CBC IV across chunks
  itself, so a host never carries cipher state between calls.
- A hook that errors, answers with the wrong length, or was never installed is
  an embedder contract violation and panics. There is no silent in-guest
  fallback, because a fallback would quietly undo the delegation.

`examples/wasm_host_extract_conformance.rs` is a complete reference embedding —
a `wasm32-wasip1` guest that declares one raw import in a `host` namespace and
forwards the hook to it — and `tools/wasm-conformance` is the native `wasmtime`
harness that runs it: it writes an encrypted fixture archive,
extracts it inside the guest through a reference host AES, and asserts the
guest's bytes equal the native decoder's. `wasmtime` is a dev-dependency of that
harness only and never enters the crate's dependency graph.

On native targets `crypto-host` is accepted but inert: AWS-LC or RustCrypto
stays selected and no hook is ever called, so feature unification in a mixed
workspace cannot turn a native build into a delegating one.

## Acknowledgements

Almost none of this crate is ours, and the people it belongs to should be
named before the licence is.

- **Nils Hasenbanck** wrote and maintains
  [`sevenz-rust2`](https://github.com/hasenbanck/sevenz-rust2), which is the
  code in this repository: the archive reader and writer, the coders, the
  encryption, the tests and the examples. He also wrote
  [`lzma-rust2`](https://github.com/hasenbanck/lzma-rust2), whose encoders
  are the `lzma-rust2-encoder` alternative. This fork is his work with two
  changes bolted on, and if you are not sure you need those changes, his
  crate is the one to use.
- **dyz1990** wrote the original
  [`sevenz-rust`](https://github.com/dyz1990/sevenz-rust) that `sevenz-rust2`
  continued, and with it the first 7z implementation in pure Rust.
- **Igor Pavlov** designed the 7z format, LZMA and LZMA2, and wrote
  [7-Zip](https://www.7-zip.org/), whose public-domain decoders are what
  [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo) ports and whose
  `7zz` is the oracle every differential test in this crate is checked
  against.
- Everyone in [CONTRIBUTORS.md](CONTRIBUTORS.md), which lists the upstream
  authors by name.

## Licence

This crate is licensed under the
[Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0),
the same as upstream, and upstream's copyright notices are unchanged.

Note that `lzma-turbo`, which this crate depends on for LZMA/LZMA2 decoding, is
licensed GPL-3.0-or-later. This crate's own source stays Apache-2.0, but a
binary that links it together with `lzma-turbo` is a combined work under the
GPL. If that is a problem for you, upstream `sevenz-rust2` is the crate you
want.
