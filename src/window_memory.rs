//! Caller memory for large decode windows.
//!
//! An LZMA or LZMA2 coder decodes into a dictionary, and a PPMd coder into a
//! model arena, each as large as the archive declares: up to 4 GiB. A caller
//! that will not hold that much in RAM can lend the memory instead, for
//! example a mapped scratch file, through a [`WindowAllocator`] set on
//! [`ArchiveLimits::window_allocator`]. A window over
//! [`ArchiveLimits::max_in_ram_window_bytes`] is then asked of the allocator
//! and does not count against [`ArchiveLimits::memory_limit_bytes`].
//!
//! The two traits are this crate's own, one contract for both codecs. The
//! private adapters below turn them into `lzma-turbo`'s `WindowAllocator` and
//! `ppmd-turbo`'s `ArenaAllocator`, so a caller writes one allocator and the
//! codec crates' types never reach this crate's public API.
//!
//! [`ArchiveLimits::window_allocator`]: crate::ArchiveLimits::window_allocator
//! [`ArchiveLimits::max_in_ram_window_bytes`]: crate::ArchiveLimits::max_in_ram_window_bytes
//! [`ArchiveLimits::memory_limit_bytes`]: crate::ArchiveLimits::memory_limit_bytes

use std::io;
use std::sync::{Arc, Mutex};

/// Memory a [`WindowAllocator`] lends for one decode window.
///
/// # Safety
///
/// An implementation must guarantee all of the following, from the call to
/// [`WindowMemory::as_mut_ptr`] until the value is dropped:
///
/// - the pointer is non-null and valid for reads and writes of `len()`
///   bytes, which is at least the `len` [`WindowAllocator::allocate`] was
///   asked for; the decoder uses exactly the first `len` asked for;
/// - those bytes are initialized, to any value: they need not be zero;
/// - the address is stable until drop, including when the `Box` holding the
///   value moves: the memory is not moved, unmapped, remapped or freed;
/// - the memory is exclusive to the decoder until drop: nothing else reads or
///   writes it, as with a `&mut [u8]`.
///
/// Dropping the value releases the memory; the decoder does not touch it
/// afterwards. No decoded byte depends on what the memory held before, so its
/// starting contents do not matter.
pub unsafe trait WindowMemory: Send {
    /// The start of the memory. Called once.
    fn as_mut_ptr(&mut self) -> *mut u8;

    /// How many bytes [`WindowMemory::as_mut_ptr`] points at: at least the
    /// `len` asked for. Called once.
    fn len(&self) -> usize;

    /// Whether [`WindowMemory::len`] is zero.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A source of caller memory for LZMA and LZMA2 dictionaries and PPMd model
/// arenas.
///
/// Asked for each window over [`ArchiveLimits::max_in_ram_window_bytes`]
/// and at or under [`ArchiveLimits::max_lent_window_bytes`]. A window over
/// the ceiling is refused with [`Error::LimitExceeded`] and the allocator is
/// never asked for it.
///
/// # Concurrency
///
/// [`WindowAllocator::allocate`] can be called from several threads at once.
/// A coder decoded on one thread holds one window, asked as it is built. A
/// threaded LZMA2 coder whose dictionary is lent asks again as its workers
/// take runs: one window per worker and one for the decoder that follows
/// the stream, each kept for the worker's next run while it fits. A
/// positional LZMA2 run asks for its ring as it starts, and drops it when it
/// ends. A reader decoding folders in parallel builds one chain per folder in
/// flight. [`Archive::decoder_memory`] gives the most windows, and bytes,
/// out at once for a thread count. A caller that wants one window at a time
/// decodes on one thread.
///
/// [`Archive::decoder_memory`]: crate::Archive::decoder_memory
/// [`ArchiveLimits::max_in_ram_window_bytes`]: crate::ArchiveLimits::max_in_ram_window_bytes
/// [`ArchiveLimits::max_lent_window_bytes`]: crate::ArchiveLimits::max_lent_window_bytes
/// [`Error::LimitExceeded`]: crate::Error::LimitExceeded
pub trait WindowAllocator: Send + Sync {
    /// Returns `len` bytes for a window, or the error that stopped it. The
    /// error reaches the caller as [`Error::WindowAllocatorFailed`], kept
    /// whole.
    ///
    /// # Errors
    ///
    /// Whatever stops the allocator: no disk space, a failed map, a quota.
    ///
    /// [`Error::WindowAllocatorFailed`]: crate::Error::WindowAllocatorFailed
    fn allocate(&self, len: usize) -> io::Result<Box<dyn WindowMemory>>;
}

/// The caller's allocator for one coder, with a slot for the reason it
/// failed.
///
/// The codec crates report an allocator's failure in their own shapes:
/// `lzma-turbo` hands the error back whole, inside the error it fails with
/// or from the threaded decoder's `take_window_error`, so the LZMA adapter
/// passes it on as it is. `ppmd-turbo` keeps only its kind, so the PPMd
/// adapter keeps the caller's own error here instead, and the coder's
/// builder takes it back out ([`Lender::take_refusal`]) to report it whole.
pub(crate) struct Lender {
    allocator: Arc<dyn WindowAllocator>,
    refusal: Mutex<Option<io::Error>>,
}

impl Lender {
    pub(crate) fn new(allocator: Arc<dyn WindowAllocator>) -> Arc<Self> {
        Arc::new(Self {
            allocator,
            refusal: Mutex::new(None),
        })
    }

