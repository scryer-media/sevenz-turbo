//! Native buffer-edge checks, including the 2 MiB alignment of HugeInput.
//! Miri cannot execute assembly or madvise; this allocator guards the native
//! source, codec and output allocations while exercising the archive APIs:
//! LZMA2 written through every push path and decoded on one thread, LZMA2
//! decoded in parallel at two threads, BCJ2, and, when their features are
//! built, AES-256 and PPMd. CI builds it with
//! `--features compress,aes256,aws-lc-crypto,ppmd`.

#![cfg(all(
    feature = "compress",
    not(miri),
    target_pointer_width = "64",
    any(unix, windows)
))]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    io::{self, Cursor, Read},
    sync::atomic::{AtomicBool, Ordering},
};

use sevenz_turbo::{
    ArchiveEntry, ArchiveLimits, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod,
    Password, SourceReader, encoder_options::Lzma2Options, prepare_block,
};

// Unlike the codec's 64 KiB guard unit, this also guards sevenz's aligned Box.
const UNIT: usize = 2 << 20;
static FRONT: AtomicBool = AtomicBool::new(false);
struct Guarded;
#[global_allocator]
static ALLOCATOR: Guarded = Guarded;

// SAFETY: map returns an owned aligned mapping with inaccessible pages on both
// sides. Returned pointers retain the requested alignment and layout size;
// dealloc reconstructs the same mapping. Larger alignments delegate to System.
unsafe impl GlobalAlloc for Guarded {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > UNIT {
            // SAFETY: the layout is forwarded unchanged, paired in dealloc.
            return unsafe { System.alloc(layout) };
        }
        let Some(data) = layout
            .size()
            .max(1)
            .checked_add(UNIT - 1)
            .map(|n| n / UNIT * UNIT)
        else {
            return std::ptr::null_mut();
        };
        let Some(base) = os::map(data) else {
            return std::ptr::null_mut();
        };
        let start = base as usize + UNIT;
        let address = if FRONT.load(Ordering::Relaxed) {
            start
        } else {
            (start + data - layout.size()) & !(layout.align() - 1)
        };
        address as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.align() > UNIT {
            // SAFETY: this branch pairs with the unchanged System allocation.
            unsafe { System.dealloc(ptr, layout) };
            return;
        }
        let data = layout.size().max(1).div_ceil(UNIT) * UNIT;
        let base = (ptr as usize & !(UNIT - 1)) - UNIT;
        os::unmap(base as *mut u8, data + 2 * UNIT);
    }
}

#[cfg(unix)]
mod os {
    use super::UNIT;
    use std::ffi::{c_int, c_void};
    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }
    pub fn map(data: usize) -> Option<*mut u8> {
        let total = data.checked_add(2 * UNIT)?;
        let span = total.checked_add(UNIT)?;
        let anonymous = if cfg!(target_vendor = "apple") {
            0x1000
        } else {
            0x20
        };
        // SAFETY: reserve a fresh anonymous mapping, trim only its ends, and
        // make only its data region accessible. Every failure releases it.
        unsafe {
            let raw = mmap(std::ptr::null_mut(), span, 0, 2 | anonymous, -1, 0);
            if raw as isize == -1 {
                return None;
            }
            let raw = raw.cast::<u8>();
            let head = (raw as usize).next_multiple_of(UNIT) - raw as usize;
            let base = raw.add(head);
            if head != 0 {
                munmap(raw.cast(), head);
            }
            let tail = span - head - total;
            if tail != 0 {
                munmap(base.add(total).cast(), tail);
            }
            if mprotect(base.add(UNIT).cast(), data, 3) != 0 {
                munmap(base.cast(), total);
                return None;
            }
            Some(base)
        }
    }
    pub fn unmap(base: *mut u8, total: usize) {
        // SAFETY: GlobalAlloc::dealloc reconstructs the live mapping's extent.
        unsafe { munmap(base.cast(), total) };
    }
}

