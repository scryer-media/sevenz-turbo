# sevenz-turbo-bench

A single Go binary that measures what the `sevenz-turbo` crate itself does
(7z container parsing, LZMA/LZMA2 decode and encode through `lzma-turbo`,
solid and non-solid folders, AES-256 with SHA-256 key derivation, the BCJ,
BCJ2 and delta filters, the decode memory budget) against the official 7-Zip
`7zz` at matching thread counts, on any host a fleet can reach: Linux x86-64
and arm64, macOS, Windows. Every row records wall time, CPU time and peak RSS.

Codecs this crate only forwards to an external crate (zstd, brotli, bzip2,
deflate, lz4) are deliberately not measured. PPMd is measured as a secondary
row only, because it is the external `ppmd-turbo` crate.

## What it runs

Every measured run is its own child process. Wall time is the harness's
monotonic clock around start-to-exit. CPU (user+sys) and peak RSS come from
the kernel's accounting of the exited child: `wait4` rusage `ru_maxrss` on
Linux and macOS, `PeakWorkingSetSize` on a handle held across the exit on
Windows. A run without a peak RSS is a failure (`harness-missing-rss`), never
a zero. The crate is driven through `tools/decode-bench op ...`, which does
exactly one operation per process and prints one JSON line, so its peak RSS is
that operation's alone and is measured the same way as `7zz`'s.

Variants:

| variant | role | binary |
|---|---|---|
| `sevenz-turbo` | candidate | `decode-bench` built with default features (AWS-LC cryptography) |
| `sevenz-turbo native-crypto` | candidate (AES rows only) | `decode-bench --features native-crypto` (RustCrypto) |
| `sevenz-turbo no-ledger` | candidate (decode ledger rows with no limit) | the same `decode-bench`, run without `--ledger`: the decode as a consumer runs it, in the same passes as the observed one |
| `7zz` | reference (oracle) | the official 7-Zip console binary: `7zz t` (decode), `7zz l -slt` (list), `7zz a` (encode) |
| `sevenz-rust2` | secondary | upstream `sevenz-rust2 =0.22.2`, already linked by `decode-bench`; decode rows only, informational |

The matrix (`T` = threads; the full and fleet sweep is 1, 2, 4, 8, 16 below
the core count, then `all`; `--quick` runs 1 and `all`):

