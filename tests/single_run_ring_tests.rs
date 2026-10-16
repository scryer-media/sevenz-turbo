#![cfg(feature = "compress")]

use sevenz_turbo::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, Password, encoder_options::Lzma2Options,
};
use std::io::{self, Cursor, Read, Seek, SeekFrom};

fn fixture(multi: bool) -> (Vec<u8>, Vec<u8>) {
    let data: Vec<_> = (0..(4 << 20) + 17)
        .map(|i| ((i * 19) ^ (i >> 5)) as u8)
        .collect();
    let mut options = if multi {
        Lzma2Options::from_level_mt(1, 2, 1 << 20)
    } else {
        Lzma2Options::from_level(1)
    };
    options.set_dictionary_size(1 << 16);
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(vec![options.into()]);
    writer.set_encrypt_header(false);
    let mut entry = ArchiveEntry::new_file("member");
    entry.size = data.len() as u64;
    writer
        .push_archive_entry(entry, Some(data.as_slice()))
        .unwrap();
    (writer.finish().unwrap().into_inner(), data)
}

#[test]
fn ordered_single_run_starts_in_the_ring_and_keeps_adaptive_and_multi_run_plans() {
    for multi in [false, true] {
        let (archive, expected) = fixture(multi);
        for adaptive in [false, true] {
            let mut reader = ArchiveReader::new(Cursor::new(&archive), Password::empty()).unwrap();
            reader.set_threads(4);
            reader.set_input_complete(true);
            reader.set_adaptive_lzma2(adaptive);
            let handle = reader.lzma2_handle();
            handle.keep_ledger();
            assert_eq!(reader.read_file("member").unwrap(), expected);
            assert_eq!(handle.ledger().unwrap().blocks, 1);
            assert_eq!(handle.progress().unwrap().block_index, 0);
        }
    }
}

struct Trickle {
    bytes: Cursor<Vec<u8>>,
    refuse_seek: bool,
}
impl Read for Trickle {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = out.len().min(3);
        self.bytes.read(&mut out[..count])
    }
}
impl Seek for Trickle {
    fn seek(&mut self, at: SeekFrom) -> io::Result<u64> {
        if std::mem::take(&mut self.refuse_seek) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        self.bytes.seek(at)
    }
}

#[test]
fn short_reads_and_a_declined_probe_restore_the_ordered_cursor() {
    let (archive, expected) = fixture(false);
    for refuse_seek in [false, true] {
        let mut reader = ArchiveReader::new(
            Trickle {
                bytes: Cursor::new(archive.clone()),
                refuse_seek: false,
            },
            Password::empty(),
        )
        .unwrap();
        reader.set_threads(2);
        reader.set_input_complete(true);
        reader.source_mut().refuse_seek = refuse_seek;
        let mut result = Vec::new();
        reader
            .for_each_entries_direct(|_, entry| {
                entry.write_rest(&mut result)?;
                Ok(true)
            })
            .unwrap();
        assert_eq!(result, expected);
    }
}

#[test]
fn the_single_run_hint_does_not_certify_corrupt_payload() {
    let (mut archive, _) = fixture(false);
    let reader = ArchiveReader::new(Cursor::new(&archive), Password::empty()).unwrap();
    let packed = reader.archive().block_pack_streams(0)[0];
    drop(reader);
    archive[(packed.offset + packed.size / 2) as usize] ^= 0x80;
    for threads in [1, 4] {
        let mut reader = ArchiveReader::new(Cursor::new(&archive), Password::empty()).unwrap();
        reader.set_threads(threads);
        reader.set_input_complete(true);
        assert!(reader.read_file("member").is_err());
    }
}

#[test]
fn custom_sources_are_not_probed_without_complete_input_permission() {
    struct Source {
        bytes: Cursor<Vec<u8>>,
        seeks: usize,
    }
    impl Read for Source {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.bytes.read(out)
        }
    }
    impl Seek for Source {
        fn seek(&mut self, at: SeekFrom) -> io::Result<u64> {
            self.seeks += 1;
            self.bytes.seek(at)
        }
    }
    let (archive, expected) = fixture(false);
    for complete in [false, true] {
        let mut reader = ArchiveReader::new(
            Source {
                bytes: Cursor::new(archive.clone()),
                seeks: 0,
            },
            Password::empty(),
        )
        .unwrap();
        reader.set_threads(4);
        if complete {
            reader.set_input_complete(true);
        }
        reader.source_mut().seeks = 0;
        let mut result = Vec::new();
        reader
            .block_decoder(0)
            .unwrap()
            .for_each_entries(&mut |_, entry| {
                entry.read_to_end(&mut result)?;
                Ok(true)
            })
            .unwrap();
        assert_eq!(result, expected);
        if complete {
            assert!(reader.source_mut().seeks > 1);
        } else {
            assert_eq!(
                reader.source_mut().seeks,
                1,
                "only the initial pack seek is permitted"
            );
        }
    }
}