#[cfg(windows)]
mod os {
    use super::UNIT;
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
    }
    pub fn map(data: usize) -> Option<*mut u8> {
        let span = data.checked_add(3 * UNIT)?;
        // SAFETY: reserve an owned address range and commit only its data
        // pages. VirtualQuery recovers the reservation base at deallocation.
        unsafe {
            let raw = VirtualAlloc(std::ptr::null_mut(), span, 0x2000, 1);
            if raw.is_null() {
                return None;
            }
            let base = (raw as usize).next_multiple_of(UNIT) as *mut u8;
            let accessible = base.add(UNIT);
            if VirtualAlloc(accessible.cast(), data, 0x1000, 4).is_null() {
                VirtualFree(raw, 0, 0x8000);
                return None;
            }
            // Windows reservations are only 64 KiB aligned. Recovering the
            // reservation from an arbitrary aligned interior pointer requires
            // VirtualQuery instead of a metadata pointer in a guarded page.
            Some(base)
        }
    }
    #[repr(C)]
    struct MemoryInfo {
        base: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        kind: u32,
    }
    unsafe extern "system" {
        fn VirtualQuery(addr: *const c_void, info: *mut MemoryInfo, len: usize) -> usize;
    }
    pub fn unmap(base: *mut u8, _: usize) {
        let mut info = std::mem::MaybeUninit::<MemoryInfo>::uninit();
        // SAFETY: VirtualQuery writes the declared output structure, and its
        // allocation_base is the original live VirtualAlloc reservation.
        unsafe {
            if VirtualQuery(
                base.cast(),
                info.as_mut_ptr(),
                std::mem::size_of::<MemoryInfo>(),
            ) == std::mem::size_of::<MemoryInfo>()
            {
                VirtualFree(info.assume_init().allocation_base, 0, 0x8000);
            }
        }
    }
}

#[test]
fn guard_pages_really_fault_at_both_edges() {
    const CHILD: &str = "SEVENZ_TURBO_GUARD_CHILD";
    if let Ok(side) = std::env::var(CHILD) {
        FRONT.store(side.starts_with("front"), Ordering::Relaxed);
        let len = if side.ends_with("aligned") {
            UNIT
        } else {
            1001
        };
        let alignment = if side.ends_with("aligned") { UNIT } else { 1 };
        let layout = Layout::from_size_align(len, alignment).unwrap();
        // SAFETY: layout is valid and nonzero. This child terminates at the
        // deliberately invalid read below; it does not transfer ownership.
        let allocation = unsafe { std::alloc::alloc(layout) };
        assert!(!allocation.is_null());
        assert_eq!(allocation.addr() % alignment, 0);
        let ptr = if side.starts_with("front") {
            allocation.wrapping_sub(1)
        } else {
            allocation.wrapping_add(len)
        };
        // SAFETY: deliberately invalid only in the fault-observing child.
        std::hint::black_box(unsafe { ptr.read_volatile() });
        return;
    }
    for side in ["front", "back", "front-aligned", "back-aligned"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "guard_pages_really_fault_at_both_edges",
                "--nocapture",
            ])
            .env(CHILD, side)
            .output()
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert!(
                matches!(output.status.signal(), Some(7 | 10 | 11)),
                "guard did not fault: {:?}",
                output.status
            );
        }
        #[cfg(windows)]
        assert_eq!(output.status.code(), Some(0xc0000005u32 as i32));
    }
}

struct ShortSource<'a> {
    data: &'a [u8],
    fail: Option<usize>,
}
impl Read for ShortSource<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.fail == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "source refused",
            ));
        }
        let len = out
            .len()
            .min(self.data.len())
            .min(7777)
            .min(self.fail.unwrap_or(usize::MAX));
        out[..len].copy_from_slice(&self.data[..len]);
        self.data = &self.data[len..];
        if let Some(fail) = &mut self.fail {
            *fail -= len;
        }
        Ok(len)
    }
}

