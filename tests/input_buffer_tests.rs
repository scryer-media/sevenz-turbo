#![cfg(feature = "compress")]

use std::io::{self, Cursor, Read};

use sevenz_turbo::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, Password, SourceReader,
    encoder_options::Lzma2Options, prepare_block,
};

struct ShortReads<'a> {
    data: &'a [u8],
    fail_after: Option<usize>,
    read: usize,
}

impl Read for ShortReads<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.fail_after == Some(self.read) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "source refused",
            ));
        }
        let available = self.fail_after.map_or(usize::MAX, |end| end - self.read);
        let n = out.len().min(self.data.len()).min(7777).min(available);
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        self.read += n;
        Ok(n)
    }
}

fn archive(path: u8, data: &[u8], known: bool, fail: bool) -> Result<Vec<u8>, sevenz_turbo::Error> {
    // Exceed one block so folder sizing cannot bypass the source buffer.
    let mut options = Lzma2Options::from_level_mt(5, 2, 4 << 20);
    options.set_dictionary_size(8 << 20);
    let methods: Vec<EncoderConfiguration> = vec![options.into()];
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new()))?;
    writer.set_content_methods(methods.clone());
    let mut entry = ArchiveEntry::new_file("data");
    if known {
        entry.size = data.len() as u64;
    }
    let source = ShortReads {
        data,
        fail_after: fail.then_some((2 << 20) + 31),
        read: 0,
    };
    match path {
        0 => {
            writer.push_archive_entry(entry, Some(source))?;
        }
        1 => {
            writer.push_archive_entries(vec![entry], vec![SourceReader::new(source)])?;
        }
        2 => {
            let block =
                prepare_block(methods.into(), vec![entry], vec![SourceReader::new(source)])?;
            writer.push_prepared_block(block)?;
        }
        3 => {
            writer.push_archive_entry_send(entry, Some(source))?;
        }
        4 => {
            writer.push_archive_entries_send(vec![entry], vec![SourceReader::new(source)])?;
        }
        _ => unreachable!(),
    }
    Ok(writer.finish()?.into_inner())
}

#[test]
fn aligned_source_paths_preserve_short_reads_bytes_and_crcs() {
    let data: Vec<u8> = (0..(8 << 20) + 37).map(|i| (i % 251) as u8).collect();
    for path in 0..5 {
        // Unknown size keeps the ordinary buffer. Both sources are longer
        // than the dictionary, so sizing leaves the same coder properties.
        let ordinary = archive(path, &data, false, false).unwrap();
        let aligned = archive(path, &data, true, false).unwrap();
        assert_eq!(aligned, ordinary, "input path {path}");
        let mut reader = ArchiveReader::new(Cursor::new(aligned), Password::empty()).unwrap();
        let mut seen = 0;
        reader
            .for_each_entries(|entry, source| {
                assert_eq!(entry.size, data.len() as u64);
                let mut decoded = Vec::new();
                source.read_to_end(&mut decoded)?;
                assert_eq!(decoded, data);
                seen += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(seen, 1);
    }
}

#[test]
fn aligned_source_paths_propagate_read_failures() {
    let data = vec![0; (8 << 20) + 37];
    for path in 0..5 {
        let err = archive(path, &data, true, true).unwrap_err();
        assert!(
            err.to_string().contains("source refused"),
            "path {path}: {err}"
        );
    }
}
