//! A push that fails after putting some of its folder in the output poisons
//! the writer: later pushes and `finish` refuse, rather than write a header
//! whose pack offsets do not match the file. A failure that left the output
//! as it was does not.

#[cfg(feature = "compress")]
use std::io::{Cursor, Read};

#[cfg(feature = "compress")]
use sevenz_turbo::*;

/// Bytes that do not compress, so a coder has output to give early.
#[cfg(feature = "compress")]
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        })
        .collect()
}

/// A source that yields `data` up to `fail_at` bytes, then fails.
#[cfg(feature = "compress")]
struct FailsAfter {
    data: Vec<u8>,
    at: usize,
    fail_at: usize,
}

#[cfg(feature = "compress")]
impl FailsAfter {
    fn new(len: usize, fail_at: usize) -> Self {
        Self {
            data: noise(len),
            at: 0,
            fail_at,
        }
    }
}

#[cfg(feature = "compress")]
impl Read for FailsAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.fail_at {
            return Err(std::io::Error::other("source failed"));
        }
        let n = buf
            .len()
            .min(64 << 10)
            .min(self.fail_at - self.at)
            .min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(feature = "compress")]
fn writer(method: EncoderMethod) -> ArchiveWriter<Cursor<Vec<u8>>> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(vec![method.into()]);
    writer.set_encrypt_header(false);
    writer
}

#[cfg(feature = "compress")]
fn is_poisoned(error: &Error) -> bool {
    matches!(error, Error::WriterPoisoned(cause) if cause.contains("source failed"))
}

/// What `finish` returns on a poisoned writer: an `io::Error` carrying
/// `Error::WriterPoisoned`.
#[cfg(feature = "compress")]
fn assert_finish_poisoned(writer: ArchiveWriter<Cursor<Vec<u8>>>) {
    let error = writer.finish().expect_err("finish on a poisoned writer");
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    let inner = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>())
        .expect("an Error inside the io::Error");
    assert!(is_poisoned(inner), "{inner:?}");
}

#[cfg(feature = "compress")]
fn read_back(bytes: &[u8], name: &str) -> Vec<u8> {
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("reader");
    reader.read_file(name).expect("read file")
}

/// The source fails 3 MiB into a folder, past the 1 MiB the writer reads
/// ahead to size it, so the folder's first bytes are already in the output.
#[cfg(feature = "compress")]
#[test]
fn a_push_that_fails_mid_folder_poisons_the_writer() {
    for method in [EncoderMethod::COPY, EncoderMethod::LZMA2] {
        let mut writer = writer(method);
        let first = noise(100_000);
        writer
            .push_archive_entry(ArchiveEntry::new_file("first"), Some(first.as_slice()))
            .expect("first push");

        let error = writer
            .push_archive_entry(
                ArchiveEntry::new_file("broken"),
                Some(FailsAfter::new(8 << 20, 3 << 20)),
            )
            .expect_err("the source fails");
        assert!(!matches!(error, Error::WriterPoisoned(_)), "{error:?}");

        let error = writer
            .push_archive_entry(ArchiveEntry::new_file("after"), Some(&b"more"[..]))
            .expect_err("a push after the failure");
        assert!(is_poisoned(&error), "{method:?}: {error:?}");
        let error = writer
            .push_archive_entries(
                vec![ArchiveEntry::new_file("solid")],
                vec![SourceReader::new(&b"more"[..])],
            )
            .map(|_| ())
            .expect_err("a solid push after the failure");
        assert!(is_poisoned(&error), "{method:?}: {error:?}");
        let error = writer
            .push_archive_entry::<&[u8]>(ArchiveEntry::new_directory("dir"), None)
            .expect_err("even a directory");
        assert!(is_poisoned(&error), "{method:?}: {error:?}");

        assert_finish_poisoned(writer);
    }
}

/// The same through the solid route.
#[cfg(feature = "compress")]
#[test]
fn a_solid_push_that_fails_mid_folder_poisons_the_writer() {
    let mut writer = writer(EncoderMethod::COPY);
    let error = writer
        .push_archive_entries(
            vec![
                ArchiveEntry::new_file("a"),
                ArchiveEntry::new_file("broken"),
            ],
            vec![
                SourceReader::new(FailsAfter::new(2 << 20, 2 << 20)),
                SourceReader::new(FailsAfter::new(2 << 20, 1 << 20)),
            ],
        )
        .map(|_| ())
        .expect_err("the second source fails");
    assert!(!matches!(error, Error::WriterPoisoned(_)), "{error:?}");
    let error = writer
        .push_archive_entry(ArchiveEntry::new_file("after"), Some(&b"more"[..]))
        .expect_err("a push after the failure");
    assert!(is_poisoned(&error), "{error:?}");
    assert_finish_poisoned(writer);
}