    /// Why the allocator could not supply a window, if it could not.
    pub(crate) fn take_refusal(&self) -> Option<io::Error> {
        self.refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Keeps `error` for the builder, and hands the codec one of its kind.
    #[cfg(feature = "ppmd")]
    fn refuse(&self, error: io::Error) -> io::Error {
        let reply = io::Error::new(error.kind(), "window allocator refused");
        *self
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        reply
    }

    /// Asks the caller's allocator for `len` bytes and checks what it gave
    /// back against the part of the [`WindowMemory`] contract that can be
    /// checked. Memory that fails the check is dropped before the error
    /// returns. The error is the allocator's own, or an `InvalidInput` one
    /// for memory that failed the check.
    fn lend(&self, len: usize) -> io::Result<LentWindow> {
        let mut memory = self.allocator.allocate(len)?;
        let ptr = memory.as_mut_ptr();
        let got = memory.len();
        // A length over `isize::MAX` cannot describe real memory.
        if ptr.is_null() || got < len || isize::try_from(got).is_err() {
            drop(memory);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("window allocator returned {got} bytes at {ptr:p} for a {len}-byte window"),
            ));
        }
        Ok(LentWindow {
            ptr,
            len: got,
            _owner: memory,
        })
    }
}

/// A window the caller lent: the pointer and length read once from the
/// [`WindowMemory`], which is kept only to be dropped.
struct LentWindow {
    ptr: *mut u8,
    len: usize,
    _owner: Box<dyn WindowMemory>,
}

// SAFETY: `ptr` points into memory the `WindowMemory` contract makes
// exclusive to `_owner`, which is `Send`; moving both moves the only access
// to that memory with them.
unsafe impl Send for LentWindow {}

