# Failed experiments

What has been tried in this crate and did not pay: slower, no gain, more
memory than it saved, or a design that measured worse than the one that
replaced it. Read it before starting performance work, and do not retry an
entry without new evidence: a different workload, a different machine class,
or the change the entry says would have to come first. Each entry names its
source, where the full numbers are; `lzma_turbo.rs` is
`src/codec/lzma_turbo.rs`.

A new failed experiment is added here in the same change that records it.
Failed experiments in the LZMA, LZMA2, filter and checksum code itself are
catalogued in lzma-turbo's own `docs/failed-experiments.md`.

## Parallel LZMA2 reader

The feed rules in `docs/design-rules.md` are what is left of most of these.

### Feeding

- **Feeding part of a run.** The decoder's chase took the partly fed run on
  the calling thread: 1.50x slower than a bare parallel decode at two threads
  (15.9 s against 10.6 s), 1.64x at four. Only whole runs are fed now.
  Commit `d205243`, `docs/benchmarking.md`.
- **A gigabyte of read-ahead to hide the chase.** About 2 GB of peak memory.
  Replaced by switching the chase off with about one run per thread. Commit
  `f84f509`, `docs/lzma-turbo-requests.md`.
- **Arming the chase while the decoder is empty.** 1280 of 1536 MiB decoded on
  the calling thread; two of twelve runs reached a worker. `lzma_turbo.rs`.
- **Reading ahead before the first worker starts.** On 1 MiB runs (`7zz -mx1`)
  about 1,000 runs were read first: 4.6 s for 1 GiB at 18 threads, 1.1 s
  after. Commit `7687b0c`.
- **Cutting reads into ration-sized feeds while chasing the first run.** Moved
  1.3 GiB for nothing on a single-run gigabyte. `lzma_turbo.rs`.
- **Feeding up to the memory limit.** Under a 512 MiB limit a four-thread
  decode ran single-threaded, 3.4x slower; `feed_target` replaced it.
  `docs/lzma-turbo-requests.md`.
- **Copying the feed** (a memmove of the unconsumed tail, then a copy into the
  decoder). 5.8 s and 76 cuts on 1.6 GiB of incompressible data at four
  threads, against 2.9 s and 11 cuts with owned pieces.
  `docs/lzma-turbo-requests.md`.
- **Owned pieces without reclaiming them.** 4-50% faster, 8-21% more peak RSS;
  `reclaim_piece` took 1.75 GB to 1.40 GB. Read size (1-8 MiB) made no
  difference on compressible lanes, and 4 MiB was best on the incompressible
  one. `docs/lzma-turbo-requests.md`, commits `c8c9492` and `50bca2f`.
- **Read pieces of 4 MiB, then 1 MiB.** At 4 MiB one piece held four 1 MiB runs
  for every run being decoded; at 1 MiB a mebibyte of overrun stayed parked.
  It is 256 KiB. `MT_INPUT_READ_BYTES` in `lzma_turbo.rs`.

### Refusals and draining

- **Waiting for a worker after a refused feed.** The delivering thread slept
  with the workers idle: 3 GB at eight threads in 40 s against 17 s. It
  returns and drains instead. Commit `7c31253`.
- **Dropping read-ahead on a refusal, or waiting for room.** Both cost more than
  the copy they avoided. `docs/lzma-turbo-requests.md`.
- **Calling drain in a loop instead of waiting.** About 15 million drains over
  828 MiB at four threads; `wait_for_worker` cut CPU by 20-25%.
  `docs/lzma-turbo-requests.md`.
- **Unbounded drain with a spill copy.** 0.7 s of a 4.8 s decode; `drain_upto`
  took it to 4.59 s. `lzma_turbo.rs`, `docs/benchmarking.md`.
- **Leaving a run undeclared after a refused piece.** At four threads on
  128 MiB runs, three of four runs were out, at twice 7-Zip's time. The page
  declare took 2 GiB at four threads from 17.3 s to 9.0 s. Commit `a493534`.
