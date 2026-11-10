//! Decoding on caller memory: an LZMA or LZMA2 dictionary, or a PPMd model
//! arena, over `ArchiveLimits::max_in_ram_window_bytes` comes from the
//! caller's `WindowAllocator` and does not count against
//! `memory_limit_bytes`.
//!
//! Every archive here is opened under a memory limit it does not fit unless
//! its windows are lent, and decoded on memory that starts as garbage; the
//! output is compared with the input byte for byte.

#![cfg(feature = "compress")]

use std::collections::BTreeMap;
use std::io::{self, Cursor};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(feature = "ppmd")]
use sevenz_turbo::encoder_options::PpmdOptions;
use sevenz_turbo::encoder_options::{DeltaOptions, Lzma2Options, LzmaOptions};
use sevenz_turbo::{
    Archive, ArchiveEntry, ArchiveLimits, ArchiveReader, ArchiveWriter, EncoderConfiguration,
    EncoderMethod, Error, Limit, Password, UnorderedEntry, WindowAllocator, WindowMemory,
};

/// What a recorder has out, and the most it has had out at once.
#[derive(Default)]
struct Live {
    windows: usize,
    bytes: u64,
    peak_windows: usize,
    peak_bytes: u64,
}

/// Heap memory filled with a pattern that is not zero, counting its drops
/// and handing its bytes back to the recorder's live count.
struct Heap {
    bytes: Box<[u8]>,
    drops: Arc<AtomicUsize>,
    live: Arc<Mutex<Live>>,
}

// SAFETY: the boxed slice is owned and initialized, its address does not
// change while the box lives, and only the decoder that holds this value
// reaches it.
unsafe impl WindowMemory for Heap {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        let mut live = self.live.lock().unwrap();
        live.windows -= 1;
        live.bytes -= self.bytes.len() as u64;
        drop(live);
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Give,
    Refuse(io::ErrorKind),
    /// Gives the first `n` windows asked for, then refuses every one after.
    RefuseAfter(usize, io::ErrorKind),
    Short,
    /// Panics on every ask.
    Panic,
}

/// Lends heap memory, records what it was asked for, counts the memory it
/// handed out that has been dropped, and keeps the most it had out at once.
struct Recorder {
    mode: Mode,
    asked: Mutex<Vec<usize>>,
    drops: Arc<AtomicUsize>,
    live: Arc<Mutex<Live>>,
}

impl Recorder {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            asked: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
            live: Arc::new(Mutex::new(Live::default())),
        })
    }

    fn asked(&self) -> Vec<usize> {
        self.asked.lock().unwrap().clone()
    }

    fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }

    /// The windows and bytes out now.
    fn out(&self) -> (usize, u64) {
        let live = self.live.lock().unwrap();
        (live.windows, live.bytes)
    }

    /// The most windows, and the most bytes, out at once.
    fn peak(&self) -> (usize, u64) {
        let live = self.live.lock().unwrap();
        (live.peak_windows, live.peak_bytes)
    }
}

impl WindowAllocator for Recorder {
    fn allocate(&self, len: usize) -> io::Result<Box<dyn WindowMemory>> {
        let nth = {
            let mut asked = self.asked.lock().unwrap();
            asked.push(len);
            asked.len()
        };
        let len = match self.mode {
            Mode::Give => len,
            Mode::Short => len - 1,
            Mode::Panic => panic!("scratch map failed"),
            Mode::Refuse(kind) => return Err(io::Error::new(kind, "scratch disk full")),
            Mode::RefuseAfter(n, kind) if nth > n => {
                return Err(io::Error::new(kind, "scratch disk full"));
            }
            Mode::RefuseAfter(..) => len,
        };
        {
            let mut live = self.live.lock().unwrap();
            live.windows += 1;
            live.bytes += len as u64;
            live.peak_windows = live.peak_windows.max(live.windows);
            live.peak_bytes = live.peak_bytes.max(live.bytes);
        }
        Ok(Box::new(Heap {
            bytes: vec![0xA5; len].into_boxed_slice(),
            drops: Arc::clone(&self.drops),
            live: Arc::clone(&self.live),
        }))
    }
}

/// What `archive` reports for `limits` on `threads` covers the most
/// `allocator` had out at once, and nothing is out now.
fn assert_within_reported(
    archive: &Archive,
    limits: &ArchiveLimits,
    threads: u32,
    allocator: &Recorder,
) {
    let reported = archive.decoder_memory(limits, threads).unwrap();
    let (windows, bytes) = allocator.peak();
    assert!(
        windows <= reported.lent_windows,
        "{windows} windows out at once on {threads} threads, {} reported",
        reported.lent_windows
    );
    assert!(
        bytes <= reported.lent_bytes,
        "{bytes} bytes out at once on {threads} threads, {} reported",
        reported.lent_bytes
    );
    assert_eq!(allocator.out(), (0, 0), "a lent window was not released");
}

/// Text-like bytes: compressible, but not a run.
fn text(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&[u8]; 12] = [
        b"amber ",
        b"basalt ",
        b"cobalt ",
        b"delta ",
        b"ember ",
        b"fjord ",
        b"granite ",
        b"harbor ",
        b"indigo ",
        b"juniper ",
        b"kestrel ",
        b"lagoon\n",
    ];
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(WORDS[(state % WORDS.len() as u64) as usize]);
        if state.is_multiple_of(7) {
            out.extend_from_slice(&(state as u32).to_le_bytes());
        }
    }
    out.truncate(len);
    out
}

/// Barely compressible bytes with an x86 call or jump every eleven bytes, so
/// a BCJ2 encoder has something to move to each of its streams.
fn x86(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out: Vec<u8> = (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8 & 0x3F
        })
        .collect();
    let mut i = 0;
    while i + 5 <= out.len() {
        out[i] = if i % 3 == 0 { 0xE9 } else { 0xE8 };
        out[i + 1..i + 5].copy_from_slice(&((i as u32 * 3) % 0x4000).to_le_bytes());
        i += 11;
    }
    out
}

