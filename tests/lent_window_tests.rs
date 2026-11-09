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

/// Heap memory filled with a pattern that is not zero, counting its drops.
struct Heap {
    bytes: Box<[u8]>,
    drops: Arc<AtomicUsize>,
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
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Give,
    Refuse(io::ErrorKind),
    Short,
}

/// Lends heap memory, records what it was asked for, and counts the memory
/// it handed out that has been dropped.
struct Recorder {
    mode: Mode,
    asked: Mutex<Vec<usize>>,
    drops: Arc<AtomicUsize>,
}

impl Recorder {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            asked: Mutex::new(Vec::new()),
            drops: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn asked(&self) -> Vec<usize> {
        self.asked.lock().unwrap().clone()
    }

    fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }
}

impl WindowAllocator for Recorder {
    fn allocate(&self, len: usize) -> io::Result<Box<dyn WindowMemory>> {
        self.asked.lock().unwrap().push(len);
        let len = match self.mode {
            Mode::Give => len,
            Mode::Short => len - 1,
            Mode::Refuse(kind) => return Err(io::Error::new(kind, "scratch disk full")),
        };
        Ok(Box::new(Heap {
            bytes: vec![0xA5; len].into_boxed_slice(),
            drops: Arc::clone(&self.drops),
        }))
    }
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
/// with one decodes to `members` on lent memory, every window lent once and
/// dropped by the end.
fn assert_decodes_on_lent_memory(bytes: &[u8], members: &[(&str, &[u8])], threads: u32) {
    let limit = tight_limit(bytes);
    let refused = decode(bytes, ArchiveLimits::memory(limit), threads).unwrap_err();
    assert_eq!(refused.limit_hit(), Some(Limit::MemoryBytes), "{refused}");

    let archive = Archive::read(&mut Cursor::new(bytes), &Password::empty()).unwrap();
    let allocator = Recorder::new(Mode::Give);
    let out = decode(bytes, lending(&allocator, limit), threads).unwrap();
    assert_eq!(out.len(), members.len());
    for (name, data) in members {
        assert!(out[*name] == *data, "{name} decoded to other bytes");
    }
    let asked = allocator.asked();
    assert!(!asked.is_empty());
    assert!(asked.len() <= windowed_coders(&archive));
    assert!(asked.iter().all(|&len| len > 1 << 20));
    assert_eq!(
        allocator.drops(),
        asked.len(),
        "a lent window was not released"
    );
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

/// A threaded LZMA2 decode does not lend its dictionaries yet, and the
/// thread count is not lowered so that one could: the block four threads
/// would split is refused with the memory-limit error, the allocator
/// unasked. On one thread the same block decodes on lent memory.
#[test]
fn threaded_lzma2_refuses_a_lent_dictionary() {
    let data = text(6 << 20, 6);
    let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
    options.set_dictionary_size(2 << 20);
    let members = [("story.txt", data.as_slice())];
    let bytes = write(vec![options.into()], &members);

    let allocator = Recorder::new(Mode::Give);
    let refused = decode(&bytes, lending(&allocator, u64::MAX), 4).unwrap_err();
    assert!(
        matches!(&refused, Error::BlockDecode { cause, .. } if matches!(**cause, Error::MaxMemLimited { .. })),
        "{refused:?}"
    );
    assert!(allocator.asked().is_empty());

    let out = decode(&bytes, lending(&allocator, u64::MAX), 1).unwrap();
    assert!(out["story.txt"] == data, "decoded to other bytes");
    assert_eq!(allocator.asked().len(), 1);
    assert_eq!(allocator.drops(), 1, "the lent window was not released");
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