| group | scenarios | what it shows |
|---|---|---|
| container parse | `list/{tree_solid,tree_nonsolid,mt,aes_kdf}` | header parse and entry walk (`Archive::read_with_limits`) vs `7zz l -slt` |
| lzma2 single-stream | `decode/st/T{1,all}` | one LZMA2 stream; threads cannot help |
| lzma2 parallel | `decode/mt/T<sweep>`, plus `Tall/no-verify` and `Tall/stream` | the parallel LZMA2 decoder; the CRC-32 cost; the single-parse streaming consumer (`block_decoder` per block, sub-stream CRC hook) |
| lzma | `decode/lzma/T1` | LZMA (not LZMA2) |
| memory budget | `decode/mt/T8/budget-<N>MiB` (full 64 and 512 MiB, quick 20 and 96 MiB) | `ArchiveLimits::memory`: `parallel_path=false` in the notes means the budget forced the single-threaded fallback; compare its peak RSS with the unbudgeted row |
| decode ledger | `decode/{media_mx5_3g,media_mx5_2g,mt}/T{2,4,8,all}/ledger` and the same under a limit, `…/budget-<N>MiB/ledger` for 512, 553, 1024, 1065 and 2089 MiB; `decode/{media_mx1,aes_mx1}/T{2,4,8}/ledger`. Full and fleet only | the parallel LZMA2 decoder given more runs than it has threads, at each thread count and under the limits a consumer passes (553, 1065 and 2089 MiB are what weaver passes at 2, 4 and 8 threads), with each decode's memory and dispatch ledger (below). The `-mx1` rows are controls: their runs are small, so a change made for the large-run fixtures must not move them |
| solid vs non-solid | `decode/tree_{solid,nonsolid}/T{1,all}`, plus `no-verify`, `stream` | many small members: per-member CRC-32, folder setup and header cost vs one large stream |
| aes-256 | `decode/aes_store/T1`, `decode/aes_mx1/T{1,all}`, `decode/aes_kdf/T1` (+ `list/aes_kdf`) | AES-256-CBC decrypt in both cryptography builds; SHA-256 key derivation dominated rows |
| filters | `decode/{bcj_x86,bcj_arm64,bcj2,delta}/T1` | the BCJ x86/ARM64, BCJ2 and delta filters |
| ppmd (secondary) | `decode/ppmd/T1` | PPMd through `ppmd-turbo` |
| encode | `encode/payload-sub/L{1,3,5,7,9}/T{1,all}`, `encode/payload-sub/L5/T<sweep>`, `encode/tree/L5/Tall/{solid,non-solid}` | the `compress` writer (LZMA2 through lzma-turbo's encoder) vs `7zz a -m0=lzma2:d=…:fb=…:mf=…:a=… -mx<L> -mmt<T>`, with the archive-size ratio; 7zz is given this crate's level settings (xz's table, not 7-Zip's `-mx` defaults), so both sides use the same dictionary, match finder and fast bytes; every archive this crate writes is checked once with an untimed `7zz t` |
| encode aes-256 | `encode/payload-sub/L5/Tall/aes`, `encode/kdf-tree/L5/T1/non-solid/aes` | AES-256 write (`-mhe=on` on the 7zz side; this crate encrypts the header by default) |

A multi-threaded encode uses the block size `7zz` would (four dictionaries,
clamped to 1-256 MiB) for the same dictionary, so the size ratio compares
like with like. The
`--quick` corpus is too small for the level-5 block (32 MiB) to split, so its
multi-threaded level-5 encode rows are effectively single-block; use the full
corpus for encode scaling.

Within a scenario the variants are interleaved and their order is reversed
every repeat. Each cell is the median [min-max] of the measured repeats.
Decodes of the same archive by any of this crate's engines must produce the
same output digest, and every decode must produce exactly the fixture's
unpacked byte count; either mismatch fails the run. The digest comes from one
extra untimed decode per engine (`--digest`), so no timed row pays for
hashing.

Every ratio is **7zz / sevenz-turbo** of the medians (oracle over ours, the
direction rarpar-bench and the weaver bench reports use, so merged reports
read the same way): ratio = 7zz / sevenz-turbo, >1 = sevenz-turbo better.
Above 1.000 sevenz-turbo is faster, smaller or lower; below 1.000 it is
slower, larger or higher. The peak RSS section lists the lowest ratio first.

### The decode ledger

A `decode ledger` row runs `decode-bench op decode --ledger`: the reader keeps
an `Lzma2Ledger` (`Lzma2Handle::keep_ledger`) of its parallel LZMA2 decode and
decode-bench prints it in its JSON line as `ledger_*` fields. Keeping it reads
gauges the decode already has, where the decode already stops to look at them;
the `sevenz-turbo no-ledger` variant of the rows with no limit is the same
command without it, so the report shows both against 7zz. A fixed thread count
above the usable cores is left out, and `all` is the usable cores.

The report gains two tables. Each cell is the median over the measured runs,
with the range where the runs differed.

`decode ledger: memory`, in MiB:

| column | what it is |
|---|---|
| limit, budget | the limit the row passed, and what it left the decoder to hold (the default for the thread count where no limit was passed) |
| peak RSS | the process's, as in every other table |
| held | the peak of lzma-turbo's `held_bytes()`: input pieces, the buffer of every run out with a worker, decoded runs waiting their turn and buffers parked for reuse, by capacity |
| queue | the peak of the reader's queue of packed input read and not yet handed to the decoder, by length (what its 192 MiB read-ahead allowance is measured against) and by the capacity of its pieces |
| spill | the peak of decoded output held between the decoder and the caller |
| together | the most `held`, the queue's capacity and `spill` came to at one moment; they peak at different moments, so it is below their sum. This is what a memory limit governs |
| dictionaries | allocated / touched / nominal. A worker has no dictionary of its own: it decodes into the run's output buffer, which `held` counts. The decode allocates one dictionary, and only when the calling thread decodes a run itself; no more of it is written than that thread decoded. Nominal is the dictionary size times the decoders that ran |
| remainder | peak RSS less `together` and the allocated dictionaries: everything the ledger does not name. Negative where buffers counted by capacity were never written, since RSS counts touched pages only |

The byte figures are sampled where the reader looks (after every read of
packed input, every hand-over to the decoder and every drain of output), so
none is above its true peak.

`decode ledger: dispatch`:

| column | what it is |
|---|---|
| runs | LZMA2 runs handed to a decoder; for a ledger fixture this is the count in `fixtures.json` |
| waves, runs per wave | a wave is the runs a decoder claimed between two sleeps of the delivering thread: it is closed each time that thread goes to sleep for a worker with runs claimed since the wave before. The sequence is the first measured run's, repeats folded (`1x3` is three waves of one run), and cut after 24 terms; `report.json` has it whole |
| runs out per wave | the runs out as the delivering thread went to sleep at the end of each wave. A decode that keeps its threads supplied claims a run for each that comes back and shows the thread count here wave after wave; one that lets them run dry claims nothing while they finish and shows only what it claimed in the wave |
| peak runs out | the most runs out with a decoder at once |
| out asleep | the mean runs out over the time the delivering thread slept: `report.json` has the time by runs out (`wait_seconds_by_runs_out`) |
| refused at boundary (busy) | input pieces the decoder handed back for want of room with the hand-over standing at the end of a run and no complete run waiting: it holds a whole run it cannot start, because the header that closes that run is at the front of the piece it refused. In brackets, those with a run out |
| refused mid-run; refused, run waiting | the other pieces handed back: part way through a run, and at a run's end with a complete run already waiting |
| gate, backlog | times the reader stopped reading ahead because what the decoder held left no room for another run under the budget, and times it stopped with room |
| waits, wait s, idle thread s | times the delivering thread slept for a worker and for how long; and that time weighted by the threads with no run out |

A run is out from the moment a decoder claims it until its last byte is
delivered, so a run that is decoded and waiting its turn behind an earlier one
still counts: runs out is at least the threads at work, not exactly them.

Three things the ledger cannot say, because lzma-turbo does not report them:
how often the decoder itself declined to start a run for want of room, how
`held` divides between input, runs out, finished runs and parked buffers, and
how many runs are being decoded at a given moment as opposed to out.

## Build

On each host (or cross-built per target):

```sh
# The candidate, in both cryptography configurations.
cargo build --locked --release -p decode-bench
cargo build --locked --release -p decode-bench --features native-crypto --target-dir target-native
cp target/release/decode-bench        dist/decode-bench            # AWS-LC (default)
cp target-native/release/decode-bench dist/decode-bench-native     # RustCrypto
# (Windows: decode-bench.exe)

# The harness: plain Go, no cgo.
cd bench/sevenz-turbo-bench
go build -o ../../dist/sevenz-turbo-bench .
GOOS=linux GOARCH=arm64 go build -o ../../dist/sevenz-turbo-bench-linux-arm64 .
GOOS=windows GOARCH=amd64 go build -o ../../dist/sevenz-turbo-bench.exe .
```

`decode-bench op version` reports the crypto backend and the Cargo build
profile it was built with and the locked versions of `lzma-turbo`,
`sevenz-rust2`, `aws-lc-rs`, `crc-fast` and every `ppmd-*` crate the lock
carries; the harness refuses a `--candidate-native` whose backend is the same
as `--candidate`'s, and refuses any candidate whose build profile is not
`release`.

## The oracle

Use the official 7-Zip console binary for the host, from
<https://www.7-zip.org/download.html> or the project's GitHub releases
(<https://github.com/ip7z/7zip/releases>). Never build the oracle yourself,
and never use p7zip (an unofficial fork frozen at 16.02; the harness refuses
it unless `--allow-p7zip`).

| host | asset | binary |
|---|---|---|
| Linux x86-64 | `7z<ver>-linux-x64.tar.xz` | `7zz` |
| Linux arm64 (Graviton) | `7z<ver>-linux-arm64.tar.xz` | `7zz` |
| macOS (universal) | `7z<ver>-mac.tar.xz` | `7zz` |
| Windows x64 / arm64 | the installer, or `7z<ver>-extra.7z` | `7z.exe` (installer) or `7za.exe` (extra) |

A distribution or Homebrew package is accepted but the report says so in its
provenance line. Set `SEVENZ_BENCH_ORACLE_PROVENANCE` to the asset URL you
downloaded and `SEVENZ_BENCH_ORACLE_OFFICIAL=1` to vouch for it; the report
records the oracle's path, SHA-256, banner and version either way.

## Fleet invocation

```sh
B=./dist
$B/sevenz-turbo-bench fixtures --profile full --dir work/fixtures --oracle /opt/7zip/7zz
$B/sevenz-turbo-bench toolchain --candidate $B/decode-bench --candidate-native $B/decode-bench-native \
    --oracle /opt/7zip/7zz > results/toolchain.json
$B/sevenz-turbo-bench run --profile fleet --dir work/fixtures --out results \
    --candidate $B/decode-bench --candidate-native $B/decode-bench-native \
    --oracle /opt/7zip/7zz --machine c7i.4xlarge-us-east-1
echo "rc=$?"
# collect results/report.json from every host, then:
$B/sevenz-turbo-bench merge --out cross-arch.md host-*/report.json
```

`merge` refuses reports that measured different workloads: a different corpus
or run profile, source content, archive switches, 7zz release, or candidate
commit or `Cargo.lock`.

`report` regenerates `report.json` and `report.md` from a `raw.json`, the same
protocol rarpar-bench's macro suites use:

```sh
$B/sevenz-turbo-bench report --input results/raw.json --out results/report.json
```

`run --profile` picks the corpus, the matrix and the repeats. The explicit
flags (`--dir`, `--repeats`, `--warmups`, `--only`) then override it. There
are three profiles:

| profile | corpus | matrix | repeats + warmups |
|---|---|---|---|
| `quick` (the same as `--quick`) | quick | threads 1 and `all`, levels 1 and 5 | 2 + 0 |
| `full` (the default) | full | every scenario | 5 + 1 |
| `fleet` | full | every scenario | 3 + 1 |

A smoke run is `fixtures --profile quick` then `run --quick`, which takes a
few minutes. `fleet` keeps every scenario of `full`, 137 on an 18-core host
(the thread sweep stops below the core count), and only cuts the repeats.
78 of them are the `decode ledger` group, which `--only /ledger` runs alone.
From the quick-corpus numbers scaled to the full corpus, the others project
to about 4.5 hours on a 12- or 16-thread x86 host. Most of that is a handful of rows:
the non-solid tree encode, the PPMd decode (mostly its secondary
sevenz-rust2 variant), the AES and single-thread level 3, 7 and 9 encodes,
and the solid tree encode, each projected at 10 minutes or more there.
`run --list` prints the planned scenarios, then their count and the number of
processes the run will launch, and exits; `run` logs the same plan line
before it starts.

### Flags and environment

| flag | env | default |
|---|---|---|
| `--candidate` | `SEVENZ_BENCH_CANDIDATE` | `<repo>/target/release/decode-bench` when run inside a checkout |
| `--candidate-native` | `SEVENZ_BENCH_CANDIDATE_NATIVE` | none: the native-crypto rows are skipped |
| `--oracle` | `SEVENZ_BENCH_ORACLE` | `7zz`, `7zz.exe`, `7z`, `7za` on `PATH` |
| `--profile` | | `full`; also `quick` and `fleet` (above) |
| `--dir` | `SEVENZ_BENCH_FIXTURES` | `bench/fixtures/<the profile's corpus>`: `full` for `full` and `fleet`, `quick` for `quick` |
| `--machine` | `SEVENZ_BENCH_MACHINE` | the hostname |
| `--repo` | `SEVENZ_BENCH_REPO` | the git toplevel of the working directory, for rustc/commit/Cargo.lock provenance |
| `--pin-cpus 0-7` | `SEVENZ_BENCH_PIN_CPUS` | none (Linux `taskset`, Windows affinity mask; not macOS) |
| `--repeats`, `--warmups` | | the profile's: 5 and 1 (fleet: 3 and 1, quick: 2 and 0) |
| `--list` | | off: print the plan and exit without running |
| `--timeout` | | 1h per process; a run past it is recorded as DNF |
| `--only a,b` | | run only scenarios whose id contains one of the substrings |
| | `SEVENZ_BENCH_INSTANCE_TYPE` | recorded in the host descriptor (e.g. `c7g.4xlarge`); never probed |
| | `SEVENZ_BENCH_ORACLE_PROVENANCE`, `SEVENZ_BENCH_ORACLE_OFFICIAL` | see the oracle section |

### Exit codes

| code | meaning |
|---|---|
| 0 | every candidate and reference run succeeded with a peak RSS |
| 1 | a candidate or reference run failed, did not finish, lacked a peak RSS, or the run was interrupted; the reports are still written |
| 2 | usage error |
| 3 | a prerequisite is missing: no oracle, no candidate, no corpus, or a p7zip oracle |

Failed `sevenz-rust2` (secondary) runs are listed in the report and never
change the exit code.

### Output files (`--out DIR`)

| file | contents |
|---|---|
| `raw.json` | `schema` `sevenz-turbo-bench/raw/1`: host, toolchain, corpus manifest, the planned scenarios with every command line, and one record per run (`wall_seconds`, `user_seconds`, `sys_seconds`, `max_rss_bytes`, `rss_source`, `bytes_in`, `bytes_out`, `load_average`, `status`, `failure`, decode-bench's own JSON `result`) |
| `report.json` | `schema_version` 1, `schema` `sevenz-turbo-bench/report/1`: per-variant rows (`wall_seconds`, `cpu_seconds`, `max_rss_bytes`, `load_average` as `{median,min,max,n}`), the `ratios` against 7zz, `rss_scenarios` sorted worst first, `ledgers` (one per decode ledger row, every figure as `{median,min,max,n}` in bytes or counts, with the first measured run's `wave_runs`), and the failure lists |
| `report.md` | the same, readable: the orientation, the host (OS, arch, CPU model, cores, memory, ISA flags such as avx2/avx512*/vbmi2/gfni/vaes/sha_ni or neon/aes/pmull/sha2/sve/sve2), the toolchain (linked lzma-turbo, crypto backends, 7zz banner and provenance, rustc, crate commit), one table per group and a worst-first "Peak RSS per scenario" section |

The host descriptor reads ISA flags from Go's `x/sys/cpu` plus
`/proc/cpuinfo` (Linux) or `sysctl hw.optional` (macOS). The load average
before every run is recorded (Linux and macOS); a row with a high load is
noisy.

## Development

```sh
cd bench/sevenz-turbo-bench
gofmt -l . && go vet ./... && go test ./...
```
