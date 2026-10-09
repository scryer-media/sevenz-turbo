# Benchmark corpus

`sevenz-turbo-bench fixtures` generates this directory on every host. No
fixture byte is committed (this README and `.gitignore` are the only tracked
files here), and CI's `hygiene` lane rejects any tracked `.7z` or `.bin`
outside `tests/resources` and `examples/data`.

```sh
cd bench/sevenz-turbo-bench
go run . fixtures --profile full  --dir ../fixtures/full    # fleet corpus, about 20 GiB
go run . fixtures --profile quick --dir ../fixtures/quick   # smoke corpus, ~250 MiB
```

Every source is generated from a fixed seed, so every host builds the same
input bytes. The archives are written by the host's `7zz a`; `fixtures.json`
records every source's and archive's size and SHA-256 and the `7zz` banner and
digest, so two hosts' corpora can be compared byte for byte. A source already
present is kept only when its `.complete` marker names the same recipe and its
members are exactly the recipe's; an archive only when its
`<name>.provenance.json` names the same `7zz`, switches and source content and
its bytes are unchanged. Anything else is regenerated. `run` rehashes every
source and archive against `fixtures.json` before planning and refuses a
corpus that has changed since.

## Sources (`src/<name>/`)

| source | full | quick | contents |
|---|---|---|---|
| `payload` | 1 GiB | 32 MiB | lzma-turbo's `cargo xtask fixtures` payload, byte for byte (SplitMix64 seed 7: hex words mixed with random runs). The first MiB hashes to `615592a9...4b112fbf`. |
| `payload-sub` | 256 MiB | 16 MiB | the first bytes of the same payload |
| `code-x86` | 64 MiB | 4 MiB | synthetic x86-64-shaped code with dense E8/E9 rel32 branches |
| `code-arm64` | 64 MiB | 4 MiB | synthetic AArch64-shaped code with dense BL branches |
| `audio` | 256 MiB | 8 MiB | synthetic 16-bit stereo PCM: two tones plus noise |
| `tree` | 8192 x ~32 KiB | 512 x ~16 KiB | many small text members under invented directories |
| `kdf-tree` | 2048 x ~2 KiB | 128 x ~2 KiB | many tiny members |
| `media` | 1 GiB | 16 MiB | near-incompressible pages (4016 random bytes, 80 zeros): the already-compressed video and audio of a usenet download |
| `media-2g` | 2 GiB | - | the same recipe at 2 GiB |
| `media-3g` | 3 GiB | - | the same recipe at 3 GiB |

## Archives

Every archive is `7zz a -t7z` with these switches (the encrypted ones use the
benchmark passphrase `bench-passphrase`, a public constant):

| archive | source | switches | what it exercises |
|---|---|---|---|
| `st.7z` | payload | `-mx=5 -m0=lzma2 -mmt=1` | one LZMA2 stream, no dictionary resets: no decoder can parallelise it |
| `mt.7z` | payload | `-mx=5 -m0=lzma2 -mmt=8` (quick: `-m0=lzma2:c=4m`) | LZMA2 in independent blocks: the parallel decode fixture |
| `lzma.7z` | payload-sub | `-mx=5 -m0=lzma -mmt=1` | LZMA (not LZMA2) |
| `tree_solid.7z` | tree | `-mx=5 -m0=lzma2 -mmt=8 -ms=on` | many small members in one solid folder |
| `tree_nonsolid.7z` | tree | `-mx=5 -m0=lzma2 -mmt=8 -ms=off` | the same members, one folder each |
| `aes_store.7z` | payload | `-mx=0 -p -mhe=on` | stored + AES-256: decrypt and CRC only |
| `aes_mx1.7z` | payload | `-mx=1 -m0=lzma2 -mmt=8 -p -mhe=on` | LZMA2 under AES-256 |
| `aes_kdf.7z` | kdf-tree | `-mx=5 -m0=lzma2 -ms=off -p -mhe=on` | one encrypted folder per tiny member: SHA-256 key derivation |
| `bcj_x86.7z` | code-x86 | `-mx=5 -mmt=1 -mf=BCJ` | BCJ x86 filter |
| `bcj_arm64.7z` | code-arm64 | `-mx=5 -mmt=1 -mf=ARM64` | BCJ ARM64 filter |
| `bcj2.7z` | code-x86 | `-mx=5 -mmt=1 -mf=BCJ2` | BCJ2 (four streams, LZMA2 + LZMA) |
| `delta.7z` | audio | `-mx=5 -mmt=1 -mf=Delta:4` | delta filter, distance 4 |
| `ppmd.7z` | payload-sub | `-mx=5 -m0=PPMd -mmt=1` | PPMd (decoded by the external `ppmd-turbo` crate): a secondary row |
| `media_mx1.7z` | media | `-mx=1 -m0=lzma2 -mmt=8` | near-incompressible LZMA2 at `-mx1` in parallel blocks: mostly stored chunks, the download-shaped decode |
| `media_mx5.7z` | media | `-mx=5 -m0=lzma2 -mmt=8` | the same at `-mx5`: fewer, larger blocks, so the parallel decoder holds fewer runs at once |
| `media_mx5_2g.7z` | media-2g | `-mx=5 -m0=lzma2 -mmt=8` | full only. 16 runs of 128 MiB: more than eight threads take at once |
| `media_mx5_3g.7z` | media-3g | `-mx=5 -m0=lzma2 -mmt=8` | full only. 24 runs of 128 MiB: more runs than the widest host measured has threads (18), so a decoder that cannot keep every thread supplied from a backlog shows it at any thread count |

`-mmt=8` rather than `-mmt=on` keeps the block layout the same on every host
whatever its core count. The quick corpus sets a 4 MiB LZMA2 block because at
32 MiB the default block (four dictionaries, 128 MiB at `-mx=5`) would leave
the archive a single block.

A run is the stretch of an LZMA2 stream from one dictionary reset to the next:
what a parallel decoder can give to one thread. 7zz cuts a multi-threaded
encode into runs of its own size, 128 MiB at `-mx=5` and 1 MiB at `-mx=1`.
`fixtures` counts the runs of `mt.7z` and of every media archive from the
archive's own stream, by walking its LZMA2 chunk headers, and records the
count and the run sizes in `fixtures.json` (`lzma2`). The counts above are
those. An archive with fewer runs than its recipe needs (`min_runs`: two for
the parallel fixtures, nine for `media_mx5_2g.7z`, nineteen for
`media_mx5_3g.7z`) is refused, so a 7zz that cuts differently cannot pass for
the fixture.

Encode rows read `payload-sub`, `tree` and `kdf-tree` directly.
