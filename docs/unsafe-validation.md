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

| Lane | Scope | Platforms |
| --- | --- | --- |
| `guard-pages` | Source, codec and output allocations during pull, Send and prepared-block APIs; short reads, failure and teardown | All four native platforms |
| `unsafe-valgrind` | The allocation unit tests and source-buffer lifecycle integration tests, on separate runners | Linux x86-64 and ARM64 |
| `unsafe-sanitizer` | The same allocation and lifecycle tests under AddressSanitizer and ThreadSanitizer, including instrumented std | Linux x86-64 and ARM64 |
| `unsafe-miri` | Initialization, unsupported advice, panic cleanup, independent allocations, movement between threads, and allocation-policy boundaries | One Linux runner per discovered portable test |

The guard allocator supports the source buffer's full 2 MiB alignment; it
does not silently send this allocation to the system allocator. A child
process proves inaccessible pages actually fault. The archive cases place
allocations against each edge and exercise native encode and decode paths,
including assembly selected by the codec dependencies. Guard pages catch
buffer overruns; they do not prove all reads inside a buffer are initialized.
Memcheck adds byte initialization and allocation/free checking.

Miri runs through Nextest with strict provenance. Discovery uses the actual
`cfg(miri)` build, requires the known ownership tests, and creates one job per
test. It excludes the native advice test because the OS FFI is replaced by a
stub under Miri. Native Memcheck, sanitizers and guards cover that FFI and
the machine code Miri cannot execute. Sanitizers cannot instrument assembly
instructions; passing them alone is not evidence of assembly memory safety.

`unsafe-complete` fails if discovery, any shard, or any native safety lane is
failed, cancelled or skipped. The required `ci-complete` gate depends on it.
No sanitizer suppression, retry or interpreter check is disabled.

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