/// One archive of `members`, each in a folder of its own.
fn write(methods: Vec<EncoderConfiguration>, members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(methods);
    for (name, data) in members {
        writer
            .push_archive_entry(ArchiveEntry::new_file(name), Some(*data))
            .unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// How many of `archive`'s coders decode into a window: LZMA, LZMA2, PPMd.
fn windowed_coders(archive: &Archive) -> usize {
    archive
        .blocks
        .iter()
        .flat_map(|block| &block.coders)
        .filter(|coder| {
            let id = coder.encoder_method_id();
            id == EncoderMethod::ID_LZMA
                || id == EncoderMethod::ID_LZMA2
                || id == EncoderMethod::ID_PPMD
        })
        .count()
}

/// A memory limit `bytes` fits only with its windows lent: its estimate,
/// less half its largest folder's windows.
fn tight_limit(bytes: &[u8]) -> u64 {
    let archive = Archive::read(&mut Cursor::new(bytes), &Password::empty()).unwrap();
    let estimate = archive.decoder_memory_estimate().unwrap();
    let largest_window = archive
        .blocks
        .iter()
        .flat_map(|block| &block.coders)
        .map(|coder| {
            let props = coder.properties();
            let id = coder.encoder_method_id();
            if id == EncoderMethod::ID_LZMA || id == EncoderMethod::ID_PPMD {
                u64::from(u32::from_le_bytes(props[1..5].try_into().unwrap()))
            } else if id == EncoderMethod::ID_LZMA2 {
                let p = u32::from(props[0]);
                u64::from((2 | (p & 1)) << (p / 2 + 11))
            } else {
                0
            }
        })
        .max()
        .unwrap();
    assert!(largest_window > 1 << 20, "a window too small to matter");
    estimate - largest_window / 2
}

/// Every member's bytes, by name, decoded in order on `threads` threads.
fn decode(
    bytes: &[u8],
    limits: ArchiveLimits,
    threads: u32,
) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    let mut reader = ArchiveReader::with_limits(Cursor::new(bytes), Password::empty(), limits)?;
    reader.set_threads(threads);
    let mut out = BTreeMap::new();
    reader.for_each_entries(|entry, read| {
        let mut data = Vec::new();
        read.read_to_end(&mut data)?;
        out.insert(entry.name().to_string(), data);
        Ok(true)
    })?;
    Ok(out)
}

fn lending(allocator: &Arc<Recorder>, memory_limit_bytes: u64) -> ArchiveLimits {
    ArchiveLimits::memory(memory_limit_bytes)
        .with_window_allocator(Arc::clone(allocator) as Arc<dyn WindowAllocator>, 1 << 20)
}

/// The archive is refused under the tight limit without an allocator, and
/// with one decodes to `members` on lent memory, every window released by
/// the end and never more out at once than `decoder_memory` reports.
fn assert_decodes_on_lent_memory(bytes: &[u8], members: &[(&str, &[u8])], threads: u32) {
    let limit = tight_limit(bytes);
    let refused = decode(bytes, ArchiveLimits::memory(limit), threads).unwrap_err();
    assert_eq!(refused.limit_hit(), Some(Limit::MemoryBytes), "{refused}");

    let archive = Archive::read(&mut Cursor::new(bytes), &Password::empty()).unwrap();
    let allocator = Recorder::new(Mode::Give);
    let limits = lending(&allocator, limit);
    let out = decode(bytes, limits.clone(), threads).unwrap();
    assert_eq!(out.len(), members.len());
    for (name, data) in members {
        assert!(out[*name] == *data, "{name} decoded to other bytes");
    }
    let asked = allocator.asked();
    assert!(!asked.is_empty());
    if threads == 1 {
        assert!(asked.len() <= windowed_coders(&archive));
    }
    assert!(asked.iter().all(|&len| len > 1 << 20));
    assert_eq!(
        allocator.drops(),
        asked.len(),
        "a lent window was not released"
    );
    assert_within_reported(&archive, &limits, threads, &allocator);

    // What stays in RAM fits the limit the archive did not fit with its
    // windows in RAM.
    let in_ram = archive
        .decoder_memory(&ArchiveLimits::memory(limit), threads)
        .unwrap();
    assert_eq!(
        (
            in_ram.resident_bytes,
            in_ram.lent_windows,
            in_ram.lent_bytes
        ),
        (archive.decoder_memory_estimate().unwrap(), 0, 0)
    );
    let split = archive.decoder_memory(&limits, threads).unwrap();
    assert!(split.resident_bytes <= limit && limit < in_ram.resident_bytes);
}

/// An LZMA2 archive of several runs, each starting with a dictionary reset,
/// so a threaded decoder has runs to hand its workers: 6 MiB of text in
/// 1 MiB blocks over a 2 MiB dictionary.
fn multi_run(seed: u64) -> (Vec<u8>, Vec<u8>) {
    let data = text(6 << 20, seed);
    let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
    options.set_dictionary_size(2 << 20);
    let bytes = write(vec![options.into()], &[("story.txt", data.as_slice())]);
    (bytes, data)
}

/// Every window, on every path below, is lent from the start.
fn lending_all(allocator: &Arc<Recorder>) -> ArchiveLimits {
    ArchiveLimits::default()
        .with_window_allocator(Arc::clone(allocator) as Arc<dyn WindowAllocator>, 0)
}

#[test]
fn lzma_decodes_on_lent_memory() {
    let data = text(3 << 20, 1);
    let mut options = LzmaOptions::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(vec![options.into()], &members);
    assert_decodes_on_lent_memory(&bytes, &members, 1);
}

#[test]
fn lzma2_decodes_on_lent_memory() {
    let data = text(3 << 20, 2);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(vec![options.into()], &members);
    assert_decodes_on_lent_memory(&bytes, &members, 1);
}

#[cfg(feature = "ppmd")]
#[test]
fn ppmd_decodes_on_lent_memory() {
    let data = text(1 << 20, 3);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(
        vec![PpmdOptions::from_order_memory_size(6, 8 << 20).into()],
        &members,
    );
    assert_decodes_on_lent_memory(&bytes, &members, 1);
}

/// BCJ2's main stream and its call and jump streams each have a coder of
/// their own; every one over the in-RAM limit is lent.
#[test]
fn bcj2_decodes_on_lent_memory() {
    let data = x86(3 << 20, 4);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("tool.exe", data.as_slice())];
    let bytes = write(
        vec![options.into(), EncoderMethod::BCJ2_FILTER.into()],
        &members,
    );
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    assert!(
        archive.blocks[0]
            .coders
            .iter()
            .any(|coder| coder.encoder_method_id() == EncoderMethod::ID_BCJ2)
    );
    assert_decodes_on_lent_memory(&bytes, &members, 1);
}

