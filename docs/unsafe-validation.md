# Native and unsafe-code validation

The extra CI lanes target sevenz's own source-buffer allocation and its
archive APIs. They do not fetch or run lzma-turbo's private kernel tests.
The ordinary test matrix independently runs AWS-LC, all features, no default
features, and RustCrypto on Linux x86-64, Linux ARM64, macOS ARM64 and Windows
x86-64. Each combination gets its own runner and uses Nextest. Rustdoc runs
doctests separately.

## Memory and ownership

The sevenz-owned unsafe operations are in `src/writer/input_buffer.rs`:
initializing an aligned allocation before `assume_init`, and the Linux
`madvise` call over that owned allocation. New unsafe operations must extend
this inventory and the focused selectors rather than run the whole archive
suite under Miri.

The crate's own threading has no unsafe code, but it is where a data race or
a use after free across threads would come from: the ordered job queue
(`src/ordered.rs`, which the folder-parallel decode and the parallel encoders
run on), the coder-stage pipeline (`src/pipeline.rs`), and the parallel LZMA2
decode driven through `lzma-turbo`. The sanitizer and guard lanes run focused
tests of those as well.

| Lane | Scope | Features | Platforms |
| --- | --- | --- | --- |
| `guard-pages` | Source, codec and output allocations during pull, Send and prepared-block APIs; short reads, failure and teardown. Parallel LZMA2 decode at two threads (cursor and positional source), BCJ2 written and committed, AES-256 encode and decode with a wrong-password refusal, PPMd encode and decode | `compress,aes256,aws-lc-crypto,ppmd` | All four native platforms |
| `unsafe-valgrind` | The allocation unit tests and source-buffer lifecycle integration tests, on separate runners | `compress` | Linux x86-64 and ARM64 |
| `unsafe-sanitizer` | The allocation and lifecycle tests above (`compress`); `ordered::tests`, `pipeline::tests` and `reader::store_overlap::tests`; `tests/parallel_decode_tests.rs` and `tests/store_overlap_tests.rs`. Each under AddressSanitizer and ThreadSanitizer, with instrumented std | `compress`; `compress,aes256,native-crypto,ppmd` for the concurrency tests | Linux x86-64 and ARM64 |
| `unsafe-miri` | Initialization, unsupported advice, panic cleanup, independent allocations, movement between threads, and allocation-policy boundaries | `compress` | One Linux runner per discovered portable test |

The sanitizer selectors, exactly:

- `--lib -E 'test(/^writer::input_buffer::tests::/)'` and
  `--test input_buffer_tests`, with `--features compress`;
- `--lib -E 'test(/^(ordered|pipeline|reader::store_overlap)::tests::/)'`,
  `--test parallel_decode_tests` and `--test store_overlap_tests`, with
  `--features compress,aes256,native-crypto,ppmd`.

`ordered::tests` covers in-order delivery, the staging bound, pacing,
cancellation by a failing consumer, a consumer moving past or ignoring a job
with its worker mid-send, panicking workers, consumers and state
constructors, at one to four workers, with heap-owning messages counted out
so that a leaked or doubly freed message fails the test. `pipeline::tests`
covers a failing and a panicking coder stage, a chain dropped while every
stage runs (and while a caller-fed pack stream is still being read), chunk
ownership transfer, refused stage threads, and every committed fixture chain
the lane's features build (BCJ2 over LZMA2 and over delta, BCJ x86 and
ARM64, delta, solid and non-solid LZMA, AES-256, PPMd) decoded through a
pipeline.
`tests/parallel_decode_tests.rs` decodes small multi-run LZMA2 archives at
two and four threads through a seekable cursor, `ArchiveReader::from_read_at`
and `ArchiveReader::open`, with and without the adaptive coder: a solid block
with zero-size sub-streams, a solid block with empty-stream files
interleaved between its streamed ones, a non-solid archive decoded
folder-parallel from the positional source, a BCJ2 chain over parallel LZMA2,
the committed BCJ2 archives, a corrupt block, and decodes a callback stops
or fails part way through. `reader::store_overlap::tests` covers a stored-folder stage's spans in
order and cut at its limit, an error after every byte before it and again
on a retried read, per-file checksums folded before the bytes arrive, a
dropped consumer stopping its stage, and a panicking stage resumed on its
consumer. `tests/store_overlap_tests.rs` decodes 12 MiB and 40 MiB solid
stored folders, plain and AES-256, at one to eight threads from a positional
source: byte-identical to one thread, damaged, read from a source that fails
part way, stopped early, and written to a sink that fails, each checking
that the stage threads stop with the caller. Those stage threads are not in
the Miri or Memcheck lanes, which hold no concurrency tests.
The concurrency tests use RustCrypto so that the
AES and SHA-256 inside the pipeline are Rust the sanitizers instrument.

The guard allocator supports the source buffer's full 2 MiB alignment; it
does not silently send this allocation to the system allocator. A child
process proves inaccessible pages actually fault. The archive cases place
allocations against each edge and exercise native encode and decode paths,
including assembly selected by the codec dependencies. Guard pages catch
buffer overruns of Rust-owned allocations; they do not prove all reads inside
a buffer are initialized, and AWS-LC's own allocations go through its
allocator, not the guarded one. Memcheck adds byte initialization and
allocation/free checking.

