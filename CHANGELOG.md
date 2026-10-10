# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Fork

`sevenz-turbo` began as a fork of
[sevenz-rust2](https://github.com/hasenbanck/sevenz-rust2), taken at its
commit `12ed7c8` (post-v0.22.2). A permanent fork was not the plan: the codec
swap and the container API were offered to sevenz-rust2 for merging, so that
one crate could carry both, and the offer was declined - see
[hasenbanck/sevenz-rust2#144](https://github.com/hasenbanck/sevenz-rust2/issues/144).
That left a hard fork as the only way to ship the work, and the two crates
have diverged for good since: this one is not rebased onto sevenz-rust2 and
nothing goes back. This section is the record of how it differs from the
commit it was taken at, kept for readers who know the other crate.
sevenz-rust2's own changelog up to the fork point continues below, unchanged.

### Packaging

- Released by `.github/workflows/release.yml`, dispatched from the Actions tab
  on `main` (it creates the tag) or started by the signed `v<version>` tag
  `cargo xtask release` pushes; crates.io trusted publishing either way. See
  `docs/publishing.md`. The crate archive no longer carries the repository's
  CI, hook and agent files.

- The fork was briefly on crates.io as `sevenz-fast`, with the same contents as
  0.23.0; that name is withdrawn, along with `lzma-fast` for
  `lzma-turbo`.
- Crate renamed to `sevenz-turbo`; the Rust module paths and the public API stay
  upstream's, so a consumer's migration is `sevenz_rust2::` → `sevenz_turbo::`.
- Version starts at `0.23.0`, one minor above the upstream base, to make the
  lineage obvious. The `0.23.0` entries below were unreleased upstream at the fork
  point; they are part of the fork base and ship with it.
- MSRV raised from 1.93 to 1.97.1, which `lzma-turbo` requires. Pinned in
  `rust-toolchain.toml`.
- `Cargo.lock` is committed (upstream ignores it) so CI can run `--locked` and
  `cargo audit` has something to audit.
- Repository hygiene adopted from the scryer-media house style: SHA-pinned CI
  (`fmt`, `clippy -D warnings`, four-platform tests, MSRV, docs, package),
  `security` (cargo-audit, zizmor), `codeql` + scorecard, gitleaks pre-commit
  hooks, renovate, issue templates, `AGENTS.md`, `SECURITY.md`,
  `CONTRIBUTORS.md`. Upstream's `.github/workflows/rust.yml` and
  `.github/dependabot.yml` were removed as duplicates of these.
- The `lzma-turbo` dependency is a crates.io version pin. While the two
  crates are developed together a `path` to the sibling checkout is added and
  dropped again before release; `cargo xtask release` refuses to tag while it
  is there.

### Decoding

- LZMA (`0x030101`) and LZMA2 (`0x21`) are decoded by
  [`lzma-turbo`](https://github.com/scryer-media/lzma-turbo) instead of
  `lzma-rust2`. On the fixtures in `docs/benchmarking.md` this is 1.62x
  upstream's throughput and level with `7zz t -mmt=1`, where upstream was 1.3x
  behind it.
- The parallel LZMA2 reader decodes no more than the caller's buffer holds
  per read. It used to drain everything the decoder had ready — at eight
  threads, up to a whole run per worker — spill the excess and copy it a
  second time on the way out. Measured on x86 at eight threads, a gigabyte
  went from 4.82 s to 4.59 s. Needs lzma-turbo 0.3.0 for `drain_upto`.
- `lzma-rust2` has left the dependency graph. Since 0.25.0 archives are
  written with `lzma-turbo`'s encoder as well, and `lzma-rust2` is reached
  only through the non-default `lzma-rust2-encoder` feature; with
  `--no-default-features` the graph is `sevenz-turbo → lzma-turbo → crc-fast`
  and nothing else.
- The branch converters, the delta filter and BCJ2 are all `lzma-turbo`'s
  (`lzma_turbo::filters`). The `Read` and `Write` wrappers around the BCJ and
  delta converters are vendored from `lzma-rust2` 0.20.1 (Apache-2.0, same
  licence) in `src/codec/filter/`; the BCJ2 reader and writer are this crate's
  own. Together that is what let `lzma-rust2` leave the decode graph rather
  than be carried for three filters. `src/codec/filter/mod.rs` documents what
  is still vendored and the mechanical changes made to it.
- PPMd decodes and encodes through
  [`ppmd-turbo`](https://github.com/scryer-media/ppmd-turbo) instead of
  `ppmd-rust` (0.27.0). The decoder is given the folder's unpacked size, a
  stream cut short is `UnexpectedEof`, a corrupt one `InvalidData`, and a
  model larger than the memory limit is refused before it is allocated.
- LZMA2 decodes on several threads through `lzma-turbo`'s `Lzma2AdaptiveDecoder`
  — a stream is cut at the dictionary resets that make a *run* independently
  decodable, and runs are decoded on workers while output stays in order.
  Upstream's `Decoder::Lzma2Mt` (`lzma-rust2`'s `Lzma2ReaderMt`) is gone;
  `Lzma2Plan` in `src/codec/lzma_turbo.rs` is the one place that chooses, and
  the rest of the decode chain, the CRC verification and the completion hook
  are unchanged by the choice.
- **The thread count now defaults to one**, where upstream defaults to
  `available_parallelism()`. A library does not decide on its own to occupy
  every core, or to hold the memory that doing so costs; the consumer asks with
  `set_threads`/`with_threads`. Existing callers that set a thread count
  explicitly are unaffected; callers that set none get today's memory and
  today's behaviour.
- **No CRC-32 is computed on the thread delivering the bytes when LZMA2 is
  decoding in parallel.** The block's file boundaries go to the coder as
  checksum split points, each worker checksums the pieces of the block it
  produced before it queues to hand that block on, and this crate folds those
  pieces into each file's CRC-32 — and into the block's — with
  `crc32_combine`, which costs the same whatever the pieces weigh. The
  verifying reader is then not built at all. What is verified, and when a
  corrupt archive is refused, does not change; a test asserts the refusal
  happens under the parallel path, where the streaming check is gone.
  Single-threaded blocks, and blocks whose LZMA2 output passes through a
  filter (BCJ, delta, BCJ2) on its way out, keep the streaming checksum: for
  the first the consuming thread is the decoding thread, and for the second
  the bytes the workers saw are not the bytes the file is made of.
- The reader walks the LZMA2 chunk headers itself and feeds the decoder **whole
  runs only**, a gigabyte or a run per thread ahead, whichever is more. Three
  things had to be true at once. A run reaches a worker only once it has
  arrived whole, so feeding a chunk at a time leaves nothing to dispatch and
  decodes everything inline. Feeding without limit decodes the whole block into
  memory before the caller sees a byte, because one `drain` decodes everything
  the fed bytes allow. And a run whose end the decoder has not seen is taken by
  its chase decoder, which finishes it on the calling thread with dispatch
  switched off — so a batch must be big enough that the one run this costs at
  its end is overlapped by several rounds of worker work, not comparable to it.
  Feeding "a run per thread, then the rest" cost 1.50x against the bare
  parallel decoder at two threads; feeding whole runs a gigabyte at a time is
  within 2% of it at every thread count measured. `docs/benchmarking.md` has
  the numbers and the memory that buys them, and
  `docs/lzma-turbo-requests.md` the upstream change that would make the
  gigabyte unnecessary.
- And when a good look at a stream has found no run boundary at all — what
  `7zz -mmt=1` writes is one run from beginning to end — the reader stops
  getting ahead altogether and lets the decoder stream it, rather than
  buffering an entire archive to hand to a single worker at the end. That is
  decided from the boundaries the reader has scanned, not from whether a worker
  has appeared, so it holds however large the read-ahead is.
- A thread count above one is a request, not a promise. A stream with no
  dictionary resets (what `7zz -mmt=1` writes) decodes single-threaded because
  there is nothing to cut, and a block whose memory budget has no room to hold
  runs in flight **degrades to single-threaded rather than failing** — a limit
  states what the caller can afford, not that the archive must be refused.
- The folders of a non-solid archive decode several at a time (0.27.0): with
  a positional source (`ReadAt`) and more than one thread, runs of folders of
  at most 8 MiB decode on workers and reach the callback in archive order,
  inside the thread count and the memory limit. A block whose heavy coders
  would share one thread decodes as a pipeline of stages on threads of their
  own. Upstream decodes every folder and every coder chain on the calling
  thread.
- LZMA1 is now subject to the same dictionary memory limit as LZMA2. Upstream
  bounded only LZMA2, so an archive declaring a 4 GiB LZMA1 dictionary would
  try to allocate it.
- CRC-32 comes from `crc-fast` (via `lzma-turbo`) rather than `crc32fast`; the
  `crc32fast` dependency is gone.
- The LZMA coder's properties are length-checked before being sliced, instead
  of panicking on a short field.

### Safety against hostile archives

Upstream trusts the numbers in a 7z header. This fork bounds every one of them
before the allocation or the work it sizes, against both the bytes the archive
actually has and the caller's `ArchiveLimits`. The model, the table of limits
and the audit are in `docs/security.md`; in summary:

- Header counts — files, blocks, coders, pack streams, sub-streams, bind pairs
  — no longer reserve on a claim. Neither does a name, a names blob, a coder
  properties field or a compressed header's declared unpacked size.
- A compressed header that decodes to another one is refused rather than
  followed, and a coder graph is validated before anything is built from it:
  indices in range, no stream bound or packed twice, exactly one unbound
  output, and no cycle.
- A coder's stream counts are bounded (`max_streams_per_coder`), which is what
  makes the graph's linear searches constant work instead of quadratic.
- Pack streams must end inside the file, so a header cannot aim a decode at an
  arbitrary offset.
- LZMA and LZMA2 dictionaries are clamped to the coder's declared unpacked
  size, the way 7-Zip reduces them: a 4 GiB dictionary on a 1 KiB stream is
  memory that would be allocated and never read. An archive that upstream
  would refuse for its budget can now decode.
- zstd's declared window is bounded by the caller's budget, and otherwise by
  the 128 MiB the reference decoder itself will not exceed.
- The AES key-derivation work factor is the caller's `max_aes_cycles_power`,
  never above 24 — the header field is six bits, so 2^63 rounds is otherwise
  expressible from a file a caller merely opened.
- The declared output and output-to-packed ratio can be bounded
  (`max_unpack_bytes`, `max_unpack_ratio`), refusing a bomb before a byte is
  decoded.
- Entry names are reported as unsafe (`ArchiveEntry::is_unsafe_path`) or
  refused outright (`ArchiveLimits::reject_unsafe_paths`), and symlink entries
  are surfaced rather than silently written as small text files.
- `fuzz/` has libFuzzer targets on the header parser and the coder graph,
  running under a counting allocator that fails a run that allocates more than
  the limits allow.

None of this changes what a well-formed archive does: the defaults are one to
two orders of magnitude above the largest legitimate values, and the
differential matrix against `7zz` stays green.

### Container API (additions only)

Everything here is new surface; no upstream signature changed meaning.

- `ArchiveLimits`, with `ArchiveReader::with_limits`,
  `Archive::read_with_limits` and `BlockDecoder::with_limits`: one model for
  what an archive is allowed to claim, checked *before* the allocation or the
  work it bounds. Two affordability limits — `memory_limit_bytes` (bounding
  `Archive::decoder_memory_estimate` and then each coder chain, summed,
  before it is built, the encoded header's included) and
  `max_end_header_bytes` (before the header is buffered) — and the structural
  bounds `max_header_unpacked_bytes`, `max_header_depth`, `max_entries`,
  `max_name_bytes`, `max_total_name_bytes`, `max_coders_per_block`,
  `max_streams_per_coder`, `max_total_coders`, `max_unpack_bytes`,
  `max_unpack_ratio`, `max_aes_cycles_power` and `reject_unsafe_paths`. Every
  field documents the attack it closes and defaults to a value no legitimate
  archive reaches; `ArchiveLimits::unlimited()` removes the structural ones for
  a caller reading archives it wrote itself. `docs/security.md` is the table.
- `Error::LimitExceeded { what: Limit, limit, requested }`, so a consumer can
  report which bound stopped a read rather than "corrupt archive", with
  `Limit::field()` naming the `ArchiveLimits` field and `Error::limit_hit()`
  mapping every way a limit is reported — including `EndHeaderTooLarge` and
  `MemoryLimited`, which predate it — onto the one enum.
- `ArchiveEntry::is_unsafe_path()` / `unsafe_path_reason()`, `is_symlink()`
  and `unix_mode()`: whether a stored name would escape an extraction
  directory and which way, and whether the entry is a link whose content is a
  target rather than a file. `Error::UnsafeEntryName` is what
  `reject_unsafe_paths` raises.
- `Archive::decoder_memory_estimate() -> Result<u64, UnsizedCoder>` and
  `coder_memory_estimate(&Coder)`: what a single-threaded decode of the archive
  needs, as the largest block's coder chain. The per-coder table is in the
  rustdoc.
- Read-only views of what the header already parsed:
  `Archive::num_unpack_sub_streams()`, `Archive::sub_stream(index)`,
  `Archive::block_sub_streams(block)` (per-entry size and CRC-32),
  `Archive::block_pack_streams(block)` (absolute `(offset, size)` ranges, four
  of them for a BCJ2 block) and `Archive::block_coders(block)`.
- `ArchiveReader::block_decoder(block_index)` borrows the reader instead of
  consuming it, so a consumer parses the header once and decodes blocks from
  the same source; `source_mut()` and `into_source()` complete that. This is
  what replaces opening the archive twice because the constructor took
  ownership.
- `ArchiveReader::set_block_complete_hook` / `clear_block_complete_hook`,
  called with a `BlockCompletion { block_index, unpacked_size, crc_verified }`
  once a block has been decoded in full and its checksum verified. A block the
  caller stopped short of is not reported.
- `ArchiveReader::set_threads` / `with_threads` / `threads` (upstream's
  `set_thread_count` still works and forwards), and
  `set_adaptive_lzma2` / `with_adaptive_lzma2`, which builds a block's LZMA2
  coder so it can widen later even while the count is one. `BlockDecoder` has
  the same four.
- `ArchiveReader::lzma2_handle() -> Lzma2Handle`: an owned, `Send + Sync`
  handle taken before a decode starts and used while it runs — a decode borrows
  the reader for its duration, so the live knob cannot be a method on it.
  `Lzma2Handle::set_threads(n)` takes effect at the next LZMA2 run boundary,
  which is a dictionary reset and therefore lossless; `1` decodes the next run
  inline on the calling thread, spawning nothing. `Lzma2Handle::progress()` and
  `ArchiveReader::lzma2_progress()` report an `Lzma2Progress { block_index,
  threads, spawned_threads, pending_runs, runs_claimed, in_flight_bytes }` —
  `pending_runs` is the backlog of complete runs an adaptive caller widens on,
  and `runs_claimed` is the run index of the block being decoded. While a run
  of small folders decodes several at a time (0.27.0), `set_threads` bounds
  how many of them decode at once and `progress()` is `None`.
- `ArchiveReader::set_sub_stream_complete_hook` / `clear_…`, called with a
  `SubStreamCompletion { block_index, sub_stream_index, file_index,
  unpacked_offset, len, crc32 }` as each file's checksum becomes final. The
  decoder computes that checksum anyway, to check it against the header; this
  hands the value over instead of discarding it, so a consumer reporting
  per-file integrity never reads the bytes a second time.
- `ArchiveReader::set_verify_checksums` / `with_verify_checksums` (and
  `BlockDecoder::with_verify_checksums`) turn the header's CRC-32 checks off
  for a consumer that verifies the bytes by other means — a PAR2 set over the
  extracted files, say — and does not want to pay for the same assurance
  twice. On by default. With it off a corrupt archive decodes into corrupt
  bytes without complaint, which is why it is spelled out rather than implied
  by a thread count or a limit.
- `crc32_combine(a, b, len_b)` and `CrcFolder`, re-exported from `lzma-turbo`:
  the checksum of two pieces joined, and a heap of `(offset, len, crc32)`
  pieces folded into any range they cover, for a consumer folding across
  boundaries this crate does not know about, such as across blocks. Re-exported
  rather than reimplemented, so a consumer folds with the same implementation
  the workers checksummed with.
- `Error::BlockDecode { block_index, packed_offset, kind, message }` with
  `BlockErrorKind::{Corrupted, ChecksumMismatch, UnsupportedMethod, Io,
  Password}`: corruption now says which block and which byte range, distinctly
  from an I/O failure on the source or a method this build cannot decode. An
  error raised by the *caller's* own callback is passed through untouched — the
  decode chain is wrapped so the two can be told apart.

### Documentation

- `docs/benchmarking.md` — the harness, the acceptance gate and the numbers.
- `docs/lzma-turbo-requests.md` — the API this crate would like from
  `lzma-turbo`, with the exact signatures and the local work-around for each.
  The parallel-decoder request landed and is recorded as such; the AES and
  key-derivation requests were withdrawn when that crate removed both on
  purpose; worker-side checksums landed and this fork folds them; what is
  outstanding is a memory limit the parallel decoder holds to, or an account
  of what it does not hold to, and a run index over a stream not yet being
  decoded.
- `AGENTS.md` gained the rule this fork is now held to: no CRC-32 is computed
  in a serialised section of the multi-threaded path, and thread counts
  default to one.

### Encoding

- BCJ2 is written as well as read (0.27.0). Upstream reads BCJ2 folders but
  cannot write one.
- `ArchiveWriter::push_archive_entries_non_solid` codes a non-solid archive's
  folders on several workers and writes them in order (0.27.0), the bytes a
  loop of `push_archive_entry` writes.
- An LZMA2 thread count is divided between block threads and each block
  coder's match-finder thread, as 7-Zip's `Lzma2EncProps_Normalize` divides it
  (0.27.0): where a level runs the match finder on a thread of its own,
  `threads` buys `threads / 2` block coders.
- The writer sizes each folder's LZMA or LZMA2 coder to the folder, the way
  7-Zip reduces its dictionary: a folder known to be smaller than the
  dictionary is coded with one its size (never below 4 KiB), and one that fits
  a single LZMA2 block is coded on one thread. `ArchiveEntry::from_path`
  records the file's length so the size is known before the push.

### Cryptography

- Archives are encrypted with 7-Zip's key-derivation work factor, 2^19
  SHA-256 rounds (0.27.0); upstream writes 2^8. A derived key is shared by
  the clones of its `Password`, so it is derived once per archive rather than
  once per folder.

- **The AES decoder decrypts in the caller's buffer.** It used to read the
  packed stream 512 bytes at a time into a fixed array, decrypt into a `Vec`
  and copy that into the caller's buffer — two million reads and two full
  copies of the payload for a 1 GiB store-mode archive. It now fills the
  caller's buffer with ciphertext and decrypts it in place, so reads are the
  caller's size and the payload is copied zero extra times. The state kept is
  the ≤15 ciphertext bytes that did not complete a block and, only for a caller
  that reads less than 64 KiB at a time (PPMd's range decoder, BCJ2's side
  streams), a 64 KiB plaintext buffer filled a chunk at a time so the cipher
  is never driven a block per call. Neither grows with the stream, and the
  buffer is within the megabyte the memory estimate charges a filter, so the
  estimate is unchanged. On the Linux bench box
  a 1 GiB store-mode AES archive went from 0.862 s to 0.282 s, which is under
  half of `7zz t -p` on the same fixture and the sum of what the cipher, the
  read and the CRC cost on their own.

- Cryptography for the `aes256` coder — SHA-256 *and* AES-256-CBC — goes
  through one internal backend module, `src/crypto_backend.rs`. SHA-256 comes
  from `lzma-turbo`'s crypto module. The default backend is
  `aws-lc-rs` (feature `aws-lc-crypto`, in `default`), the scryer-media house
  convention shared with `lzma-turbo` and `rarpar`; `native-crypto` selects
  RustCrypto's `sha2` and **takes precedence** when both are compiled, so a
  consumer who cannot build C uses `default-features = false` plus
  `native-crypto`.
- **AES-256-CBC is this crate's own code, but not its own backend.** `lzma-turbo`
  removed AES and the 7z key derivation deliberately — 7z cryptography is this
  crate's job — so the cipher lives in `src/crypto_backend.rs`, and it follows
  the same feature switch SHA-256 does: `aws_lc_rs::cipher::DecryptingKey::cbc`
  (AWS-LC's unpadded CBC mode, no PKCS7 — `StreamingDecryptingKey` is the padded
  one and is not used) on the default lane, RustCrypto's `aes`/`cbc` on
  `native-crypto`, which compiles to AES-NI on x86-64 and to the ARMv8
  cryptography extensions on aarch64. Neither lane needs a streaming API: a
  chunk is decrypted with the current IV and that chunk's last ciphertext
  block, copied out before the in-place decrypt, is the next chunk's IV. The
  encoder (`compress`) follows the same switch over the same unpadded CBC:
  AWS-LC's `EncryptingKey::cbc` by default, RustCrypto's `cbc::Encryptor`
  under `native-crypto` and wherever AWS-LC is not selected (a wasm build
  encrypts in the guest).
  `aws-lc-rs` is a direct optional dependency on the pin and features
  `lzma-turbo` uses, so a build with both crates resolves one copy of AWS-LC.
- `aes256` no longer implies a backend: enabling it with neither
  `aws-lc-crypto` nor `native-crypto` is a compile error. A consumer migrating
  from upstream with `default-features = false, features = ["aes256", …]` adds
  `"aws-lc-crypto"` to that list.
- **A wasm guest can hand the block cipher to its host** (`crypto-host`, new
  public module `sevenz_turbo::hooks`). On a `wasm32` target the feature routes
  the bulk AES-256-CBC *decrypt* through a plain `fn` pointer the embedding
  program installs; everything else — the SHA-256 key derivation, the
  LZMA/LZMA2 decode, the CRCs — stays in the guest, and the public API is
  unchanged. The hook takes `(key, iv, data)` and returns a fresh buffer of the
  same length, unpadded, and is **stateless per call**: `HostAes256Cbc` in
  `src/crypto_backend.rs` threads the CBC IV across chunks itself. A hook that
  errors, answers with the wrong length, or was never installed panics; there
  is no silent in-guest fallback, which would undo the delegation. Backend
  precedence becomes host (wasm + `crypto-host`) > `native-crypto` > AWS-LC,
  and on native targets the feature is accepted but inert, so feature
  unification in a mixed workspace cannot turn a native build into a
  delegating one. It adds no AES dependency — `aes256,crypto-host` without
  `compress` carries no in-guest block cipher — and the seam mirrors
  `rarpar`'s `unrar-rs` hooks module deliberately, so an embedder wires both
  crates the same way.
  Since 0.27.0 `crypto-host` also delegates the KDF's SHA-256, and
  `crc-host` the CRC-32, through `lzma-turbo`'s own hooks (see that release).
- New `sevenz_turbo::crypto_backend() -> &'static str`, reporting which backend
  a build selected (`"aws-lc"`, `"rustcrypto"`, or `"host"` on a delegating
  wasm build), for consumers who want to assert on it.
- The `aes` and `cbc` dependencies are enabled by `native-crypto` (decryption)
  and by `compress` (encryption); the AWS-LC lane does not compile them. The
  direct `sha2` dependency is gone.
- When both backends are compiled, a differential test checks they agree on
  SHA-256, on the 7z key derivation and on AES-256-CBC — the NIST SP 800-38A
  F.2.6 vector on each lane, whole-buffer equality at several sizes, chunked
  chaining at 1/2/3/5/13 blocks per call against the one-shot result, empty
  calls and the partial-block refusal. The selected lane is also checked
  against NIST on its own, block by block as well as in one call, and SHA-256
  against its own vectors. `7zAes.c`'s two special cycle counts (`0x3F`, `>= 0x40`) have tests of
  their own.
- The host-delegated lane is proven twice. Natively, `HostAes256Cbc` is driven
  through the real hook (a `fn` pointer links on any target) and compared with
  the RustCrypto lane's one-shot answer at 1/2/3/5/13/64 blocks per call, which
  is what pins the guest-tracked IV threading. In a real guest,
  `tools/wasm-conformance` builds
  `examples/wasm_host_extract_conformance.rs` for `wasm32-wasip1`, runs it
  under `wasmtime` with a reference host AES, and asserts that its extraction
  of a freshly written encrypted archive is byte-identical to the native
  decoder's — and that a guest which never installs a hook panics with the
  documented message. The harness is a workspace member of its own, so
  `wasmtime` is a dev-dependency of that member only and never enters the
  crate's dependency graph.

### Testing

- `tests/differential_7zz_tests.rs`: archives built with `7zz a` across the
  method matrix are extracted with both `7zz x` and this crate and compared
  byte for byte. Skips itself when `7zz` is not on `PATH`.
- `tests/lzma2_mt_tests.rs`: the multi-threaded path against `7zz x -so` at 1,
  2, 8 and every thread; that the parallel path is actually engaged and reports
  its backlog; that moving the thread count mid-archive changes no byte; that a
  memory limit too small for threads degrades to single-threaded instead of
  failing; that the block-completion hook still fires once per block; that
  per-file checksums match the header and a checksum taken over the bytes by an
  unrelated implementation; and that folding checksums equals checksumming the
  whole. The differential matrix now runs every case through three lanes —
  one thread, eight threads, and the adaptive coder at one thread.
- `tools/decode-bench`: times `7zz t` (all threads and `-mmt=1`), upstream
  `sevenz-rust2` 0.22.2 at 16 threads (its `Lzma2ReaderMt` path) and this fork
  at 1, 2, 8 and every thread, on the same archive in one session. Results in
  `docs/benchmarking.md`.
- The vendored BCJ round-trip tests generate their sample data instead of
  reading the binary fixtures `lzma-rust2` keeps in its repository, which are
  not ours to vendor.

## 0.27.0 - 2026-10-09

- Removed: the `lzma-rust2-encoder` feature and the `lzma-rust2` dependency
  are gone; archives are written by `lzma-turbo`'s encoder only, and
  `lzma_encoder()` always returns `"lzma-turbo"`. The BCJ and delta filter
  modules no longer reference `lzma-rust2`.
- Security fix: every AES-256 folder, and the encrypted header, is now
  encrypted under its own random IV, as 7-Zip's writer does.
  `AesEncoderOptions::new` drew one IV, and every folder written with those
  options - and the header - reused it under the same key, so two folders that
  began with the same bytes began with the same ciphertext. The IV is drawn
  per folder and written into that folder's coder properties; the salt and
  cycle count stay the configured ones, so every folder's key is the same
  one, derived as before.
  `ArchiveWriter` no longer uses the `iv` field of `AesEncoderOptions` as any
  folder's IV.
  Two writes of the same encrypted input therefore no longer produce the same
  bytes; the archives decode the same, and 7-Zip extracts them.
- **Security fix: archives this crate encrypts use 7-Zip's key-derivation
  work factor.** `AesEncoderOptions::new` defaulted `num_cycles_power` to 8
  (2^8 = 256 SHA-256 rounds per key), against the 19 (2^19 = 524,288 rounds)
  that 7-Zip writes, so a password guess against an archive written here was
  2048 times cheaper than against one 7-Zip wrote. The default is now 19,
  `AesEncoderOptions::DEFAULT_NUM_CYCLES_POWER`; archives written before
  0.27.0 are as weak as they were and should be re-encrypted if the password
  matters. A lower work factor can still be chosen deliberately, with
  `with_num_cycles_power` or the public field. Both directions are held to
  7-Zip: `7zz t` passes what is written at 19 and at a lowered power and `7zz
  l` reports `7zAES:19`, and archives 7-Zip writes (at 19) decode here. The
  coder properties hold six bits of the value, and a work factor they cannot
  say - anything above 24 other than 63, the format's value for a key used
  as it stands - is refused with `Limit::AesCyclesPower` when the archive is
  written, before those bits are taken: cut down to them, 64 would have been
  written as one round and 127 as no derivation at all.
- A derived AES key travels with every clone of its `Password`. The cache
  was per `Password` value and a clone started empty, so the encoder, which
  clones its options for each folder it sizes and for the encrypted header,
  derived the key once per folder; that is why the default could not be
  raised on its own. Clones now share the cache (an `Arc`, keyed by salt and
  work factor, up to four keys; the bytes of a password never change, so the
  password is the cache's owner), and the derivation runs under its lock, so
  copies asking at once derive once. The `max_aes_kdf_rounds` budget stays
  each copy's own, as before; a key a clone already derived costs a copy
  nothing. On Apple M5 Max, encoding 64 one-folder members with AES at 2^19
  takes 30 ms instead of the 691 ms per-folder derivation would have cost, and
  the 2049-folder bench tree 451 ms instead of 21.7 s: the work factor is a
  one-off 8.5 ms per archive.
- The key derivation hands the hash 64 rounds at a time (`7zAes.cpp`'s
  unrolled buffer) instead of three calls a round; the bytes hashed are the
  same. One 2^19 derivation is about 2 ms faster with AWS-LC on Apple M5 Max.
  The buffer is at most 64 KiB whatever the password's length: a password
  past about a kilobyte gets fewer rounds to a call, and one past 64 KiB is
  hashed where it lies, three calls a round as before, with no copy made.
- An entry can be written on straight from the decoder's output. A new
  `EntryRead` trait (a `Read` with `write_rest`) is what
  `ArchiveReader::for_each_entries_direct` and
  `BlockDecoder::for_each_entries_direct` hand each entry over as; `write_rest`
  passes the bytes to the sink in the pieces the decoder made them in, so a
  consumer that only writes an entry to a file needs no buffer of its own and
  copies nothing. `for_each_entries` is unchanged and is now built on it. The
  multi-threaded LZMA2 reader reads its input 256 KiB at a time, the size
  7-Zip reads a parallel block's input in, with one piece of slack, and a run
  is dispatched as soon as the feed completes it, so every thread starts on
  its own block instead of waiting for a wave to be read whole; the first wave
  of a 2 GiB archive used to be read whole, up to half a gibibyte, before any
  worker started. Verified, medians of three on an x86-64 Linux host against
  7-Zip (time s / peak RSS MiB, ours vs 7-Zip): LZMA2 mx1 AES two threads
  11.41 / 10.7 vs 11.43 / 11.4, eight threads 3.78 / 23.4 vs 3.95 / 25.0;
  mx1 two threads 11.85 / 10.2 vs 11.82 / 10.6, eight threads 3.91 / 23.3 vs
  4.08 / 23.8; stored AES two threads 0.23 / 6.4 vs 0.48 / 8.5; a 2 GiB mx5
  archive on four threads 14.15 / 1035.8 vs 14.12 / 1035.7.
- The folders of a non-solid archive decode in parallel. A new `ReadAt` trait
  reads archive bytes at an offset with no shared cursor; it is implemented
  for `std::fs::File` (`pread` on Unix, `seek_read` on Windows), for bytes in
  memory (`[u8]`, `Vec<u8>`, and through `&`, `Box` and `Arc`), and by
  `SerialReadAt`, which serialises the reads of any `Read + Seek`.
  `ArchiveReader::open` sets one up from the file it opens;
  `ArchiveReader::from_read_at` builds a reader over any `ReadAt` (its source
  type is the new `ReadAtCursor`); `set_positional_source`,
  `with_positional_source` and `clear_positional_source` attach or remove one
  on a reader built any other way. With a positional source and more than one
  thread, `for_each_entries` decodes runs of folders of at most 8 MiB
  unpacked on parallel workers, each from its own cursor.
- The callback still sees every entry, its bytes, the sub-stream and block
  completion hooks, and any error in archive order, exactly as the
  sequential walk would show them: a worker's output is staged and replayed on
  the calling thread. A checksum mismatch or a damaged stream is still
  reported as `Error::BlockDecode` naming the folder it is in and its packed
  offset; a folder past it is never shown to the callback. The one visible
  difference is that a worker reads each member to its end, so a member the
  callback skipped is still checked. A callback that reads a member to its
  end and then answers `false` still has that member's sub-stream hook called
  first. A callback that panics unwinds out of `for_each_entries` once the
  workers have been stopped, and likewise the writer of
  `push_archive_entries_non_solid`.
- The thread count is a budget, not a per-folder count: workers x threads
  per folder never exceeds it. A folder larger than 8 MiB is decoded alone on
  the calling thread with the whole thread count, so a media-sized archive
  takes the multi-threaded LZMA2 path as before. What is staged between the
  workers and the caller is bounded: two folders per worker, at most 8 MiB
  each, so at most 16 MiB per worker, whatever the archive says. Without a
  positional source, with one thread, or on `wasm32`, every folder decodes on
  the calling thread as before.
- Under a memory limit each folder of a run gets one thread, and the run gets
  as many workers as fit the limit together. A worker is charged the largest
  decoder of its run, the 64 KiB input buffer under it, its two 8 MiB stages
  and the 1 MiB piece it is filling for them; the calling thread is charged
  the 1 MiB piece its callback is reading. A run the limit leaves one worker
  for is decoded on the calling thread, a folder at a time.
- `Lzma2Handle::set_threads` is followed while a run of small folders
  decodes. A run starts at the reader's thread count, as a block's coder
  does; from there the ceiling bounds how many of the run's folders decode
  at once and the threads of each one's own coder, from the next folder to
  start. A run never has more workers than it was planned with.
  `Lzma2Handle::progress` and `ArchiveReader::lzma2_progress` are `None` for
  as long as such a run lasts, since no one coder is decoding for the reader
  then.
- `ArchiveWriter::push_archive_entries_non_solid(entries, open, threads)`
  codes each entry as a folder of its own on up to `threads` workers and
  writes them in the order given: the archive a loop of
  `push_archive_entry` writes, byte for byte apart from the encrypted
  folders' IVs. `open(index, entry)` is called on the coding thread, so no
  more files are open than folders in flight. Each worker stages at most two
  folders of at most 8 MiB of compressed bytes before it waits for the
  writer, so memory is bounded by threads x (one coder + 16 MiB), however
  large the inputs. A folder whose coder would start block threads of its
  own is coded alone on the calling thread, after the folders before it. A
  worker and its folder's coder thread count as one of `threads`; a chain
  with more than one LZMA or LZMA2 coder has a coder thread for each, and
  that many fewer folders in flight (`threads / 2` for two), so the threads
  at work stay within `threads`. A compressor that is not LZMA or LZMA2
  (PPMd, BZip2 and the rest) codes on the worker itself, so beside an LZMA
  or LZMA2 coder it counts as a thread too: eight folders of PPMd or BZip2
  with LZMA2 kept up to 12.7 cores busy at `threads = 8` on Apple M5 Max
  when it did not. A filter or AES-256 in the chain is not counted, since
  the worker runs it between its waits for the coder: eight folders of
  LZMA2 kept 7.7 to 7.9 cores busy with AES-256 over it and 7.7 to 8.0
  without. A BCJ2 folder coded on a worker runs its
  call and jump coders on that worker rather than on two threads of their
  own, and writes the same bytes. Its
  three other pack streams, held whole by the worker until the folder ends as
  on every path, then go through the worker's stage after the main stream, in
  pieces of at most 256 KiB, each counted against the stage like the main
  stream's bytes: a worker whose stage has no room for them waits for the
  writer instead of starting another folder. They used to follow as one
  message holding all three, which a stage the writer had already emptied took
  at once whatever its size, so each folder in the window could leave its
  whole tail parked beside the next.
- On Apple M5 Max (18 threads), the 8192-member non-solid tree (256 MiB
  unpacked, 32 KiB average, written by `7zz -mx=5 -ms=off`) decodes in
  0.30 s at all threads, from 3.35 s (11.2x; `7zz t` takes 3.43 s, as it
  decodes non-solid folders one at a time), with peak RSS 18.5 MiB from
  9.1 MiB. Writing it non-solid at all threads takes 1.72 s, from 20.2 s
  (11.7x; `7zz a -mmt=18` takes 16.8 s), at the same archive size, with peak
  RSS 186 MiB from 32 MiB. Single-threaded decode and encode, solid and
  single-folder archives, and the media rows are unchanged.
- A block whose CPU-heavy coders would share the caller's thread decodes as
  a pipeline when more than one thread is allowed: each coder below the top
  with at least 1 MiB of output gets a thread of its own (at most one fewer
  than the threads allowed, largest first), and the stages are joined by
  bounded pipes of four 256 KiB pieces. An LZMA2 coder that decodes in
  parallel already has its own workers and does not count, so AES over
  parallel LZMA2 stays sequential: a thread for the cipher measured as a
  wash. Beside such a coder the stages are threads on top of its workers,
  not out of them: for a BCJ2 chain as 7-Zip writes it, two, decoding the
  call and jump streams the calling thread decoded before. The work is the
  same and now overlaps the main stream's; `Lzma2Handle::set_threads`
  narrows the main coder's workers and not the stages. A coder's error
  arrives after the bytes it produced and unchanged, so the block it is
  reported against is the same as before. A pipe holds up to 1.25 MiB, and
  a block's pipes are charged to the memory limit with its coders before
  any of them is built: every coder, the filters and the fixed-size codecs
  at the figures `Archive::decoder_memory_estimate` gives them. A block
  whose pipes do not fit beside them keeps the sequential chain; nothing is
  refused for it. The parallel LZMA2 plan's run buffers are sized against
  the same remainder: the limit less every other coder of the chain, the
  fixed-size ones at those figures too, where they were sized against the
  limit less the sized coders alone. So does the rest of a chain from the first coder whose thread
  cannot be started: it decodes on the caller's thread, reading the stages
  already running. One thread, and wasm32, keep the sequential chain. A
  BCJ2 archive written by 7-Zip (LZMA2 main stream, LZMA call and jump
  streams) decodes 1.07-1.08x faster at 2-18 threads on Apple M5 Max and
  1.08-1.11x at 2-8 threads on x86.
- A chain has one parallel LZMA2 reader. Each LZMA2 coder of a block planned
  for itself, so a chain of two with enough to decode started twice the
  workers the block was given threads for, each sized to the whole of what
  the memory limit leaves. The coder with the most to decode keeps the plan,
  the first of several that tie; every other LZMA2 coder of the chain decodes
  on one thread, and counts for the pipeline as any other CPU-heavy coder.
- A reader no longer builds its name index when it is opened.
  `ArchiveReader::read_file` and `file_compression_methods` build it on the
  first lookup by name; a consumer that walks the entries never does, and no
  longer holds a second copy of every name. Over a parsed archive of 100,000
  entries, making the reader took 3.7 ms on Apple M5 Max and now takes under
  a microsecond; the first lookup by name then takes 2.2 ms, the index being
  sized for the entry count where it used to grow as it filled. Of two
  entries with one name a lookup finds the later, as before.
- An adaptive LZMA2 decode reaches the fixed plan's width. It started at one
  thread with the first run decoded on the calling thread, which held the
  stream's cursor; a widening was only heard once a whole run had landed;
  and the backlog it reported left out runs already claimed, so a governor
  narrowed it again straight after widening it. It now hands the first run
  to a worker, listens for a widening while that worker runs, counts every
  run in hand behind the front of the output, and reads ahead for the width
  it is offered. On a 1 GiB mx5 stream at 18 threads on Apple M5 Max, an
  adaptive decode went from 4.45 s to 2.46 s (the fixed plan: 2.37 s) at
  the fixed plan's 2.07 GB peak RSS, and from 7.56 s to 4.77 s at 8 threads
  on x86 (fixed: 4.74 s). Incompressible data stays narrow.
- A parallel LZMA2 decode of more runs than it has threads keeps every
  thread it can afford busy. The reader hands over whole runs and stops at a
  run's end; the decoder closes a run only on the control byte after it,
  which is the first byte of the next piece; and once the input budget was
  spent on the run in hand and the runs out with workers, that piece was
  refused for its size while the run it would have closed sat undispatched
  with a worker idle. Nothing moved it: a drain had no declared run to give,
  and the room a landing worker freed went on the next whole piece, so after
  the first wave the decode ran one run short of its width. A piece refused
  at a run boundary now has its first page fed on its own, the decoder
  closes the run on the next drain and gives it out, and the rest of the
  piece is still the next thing offered whole. On an Apple M5 Max a 2 GiB
  `-mx=5` archive of sixteen 128 MiB runs decodes at 4 threads in 9.0 s
  instead of 17.3 s and at 8 threads in 4.7 s instead of 6.8 s (medians of
  three over two alternating passes on a loaded machine; 7-Zip with every
  core: 3.0 s). Under a memory limit that pays for four runs the 4-thread
  decode went from 15.4 s to 13.1 s; the remaining gap under a tight limit
  is the decoder's budgeting, not the reader's.
- A block of exactly one LZMA2 run (1 MiB) decodes single-threaded. The
  parallel path wins only from the second run, and one run decoded in
  parallel was 4.5% slower; the threshold is the encoder's minimum run size,
  measured the same on x86 and arm64.
- A multi-threaded LZMA2 decode of a stream that is a single run no longer
  holds its input. The reader looked ahead for a run boundary to hand whole
  runs to its workers, and a stream written as one run has none: it read up
  to 192 MiB ahead and then went on feeding the decoder in 1 MiB slices
  copied out of that hold, and a stream smaller than its 256 MiB give-up was
  read whole first. Once a first run has decoded past the longest run an
  encoder writes for that dictionary (7-Zip's block size: four dictionaries,
  kept to 1..=256 MiB and never under one dictionary), the reader takes the
  stream as one run and streams it, reading a few MiB ahead of the decoder.
  On Apple M5 Max at 18 threads, peak RSS went from 398 to 148 MiB on a
  900 MiB single-run archive with a 16 MiB dictionary (7-Zip: 156 MiB) and
  from 424 to 116 MiB on a 160 MiB delta-filtered one (7-Zip: 122 MiB), at
  the same wall time; multi-run archives decode as before.
- Which runs decode on fewer threads is decided from the chunk headers, not
  from the ratio. A run whose packed size was no smaller than its unpacked
  size was taken for stored data, and decoded narrow because a copy has
  nothing to gain from more threads; but LZMA-coded data that barely
  compresses has the same ratio and decodes about as slowly as anything. A
  run is now narrowed when under one part in 64 of its output comes from
  LZMA-coded chunks, by the counts `lzma-turbo` 0.7.0 records for each run;
  stored data still goes narrow, and an LZMA-coded run stays as wide as it
  was asked to be whatever its ratio.
- `lzma-turbo` 0.7.0.
- An LZMA2 encode on more than one thread gives the binary-tree match finder
  a thread of its own, as 7-Zip does (`numThreads = 2` for the normal
  algorithm with a binary-tree finder; one for the fast algorithm and hash
  chains). A single-threaded encode and the LZMA coder are unchanged. As in
  7-Zip, the block threads are the thread count divided by the match
  finder's two, so an LZMA2 encode on 8 threads codes 4 blocks at once
  rather than 8 blocks each with a finder thread (16 busy threads); the
  bytes are the same. A folder coded on a worker of
  `push_archive_entries_non_solid` keeps its match finder on the worker's
  thread, so the workers stay within the thread count.
- Bench tooling: `decode-bench op version` reports `ppmd_crates`, every
  `ppmd-*` crate in the `Cargo.lock` the binary embeds with its version, and
  `ppmd_turbo` (its version, or `absent`), in place of `ppmd_rust`; the
  harness report's toolchain line prints the PPMd crates and still reads a
  binary that reports only `ppmd_rust`. Nothing names the PPMd engine by
  crate any more, so the record survives a change of engine.
- PPMd decodes and encodes through `ppmd-turbo` instead of `ppmd-rust`, in
  both directions: its reader reads straight out of the coder's 64 KiB
  input buffer, and its writer settles 64 KiB of output at a time and ends
  the stream once, with no end marker, as 7-Zip does. The decoder is built
  with the folder's unpacked size, which ends the stream. A stream cut short
  fails its read as an I/O error of kind `UnexpectedEof` and a corrupt one
  as `InvalidData`, located in its block, the same errors the LZMA decoders
  give. A model larger than the memory limit is refused with
  `MaxMemLimited` before it is allocated. The encoder writes the same bytes
  as before, which are the bytes `7zz a` writes for the same order and
  memory size. On Apple M5 Max, 7 interleaved runs each, a 15 MB PPMd
  block written by 7-Zip decoded in 1.98 s instead of 2.92 s (7zz:
  2.63 s), and the same data under AES-256 in 2.00 s instead of 2.93 s
  (7zz: 2.64 s). Encoding runs within 3% of before: 16 MiB of data that
  barely compresses at order 8 took 2.52 s instead of 2.45 s (7zz:
  2.44 s), and 4 MiB of x86 code 0.43 s instead of 0.42 s. Text encodes as
  fast as before.
- Writing LZMA or LZMA2 where no thread can be started (`wasm32`) no longer
  holds the folder's whole input. The thread-less path collected every byte
  and encoded on `finish`, so its memory grew with the input; it now pushes
  each write into `lzma-turbo`'s push encoders on the caller's thread, which
  run the encoder as far as a queue of about one 2 MiB LZMA2 chunk allows.
  The packed bytes are unchanged, byte for byte, and the threaded path is
  untouched. Forced onto that path on Apple silicon (level 1, 8 MiB
  dictionary, one thread), peak RSS went from 395 to 67 MiB for a 256 MiB
  input and from 1382 to 67 MiB for 1 GiB, with wall and CPU time no worse.
  Building with `--cfg sevenz_turbo_unthreaded` sends every writer down that
  path, for tests and measurement on a host with threads, and CI runs clippy
  and the default-feature tests built that way (the `unthreaded` job).
- The `wasm32-unknown-unknown` clippy gate (`--no-default-features --features
  default_wasm`, `-D warnings`) passes: `Decoder::Delta` boxes its reader,
  whose filter history would otherwise size every variant, and two closures
  in `util::wasm` became the functions they wrapped. No behaviour changed.
- Bench harness: `run` refuses an `--out` whose `scratch` directory already
  exists instead of deleting it at the end of the run, and removes the one
  it made however the run ends, so a planning failure no longer leaves a
  directory that refuses the next run.
- Under a memory limit, a block's parallel LZMA2 decoder is sized against
  what the limit leaves after the rest of the chain: the pack-stream buffer
  and the other coders' memory the chain check reserved. The plan was handed
  the whole limit and subtracted only its own dictionary and state, so its
  in-flight runs could take the chain past the limit by the reserved amount.
- `sevenz_turbo::sha256` digests with the backend `crypto_backend` names.
  decode-bench takes its `Cargo.lock` digest through it, so the
  `native-crypto` candidate no longer links AWS-LC's SHA-256 beside
  RustCrypto's for that one digest.
- The memory limit charges a PPMd coder for the 64 KiB input buffer it reads
  through as well as its model, in the per-coder check and the chain check
  alike, so the buffer is counted before the coder is built.
- decode-bench reports the Cargo profile it was built under as
  `build_profile`, and the bench harness refuses a candidate that does not
  report `release` and counts the profile in the identity the native
  candidate must share with the primary one.
- Each folder's LZMA and LZMA2 coder is sized to the folder. Every folder was
  set up with the full dictionary and, with more than one thread allowed, a
  multi-threaded LZMA2 coder that buffers a whole 32 MiB block, so a
  non-solid archive of small files paid that setup per file. A folder whose
  size is known and smaller than the dictionary now gets a dictionary its own
  size (7-Zip's rule, never below 4 KiB), recorded in the coder's properties,
  and a folder that fits one LZMA2 block is coded on one thread. The encoded
  header is sized the same way. Archives whose folders are no smaller than
  the dictionary are written byte for byte as before. On Apple M5 Max, 512
  non-solid 16 KiB files went from 0.70 s CPU and 101 MB peak RSS to 0.50 s
  and 8.5 MB, at the same archive size.
- The writer learns a folder's size from its entries: `ArchiveEntry::size` is
  read as a size hint before a push (zero means unknown), `from_path` now
  fills it in from the file's length, and a solid block is sized by the sum
  when every file in it declares a size. When any file says zero, the writer
  reads up to 1 MiB ahead instead and sizes a shorter stream by what it read,
  so a block mixing `from_path` and `new_file` entries is never sized by its
  known entries alone. A wrong hint costs ratio or threads, never
  correctness; the push records the bytes actually read, as before.
- BCJ2 can be written. It is asked for as the single-stream filters are, as
  the last content method after the coder for its main stream:
  `set_content_methods(vec![EncoderMethod::LZMA2.into(),
  EncoderMethod::BCJ2_FILTER.into()])` (LZMA in place of LZMA2 works too, as
  do filters between them). It is opt-in: the default method stays LZMA2
  alone, for executables as for anything else.
- The block is the four-stream folder 7-Zip writes for `-mf=BCJ2`, coder for
  coder: an LZMA coder each for the jump and call streams, the main stream's
  coders, then BCJ2 with four inputs; BCJ2's main, call and jump inputs bound
  to those coders; and four pack streams in 7-Zip's file order - main, the
  range-coded stream stored raw, call, jump. The call and jump coders take
  7-Zip's settings from `AddBcj2Methods` in `7zUpdate.cpp`: a 1 MiB
  dictionary, 128 fast bytes, one thread, `lc0 lp2`. The conversion is
  `lzma-turbo`'s port of the SDK's `Bcj2Enc.c`, run with its defaults.
- The main stream is coded as it arrives. The call and jump streams are coded
  as they arrive too, into memory, and appended after the main stream with
  the range-coded stream when the block ends, so a block holds its coded call
  and jump streams in memory until then - as 7-Zip does with the streams
  after the first.
- All three ways of writing a block take it: `push_archive_entry`,
  `push_archive_entries` (solid) and `prepare_block` /
  `push_prepared_block`. A `PreparedBlock`'s `compressed_len` counts all four
  pack streams, and so does a BCJ2 entry's `compressed_size`.
- A method list that puts BCJ2 anywhere but last, gives it no coder for the
  main stream, or combines it with AES-256 is refused with
  `Error::Unsupported` when the first entry is written. Encrypting a BCJ2
  block would need an AES coder on every pack stream, which this writer does
  not build.
- Fixed: a header whose pack-stream CRCs were not all defined (a pack stream
  whose CRC32 is 0) wrote the defined-bits vector but not the CRC values
  that 7-Zip's `WriteHashDigests` puts after it, so the header did not parse.
  The values are now written. One pack stream in four billion hits this; a
  BCJ2 block has four.
- Tested: the folder layout, bind pairs and pack-stream order against the
  ones 7-Zip writes; round trips through this crate's reader single- and
  multi-threaded, in all three layouts, for empty and one-to-five-byte
  inputs and for x86 code whose call and jump streams are not empty; that how
  the source is read cannot change a byte of the archive; and, where `7zz` or
  `7z` is on `PATH`, that 7-Zip tests and extracts the archives to their
  inputs.
- Fixed: the reader set a BCJ2 entry's `compressed_size` to its block's first
  pack stream alone, so the same entry reported a smaller size after the
  archive was reopened than the writer reported when it was pushed. The
  first entry of a block now carries the sum of every pack stream the block
  reads.
- New `sevenz_turbo::lzma_encoder() -> &'static str` (behind `compress`):
  `"lzma-turbo"`, or `"lzma-rust2"` when the `lzma-rust2-encoder` feature
  is on, which a dependency can turn on unnoticed - the encoder's
  counterpart of `crypto_backend()`.
- The one-block rule applies to the `lzma-rust2-encoder` build as well: a
  folder known to fit one LZMA2 block is coded by `lzma-rust2`'s
  single-threaded writer instead of starting its multi-threaded one.
- The parallel LZMA2 reader no longer holds LZMA-coded data that barely
  compressed to two threads. A run counted as incompressible, and was
  decoded on two threads however many were asked for, once it packed to 90
  percent of its size or more; that line was meant for stored chunks, whose
  decode is a copy bound by the read, but it also caught media archived at an
  ordinary level, whose chunks are LZMA-coded at 92 to 99.5 percent and are
  the slowest LZMA there is to decode. A run is now taken as incompressible
  only when it is no smaller than what it decodes to, which an encoder that
  codes a chunk only when coding shrinks it makes the stored case. On Apple
  M5 Max at 18 threads, a gigabyte of such data written by `7zz -mx5` went
  from 17.0 s to 4.0 s (7zz: 3.9 s) and by `-mx1` from 13.4 s to 2.2 s
  (7zz: 2.4 s); a gigabyte of random data, stored chunks, still decodes on
  two threads, in 0.2 s.
- Fixed: a CRC-32 that does not match is one error on every path:
  `Error::BlockDecode` with `BlockErrorKind::ChecksumMismatch`, located in
  its block, with the same message. Only the parallel path's folded check
  said so. A block holding one file - every block of a non-solid store
  archive, where a damaged byte shows up as nothing but a CRC mismatch -
  reported `BlockDecode` of kind `Io` around an `io::Error` of kind `Other`,
  and a file of a solid block decoded on one thread came out as a bare
  `Error::Io` with no block at all, so a consumer that keeps a damaged set
  for repair on a checksum mismatch gave those up as fatal.
  `ArchiveReader::read_file` reports the same error. An encrypted block's
  mismatch is still `BlockErrorKind::Password` on every path, `read_file`
  included, since a wrong password looks the same.
- Fixed: `set_verify_checksums(false)` turns off the block checksum as well.
  A block holding one file was still checked against the file's CRC, which
  the block borrows, and a block's own CRC was checked on the consuming
  thread even where the parallel decoder's workers already fold it. A
  one-file block read through `for_each_entries` or `read_file` is now
  checked once, against the file's CRC, instead of twice over the same
  bytes; the block check stays wherever it is the only one.
- Fixed: the files of an LZMA2 folder decoded as one long run are compared
  with their CRCs at every thread count. Above one thread the parallel
  reader's workers take the checksums and the reader asks for each file's.
  A stream the reader cannot split - one run longer than the reader holds
  for a split, which is what 7-Zip writes when it has no block threading
  (`-mmt=1`, `-mmt=2`), at any size past a few megabytes packed - is
  checksummed as one open piece, and the decoder gives that piece up only
  when it reaches the stream's end marker. A reader that stopped at a file's
  last byte never got there: it found no checksum for the file, took that as
  bytes the callback had left unread, and compared nothing. Such a file was
  delivered with a wrong CRC unnoticed, its sub-stream hook was never
  called, and the block's own CRC was passed over the same way, while the
  block hook still reported it verified. One thread compared them, as
  0.26.1 did at every count; this was never in a published version.
  The reader now keeps such a file owed. Once the stream has been read to
  its end it makes the one read that reaches the end marker, then compares
  every owed file in archive order and calls its hook; a folder worker does
  the same, and so does the block's own check. A file with a CRC for which
  the decoder still has no checksum after the end of the stream is an
  error, never a pass. Known limit: files that share one such run and are
  not the last of their block are compared, and their hooks called, when
  the block ends and not as each file ends, and a callback that stops after
  one of them has the rest of the block decoded and thrown away to get its
  checksum. Comparing at each file's end needs the decoder to give up the
  open piece at a file boundary, which is a later lzma-turbo version.
- Fixed: a block whose only checksum is its own is compared once every byte
  of it has been handed over, whether or not the callback handed the last
  byte then stops. Above one thread that checksum is folded from the
  workers' after the last callback returns, and a callback that answered
  `false` was taken at its word first: a damaged block whose files carry no
  CRCs of their own passed unnoticed, whether its stream was one run or
  many. A folder decoded on a worker had its verdict left unread the same
  way. One thread compares on the read that hands the last byte over, as
  0.26.1 did at every count; this was never in a published version. The
  comparison now comes first, and the stop is honoured after it.
- Fixed: damage in a block that does not decrypt is reported as damage
  whatever password the caller holds. Whether a failure might be a wrong
  password was decided by whether a password had been supplied, not by
  whether the block has an AES coder. A consumer that hands every archive of
  a job the job's password was therefore told `BlockErrorKind::Password`, or
  `Error::MaybeBadPassword`, for a checksum mismatch or a broken stream in a
  plain block, and was asked for a password where it could have repaired.
  0.26.1 answers `Password` for a flipped byte of a store-mode block read
  under such a password. The answer is now read from the block's own coders
  on every path: `for_each_entries` on one thread and on folder workers,
  `read_file`, the construction of a coder from its properties, and the
  compressed header, which is damage under any password unless it is itself
  encrypted. A block that does decrypt is unchanged: a wrong key and damaged
  ciphertext cannot be told apart, and both stay `Password`.
- Fixed: a store-mode block whose source ends before the bytes its header
  declares is an error. Copy keeps no count of its own, and neither does a
  filter over it, so an archive cut short, or a header declaring more than
  was packed, ended the stream with a clean end of input: each file came out
  short and the decode returned `Ok`. A CRC did not catch it, because a
  file's CRC is compared once every byte it declares has been read. 0.26.1
  does the same: of a two-file store archive cut inside the first file it
  delivers 60,000 of 100,000 bytes, none of the second file, and `Ok`, with
  verification on or off. A block's packed stream, each packed stream of a
  BCJ2 block and the block's decoded output now fail the read that finds the
  stream ended with bytes still owed, with `io::ErrorKind::UnexpectedEof`,
  the kind a truncated LZMA stream gives. `for_each_entries` reports it as
  `Error::BlockDecode` of kind `Io` located in the block, on one thread and
  on folder workers, and `read_file` as `Error::Io` of that kind. A read of
  no bytes is still answered with none, and a read past a block's declared
  length is passed on, so an LZMA2 coder is still brought to its end marker.
- Fixed: a PPMd block this crate wrote failed `7zz t` with "Data Error",
  although `7zz x` and this crate's reader both gave the right bytes back.
  The writer flushes a block's coder chain before finishing it, and
  `ppmd-rust`'s `flush` ends the range coder, which `finish` then ended a
  second time, so every PPMd pack stream carried five bytes after its end
  that 7-Zip's strict end-of-stream check refuses. A flush now only passes
  down to the sink, and the stream ends once. Tested: where `7zz` or `7z` is
  on `PATH`, 7-Zip tests and extracts what every encode method writes -
  Copy, LZMA, LZMA2, LZMA2 with BCJ and with delta, PPMd, BZip2 and Deflate,
  solid and not, with and without AES-256.
- PPMd decodes as fast as 7-Zip. Its range decoder takes its input a byte
  at a time, and nothing between it and the archive buffered, so every
  compressed byte was a read call on the source; a PPMd block now reads
  through a 64 KiB buffer wherever it sits, under AES or in a BCJ2 graph
  included, and AES under it decrypts 64 KiB at a time instead of one
  16-byte block per call. On Apple M5 Max (heavily loaded), a 15 MB PPMd
  block went from 12.5 s, 4.4 s of it in the kernel, to 5.8 s with none
  (`7zz t -mmt=1`: 6.1 s), and an AES-256 PPMd block of 30 MB from 1.86
  million reads to 454 and from 13.1 s to 12.4 s (7zz: 12.6 s).
- The BCJ filters read and filter 64 KiB at a time rather than 4 KiB,
  Brotli reads 64 KiB of input at a time, and every block's pack stream is
  read through a 64 KiB buffer, so a chain whose first coder reads whatever
  it is asked for - Copy, delta - makes no more read calls than that however
  small the caller's reads are. 7-Zip's filter coders read at least as
  much. A Copy+BCJ block of 4 MiB went from 1,033 reads to 69. A coder that
  already reads large pieces - LZMA, LZMA2 - goes through the buffer
  without a second copy, and no other row moved. The buffer counts against
  `memory_limit_bytes`: `Archive::decoder_memory_estimate` charges every
  block 64 KiB for it, and so does the chain check of a block read through
  it, so a Copy block no longer estimates at zero.
- An LZMA2 block that declares less than 1 MiB of output is decoded
  single-threaded whatever thread count or adaptive mode was asked for.
  7-Zip's and lzma-turbo's multi-threaded encoders never cut a run finer
  than `max(4 x dict, 1 MiB)`, so such a block is one run, and the parallel
  path decoded it on the calling thread anyway after starting workers and
  reserving its read-ahead. A non-solid archive of 512 blocks of 16 KiB went
  from 0.24 s, 12.7 MiB peak RSS and 27,000 involuntary context switches at
  18 threads to 0.14 s, 4.2 MiB and 1,100: the same as one thread. The
  parallel path reads the remainder of a block in pieces sized to what the
  block declares rather than 4 MiB each, so it no longer zero-fills and
  reserves 4 MiB for the last kilobytes of a stream; large decodes are
  unchanged.
- An adaptive LZMA2 decode with no memory limit widens. Its in-flight
  budget is set when the block's coder is built, and with no limit it was
  the backstop for the one thread the decode starts at, so it never held
  enough runs to widen past two. It is now the backstop for the threads it
  may widen to: the machine's parallelism, or the threads asked for if that
  is more. A decode under a memory limit is unchanged. On Apple M5 Max, a
  1 GiB near-incompressible `-mx5` archive decoded adaptively at 18 threads
  went from 17.3 s at a widest of 2 threads and 443 MB peak RSS to 4.6 s at
  9 and 2.06 GB, the same as with a 4 GiB limit.
- The AES-256 encoder encrypts and writes 64 KiB at a time through the
  selected cryptography backend - AWS-LC's unpadded CBC by default,
  RustCrypto's under `native-crypto` and on wasm - instead of encrypting
  each 16-byte block with RustCrypto and writing it on its own. The output
  is the same CBC ciphertext. An AES-256 LZMA2 level-5 encode of 16 MiB at
  18 threads went from 5.4 s wall, 1.6 s of it in the kernel, to 4.5 s and
  0.07 s. Its dead 32-bit `write_size` counter, which overflowed at 4 GiB in
  a debug build, is gone.
- The AES-256 decoder serves a caller that reads less than 64 KiB at a time
  from 64 KiB it decrypted ahead, rather than decrypting and reading one
  block per call below 16 bytes. A caller reading 64 KiB or more is still
  decrypted in its own buffer with no copy, so the AES decode rows, which
  already read in bulk, did not move.
- `push_archive_entry` and `push_archive_entries` gather a folder's
  compressed bytes in a 256 KiB buffer before they reach the archive's
  writer, and flush it, reporting a failure, when the folder is done. A
  coder that writes a byte or two at a time - PPMd, the BCJ2 range coder -
  no longer makes a write call per byte on a bare `File`: a solid PPMd
  encode of 16 MiB to `ArchiveWriter::create` went from 37.3 s, 27 s of it
  in the kernel, to 5.1 s and 0.03 s. Entry sources are read 1 MiB at a
  time rather than 4 KiB, which also hands a multi-threaded LZMA2 coder one
  message per mebibyte. The output is byte-identical (LZMA2 level 5, BCJ2
  and PPMd checked), and `ArchiveWriter::create` still returns
  `ArchiveWriter<File>`.
- Bench harness: the full corpus gains `media_mx5_2g.7z` and
  `media_mx5_3g.7z`, the near-incompressible recipe at 2 and 3 GiB written at
  `-mx=5 -mmt=8` with 7-Zip's own run size: 16 and 24 LZMA2 runs of 128 MiB.
  The larger has more runs than the widest host measured has threads (18), so
  a parallel decoder that cannot keep every thread supplied from a backlog
  shows it at any thread count; the 1 GiB archives have eight runs and hide
  it from eight threads up. `fixtures` counts the runs of these, of `mt.7z`
  and of the other media archives from each archive's own stream, by walking
  its LZMA2 chunk headers, records the count and the run sizes in
  `fixtures.json`, and refuses an archive with fewer runs than its recipe
  needs. The quick corpus is unchanged.
- `Lzma2Handle::keep_ledger` asks a reader to account for its parallel LZMA2
  decodes, and `Lzma2Handle::ledger` returns the account as an `Lzma2Ledger`:
  the most the decoder held, the most packed input queued for it and decoded
  output held behind it, and the most the three came to at one moment; the
  runs handed out, in waves, where a wave is the runs the decoder claimed
  between two sleeps of the delivering thread, each with the runs out as that
  thread went to sleep; how often the decoder handed a
  piece of input back for want of room, split by where in a run the piece was
  offered; how often the reader stopped reading ahead for room and how often
  because the decoder had its backlog; and how long the delivering thread
  slept, by the runs out as it went to sleep. A ledger steers nothing: it
  reads the gauges a decode already keeps at the points where it already
  looks at them. A reader not asked for one carries an empty `Option` and
  every hook returns at its first branch. `decode-bench op decode --ledger`
  keeps one and reports it as `ledger_*` fields. Three figures are not in it,
  because lzma-turbo 0.7.0 does not expose them: how many times the decoder
  declined to give a complete run to a worker for want of room (it reports
  the bytes, behind a feature, not the count), how its held bytes divide
  between input, runs out, runs waiting and parked buffers, and how many runs
  are being decoded at a moment, as opposed to claimed and not yet delivered.
- Bench harness: a `decode ledger` group in the `full` and `fleet` profiles
  decodes `media_mx5_3g.7z`, `media_mx5_2g.7z` and `mt.7z` at 2, 4, 8 and all
  threads, with no memory limit and with limits of 512, 553, 1024, 1065 and
  2089 MiB, against 7zz at the same thread count, and `media_mx1.7z` and
  `aes_mx1.7z` at 2, 4 and 8 threads as rows that a change to the run
  hand-over must not move. The candidate keeps a ledger; each row without a
  memory limit also runs it without one, as `sevenz-turbo no-ledger`, so the
  cost of keeping it is measured rather than assumed. The report gains two
  tables: memory (peak RSS, what the decoder held, the reader's queue, the
  output behind it, the three together, the dictionary the decode allocated
  beside dictionary size times the decoders that ran, and the remainder) and
  dispatch (runs, waves, the runs claimed in each wave and the runs out at
  its end, the mean runs out while the delivering thread slept, refusals by
  where in a run the piece was offered, the reader's stops for room, and the
  time the delivering thread slept and the worker time that stood idle with
  it). The quick profile plans none of these rows.
- Bench harness: report.md gives throughput to three significant figures, so
  a row under 0.5 MiB/s no longer reads as 0.
- Bench harness: raw.json and the reports built from it name the corpus, the
  run's scratch and output directories, the checkout and the home directory
  as `<fixtures>`, `<scratch>`, `<out>`, `<repo>` and `~`, not by their
  absolute paths.
- Bench harness: the Cargo.lock digest that ties a candidate to its checkout
  reads CRLF line endings as LF, in decode-bench and in the harness, so
  `merge` no longer refuses a report from a CRLF checkout over line endings
  alone.
- Bench harness: BCJ2 encode rows, `encode/bcj2/L5/{T1,T4,Tall}`, in the full
  and fleet profiles. They write the x86-shaped `code-x86` source with
  `decode-bench op encode --filter bcj2`, a new flag that writes the BCJ2 chain
  7zz writes for `-mf=BCJ2`: BCJ2 first, LZMA2 for its main stream, LZMA for
  its call and jump streams. The reference is `7zz a -mf=BCJ2 -mx5 -mmt<T>`.
  7zz refuses `-mmtf=off` with a filter when it writes, so the one-thread
  encode row has only the plain `7zz -mmt=1` reference.
- Bench harness: the one-thread BCJ2 row, `decode/bcj2/T1`, judges parity
  against `7zz -mmt=1 -mmtf=off`, the 7zz that also decodes on one thread;
  plain `7zz -mmt=1` still runs the BCJ2 stage on a second thread, and its
  ratio is reported beside for a user's view. Every ratio in report.json
  names the reference it is against (`reference`) and whether that is the
  row's parity reference (`parity_reference`). The scenario names a parity
  reference other than 7zz in `parity_reference`, and both reports mark the
  parity ratio `(parity reference)`.
- `crypto-host` now delegates the 7z key derivation's SHA-256 as well as the
  AES-256-CBC decrypt, and a new `crc-host` feature delegates every CRC-32 the
  archive carries (start header, header, members); both on `wasm32` only,
  inert on native targets. They forward `lzma-turbo/crypto-host` and
  `lzma-turbo/crc-host`: until now `crypto-host` forwarded
  `lzma-turbo/native-crypto` and kept SHA-256 in the guest, and `crc-host` did
  not exist. `sevenz_turbo::hooks` re-exports `lzma-turbo`'s hook API
  (`HostHashHooks`, `install_host_hash_hooks`, …), so an embedder installs both
  seams without depending on `lzma-turbo` directly; the module is present with
  either feature. A wasm embedder that enables `crypto-host` must now install
  the hash hooks too, or the key derivation panics naming
  `install_host_hash_hooks`. The conformance guest builds with
  `aes256,crc-host,crypto-host`, and the `wasmtime` harness serves a reference
  SHA-256 and CRC-32 beside its AES, asserts that each import was called and
  every SHA-256 handle closed, and checks that a guest missing either set of
  hooks panics with that set's message.
- CI's package job builds the crate from its own archive (`cargo package
  --locked`) as well as listing it, so a file the build needs that the
  archive leaves out fails on the pull request rather than at publish.
- `ArchiveEntry::from_path` reads the path's metadata once, where it read
  it three times (`is_file`, `is_dir`, then the metadata itself). What it
  returns is unchanged: a link is what it points to, and a path whose
  metadata cannot be read is neither a file nor a directory.
- A non-solid folder whose LZMA or LZMA2 coder runs on one thread is now
  encoded on the thread that adds it, by an encoder that is kept and reused
  for the next such folder, where each folder started an encoder thread and
  built a new encoder (window, tables and a 1 MiB read buffer) of its own.
  `push_archive_entries_non_solid` keeps one encoder per worker for the
  length of the call; `push_archive_entry` also codes on the calling thread
  but still builds an encoder per entry. Folders under 4 KiB now share one set of encoder settings: the
  size hint given to the encoder is at least 4 KiB, below which the
  encoder's dictionary does not shrink further anyway. The archive is the
  same bytes, also built with `--cfg sevenz_turbo_unthreaded`, where the
  writer ignores an LZMA2 block plan and so does this coder; on wasm an
  LZMA2 coder with a block plan is left to the writer.
- New `ArchiveEntry::from_metadata` builds an entry from metadata the caller
  already holds, such as a directory walk's `DirEntry::metadata`, where
  `from_path` looks the path up again. On Windows that lookup opens the
  file, so a tree walked and added with `from_path` opened every file twice.
  `from_path` is now `from_metadata` over the path's metadata.
- `push_archive_entries` (a solid folder) also codes a one-thread LZMA or
  LZMA2 coder on the calling thread, pulling the members itself, where it
  read them through a 1 MiB buffer and handed them to an encoder thread in
  1 MiB chunks, up to four in flight. At level 1 on one thread that was
  half the writer's peak memory. The archive is the same bytes.
- The `SEVENZ_TURBO_MT_TRACE` line of the parallel LZMA2 reader also gives
  the most the decoder held (`peak_held`, what a memory limit governs), the
  most the reader had queued for it (`peak_queue`, outside the limit), the
  most the two came to at one moment (`peak_sum`), and the waves of the
  decode (`waves`): for each time the delivering thread went to wait, the
  runs claimed since the last wait and the runs out with workers. Nothing
  is sampled when the variable is unset. `peak_held` and the decoder's
  reasons for holding a run back (`dispatch_held_back`, `input_refused`,
  `sheds`) come from `lzma-turbo`'s `AdaptiveLedger`. `wait` splits the
  delivering thread's waits on a worker by why fewer runs were being
  decoded than there are threads: none (`full`), finished blocks queued
  behind the one waited on (`ordered`), no complete run at the cursor
  (`input`), or a complete run held back (`held`).
- A parallel LZMA2 decode under a memory limit keeps the decoder and the
  reader's queue inside it together. The memory contract: the limit governs
  the decoder's held bytes (`AdaptiveLedger`'s `input_bytes`,
  `runs_out_bytes`, `runs_waiting_bytes` and `parked_bytes`) plus the
  reader's queue (pieces read and not yet handed over, at their capacity).
  Dictionaries, coder state and allocator slack are documented additions,
  not bounded by it. The queue used to sit outside the limit, and a decode
  could hold a read more than it allowed. The reader now does four things.
  It sets the decoder's limit to what is left after its queue before every
  feed and drain. It reads again only when a read fits beside both. It asks
  the decoder's own `dispatch_cost` whether another run fits, so a parked
  output buffer is not charged twice. And it cuts a small head off a read
  as an exact-size copy, so a few kilobytes are not charged, or kept, as
  the whole 4 MiB read. Requires `lzma-turbo` 0.8.0.
- A parallel LZMA2 decode gives every thread a run in the first wave. The
  decoder holds one run pair (input and output) per thread under any larger
  limit, but it learns the pair's size only when it scans, which it does in
  a drain. The reader used to feed its whole read-ahead before the first
  drain, so that drain found no room for some of the outputs. On 128 MiB
  runs at four threads, two workers started a run late, and every later
  wave waited on that offset. Until the decoder has scanned a run, the
  read-ahead now reserves an output for every run fed, and works to the
  pair bound itself.
  It also declares the last run of that first wave, by handing over the
  next run's header, so that run's worker is not left idle until a run is
  handed back. After the first scan the decoder's own bound governs, as
  before: reserving there as well held back the run a finishing worker
  would have taken next and lost a thread for the rest of the decode.
- A parallel LZMA2 decode no longer holds a run's input queued in the
  reader while the decoder refuses it. The reader used to hand a run over
  only once it had read the run's end, so on 128 MiB runs a whole run sat in
  its queue (130 MiB at peak) while the decoder, holding one run pair per
  thread, refused it. Past the first wave the reader now hands each read over
  as it is read, and lets the decoder's refusal stop the feed. While the
  decoder refuses, the reader reads no more than two pieces ahead. The queue
  now peaks at one 4 MiB read.
- A parallel LZMA2 decode of small runs stops reading ahead where it means
  to. Past the first wave each read goes over whole, and the feed stopped
  only if it happened to end at a run boundary, which it seldom did; and it
  counted only the runs the decoder had scanned, which it does in a drain,
  so the runs just fed were invisible to it. So it fed on until the decoder
  refused input at its pair bound: on the 1 MiB runs `7zz -mx1` writes, at
  eight threads, about seventy runs held where the read-ahead asks for
  sixteen. The feed now counts every run fed that no worker has taken, and
  once the read-ahead is full it hands over the run in hand to its end and
  stops there, declaring it with the next header. Linux x86-64, 1 GiB,
  median of 3, peak RSS: `aes_mx1` 33 to 28 MiB at two threads and 81 to 63
  MiB at eight; `media_mx1` 33 to 29 MiB and 81 to 69 MiB. Wall time is
  unchanged within 1.5%.
- A parallel LZMA2 decode of runs of at most 4 MiB, which is what `7zz` writes
  at `-mx1` and for dictionaries up to 1 MiB, reads its input in 1 MiB pieces
  and keeps one run per thread waiting instead of two. A piece is let go
  only once every run in it is done, so a 4 MiB read held four small runs for
  each one being decoded. The run size comes from the runs already scanned,
  and before the first has closed from the dictionary, so the first reads
  are already the right size. Larger runs are read as before: on 128 MiB
  runs either change alone cost a sixth of the wall time at four threads.
  Linux x86-64, 1 GiB, median of 3, peak RSS: `aes_mx1` 28 to 19 MiB at two
  threads and 63 to 48 MiB at eight; `media_mx1` 28 to 18 MiB and 66 to 52
  MiB; 4 MiB runs at two threads 50 to 33 MiB. Wall time is unchanged within
  the run-to-run spread, and the 2 GiB, 128 MiB-run decode at four threads is
  unchanged.
- A parallel LZMA2 decode of those small runs keeps no more of them waiting
  than its idle workers can take plus two, so with every thread busy two
  runs wait instead of one per thread. Linux x86-64, 1 GiB, median of 3,
  peak RSS at eight threads: `aes_mx1` 46 to 39-41 MiB and `media_mx1` 50
  to 40-43 MiB; at two threads within 1 MiB of before. Wall time is
  unchanged within the run-to-run spread.
- An LZMA or LZMA2 folder coded on an encoder thread reuses the buffers that
  carry its input to that thread and its output back, where it allocated a
  new one for every 1 MiB of input and for every piece of output, each freed
  on the other thread. That churn left the allocator holding several
  megabytes it could not hand back. The input now travels in 256 KiB pieces,
  and at most five are allocated for a folder of any length. A folder coded
  in parallel blocks hands each finished block to the writer as the buffer
  it was coded into, where it copied it, so a block is no longer held twice
  (about 56 MiB less at level 5 on four threads). The archive is the same
  bytes.
- The compressed output of an LZMA or LZMA2 folder coded on an encoder
  thread no longer queues without bound while it waits for the writer. A
  block-parallel coder whose output outran the sink held every finished
  block until the writer drained them. The encoder now waits once 4 MiB of
  output is queued (a single larger block waits alone), and the writer takes
  output while it waits for room to queue input, so neither side stalls the
  other. The archive is the same bytes.

## 0.26.1 - 2026-09-29

- Fixed: `ArchiveLimits::memory_limit_bytes` bounds a coder chain as a whole,
  not each coder on its own. An encoded header was decoded with every coder
  checked alone against the limit, so a crafted header chaining several LZMA,
  LZMA2 or PPMd coders, each just under it, held the limit several times over
  in decoder state. The chain's dictionaries and models — by the same model
  the per-coder check uses, dictionaries clamped to each coder's output — are
  now summed and checked before any coder is built, and a chain over the
  limit is refused with the `Error::MaxMemLimited` a single coder over it
  already raised, carrying the chain's total. A Zstandard window in the same
  chain is capped at what the sized coders leave. A header whose coders fit
  together reads exactly as before.
- The same sum bounds block decodes. Under `ArchiveReader::with_limits` it
  refuses nothing new, because `decoder_memory_estimate` already bounds the
  sum from above; it is what now bounds a `BlockDecoder` built with limits of
  its own, which reports it located in the block, as it reports a single
  coder over the limit.
- Fixed: a decoded header is read into a buffer reserved once at its declared
  size, which `max_header_unpacked_bytes` has already bounded, instead of one
  grown by doubling. Doubling could leave the buffer at nearly twice the
  declared size, past the limit it was checked against. The reservation is
  fallible, so a caller that lifted the limit is refused rather than aborted
  by a header claiming more than the process can have.

## 0.26.0 - 2026-09-22

- The parallel LZMA2 reader sizes itself from the stream in front of it. It
  walks the chunk headers with `Lzma2RunScanner` as they arrive, so it knows
  where every dictionary-reset run ends, and what it costs packed and
  unpacked, before it hands anything over. What it reads ahead is then a
  number of complete runs per thread rather than a fixed number of bytes: a
  decode of an archive with large runs no longer pulls a gigabyte of packed
  input in behind itself, and one with small runs no longer starves its
  workers a megabyte at a time. Decoded bytes are unchanged; a caller that
  sets no thread count and no memory limit sees the same API it always did.
- A caller's memory limit is spent on decoding before it is spent on reading
  ahead. The limit decides how many runs can be inside the decoder at once,
  and so how many threads can actually be decoding; the read-ahead is sized
  by that count, and a budget with no room for another run stops the feed
  even where the backlog looks thin. Feeding right up to a limit used to
  leave no room to dispatch what had just been fed, which turned a
  four-thread decode under 512 MiB into a single-threaded one.
- An incompressible archive is decoded narrow. Runs whose packed size is
  within a tenth of their unpacked size hold data an encoder could not beat;
  such a stream is read-bound rather than decode-bound, so the reader caps it
  at two threads, and reads ahead for those two rather than for the count the
  caller asked for. Two runs in a row settle the question either way, so an
  archive that is a film beside a text file narrows for the one and widens
  again for the other. Measured on 1.5 GiB of incompressible payload at eight
  threads: 3.06 GB resident and 1.72 s becomes 0.90 GB and 1.42 s. Archives
  whose runs compress are untouched.
- The packed input reaches the decoder as the pieces it was read in, handed
  over by value and never copied down over itself, and each read refills the
  piece the decode hands back rather than asking the allocator for another.
- Fixed: the calling thread's chase decoder was re-armed on an empty decoder
  rather than on what was left to decode, so a stream whose last run was
  still arriving could sit with the workers idle. It is now armed from the
  three cases that need it — a stream written as one run, a run larger than
  the allowance, and a stream whose headers could not be walked — and a
  decode that has runs waiting or out with a worker leaves it off.
- Fixed: a feed the decoder refused for want of room is offered again after
  the next drain instead of waited on. Waiting put the one thread that hands
  output to the caller to sleep; on a three-gigabyte archive at eight threads
  that cost 42 s against 18 s, with the workers idle for most of it. A decode
  that genuinely cannot proceed is still given up on rather than hung: the
  reader waits only once it has established there is nothing else it can do,
  and reports a stall rather than blocking for ever.
- Fixed: the end of the input is announced to the decoder only once the last
  piece read has been taken, rather than the moment the source runs dry. A
  decoder told the input is over treats a stream it cannot then finish as a
  corrupt archive, so a tail still queued in the reader — bytes beyond a run
  that has not closed, or a piece the allowance had no room for — could have
  been reported as damage. A stream that really is short still says so.
- Requires `lzma-turbo` 0.6.0.
- The wasm host-AES conformance harness moved out of `tests/` into
  `tools/wasm-conformance`, a workspace member excluded from every
  workspace-wide `cargo test` and `cargo clippy` (CI, release, `cargo xtask
  release`). `wasmtime` was a dev-dependency of the crate itself, so every
  `cargo test` built it; now only the harness's own CI job does. No change to
  the crate or its features.

## 0.25.0 - 2026-09-19

- LZMA and LZMA2 are encoded by `lzma-turbo`'s port of the SDK encoder.
  `compress` no longer pulls `lzma-rust2`; the archive writer's LZMA (`03 01
  01`) and LZMA2 (`21`) coders are `src/codec/lzma_turbo/writer.rs`, a `Write`
  over `lzma-turbo`'s pull-driven encoders. The encoder runs on a thread,
  pulling from a bounded channel the writer feeds, so an entry of any size
  streams through it with a bounded amount in flight; where no thread can be
  started (`wasm32-unknown-unknown`) the writer holds the input and encodes
  it on finish, producing the same bytes. `Lzma2Options::from_level_mt`'s
  threads and chunk size are `lzma-turbo`'s block threads and block size.
- New feature `lzma-rust2-encoder`, off by default: encode LZMA and LZMA2
  with `lzma-rust2`'s pure-Rust encoders instead, as every earlier version
  did. `LzmaOptions` and `Lzma2Options` are the same types either way; they
  now hold their own level, dictionary and nice length rather than wrapping
  `lzma-rust2`'s option types, and a level means the same dictionary under
  both encoders: the table `lzma-rust2` and xz use, 256 KiB at level 0 to
  64 MiB at level 9, rather than the SDK's own level defaults. The public
  API does not change.
- Requires `lzma-turbo` 0.5.0 with its `enc` feature.
- Fixed: the LZMA2 property byte for a dictionary that is not a power of
  two or three times one was rounded down (5 MiB was written as 4 MiB) while
  the encoder used the full window, so a reader could hit a match beyond its
  dictionary. It is now rounded up, as `Lzma2Enc_WriteProperties` does.
- BCJ2 is `lzma-turbo`'s too. Its 0.5.0 ports the SDK's `Bcj2.c` and
  `Bcj2Enc.c`, bit-exact against them, so the last conversion vendored from
  `lzma-rust2` is gone: `src/codec/filter/bcj2.rs` is now a `Read` over
  `lzma_turbo::filters::bcj2::Bcj2Dec` that buffers the folder's four
  sub-streams and refills whichever the decoder runs dry. Decoded bytes are
  unchanged; `Bcj2Reader::new` keeps its signature.
- Extraction is confined to the destination. `decompress` and every
  convenience around it open the destination once as a directory handle
  (`cap-std`, under the default `util` feature, native targets only) and
  create every entry relative to it, so a symbolic link anywhere below the
  destination, whether it points outside or back inside, is refused instead
  of followed, and a destination renamed mid-extraction keeps receiving the
  files. An existing file is replaced rather than truncated in place, so a
  hard link to it is left alone. `default_entry_extract_fn` keeps its
  signature but now requires its `dest` to end in the validated entry name,
  which it uses to find the root; a callback that renames entries on the way
  out must do its own writing. Names that are empty, only dots, carry a NUL,
  a drive letter or a root are refused before any path is built, on wasm too.
  No entry is ever created as a symbolic or hard link, so an archive cannot
  plant a link for a later entry to be written through. That is the 7-Zip
  class fixed in 25.01 as CVE-2025-55188 (a link back inside the extraction
  root, then a write through it, escalating to an arbitrary file write), the
  same technique in its ZIP reader as CVE-2025-11001 and CVE-2025-11002, and
  p7zip's original CVE-2015-1038; none of them has a path through this crate.
- Every convenience function has a `_with_limits` form taking an
  `ArchiveLimits`, checked when the archive is opened and before any output
  or callback: `decompress_with_limits`, `decompress_file_with_limits`,
  `decompress_with_extract_fn_and_limits`,
  `decompress_file_with_extract_fn_and_limits` and the `_with_password`
  variants. The wasm entry point gains `decompress_with_limits` and
  `default_archive_limits`, and `ArchiveLimits` is a `wasm_bindgen` class
  there.
- `ArchiveLimits::max_aes_kdf_rounds`, a new public field, default 2^28:
  the SHA-256 rounds one `Password` will spend deriving keys, across the
  encoded header and every block, charged before hashing. A derivation the
  password's cache already holds costs nothing, so an archive whose blocks
  all share one salt and work factor, which is what 7-Zip writes, costs one
  derivation however many entries it has. `Limit::AesKdfRounds` names it.
  Building a struct literal without `..Default::default()` needs the new
  field.
- The derived-key cache belongs to the `Password` rather than the process:
  it is dropped with it, and a clone starts with none. `Password`'s `Debug`
  prints `Password([REDACTED])`. The password bytes, the cached key, the
  AES key schedule (RustCrypto's `zeroize` feature) and the decoder's
  plaintext buffer are cleared on drop.
- Sub-stream and digest counts are summed with overflow checks and held to
  `max_entries`, as `CInArchive::ReadSubStreamsInfo` holds them.
- An AES work factor over `max_aes_cycles_power` is refused when the archive
  is opened, not when its block is first decoded.

## 0.24.0 - 2026-09-18

- The BCJ and delta filters are `lzma-turbo`'s. They were vendored from
  `lzma-rust2` 0.20.1, which meant this crate carried a second port of the
  same eight branch converters and the same delta filter from the same
  public-domain C that `lzma-turbo` - already the dependency the LZMA comes
  from - ports as well. `src/codec/filter/bcj/` is gone, and `BcjFilter` and
  `Delta` are handles on `lzma_turbo::filters::{bcj, delta}`. The readers and
  writers around them are untouched, so the crate's own API does not move.
  BCJ2 stays vendored: `lzma-turbo` has no BCJ2, because .xz has none.
- The delta filter is a straight walk rather than a 256-byte ring. Upstream's
  carried its history in a ring with a moving index and paid two masked index
  computations, a load, an add and a store for every byte in both directions;
  `lzma-turbo`'s keeps the C's own shape, a history prefix that is shifted, and
  at distances of sixteen and up adds a block of `distance` bytes at a time,
  which is legal because any `distance` consecutive outputs depend on bytes
  that are already final.
- On a Zen 2 machine, medians of nine interleaved rounds, one thread,
  extracting 39.7 MiB through delta and 64.0 MiB through BCJ: delta at
  distance 64, 0.456s to 0.414s (+9.2%); at distance 4, 0.313s to 0.290s
  (+7.4%); the x86 branch filter, 0.386s to 0.363s (+6.0%); ARM64, +0.3%. The
  same archives with no filter in the chain move 0.00%, which is the control.
- Requires `lzma-turbo` 0.4.0 and turns on its `filters` feature: the
  converters alone, without the `.xz` stream layer, its readers or `crc-fast`.

## 0.23.4 - 2026-09-18

- Fixed: the `util` feature did not compile for `wasm32-unknown-unknown` unless
  `compress` was also on. `src/util/wasm.rs`'s `compress` export names
  `ArchiveWriter` and `SourceReader`, which live behind the `compress` feature,
  and it was not gated on it. The export is now `#[cfg(feature = "compress")]`,
  so a decode-only wasm guest built with `util` exports `decompress` alone. No
  change to the `default_wasm` feature set's public API. A CI lane checks the
  decode-only-with-`util` set so this cannot regress.

## 0.23.3 - 2026-09-18

- New `crypto-host` feature and `sevenz_turbo::hooks` module: on a `wasm32`
  target the bulk AES-256-CBC decrypt is delegated to a hook the embedding host
  installs, so a guest with no AES-NI and no ARMv8 cryptography extensions does
  not run the block cipher itself. Accepted but inert on native targets. See
  the "WASM support" section of the README for the contract, and
  `examples/wasm_host_extract_conformance.rs` for a complete reference
  embedding.

## 0.23.2 - 2026-09-17

- Raised `lzma-turbo` to 0.3.3. It fixes an `.xz` block that declares no
  uncompressed size and runs past the caller's output cap: that was reported as
  corrupt data instead of a cap hit.

## 0.23.1 - 2026-09-17

The first release under the name `sevenz-turbo`, on `lzma-turbo` 0.3.1. The code
is 0.23.0's; only the crate names changed.

## 0.23.0 - 2026-09-17

The first release of the fork. It is everything in the [Fork](#fork)
section above, together with these upstream changes, which were unreleased at
the fork point and ship here for the first time.

### Added

- `LzmaOptions::set_nice_len`, `LzmaOptions::set_dictionary_size` and `Lzma2Options::set_nice_len`.
- `EncoderConfiguration` can be built from `LzmaOptions` with `into()`.

### Fixed

- Reject unsupported LZMA and LZMA2 encoder dictionary sizes with an error before allocating the encoder or starting
  compression workers, avoiding capacity-overflow panics.
- Improved decompression performance for non-solid 7z archives containing many files.
- Threaded LZMA2 decoding of archives made of many small runs - what `7zz -mx1` writes for data that does not
  compress, 1 MiB a run - scales with the thread count. It took two changes: `lzma-turbo` 0.3.0 no longer moves its
  whole input buffer after every run, and the reader here stops reading ahead once there are two runs per thread
  waiting and tops that up before every drain, instead of reading a gigabyte before the first worker started. A 1 GiB
  archive of that shape at 18 threads: 4.6 s before, 1.1 s after, level with `7zz t`.

## 0.22.2 - 2026-08-25

### Fixed

- Fixed build process and tests for all cargo feature combinations (#126)

## 0.22.1 - 2026-08-25

### Fixed

- Fixed memory check on 32-bit systems when using PPMd (#134)

## 0.22.0 - 2026-08-23

### Changed

- Bumped `lzma_rust2` to 0.20.
- Expose coder's properties (#131, thanks @lukr54)
- Let a solid pack be compressed away from the writer (#132, thanks @lukr54)

## 0.21.5 - 2026-08-16

### Changed

- Bumped `lzma_rust2` to 0.19.
- Batch AES-CBC decryption to improve performance.

## 0.21.4 - 2026-08-01

### Fixed

- Fixed an integer overflow when summing attacker-controlled coder stream counts while parsing a block header. Malformed
  archives panicked in debug builds and bypassed the stream-count bound in release builds. They are now rejected with an
  error. (#127, thanks @tyrex-vberthier)

## 0.21.3 - 2026-07-05

### Changed

- Cache most recent AES key derivation to improve performance (#118, thanks @jdlien)

### Fixed

- Hardened the library against malicious or malformed archives that could otherwise cause a panic, an infinite loop, or
  an unbounded allocation while parsing or decoding.

## 0.21.2 - 2026-07-01

### Fixed

- Decode 7z folders that layer a single-input filter (e.g. Delta) on top of a BCJ2 coder (`Method = Delta BCJ2`). These
  folders previously failed to decode with an
  `Unsupported method` error because the decoder required the folder's final output coder to be BCJ2 itself. (#117,
  thanks @trevorWieland)

## 0.21.1 - 2026-06-23

### Fixed

- Fix security issue were malicious 7z files could write files outside the destination directory. Reported by @lintowe
  (#116)

## 0.21.0 - 2026-04-25

### Changed

- Bumped MSRV to 1.93 (required by `nt-time` 0.15)

### Updated

- Bumped `nt-time` to 0.15 (#105)
- Bumped `aes` to 0.9 and `cbc` to 0.2 (cipher 0.5) (#111, #110)

### Fixed

- `K_ANTI` property block was not written for archives containing only anti-items, so anti-items were extracted as
  0-byte files instead of acting as deletion markers (#112, thanks @uraf)
- `compress_path` no longer emits a spurious entry for the root directory itself when compressing a directory tree (#79,
  thanks @super1207)

## 0.20.2 - 2026-02-24

### Fixed

- Updated internal dependencies `lzma-rust2` to v0.16 and `getrandom` to v0.4

## 0.20.1 - 2026-01-01

### Fixed

- Fix bug where small headers of encrypted files were not encrypted (#98)

## 0.20.0 - 2025-12-07

### Changed

- Expose public getters for streaming archive data @vihu (#93)

### Fixed

- Brotli codec decompression is not working without compress (#95)

## 0.19.4 - 2025-11-26

### Fixed

- Fixed "AES256 properties too short" bug (#91)

## 0.19.3 - 2025-11-01

### Updated

- Target ppmd-rust v1. Since ppmd-rust is stable and has a major version, we will target it directly to reduce
  maintenance burden.

## 0.19.2 - 2025-10-29

### Updated

- Target lzma-rust2 v0.15.

## 0.19.1 - 2025-09-23

### Fixed

- Removed too strict debug_assert as reported in #81
- Removed incompatibility issues when using the `ArchiveWriter::push_source_path()` with single files. We now properly
  handle both cases were a file or a directory is given to the function.

## 0.19.0 - 2025-09-20

### Changed

- Breaking change: Rename identifiers to follow Rust API Guidelines @sorairolake (#59)

### Updated

- Target lzma-rust2 v0.14.
- Updated the documentation to include the BCJ2 codec again. It was never gone, only overlooked.

### Fixed

- Improved compatibility with third party utilities when creating 7z files (esp. Windows's Explorer and macOS's Finder).
- Improve docs.rs feature compatibility @sorairolake (#56)

### Removed

- Removed the dependency to `byteorder`.

## 0.18.0 - 2025-08-25

### Added

- Add support for BCJ IA64 filter.
- Add support for BCJ RISC-V filter.
- Added encoder support for all BCJ filter.

### Updated

- Target lzma-rust2 v0.10.

### Changed

- Changed one of LZMA2Options::from_level_mt's parameter name.

## 0.17.1 - 2025-07-26

### Updated

- Target lzma-rust2 v0.6.

## 0.17.0 - 2025-07-25

### Added

- Added muti-threading support for LZMA2 compression & decompression.
- Added `ArchiveReader::set_thread_count()` to set the thread count when decoding with multiple threads. Only LZMA2 is
  supported right now. `ArchiveReader` will set the thread_count with the help of
  `std::thread::available_parallelism` as default.
- Added missing documentation for the public API.

### Changed

- Split `LZMA2Option` into `LZMAOptions` and `LZMA2Options` to better support multi-threading encoding for LZMA2.
- Removed `EncoderOptions::Num` enum, which means encoders needs to be configured with their respected option struct.

## 0.16.2 - 2025-07-16

### Updated

- Target lzma-rust2 v0.4 which increases the encoding speed.

## 0.16.1 - 2025-07-12

### Updated

- Target lzma-rust2 v0.3 which increases the decoding speed dramatically (around 50% on x86_64).

## 0.16.0 - 2025-07-04

### Changed

- Removed a lot of exports that were not used in the public facing API.
- Expose all compression and filter method options via the `encoder_options` module.
- Renamed the following structs in an attempt to make the API easier to navigate:
    - `SevenZArchiveEntry` -> `ArchiveEntry`
    - `SevenZReader` -> `ArchiveReader`
    - `SevenZWriter` -> `ArchiveWriter`
    - `SevenZMethod` -> `EncoderMethod`
    - `SevenZMethodConfiguration` -> `EncoderConfiguration`
    - `MethodOptions` -> `EncoderOptions`
    - `Folder` -> `Block`
- Renamed all instances of "folder" in the API to "directory" instead to clearly distinct from the "folder" in the 7z
  format specification and align with the std fs module.
- Internal `Archive`, `SteamMap`, `Coder` and `Block` fields are removed from the public API.
- What the 7z specification calls "folders" are a false friend and we instead call them "blocks".
- Every API that takes a password now uses the `Password` struct instead. Added helper functions to create password from
  strings and raw bytes.
- The needed features for WASM changed. Please use the "default_wasm" feature.
- Removed the hard dependency to `nt-time` by creating our own time struct `NtTime`. The feature can be used to convert
  to and from `nt-time::FileTime`.

### Removed

- Removed the dependency to `bit-set` and `filetime_creation`.
- `nt-time` dependency is now optional.

### Updated

- PPMd crate to version v1.2, which fixes the compatibility issues with existing 7z files using PPMd7.

## 0.15.3 - 2025-06-28

### Fixed

- Properly finish PPMd files.

## 0.15.2 - 2025-06-27

### Fixed

- No functional updates.
- Moved lzma-rust2 and ppmd-rust into their own crates.

## 0.15.1 - 2025-06-22

### Fixed

- Updated outdated documentation.

## 0.15.0 - 2025-06-22

### Updated

- Target optional dependency bzip2 v0.6.

### Changed

- The PPMd crate is using a native Rust version that is validated with Miri. All 7zip supported compression algorithms
  (LZMA, LZMA2, BZIP2 and PPMd) have now Rust native implementations and don't need a C compiler.
- All standard compression algorithms of 7zip are enabled by default.
- Use default feature of lz4_flex

## 0.14.1 - 2025-06-02

### Added

- Added support for ARM64 BCJ filter (by Benkol003).
- Add support for encoding LZ4 with skippable frames.

### Fixed

- Fixed decompressing LZ4 that contain skippable frames.

### Changed

- Use lz4_flex instead of lz4, which is a faster Rust native implementation. The only downside is, that only one
  compression level is supported.

## 0.13.2 - 2025-05-01

### Fixed

- Loose version restrictions on some dependencies.

## 0.13.1 - 2025-04-05

### Fixed

- Fix broken WASM build.

## 0.13.0 - 2025-03-31

### Fixed

- Fix the bug where the optional compression methods did not finish properly and created invalid entries when writing 7z
  archives.

### Changed

- Moved `CountingWriter` from lzma to sevenz crate, since it was an internal implementation detail of the sevenz crate.
- Remove implicit way to call `finish()` by `calling write(&[])` on the lzma and lzma2 writer. This was again an
  implementation detail of the sevenz. `finish()` now also takes `self`, like other compression libraries.

### Added

- `LZMAWriter` and `LZMA2Writer` now expose the inner writer and also return it when calling `finish(self)`.

## 0.12.1 - 2025-03-10

### Fixed

- Fix broken LZ4 feature compilation

## 0.12.0 - 2025-02-28

### Added

- Support for Delta filter compression
- Support for PPDm compression / decompression

## 0.11.0 - 2025-02-26

### Updated

- Updated dependency nt-time to 0.11

### Changed

- Introduced a new feature "util", so that users can deactivate those functionality, if not needed
- Added a lot of documentation tags for docs.rs
- Update of nt-time introduce the need to increase MSRV to 1.85

## 0.10.0 - 2025-02-26

### Changed

- The Brotli codec now supports the skippable frame encoding found in zstdmt (used by 7zip ZS and NanaZip). This is the
  default format, since it seems to be the default for user facing programs. The default data a frame contains is 128
  KiB.

## 0.9.0 - 2025-02-25

### Added

- Add `SevenZReader::file_compression_methods()`.

### Fixed

- Improve compatibility with third party programs (tested with 7-Zip ZS 1.5.6)
  bzip2, LZ4 and ZSTD now work flawless. BROTLI isn't compatible right now.

## 0.8.0 - 2025-02-25

### Added

- Optional support for compressing / decompressing with BROTLI
- Optional support for compressing with bzip2
- Optional support for compressing / decompressing with DEFLATE
- Optional support for compressing / decompressing with LZ4
- Optional support for compressing with ZStandard

### Changed

- Replaced all unsafe code from sevenz-rust2 and lzma-rust2 with safe alternatives
- sha2 is now an optional dependency that is activated when using the aes256 features
- Update of the documentation
- Removed the need to provide the length of a reader when reading an archive file

## 0.7.0 - 2025-02-24

This release should be mostly compatible with the old 0.6.1. The breaking changes are:

- Previously deprecated functionality were removed
- Spelling issues were fixed in some API names

### Added

- `SevenZReader::readFile()` added
- `SevenZArchiveEntry::new_file()` and `SevenZArchiveEntry::new_folder()` factory functions added
- Compression method COPY is now supported

### Changed

- Updated dependency bit-set v0.8
- Updated dependency bzip2 v0.5
- Updated dependency nt-time v0.10
- Use crc32fast instead of crc

### Fixed

- Replaces insecure usage of rand with getrandom
- Renamed `get_memery_usage()` into `get_memory_usage()`
- Renamed `compress_encypted()` into `compress_encrypted()`

### Removed

- Removed deprecated `FolderDecoder`. Use `BlockDecoder` instead
- Removed deprecated `SevenZWriter::create_archive_entry()`. Use `SevenZArchiveEntry::from_path()` instead

## 0.6.1 - 2024-07-17

- Fixed 'unsafe precondition (s) violated'. Closed #63

## 0.6.0 - 2024-04-05

- Added support for encrypted headers - close #55
- Return a consistent error in case the password is invalid - close #53

## 0.5.4 - 2023-12-13

- Added docs
- Renamed `FolderDecoder` to `BlockDecoder`
- Added method to compress paths in non-solid mode
- Fixed entry's compressed_size is always 0 when reading archives.

## 0.5.3

Fixed 'Too many open files' Reduce unnecessary public items #37

## 0.5.2 - 2023-08-24

Fixed file separator issue on Windows system #35

## 0.5.1 - 2023-08-23

Sub crate `lzma-rust` code optimization

## 0.5.0 - 2023-08-19

- Added support for BCJ2.
- Added multi-thread decompress example

## 0.4.3 - 2023-06-16

- Support write encoded header
- Added `LZMAWriter`

## 0.4.2 - 2023-06-10

- Removed unsafe code
- Changed `SevenZWriter.finish` method return inner writer
- Added wasm compress function
- Updates bzip dependency to the patch version of 0.4.4 ([#23](https://github.com/dyz1990/sevenz-rust/pull/23))

## 0.4.1 - 2023-06-07

- Fixed unable to build without default features

## 0.4.0 - 2023-06-03 - Solid compression

## 0.3.0 - 2023-06-02 - Encrypted compression

- Added Encrypted compression

## 0.2.11 - 2023-05-24

- Fixed numerical overflow

## 0.2.10 - 2023-04-18

- Change to use nt-time crate ([#20](https://github.com/dyz1990/sevenz-rust/pull/20))
- Fix typo ([#18](https://github.com/dyz1990/sevenz-rust/pull/18))
- make function generics less restrictive ([#17](https://github.com/dyz1990/sevenz-rust/pull/17))
- Solve warnings ([#16](https://github.com/dyz1990/sevenz-rust/pull/16))
- run rustfmt on code ([#15](https://github.com/dyz1990/sevenz-rust/pull/15))

## 0.2.9 - 2023-03-16

- Added bzip2 support ([#14](https://github.com/dyz1990/sevenz-rust/pull/14))

## 0.2.8 - 2023-03-06

- Fixed write bitset bugs

## 0.2.7 - 2023-03-05

- Fixed bug while read files info

## 0.2.6 - 2023-02-23

- Added zstd support and use enhanced filetime lib ([#11](https://github.com/dyz1990/sevenz-rust/pull/11))
- Fixed lzma encoder bugs

## 0.2.4 - 2023-02-16

- Changed return entry ref when pushing to writer ([#10](https://github.com/dyz1990/sevenz-rust/pull/10))

## 0.2.3 - 2023-02-07

- Fixed incorrect handling of 7z time

## 0.2.2 - 2023-01-31 - Create sub crate `lzma-rust`

- Move mod `lzma` to sub crate `lzma-rust`
- Modify GitHub Actions to run tests with --all-features

## 0.2.0 - 2023-01-08 - Added compression supporting

- Added compression supporting

## 0.1.5 - 2022-11-01 - Encrypted 7z files decompression supported

- Added aes256sha256 decode method
- Added wasm support
- Added new tests (for Delta and Copy) and GitHub Actions CI ([#5](https://github.com/dyz1990/sevenz-rust/pull/5))
  by [bfrazho](https://github.com/bfrazho)

## 0.1.4 - 2022-09-20 - Replace lzma/lzma2 decoder

- Chnaged new lzma/lzma2 decoder

## 0.1.3 - 2022-09-18 - add more bcj filters

- Added bcj arm/ppc/sparc and delta filters
- Added test for bcj x86 ([#3](https://github.com/dyz1990/sevenz-rust/pull/3)) by [bfrazho](https://github.com/bfrazho)

## 0.1.2 - 2022-09-14 - bcj x86 filter supported

- Added bcj x86 filter
- Added LZMA tests ([#2](https://github.com/dyz1990/sevenz-rust/pull/2)) by [bfrazho](https://github.com/bfrazho)
- Fixed extract empty file

## 0.1.1 - 2022-08-10 - Modify decompression function

## 0.1.0 - 2022-08-10 - Decompression