fn archive(data: &[u8], path: usize, fail: bool) -> Result<Vec<u8>, sevenz_turbo::Error> {
    let mut options = Lzma2Options::from_level_mt(1, 2, 4 << 20);
    options.set_dictionary_size(8 << 20);
    let methods: Vec<EncoderConfiguration> = vec![options.into()];
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new()))?;
    writer.set_content_methods(methods.clone());
    let mut entry = ArchiveEntry::new_file("guarded");
    entry.size = data.len() as u64;
    let source = ShortSource {
        data,
        fail: fail.then_some((2 << 20) + 31),
    };
    match path {
        0 => {
            writer.push_archive_entry(entry, Some(source))?;
        }
        1 => {
            writer.push_archive_entry_send(entry, Some(source))?;
        }
        2 => {
            writer.push_prepared_block(prepare_block(
                methods.into(),
                vec![entry],
                vec![SourceReader::new(source)],
            )?)?;
        }
        _ => unreachable!(),
    }
    Ok(writer.finish()?.into_inner())
}

#[test]
#[ignore = "native memory-safety CI runs the aligned source paths with guards"]
fn guarded_aligned_sources_survive_short_reads_errors_and_ownership_transfer() {
    let data: Vec<_> = (0..(8 << 20) + 37).map(|i| (i % 251) as u8).collect();
    for front in [false, true] {
        FRONT.store(front, Ordering::Relaxed);
        for path in 0..3 {
            let bytes = archive(&data, path, false).unwrap();
            let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
            let mut seen = 0;
            reader
                .for_each_entries(|_, input| {
                    let mut actual = Vec::new();
                    input.read_to_end(&mut actual)?;
                    assert_eq!(actual, data);
                    seen += 1;
                    Ok(true)
                })
                .unwrap();
            assert_eq!(seen, 1);
            assert!(
                archive(&data, path, true)
                    .unwrap_err()
                    .to_string()
                    .contains("source refused")
            );
        }
    }
}

/// Compressible bytes shaped like x86 code, so that BCJ2 has calls and
/// jumps to split out and every other coder has something to model.
fn code_like(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend(std::iter::repeat_n(
            b'a' + (x >> 40) as u8 % 26,
            1 + (x % 7) as usize,
        ));
        out.push((x >> 8) as u8);
    }
    out.truncate(len);
    let mut i = (seed % 7) as usize;
    while i + 5 <= out.len() {
        out[i] = if i.is_multiple_of(3) { 0xE9 } else { 0xE8 };
        out[i + 1..i + 5].copy_from_slice(&((i as u32 * 3) % 0x4000).to_le_bytes());
        i += 11;
    }
    out
}

/// LZMA2 in many independent runs: written by two threads in 256 KiB
/// chunks over a 64 KiB dictionary.
fn multi_run() -> EncoderConfiguration {
    let mut options = Lzma2Options::from_level_mt(1, 2, 256 << 10);
    options.set_dictionary_size(64 << 10);
    options.into()
}

/// Members with an empty one between streamed ones, written solid.
fn members() -> Vec<(String, Vec<u8>)> {
    [(700 << 10) + 3, 0, (1 << 20) + 11, 1]
        .into_iter()
        .enumerate()
        .map(|(i, len)| (format!("guarded-{i}"), code_like(len, i as u64)))
        .collect()
}

fn solid(methods: Vec<EncoderConfiguration>, members: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(methods);
    let entries = members
        .iter()
        .map(|(name, data)| {
            let mut entry = ArchiveEntry::new_file(name);
            entry.size = data.len() as u64;
            entry
        })
        .collect();
    let readers = members
        .iter()
        .map(|(_, data)| SourceReader::new(ShortSource { data, fail: None }))
        .collect();
    writer.push_archive_entries(entries, readers).unwrap();
    writer.finish().unwrap().into_inner()
}

