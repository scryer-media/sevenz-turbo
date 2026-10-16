# Archive source-buffer huge-page audit

Machine-readable evidence named below is stored outside the repository.
See [Benchmark data archive](benchmark-data.md) for the archive layout and checksums.

This change applies Linux `MADV_HUGEPAGE` in the archive writer, before
initializing its reusable source buffer. It covers `push_archive_entry`,
`push_archive_entries`, and `prepare_block`. The allocation and its
deallocation both use a 2 MiB alignment. Unsupported advice is harmless.

Only declared folders of at least 8 MiB with an LZMA/LZMA2 dictionary of at
least 8 MiB qualify. Other sources retain the 1 MiB buffer. The new buffer
is 2 MiB regardless of file size. It does not stage a whole input file.
Single-threaded pull paths already read directly into codec-owned storage
and do not allocate this source buffer.

The bridge returns a consumed chunk before the encoder processes the
copied bytes. Its queue depth stays two, and payload copying stays outside
the queue lock. Preparation of the next chunk still overlaps compression.
The tested allocation bound drops from five chunks to four, avoiding up to
256 KiB of retained input storage. A stricter three-chunk variant delayed
input preparation until queue space was available; that saved memory but
cost roughly 8% wall time on the Zen 4 T4 zeros case, so it was rejected.

The acceptance target is at most 3% above the reference for wall and CPU
time. RSS is reported on every case; exceeding 3% is acceptable when paired
with a substantial wall-time or CPU improvement, per the maintainer's
revised criterion.

## Why the whole-file probe result does not transfer directly

The earlier encoder probe read an entire 256 MiB file into a newly
allocated ordinary-page buffer. Huge-page advice before that allocation's
first touch removed substantial page-fault and teardown time. The real
archive writer already streams through bounded, reusable source storage,
and the encoder already advises its large windows and input blocks. The
probe's saving therefore cannot be credited to this archive-writer change.

## Measurement scope

The base is sevenz-turbo `a1731cca4a810d91940f45077a849763ff9f7af0`.
The original package pinned lzma-turbo 0.8.0. Integration builds used the first-party lzma-turbo
source at `25df1bea056ba07b170bb38bbe607eb76706810b` in both the
baseline and candidate. It carried a provisional 0.9.1 build label; the
maintainer has since retargeted the encoder work to 0.9.0. Historical labels
remain in the raw evidence. The 0.27.0 release requires exactly
lzma-turbo 0.9.0. The encoder source at lzma-turbo `fecf6b3` is byte-identical
to the measured encoder source above (`src/enc` and `src/kernel_ab.rs`);
archive source hashes also match the retained measurements.

lzma-turbo 0.9.0 was published to crates.io on 2026-10-12 from its tag
`v0.9.0` (`170ec8b`), which is `fecf6b3` plus one review-fix commit
(`80909a6`: encoder state preserved after a failed buffer replacement, stale
size hints cleared before unsized encoding, fallible hash-chain head
allocation, retained streamed windows counted in the block-thread memory
estimate). lzma-turbo's own `docs/encoder-perf-0.9.0.md` records that those
corrections were not rebenchmarked there, and they were not rebenchmarked
here either: the measurements in this document describe the `fecf6b3`
encoder. `Cargo.lock` records the registry entry and checksum of the
published crate; there is no path, Git or command-line override anywhere in
the dependency resolution.

The measured program is `tools/decode-bench`'s existing `op encode` command:
filesystem enumeration, lazy file opens, actual `ArchiveWriter` calls,
checksumming, compression, archive finalization, and process teardown are
all included. No encoder-only or whole-file staging probe supplies these
results. Both binaries use the same Rust 1.97.1 release profile, default
features, and generic x86-64 target, on otherwise idle disposable c7a.xlarge
(Zen 4) and c7i.xlarge (Sapphire Rapids) hosts.

