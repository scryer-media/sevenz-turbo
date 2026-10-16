//! Bounded framing inspection before a seekable coder graph is built.

use std::io::{Read, Seek, SeekFrom};

use super::Lzma2RunScanner;

/// Establishes one complete run without retaining its compressed payload.
/// Uses the same scanner as `Lzma2MtReader`, but skips payload via `Seek`, as
/// `lzma_turbo::run_boundaries` does. This is only a scheduling hint: errors,
/// another run, or the inspection bound retain the ordinary decoder path.
/// The caller must seek to its own pack offset before decoding, regardless
/// of the result. No decoded bytes or checksums are certified here.
pub(crate) fn is_single_run<R: Read + Seek>(
    source: &mut R,
    offset: u64,
    packed: u64,
    unpacked: u64,
) -> bool {
    is_single_run_with_limit(source, offset, packed, unpacked, 1024)
}

/// The same proof with an explicit inspection budget. Large standalone
/// streams can afford more framing reads than an individual BCJ2 side stream.
pub(crate) fn is_single_run_with_limit<R: Read + Seek>(
    source: &mut R,
    offset: u64,
    packed: u64,
    unpacked: u64,
    max_reads: usize,
) -> bool {
    let Some(end) = offset.checked_add(packed) else {
        return false;
    };
    let mut scanner = Lzma2RunScanner::new();
    let mut header = [0u8; 64];
    for _ in 0..max_reads {
        let remaining = packed.saturating_sub(scanner.in_position());
        scanner.skip_payload(remaining);
        let Some(at) = offset.checked_add(scanner.in_position()) else {
            return false;
        };
        if at >= end || source.seek(SeekFrom::Start(at)).is_err() {
            return false;
        }
        let want = (end - at).min(header.len() as u64) as usize;
        let size = match source.read(&mut header[..want]) {
            Ok(0) | Err(_) => return false,
            Ok(size) if size <= want => size,
            Ok(_) => return false,
        };
        if scanner.feed(&header[..size]).is_err() || scanner.out_position() > unpacked {
            return false;
        }
        if scanner.pending_runs() != 0 {
            if !scanner.finished()
                || scanner.pending_runs() != 1
                || scanner.in_position() != packed
                || scanner.out_position() != unpacked
            {
                return false;
            }
            return scanner.next_run().is_some_and(|run| {
                run.has_dict_reset
                    && run.in_offset == 0
                    && run.out_offset == 0
                    && run.unpacked_len == unpacked
                    && run.packed_len.checked_add(1) == Some(packed)
            });
        }
        if scanner.finished() {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};

    use super::*;

    struct Counted {
        inner: Cursor<Vec<u8>>,
        read: usize,
        step: usize,
    }

    impl Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let want = buf.len().min(self.step);
            let n = self.inner.read(&mut buf[..want])?;
            self.read += n;
            Ok(n)
        }
    }

    impl Seek for Counted {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn stored(reset: bool, len: usize) -> Vec<u8> {
        let mut out = vec![if reset { 1 } else { 2 }];
        out.extend_from_slice(&((len - 1) as u16).to_be_bytes());
        out.resize(out.len() + len, 0x5a);
        out
    }

    #[test]
    fn a_probe_skips_payload_and_respects_the_exact_range() {
        let mut bytes = vec![0xff; 19];
        bytes.extend(stored(true, 8192));
        bytes.extend(stored(false, 4096));
        bytes.push(0);
        let packed = bytes.len() as u64 - 19;
        bytes.extend_from_slice(&[0xff; 19]);
        for step in [1, 3, 64] {
            let mut source = Counted {
                inner: Cursor::new(bytes.clone()),
                read: 0,
                step,
            };
            assert!(is_single_run(&mut source, 19, packed, 12288));
            assert!(source.read <= 129);
            assert!(!is_single_run(&mut source, 19, packed + 1, 12288));
            assert!(!is_single_run(&mut source, 19, packed, 12287));
        }
    }

    #[test]
    fn another_reset_or_incomplete_framing_keeps_the_normal_path() {
        let mut bytes = stored(true, 8192);
        bytes.extend(stored(true, 8192));
        bytes.push(0);
        let mut source = Counted {
            inner: Cursor::new(bytes),
            read: 0,
            step: 64,
        };
        let packed = source.inner.get_ref().len() as u64;
        assert!(!is_single_run(&mut source, 0, packed, 16384));
        assert!(source.read <= 128);
        for bytes in [
            vec![],
            vec![0],
            vec![1, 0],
            vec![0xff, 0, 0],
            stored(true, 8),
        ] {
            let packed = bytes.len() as u64;
            assert!(!is_single_run(&mut Cursor::new(bytes), 0, packed, 8));
        }
        assert!(!is_single_run(&mut Cursor::new(vec![0]), u64::MAX, 2, 8));
    }

    #[test]
    fn inspection_work_is_bounded_even_for_one_run() {
        let mut bytes = stored(true, 64);
        for _ in 0..1024 {
            bytes.extend(stored(false, 64));
        }
        bytes.push(0);
        let packed = bytes.len() as u64;
        let mut source = Counted {
            inner: Cursor::new(bytes),
            read: 0,
            step: 1,
        };
        assert!(!is_single_run(&mut source, 0, packed, 1025 * 64));
        assert_eq!(source.read, 1024);
        source.read = 0;
        assert!(is_single_run_with_limit(
            &mut source,
            0,
            packed,
            1025 * 64,
            4096
        ));
        assert!(source.read <= 4096);
    }

    #[test]
    fn a_standalone_probe_covers_a_large_chunk_count_with_bounded_reads() {
        let mut bytes = stored(true, 64);
        for _ in 1..16_384 {
            bytes.extend(stored(false, 64));
        }
        bytes.push(0);
        let packed = bytes.len() as u64;
        let mut source = Counted {
            inner: Cursor::new(bytes),
            read: 0,
            step: 64,
        };
        assert!(!is_single_run_with_limit(
            &mut source,
            0,
            packed,
            1 << 20,
            4096
        ));
        source.read = 0;
        assert!(is_single_run_with_limit(
            &mut source,
            0,
            packed,
            1 << 20,
            65_536
        ));
        assert!(source.read <= 4 << 20);
    }

    #[test]
    fn source_errors_only_decline_the_hint() {
        struct Fails(bool);
        impl Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Interrupted.into())
            }
        }
        impl Seek for Fails {
            fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
                if self.0 {
                    Err(io::ErrorKind::Unsupported.into())
                } else {
                    Ok(0)
                }
            }
        }
        assert!(!is_single_run(&mut Fails(true), 0, 9, 5));
        assert!(!is_single_run(&mut Fails(false), 0, 9, 5));
    }
}
