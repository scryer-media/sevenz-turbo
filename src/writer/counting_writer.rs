use std::{
    cell::Cell,
    io::{Seek, SeekFrom, Write},
    rc::Rc,
};

pub(crate) struct CountingWriter<W> {
    inner: W,
    counting: Rc<Cell<u64>>,
    written_bytes: u64,
}

impl<W> CountingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            counting: Rc::new(Cell::new(0)),
            written_bytes: 0,
        }
    }

    pub(crate) fn counting(&self) -> Rc<Cell<u64>> {
        Rc::clone(&self.counting)
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written_bytes
            .checked_add(buf.len() as u64)
            .ok_or_else(|| std::io::Error::other("7z output byte count overflow"))?;
        let len = self.inner.write(buf)?;
        self.written_bytes += len as u64;
        self.counting.set(self.written_bytes);
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write + Seek> Seek for CountingWriter<W> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Partial(Vec<u8>);
    impl Write for Partial {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let count = bytes.len().min(3);
            self.0.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn counts_partial_writes_beyond_32_bit_and_refuses_u64_overflow() {
        let mut writer = CountingWriter::new(Partial(Vec::new()));
        writer.written_bytes = u32::MAX as u64 - 1;
        let count = writer.counting();
        assert_eq!(writer.write(b"abcde").unwrap(), 3);
        assert_eq!(count.get(), u32::MAX as u64 + 2);
        assert_eq!(writer.inner.0, b"abc");
        writer.written_bytes = u64::MAX;
        assert!(writer.write(b"x").is_err());
        assert_eq!(writer.inner.0, b"abc");
        assert_eq!(count.get(), u32::MAX as u64 + 2);
    }
}
