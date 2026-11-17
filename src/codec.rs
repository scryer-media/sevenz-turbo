#[cfg(feature = "brotli")]
pub mod brotli;
pub(crate) mod filter;
#[cfg(feature = "lz4")]
pub mod lz4;
pub(crate) mod lzma_turbo;

#[cfg(all(feature = "compress", any(feature = "brotli", feature = "lz4")))]
fn packed_frame_size(size: usize) -> std::io::Result<u32> {
    u32::try_from(size)
        .map_err(|_| std::io::Error::other("compressed frame exceeds its 32-bit size field"))
}

#[cfg(all(test, feature = "compress", any(feature = "brotli", feature = "lz4")))]
mod frame_size_tests {
    #[test]
    fn wire_size_is_checked_without_allocating_a_frame() {
        assert_eq!(
            super::packed_frame_size(u32::MAX as usize).unwrap(),
            u32::MAX
        );
        #[cfg(target_pointer_width = "64")]
        assert!(super::packed_frame_size(u32::MAX as usize + 1).is_err());
    }
}
