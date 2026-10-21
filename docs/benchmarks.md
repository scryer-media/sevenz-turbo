# Benchmarks

Current measurements of sevenz-turbo 0.29.1 against 7-Zip 26.04 and
sevenz-rust2 0.22.2 on two CPUs.
Taken 2026-10-18 with the fleet harness in
[`bench/sevenz-turbo-bench`](../bench/sevenz-turbo-bench) (`--profile fleet`,
3 timed repeats after 1 warm-up, medians). How to reproduce a measurement is
in [benchmarking.md](benchmarking.md). Earlier results, including the
five-machine 0.27.0 round of 2026-10-13, are in this file's git history.

Every ratio is **reference / sevenz-turbo**, so above 1.000 means
sevenz-turbo took less wall time, less CPU time or less peak memory than the
reference on that row. Values below 0.970 are in bold. A dash means the row
was not run on that host. Thread counts: `T1`, `T2`, `T4`, `T8` are fixed,
`Tall` is every hardware thread the host has.

Build: `sevenz-turbo` 0.29.1 (commit d576392) default features (`aws-lc-rs`
1.18.1 crypto backend, `lzma-turbo` 0.10.1, `ppmd-turbo` 0.2.0), release
profile, rustc 1.97.1. 7-Zip is the official 26.04 binary for each platform.
sevenz-rust2 is the released 0.22.2 crate, decode rows only, reading the
archive from a bare `File` as its own `ArchiveReader::open` does. The harness
has since moved it behind a 1 MiB `BufReader` (see
[sevenz-turbo vs sevenz-rust2](#sevenz-turbo-vs-sevenz-rust2)).

## Hosts

| column | CPU | threads | OS | 7-Zip build |
| --- | --- | ---: | --- | --- |
| Sapphire Rapids | Intel Xeon Platinum 8488C (Sapphire Rapids, AVX-512) | 16 | Linux, c7i.4xlarge | 7-Zip (z) 26.04 (x64) |
| Graviton4 | AWS Graviton4 (Neoverse V2, SVE2) | 16 | Linux, c8g.4xlarge | 7-Zip (z) 26.04 (arm64) |

Both hosts were idle.

## Summary

Geometric mean of the per-row ratios on each host. Decode covers every
default-build `decode` row and encode every `encode` row, except the
memory-budget and adaptive rows, which are by design and listed separately
below. Peak RSS also covers the `list` rows.

| CPU | threads | vs 7-Zip: decode wall | encode wall | peak RSS | vs sevenz-rust2: decode wall | peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Sapphire Rapids | 16 | 1.26 | 1.50 | 0.99 | 1.87 | 1.18 |
| Graviton4 | 16 | 1.15 | 1.49 | 1.01 | 1.96 | 1.24 |

## sevenz-turbo vs 7-Zip

Default build, one table per scenario group. `list` rows parse the header and
walk every entry; 7-Zip's `l -slt` also formats every property.

### container parse

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `list/tree_solid` | 5.256 | 5.459 | 1.088 | 5.728 | 5.821 | 1.143 |
| `list/tree_nonsolid` | 4.204 | 4.373 | 1.012 | 4.608 | 4.646 | 1.069 |
| `list/mt` | 1.724 | 1.982 | 1.250 | 1.809 | 1.915 | 1.413 |
| `list/aes_kdf` | 1.422 | 1.414 | 1.059 | 1.690 | 1.695 | 1.159 |

### lzma2 single-stream

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/st/T1` | 1.009 | 1.009 | 1.043 | 1.009 | 1.009 | 1.056 |
| `decode/st/Tall` | 1.008 | 1.008 | 3.093 | 1.004 | 1.004 | 3.161 |

### lzma2 parallel

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/mt/T1` | 1.009 | 1.010 | 1.043 | 1.007 | 1.007 | 1.056 |
| `decode/mt/T2` | 0.997 | 1.004 | 1.001 | 0.999 | 1.001 | 1.002 |
| `decode/mt/T4` | 0.993 | 1.002 | 1.000 | 0.998 | 1.001 | 1.001 |
| `decode/mt/T8` | 0.986 | 1.003 | 1.000 | 0.992 | 1.000 | 1.000 |
| `decode/mt/Tall` | 0.976 | 1.000 | 1.000 | 0.997 | 1.002 | 1.000 |
| `decode/mt/Tall/no-verify` | 0.986 | 1.006 | 1.000 | 0.988 | 1.001 | 1.000 |
| `decode/mt/Tall/stream` | 0.975 | 1.000 | 1.000 | 0.997 | 1.001 | 1.000 |

### lzma2 near-incompressible

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/media_mx1/T1` | 1.005 | 1.005 | 1.313 | 1.000 | 1.000 | 1.441 |
| `decode/media_mx1/Tall` | **0.941** | 1.007 | 1.080 | **0.954** | 1.000 | 1.093 |
| `decode/media_mx5/T1` | 1.008 | 1.008 | 1.039 | 1.001 | 1.001 | 1.055 |
| `decode/media_mx5/Tall` | 0.978 | 0.999 | 1.000 | 0.992 | 1.000 | 1.000 |

### decode ledger

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/media_mx5_3g/T2/ledger` | 0.995 | 1.003 | 1.000 | 0.998 | 1.001 | 1.001 |
| `decode/media_mx5_3g/T4/ledger` | 1.009 | 1.006 | 1.000 | 0.998 | 1.001 | 1.000 |
| `decode/media_mx5_3g/T8/ledger` | 0.988 | 1.000 | 1.000 | 0.993 | 1.000 | 1.000 |
| `decode/media_mx5_3g/Tall/ledger` | 0.977 | 1.000 | 1.000 | 0.988 | 0.999 | 1.000 |
| `decode/media_mx5_2g/T2/ledger` | 0.994 | 1.002 | 1.000 | 0.999 | 1.002 | 1.001 |
| `decode/media_mx5_2g/T4/ledger` | 0.993 | 1.002 | 1.000 | 0.997 | 1.001 | 1.000 |
| `decode/media_mx5_2g/T8/ledger` | 0.983 | 0.999 | 1.000 | 0.999 | 1.001 | 1.000 |
| `decode/media_mx5_2g/Tall/ledger` | **0.956** | 1.004 | 1.000 | 0.977 | 1.000 | 1.000 |
| `decode/mt/T2/ledger` | 0.997 | 1.004 | 1.000 | 0.998 | 1.001 | 1.002 |
| `decode/mt/T4/ledger` | 0.994 | 1.003 | 1.000 | 1.000 | 1.002 | 1.001 |
| `decode/mt/T8/ledger` | 0.978 | 1.000 | 1.000 | 0.994 | 1.000 | 1.000 |
| `decode/mt/Tall/ledger` | 0.979 | 1.000 | 1.000 | 0.988 | 1.000 | 1.000 |
| `decode/media_mx1/T2/ledger` | 0.991 | 1.003 | 1.068 | 1.001 | 1.002 | 1.096 |
| `decode/media_mx1/T4/ledger` | 0.986 | 1.002 | 1.054 | 1.000 | 1.002 | 1.088 |
| `decode/media_mx1/T8/ledger` | 0.971 | 0.993 | 1.070 | 0.998 | 1.002 | 1.102 |
| `decode/aes_mx1/T2/ledger` | 1.005 | 1.006 | 1.150 | 1.002 | 1.004 | 1.184 |
| `decode/aes_mx1/T4/ledger` | 1.004 | 1.006 | 1.152 | 1.003 | 1.004 | 1.184 |
| `decode/aes_mx1/T8/ledger` | 0.997 | 0.999 | 1.153 | 1.003 | 1.004 | 1.175 |

### lzma

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/lzma/T1` | 1.010 | 1.010 | 1.049 | 1.004 | 1.005 | 1.063 |

### solid vs non-solid

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/tree_solid/T1` | 1.013 | 1.013 | 1.031 | 1.014 | 1.014 | 1.039 |
| `decode/tree_solid/Tall` | 1.009 | 1.013 | 0.999 | 0.982 | 0.982 | 1.000 |
| `decode/tree_solid/Tall/no-verify` | 1.029 | 1.031 | 1.000 | **0.969** | **0.970** | 1.001 |
| `decode/tree_solid/Tall/stream` | 1.014 | 1.016 | 0.999 | 0.993 | 0.995 | 1.000 |
| `decode/tree_nonsolid/T1` | 1.011 | 1.011 | 1.002 | 1.006 | 1.006 | 1.067 |
| `decode/tree_nonsolid/Tall` | 12.1 | **0.790** | **0.510** | 14.3 | **0.962** | **0.495** |
| `decode/tree_nonsolid/Tall/no-verify` | 12.1 | **0.793** | **0.567** | 14.4 | **0.969** | **0.552** |
| `decode/tree_nonsolid/Tall/stream` | 1.012 | 1.012 | **0.962** | 1.006 | 1.005 | 1.002 |

### stored

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/store/T1` | 2.418 | 2.421 | 1.061 | 1.573 | 1.574 | 1.121 |
| `decode/store/T2` | 2.204 | 2.136 | **0.655** | 1.611 | 1.575 | **0.778** |
| `decode/store/T4` | 2.196 | 2.135 | **0.676** | 1.547 | 1.511 | **0.778** |
| `decode/store/T8` | 2.172 | 2.105 | **0.676** | 1.615 | 1.580 | **0.778** |

### aes-256

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/aes_store/T1` | 2.155 | 1.828 | 1.449 | 1.094 | 1.109 | 1.549 |
| `decode/aes_store/T2` | 2.606 | 1.357 | **0.635** | 1.160 | **0.912** | **0.687** |
| `decode/aes_store/T4` | 2.620 | 1.364 | **0.644** | 1.114 | **0.867** | **0.687** |
| `decode/aes_store/T8` | 2.614 | 1.353 | **0.643** | 1.143 | **0.898** | **0.687** |
| `decode/aes_mx1/T1` | 0.997 | 1.006 | 1.335 | 0.991 | 0.998 | 1.456 |
| `decode/aes_mx1/Tall` | 0.979 | 1.011 | 1.147 | 0.982 | 1.002 | 1.158 |
| `decode/aes_kdf/T1` | 3.069 | 1.574 | 1.044 | 1.319 | 1.388 | 1.102 |

### filters

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/bcj_x86/T1` | 1.066 | 1.033 | 1.059 | 0.987 | 0.987 | 1.073 |
| `decode/bcj_arm64/T1` | 1.010 | 1.033 | 1.062 | **0.946** | 0.971 | 1.073 |
| `decode/bcj2/T1` (vs `7zz -mmt=1 -mmtf=off`, the one-thread parity reference) | 1.034 | 1.034 | 1.089 | 1.036 | 1.036 | 1.104 |
| `decode/delta/T1` | 1.026 | 1.045 | 1.059 | **0.958** | 0.982 | 1.073 |

### ppmd (secondary)

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/ppmd/T1` | 1.286 | 1.286 | 1.082 | 1.246 | 1.246 | 1.107 |

### encode

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `encode/payload-sub/L1/T1` | 1.163 | 1.163 | 1.110 | 1.248 | 1.248 | 1.131 |
| `encode/payload-sub/L1/Tall` | 0.986 | **0.937** | **0.944** | 1.277 | 1.279 | **0.928** |
| `encode/payload-sub/L3/T1` | 1.190 | 1.190 | 1.017 | 1.275 | 1.275 | 1.022 |
| `encode/payload-sub/L3/Tall` | 1.140 | 1.134 | 0.994 | 1.293 | 1.301 | 0.994 |
| `encode/payload-sub/L5/T1` | 1.920 | 1.920 | 0.996 | 2.295 | 2.295 | 0.998 |
| `encode/payload-sub/L5/Tall` | 1.008 | 0.989 | 0.999 | 1.027 | 1.014 | 0.999 |
| `encode/payload-sub/L7/T1` | 2.173 | 2.173 | 0.997 | 2.226 | 2.226 | 0.999 |
| `encode/payload-sub/L7/Tall` | 0.991 | 0.994 | 1.001 | 1.030 | 1.017 | 1.000 |
| `encode/payload-sub/L9/T1` | 2.240 | 2.240 | 0.999 | 1.965 | 1.964 | 1.000 |
| `encode/payload-sub/L9/Tall` | 0.997 | 0.988 | 1.000 | **0.945** | **0.957** | 1.001 |
| `encode/payload-sub/L5/T2` | 1.025 | 1.037 | 1.005 | 1.109 | 1.068 | 1.007 |
| `encode/payload-sub/L5/T4` | 0.997 | 0.972 | 0.995 | 1.039 | 1.024 | 0.995 |
| `encode/payload-sub/L5/T8` | 0.999 | 0.994 | 1.036 | 1.062 | 1.036 | 1.048 |
| `encode/tree/L5/Tall/solid` | 0.979 | 0.973 | 0.990 | 0.991 | **0.967** | 0.991 |
| `encode/tree/L5/Tall/non-solid` | 11.4 | **0.683** | **0.338** | 13.3 | 1.001 | **0.324** |
| `encode/tree/L5/T1/non-solid` | **0.968** | **0.968** | 1.028 | 0.991 | 0.991 | 1.060 |
| `encode/kdf-tree/L5/T1/non-solid` | 1.298 | 1.298 | 1.040 | 1.446 | 1.447 | 1.124 |
| `encode/kdf-tree/L5/Tall/non-solid` | 23.0 | 1.444 | **0.652** | 11.8 | 1.864 | **0.621** |

### encode bcj2

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `encode/bcj2/L5/T1` | 1.113 | 1.099 | 0.973 | 1.064 | 1.070 | 0.974 |
| `encode/bcj2/L5/T4` | **0.962** | **0.957** | 1.083 | **0.967** | **0.957** | 1.089 |
| `encode/bcj2/L5/Tall` | **0.968** | **0.957** | 1.084 | 0.998 | 0.996 | 1.093 |

### encode aes-256

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `encode/payload-sub/L5/Tall/aes` | 0.996 | 0.991 | 1.000 | 1.004 | 0.993 | 1.000 |
| `encode/kdf-tree/L5/T1/non-solid/aes` | 2.144 | 1.506 | 1.008 | 1.352 | 1.376 | 1.047 |

Archive size ratio (7-Zip bytes / sevenz-turbo bytes at the same settings)
is 1.000 on every encode row on both hosts except the `kdf-tree` rows (0.996
to 0.997, about 0.4% larger) and `encode/payload-sub/L5/T2` (0.999). The
harness compares sizes, not bytes; salts, IVs and file attributes make equal
recipes differ byte for byte.

## By design: memory budget and adaptive rows

These rows measure what honouring a decode memory budget costs, and weaver's
adaptive chase decode (`set_adaptive_lzma2` plus `set_threads(1)`, widened
every 100 ms). 7-Zip has no decode budget and no adaptive mode, so it runs
unbounded at the same thread count on every one of them. They are not parity
targets.

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/mt/Tall/adaptive` | **0.236** | 1.045 | 3.979 | **0.232** | 1.023 | 3.986 |
| `decode/media_mx5/Tall/adaptive` | **0.237** | 1.045 | 3.980 | **0.231** | 1.020 | 3.989 |
| `decode/media_mx5/Tall/adaptive/budget-4096MiB` | **0.237** | 1.045 | 3.981 | **0.231** | 1.020 | 3.989 |
| `decode/media_mx5_3g/T2/budget-553MiB/ledger` | 0.995 | 1.003 | 1.000 | 0.998 | 1.001 | 1.001 |
| `decode/media_mx5_3g/T4/budget-1065MiB/ledger` | 0.994 | 1.002 | 1.000 | 0.997 | 1.000 | 1.000 |
| `decode/media_mx5_3g/T8/budget-2089MiB/ledger` | 0.987 | 1.001 | 1.000 | 0.994 | 1.000 | 1.000 |
| `decode/media_mx5_2g/T2/budget-553MiB/ledger` | 0.995 | 1.003 | 1.000 | 0.998 | 1.001 | 1.001 |
| `decode/media_mx5_2g/T4/budget-1065MiB/ledger` | 0.994 | 1.003 | 1.000 | 0.998 | 1.001 | 1.000 |
| `decode/media_mx5_2g/T8/budget-2089MiB/ledger` | 0.982 | 0.998 | 1.000 | 0.993 | 1.000 | 1.000 |
| `decode/mt/T2/budget-553MiB/ledger` | 0.997 | 1.004 | 1.001 | 0.998 | 1.001 | 1.002 |
| `decode/mt/T4/budget-1065MiB/ledger` | 0.993 | 1.002 | 1.000 | 1.000 | 1.002 | 1.001 |
| `decode/mt/T8/budget-2089MiB/ledger` | 0.981 | 1.001 | 1.000 | 0.991 | 1.000 | 1.000 |
| `decode/mt/T8/budget-64MiB` | **0.151** | 1.063 | 52.6 | **0.146** | 1.029 | 54.4 |
| `decode/mt/T2/budget-553MiB` | 0.997 | 1.003 | 1.001 | 0.999 | 1.001 | 1.002 |
| `decode/mt/T4/budget-1065MiB` | 0.994 | 1.002 | 1.001 | 0.998 | 1.002 | 1.001 |
| `decode/mt/T8/budget-2089MiB` | 0.979 | 0.999 | 1.000 | 0.995 | 1.000 | 1.000 |

## sevenz-turbo vs sevenz-rust2

Ratios are sevenz-rust2 0.22.2 / sevenz-turbo, from the same run's
sevenz-rust2 decode rows. sevenz-rust2 decodes on at most a few threads, so
on `decode/mt/Tall` it holds less memory and takes several times the wall
time; on `decode/st/Tall` it holds many times more.

This run fed sevenz-rust2 a bare `File`. Its LZMA range decoder takes one
byte per read from its input, so `decode/lzma/T1` paid a system call per
compressed byte: that row's 10.3x and 14.9x are mostly system time, and they
lift the sevenz-rust2 summary above. Behind a `BufReader`, which is how
sevenz-rust2 is meant to be fed and what the harness now does, a single local
check of the same fixture took 7.3 s of user CPU against sevenz-turbo's 4.5 s
(about 1.6x) instead of 82 s. The LZMA2 rows read through a chunk buffer of
their own and are not affected the same way.

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/st/T1` | 1.402 | 1.402 | 1.014 | 1.469 | 1.469 | 1.014 |
| `decode/st/Tall` | 1.450 | 1.452 | 62.0 | 1.485 | 1.489 | 63.7 |
| `decode/mt/T1` | 1.404 | 1.404 | 1.010 | 1.471 | 1.471 | 1.014 |
| `decode/mt/Tall` | 9.438 | 1.375 | **0.275** | 3.913 | 1.467 | **0.605** |
| `decode/media_mx1/T1` | 1.408 | 1.408 | 1.074 | 1.476 | 1.476 | 1.078 |
| `decode/media_mx5/T1` | 1.418 | 1.418 | 1.012 | 1.476 | 1.476 | 1.014 |
| `decode/lzma/T1` | 10.3 | 10.3 | 1.018 | 14.9 | 14.9 | 1.018 |
| `decode/tree_solid/T1` | 1.393 | 1.393 | 1.031 | 1.370 | 1.370 | 1.029 |
| `decode/tree_nonsolid/T1` | 1.390 | 1.390 | 1.363 | 1.448 | 1.448 | 1.432 |
| `decode/store/T1` | 1.080 | 1.078 | **0.900** | 2.050 | 2.052 | **0.890** |
| `decode/aes_store/T1` | 3.099 | 3.105 | **0.776** | 4.483 | 4.486 | **0.752** |
| `decode/aes_mx1/T1` | 1.417 | 1.417 | 0.996 | 1.487 | 1.487 | 1.014 |
| `decode/aes_kdf/T1` | 1.321 | 1.322 | **0.952** | 1.401 | 1.402 | **0.966** |
| `decode/bcj_x86/T1` | 1.270 | 1.270 | 1.009 | 1.316 | 1.316 | 1.014 |
| `decode/bcj_arm64/T1` | 1.228 | 1.228 | 1.014 | 1.295 | 1.295 | 1.014 |
| `decode/bcj2/T1` | 2.455 | 2.456 | 1.009 | 2.740 | 2.740 | 1.016 |
| `decode/delta/T1` | 1.342 | 1.342 | 1.009 | 1.321 | 1.321 | 1.014 |
| `decode/ppmd/T1` | 2.244 | 2.244 | 1.011 | 1.949 | 1.949 | 1.019 |

## Other variants

`native-crypto` is a non-default build with the RustCrypto backend instead
of `aws-lc-rs`. `no-ledger` is the default build run without `--ledger`:
the `ledger` rows above ask the reader to keep an accounting of its parallel
LZMA2 decode (`Lzma2Handle::keep_ledger`) and report it, which no consumer
does unless it asks; `no-ledger` is the same decode as a consumer runs it.
Both are against 7-Zip on the same rows.

### native-crypto

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/aes_mx1/T2/ledger` | 1.003 | 1.004 | 1.155 | 0.996 | 0.998 | 1.200 |
| `decode/aes_mx1/T4/ledger` | 1.004 | 1.006 | 1.162 | 0.997 | 0.998 | 1.196 |
| `decode/aes_mx1/T8/ledger` | 0.995 | 0.996 | 1.158 | 0.997 | 0.998 | 1.176 |
| `decode/aes_store/T1` | 1.855 | 1.573 | 1.440 | **0.745** | **0.755** | 1.591 |
| `decode/aes_store/T2` | 2.240 | 1.234 | **0.646** | **0.729** | **0.629** | **0.696** |
| `decode/aes_store/T4` | 2.263 | 1.242 | **0.654** | **0.719** | **0.606** | **0.695** |
| `decode/aes_store/T8` | 2.258 | 1.234 | **0.650** | **0.750** | **0.643** | **0.695** |
| `decode/aes_mx1/T1` | 0.994 | 1.003 | 1.363 | 0.986 | 0.994 | 1.546 |
| `decode/aes_mx1/Tall` | **0.949** | 1.011 | 1.152 | **0.966** | 0.997 | 1.162 |
| `decode/aes_kdf/T1` | 3.077 | 1.578 | 1.077 | 1.286 | 1.354 | 1.143 |
| `encode/payload-sub/L5/Tall/aes` | 0.994 | 0.994 | 1.000 | 0.999 | 1.008 | 1.000 |
| `encode/kdf-tree/L5/T1/non-solid/aes` | 2.152 | 1.512 | 1.000 | 1.335 | 1.358 | 1.034 |

### no-ledger

| scenario | Sapphire Rapids wall | Sapphire Rapids CPU | Sapphire Rapids RSS | Graviton4 wall | Graviton4 CPU | Graviton4 RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `decode/media_mx5_3g/T2/ledger` | 0.996 | 1.003 | 1.000 | 0.998 | 1.001 | 1.001 |
| `decode/media_mx5_3g/T4/ledger` | 1.009 | 1.007 | 1.000 | 0.997 | 1.001 | 1.000 |
| `decode/media_mx5_3g/T8/ledger` | 0.989 | 1.002 | 1.000 | 0.995 | 1.000 | 1.000 |
| `decode/media_mx5_3g/Tall/ledger` | 0.976 | 1.000 | 1.000 | 0.989 | 0.999 | 1.000 |
| `decode/media_mx5_2g/T2/ledger` | 0.995 | 1.003 | 1.000 | 0.998 | 1.001 | 1.001 |
| `decode/media_mx5_2g/T4/ledger` | 0.993 | 1.002 | 1.000 | 0.998 | 1.001 | 1.000 |
| `decode/media_mx5_2g/T8/ledger` | 0.976 | 0.995 | 1.000 | 1.000 | 1.001 | 1.000 |
| `decode/media_mx5_2g/Tall/ledger` | **0.955** | 1.003 | 1.000 | 0.978 | 1.000 | 1.000 |
| `decode/mt/T2/ledger` | 0.997 | 1.004 | 1.001 | 0.997 | 1.000 | 1.002 |
| `decode/mt/T4/ledger` | 0.993 | 1.001 | 1.000 | 1.002 | 1.003 | 1.001 |
| `decode/mt/T8/ledger` | 0.981 | 1.001 | 1.000 | 0.995 | 1.000 | 1.000 |
| `decode/mt/Tall/ledger` | 0.985 | 1.002 | 1.000 | 0.991 | 1.000 | 1.001 |
| `decode/media_mx1/T2/ledger` | 0.991 | 1.003 | 1.058 | 1.001 | 1.002 | 1.128 |
| `decode/media_mx1/T4/ledger` | 0.987 | 1.002 | 1.047 | 1.000 | 1.001 | 1.089 |
| `decode/media_mx1/T8/ledger` | 0.973 | 0.992 | 1.075 | 0.998 | 1.001 | 1.094 |
| `decode/aes_mx1/T2/ledger` | 1.002 | 1.002 | 1.132 | 1.002 | 1.004 | 1.183 |
| `decode/aes_mx1/T4/ledger` | 1.006 | 1.007 | 1.153 | 1.003 | 1.004 | 1.185 |
| `decode/aes_mx1/T8/ledger` | 0.996 | 0.999 | 1.144 | 1.003 | 1.003 | 1.176 |

## Lane view

These images are from the earlier 0.27.0 round on five machines and have not
been redrawn for this run. They group that run by the rows that were still
behind parity, one lane per gap. Click an image for the full-size version.

[![sevenz-turbo 0.27.0 vs 7-Zip 26.03, lane view](images/fleet-lanes-vs-7zip.webp)](images/fleet-lanes-vs-7zip.webp)

[![sevenz-turbo 0.27.0 vs sevenz-rust2 0.22.2, lane view](images/fleet-lanes-vs-sevenz-rust2.webp)](images/fleet-lanes-vs-sevenz-rust2.webp)
