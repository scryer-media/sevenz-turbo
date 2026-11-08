//! Shared scaffolding for the fuzz targets: a container the fuzzer's bytes can
//! be a header inside, and an allocator that says how much a single run asked
//! for.
//!
//! Each target uses only part of this, so what one leaves unused is not dead.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Live bytes and the high-water mark since [`reset_peak`].
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size >= layout.size() {
            let grew = new_size - layout.size();
            let live = LIVE.fetch_add(grew, Ordering::Relaxed) + grew;
            PEAK.fetch_max(live, Ordering::Relaxed);
        } else {
            LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

pub fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

pub fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

const SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Wraps `next_header` in a container whose signature header describes it
/// truthfully, so the fuzzer spends its time on header structure rather than on
/// guessing a CRC.
pub fn wrap(next_header: &[u8]) -> Vec<u8> {
    let mut start = [0u8; 20];
    start[0..8].copy_from_slice(&0u64.to_le_bytes());
    start[8..16].copy_from_slice(&(next_header.len() as u64).to_le_bytes());
    start[16..20].copy_from_slice(&crc32(next_header).to_le_bytes());

    let mut out = Vec::with_capacity(32 + next_header.len());
    out.extend_from_slice(&SIGNATURE);
    out.extend_from_slice(&[0x00, 0x04]);
    out.extend_from_slice(&crc32(&start).to_le_bytes());
    out.extend_from_slice(&start);
    out.extend_from_slice(next_header);
    out
}

/// What a consumer of untrusted archives sets: the structural defaults, plus a
/// budget, because a dictionary is sized by the header too and the default
/// budget is "whatever the caller can afford".
pub fn limits() -> sevenz_turbo::ArchiveLimits {
    sevenz_turbo::ArchiveLimits::memory(64 << 20)
}

/// The ceiling a single run has to stay under. Well above what the limits allow
/// the archive itself (64 MiB of decoder, a header no larger than the input),
/// low enough that a count believed without a bound trips it long before the
/// machine notices.
pub const MAX_RUN_BYTES: usize = 256 << 20;

/// Opens the archive the bytes describe and reads whatever it offers, then says
/// what that cost.
pub fn exercise(bytes: Vec<u8>) {
    use std::io::Read;

    use sevenz_turbo::{ArchiveEntry, ArchiveReader, Password};

    reset_peak();
    if let Ok(mut reader) = ArchiveReader::with_limits(
        std::io::Cursor::new(bytes),
        Password::empty(),
        limits(),
    ) {
        let _ = reader.for_each_entries(&mut |_: &ArchiveEntry, rd: &mut dyn std::io::Read| {
            // Bounded: a run that decodes forever is a hang, and the point of
            // the limits is that it cannot.
            let mut sink = std::io::sink();
            let _ = std::io::copy(&mut rd.take(1 << 24), &mut sink);
            Ok(true)
        });
    }
    let peak = peak();
    assert!(
        peak <= MAX_RUN_BYTES,
        "one archive allocated {peak} bytes, above the {MAX_RUN_BYTES}-byte ceiling"
    );
}

/// [`exercise`] through [`sevenz_turbo::ArchiveReader::for_each_entries_unordered`]
/// over a positional source with several threads, so folders the header
/// describes as independent are decoded side by side.
pub fn exercise_unordered(bytes: Vec<u8>) {
    use sevenz_turbo::{ArchiveReader, Password, UnorderedEntry};

    reset_peak();
    if let Ok(reader) = ArchiveReader::from_read_at(bytes, Password::empty(), limits()) {
        let mut reader = reader.with_threads(4);
        let _ = reader.entry_order();
        // Bounded as `exercise` bounds each entry: a run that hands over bytes
        // forever is a hang, and the point of the limits is that it cannot.
        let mut handed = 0u64;
        let _ = reader.for_each_entries_unordered(|event| {
            if let UnorderedEntry::Data { bytes, .. } = event {
                handed += bytes.len() as u64;
            }
            Ok(handed <= 1 << 24)
        });
    }
    let peak = peak();
    assert!(
        peak <= MAX_RUN_BYTES,
        "one archive allocated {peak} bytes, above the {MAX_RUN_BYTES}-byte ceiling"
    );
}