#[test]
fn a_filter_chain_decodes_on_lent_memory() {
    let data = text(3 << 20, 5);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(
        vec![options.into(), DeltaOptions::from_distance(4).into()],
        &members,
    );
    assert_decodes_on_lent_memory(&bytes, &members, 1);
}

/// A threaded LZMA2 decode lends every worker's buffer, on two and on four
/// threads: the output is the owned decode's, the thread count is the
/// caller's, more than one window is out at once, and never more than
/// `decoder_memory` reports.
#[test]
fn threaded_lzma2_decodes_on_lent_memory() {
    let (bytes, data) = multi_run(6);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let owned = decode(&bytes, ArchiveLimits::default(), 4).unwrap();
    assert!(owned["story.txt"] == data);
    for threads in [2, 4] {
        let allocator = Recorder::new(Mode::Give);
        let limits = lending(&allocator, u64::MAX);
        let out = decode(&bytes, limits.clone(), threads).unwrap();
        assert!(
            out["story.txt"] == owned["story.txt"],
            "decoded to other bytes on {threads} threads"
        );
        let asked = allocator.asked();
        assert!(asked.len() > 1, "{threads} threads asked {asked:?}");
        assert_eq!(allocator.drops(), asked.len());
        assert_within_reported(&archive, &limits, threads, &allocator);
        // Every buffer a worker holds is on the caller's memory: what the
        // limit is checked against is the coder's state alone.
        assert!(
            archive
                .decoder_memory(&limits, threads)
                .unwrap()
                .resident_bytes
                < 2 << 20
        );
    }
}

/// On four threads, a refusal on the k-th window asked fails the decode with
/// the allocator's own error, and every window lent before it is released.
#[test]
fn threaded_lzma2_refusal_on_any_ask_releases_everything() {
    let (bytes, _) = multi_run(15);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let total = {
        let allocator = Recorder::new(Mode::Give);
        decode(&bytes, lending(&allocator, u64::MAX), 4).unwrap();
        allocator.asked().len()
    };
    assert!(total > 1);
    for given in 0..total {
        let allocator = Recorder::new(Mode::RefuseAfter(given, io::ErrorKind::StorageFull));
        let limits = lending(&allocator, u64::MAX);
        match decode(&bytes, limits.clone(), 4) {
            Err(Error::WindowAllocatorFailed { bytes, error }) => {
                assert_eq!(error.kind(), io::ErrorKind::StorageFull);
                assert_eq!(error.to_string(), "scratch disk full");
                let asked = allocator.asked();
                assert!(asked.len() > given);
                // One of the asks refused; which one first fails the
                // decode is the workers' race.
                assert!(
                    asked[given..].iter().any(|&len| len as u64 == bytes),
                    "refused after {given}: {bytes} not among {asked:?}"
                );
            }
            // A schedule that needs fewer windows than the one counted can
            // finish before the refusal: everything is still released.
            Ok(_) => assert!(allocator.asked().len() <= given),
            Err(other) => panic!("refused after {given}: {other:?}"),
        }
        assert_within_reported(&archive, &limits, 4, &allocator);
        assert_eq!(allocator.drops(), allocator.asked().len().min(given));
    }
}

/// A damaged LZMA2 stream decoded on four threads fails, and every lent
/// window is released.
#[test]
fn a_damaged_threaded_stream_releases_everything() {
    let (mut bytes, _) = multi_run(16);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let range = archive.block_pack_streams(0)[0];
    let (pack, packed) = (range.offset as usize, range.size as usize);
    for at in [packed / 3, packed / 2, packed * 5 / 6] {
        bytes[pack + at] ^= 0x55;
    }
    let allocator = Recorder::new(Mode::Give);
    let limits = lending(&allocator, u64::MAX);
    assert!(decode(&bytes, limits.clone(), 4).is_err());
    assert_within_reported(&archive, &limits, 4, &allocator);
    assert_eq!(allocator.drops(), allocator.asked().len());
}

/// A small dictionary over one huge run: no window a threaded decoder asks
/// for is over `lzma-turbo`'s worker bound for the dictionary, however much
/// the run decodes to.
#[test]
fn a_small_dictionary_with_a_huge_run_never_asks_over_the_bound() {
    let data = text(12 << 20, 17);
    let mut options = Lzma2Options::from_level_mt(1, 4, 12 << 20);
    options.set_dictionary_size(64 << 10);
    let bytes = write(vec![options.into()], &[("story.txt", data.as_slice())]);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let prop = archive.blocks[0].coders[0].properties()[0];
    let bound = lzma_turbo::Lzma2Decoder::max_worker_window_len(prop).unwrap();
    assert!(bound < 12 << 20);
    for threads in [1, 2, 4] {
        let allocator = Recorder::new(Mode::Give);
        let limits = lending_all(&allocator);
        let out = decode(&bytes, limits.clone(), threads).unwrap();
        assert!(out["story.txt"] == data);
        let asked = allocator.asked();
        assert!(!asked.is_empty());
        assert!(
            asked.iter().all(|&len| len <= bound),
            "{threads} threads asked {asked:?} over {bound}"
        );
        assert_within_reported(&archive, &limits, threads, &allocator);
    }
}

