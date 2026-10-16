//! Bounded source storage, advised before its first page fault on Linux.

use std::ops::{Deref, DerefMut};

use crate::{EncoderConfiguration, EncoderMethod};

#[cfg(target_os = "linux")]
const HUGE_PAGE: usize = 2 << 20;

/// Keep the extra resident page out of small folders and small-dictionary or
/// non-LZMA coders. This is an allocation policy, never a bound on input length.
fn use_large_buffer(size: Option<u64>, methods: &[EncoderConfiguration]) -> bool {
    size.is_some_and(|size| size >= 8 << 20)
        && methods.iter().any(|method| {
            matches!(
                method.method.id(),
                EncoderMethod::ID_LZMA | EncoderMethod::ID_LZMA2
            ) && method
                .options
                .as_ref()
                .is_none_or(|options| options.get_lzma_dict_size() >= 8 << 20)
        })
}

// The alignment is part of the Box's allocation AND deallocation layout.
// A normally aligned 1 MiB Vec cannot contain an entire 2 MiB huge page.
#[cfg(target_os = "linux")]
#[repr(C, align(2097152))]
pub(super) struct HugeInput([u8; HUGE_PAGE]);

pub(super) enum InputBuffer {
    Ordinary(Vec<u8>),
    #[cfg(target_os = "linux")]
    Huge(Box<HugeInput>),
}

impl InputBuffer {
    pub(super) fn new(size: Option<u64>, methods: &[EncoderConfiguration]) -> Self {
        let large = use_large_buffer(size, methods);
        #[cfg(target_os = "linux")]
        if large {
            return Self::huge_with_advice(advise);
        }
        let _ = large;
        Self::Ordinary(vec![0; super::SOURCE_READ_LEN])
    }

    #[cfg(target_os = "linux")]
    fn huge_with_advice(advice: impl FnOnce(*mut u8, usize)) -> Self {
        let mut buffer = Box::<HugeInput>::new_uninit();
        let data = buffer.as_mut_ptr().cast::<u8>();
        advice(data, HUGE_PAGE);
        // SAFETY: the Box owns an aligned HUGE_PAGE-byte allocation. No
        // reference to HugeInput is formed until every byte is initialized;
        // HugeInput contains only this byte array and has no padding.
        unsafe {
            data.write_bytes(0, HUGE_PAGE);
            Self::Huge(buffer.assume_init())
        }
    }
}

#[cfg(all(target_os = "linux", not(miri)))]
fn advise(data: *mut u8, len: usize) {
    unsafe extern "C" {
        fn madvise(addr: *mut core::ffi::c_void, length: usize, advice: i32) -> i32;
    }
    /// `MADV_HUGEPAGE`, as `<asm-generic/mman-common.h>` defines it.
    const MADV_HUGEPAGE: i32 = 14;
    // SAFETY: huge_with_advice supplies the complete owned, 2 MiB-aligned
    // allocation. MADV_HUGEPAGE changes page policy, never bytes or ownership.
    // The result is deliberately ignored: advice is best effort, and a kernel
    // without transparent huge pages, or one that denies the advice, leaves
    // ordinary pages, which hold the same bytes.
    let _ = unsafe { madvise(data.cast(), len, MADV_HUGEPAGE) };
}

#[cfg(all(target_os = "linux", miri))]
fn advise(_: *mut u8, _: usize) {}

impl Deref for InputBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Ordinary(buffer) => buffer,
            #[cfg(target_os = "linux")]
            Self::Huge(buffer) => &buffer.0,
        }
    }
}

impl DerefMut for InputBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Ordinary(buffer) => buffer,
            #[cfg(target_os = "linux")]
            Self::Huge(buffer) => &mut buffer.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder_options::Lzma2Options;

    #[test]
    fn small_unknown_and_non_lzma_inputs_keep_the_existing_memory_bound() {
        let lzma = [EncoderConfiguration::new(EncoderMethod::LZMA2)];
        for size in [None, Some(0), Some((8 << 20) - 1)] {
            assert!(!use_large_buffer(size, &lzma));
            assert_eq!(
                InputBuffer::new(size, &lzma).len(),
                super::super::SOURCE_READ_LEN
            );
        }
        let copy = [EncoderConfiguration::new(EncoderMethod::COPY)];
        assert!(!use_large_buffer(Some(u64::MAX), &copy));
        let mut options = Lzma2Options::from_level(1);
        options.set_dictionary_size(1 << 20);
        let small = [EncoderConfiguration::new(EncoderMethod::LZMA2).with_options(options.into())];
        assert!(!use_large_buffer(Some(u64::MAX), &small));
        assert!(use_large_buffer(Some(8 << 20), &lzma));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn advice_precedes_initialization_and_refusal_leaves_usable_owned_storage() {
        let mut called = false;
        let mut buffer = InputBuffer::huge_with_advice(|data, len| {
            assert_eq!(data.addr() % HUGE_PAGE, 0);
            assert_eq!(len, HUGE_PAGE);
            called = true;
            // Simulate ignored/unsupported advice without changing the bytes.
        });
        assert!(called);
        assert!(buffer.iter().all(|&byte| byte == 0));
        buffer[0] = 1;
        buffer[HUGE_PAGE - 1] = 2;
        assert_eq!((buffer[0], buffer[HUGE_PAGE - 1]), (1, 2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn panicking_advice_releases_the_uninitialized_allocation() {
        // In its own Miri process this also checks that unwinding drops the
        // Box with its original aligned allocation layout, without a leak.
        let result = std::panic::catch_unwind(|| {
            InputBuffer::huge_with_advice(|_, _| panic!("advice interrupted"))
        });
        assert!(result.is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn aligned_storage_keeps_ownership_when_moved_between_threads() {
        let mut first = InputBuffer::huge_with_advice(|_, _| {});
        let mut second = InputBuffer::huge_with_advice(|_, _| {});
        assert_ne!(first.as_ptr(), second.as_ptr());
        first[0] = 7;
        second[HUGE_PAGE - 1] = 11;
        let first = std::thread::spawn(move || {
            assert_eq!((first[0], first[HUGE_PAGE - 1]), (7, 0));
            first[HUGE_PAGE - 1] = 13;
            first
        })
        .join()
        .unwrap();
        assert_eq!((first[0], first[HUGE_PAGE - 1]), (7, 13));
        assert_eq!((second[0], second[HUGE_PAGE - 1]), (0, 11));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_advice_preserves_the_initialized_buffer() {
        let methods = [EncoderConfiguration::new(EncoderMethod::LZMA2)];
        let buffer = InputBuffer::new(Some(8 << 20), &methods);
        assert_eq!(buffer.len(), HUGE_PAGE);
        assert_eq!(buffer.as_ptr().addr() % HUGE_PAGE, 0);
        assert!(buffer.iter().all(|&byte| byte == 0));
    }
}
