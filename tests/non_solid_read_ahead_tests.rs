#![cfg(feature = "compress")]

//! A non-solid LZMA2 folder of up to 1 MiB is read to its end before it is
//! coded, so that its coder can be told its exact length. These tests hold the
//! read-ahead to what it promises: a source is read once, a folder of exactly
//! the read-ahead's length is still sized, and folders coded by anything else
//! stream as they did.

use std::{
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use sevenz_turbo::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod, Password,
    encoder_options::{Lzma2Options, LzmaOptions},
};

/// Bytes that do not compress, so a coder's output keeps pace with its input.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn lzma2() -> EncoderConfiguration {
    Lzma2Options::from_level(5).into()
}

fn decoded(archive: Vec<u8>) -> Vec<Vec<u8>> {
    let mut reader = ArchiveReader::new(Cursor::new(archive), Password::empty()).unwrap();
    let mut files = Vec::new();
    reader
        .for_each_entries(|_, source| {
            let mut data = Vec::new();
            source.read_to_end(&mut data)?;
            files.push(data);
            Ok(true)
        })
        .unwrap();
    files
}

/// Yields `first`, then reports end of file once, then yields `later`: a file
/// appended to after it was read to its end.
struct EndsThenGrows {
    first: Vec<u8>,
    later: Vec<u8>,
    ended: bool,
}

impl Read for EndsThenGrows {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.first.is_empty() {
            let n = out.len().min(self.first.len());
            out[..n].copy_from_slice(&self.first[..n]);
            self.first.drain(..n);
            return Ok(n);
        }
        if !self.ended {
            self.ended = true;
            return Ok(0);
        }
        let n = out.len().min(self.later.len());
        out[..n].copy_from_slice(&self.later[..n]);
        self.later.drain(..n);
        Ok(n)
    }
}

/// A folder read to its end is coded from what was read: the source is not
/// read again, so bytes that appear after the end was seen are not coded past
/// the length the coder was promised, and the push succeeds.
#[test]
fn a_source_that_grows_after_its_end_is_coded_as_first_read() {
    let first = noise(3000);
    for declared in [0, 3000, 100] {
        for send in [false, true] {
            let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
            writer.set_content_methods(vec![lzma2()]);
            let mut entry = ArchiveEntry::new_file("grows");
            entry.size = declared;
            let source = EndsThenGrows {
                first: first.clone(),
                later: noise(5000),
                ended: false,
            };
            if send {
                writer.push_archive_entry_send(entry, Some(source)).unwrap();
            } else {
                writer.push_archive_entry(entry, Some(source)).unwrap();
            }
            let archive = writer.finish().unwrap().into_inner();
            assert_eq!(
                decoded(archive),
                std::slice::from_ref(&first),
                "{declared} {send}"
            );
        }
    }
}

/// A folder of exactly the read-ahead's length, and one a byte either side of
/// it, round-trip whether or not the entry declares its size.
#[test]
fn a_folder_at_the_read_ahead_length_round_trips() {
    for len in [(1 << 20) - 1, 1 << 20, (1 << 20) + 1] {
        let data = noise(len);
        let mut archives = Vec::new();
        for declared in [0, len as u64] {
            let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
            writer.set_content_methods(vec![lzma2()]);
            let mut entry = ArchiveEntry::new_file("f");
            entry.size = declared;
            writer
                .push_archive_entry(entry, Some(data.as_slice()))
                .unwrap();
            let archive = writer.finish().unwrap().into_inner();
            assert_eq!(
                decoded(archive.clone()),
                std::slice::from_ref(&data),
                "{len} {declared}"
            );
            archives.push(archive);
        }
        // Up to the read-ahead's length both are read to their end and coded
        // with the same promise. Past it, a folder that declares nothing has
        // no size, so its dictionary is not fitted to it and the bytes differ.
        if len <= 1 << 20 {
            assert_eq!(archives[0], archives[1], "{len}");
        }
    }
}

/// An archive's output, counting what has reached it.
struct CountedOutput {
    inner: Cursor<Vec<u8>>,
    written: Arc<AtomicUsize>,
}

impl Write for CountedOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for CountedOutput {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A source that notes how much of the archive had been written when it
/// first reported its end, waiting a while at the end for the first write.
struct Watched<'a> {
    data: &'a [u8],
    written: Arc<AtomicUsize>,
    at_end: Option<usize>,
}

impl Read for Watched<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = out.len().min(self.data.len()).min(64 << 10);
        if n == 0 && self.at_end.is_none() {
            // A coder on a thread of its own may lag its input: give it time
            // to write. A folder read ahead writes nothing however long the
            // source holds its end back, since its coder has not started.
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.written.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.at_end = Some(self.written.load(Ordering::Relaxed));
        }
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// How much of the archive was written before the source of a 600 KiB
/// folder, which declares its size, reported its end.
fn written_before_end(method: EncoderConfiguration) -> usize {
    let data = noise(600 << 10);
    let written = Arc::new(AtomicUsize::new(0));
    let output = CountedOutput {
        inner: Cursor::new(Vec::new()),
        written: Arc::clone(&written),
    };
    let mut writer = ArchiveWriter::new(output).unwrap();
    writer.set_content_methods(vec![method]);
    let mut entry = ArchiveEntry::new_file("f");
    entry.size = data.len() as u64;
    let mut source = Watched {
        data: &data,
        written,
        at_end: None,
    };
    writer.push_archive_entry(entry, Some(&mut source)).unwrap();
    writer.finish().unwrap();
    source.at_end.expect("the source was read to its end")
}

/// Only an LZMA2 folder reads ahead: COPY and LZMA folders stream their
/// output while the source is still being read, as they did before.
#[test]
fn only_an_lzma2_folder_reads_ahead() {
    let copy = EncoderConfiguration::new(EncoderMethod::COPY);
    let lzma: EncoderConfiguration = LzmaOptions::from_level(5).into();
    assert!(written_before_end(copy) > 0, "COPY");
    assert!(written_before_end(lzma) > 0, "LZMA");
    // The control: the LZMA2 folder is read whole before anything is coded.
    assert_eq!(written_before_end(lzma2()), 0, "LZMA2");
}

fn threaded_lzma2() -> EncoderConfiguration {
    Lzma2Options::from_level_mt(5, 4, 4 << 20).into()
}

/// The same threaded LZMA2 folder written from a local reader (pushed, no
/// read-ahead) and from a `Send` one (pulled, read ahead and limited) is the
/// same archive. Which of the two reads ahead is asserted by the writer's own
/// `only_a_pulled_lzma2_folder_reads_ahead`: a threaded coder holds its input
/// too long before writing for the archive's writes to tell.
#[test]
fn a_threaded_lzma2_folder_is_the_same_bytes_pushed_or_pulled() {
    let data = noise(600 << 10);
    let archive = |send: bool| {
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(vec![threaded_lzma2()]);
        let mut entry = ArchiveEntry::new_file("f");
        entry.size = data.len() as u64;
        if send {
            writer
                .push_archive_entry_send(entry, Some(data.as_slice()))
                .unwrap();
        } else {
            writer
                .push_archive_entry(entry, Some(data.as_slice()))
                .unwrap();
        }
        writer.finish().unwrap().into_inner()
    };
    let pushed = archive(false);
    assert_eq!(pushed, archive(true));
    assert_eq!(decoded(pushed), [data]);
}