/// On four threads, a dictionary over `max_lent_window_bytes` is refused as
/// a limit before the allocator is asked; a ceiling over the dictionary but
/// under the worker bound decodes, every window held to the ceiling.
#[test]
fn threaded_lzma2_respects_the_lent_ceiling() {
    let (bytes, data) = multi_run(18);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let prop = archive.blocks[0].coders[0].properties()[0];
    let dictionary = lzma_turbo::Lzma2Decoder::window_len(prop).unwrap();
    let worker = lzma_turbo::Lzma2Decoder::max_worker_window_len(prop).unwrap();
    assert!(dictionary < worker);

    let allocator = Recorder::new(Mode::Give);
    let mut limits = lending(&allocator, u64::MAX);
    limits.max_lent_window_bytes = dictionary as u64 - 1;
    let error = decode(&bytes, limits, 4).unwrap_err();
    assert!(
        matches!(
            error,
            Error::LimitExceeded {
                what: Limit::LentWindowBytes,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(allocator.asked().is_empty());

    let allocator = Recorder::new(Mode::Give);
    let mut limits = lending(&allocator, u64::MAX);
    limits.max_lent_window_bytes = dictionary as u64;
    let out = decode(&bytes, limits.clone(), 4).unwrap();
    assert!(out["story.txt"] == data);
    let asked = allocator.asked();
    assert!(
        asked.iter().all(|&len| len <= dictionary),
        "asked {asked:?} over the ceiling {dictionary}"
    );
    assert_within_reported(&archive, &limits, 4, &allocator);
}

/// A refusing allocator fails a threaded decode with its own error.
#[test]
fn a_refusing_allocator_fails_a_threaded_decode_with_its_own_error() {
    let (bytes, _) = multi_run(19);
    for threads in [2, 4] {
        let allocator = Recorder::new(Mode::Refuse(io::ErrorKind::QuotaExceeded));
        let error = decode(&bytes, lending(&allocator, u64::MAX), threads).unwrap_err();
        match &error {
            Error::WindowAllocatorFailed { bytes, error } => {
                assert_eq!(error.kind(), io::ErrorKind::QuotaExceeded);
                assert_eq!(error.to_string(), "scratch disk full");
                assert!(allocator.asked().contains(&(*bytes as usize)));
            }
            other => panic!("not the allocator's error on {threads} threads: {other:?}"),
        }
        assert_eq!(allocator.out(), (0, 0));
    }
}

/// Runs of one folder decoded on their own, as a caller drives a
/// [`sevenz_turbo::PositionalLzma2Folder`] on several threads: each run
/// lends one ring, the output is the archive's, and never more is out at
/// once than `decoder_memory` reports for that many runs at a time.
#[test]
fn positional_runs_decode_on_lent_memory() {
    use sevenz_turbo::PositionalLzma2Folder;
    use std::sync::atomic::AtomicBool;

    let (bytes, data) = multi_run(20);
    let bytes = Arc::new(bytes);
    let archive = Arc::new(Archive::read(&mut Cursor::new(&*bytes), &Password::empty()).unwrap());
    for (mode, threads) in [
        (Mode::Give, 2u32),
        (Mode::Give, 4),
        (Mode::Refuse(io::ErrorKind::StorageFull), 2),
    ] {
        let allocator = Recorder::new(mode);
        let limits = lending_all(&allocator);
        let folder = PositionalLzma2Folder::new(archive.clone(), bytes.clone(), 0, limits.clone())
            .unwrap()
            .unwrap();
        let pack = folder.packed_range();
        let mut scanner = folder.scanner();
        let mut packed = &bytes[pack.offset as usize..pack.end() as usize];
        let mut runs = Vec::new();
        while !packed.is_empty() {
            let used = scanner.feed(packed).unwrap();
            packed = &packed[used..];
            while let Some(run) = scanner.next_run() {
                runs.push(run);
            }
        }
        scanner.finish().unwrap();
        assert!(runs.len() > 1);
        let out = Mutex::new(vec![0u8; data.len()]);
        let failed = Mutex::new(Vec::new());
        let next = AtomicUsize::new(0);
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        let Some(run) = runs.get(i) else { break };
                        let result = run.clone().decode(&cancel, &mut |offset, piece: &[u8]| {
                            let at = offset as usize;
                            out.lock().unwrap()[at..at + piece.len()].copy_from_slice(piece);
                            Ok(piece.len())
                        });
                        if let Err(error) = result {
                            failed.lock().unwrap().push(error);
                        }
                    }
                });
            }
        });
        let failed = failed.into_inner().unwrap();
        match mode {
            Mode::Give => {
                assert!(failed.is_empty(), "{failed:?}");
                assert!(out.into_inner().unwrap() == data);
                assert_eq!(allocator.asked().len(), runs.len());
            }
            _ => {
                assert_eq!(failed.len(), runs.len());
                for error in failed {
                    assert!(
                        matches!(&error, Error::WindowAllocatorFailed { error, .. } if error.kind() == io::ErrorKind::StorageFull && error.to_string() == "scratch disk full"),
                        "{error:?}"
                    );
                }
            }
        }
        assert_within_reported(&archive, &limits, threads, &allocator);
    }
}

/// BCJ2's coders on four threads, and a PPMd model on two: the windows out
/// at once are within what `decoder_memory` reports.
#[test]
fn bcj2_on_threads_stays_within_what_is_reported() {
    let data = x86(6 << 20, 21);
    let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
    options.set_dictionary_size(2 << 20);
    let members = [("tool.exe", data.as_slice())];
    let bytes = write(
        vec![options.into(), EncoderMethod::BCJ2_FILTER.into()],
        &members,
    );
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    for threads in [1, 4] {
        let allocator = Recorder::new(Mode::Give);
        let limits = lending_all(&allocator);
        let out = decode(&bytes, limits.clone(), threads).unwrap();
        assert!(out["tool.exe"] == data);
        assert!(!allocator.asked().is_empty());
        assert_within_reported(&archive, &limits, threads, &allocator);
    }
}