/// The folder-parallel route writes a folder's staged bytes as they arrive,
/// before the worker reports that the folder failed.
#[cfg(feature = "compress")]
#[test]
fn a_parallel_non_solid_push_that_fails_mid_folder_poisons_the_writer() {
    let mut writer = writer(EncoderMethod::COPY);
    let entries = (0..4)
        .map(|i| ArchiveEntry::new_file(&format!("member{i}")))
        .collect();
    let error = writer
        .push_archive_entries_non_solid(
            entries,
            |index, _| {
                Ok(Some(if index == 2 {
                    FailsAfter::new(8 << 20, 3 << 20)
                } else {
                    FailsAfter::new(200_000, usize::MAX)
                }))
            },
            4,
        )
        .map(|_| ())
        .expect_err("member 2 fails");
    assert!(!matches!(error, Error::WriterPoisoned(_)), "{error:?}");
    let error = writer
        .push_archive_entry(ArchiveEntry::new_file("after"), Some(&b"more"[..]))
        .expect_err("a push after the failure");
    assert!(is_poisoned(&error), "{error:?}");
    assert_finish_poisoned(writer);
}

/// A failure before any of the folder reached the output - here, the very
/// first read - leaves the output as the header describes it, so the writer
/// carries on and the archive it finishes is whole.
#[cfg(feature = "compress")]
#[test]
fn a_push_that_fails_before_writing_leaves_the_writer_usable() {
    let mut writer = writer(EncoderMethod::COPY);
    let first = noise(100_000);
    writer
        .push_archive_entry(ArchiveEntry::new_file("first"), Some(first.as_slice()))
        .expect("first push");
    writer
        .push_archive_entry(
            ArchiveEntry::new_file("broken"),
            Some(FailsAfter::new(1 << 20, 0)),
        )
        .expect_err("the source fails at once");
    writer
        .push_archive_entry(ArchiveEntry::new_file("after"), Some(&b"more"[..]))
        .expect("the writer is not poisoned");
    let bytes = writer.finish().expect("finish").into_inner();
    assert!(read_back(&bytes, "first") == first);
    assert_eq!(read_back(&bytes, "after"), b"more");
}

/// A writer that never failed is untouched by the poisoning: every push
/// route and `finish` work as before, and the archive reads back.
#[cfg(feature = "compress")]
#[test]
fn a_writer_without_errors_is_unaffected() {
    let mut writer = writer(EncoderMethod::LZMA2);
    let a = noise(300_000);
    let b = noise(50_000);
    writer
        .push_archive_entry(ArchiveEntry::new_file("a"), Some(a.as_slice()))
        .expect("push a");
    writer
        .push_archive_entries(
            vec![ArchiveEntry::new_file("b")],
            vec![SourceReader::new(b.as_slice())],
        )
        .expect("push b");
    writer
        .push_archive_entries_non_solid(
            vec![ArchiveEntry::new_file("c"), ArchiveEntry::new_file("d")],
            |_, _| Ok(Some(&b"cd"[..])),
            2,
        )
        .expect("push c and d");
    let bytes = writer.finish().expect("finish").into_inner();
    assert!(read_back(&bytes, "a") == a);
    assert!(read_back(&bytes, "b") == b);
    assert_eq!(read_back(&bytes, "c"), b"cd");
    assert_eq!(read_back(&bytes, "d"), b"cd");
}

/// Dropping a poisoned writer, bare or inside `AutoFinisher`, does not panic.
#[cfg(feature = "compress")]
#[test]
fn a_poisoned_writer_drops_cleanly() {
    for auto in [false, true] {
        let mut writer = writer(EncoderMethod::COPY);
        writer
            .push_archive_entry(
                ArchiveEntry::new_file("broken"),
                Some(FailsAfter::new(8 << 20, 3 << 20)),
            )
            .expect_err("the source fails");
        if auto {
            drop(writer.auto_finish());
        } else {
            drop(writer);
        }
    }
}
