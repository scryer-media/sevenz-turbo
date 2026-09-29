# Reading a hostile archive

This crate is pointed at archives that arrive from strangers. A 7z header is a
sequence of attacker-chosen numbers, and a reader that believes them will
allocate what they say, walk what they say, and read where they say. This
document is the model that says it does not: what is assumed, what is bounded,
by what, and what happens when a bound is hit.

## Threat model

**The attacker controls the whole file.** Every byte, every count, every size,
every offset, every name, and the relationships between them. They may be
mutually inconsistent, and the file may stop anywhere.

**What they are trying to do**, in the order the code meets them:

1. **Exhaust memory** by declaring a large number where an allocation is sized
   by one — a count of files, a dictionary size, an unpacked header size.
2. **Exhaust CPU** by declaring work: a key-derivation factor of 2^62, a coder
   graph that is walked quadratically, a decode that never terminates.
3. **Panic the process** by making an index, a slice, or an arithmetic
   operation go out of range — a denial of service in any consumer that is not
   catching unwinds, and in a `panic=abort` build, the whole process.
4. **Read outside the file**, by pointing a pack stream at an offset that is
   not in it.
5. **Write outside the extraction directory**, by naming an entry `../..`, an
   absolute path, a drive, or a symlink target.
6. **Bomb the consumer's disk**, by packing a terabyte of zeroes into a
   kilobyte.

**What is out of scope.** Cryptographic attacks on AES-256 or on 7z's key
derivation; a consumer that extracts to a directory it should not have; the
correctness of a `lzma-turbo` decode, which is that crate's contract; and
side-channels. A password is assumed to be the caller's to give.

**The invariant.** *No allocation and no unit of work is sized by a
header-declared number without a bound checked first.* The bound is one of two
things, and usually both:

- **the bytes actually present.** Everything a header describes costs at least
  one byte to describe, so no count can exceed the header's own length. This
  bound needs no caller and is always in force.
- **an [`ArchiveLimits`] field.** The byte bound alone is not enough, because
  it amplifies: one byte of count can reserve a hundred bytes of `Vec`, so a
  64 MiB header could still ask for gigabytes.

## The limits

Every field of `ArchiveLimits` is listed here with its default, the attack it
closes, and what a caller sees when an archive trips it. **No default rejects
an archive a mainstream 7-Zip writes**; the structural defaults are one to two
orders of magnitude above the largest legitimate values observed.

Passing no limits (`ArchiveReader::new`) uses `ArchiveLimits::default()`, so
every structural bound below is in force by default. The two affordability
bounds — decoder memory and end-header size — are `u64::MAX` unless a caller
sets them, because only the caller knows what it can afford.