Wall time comes from a monotonic clock around the child process. CPU time
is user plus system time from `wait4`; peak RSS is that child's `ru_maxrss`.
Warm-ups are separate. Timed arms rotate order between repetitions. Zeros
uses 256 MiB and nine repetitions per arm/thread count. The nonzero guard
uses a deterministic 8 MiB pseudorandom pattern repeated to 64 MiB and
three repetitions, still crossing multiple 32 MiB blocks.

Matched settings are level 5, 8 MiB dictionary, 32 fast bytes, BT4, 32 match
cycles, the default literal/position bits, 32 MiB LZMA2 blocks, and T2/T4.
The library operation's dictionary and block-size fields are asserted on
every measured invocation. The official reference is 7zz 26.03.

Initial reference runs mistakenly specified a 32 MiB dictionary for 7zz
while the archive writer used 8 MiB. Those runs are retained as rejected
reference comparisons; their baseline/candidate pairs still use identical
settings. Their apparent SDK speed and RSS advantages are not acceptance
evidence. A nonzero run using the same incorrect reference was interrupted
and replaced by the explicitly matched guard.

## Correctness and platform validation

- All-features Cargo sweep: 408 passed, zero failed; one existing ignored
  documentation example. The added consumed-chunk test also passed with
  all features, and the bridge suite was rerun after the wakeup correction.
  The external 7zz oracle was available.
- No-default-features Nextest sweep: 120 passed, zero skipped.
- Encoder-only Nextest sweep after the bridge correction: 243 passed,
  zero skipped.
- Formatting and all-target/all-feature Clippy passed. No-default-feature
  Clippy passed as well.
- Both Linux hosts passed the allocation tests and the multi-block source
  integration tests, including short reads, byte-identical archive output,
  CRC verification, and propagated source errors. Linux Clippy passed.
- The wasm default feature set compiled on Linux. The local wasm attempt
  could not find the target standard library; the clean Linux toolchain
  supplied that independent check.
- Timed baseline/candidate archives are compared by SHA-256. Separate
  official `7zz t` and extraction checks verify archive validity and exact
  input recovery.

## Results

All eight cases pass the 3% wall, CPU and RSS gates without requiring an
RSS exception. Negative deltas mean lower time or memory. These are median
complete-process measurements against matched 7zz.

| CPU | Input | Threads | Wall delta | CPU delta | RSS delta |
| --- | --- | ---: | ---: | ---: | ---: |
| zen4 | seeded64 | 2 | -38.16% | -32.81% | +2.84% |
| zen4 | seeded64 | 4 | -32.16% | -28.90% | +1.06% |
| zen4 | zeros | 2 | -21.13% | -13.91% | +2.53% |
| zen4 | zeros | 4 | -16.23% | -15.33% | +1.14% |
| spr | seeded64 | 2 | -35.10% | -28.06% | +2.75% |
| spr | seeded64 | 4 | -21.96% | -18.49% | +1.07% |
| spr | zeros | 2 | -20.53% | -9.19% | +2.54% |
| spr | zeros | 4 | -12.26% | -12.23% | +1.13% |

The isolated archive-input change is smaller than the combined encoder
advantage above. Against the unchanged archive writer using the same
optimized encoder, Zen 4 zeros T4 improves wall time 3.69% and CPU 1.58%,
with RSS 0.36% higher. Across all eight cases, baseline-relative wall deltas
range from -3.69% to +1.61%, CPU from -1.58% to +1.25%, and RSS from +0.32%
to +0.83%. Earlier candidates and their less favorable results remain in
the evidence; this does not establish a large universal huge-page speedup.

The companion `input-buffer-perf.json` contains every sample, medians,
baseline-relative deltas, archive hashes and extraction checks, all four
candidate variants, rejected mismatched-reference runs, exact timing
harnesses, source/binary hashes and validation logs. CPU model names are
retained; machine-specific identifiers are omitted.

Both disposable benchmark hosts are confirmed terminated. Their combined
launch-to-confirmed-termination lifetime was at most 1.818
instance-hours, including setup and idle time. This is runtime accounting,
not a price estimate.