- **Holding a run's input queued until its end was read.** 130 MiB queued at
  peak on 128 MiB runs. `lzma_turbo.rs`.

### Read-ahead and memory budgets

- **Counting read-ahead by the decoder's pending runs.** At eight threads on
  1 MiB runs, 60 runs were held while 4 were counted, and RSS was 3x 7zz's.
  `lzma_turbo.rs`.
- **Counting read-ahead by held bytes instead of waiting runs.** It stops the
  feed while the decode is busy and hands the run to the chase.
  `lzma_turbo.rs` (reasoned, not measured).
- **A read-ahead floor beyond the budget.** At eight threads under 1 GiB, three
  runs fit where six were affordable. `lzma_turbo.rs`.
- **Charging each thread for its read-ahead in `affordable_threads`.** It
  affords fewer threads for more time than the memory saves. `lzma_turbo.rs`
  (reasoned, not measured).
- **A gate charging `held + packed + unpacked`.** It charged a parked 128 MiB
  buffer twice and lost the second thread at two threads. `lzma_turbo.rs`.
- **The first-wave reservation, missing and over-applied.** Without it, 2 GiB
  of 128 MiB runs at four threads lost 3 s of 17 s to late starts; reserved
  again after the first scan, it lost a thread for good. `lzma_turbo.rs`.
- **Small-run read sizes and backlog on large runs.** On 128 MiB runs they cost
  a sixth of the wall time at four threads; they apply to runs of 4 MiB or
  less now. Commit `1e9499d`.
- **A fixed 512 MiB per thread with 128 MiB of read-ahead.** The decoder fills
  whatever ceiling it is given. `lzma_turbo.rs`.
- **Adaptive decode with no limit.** It never widened past two threads: 17.3 s
  with 443 MB, against 4.6 s at nine threads after the fix. `CHANGELOG.md`.
- **Adaptive decode starting inline on the calling thread.** 4.45 s, 2.46 s
  after the fix. `CHANGELOG.md`.

### Where the parallel path loses

- **More than two threads on stored (incompressible) runs.** 1.5 GiB was no
  faster at eight threads than at two and held 3 GB against 0.75 GB. Narrowing
  the threads without also cutting read-ahead still left 3 GB resident.
  Commit `17681d2`.
- **Detecting incompressible streams by a 90% ratio.** It caught media at
  ordinary levels and held them to two threads, five times slower than 7-Zip.
  The test is chunk kinds now, with a 1/64 LZMA share: `-mx5` at 18 threads
  went from 17.0 s to 4.0 s. Commits `17681d2` and `f7f134c`.
- **One 1 MiB run in parallel.** 4.5% slower on x86; parallel decoding first
  wins at the second run. Commit `19cc127`.
- **Blocks under 1 MiB in parallel.** 512 blocks of 16 KiB took 0.24 s and
  27k context switches, against 0.14 s and 1.1k on one thread. `CHANGELOG.md`.
- **Buffering a whole single-run stream (`7zz -mmt=1`) for one worker.**
  897 MiB in 23.3 s and 4.1 GiB, against 21.2 s and 376 MiB. Commit `d205243`.
- **A stream with no run boundaries** (`st.7z`). The parallel path is about 6%
  behind one thread. `docs/benchmarking.md`.

## Stored and AES folders

- **A stage thread when the sink does nothing.** A stored folder is 5-9% slower
  from two threads. Kept on: no thread or size gate can tell that sink from one
  that writes, and with one that writes it is 29% faster.
  `docs/benchmarking.md`.
- **Stage threads for folders under `FOLDER_STAGE_BYTES` (8 MiB).** The threads
  cost more than the overlap gains. `src/reader.rs`.
- **AES over parallel LZMA2 with the cipher on its own thread.** A wash; it stays
  sequential. Commit `d7b008c`, `src/pipeline.rs`.
- **`Pipeline::reserve`/`start` for AES over COPY.** It would change every AES
  chain at two threads, and a 256 KiB pipe chunk is a quarter of the best span.
  Commit `78f5836` (reasoned, not measured).