#[cfg(feature = "ppmd")]
#[test]
fn ppmd_on_threads_stays_within_what_is_reported() {
    let first = text(1 << 20, 22);
    let second = text(1 << 20, 23);
    let members = [
        ("one.txt", first.as_slice()),
        ("two.txt", second.as_slice()),
    ];
    let bytes = write(
        vec![PpmdOptions::from_order_memory_size(6, 8 << 20).into()],
        &members,
    );
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    for threads in [1, 2] {
        let allocator = Recorder::new(Mode::Give);
        let limits = lending_all(&allocator);
        let out = decode(&bytes, limits.clone(), threads).unwrap();
        assert!(out["one.txt"] == first && out["two.txt"] == second);
        assert!(!allocator.asked().is_empty());
        assert_within_reported(&archive, &limits, threads, &allocator);
    }
}

/// Folders decoded side by side each lend their own window, from several
/// threads at once.
#[test]
fn folders_decoded_in_parallel_each_lend_their_window() {
    let first = text(3 << 20, 7);
    let second = text(3 << 20, 8);
    let third = text(3 << 20, 9);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [
        ("one.txt", first.as_slice()),
        ("two.txt", second.as_slice()),
        ("three.txt", third.as_slice()),
    ];
    let bytes = write(vec![options.into()], &members);
    let allocator = Recorder::new(Mode::Give);
    let limits = ArchiveLimits::default()
        .with_window_allocator(Arc::clone(&allocator) as Arc<dyn WindowAllocator>, 1 << 20);
    let mut reader = ArchiveReader::from_read_at(bytes.clone(), Password::empty(), limits).unwrap();
    reader.set_threads(3);
    let mut out: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    reader
        .for_each_entries_unordered(|event| {
            match event {
                UnorderedEntry::Begin { file_index, .. } => {
                    out.insert(file_index, Vec::new());
                }
                UnorderedEntry::Data {
                    file_index, bytes, ..
                } => out.get_mut(&file_index).unwrap().extend_from_slice(bytes),
                _ => {}
            }
            Ok(true)
        })
        .unwrap();
    drop(reader);
    for (index, (_, data)) in members.iter().enumerate() {
        assert!(
            out[&index] == *data,
            "member {index} decoded to other bytes"
        );
    }
    assert_eq!(allocator.asked().len(), 3);
    assert_eq!(allocator.drops(), 3);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let limits = ArchiveLimits::default()
        .with_window_allocator(Arc::clone(&allocator) as Arc<dyn WindowAllocator>, 1 << 20);
    assert_within_reported(&archive, &limits, 3, &allocator);
}

/// A window at or under the in-RAM limit is allocated as before, and the
/// allocator is never asked.
#[test]
fn a_window_under_the_in_ram_limit_is_not_lent() {
    let data = text(3 << 20, 10);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(vec![options.into()], &members);
    let allocator = Recorder::new(Mode::Give);
    let limits = ArchiveLimits::default()
        .with_window_allocator(Arc::clone(&allocator) as Arc<dyn WindowAllocator>, 8 << 20);
    let out = decode(&bytes, limits, 1).unwrap();
    assert!(out["story.txt"] == data);
    assert!(allocator.asked().is_empty());
}

/// The allocator's own error reaches the caller whole, named as the
/// allocator's and not as damage in the archive.
#[test]
fn a_refusing_allocator_fails_the_decode_with_its_own_error() {
    let data = text(3 << 20, 11);
    let members = [("story.txt", data.as_slice())];
    let mut lzma2 = Lzma2Options::from_level(1);
    lzma2.set_dictionary_size(4 << 20);
    let mut lzma = LzmaOptions::from_level(1);
    lzma.set_dictionary_size(4 << 20);
    for method in [EncoderConfiguration::from(lzma2), lzma.into()] {
        let bytes = write(vec![method], &members);
        let allocator = Recorder::new(Mode::Refuse(io::ErrorKind::StorageFull));
        let error = decode(&bytes, lending(&allocator, u64::MAX), 1).unwrap_err();
        match &error {
            Error::WindowAllocatorFailed { bytes, error } => {
                assert_eq!(error.kind(), io::ErrorKind::StorageFull);
                assert_eq!(error.to_string(), "scratch disk full");
                assert_eq!(*bytes, allocator.asked()[0] as u64);
            }
            other => panic!("not the allocator's error: {other:?}"),
        }
        assert_eq!(error.io_error().unwrap().kind(), io::ErrorKind::StorageFull);
        assert_eq!(allocator.asked().len(), 1);
    }
}

#[cfg(feature = "ppmd")]
#[test]
fn a_refusing_allocator_fails_a_ppmd_decode_with_its_own_error() {
    let data = text(256 << 10, 12);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(
        vec![PpmdOptions::from_order_memory_size(6, 8 << 20).into()],
        &members,
    );
    let allocator = Recorder::new(Mode::Refuse(io::ErrorKind::QuotaExceeded));
    let error = decode(&bytes, lending(&allocator, u64::MAX), 1).unwrap_err();
    assert!(
        matches!(&error, Error::WindowAllocatorFailed { error, .. } if error.kind() == io::ErrorKind::QuotaExceeded),
        "{error:?}"
    );
}

/// Memory shorter than the window asked for is refused before the codec
/// uses it, and released.
#[test]
fn memory_shorter_than_asked_is_refused_and_released() {
    let data = text(3 << 20, 13);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(vec![options.into()], &members);
    let allocator = Recorder::new(Mode::Short);
    let error = decode(&bytes, lending(&allocator, u64::MAX), 1).unwrap_err();
    assert!(
        matches!(&error, Error::WindowAllocatorFailed { error, .. } if error.kind() == io::ErrorKind::InvalidInput),
        "{error:?}"
    );
    assert_eq!(allocator.asked().len(), 1);
    assert_eq!(allocator.drops(), 1);
}

/// A window over `max_lent_window_bytes` is refused as a limit, and the
/// allocator is never asked.
#[test]
fn a_window_over_the_ceiling_is_refused_unasked() {
    let data = text(3 << 20, 14);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(vec![options.into()], &members);
    let allocator = Recorder::new(Mode::Give);
    let mut limits = lending(&allocator, u64::MAX);
    limits.max_lent_window_bytes = 2 << 20;
    let error = decode(&bytes, limits, 1).unwrap_err();
    assert!(
        matches!(
            error,
            Error::LimitExceeded {
                what: Limit::LentWindowBytes,
                limit,
                requested,
            } if limit == 2 << 20 && requested > 2 << 20
        ),
        "{error:?}"
    );
    assert_eq!(Limit::LentWindowBytes.field(), "max_lent_window_bytes");
    assert!(allocator.asked().is_empty());
}