/// Decodes `bytes` through a seekable cursor and through a positional source
/// at each of `threads`, with allocations against each edge, and compares.
fn decodes_to(bytes: &[u8], password: &str, members: &[(String, Vec<u8>)], threads: &[u32]) {
    for front in [false, true] {
        FRONT.store(front, Ordering::Relaxed);
        for &threads in threads {
            let password = Password::from(password);
            let cursor = ArchiveReader::new(Cursor::new(bytes), password.clone())
                .unwrap()
                .with_threads(threads);
            let positional =
                ArchiveReader::from_read_at(bytes.to_vec(), password, ArchiveLimits::default())
                    .unwrap()
                    .with_threads(threads);
            for decoded in [entries_of(cursor), entries_of(positional)] {
                assert!(decoded == members, "front {front}, {threads} threads");
            }
        }
    }
}

fn entries_of<R: Read + io::Seek>(mut reader: ArchiveReader<R>) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    reader
        .for_each_entries(|entry, input| {
            let mut data = Vec::new();
            input.read_to_end(&mut data)?;
            out.push((entry.name().to_owned(), data));
            Ok(true)
        })
        .unwrap();
    out
}

#[test]
#[ignore = "native memory-safety CI runs the parallel decode with guards"]
fn guarded_parallel_lzma2_decode_at_two_threads() {
    let members = members();
    let bytes = solid(vec![multi_run()], &members);
    decodes_to(&bytes, "", &members, &[1, 2]);
}

#[test]
#[ignore = "native memory-safety CI runs the BCJ2 chain with guards"]
fn guarded_bcj2_chain_encode_and_decode() {
    let members = members();
    let bytes = solid(
        vec![multi_run(), EncoderMethod::BCJ2_FILTER.into()],
        &members,
    );
    decodes_to(&bytes, "", &members, &[1, 2]);
    // An archive 7-Zip wrote, whose BCJ2 side streams are LZMA.
    let committed = std::fs::read("tests/resources/7za433_7zip_lzma2_bcj2.7z").unwrap();
    let mut expected = None;
    for front in [false, true] {
        FRONT.store(front, Ordering::Relaxed);
        for threads in [1, 2] {
            let reader = ArchiveReader::new(Cursor::new(&committed), Password::empty())
                .unwrap()
                .with_threads(threads);
            let decoded = entries_of(reader);
            assert!(&decoded == expected.get_or_insert_with(|| decoded.clone()));
        }
    }
}

#[cfg(feature = "aes256")]
#[test]
#[ignore = "native memory-safety CI runs AES-256 with guards"]
fn guarded_aes256_encode_and_decode() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;
    let members = members();
    // A low work factor: the key derivation allocates nothing, and the
    // default 2^19 rounds would only make the lane slower.
    let aes = AesEncoderOptions::new(Password::from("guarded")).with_num_cycles_power(10);
    let bytes = solid(vec![aes.into(), multi_run()], &members);
    decodes_to(&bytes, "guarded", &members, &[1, 2]);
    // A wrong password is refused, at the encrypted header or at the first
    // block, without touching memory it does not own.
    for threads in [1, 2] {
        let refused =
            ArchiveReader::new(Cursor::new(&bytes), Password::from("not it")).and_then(|reader| {
                reader
                    .with_threads(threads)
                    .for_each_entries(|_, input| {
                        io::copy(input, &mut io::sink())?;
                        Ok(true)
                    })
                    .map(drop)
            });
        assert!(refused.is_err(), "{threads} threads");
    }
}

#[cfg(feature = "ppmd")]
#[test]
#[ignore = "native memory-safety CI runs PPMd with guards"]
fn guarded_ppmd_encode_and_decode() {
    use sevenz_turbo::encoder_options::PpmdOptions;
    let members = members();
    let bytes = solid(
        vec![PpmdOptions::from_order_memory_size(6, 4 << 20).into()],
        &members,
    );
    decodes_to(&bytes, "", &members, &[1, 2]);
}
