# Design rules

The rules this crate's code holds to, beyond the general ones in
`AGENTS.md`. Read this before changing a codec seam, the multi-threaded
reader, the header parser or its limits, the public API, or the test corpus.

## Divergence from sevenz-rust2

1. **Every change is ours to make.** There is no upstream to confine a diff
   for, no signature that is off limits, and no rebase that a reformat could
   break. Change what the work needs, where it lives.
2. **Bugs are fixed here.** A bug inherited from sevenz-rust2 is a bug in this
   crate; it is fixed and released here, whether or not the other crate has
   it too.
3. **Keep the `## Fork` section of `CHANGELOG.md` honest.** It is the record
   of how this crate differs from the commit it was taken at, kept for readers
   who know the other crate. A change that adds to that difference is noted
   there in the same commit; it is a record, not a checklist to reconcile.

## Codec rules

- LZMA and LZMA2 coding is `lzma-turbo`'s. The crate names `lzma_turbo::`
  only at these seams, and a new use goes in one of them or is added to this
  list with a reason:
  - `src/codec/lzma_turbo.rs` (decoding, and the multi-threaded LZMA2 coder;
    see `docs/lzma-turbo-requests.md`) and `src/codec/lzma_turbo/writer.rs`
    (encoding: the `Write` bridge over its pull-driven encoders);
  - `src/encoder.rs`, which turns the option types into the encoder's
    settings in `turbo_props` (`LzmaEncProps`, `MatchFinderKind`,
    `BLOCK_SIZE_SOLID`);
  - `src/decoder.rs`, which builds the single-threaded `LzmaReader`;
  - the filter wrappers in `src/codec/filter/` (below);
  - `src/crypto_backend.rs`, for SHA-256 (`lzma_turbo::crypto`);
  - `src/hooks.rs`, which re-exports `lzma_turbo::hooks`;
  - `src/container.rs`, which re-exports `lzma_turbo::crc::{crc32_combine,
    CrcFolder}` as public API;
  - `lzma_turbo::crc` wherever a CRC-32 is computed (`src/reader.rs`,
    `src/writer.rs`, `src/writer/source_reader.rs`, and
    `src/reader/store_overlap.rs`, whose stage thread checksums a stored
    folder's files where it reads or decrypts them; see the CRC rules
    below).

  The option types in `src/encoder_options.rs` name no encoder; what codes
  them is decided in `src/encoder.rs` alone.
- The BCJ and delta filters are `lzma-turbo`'s (`lzma_turbo::filters`, behind
  its `filters` feature); `src/codec/filter/bcj.rs` and `delta.rs` are handles
  on them, and `bcj2.rs` is this crate's `Read` and `Write` over
  `lzma_turbo::filters::bcj2`. The `Read` and `Write` wrappers in `bcj.rs`
  and `delta.rs` come from `lzma-rust2` 0.20.1 (Apache-2.0), itself a port of
  XZ for Java; each file's header records that origin and the changes made
  here (Apache-2.0 section 4(b)). Keep those notices when editing the files,
  and add to the list of changes.
- Crypto goes through `src/crypto_backend.rs` — SHA-256 *and* AES-256-CBC,
  decrypt and encrypt: `aws-lc-rs` by default, RustCrypto when the
  `native-crypto` feature is on. Never call a backend crate directly from
  anywhere else; the one exception is the test-only reference host hook in
  `src/hooks/aes.rs`, which stands in for an embedder's AES.
- CRC-32 is `crc-turbo` (via `lzma-turbo`'s `crc` module), never `crc32fast`.
- **No CRC-32 is computed in a serialised section of the multi-threaded path.**
  Checksumming is O(bytes) and the in-order section is the one place where the
  core count does not help, so a checksum taken there is a tax that grows with
  the archive. Checksums are computed where the bytes are produced — on the
  worker that decoded them — and folded with `crc32_combine`, which costs the
  same regardless of how long the pieces are. The single-threaded path is
  exempt: it *is* the calling thread, so there is no section to serialise
  against, and so is a block whose LZMA2 output passes through a filter (BCJ,
  delta, BCJ2) on the way out, because the bytes the workers checksummed are
  not the bytes the file is made of — that filter runs on the consuming thread
  and the checksum has to run there with it. Everywhere else, the block's file
  boundaries go to the coder as split points and `Crc32VerifyingReader` is not
  built at all: see `BlockDecoder::file_boundaries` and `Lzma2Control::folded`.
  Verification is not weakened by this — a corrupt block is still refused, and
  a test asserts it under the parallel path.
- Thread counts default to one, everywhere. A library does not decide on its
  own to occupy every core, or to hold the memory that doing so costs; the
  consumer asks.
  - One carve-out, matching 7-Zip: when a BCJ2 folder is coded alone, the
    LZMA coders of its call and jump streams run on threads of their own,
    as 7-Zip's `CMixerMT` runs one thread per non-main coder whatever `-mmt`
    says. `Bcj2Sides::Inline` is the equivalent of 7-Zip's `-mtf=off` and is
    what a thread-budgeted multi-folder encode uses. This is not a
    thread-count default; do not re-raise it as one.
- **The parallel LZMA2 reader feeds whole runs, and never stops the ring.** The
  decoder gives the run at its cursor to its chase path — single-threaded, on
  the calling thread, with dispatch switched off until that run is done —
  whenever the run's end has not arrived. So this crate walks the chunk headers
  itself and feeds only runs it has seen the end of, and it keeps the batch
  large enough that the one run the chase still takes at the end of a batch is
  overlapped rather than waited on. Both rules are load-bearing: dropping
  either cost 1.5x against a bare parallel decode at two threads. Anything
  changing `pump_input` or the feed constants must be measured at 2, 4 and 8
  threads, not only at the machine's full width, where the whole archive fits
  one batch and the bug is invisible. `SEVENZ_TURBO_MT_TRACE=1` prints the phase
  split — how much was chased, how long was spent feeding, how long draining —
  and is how that is checked.

## Reading hostile archives

- **No allocation and no unit of work is sized by a header field without a
  limit check first.** Every number in a 7z header is attacker-chosen. Before
  anything is reserved, walked, decoded or derived from one, it is bounded by
  the bytes the archive actually has *and* by the relevant [`ArchiveLimits`]
  field — a count is one byte and the entry it reserves is a hundred, so the
  byte bound alone is not enough. `Vec::with_capacity(claimed)` and
  `vec![x; claimed]` on an unchecked number are the shape of the bug; the
  parser's `HeaderBounds` is where the check goes.
- A new limit is a documented field of `ArchiveLimits` with a default a
  legitimate 1 GiB archive never reaches, an entry in the table in
  `docs/security.md`, a `Limit` variant so a consumer can say which bound
  stopped it, and a crafted archive in `tests/security_tests.rs`.
- The defaults must never change what a well-formed archive does. The
  differential matrix is what says so.

## Public API and repository layout

- Every public enum (`Error`, `Limit`, `BlockErrorKind`, `EncoderOptions`,
  `hooks::HostAesError`) is `#[non_exhaustive]` so that a new variant is not
  a breaking change; a new public enum is too.
- Never commit generated archive fixtures outside `tests/resources/` and
  `examples/data/`. Every tracked file in that differential corpus is capped
  at 1 MiB by the `hygiene` CI lane. This is a repository fixture rule, not
  a limit on archives the library can process.
- `tools/wasm-conformance` is the only place `wasmtime` is allowed: it is a
  workspace member, not a test of this crate, and every workspace-wide
  `cargo nextest run`/`cargo clippy` in CI and the release path excludes it. Do not
  add `wasmtime` (or any wasm host) to this crate's dev-dependencies.