Miri runs through Nextest with strict provenance. Discovery uses the actual
`cfg(miri)` build, requires the known ownership tests, and creates one job per
test. It excludes the native advice test because the OS FFI is replaced by a
stub under Miri. Native Memcheck, sanitizers and guards cover that FFI and
the machine code Miri cannot execute. Sanitizers cannot instrument assembly
instructions; passing them alone is not evidence of assembly memory safety.

### Not instrumented

What no lane above runs, so that a pass is not read as more than it is:

- Memcheck runs only the input-buffer tests. The ordered queue, the pipeline
  and the parallel decodes are under the sanitizers and guards, not Valgrind.
- The parallel *encoders* (multi-threaded LZMA2, `push_archive_entries_send`,
  the folder-parallel `push_archive_entries_non_solid`, the BCJ2 side-coder
  threads) are exercised only as far as the guard cases and the
  concurrency tests write archives; no sanitizer test targets their
  teardown and error paths directly.
- The positional run API (`PositionalLzma2Folder`, its scanner and run
  decoder), the adaptive coder widening while a decode runs, and
  `decode_blocks_parallel` across a folder too large to stage are covered by
  the ordinary test matrix only.
- bzip2, Deflate, Zstandard, Brotli and LZ4 are not under any sanitizer or
  guard lane; bzip2 and Zstandard are C libraries the sanitizers would not
  instrument in any case.
- AWS-LC's AES and SHA-256 are under the guard lane only; the sanitizer lanes
  use RustCrypto. `lzma-turbo`'s assembly kernels run under the guards and
  Memcheck but cannot be instrumented by the sanitizers.
- The `crypto-host`/`crc-host` wasm delegation is covered by the wasm
  conformance harness, not by any memory-safety lane.
- The sanitizer and Valgrind lanes are Linux only; macOS and Windows have the
  guard lane alone.

`unsafe-complete` fails if discovery, any shard, or any native safety lane is
failed, cancelled or skipped. The required `ci-complete` gate depends on it
and makes the same all-success check over every lane it needs, so a skipped
lane fails it too. No sanitizer suppression, retry or interpreter check is
disabled.

## 7-Zip interoperability

`tests/sevenzip_encode_tests.rs`, the 7-Zip half of
`tests/bcj2_compress_tests.rs` and `tests/differential_7zz_tests.rs` need
`7zz` (or `7z`) on `PATH` and skip themselves without it. ubuntu-26.04
preinstalls apt's `7zip`, so the Linux CI lanes run them against whatever
7-Zip that image carries. With `SEVENZ_TURBO_7ZZ_REQUIRED` set, a missing
7-Zip fails them instead, as `SEVENZ_TURBO_SDK_REQUIRED` does for the SDK
oracle. A `sevenzip-interop` job that sets it is prepared in `ci.yml`,
commented out until the operator approves where 7-Zip comes from (apt's
`7zip` on ubuntu-26.04, or `7zz` built from the pinned tree the SDK lane
fetches).
`tests/lzma2_mt_tests.rs` also needs `7zz` and does not yet honour the
variable.

## SDK functional contracts

The pinned reference is [7-Zip 26.03, commit
0766b733fe3e06dd2a7f9a3cfbf2108ac73abd17](https://github.com/ip7z/7zip/tree/0766b733fe3e06dd2a7f9a3cfbf2108ac73abd17).
That tree has no standalone archive test suite. `tests/sdk_functional.rs`
translates uncovered contracts from `C/7zArcIn.c` and `C/7zDec.c` into
generated fixtures and compares them with the SDK's public-domain
`C/Util/7z/7zMain.c` decoder. No SDK source or generated archive is committed.

| Contract | Regression coverage |
| --- | --- |
| Major/minor version handling | All sampled minor versions remain readable; unsupported major versions fail |
| Empty archive encodings | Absent next header, Header/End, and an empty FilesInfo |
| Empty-stream and empty-file bitmaps | Mixed directories, empty files and streamed members across the byte boundary |
| Names and solid substreams | UTF-16 surrogate pairs, duplicate names, exact metadata order, complete member multiset and block-entry spans |
| Sparse metadata | Definition masks crossing a byte boundary, creation/modification times and attributes |
| CRC definitions | Sparse inherited folder digests and a defined zero CRC for an empty stream |
| Optional properties | Unknown archive and file properties skipped by their declared lengths |
| Corruption | Every start-header field, independent member/header CRCs and every truncation |
| Decoder functionality | Exact extracted bytes from committed LZMA, LZMA2, solid/non-solid, delta, BCJ2 and optional PPMd archives |

The C decoder supports fewer coder graphs than full 7-Zip: the existing
Copy+LZMA2 and ARM64-BCJ corpus files are outside that decoder's subset.
Their existing sevenz/full-7-Zip tests remain; the new generated Copy archives
are checked by the C oracle. This suite does not claim all 7-Zip features are
implemented, such as external file-property streams.

The dedicated SDK jobs build the pinned C decoder on both Linux architectures
and require it. Other test lanes still run the generated Rust assertions;
the oracle portion is optional outside the dedicated jobs. To run locally:

```sh
bash .github/scripts/build-sdk-oracle.sh
SEVENZ_TURBO_SDK_7ZDEC="$PWD/target/sdk-oracle/source/C/Util/7z/_o/7zdec" \
  SEVENZ_TURBO_SDK_REQUIRED=1 cargo nextest run --locked --test sdk_functional --no-fail-fast
```