// SAFETY: every requirement the codec's trait states is one the caller's
// `WindowMemory` implementation guarantees, and `Lender::lend` checked the
// ones it can (non-null, at least the length asked, at most `isize::MAX`).
// The pointer and length were read once, as the caller's contract allows,
// and are handed out unchanged; `_owner` keeps the memory alive and in place
// until this value is dropped.
unsafe impl lzma_turbo::WindowMemory for LentWindow {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl lzma_turbo::WindowAllocator for Lender {
    fn allocate(&self, len: usize) -> io::Result<Box<dyn lzma_turbo::WindowMemory>> {
        Ok(Box::new(self.lend(len)?))
    }
}

// SAFETY: as for `lzma_turbo::WindowMemory` above; `ppmd-turbo`'s contract
// asks the same four things of the memory.
#[cfg(feature = "ppmd")]
unsafe impl ppmd_turbo::ArenaMemory for LentWindow {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(feature = "ppmd")]
impl ppmd_turbo::ArenaAllocator for Lender {
    fn allocate(
        &self,
        len: usize,
    ) -> Result<Box<dyn ppmd_turbo::ArenaMemory>, ppmd_turbo::ArenaAllocError> {
        Ok(Box::new(self.lend(len).map_err(|e| self.refuse(e))?))
    }
}

/// A heap allocator for this crate's own tests of a lent decode.
#[cfg(all(test, feature = "compress"))]
pub(crate) struct TestHeap {
    /// How many windows it has lent.
    pub(crate) asked: std::sync::atomic::AtomicUsize,
}

#[cfg(all(test, feature = "compress"))]
impl WindowAllocator for TestHeap {
    fn allocate(&self, len: usize) -> io::Result<Box<dyn WindowMemory>> {
        struct Memory(Box<[u8]>);
        // SAFETY: the boxed slice is owned and initialized, does not move
        // while the box lives, and is reached only through this value.
        unsafe impl WindowMemory for Memory {
            fn as_mut_ptr(&mut self) -> *mut u8 {
                self.0.as_mut_ptr()
            }
            fn len(&self) -> usize {
                self.0.len()
            }
        }
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(Memory(vec![0x5A; len].into_boxed_slice())))
    }
}

#[cfg(test)]
mod tests {
    //! The adapter alone, without a codec: small and portable enough for
    //! Miri, which checks the pointer the codec traits hand out against the
    //! allocation the caller's memory owns, and that it is freed once.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Heap memory filled with a pattern, counting its drops.
    struct Heap {
        bytes: Box<[u8]>,
        extra_len: usize,
        null: bool,
        drops: Arc<AtomicUsize>,
    }

    // SAFETY: the boxed slice is owned, initialized, never moved while the
    // box lives, and reached only through this value. `extra_len` is zero
    // except in a test that the adapter must refuse before any use.
    unsafe impl WindowMemory for Heap {
        fn as_mut_ptr(&mut self) -> *mut u8 {
            if self.null {
                std::ptr::null_mut()
            } else {
                self.bytes.as_mut_ptr()
            }
        }

        fn len(&self) -> usize {
            self.bytes.len() + self.extra_len
        }
    }

    impl Drop for Heap {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Hands out heap memory, `short` bytes too few when asked to, or a null
    /// pointer, or refuses.
    struct Alloc {
        short: usize,
        null: bool,
        refuse: Option<io::ErrorKind>,
        drops: Arc<AtomicUsize>,
        asked: AtomicUsize,
    }

    impl Alloc {
        fn new() -> Self {
            Self {
                short: 0,
                null: false,
                refuse: None,
                drops: Arc::new(AtomicUsize::new(0)),
                asked: AtomicUsize::new(0),
            }
        }
    }

    impl WindowAllocator for Alloc {
        fn allocate(&self, len: usize) -> io::Result<Box<dyn WindowMemory>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            if let Some(kind) = self.refuse {
                return Err(io::Error::new(kind, "quota"));
            }
            Ok(Box::new(Heap {
                bytes: vec![0xA5; len - self.short].into_boxed_slice(),
                extra_len: 0,
                null: self.null,
                drops: Arc::clone(&self.drops),
            }))
        }
    }

    /// Writes through the pointer the codec trait hands out, reads it back,
    /// moves the window to another thread, and drops it once.
    #[test]
    fn a_lent_window_is_the_callers_memory_and_is_freed_once() {
        let alloc = Arc::new(Alloc::new());
        let drops = Arc::clone(&alloc.drops);
        let lender = Lender::new(alloc);
        let mut window = lzma_turbo::WindowAllocator::allocate(&*lender, 64).unwrap();
        assert_eq!(lzma_turbo::WindowMemory::len(&*window), 64);
        let ptr = lzma_turbo::WindowMemory::as_mut_ptr(&mut *window);
        // SAFETY: the adapter checked `ptr` is non-null with 64 bytes behind
        // it, which the `Heap` owns and nothing else touches.
        let bytes = unsafe { std::slice::from_raw_parts_mut(ptr, 64) };
        assert!(bytes.iter().all(|&b| b == 0xA5));
        bytes.fill(7);
        let window = std::thread::spawn(move || {
            let mut window = window;
            let ptr = lzma_turbo::WindowMemory::as_mut_ptr(&mut *window);
            // SAFETY: as above; the window moved here with its owner.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, 64) };
            assert!(bytes.iter().all(|&b| b == 7));
            window
        })
        .join()
        .unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(window);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(lender.take_refusal().is_none());
    }

