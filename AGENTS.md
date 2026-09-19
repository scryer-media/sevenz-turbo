# Agent and contributor rules for sevenz-turbo

These rules apply to every automated agent and every human contributor.

## What this repository is

`sevenz-turbo` began as a fork of
[sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2) by hasenbanck,
Apache-2.0, taken at its commit `12ed7c8` (post-v0.22.2). It is a hard fork:
it is not rebased onto sevenz-rust2, nothing is sent back there, and the two
have diverged for good. The origin is recorded for licensing and for readers
arriving from the other crate, not as a constraint. The fork was made for two
reasons:

1. **Codec swap.** LZMA (`03 01 01`) and LZMA2 (`21`) decode through
   [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo), a port of Igor
   Pavlov's reference decoder, instead of `lzma-rust2`, and are encoded by
   its port of the SDK encoder when archives are written. `lzma-rust2` is not
   in the dependency graph unless the non-default `lzma-rust2-encoder`
   feature asks for its encoders instead.
2. **Container API.** The 7z detail a streaming consumer needs and upstream
   does not expose: memory limits enforced before allocation, per-member CRCs,
   folder-to-pack-stream byte ranges, a reader that parses once and decodes
   from a caller-supplied `Read + Seek`, typed corruption errors carrying a
   block index and packed offset, and a per-block completion hook.

The Rust module paths and the public API started as sevenz-rust2's, so a
consumer's migration is `sevenz_rust2::` → `sevenz_turbo::` plus the new
calls. That compatibility is a courtesy this crate keeps while it costs
nothing, not a rule: an API that the container work needs to change, changes,
with a version bump and a changelog entry like any other.

## Divergence rules

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

- LZMA and LZMA2 coding is `lzma-turbo`'s, reached only through
  `src/codec/lzma_turbo.rs` (decoding, and the multi-threaded LZMA2 coder;
  see `docs/lzma-turbo-requests.md`) and `src/codec/lzma_turbo/writer.rs`
  (encoding: the `Write` bridge over its pull-driven encoders). Nothing else
  in the crate names `lzma_turbo::` except the filter wrappers below. The
  `lzma-rust2-encoder` feature swaps the encoders for `lzma-rust2`'s; the
  option types in `src/encoder_options.rs` are encoder-agnostic so that the
  swap is confined to `src/encoder.rs`.
- The BCJ and delta filters are `lzma-turbo`'s (`lzma_turbo::filters`, behind
  its `filters` feature); `src/codec/filter/bcj.rs` and `delta.rs` are handles
  on them, and `bcj2.rs` is this crate's `Read` over `lzma_turbo::filters::bcj2`.
  No conversion is vendored from `lzma-rust2` any more; what is left of it
  under `src/codec/filter/` is the readers and writers of `bcj` and `delta`.
- Crypto goes through `src/crypto_backend.rs` — SHA-256 *and* AES-256-CBC:
  `aws-lc-rs` by default, RustCrypto when the `native-crypto` feature is on.
  Never call a backend crate directly from anywhere else. The one exception is
  the encoder's `cbc::Encryptor` in `src/encryption/aes.rs`, behind `compress`.
- CRC-32 is `crc-fast` (via `lzma-turbo`'s `crc` module), never `crc32fast`.
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

## Repository hygiene

- Commits are SSH-signed. Never pass `--no-gpg-sign`.
- Never run destructive git working-tree operations (`checkout --`,
  `restore`, `reset --hard`, `clean`, `stash`) in a shared checkout.
- Branch names use gitflow prefixes: `feature/…`, `bugfix/…`, `hotfix/…`.
- Do not push. The maintainer pushes.
- The pre-commit hook (`.githooks/pre-commit`) rejects home paths, the local
  username and secrets. Keep it enabled: `git config core.hooksPath .githooks`.
- Never commit generated archives. The small archives under `tests/resources/`
  and `examples/data/` are the tracked differential corpus; anything larger
  than 1 MiB, or anywhere else, is rejected by the `hygiene` CI lane.
- `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`
  and `cargo test --all-features` must pass before a commit is proposed.
- Any code change bumps the crate version, `Cargo.lock` and `CHANGELOG.md` in
  the same change.
