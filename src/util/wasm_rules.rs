//! The decisions the WASM `decompress` entry points make about each member,
//! kept apart from the JavaScript bindings so that they build and are tested
//! on the host, where `wasm.rs` does not build.

/// The buffer a member of `size` declared bytes is read into, or why it is
/// refused before anything is allocated: past a finite `max_unpack_bytes`, or
/// too large for `usize`.
pub(crate) fn member_buffer_len(size: u64, max_unpack_bytes: u64) -> Result<usize, String> {
    member_buffer_len_within(size, max_unpack_bytes, usize::MAX as u64)
}

fn member_buffer_len_within(
    size: u64,
    max_unpack_bytes: u64,
    max_len: u64,
) -> Result<usize, String> {
    if max_unpack_bytes != u64::MAX && size > max_unpack_bytes {
        return Err(format!(
            "entry declares {size} bytes, more than max_unpack_bytes ({max_unpack_bytes})"
        ));
    }
    if size > max_len {
        return Err(format!(
            "entry declares {size} bytes, more than this target can buffer"
        ));
    }
    Ok(size as usize)
}

/// The error a failed extraction callback ends the extraction with, from the
/// string form of what it threw, when it has one.
pub(crate) fn callback_failure(thrown: Option<String>) -> std::io::Error {
    std::io::Error::other(thrown.unwrap_or_else(|| "extraction callback failed".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_past_the_output_limit_is_refused() {
        assert_eq!(member_buffer_len(10, 10).unwrap(), 10);
        assert_eq!(member_buffer_len(0, 0).unwrap(), 0);
        let err = member_buffer_len(11, 10).unwrap_err();
        assert!(
            err.contains("11") && err.contains("max_unpack_bytes"),
            "{err}"
        );
        assert!(member_buffer_len(1 << 30, 1 << 30).is_ok());
        assert!(member_buffer_len((1 << 30) + 1, 1 << 30).is_err());
    }

    #[test]
    fn an_unlimited_caller_is_bounded_only_by_the_target() {
        assert_eq!(
            member_buffer_len_within(1 << 40, u64::MAX, u64::MAX).unwrap(),
            1 << 40
        );
        let err = member_buffer_len_within(u64::from(u32::MAX) + 1, u64::MAX, u64::from(u32::MAX))
            .unwrap_err();
        assert!(err.contains("this target"), "{err}");
        assert!(
            member_buffer_len_within(u64::from(u32::MAX), u64::MAX, u64::from(u32::MAX)).is_ok()
        );
    }

    #[test]
    fn a_callback_failure_carries_what_was_thrown() {
        let err = callback_failure(Some("Error: disk full".to_owned()));
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(err.to_string(), "Error: disk full");
        assert_eq!(
            callback_failure(None).to_string(),
            "extraction callback failed"
        );
    }

    /// The entry closure's shape in `decompress_with_limits`: a failure
    /// stops the walk, and its message is in the `String` the call returns.
    #[cfg(feature = "compress")]
    #[test]
    fn a_callback_failure_ends_the_extraction_with_its_message() {
        use std::io::Cursor;

        use crate::{ArchiveEntry, ArchiveReader, ArchiveWriter, Password, SourceReader};

        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        let entries = vec![ArchiveEntry::new_file("a"), ArchiveEntry::new_file("b")];
        let readers = vec![
            SourceReader::new(Cursor::new(b"first".to_vec())),
            SourceReader::new(Cursor::new(b"second".to_vec())),
        ];
        writer.push_archive_entries(entries, readers).unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
        let mut called = Vec::new();
        let result = reader
            .for_each_entries(|entry, _| {
                called.push(entry.name().to_owned());
                Err(callback_failure(Some("Error: disk full".to_owned())))?
            })
            .map_err(|e| e.to_string());
        assert_eq!(called, ["a"]);
        let message = result.unwrap_err();
        assert!(message.contains("Error: disk full"), "{message}");
    }
}