/// Every public read path decodes under the caller's limits and so reaches
/// the one lending decision: each decode below asks the allocator for the
/// folder's dictionary, gets the archive's bytes, and releases the window.
#[test]
fn every_public_read_path_lends_through_the_callers_limits() {
    use sevenz_turbo::{BlockDecoder, PositionalBlockDecoder};

    let data = text(3 << 20, 10);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(vec![options.into()], &members);
    let allocator = Recorder::new(Mode::Give);
    let limits = lending(&allocator, u64::MAX);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let mut asked = 0;
    let mut check = |path: &str, out: Vec<u8>| {
        assert!(out == data, "{path}: decoded to other bytes");
        asked += 1;
        assert_eq!(
            allocator.asked().len(),
            asked,
            "{path}: the window was not lent"
        );
        assert_eq!(
            allocator.drops(),
            asked,
            "{path}: the window was not released"
        );
    };
    let read_all = |rd: &mut dyn io::Read| {
        let mut out = Vec::new();
        rd.read_to_end(&mut out).map(|_| out)
    };

    let mut reader = ArchiveReader::with_limits(
        Cursor::new(bytes.clone()),
        Password::empty(),
        limits.clone(),
    )
    .unwrap();
    let mut out = Vec::new();
    reader
        .for_each_entries(|_, rd| {
            out = read_all(rd)?;
            Ok(true)
        })
        .unwrap();
    check("ArchiveReader::for_each_entries", out);

    let mut out = Vec::new();
    reader
        .for_each_entries_direct(|_, rd| {
            out = read_all(rd)?;
            Ok(true)
        })
        .unwrap();
    check("ArchiveReader::for_each_entries_direct", out);

    check(
        "ArchiveReader::read_file",
        reader.read_file("story.txt").unwrap(),
    );

    let mut out = Vec::new();
    reader
        .block_decoder(0)
        .unwrap()
        .for_each_entries(&mut |_, rd| {
            out = read_all(rd)?;
            Ok(true)
        })
        .unwrap();
    check("ArchiveReader::block_decoder", out);

    let mut source = Cursor::new(bytes.clone());
    let password = Password::empty();
    let mut out = Vec::new();
    BlockDecoder::with_limits(1, 0, &archive, &password, &mut source, limits.clone())
        .for_each_entries(&mut |_, rd| {
            out = read_all(rd)?;
            Ok(true)
        })
        .unwrap();
    check("BlockDecoder::with_limits", out);

    let mut out = Vec::new();
    PositionalBlockDecoder::with_limits(
        Arc::new(archive.clone()),
        Arc::new(Password::empty()),
        Arc::new(bytes.clone()),
        0,
        limits.clone(),
    )
    .unwrap()
    .for_each_entries(&mut |_, rd| {
        out = read_all(rd)?;
        Ok(true)
    })
    .unwrap();
    check("PositionalBlockDecoder::with_limits", out);

    let mut reader = ArchiveReader::from_archive_with_limits(
        archive.clone(),
        Cursor::new(bytes.clone()),
        Password::empty(),
        limits.clone(),
    )
    .unwrap();
    check(
        "ArchiveReader::from_archive_with_limits",
        reader.read_file("story.txt").unwrap(),
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("story.7z");
    std::fs::write(&path, &bytes).unwrap();
    let mut reader =
        ArchiveReader::open_with_limits(&path, Password::empty(), limits.clone()).unwrap();
    check(
        "ArchiveReader::open_with_limits",
        reader.read_file("story.txt").unwrap(),
    );

    #[cfg(feature = "util")]
    {
        let out_dir = dir.path().join("decompress");
        sevenz_turbo::decompress_with_limits(Cursor::new(bytes.clone()), &out_dir, limits.clone())
            .unwrap();
        check(
            "decompress_with_limits",
            std::fs::read(out_dir.join("story.txt")).unwrap(),
        );
    }

    // `extract_to` on a file opened under the limits, on four threads, with
    // one member of several runs over a dictionary smaller than it: every
    // gate of the ring extraction passes but the lending one, which sends a
    // lent dictionary down the ordinary path, so the dictionary is asked
    // for and every window is released.
    #[cfg(feature = "util")]
    extract_to_reaches_the_lending_gate(dir.path());
}

#[cfg(feature = "util")]
fn extract_to_reaches_the_lending_gate(dir: &std::path::Path) {
    let (bytes, data) = multi_run(21);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let coder = &archive.blocks[0].coders[0];
    let dictionary = lzma_turbo::Lzma2Decoder::window_len(coder.properties()[0]).unwrap();
    assert!((dictionary as u64) < archive.blocks[0].get_unpack_size());
    let path = dir.join("story-runs.7z");
    std::fs::write(&path, &bytes).unwrap();
    for (in_ram, lent) in [(1u64 << 20, true), (u64::MAX, false)] {
        let allocator = Recorder::new(Mode::Give);
        let limits = ArchiveLimits::default()
            .with_window_allocator(Arc::clone(&allocator) as Arc<dyn WindowAllocator>, in_ram);
        let mut reader =
            ArchiveReader::open_with_limits(&path, Password::empty(), limits.clone()).unwrap();
        reader.set_threads(4);
        let out_dir = dir.join(format!("extract_to-{lent}"));
        reader.extract_to(&out_dir).unwrap();
        assert!(std::fs::read(out_dir.join("story.txt")).unwrap() == data);
        let asked = allocator.asked();
        // Lent, the threaded decode asks; kept in RAM, the rings are
        // allocated in RAM and nothing is asked.
        assert_eq!(
            !asked.is_empty(),
            lent,
            "extract_to with the dictionary lent: {lent}, asked {asked:?}"
        );
        assert_eq!(allocator.drops(), asked.len());
        assert_within_reported(&archive, &limits, 4, &allocator);
    }
}

/// An encoded header is decoded under the caller's limits too, through the
/// same decision: with every window lent, reading the archive's metadata
/// asks for the header coder's dictionary, clamped to the header's size.
#[test]
fn an_encoded_header_is_lent_by_the_same_decision() {
    // Enough names that the writer compresses the header rather than keep
    // it raw.
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_encrypt_header(false);
    for index in 0..512 {
        writer
            .push_archive_entry(
                ArchiveEntry::new_file(&format!("chapters/chapter-{index:04}.txt")),
                Some(&b"story"[..]),
            )
            .unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();
    let allocator = Recorder::new(Mode::Give);
    let every_window = ArchiveLimits::default()
        .with_window_allocator(Arc::clone(&allocator) as Arc<dyn WindowAllocator>, 0);
    let archive =
        Archive::read_with_limits(&mut Cursor::new(&bytes), &Password::empty(), &every_window)
            .unwrap();
    assert_eq!(archive.files[0].name(), "chapters/chapter-0000.txt");
    let asked = allocator.asked();
    assert_eq!(asked.len(), 1, "the header's window was not lent");
    assert!(asked[0] <= 1 << 20, "a header window of {} bytes", asked[0]);
    assert_eq!(allocator.drops(), 1);

    let unlent = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    assert_eq!(archive.files.len(), unlent.files.len());
}

// `ArchiveLimits` keeps its auto traits with an allocator in it.
const _: () = {
    const fn auto_traits<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe + Send + Sync>() {}
    auto_traits::<ArchiveLimits>();
};

/// One member of 6 MiB in an LZMA2 folder whose coder declares a 64 MiB
/// dictionary: the declared window is over a 16 MiB in-RAM limit, the
/// window the decoder clamps it to is not.
fn declared_over_clamped_under() -> (Vec<u8>, Vec<u8>) {
    let data = text(6 << 20, 22);
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(64 << 20);
    let bytes = write(vec![options.into()], &[("story.txt", data.as_slice())]);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let prop = archive.blocks[0].coders[0].properties()[0];
    assert!(lzma_turbo::Lzma2Decoder::window_len(prop).unwrap() > 16 << 20);
    (bytes, data)
}

/// `resident_bytes` and the check `with_limits` makes up front count a
/// declared dictionary over the in-RAM limit at the window the decoder
/// clamps it to, which stays in RAM: a limit under it is refused before
/// decoding, not part way, and a limit over it decodes with nothing lent.
#[test]
fn a_declared_dictionary_clamped_into_ram_is_counted_as_ram() {
    let (bytes, data) = declared_over_clamped_under();
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let allocator = Recorder::new(Mode::Give);
    let limits = |memory: u64| {
        ArchiveLimits::memory(memory)
            .with_window_allocator(Arc::clone(&allocator) as Arc<dyn WindowAllocator>, 16 << 20)
    };
    let reported = archive.decoder_memory(&limits(u64::MAX), 4).unwrap();
    assert!(
        reported.resident_bytes >= (6 << 20) + (1 << 20),
        "{reported:?}"
    );
    // `lent_*` may still count the worker buffers a threaded LZMA2 decode
    // could lend (an upper bound); the dictionary itself is RAM.
    assert_eq!(
        reported.resident_bytes,
        archive.decoder_memory_estimate().unwrap()
    );

    let error = decode(&bytes, limits(4 << 20), 1).unwrap_err();
    assert!(
        matches!(error, Error::MemoryLimited { required_bytes, .. }
            if required_bytes == reported.resident_bytes),
        "{error:?}"
    );
    for threads in [1, 4] {
        let out = decode(&bytes, limits(24 << 20), threads).unwrap();
        assert!(out["story.txt"] == data);
    }
    assert!(allocator.asked().is_empty());
}

/// Four folders of 6 MiB declaring 64 MiB dictionaries, decoded side by
/// side from a positional source under a 24 MiB limit with a 16 MiB in-RAM
/// limit: each dictionary is clamped into RAM, so the plan counts it, and
/// both walks decode every folder in RAM, none refused.
#[test]
fn small_folders_with_dictionaries_clamped_into_ram_fit_side_by_side() {
    let members: Vec<(String, Vec<u8>)> = (0..4)
        .map(|index| (format!("{index}.txt"), text(6 << 20, 30 + index)))
        .collect();
    let named: Vec<(&str, &[u8])> = members
        .iter()
        .map(|(name, data)| (name.as_str(), data.as_slice()))
        .collect();
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(64 << 20);
    let bytes = write(vec![options.into()], &named);
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    assert_eq!(archive.blocks.len(), 4);
    let allocator = Recorder::new(Mode::Give);
    let limits = ArchiveLimits::memory(24 << 20)
        .with_window_allocator(Arc::clone(&allocator) as Arc<dyn WindowAllocator>, 16 << 20);

    let mut reader =
        ArchiveReader::from_read_at(bytes.clone(), Password::empty(), limits.clone()).unwrap();
    reader.set_threads(4);
    let mut out = BTreeMap::new();
    reader
        .for_each_entries(|entry, read| {
            let mut data = Vec::new();
            read.read_to_end(&mut data)?;
            out.insert(entry.name().to_string(), data);
            Ok(true)
        })
        .unwrap();
    for (name, data) in &members {
        assert!(out[name] == *data, "{name} decoded to other bytes");
    }

    let mut reader = ArchiveReader::from_read_at(bytes, Password::empty(), limits).unwrap();
    reader.set_threads(4);
    let mut out = BTreeMap::<usize, Vec<u8>>::new();
    reader
        .for_each_entries_unordered(|event| {
            match event {
                UnorderedEntry::Begin { file_index, .. } => {
                    out.insert(file_index, Vec::new());
                }
                UnorderedEntry::Data {
                    file_index, bytes, ..
                } => out.get_mut(&file_index).unwrap().extend_from_slice(bytes),
                UnorderedEntry::End { .. } => {}
                _ => unreachable!(),
            }
            Ok(true)
        })
        .unwrap();
    for (index, (_, data)) in members.iter().enumerate() {
        assert!(out[&index] == *data, "{index} decoded to other bytes");
    }
    assert!(allocator.asked().is_empty());
}

/// An LZMA2 archive whose runs are larger than its dictionary: 24 MiB of
/// text in 4 MiB blocks over a 1 MiB dictionary, so `lzma-turbo`'s worker
/// bound is four dictionaries and each run fills it.
fn runs_over_the_dictionary() -> (Vec<u8>, Vec<u8>) {
    let data = text(24 << 20, 23);
    let mut options = Lzma2Options::from_level_mt(1, 4, 4 << 20);
    options.set_dictionary_size(1 << 20);
    let bytes = write(vec![options.into()], &[("story.txt", data.as_slice())]);
    (bytes, data)
}

/// The rule is per buffer: with the dictionary under the in-RAM limit and
/// the workers' run buffers over it, a threaded decode keeps the dictionary
/// in RAM, charged, and lends every run buffer over the limit and nothing
/// under it; `decoder_memory` charges the dictionary as RAM and covers the
/// buffers lent.
#[test]
fn a_dictionary_in_ram_does_not_stop_the_workers_lending() {
    let (bytes, data) = runs_over_the_dictionary();
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let prop = archive.blocks[0].coders[0].properties()[0];
    let dictionary = lzma_turbo::Lzma2Decoder::window_len(prop).unwrap();
    let worker = lzma_turbo::Lzma2Decoder::max_worker_window_len(prop).unwrap();
    let in_ram = 2usize << 20;
    assert!(dictionary <= in_ram && in_ram < worker);
    for threads in [2, 4] {
        let allocator = Recorder::new(Mode::Give);
        let limits = ArchiveLimits::default().with_window_allocator(
            Arc::clone(&allocator) as Arc<dyn WindowAllocator>,
            in_ram as u64,
        );
        let out = decode(&bytes, limits.clone(), threads).unwrap();
        assert!(out["story.txt"] == data);
        let asked = allocator.asked();
        assert!(!asked.is_empty(), "{threads} threads lent no run buffer");
        assert!(
            asked.iter().all(|&len| len > in_ram && len <= worker),
            "{threads} threads asked {asked:?}"
        );
        assert_eq!(allocator.drops(), asked.len());
        assert_within_reported(&archive, &limits, threads, &allocator);
        let reported = archive.decoder_memory(&limits, threads).unwrap();
        assert_eq!(
            reported.resident_bytes,
            archive.decoder_memory_estimate().unwrap(),
            "the dictionary is RAM and charged"
        );
    }
}

/// A `max_lent_window_bytes` over the dictionary but under the worker
/// bound, with runs longer than it: the threaded decode is not refused.
/// Those runs decode on one thread into the dictionary; the decode
/// completes to the archive's bytes and nothing over the ceiling is asked.
#[test]
fn runs_longer_than_the_lent_ceiling_decode_on_one_thread() {
    let (bytes, data) = runs_over_the_dictionary();
    let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty()).unwrap();
    let prop = archive.blocks[0].coders[0].properties()[0];
    let dictionary = lzma_turbo::Lzma2Decoder::window_len(prop).unwrap();
    let worker = lzma_turbo::Lzma2Decoder::max_worker_window_len(prop).unwrap();
    let ceiling = 2usize << 20;
    assert!(dictionary <= ceiling && ceiling < worker);
    let allocator = Recorder::new(Mode::Give);
    let mut limits = lending_all(&allocator);
    limits.max_lent_window_bytes = ceiling as u64;
    let out = decode(&bytes, limits.clone(), 4).unwrap();
    assert!(out["story.txt"] == data);
    let asked = allocator.asked();
    assert!(!asked.is_empty());
    assert!(
        asked.iter().all(|&len| len <= ceiling),
        "asked {asked:?} over the ceiling {ceiling}"
    );
    assert_within_reported(&archive, &limits, 4, &allocator);
}

/// A panicking allocator fails an LZMA, an LZMA2 and a threaded LZMA2
/// decode with `WindowAllocatorFailed`, its error naming the panic, and
/// leaves nothing lent.
#[test]
fn a_panicking_allocator_fails_lzma_with_its_error() {
    let data = text(3 << 20, 24);
    let members = [("story.txt", data.as_slice())];
    let mut lzma2 = Lzma2Options::from_level(1);
    lzma2.set_dictionary_size(4 << 20);
    let mut lzma = LzmaOptions::from_level(1);
    lzma.set_dictionary_size(4 << 20);
    let (threaded, _) = multi_run(25);
    for (bytes, threads) in [
        (write(vec![lzma.into()], &members), 1),
        (write(vec![lzma2.into()], &members), 1),
        (threaded, 4),
    ] {
        let allocator = Recorder::new(Mode::Panic);
        let error = decode(&bytes, lending(&allocator, u64::MAX), threads).unwrap_err();
        match &error {
            Error::WindowAllocatorFailed { error, .. } => {
                assert_eq!(error.kind(), io::ErrorKind::Other);
                assert!(error.to_string().contains("scratch map failed"), "{error}");
            }
            other => panic!("not the allocator's error on {threads} threads: {other:?}"),
        }
        assert!(!allocator.asked().is_empty());
        assert_eq!(allocator.out(), (0, 0));
    }
}

#[cfg(feature = "ppmd")]
#[test]
fn a_panicking_allocator_fails_ppmd_with_its_error() {
    let data = text(256 << 10, 26);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(
        vec![PpmdOptions::from_order_memory_size(6, 8 << 20).into()],
        &members,
    );
    let allocator = Recorder::new(Mode::Panic);
    let error = decode(&bytes, lending(&allocator, u64::MAX), 1).unwrap_err();
    assert!(
        matches!(&error, Error::WindowAllocatorFailed { error, .. }
            if error.kind() == io::ErrorKind::Other
                && error.to_string().contains("scratch map failed")),
        "{error:?}"
    );
    assert_eq!(allocator.asked().len(), 1);
    assert_eq!(allocator.out(), (0, 0));
}