- **Reading AES ciphertext in 512 B pieces.** 0.862 s against 0.282 s, and it
  made AWS-LC look slower than RustCrypto. `docs/benchmarking.md`.
- **Sizing stored buffers to the folder, as a speed fix.** No change on 4,096
  small members at one or eight threads; kept as a memory fix only. Commit
  `c4fd4bb`.

## CRC

- **Checksumming on another thread for pipelined chains.** It would cost a pipe
  and a copy, more than the checksum. `src/reader.rs` (reasoned, not measured).

## Encoder and writer

- **The SDK's 273 fast bytes for the header.** Shipped in 0.27.1 and reverted:
  6-7x slower header encode (224 ms against 37 ms at 8,192 members). 64 and 128
  fast bytes, and 273 at depth 8, were also slower. It is 32, at a 0.008-0.13%
  size cost. `src/encoder.rs` (`SetHeaderMethod`), commit `96ad27e`.
- **Coding the BCJ2 call and jump streams inline beside parallel LZMA2.** It
  gave up the measured 1.07-1.11x; their own threads were restored. Commit
  `3397cf9`.
- **A three-chunk input bridge.** It saved memory and cost about 8% wall on Zen 4
  T4 zeros. `docs/input-buffer-perf.md`.
- **Holding the whole input on the thread-less writer path.** 395 MB and
  1382 MB, against 67 MB. `CHANGELOG.md`.
- **A full dictionary or the MT coder for every folder.** 512 folders of 16 KiB
  took 0.70 s and 101 MB, against 0.50 s and 8.5 MB. `CHANGELOG.md`.
- **Leaving non-LZMA compressors uncounted on folder workers.** Up to 12.7 cores
  busy at threads=8. Commit `d278180`.
- **An AES key cache that did not survive clones.** 21.7 s against 450.7 ms for
  2,049 folders. Commit `fb65b2d`.

## I/O sizes

Byte-at-a-time and tiny-block paths, all fixed; do not bring them back. PPMd
byte reads (12.5 s, 5.8 s after), PPMd byte writes (37.3 s, 5.1 s after), the
AES encoder at 16 B blocks (5.4 s, 4.5 s after), and BCJ at 4 KiB reads (1,033
reads, 69 after). `CHANGELOG.md`.

## Bench harness and measurement

- **Byte-at-a-time CRC-32 in the sink.** A quarter of the measured time; it made
  the fork look 30% slower. `tools/decode-bench/src/main.rs`, commit `0ec0680`.
- **A single-accumulator digest.** About 0.2 s per GiB; four lanes are used.
  `tools/decode-bench/src/main.rs`.
- **Driving sevenz-rust2 through a bare `File`, then a 1 MiB `BufReader`.** A
  system call per compressed byte, then `BufReader::seek` dropping its buffer
  (bcj2 at one thread went from 2.3 s to 58.6 s). A windowed cache replaced
  both. Commits `5cdcbea` and `b1b5678`.
- **RSS from cgroup `memory.peak` or Linux `ru_maxrss`.** The first counts page
  cache and needs a delegated cgroup; the second is floored at the harness's
  own high-water mark. VmHWM through ptrace is used. Commits `91e2b94` and
  `16423b8`.
- **Untimed checks between measured variants.** They biased the runs. Commit
  `b9b8314`.
- **Memory-budget rows at thread counts weaver never uses.** They read
  0.22-0.43x by construction; matched pairs replaced them. Commit `e809ddc`.
- **`rsync -a` of the source.** It kept the mtimes, Cargo skipped the rebuild,
  and the "after" run measured the old binary. `docs/benchmarking.md`.
- **A 7zz reference with a different dictionary** (32 MiB). Not acceptance
  evidence. `docs/input-buffer-perf.md`.
- **Optimising the crate, or its native dependencies, in test builds.** The
  crate saved 17 s of test time for 52 s of compile; the dependency overrides
  saved nothing beyond noise. Commit `36cebc8`.