| Field | Default | The attack it closes | When it is hit |
| --- | --- | --- | --- |
| `memory_limit_bytes` | unlimited | A block declaring a 4 GiB dictionary. A header or block chaining several coders that each fit it alone. Bounds `Archive::decoder_memory_estimate`, and, for every decode including the encoded header's, the sum of the chain's LZMA, LZMA2 and PPMd coders before any of them is built; caps the zstd window at what that sum leaves. | `Error::MemoryLimited` (from the estimate) / `Error::MaxMemLimited` (a chain or coder at decode time, carrying the chain's total) before the decoder exists. A parallel LZMA2 decode that cannot fit its in-flight runs in the budget degrades to single-threaded instead of failing. |
| `max_end_header_bytes` | unlimited | The end-header size is read out of the file's first 32 bytes and that many bytes are buffered to parse it. | `Error::EndHeaderTooLarge`, before the buffer is allocated. |
| `max_header_unpacked_bytes` | 64 MiB | A compressed header is a block like any other: a kilobyte of input can declare that it unpacks to a terabyte, and the result is buffered whole before it can be parsed. | `Error::LimitExceeded { what: HeaderUnpackedBytes }` before a byte is decoded. The buffer is reserved once at the declared size and never grows past it. |
| `max_header_depth` | 2 | A compressed header that decodes to another compressed header: unbounded recursion driven by a few bytes. 2 is one encoded header containing the real one, the only nesting 7-Zip writes. | `Error::LimitExceeded { what: HeaderDepth }`. |
| `max_entries` | 1,000,000 | The amplifying counts: files, blocks, pack streams, sub-streams, bind pairs. An `ArchiveEntry` is a hundred-odd bytes reserved by one byte of count. The largest archives seen in the wild have a few hundred thousand entries. | `Error::LimitExceeded { what: Entries }` before the reservation. |
| `max_name_bytes` | 64 KiB | The names blob carries one length for all of the names in it, so without a per-name bound a single name can be the whole blob — and a name becomes a path in every consumer. Four times the longest path any mainstream filesystem accepts. | `Error::LimitExceeded { what: NameBytes }`. |
| `max_total_name_bytes` | 64 MiB | The names blob itself; a million names of 64 characters. | `Error::LimitExceeded { what: TotalNameBytes }`. |
| `max_coders_per_block` | 8 | A block's coder chain is built recursively and walked per stream. 7-Zip writes at most four (AES over BCJ2 over LZMA2). | `Error::LimitExceeded { what: CodersPerBlock }` before the coders are read. |
| `max_streams_per_coder` | 8 | `num_in_streams` / `num_out_streams` are unbounded varints, and the graph is searched linearly per stream: one coder claiming a million streams is a million-entry allocation and a quadratic walk. Only BCJ2 takes more than one stream (four in, one out). | `Error::LimitExceeded { what: StreamsPerCoder }`. |
| `max_total_coders` | 1,000,000 | A block is cheap to declare and a coder is not; the per-block bound says nothing about a million blocks. | `Error::LimitExceeded { what: TotalCoders }`. |
| `max_unpack_bytes` | unlimited | The decompression bomb. The sizes are in the header, so an archive whose declared output exceeds what the consumer will store is refused before a byte is decoded. | `Error::LimitExceeded { what: UnpackBytes }` at open. |
| `max_unpack_ratio` | unlimited | The same, expressed as output per packed byte. Off by default: LZMA legitimately reaches ratios in the thousands on repetitive data, so only a caller who knows what it is feeding should set it. | `Error::LimitExceeded { what: UnpackRatio }` at open. |
| `max_aes_cycles_power` | 24 | `2^power` SHA-256 rounds from a header a caller merely opened. The field is six bits, so 63 — 9.2 × 10^18 rounds — is expressible. 24 is what 7-Zip itself will not exceed. A caller can lower it; the crate will not go above 24 whatever is asked, because that also keeps the `1 << power` shift in range. | `Error::LimitExceeded { what: AesCyclesPower }` before the first round. |
| `reject_unsafe_paths` | false | Path traversal on extraction. Off by default because a reader is not always a writer: a consumer listing an archive should see what it really contains, and `ArchiveEntry::is_unsafe_path` says which entries are dangerous. A consumer that extracts should set it. | `Error::UnsafeEntryName { name, reason }` at open, for the first entry that would escape. |

`ArchiveLimits::unlimited()` removes every structural bound, for a caller
reading archives it wrote itself. Nothing in this crate calls it.

### Reporting which bound was hit

`Error::LimitExceeded { what, limit, requested }` carries the bound, the limit
in force, and what the archive declared — both in the bound's own unit.
`requested` is what was *declared*, not what was reached: nothing of that size
was ever allocated, decoded or derived. `Limit::field()` gives the
`ArchiveLimits` field name, and `Error::limit_hit()` maps every way a limit is
reported — including the two that predate `LimitExceeded` — onto the one enum.

## Bounds that are not caller-settable

Some things are not a budget; they are structure, and an archive that violates
them is malformed however generous the caller feels.

- **Pack streams lie inside the file.** They are laid end to end from
  `pack_pos`, so they cannot overlap or run backwards, and the last one is
  checked to end inside the archive. Otherwise a header can aim a decode at an
  arbitrary offset.
- **The coder graph is decodable.** Every bind-pair and packed-stream index is
  in range; no stream is bound or packed twice; exactly one output stream is
  unbound (the block's own output); and the graph has no cycle. A cycle
  otherwise makes the ordered coder walk revisit coders forever, stacking
  decoders until memory is gone.
- **Every count is at most the bytes the header has.** Reported as
  `Limit::ArchiveBytes`, where `limit` is what the archive could support and
  `requested` what it claimed.
- **Spans are consistent.** A block's pack-stream span, sub-stream span and
  file span are each checked against what the archive actually has, and every
  running index is a checked addition.
- **A varint stops.** The 7z NUMBER encoding is at most nine bytes and the
  decoder cannot be made to read further.

## Multi-volume archives

This crate has no volume layer: a split archive (`.7z.001`, `.7z.002`, …) is
presented to it as one reader over the concatenation, which is what the
differential test does and what consumers do. That means the volume count and
the split points are the caller's business, not a header field this crate can
be lied to about — and the pack-range check above is against the length of the
stream the caller actually presented, so an archive that describes more bytes
than the volumes hold is refused at open rather than short-reading mid-decode.

## Work that has to stay linear

An allocation bound is not enough on its own: a bounded number of things can
still be walked a quadratic number of times.

- The coder graph is searched linearly per stream, in this crate's validation,
  in `Block::get_unpack_size` and in the folder reader. `max_coders_per_block`
  and `max_streams_per_coder` are what make those searches constant work: a
  block's graph is at most 64 streams by default.
- A block's sub-stream and file spans are indexed through
  `StreamMap::block_first_sub_stream_index` and `block_first_file_index`,
  computed once, rather than re-summed per block — extracting an archive of
  many non-solid blocks was quadratic before that.
- Key derivation is the one deliberately expensive loop, and it is bounded by
  `max_aes_cycles_power`.

## The codecs

Each coder is reached with a declared output size and a properties blob, both
attacker-chosen. What bounds each:

| Coder | What it allocates | Bound |
| --- | --- | --- |
| LZMA, LZMA2 | The dictionary | **Clamped to the coder's declared unpacked size** before the budget check: a match can never reach further back than the output produced so far, so a 4 GiB dictionary on a 1 KiB stream is memory that is allocated and never read. 7-Zip reduces it the same way. Then `memory_limit_bytes`, summed over the chain. |
| LZMA2, parallel | Runs in flight (packed + unpacked at once) | The in-flight budget, which is `memory_limit_bytes` minus the decoder's own footprint. Too small to hold a run means single-threaded, not refused. |
| PPMd | `mem_size` bytes of model | The format's own `PPMD7_MIN/MAX_MEM_SIZE` and `MIN/MAX_ORDER`, then `memory_limit_bytes`, summed over the chain. |
| zstd | The back-reference window the frame declares | `window_log_max`, set from what the chain's sized coders leave of `memory_limit_bytes`, and otherwise the 128 MiB the reference decoder itself refuses to exceed. |
| brotli | The window | Bounded by the format at 16 MiB; the large-window extension is not enabled. |
| bzip2 | The block | Fixed by the format at 900 KiB. |
| deflate | The window | Fixed by the format at 32 KiB. |
| lz4 | The frame block | Bounded by the format at 4 MiB. |
| BCJ, BCJ2, Delta | Fixed-size buffers | Compile-time constants; no header field reaches them. The Delta distance is a single byte, widened before its `+1` so `0xFF` is 256 and not zero. |
| AES-256 | Key, salt, IV | Fixed sizes; the declared salt and IV sizes are checked against the properties length before either is copied, and the work factor against `max_aes_cycles_power`. |

A wrong password produces bytes that are not a valid stream, which surfaces as
`Error::MaybeBadPassword` rather than as generic corruption — the decoders
report the difference rather than the consumer having to guess.

## Extraction

The library does not extract; `util::decompress` does, and it joins each entry
name to the destination itself with a component walk that refuses `..`, a root,
and a drive prefix, treating `\` as a separator on every platform.

A consumer that writes files itself gets the same policy in queryable form:

- `ArchiveEntry::is_unsafe_path()` / `unsafe_path_reason()` — whether this
  name would leave the extraction directory, and which way.
- `ArchiveLimits::reject_unsafe_paths` — make such an archive unreadable
  instead.
- `ArchiveEntry::is_symlink()` / `unix_mode()` — a symlink is stored as an
  ordinary entry whose *content* is the link target. A consumer that does not
  check writes a small text file where a link was meant; one that does create
  the link **must treat the target as hostile**, because the target is not a
  name this crate can vet — it is decoded bytes, and it can point anywhere.

## How this is checked

- **Crafted archives**, one per bound, in `tests/security_tests.rs`: the
  smallest header that asks for more than the limit allows, asserting the typed
  bound that refused it — and, where the same archive is legitimate at the
  defaults, that it still opens.
- **Fuzzing**, in `fuzz/`: `header` runs arbitrary bytes through the whole
  header parser, and `folder_graph` places them where the coder graph goes.
  Both wrap the input in a container whose signature header describes it
  truthfully, so the fuzzer spends its time on structure rather than on
  guessing a CRC. Both run under a counting global allocator that fails the run
  if one archive allocates more than 256 MiB — far above what the limits allow
  the archive, low enough that a count believed without a bound trips it long
  before the machine notices. To run them:

  ```
  cargo +nightly fuzz run header -- -max_total_time=600
  cargo +nightly fuzz run folder_graph -- -max_total_time=600
  ```

- **The differential matrix** (`tests/differential_7zz_tests.rs`) is what says
  the defaults changed nothing: every case is decoded by `7zz` and by this
  crate, across coder chains and thread counts, and the bytes have to match.

## Reporting

See [SECURITY.md](../SECURITY.md). A panic, an unbounded allocation, a hang, or
a path escape on a crafted archive is a security report, not a bug report.