    /// The allocator's own error goes to `lzma-turbo` as it is, which hands
    /// it back whole; nothing is kept for the builder.
    #[test]
    fn a_refusal_goes_to_lzma_turbo_whole() {
        let mut alloc = Alloc::new();
        alloc.refuse = Some(io::ErrorKind::StorageFull);
        let lender = Lender::new(Arc::new(alloc));
        let error = lzma_turbo::WindowAllocator::allocate(&*lender, 64)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(error.to_string(), "quota");
        assert!(lender.take_refusal().is_none());
    }

    /// For PPMd, whose codec keeps only the kind, the allocator's own error
    /// is kept whole for the coder's builder.
    #[cfg(feature = "ppmd")]
    #[test]
    fn a_ppmd_refusal_is_kept_for_the_builder() {
        let mut alloc = Alloc::new();
        alloc.refuse = Some(io::ErrorKind::StorageFull);
        let lender = Lender::new(Arc::new(alloc));
        assert!(ppmd_turbo::ArenaAllocator::allocate(&*lender, 64).is_err());
        let kept = lender.take_refusal().unwrap();
        assert_eq!(kept.kind(), io::ErrorKind::StorageFull);
        assert_eq!(kept.to_string(), "quota");
        assert!(lender.take_refusal().is_none());
    }

    /// Memory shorter than asked, a null pointer, or a length past
    /// `isize::MAX` is refused before the codec sees it, and dropped.
    #[test]
    fn memory_that_does_not_fit_is_refused_and_dropped() {
        for short in [true, false] {
            let mut alloc = Alloc::new();
            if short {
                alloc.short = 1;
            } else {
                alloc.null = true;
            }
            let drops = Arc::clone(&alloc.drops);
            let lender = Lender::new(Arc::new(alloc));
            let error = lzma_turbo::WindowAllocator::allocate(&*lender, 64)
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(lender.take_refusal().is_none());
        }

        struct Huge(Arc<AtomicUsize>);
        impl WindowAllocator for Huge {
            fn allocate(&self, _: usize) -> io::Result<Box<dyn WindowMemory>> {
                Ok(Box::new(Heap {
                    bytes: vec![0; 8].into_boxed_slice(),
                    extra_len: usize::MAX - 8,
                    null: false,
                    drops: Arc::clone(&self.0),
                }))
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let lender = Lender::new(Arc::new(Huge(Arc::clone(&drops))));
        let error = lzma_turbo::WindowAllocator::allocate(&*lender, 8)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// The PPMd adapter hands out the same memory, under the same checks.
    #[cfg(feature = "ppmd")]
    #[test]
    fn a_lent_arena_is_the_callers_memory_and_is_freed_once() {
        let alloc = Arc::new(Alloc::new());
        let drops = Arc::clone(&alloc.drops);
        let lender = Lender::new(alloc);
        let mut arena = ppmd_turbo::ArenaAllocator::allocate(&*lender, 32).unwrap();
        assert_eq!(ppmd_turbo::ArenaMemory::len(&*arena), 32);
        let ptr = ppmd_turbo::ArenaMemory::as_mut_ptr(&mut *arena);
        // SAFETY: the adapter checked `ptr` is non-null with 32 bytes behind
        // it, owned by the `Heap` inside `arena`.
        unsafe { std::ptr::write_bytes(ptr, 3, 32) };
        // SAFETY: as above.
        assert!(
            unsafe { std::slice::from_raw_parts(ptr, 32) }
                .iter()
                .all(|&b| b == 3)
        );
        drop(arena);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        let mut alloc = Alloc::new();
        alloc.short = 1;
        let lender = Lender::new(Arc::new(alloc));
        assert!(ppmd_turbo::ArenaAllocator::allocate(&*lender, 32).is_err());
        assert_eq!(
            lender.take_refusal().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
