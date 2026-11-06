//! Regression tests ported from sevenz-rust2 0.23 through 0.25, the releases after
//! the 0.22.2 fork point. Each module holds the tests upstream added to the suite of
//! the same name; tests sevenz-turbo already carried are not repeated. Where
//! sevenz-turbo deliberately differs from upstream, the test asserts its own behavior
//! and says why.

mod compress {

    #[cfg(all(feature = "compress", feature = "ppmd"))]
    mod ppmd_regressions {
        use std::io::Cursor;

        use sevenz_turbo::{
            Archive, ArchiveEntry, ArchiveWriter, Password, encoder_options::PpmdOptions,
        };

        #[test]
        fn packed_stream_matches_7zip() {
            let input = b"PPMd regression: one stream, one finish.\n".repeat(3);
            // Independently generated with 7-Zip 26.03:
            // 7z a reference.7z sample.txt -m0=PPMd:o6:mem24 -mhc=off
            // sample.txt contains `input`; these are its packed bytes starting at offset 32.
            let expected = [
                0x00, 0x50, 0x01, 0xE2, 0xFB, 0xF5, 0x16, 0xDF, 0x8A, 0x30, 0xFF, 0x0C, 0x45, 0x27,
                0x9B, 0x64, 0x5D, 0x23, 0xEA, 0x13, 0x96, 0x12, 0x93, 0x15, 0xA5, 0xDC, 0x12, 0xB6,
                0x3B, 0xB5, 0x65, 0x3D, 0xCE, 0x1E, 0x68, 0xA2, 0x14, 0x40, 0x00, 0x00, 0x00, 0x00,
            ];

            let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
            writer
                .set_content_methods(vec![PpmdOptions::from_order_memory_size(6, 1 << 24).into()]);
            writer
                .push_archive_entry(ArchiveEntry::new_file("sample.txt"), Some(input.as_slice()))
                .unwrap();
            let mut output = writer.finish().unwrap();
            output.set_position(0);
            let archive = Archive::read(&mut output, &Password::empty()).unwrap();
            assert_eq!(archive.pack_sizes(), &[expected.len() as u64]);
            let start = 32 + archive.pack_pos() as usize;
            assert_eq!(&output.get_ref()[start..start + expected.len()], &expected);
        }

        #[cfg(feature = "aes256")]
        #[test]
        fn encrypted_final_block_preserves_output_error() {
            use std::io::{self, Seek, SeekFrom, Write};

            use sevenz_turbo::{Error, encoder_options::AesEncoderOptions};

            #[derive(Default)]
            struct RejectPayload {
                storage: Cursor<Vec<u8>>,
                rejected_lengths: Vec<usize>,
            }

            impl Write for RejectPayload {
                fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                    if self.storage.position() >= 32 && !bytes.is_empty() {
                        self.rejected_lengths.push(bytes.len());
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "payload rejected",
                        ));
                    }
                    self.storage.write(bytes)
                }

                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }

            impl Seek for RejectPayload {
                fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                    self.storage.seek(position)
                }
            }

            let mut output = RejectPayload::default();
            {
                let mut writer = ArchiveWriter::new(&mut output).unwrap();
                writer.set_content_methods(vec![
                    AesEncoderOptions::new(Password::new("ppmd-regression")).into(),
                    PpmdOptions::from_order_memory_size(6, 1 << 24).into(),
                ]);
                // A single symbol produces less than one AES block, so the output
                // failure can only occur when the encryption buffer is finalized.
                let error = writer
                    .push_archive_entry(ArchiveEntry::new_file("symbol.txt"), Some(&b"x"[..]))
                    .expect_err("the final encrypted block must reach the output");
                match error {
                    Error::Io(source, _) => {
                        assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
                        assert_eq!(source.to_string(), "payload rejected");
                    }
                    other => panic!("expected the output error, got {other:?}"),
                }
            }
            // Upstream asserts exactly one rejected write. Here the 16 byte block also reaches
            // the output a second time: std `BufWriter` retries its flush when it is dropped on
            // the error path. Those are the same ciphertext bytes, after the error was returned.
            assert_eq!(output.rejected_lengths.first(), Some(&16));
        }
    }
}

mod decompress_encrypted {

    #[cfg(feature = "aes256")]
    #[test]
    fn test_unread_entry_data_of_an_encrypted_solid_block() {
        use sevenz_turbo::ArchiveReader;

        // One encrypted solid block holds `encripted/7zFormat.txt` and `encripted/7ziplogo.png`. The
        // closure leaves the first entry unread. The second entry must decode with the right password.
        let path = "tests/resources/encrypted.7z";
        let password = "sevenz-rust";
        let mut reader = ArchiveReader::open(path, password.into()).unwrap();
        let expected = reader.read_file("encripted/7ziplogo.png").unwrap();

        let mut reader = ArchiveReader::open(path, password.into()).unwrap();
        let mut second = None;
        reader
            .for_each_entries(|entry, data| {
                if entry.name() == "encripted/7ziplogo.png" {
                    let mut buf = Vec::new();
                    data.read_to_end(&mut buf)?;
                    second = Some(buf);
                }
                Ok(true)
            })
            .unwrap();
        assert_eq!(second, Some(expected));
    }
}

mod decompression {
    use std::fs::File;

    use sevenz_turbo::{Archive, ArchiveReader, BlockDecoder, Password};

    #[test]
    fn test_empty_entry_between_files_of_a_solid_block() {
        // One solid block holds `d.txt`, `b.txt` and `c.txt`. The empty `a.txt` sits between
        // `d.txt` and `b.txt` in the file list. The block spans four entries for three sub-streams.
        let path = "tests/resources/interleaved_empty_entry.7z";
        let expected = vec![
            ("d.txt".to_string(), 1000),
            ("a.txt".to_string(), 0),
            ("b.txt".to_string(), 1000),
            ("c.txt".to_string(), 1000),
        ];

        let mut reader = ArchiveReader::open(path, Password::empty()).unwrap();
        let mut decoded = Vec::new();
        reader
            .for_each_entries(|entry, data| {
                let mut buf = Vec::new();
                data.read_to_end(&mut buf)?;
                decoded.push((entry.name().to_string(), buf.len()));
                Ok(true)
            })
            .unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(reader.read_file("c.txt").unwrap().len(), 1000);

        let mut file = File::open(path).unwrap();
        let archive = Archive::read(&mut file, &Password::empty()).unwrap();
        let password = Password::empty();
        let fd = BlockDecoder::new(1, 0, &archive, &password, &mut file);
        assert_eq!(fd.entry_count(), 4);
        let names: Vec<&str> = fd.entries().iter().map(|entry| entry.name()).collect();
        assert_eq!(names, ["d.txt", "a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn test_unread_entry_data_of_a_solid_block() {
        // One solid block holds `test/test1.txt` and `test/test2.txt`. The callback leaves all or
        // part of `test/test1.txt` unread. `test/test2.txt` must still decode to its own data.
        let path = "tests/resources/solid.7z";
        let mut reader = ArchiveReader::open(path, Password::empty()).unwrap();
        let expected = reader.read_file("test/test2.txt").unwrap();

        for read_len in [0, 100] {
            let mut reader = ArchiveReader::open(path, Password::empty()).unwrap();
            let mut second = None;
            reader
                .for_each_entries(|entry, data| {
                    if entry.name() == "test/test1.txt" {
                        let mut buf = vec![0; read_len];
                        data.read_exact(&mut buf)?;
                    } else if entry.name() == "test/test2.txt" {
                        let mut buf = Vec::new();
                        data.read_to_end(&mut buf)?;
                        second = Some(buf);
                    }
                    Ok(true)
                })
                .unwrap();
            assert_eq!(second.as_ref(), Some(&expected), "read_len {read_len}");
        }
    }

    // 7z structure IDs for the hand-crafted archives below.
    const K_END: u8 = 0x00;
    const K_HEADER: u8 = 0x01;
    const K_MAIN_STREAMS_INFO: u8 = 0x04;
    const K_FILES_INFO: u8 = 0x05;
    const K_PACK_INFO: u8 = 0x06;
    const K_UNPACK_INFO: u8 = 0x07;
    const K_SUB_STREAMS_INFO: u8 = 0x08;
    const K_SIZE: u8 = 0x09;
    const K_CRC: u8 = 0x0A;
    const K_FOLDER: u8 = 0x0B;
    const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
    const K_NUM_UNPACK_STREAM: u8 = 0x0D;
    const K_EMPTY_STREAM: u8 = 0x0E;
    const K_NAME: u8 = 0x11;

    /// Coder description of a single COPY coder: id size 1, no properties, id 0x00.
    const COPY_CODER: &[u8] = &[0x01, 0x00];

    /// A block of a hand-crafted archive with a single coder.
    struct RawBlock {
        /// Coder flags, id and properties of the single coder.
        coder: Vec<u8>,
        /// The unpack size the header declares for the block.
        unpack_size: u64,
        /// The block CRC. The header carries block CRCs when every block has one.
        crc: Option<u32>,
        /// The declared size of each sub-stream. The count can be zero.
        sub_streams: Vec<u64>,
    }

    /// Encodes a value in the 7z NUMBER format.
    fn write_number(out: &mut Vec<u8>, value: u64) {
        let mut first: u8 = 0;
        let mut mask: u8 = 0x80;
        let mut low = Vec::new();
        for i in 0..8 {
            if value < (1u64 << (7 * (i + 1))) {
                first |= (value >> (8 * i)) as u8;
                break;
            }
            first |= mask;
            mask >>= 1;
            low.push((value >> (8 * i)) as u8);
        }
        out.push(first);
        out.extend_from_slice(&low);
    }

    /// Builds an unencoded header. Each block reads one pack stream of the given size.
    /// Files listed in `empty_files` have no stream. The other files map to the
    /// sub-streams in block order.
    fn raw_header(
        pack_sizes: &[u64],
        blocks: &[RawBlock],
        names: &[&str],
        empty_files: &[usize],
    ) -> Vec<u8> {
        let mut h = vec![K_HEADER, K_MAIN_STREAMS_INFO, K_PACK_INFO, 0x00];
        write_number(&mut h, pack_sizes.len() as u64);
        h.push(K_SIZE);
        for &size in pack_sizes {
            write_number(&mut h, size);
        }
        h.push(K_END);

        h.extend_from_slice(&[K_UNPACK_INFO, K_FOLDER]);
        write_number(&mut h, blocks.len() as u64);
        h.push(0x00);
        for block in blocks {
            h.push(0x01);
            h.extend_from_slice(&block.coder);
        }
        h.push(K_CODERS_UNPACK_SIZE);
        for block in blocks {
            write_number(&mut h, block.unpack_size);
        }
        if blocks.iter().all(|block| block.crc.is_some()) {
            h.extend_from_slice(&[K_CRC, 0x01]);
            for block in blocks {
                h.extend_from_slice(&block.crc.unwrap().to_le_bytes());
            }
        }
        h.push(K_END);

        h.extend_from_slice(&[K_SUB_STREAMS_INFO, K_NUM_UNPACK_STREAM]);
        for block in blocks {
            write_number(&mut h, block.sub_streams.len() as u64);
        }
        if blocks.iter().any(|block| block.sub_streams.len() > 1) {
            h.push(K_SIZE);
            for block in blocks {
                for &size in block.sub_streams.iter().rev().skip(1).rev() {
                    write_number(&mut h, size);
                }
            }
        }
        h.extend_from_slice(&[K_END, K_END]);

        h.push(K_FILES_INFO);
        write_number(&mut h, names.len() as u64);
        if !empty_files.is_empty() {
            let mut bits = vec![0u8; names.len().div_ceil(8)];
            for &index in empty_files {
                bits[index / 8] |= 0x80 >> (index % 8);
            }
            h.push(K_EMPTY_STREAM);
            write_number(&mut h, bits.len() as u64);
            h.extend_from_slice(&bits);
        }
        let mut name_bytes = vec![0x00];
        for name in names {
            for unit in name.encode_utf16().chain([0]) {
                name_bytes.extend_from_slice(&unit.to_le_bytes());
            }
        }
        h.push(K_NAME);
        write_number(&mut h, name_bytes.len() as u64);
        h.extend_from_slice(&name_bytes);
        h.extend_from_slice(&[K_END, K_END]);
        h
    }

    /// Assembles a 7z file from the pack data and the header that follows it.
    fn raw_7z(pack: &[u8], header: &[u8]) -> Vec<u8> {
        let mut start = Vec::with_capacity(20);
        start.extend_from_slice(&(pack.len() as u64).to_le_bytes());
        start.extend_from_slice(&(header.len() as u64).to_le_bytes());
        start.extend_from_slice(&lzma_turbo::crc::crc32(header).to_le_bytes());

        let mut out = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0x00, 0x04];
        out.extend_from_slice(&lzma_turbo::crc::crc32(&start).to_le_bytes());
        out.extend_from_slice(&start);
        out.extend_from_slice(pack);
        out.extend_from_slice(header);
        out
    }

    /// A block that declares one 16 byte entry but decodes to 64 MiB. Upstream fails
    /// the iteration for every callback limit. sevenz-turbo fails it once the entry has
    /// been read in full, because one probe byte past the declared size shows the
    /// surplus. A callback that stops early gets `Ok`: nothing past what was asked for
    /// is decoded, so the surplus is never produced.
    #[cfg(feature = "compress")]
    #[test]
    fn continue_after_limited_read_does_not_drain_surplus() {
        const DECLARED: u64 = 16;

        let props = lzma_turbo::LzmaEncProps::new()
            .with_level(1)
            .with_dict_size(1 << 20);
        let (_, pack) = lzma_turbo::encode_lzma2(&vec![0; 64 << 20], &props).unwrap();

        // LZMA2 coder: id size 1 with properties, id 0x21, one property byte for a 1 MiB dictionary.
        let block = RawBlock {
            coder: vec![0x21, 0x21, 0x01, 16],
            unpack_size: DECLARED,
            crc: None,
            sub_streams: vec![DECLARED],
        };
        let header = raw_header(&[pack.len() as u64], &[block], &["bomb"], &[]);
        let archive = raw_7z(&pack, &header);

        // The first limit ends inside the declared size, the others at or past it.
        for limit in [8, DECLARED, 4096] {
            let mut reader =
                ArchiveReader::new(std::io::Cursor::new(archive.clone()), Password::empty())
                    .unwrap();
            reader.set_thread_count(1);

            let mut delivered = 0;
            let result = reader.for_each_entries(|_entry, data| {
                delivered +=
                    std::io::copy(&mut std::io::Read::take(data, limit), &mut std::io::sink())?;
                Ok(true)
            });

            assert_eq!(
                delivered,
                limit.min(DECLARED),
                "delivered with limit {limit}"
            );
            if limit < DECLARED {
                assert!(
                    result.is_ok(),
                    "a partial read must not fail, got: {result:?}"
                );
            } else {
                let err = result.expect_err("the surplus must fail the iteration");
                assert!(
                    format!("{err:?}").contains("more than its declared size"),
                    "unexpected error with limit {limit}: {err:?}"
                );
            }
        }
    }

    /// A solid LZMA2 block declares two 16 byte entries but decodes to 16 MiB. No entry
    /// may hand out more bytes than its declared size, neither through `for_each_entries`
    /// nor through `read_file`.
    #[cfg(feature = "compress")]
    #[test]
    fn final_solid_entry_output_is_bounded_by_declared_size() {
        use std::io::{Cursor, Write};

        const DECLARED: u64 = 16;

        let _ = Cursor::new(Vec::<u8>::new()).write_all(b"");
        let props = lzma_turbo::LzmaEncProps::new()
            .with_level(1)
            .with_dict_size(1 << 20);
        let (_, pack) = lzma_turbo::encode_lzma2(&vec![0; 16 << 20], &props).unwrap();

        // LZMA2 coder: id size 1 with properties, id 0x21, one property byte for a 1 MiB dictionary.
        let block = RawBlock {
            coder: vec![0x21, 0x21, 0x01, 16],
            unpack_size: 2 * DECLARED,
            crc: None,
            sub_streams: vec![DECLARED, DECLARED],
        };
        let header = raw_header(&[pack.len() as u64], &[block], &["a", "b"], &[]);
        let archive = raw_7z(&pack, &header);

        let mut reader =
            ArchiveReader::new(Cursor::new(archive.clone()), Password::empty()).unwrap();
        let mut delivered = Vec::new();
        let _ = reader.for_each_entries(|entry, data| {
            let size = std::io::copy(data, &mut std::io::sink())?;
            delivered.push((entry.name().to_string(), size));
            Ok(true)
        });
        for (name, size) in &delivered {
            assert!(
                *size <= DECLARED,
                "entry {name} delivered {size} bytes but declares {DECLARED}"
            );
        }

        let mut reader = ArchiveReader::new(Cursor::new(archive), Password::empty()).unwrap();
        if let Ok(data) = reader.read_file("b") {
            assert!(
                data.len() as u64 <= DECLARED,
                "read_file returned {} bytes but the entry declares {DECLARED}",
                data.len()
            );
        }
    }

    /// A callback that ignores a checksum error from its reader and returns `true` must not
    /// make the iteration succeed.
    #[test]
    fn ignored_checksum_error_fails_iteration() {
        use std::io::Cursor;

        let payload = b"hello world";
        let block = RawBlock {
            coder: COPY_CODER.to_vec(),
            unpack_size: payload.len() as u64,
            crc: Some(!lzma_turbo::crc::crc32(payload)),
            sub_streams: vec![payload.len() as u64],
        };
        let header = raw_header(&[payload.len() as u64], &[block], &["payload"], &[]);
        let archive = raw_7z(payload, &header);

        let mut reader = ArchiveReader::new(Cursor::new(archive), Password::empty()).unwrap();
        let mut saw_error = false;
        let result = reader.for_each_entries(|_entry, data| {
            saw_error |= data.read_to_end(&mut Vec::new()).is_err();
            Ok(true)
        });

        assert!(saw_error, "the callback must see the checksum error");
        assert!(
            result.is_err(),
            "iteration must fail after a checksum error, got: {result:?}"
        );
    }

    /// `false` from the `for_each_entries` callback ends the current block. Iteration
    /// continues with the next block.
    #[test]
    fn false_from_callback_ends_only_the_current_block() {
        use std::io::Cursor;

        let first = b"first";
        let second = b"second";
        let blocks = [first.as_slice(), second.as_slice()].map(|data| RawBlock {
            coder: COPY_CODER.to_vec(),
            unpack_size: data.len() as u64,
            crc: Some(lzma_turbo::crc::crc32(data)),
            sub_streams: vec![data.len() as u64],
        });
        let header = raw_header(
            &[first.len() as u64, second.len() as u64],
            &blocks,
            &["first", "second"],
            &[],
        );
        let archive = raw_7z(&[first.as_slice(), second.as_slice()].concat(), &header);

        let mut reader = ArchiveReader::new(Cursor::new(archive), Password::empty()).unwrap();
        let mut visited = Vec::new();
        reader
            .for_each_entries(|entry, _data| {
                visited.push(entry.name().to_string());
                Ok(false)
            })
            .unwrap();

        assert_eq!(visited, ["first", "second"]);
    }

    /// The 7z format allows a block without sub-streams. Such a block holds no entry
    /// and must not fail iteration.
    #[test]
    fn block_without_sub_streams_is_accepted() {
        use std::io::Cursor;

        let payload = b"data";
        let block = RawBlock {
            coder: COPY_CODER.to_vec(),
            unpack_size: payload.len() as u64,
            crc: Some(lzma_turbo::crc::crc32(payload)),
            sub_streams: Vec::new(),
        };
        let header = raw_header(&[payload.len() as u64], &[block], &["empty"], &[0]);
        let archive = raw_7z(payload, &header);

        let mut reader = ArchiveReader::new(Cursor::new(archive), Password::empty()).unwrap();
        let mut visited = Vec::new();
        let result = reader.for_each_entries(|entry, data| {
            std::io::copy(data, &mut std::io::sink())?;
            visited.push(entry.name().to_string());
            Ok(true)
        });

        assert!(
            result.is_ok(),
            "a block without sub-streams must be accepted, got: {result:?}"
        );
        assert_eq!(visited, ["empty"]);
    }

    /// An AES coder decrypts whole 16 byte blocks. The padding after the declared unpack
    /// size of the coder is not part of the entry, so an encrypted COPY archive decodes to
    /// exactly its declared size.
    #[cfg(all(feature = "compress", feature = "aes256"))]
    #[test]
    fn encrypted_copy_entry_excludes_aes_padding() {
        use std::io::Cursor;

        use sevenz_turbo::{
            ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod,
            encoder_options::AesEncoderOptions,
        };

        let content = b"hello world, this is not a multiple of sixteen";
        let mut bytes = Vec::new();
        {
            let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
            writer.set_content_methods(vec![
                AesEncoderOptions::new(Password::new("test")).into(),
                EncoderConfiguration::new(EncoderMethod::COPY),
            ]);
            writer
                .push_archive_entry(ArchiveEntry::new_file("a.txt"), Some(content.as_slice()))
                .unwrap();
            writer.finish().unwrap();
        }

        let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::new("test")).unwrap();
        let mut data = Vec::new();
        let result = reader.for_each_entries(|_entry, reader| {
            std::io::copy(reader, &mut data)?;
            Ok(true)
        });

        assert!(
            result.is_ok(),
            "a valid encrypted COPY archive must decode, got: {result:?}"
        );
        assert_eq!(data, content);
    }

    /// Upstream 4bbc327: `read_file` on a non-solid entry must verify the block CRC.
    #[test]
    fn read_file_verifies_the_block_crc() {
        use std::io::Cursor;
        let payload = b"hello world";
        let block = RawBlock {
            coder: COPY_CODER.to_vec(),
            unpack_size: payload.len() as u64,
            crc: Some(!lzma_turbo::crc::crc32(payload)),
            sub_streams: vec![payload.len() as u64],
        };
        let header = raw_header(&[payload.len() as u64], &[block], &["payload"], &[]);
        let archive = raw_7z(payload, &header);
        let mut reader = ArchiveReader::new(Cursor::new(archive), Password::empty()).unwrap();
        let result = reader.read_file("payload");
        assert!(
            result.is_err(),
            "a bad block CRC must fail read_file, got: {result:?}"
        );
    }
}

#[cfg(feature = "compress")]
mod regression {

    use std::io::Cursor;

    use sevenz_turbo::{Archive, ArchiveEntry, ArchiveWriter, Password};

    /// Writes `first`, then 20 directories, and reads the archive back. The 21 entries have no
    /// stream. The bit vectors with one bit for every entry without a stream span 3 bytes.
    fn write_with_directories_after(first: ArchiveEntry) -> Archive {
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.push_archive_entry::<&[u8]>(first, None).unwrap();
        for i in 0..20 {
            let directory = ArchiveEntry::new_directory(&format!("dir{i:02}"));
            writer.push_archive_entry::<&[u8]>(directory, None).unwrap();
        }
        let mut output = writer.finish().unwrap();
        output.set_position(0);
        Archive::read(&mut output, &Password::empty()).unwrap()
    }

    fn assert_directories_after(archive: &Archive, first: &str, first_is_directory: bool) {
        assert_eq!(archive.files.len(), 21);
        assert_eq!(archive.files[0].name(), first);
        assert_eq!(archive.files[0].is_directory(), first_is_directory);
        for (i, entry) in archive.files[1..].iter().enumerate() {
            assert_eq!(entry.name(), format!("dir{i:02}"));
            assert!(entry.is_directory());
        }
    }

    #[test]
    fn empty_file_followed_by_directories_round_trips() {
        let archive = write_with_directories_after(ArchiveEntry::new_file("empty.txt"));

        assert_directories_after(&archive, "empty.txt", false);
    }

    /// All entries are directories. The writer writes no `kEmptyFile` vector and only the `kAnti`
    /// vector is under test.
    #[test]
    fn anti_item_followed_by_directories_round_trips() {
        let mut anti = ArchiveEntry::new_directory("removed");
        anti.is_anti_item = true;
        let archive = write_with_directories_after(anti);

        assert_directories_after(&archive, "removed", true);
        assert!(archive.files[0].is_anti_item());
        assert!(archive.files[1..].iter().all(|entry| !entry.is_anti_item()));
    }

    /// A file time of 2^63 or more exceeds the range of `SystemTime` on Windows. Extracting an
    /// entry with such times must succeed.
    #[cfg(all(windows, feature = "util"))]
    #[test]
    fn extracting_an_entry_with_file_times_beyond_system_time_succeeds() {
        use sevenz_turbo::{NtTime, decompress};

        let time = NtTime::new(u64::MAX);
        let mut entry = ArchiveEntry::new_file("file.txt");
        entry.has_creation_date = true;
        entry.creation_date = time;
        entry.has_last_modified_date = true;
        entry.last_modified_date = time;
        entry.has_access_date = true;
        entry.access_date = time;
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer
            .push_archive_entry(entry, Some(&b"data"[..]))
            .unwrap();
        let mut archive = writer.finish().unwrap();
        archive.set_position(0);

        let read = Archive::read(&mut archive, &Password::empty()).unwrap();
        assert_eq!(read.files[0].last_modified_date(), time);
        archive.set_position(0);

        let dest = tempfile::tempdir().unwrap();
        decompress(archive, dest.path()).unwrap();
        assert_eq!(
            std::fs::read(dest.path().join("file.txt")).unwrap(),
            b"data"
        );
    }
}

#[cfg(all(feature = "compress", feature = "util"))]
mod security {
    //! Security regression tests for malicious-archive handling in the decode paths.

    use std::io::Cursor;

    use sevenz_turbo::{ArchiveEntry, ArchiveReader, Password};

    const SEVEN_Z_SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

    /// CRC-32/ISO-HDLC over the start/next headers, reusing the crate's own `crc32fast`
    /// dependency (available to integration tests) so the crafted archives pass verification.
    fn crc32(data: &[u8]) -> u32 {
        lzma_turbo::crc::crc32(data)
    }

    // 7z structure IDs used by the crafted headers.
    const K_END: u8 = 0x00;
    const K_HEADER: u8 = 0x01;
    const K_MAIN_STREAMS_INFO: u8 = 0x04;
    const K_FILES_INFO: u8 = 0x05;
    const K_PACK_INFO: u8 = 0x06;
    const K_UNPACK_INFO: u8 = 0x07;
    const K_SUB_STREAMS_INFO: u8 = 0x08;
    const K_SIZE: u8 = 0x09;
    const K_FOLDER: u8 = 0x0B;
    const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
    const K_NUM_UNPACK_STREAM: u8 = 0x0D;

    /// Encodes a value using the 7z NUMBER varint format, matching the reader's decoder.
    fn write_number(out: &mut Vec<u8>, value: u64) {
        let mut first: u8 = 0;
        let mut mask: u8 = 0x80;
        let mut low = Vec::new();
        let mut i = 0u32;
        while i < 8 {
            if value < (1u64 << (7 * (i + 1))) {
                first |= (value >> (8 * i)) as u8;
                break;
            }
            first |= mask;
            mask >>= 1;
            low.push((value >> (8 * i)) as u8);
            i += 1;
        }
        out.push(first);
        out.extend_from_slice(&low);
    }

    /// Assembles a raw 7z file: signature header + (empty pack area) + `next_header` placed
    /// immediately after the signature header, with `declared_next_header_size` written into
    /// the start header (normally the real length, but overridable to fake an oversized one).
    fn raw_7z(next_header: &[u8], declared_next_header_size: u64) -> Vec<u8> {
        let mut start = [0u8; 20];
        start[0..8].copy_from_slice(&0u64.to_le_bytes()); // next_header_offset
        start[8..16].copy_from_slice(&declared_next_header_size.to_le_bytes());
        start[16..20].copy_from_slice(&crc32(next_header).to_le_bytes());
        let start_crc = crc32(&start);

        let mut out = Vec::new();
        out.extend_from_slice(&SEVEN_Z_SIGNATURE);
        out.extend_from_slice(&[0x00, 0x04]); // format version major.minor
        out.extend_from_slice(&start_crc.to_le_bytes());
        out.extend_from_slice(&start);
        out.extend_from_slice(next_header);
        out
    }

    /// Convenience: build a file whose start header truthfully describes `next_header`.
    fn raw_7z_exact(next_header: &[u8]) -> Vec<u8> {
        raw_7z(next_header, next_header.len() as u64)
    }

    fn open_err(bytes: &[u8]) -> Result<(), String> {
        match ArchiveReader::new(Cursor::new(bytes.to_vec()), Password::empty()) {
            Ok(_) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Drives full extraction so decode-path issues are exercised.
    fn decode_err(bytes: &[u8]) -> Result<(), String> {
        let mut reader = ArchiveReader::new(Cursor::new(bytes.to_vec()), Password::empty())
            .map_err(|e| e.to_string())?;
        reader
            .for_each_entries(&mut |_e: &ArchiveEntry, rd: &mut dyn std::io::Read| {
                std::io::copy(rd, &mut std::io::sink())?;
                Ok(true)
            })
            .map_err(|e| e.to_string())
    }

    /// Like `raw_7z_exact` but places `packed` bytes right after the signature header and puts
    /// the next header after them (so coders have real packed data to read during decode).
    fn raw_7z_with_packed(packed: &[u8], next_header: &[u8]) -> Vec<u8> {
        let mut start = [0u8; 20];
        start[0..8].copy_from_slice(&(packed.len() as u64).to_le_bytes()); // next_header_offset
        start[8..16].copy_from_slice(&(next_header.len() as u64).to_le_bytes());
        start[16..20].copy_from_slice(&crc32(next_header).to_le_bytes());
        let start_crc = crc32(&start);

        let mut out = Vec::new();
        out.extend_from_slice(&SEVEN_Z_SIGNATURE);
        out.extend_from_slice(&[0x00, 0x04]);
        out.extend_from_slice(&start_crc.to_le_bytes());
        out.extend_from_slice(&start);
        out.extend_from_slice(packed);
        out.extend_from_slice(next_header);
        out
    }

    /// An archive whose header has streams info but no files info must be rejected.
    #[test]
    fn streams_without_files_info_are_rejected() {
        let bytes = std::fs::read("tests/resources/streams_without_files_info.bin").unwrap();

        assert!(
            open_err(&bytes).is_err(),
            "streams without files info must be rejected when opening"
        );
        assert!(
            decode_err(&bytes).is_err(),
            "streams without files info must be rejected when decoding"
        );
    }

    /// The same defect as `streams_without_files_info_are_rejected`, inside an LZMA encoded header.
    /// The decoded header declares four blocks but no files info.
    #[test]
    fn encoded_streams_without_files_info_are_rejected() {
        let bytes =
            std::fs::read("tests/resources/encoded_streams_without_files_info.bin").unwrap();

        assert!(
            open_err(&bytes).is_err(),
            "streams without files info must be rejected when opening"
        );
        assert!(
            decode_err(&bytes).is_err(),
            "streams without files info must be rejected when decoding"
        );
    }

    /// A coder with zero input streams gives a block with zero packed streams. Its first pack
    /// stream index equals `pack_sizes.len()`, and decoding the block must not index past the
    /// pack streams.
    #[test]
    fn block_without_packed_streams_is_rejected() {
        let lone: &[u8] = &[
            K_HEADER,
            K_MAIN_STREAMS_INFO,
            K_UNPACK_INFO,
            K_FOLDER,
            0x01, // num_blocks
            0x00, // external
            0x01, // num_coders
            0x11, // coder flags: id_size=1, complex, no attributes
            0x00, // coder id = COPY
            0x00, // num_in_streams = 0
            0x01, // num_out_streams = 1
            K_CODERS_UNPACK_SIZE,
            0x00,  // unpack_sizes[0] = 0
            K_END, // unpack_info end
            K_END, // streams_info end
            K_FILES_INFO,
            0x00,  // num_files = 0
            K_END, // files_info props end
            K_END, // header end
        ];
        assert!(
            decode_err(&raw_7z_exact(lone)).is_err(),
            "a lone block without packed streams must be rejected"
        );

        let after_copy_block: &[u8] = &[
            K_HEADER,
            K_MAIN_STREAMS_INFO,
            K_PACK_INFO,
            0x00, // pack_pos
            0x01, // num_pack_streams
            K_SIZE,
            0x05, // pack_sizes[0] = 5
            K_END,
            K_UNPACK_INFO,
            K_FOLDER,
            0x02, // num_blocks
            0x00, // external
            0x01, // block 0: num_coders
            0x01, // coder flags: id_size=1, simple, no attributes
            0x00, // coder id = COPY
            0x01, // block 1: num_coders
            0x11, // coder flags: id_size=1, complex, no attributes
            0x00, // coder id = COPY
            0x00, // num_in_streams = 0
            0x01, // num_out_streams = 1
            K_CODERS_UNPACK_SIZE,
            0x05, // unpack_sizes[0] = 5
            0x00, // unpack_sizes[1] = 0
            K_END,
            K_SUB_STREAMS_INFO,
            K_NUM_UNPACK_STREAM,
            0x01,  // block 0: one sub-stream
            0x00,  // block 1: no sub-streams
            K_END, // sub_streams_info end
            K_END, // streams_info end
            K_FILES_INFO,
            0x01,  // num_files = 1
            K_END, // files_info props end
            K_END, // header end
        ];
        assert!(
            decode_err(&raw_7z_with_packed(&[0xAA; 5], after_copy_block)).is_err(),
            "a block without packed streams after a copy block must be rejected"
        );
    }

    /// Every coder has exactly one output stream. Here coder 1 declares none, so the block has two
    /// coders and one unpack size. `Block::get_unpack_size_for_coder` indexes the unpack sizes by
    /// the coder position. Opening the archive must fail.
    #[test]
    fn coder_without_output_stream_is_rejected() {
        let nh: &[u8] = &[
            K_HEADER,
            K_MAIN_STREAMS_INFO,
            K_PACK_INFO,
            0x00, // pack_pos
            0x02, // num_pack_streams
            K_SIZE,
            0x01, // pack_sizes[0] = 1
            0x01, // pack_sizes[1] = 1
            K_END,
            K_UNPACK_INFO,
            K_FOLDER,
            0x01, // num_blocks
            0x00, // external
            0x02, // num_coders
            0x01, // coder 0 flags: id_size=1, simple, no attributes
            0x00, // coder 0 id = COPY
            0x11, // coder 1 flags: id_size=1, complex, no attributes
            0x00, // coder 1 id = COPY
            0x01, // num_in_streams = 1
            0x00, // num_out_streams = 0
            0x00, // packed_streams[0] = 0
            0x01, // packed_streams[1] = 1
            K_CODERS_UNPACK_SIZE,
            0x01,  // unpack_sizes[0] = 1
            K_END, // unpack_info end
            K_SUB_STREAMS_INFO,
            K_END, // sub_streams_info end
            K_END, // streams_info end
            K_FILES_INFO,
            0x01,  // num_files = 1
            K_END, // files_info props end
            K_END, // header end
        ];
        let bytes = raw_7z_with_packed(&[0xAA, 0xBB], nh);
        assert!(
            open_err(&bytes).is_err(),
            "a coder without an output stream must be rejected"
        );
    }

    /// A header with one block of `num_coders` COPY coders in a chain over five packed bytes, and
    /// one file.
    fn copy_chain_header(num_coders: u64) -> Vec<u8> {
        let mut nh = vec![
            K_HEADER,
            K_MAIN_STREAMS_INFO,
            K_PACK_INFO,
            0x00, // pack_pos
            0x01, // num_pack_streams
            K_SIZE,
            0x05, // pack_sizes[0] = 5
            K_END,
            K_UNPACK_INFO,
            K_FOLDER,
            0x01, // num_blocks
            0x00, // external
        ];
        write_number(&mut nh, num_coders);
        for _ in 0..num_coders {
            nh.extend_from_slice(&[0x01, 0x00]); // simple COPY coder
        }
        // Coder `i` reads the output of coder `i + 1`. The packed stream feeds the last coder.
        for i in 0..num_coders - 1 {
            write_number(&mut nh, i); // in_index
            write_number(&mut nh, i + 1); // out_index
        }
        nh.push(K_CODERS_UNPACK_SIZE);
        nh.resize(nh.len() + num_coders as usize, 0x05); // unpack size of every coder
        nh.extend_from_slice(&[
            K_END, // unpack_info end
            K_SUB_STREAMS_INFO,
            K_END, // sub_streams_info end
            K_END, // streams_info end
            K_FILES_INFO,
            0x01,  // num_files = 1
            K_END, // files_info props end
            K_END, // header end
        ]);
        nh
    }

    /// The decoder nests one reader for each coder of a block, and a long chain of coders exhausts
    /// the stack. 7-Zip accepts at most 64 coders in a block.
    #[test]
    fn block_with_too_many_coders_is_rejected() {
        let bytes = raw_7z_with_packed(&[0xAA; 5], &copy_chain_header(64));
        assert!(
            decode_unlimited(&bytes).is_ok(),
            "64 coders must decode under ArchiveLimits::unlimited; the default limit is 8"
        );

        let bytes = raw_7z_with_packed(&[0xAA; 5], &copy_chain_header(65));
        assert!(
            open_err(&bytes).is_err(),
            "a block with 65 coders must be rejected"
        );
    }

    /// A header from `coder_graph_header` and its number of packed streams. Coder `k` declares
    /// `streams_per_coder[k]` input streams. The first input of coder `k` reads the output of coder
    /// `k + 1`. Every other input reads a packed stream.
    fn input_streams_header(streams_per_coder: &[u64]) -> (Vec<u8>, usize) {
        let total: u64 = streams_per_coder.iter().sum();
        let mut bind_pairs = Vec::new();
        let mut first = 0;
        for (k, &streams) in streams_per_coder[..streams_per_coder.len() - 1]
            .iter()
            .enumerate()
        {
            bind_pairs.push((first, k as u64 + 1));
            first += streams;
        }
        let packed: Vec<u64> = (0..total)
            .filter(|i| !bind_pairs.iter().any(|&(in_index, _)| in_index == *i))
            .collect();
        let nh = coder_graph_header(streams_per_coder, &bind_pairs, &packed);
        (nh, packed.len())
    }

    /// 7-Zip accepts at most 64 input streams in a block. One coder declares 65, or two coders
    /// declare 33 each.
    #[test]
    fn block_with_too_many_input_streams_is_rejected() {
        for streams_per_coder in [&[65][..], &[33, 33]] {
            let (nh, num_packed) = input_streams_header(streams_per_coder);
            let bytes = raw_7z_with_packed(&vec![0xAA; num_packed], &nh);
            assert!(
                open_err(&bytes).is_err(),
                "a block with input streams {streams_per_coder:?} must be rejected"
            );
        }
    }

    /// The LZMA2 encoded header decodes to 30,000,000 bytes and declares 29,999,984 files. Their
    /// entries need more than `isize::MAX` bytes on 32-bit targets. Opening the archive must fail
    /// with an error.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn encoded_header_with_too_many_files_for_the_address_space_is_rejected() {
        let bytes = std::fs::read("tests/resources/encoded_header_with_many_files.bin").unwrap();

        assert!(
            open_err(&bytes).is_err(),
            "files beyond the address space must be rejected"
        );
    }

    /// A header with one block of COPY coders and one file. Coder `k` declares
    /// `streams_per_coder[k]` input streams. Each bind pair is `(in_index, out_index)`, and every
    /// packed stream index gets a one-byte pack stream.
    fn coder_graph_header(
        streams_per_coder: &[u64],
        bind_pairs: &[(u64, u64)],
        packed_streams: &[u64],
    ) -> Vec<u8> {
        let mut nh = vec![K_HEADER, K_MAIN_STREAMS_INFO, K_PACK_INFO, 0x00]; // pack_pos
        write_number(&mut nh, packed_streams.len() as u64); // num_pack_streams
        nh.push(K_SIZE);
        nh.resize(nh.len() + packed_streams.len(), 0x01); // pack_sizes = 1 each
        nh.extend_from_slice(&[
            K_END,
            K_UNPACK_INFO,
            K_FOLDER,
            0x01, // num_blocks
            0x00, // external
        ]);
        write_number(&mut nh, streams_per_coder.len() as u64); // num_coders
        for &streams in streams_per_coder {
            nh.extend_from_slice(&[0x11, 0x00]); // complex COPY coder
            write_number(&mut nh, streams); // num_in_streams
            nh.push(0x01); // num_out_streams = 1
        }
        for &(in_index, out_index) in bind_pairs {
            write_number(&mut nh, in_index);
            write_number(&mut nh, out_index);
        }
        if packed_streams.len() > 1 {
            for &in_index in packed_streams {
                write_number(&mut nh, in_index);
            }
        }
        nh.push(K_CODERS_UNPACK_SIZE);
        nh.resize(nh.len() + streams_per_coder.len(), 0x01); // unpack size of every coder
        nh.extend_from_slice(&[
            K_END, // unpack_info end
            K_SUB_STREAMS_INFO,
            K_END, // sub_streams_info end
            K_END, // streams_info end
            K_FILES_INFO,
            0x01,  // num_files = 1
            K_END, // files_info props end
            K_END, // header end
        ]);
        nh
    }

    /// A name, the input streams of each coder, the bind pairs and the packed streams of a block.
    type CoderGraph<'a> = (&'a str, &'a [u64], &'a [(u64, u64)], &'a [u64]);

    /// Each input stream of a block is bound or packed once, and each coder output is bound once.
    /// A stream used twice makes the decoder build the coders behind it once for every use. A
    /// chain of BCJ2 coders with three inputs bound to the same output builds 3^n readers.
    #[test]
    fn block_with_a_stream_used_twice_is_rejected() {
        let valid = coder_graph_header(&[2, 1], &[(0, 1)], &[1, 2]);
        assert!(
            open_err(&raw_7z_with_packed(&[0xAA; 2], &valid)).is_ok(),
            "a block with each stream used once must open"
        );

        let cases: [CoderGraph; 5] = [
            (
                "an output bound twice",
                &[2, 1, 1],
                &[(0, 1), (1, 1)],
                &[2, 3],
            ),
            (
                "an input bound twice",
                &[2, 1, 1],
                &[(0, 1), (0, 2)],
                &[1, 2],
            ),
            ("an input packed twice", &[2, 1], &[(0, 1)], &[1, 1]),
            ("an input bound and packed", &[2, 1], &[(0, 1)], &[0, 1]),
            ("a packed input out of range", &[2, 1], &[(0, 1)], &[1, 5]),
        ];
        for (name, streams_per_coder, bind_pairs, packed_streams) in cases {
            let nh = coder_graph_header(streams_per_coder, bind_pairs, packed_streams);
            let bytes = raw_7z_with_packed(&vec![0xAA; packed_streams.len()], &nh);
            assert!(open_err(&bytes).is_err(), "{name} must be rejected");
        }
    }

    /// A source of `len` bytes that holds `head` at its start and zeros after it.
    #[cfg(target_pointer_width = "32")]
    struct SparseSource {
        head: Vec<u8>,
        len: u64,
        pos: u64,
    }

    #[cfg(target_pointer_width = "32")]
    impl std::io::Read for SparseSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.len.saturating_sub(self.pos).min(buf.len() as u64) as usize;
            let out = &mut buf[..n];
            out.fill(0);
            if let Some(head) = self.head.get(self.pos as usize..) {
                let m = head.len().min(n);
                out[..m].copy_from_slice(&head[..m]);
            }
            self.pos += n as u64;
            Ok(n)
        }
    }

    #[cfg(target_pointer_width = "32")]
    impl std::io::Seek for SparseSource {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.pos = match pos {
                std::io::SeekFrom::Start(n) => n,
                std::io::SeekFrom::End(n) => self.len.checked_add_signed(n).unwrap(),
                std::io::SeekFrom::Current(n) => self.pos.checked_add_signed(n).unwrap(),
            };
            Ok(self.pos)
        }
    }

    /// The start header declares a next header of 0x9000_0000 bytes in a 3 GiB source. The size
    /// fits the source and exceeds `isize::MAX` on 32-bit targets. Opening the archive must fail
    /// with an error.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn next_header_beyond_the_address_space_is_rejected() {
        let source = SparseSource {
            head: raw_7z(&[], 0x9000_0000),
            len: 3 << 30,
            pos: 0,
        };
        assert!(
            ArchiveReader::new(source, Password::empty()).is_err(),
            "a next header beyond the address space must be rejected"
        );
    }

    /// The start header declares a next header of 0x4800_0000 bytes in a 3 GiB source. It holds an
    /// encoded header with one COPY block of 0x5000_0000 bytes, more than the next header holds.
    /// Doubling a buffer of the next header size exceeds `isize::MAX` on 32-bit targets. Opening
    /// the archive must fail with an error.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn encoded_header_beyond_the_address_space_is_rejected() {
        const NEXT_HEADER_SIZE: u64 = 0x4800_0000;
        const UNPACK_SIZE: u64 = 0x5000_0000;

        let mut next_header = vec![
            0x17, // K_ENCODED_HEADER
            K_PACK_INFO,
            0x00, // pack_pos
            0x01, // num_pack_streams
            K_SIZE,
        ];
        write_number(&mut next_header, UNPACK_SIZE); // pack_sizes[0]
        next_header.extend_from_slice(&[
            K_END,
            K_UNPACK_INFO,
            K_FOLDER,
            0x01, // num_blocks
            0x00, // external
            0x01, // num_coders
            0x01, // coder flags: id_size=1, simple, no attributes
            0x00, // coder id = COPY
            K_CODERS_UNPACK_SIZE,
        ]);
        write_number(&mut next_header, UNPACK_SIZE); // unpack_sizes[0]
        next_header.extend_from_slice(&[K_END, K_END]); // unpack_info end, streams_info end

        // The next header continues with zeros up to its declared size.
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&next_header);
        let zeros = vec![0u8; 1 << 20];
        let mut left = NEXT_HEADER_SIZE - next_header.len() as u64;
        while left > 0 {
            let n = left.min(zeros.len() as u64) as usize;
            hasher.update(&zeros[..n]);
            left -= n as u64;
        }

        let mut start = [0u8; 20];
        start[8..16].copy_from_slice(&NEXT_HEADER_SIZE.to_le_bytes());
        start[16..20].copy_from_slice(&hasher.finalize().to_le_bytes());
        let mut head = SEVEN_Z_SIGNATURE.to_vec();
        head.extend_from_slice(&[0x00, 0x04]);
        head.extend_from_slice(&crc32(&start).to_le_bytes());
        head.extend_from_slice(&start);
        head.extend_from_slice(&next_header);

        let source = SparseSource {
            head,
            len: 3 << 30,
            pos: 0,
        };
        assert!(
            ArchiveReader::new(source, Password::empty()).is_err(),
            "an encoded header beyond the address space must be rejected"
        );
    }

    fn decode_unlimited(bytes: &[u8]) -> Result<(), String> {
        let mut reader = ArchiveReader::with_limits(
            Cursor::new(bytes.to_vec()),
            Password::empty(),
            sevenz_turbo::ArchiveLimits::unlimited(),
        )
        .map_err(|e| e.to_string())?;
        reader
            .for_each_entries(&mut |_e: &ArchiveEntry, rd: &mut dyn std::io::Read| {
                std::io::copy(rd, &mut std::io::sink())?;
                Ok(true)
            })
            .map_err(|e| e.to_string())
    }

    /// A chain of copy coders. The default limit refuses more than 8 coders. Under
    /// `ArchiveLimits::unlimited` the hard cap of 64 coders still applies: before it,
    /// a 100,000 coder chain overflowed the stack while the decoders were built.
    #[test]
    fn deep_copy_chain_is_capped_even_when_unlimited() {
        for n in [8u64, 64] {
            let bytes = raw_7z_with_packed(&[0xAA; 5], &copy_chain_header(n));
            assert_eq!(decode_unlimited(&bytes), Ok(()), "{n} coders, unlimited");
        }
        let bytes = raw_7z_with_packed(&[0xAA; 5], &copy_chain_header(8));
        assert_eq!(decode_err(&bytes), Ok(()), "8 coders, default limits");
        for n in [9u64, 65, 100_000] {
            let bytes = raw_7z_with_packed(&[0xAA; 5], &copy_chain_header(n));
            assert!(decode_err(&bytes).is_err(), "{n} coders, default limits");
        }
        for n in [65u64, 100_000] {
            let bytes = raw_7z_with_packed(&[0xAA; 5], &copy_chain_header(n));
            let err = decode_unlimited(&bytes).expect_err("past the hard cap");
            assert!(err.contains("more than 64 coders"), "{n} coders: {err}");
        }
    }

    /// Upstream 2168265: every coder has exactly one output stream. A coder that
    /// declares zero or two outputs is refused, under any limits.
    #[test]
    fn coder_with_other_than_one_output_is_rejected() {
        for outs in [0u8, 2] {
            let nh: Vec<u8> = vec![
                K_HEADER,
                K_MAIN_STREAMS_INFO,
                K_PACK_INFO,
                0x00, // pack_pos
                0x02, // num_pack_streams
                K_SIZE,
                0x01, // pack_sizes[0] = 1
                0x01, // pack_sizes[1] = 1
                K_END,
                K_UNPACK_INFO,
                K_FOLDER,
                0x01, // num_blocks
                0x00, // external
                0x02, // num_coders
                0x01, // coder 0 flags: id_size=1, simple, no attributes
                0x00, // coder 0 id = COPY
                0x11, // coder 1 flags: id_size=1, complex, no attributes
                0x00, // coder 1 id = COPY
                0x01, // num_in_streams = 1
                outs, // num_out_streams
                0x00, // packed_streams[0] = 0
                0x01, // packed_streams[1] = 1
                K_CODERS_UNPACK_SIZE,
                0x01,  // unpack_sizes[0] = 1
                K_END, // unpack_info end
                K_SUB_STREAMS_INFO,
                K_END, // sub_streams_info end
                K_END, // streams_info end
                K_FILES_INFO,
                0x01,  // num_files = 1
                K_END, // files_info props end
                K_END, // header end
            ];
            let bytes = raw_7z_with_packed(&[0xAA, 0xBB], &nh);
            for result in [decode_err(&bytes), decode_unlimited(&bytes)] {
                let err = result.expect_err("a coder must have exactly one output");
                assert!(
                    err.contains("exactly one output stream"),
                    "{outs} outputs: {err}"
                );
            }
        }
    }
}

mod solid_compress {
    #[cfg(feature = "compress")]
    use sevenz_turbo::*;

    #[cfg(feature = "compress")]
    type Sources = Vec<SourceReader<std::io::Cursor<Vec<u8>>>>;

    /// The entries of issue 157: a directory and an empty file between files with data.
    #[cfg(feature = "compress")]
    fn mixed_entries() -> (Vec<ArchiveEntry>, Sources) {
        use std::io::Cursor;

        let entries = vec![
            ArchiveEntry::new_file("a.txt"),
            ArchiveEntry::new_directory("dir"),
            ArchiveEntry::new_file("b.txt"),
            ArchiveEntry::new_file("empty.txt"),
        ];
        let readers = [&b"AAAAA"[..], b"", b"BBBBB", b""]
            .iter()
            .map(|data| SourceReader::new(Cursor::new(data.to_vec())))
            .collect();
        (entries, readers)
    }

    /// Every entry of an archive as `(name, contents)`, in order.
    #[cfg(feature = "compress")]
    fn read_entries(archive: Vec<u8>) -> Vec<(String, Vec<u8>)> {
        let mut reader =
            ArchiveReader::new(std::io::Cursor::new(archive), Password::empty()).unwrap();
        let mut entries = Vec::new();
        reader
            .for_each_entries(|entry, data| {
                let mut buffer = Vec::new();
                data.read_to_end(&mut buffer)?;
                entries.push((entry.name.clone(), buffer));
                Ok(true)
            })
            .unwrap();
        entries
    }

    #[cfg(feature = "compress")]
    fn expected_mixed_entries() -> Vec<(String, Vec<u8>)> {
        vec![
            ("a.txt".to_string(), b"AAAAA".to_vec()),
            ("dir".to_string(), Vec::new()),
            ("b.txt".to_string(), b"BBBBB".to_vec()),
            ("empty.txt".to_string(), Vec::new()),
        ]
    }

    #[cfg(feature = "compress")]
    #[test]
    fn push_archive_entries_with_a_directory_round_trips() {
        let (entries, readers) = mixed_entries();
        let mut writer = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
        writer.push_archive_entries(entries, readers).unwrap();
        let archive = writer.finish().unwrap().into_inner();

        assert_eq!(read_entries(archive), expected_mixed_entries());
    }

    #[cfg(feature = "compress")]
    #[test]
    fn prepare_block_with_a_directory_round_trips() {
        use std::sync::Arc;

        let (entries, readers) = mixed_entries();
        let methods = Arc::new(vec![EncoderConfiguration::new(EncoderMethod::LZMA2)]);
        let block = prepare_block(methods, entries, readers).unwrap();
        let mut writer = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
        writer.push_prepared_block(block).unwrap();
        let archive = writer.finish().unwrap().into_inner();

        assert_eq!(read_entries(archive), expected_mixed_entries());
    }

    #[cfg(feature = "compress")]
    #[test]
    fn push_archive_entries_rejects_data_for_a_directory() {
        use std::io::Cursor;

        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        let result = writer.push_archive_entries(
            vec![
                ArchiveEntry::new_file("a.txt"),
                ArchiveEntry::new_directory("dir"),
            ],
            vec![
                SourceReader::new(Cursor::new(b"AAAAA".to_vec())),
                SourceReader::new(Cursor::new(b"DIR".to_vec())),
            ],
        );

        assert!(result.is_err());
    }
}

#[cfg(feature = "compress")]
mod large_size {
    //! Regression tests for archive sizes above 4 GiB, which wrap on 32-bit targets.

    use std::io::{self, Cursor, Read, Seek, SeekFrom};

    use sevenz_turbo::{
        Archive, ArchiveEntry, ArchiveReader, ArchiveWriter, BlockDecoder, EncoderMethod, Password,
    };

    const SEVEN_Z_SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

    const K_END: u8 = 0x00;
    const K_HEADER: u8 = 0x01;
    const K_MAIN_STREAMS_INFO: u8 = 0x04;
    const K_FILES_INFO: u8 = 0x05;
    const K_PACK_INFO: u8 = 0x06;
    const K_UNPACK_INFO: u8 = 0x07;
    const K_SUB_STREAMS_INFO: u8 = 0x08;
    const K_SIZE: u8 = 0x09;
    const K_CRC: u8 = 0x0A;
    const K_FOLDER: u8 = 0x0B;
    const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
    const K_NUM_UNPACK_STREAM: u8 = 0x0D;
    const K_ENCODED_HEADER: u8 = 0x17;

    const MIB: u64 = 1 << 20;

    /// Wraps to 1 MiB when truncated to 32 bits.
    const LARGE_SIZE: u64 = (1 << 32) + MIB;

    const READ_LEN: usize = 2 << 20;

    fn write_number(out: &mut Vec<u8>, value: u64) {
        let mut first: u8 = 0;
        let mut mask: u8 = 0x80;
        let mut low = Vec::new();
        let mut i = 0u32;
        while i < 8 {
            if value < (1u64 << (7 * (i + 1))) {
                first |= (value >> (8 * i)) as u8;
                break;
            }
            first |= mask;
            mask >>= 1;
            low.push((value >> (8 * i)) as u8);
            i += 1;
        }
        out.push(first);
        out.extend_from_slice(&low);
    }

    fn simple_folder(coder_id: &[u8], properties: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        write_number(&mut f, 1); // num_coders
        let mut flags = coder_id.len() as u8;
        if !properties.is_empty() {
            flags |= 0x20;
        }
        f.push(flags);
        f.extend_from_slice(coder_id);
        if !properties.is_empty() {
            write_number(&mut f, properties.len() as u64);
            f.extend_from_slice(properties);
        }
        f
    }

    fn streams_info(pack_sizes: &[u64], folder: &[u8], unpack_size: u64) -> Vec<u8> {
        let mut h = vec![K_PACK_INFO];
        write_number(&mut h, 0); // pack_pos
        write_number(&mut h, pack_sizes.len() as u64);
        h.push(K_SIZE);
        for size in pack_sizes {
            write_number(&mut h, *size);
        }
        h.push(K_END);

        h.push(K_UNPACK_INFO);
        h.push(K_FOLDER);
        write_number(&mut h, 1); // num_blocks
        h.push(0); // external
        h.extend_from_slice(folder);
        h.push(K_CODERS_UNPACK_SIZE);
        write_number(&mut h, unpack_size);
        h.push(K_END);
        h
    }

    fn header(
        pack_sizes: &[u64],
        folder: &[u8],
        unpack_size: u64,
        file_sizes: &[u64],
        file_crc: Option<u32>,
    ) -> Vec<u8> {
        let mut h = vec![K_HEADER, K_MAIN_STREAMS_INFO];
        h.extend_from_slice(&streams_info(pack_sizes, folder, unpack_size));

        h.push(K_SUB_STREAMS_INFO);
        if file_sizes.len() > 1 {
            h.push(K_NUM_UNPACK_STREAM);
            write_number(&mut h, file_sizes.len() as u64);
            h.push(K_SIZE);
            for size in &file_sizes[..file_sizes.len() - 1] {
                write_number(&mut h, *size);
            }
        }
        if let Some(crc) = file_crc {
            h.push(K_CRC);
            h.push(1); // all defined
            h.extend_from_slice(&crc.to_le_bytes());
        }
        h.push(K_END);
        h.push(K_END); // main streams info

        h.push(K_FILES_INFO);
        write_number(&mut h, file_sizes.len() as u64);
        h.push(K_END);
        h.push(K_END); // header
        h
    }

    fn signature_header(pack_len: u64, next_header: &[u8]) -> Vec<u8> {
        let mut start = [0u8; 20];
        start[0..8].copy_from_slice(&pack_len.to_le_bytes());
        start[8..16].copy_from_slice(&(next_header.len() as u64).to_le_bytes());
        start[16..20].copy_from_slice(&lzma_turbo::crc::crc32(next_header).to_le_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(&SEVEN_Z_SIGNATURE);
        out.extend_from_slice(&[0x00, 0x04]);
        out.extend_from_slice(&lzma_turbo::crc::crc32(&start).to_le_bytes());
        out.extend_from_slice(&start);
        out
    }

    struct SparseSource {
        prefix: Vec<u8>,
        zeros: u64,
        suffix: Vec<u8>,
        pos: u64,
    }

    impl SparseSource {
        fn len(&self) -> u64 {
            self.prefix.len() as u64 + self.zeros + self.suffix.len() as u64
        }
    }

    impl Read for SparseSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let prefix_len = self.prefix.len() as u64;
            let suffix_start = prefix_len + self.zeros;
            if self.pos >= self.len() || buf.is_empty() {
                return Ok(0);
            }
            let n = if self.pos < prefix_len {
                let start = self.pos as usize;
                let n = buf.len().min(self.prefix.len() - start);
                buf[..n].copy_from_slice(&self.prefix[start..start + n]);
                n
            } else if self.pos < suffix_start {
                let n = buf
                    .len()
                    .min(usize::try_from(suffix_start - self.pos).unwrap_or(usize::MAX));
                buf[..n].fill(0);
                n
            } else {
                let start = (self.pos - suffix_start) as usize;
                let n = buf.len().min(self.suffix.len() - start);
                buf[..n].copy_from_slice(&self.suffix[start..start + n]);
                n
            };
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for SparseSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let new_pos = match pos {
                SeekFrom::Start(p) => Some(p),
                SeekFrom::End(d) => self.len().checked_add_signed(d),
                SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            };
            self.pos = new_pos
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"))?;
            Ok(self.pos)
        }
    }

    fn read_first_entry(make_source: impl Fn() -> SparseSource) -> (Vec<u8>, Vec<u8>) {
        let read_prefix = |reader: &mut dyn Read| -> Result<Vec<u8>, sevenz_turbo::Error> {
            let mut data = vec![0xFF; READ_LEN];
            reader.read_exact(&mut data)?;
            Ok(data)
        };

        let mut via_reader = None;
        let mut reader = ArchiveReader::new(make_source(), Password::empty()).unwrap();
        reader
            .for_each_entries(|_, r| {
                via_reader = Some(read_prefix(r)?);
                Ok(false)
            })
            .expect("ArchiveReader must decode past the wrapped size");

        let mut via_block = None;
        let mut source = make_source();
        let archive = Archive::read(&mut source, &Password::empty()).unwrap();
        BlockDecoder::new(1, 0, &archive, &Password::empty(), &mut source)
            .for_each_entries(&mut |_, r| {
                via_block = Some(read_prefix(r)?);
                Ok(false)
            })
            .expect("BlockDecoder must decode past the wrapped size");

        (via_reader.unwrap(), via_block.unwrap())
    }

    #[test]
    fn copy_block_larger_than_4_gib_decodes_past_wrapped_size() {
        let make_source = || {
            let next_header = header(
                &[LARGE_SIZE],
                &simple_folder(EncoderMethod::ID_COPY, &[]),
                LARGE_SIZE,
                &[LARGE_SIZE],
                Some(0),
            );
            SparseSource {
                prefix: signature_header(LARGE_SIZE, &next_header),
                zeros: LARGE_SIZE,
                suffix: next_header,
                pos: 0,
            }
        };

        let (via_reader, via_block) = read_first_entry(make_source);
        assert!(via_reader.iter().all(|b| *b == 0));
        assert!(via_block.iter().all(|b| *b == 0));
    }

    #[test]
    fn lzma_block_larger_than_4_gib_decodes_past_wrapped_size() {
        let content = vec![0x5Au8; READ_LEN];

        let mut bytes = Vec::new();
        {
            let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
            writer.set_content_methods(vec![EncoderMethod::LZMA.into()]);
            writer
                .push_archive_entry(ArchiveEntry::new_file("data"), Some(content.as_slice()))
                .unwrap();
            writer.finish().unwrap();
        }
        let archive =
            Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap();
        let pack_start = 32 + archive.pack_pos() as usize;
        let packed = bytes[pack_start..pack_start + archive.pack_sizes()[0] as usize].to_vec();
        let properties = archive.blocks[0].coders[0].properties().to_vec();

        let make_source = || {
            let next_header = header(
                &[packed.len() as u64],
                &simple_folder(EncoderMethod::ID_LZMA, &properties),
                LARGE_SIZE,
                &[READ_LEN as u64, LARGE_SIZE - READ_LEN as u64],
                None,
            );
            let mut prefix = signature_header(packed.len() as u64, &next_header);
            prefix.extend_from_slice(&packed);
            SparseSource {
                prefix,
                zeros: 0,
                suffix: next_header,
                pos: 0,
            }
        };

        let (via_reader, via_block) = read_first_entry(make_source);
        assert_eq!(via_reader, content);
        assert_eq!(via_block, content);
    }

    #[test]
    fn bcj2_block_larger_than_4_gib_decodes_past_wrapped_size() {
        let mut folder = Vec::new();
        write_number(&mut folder, 1); // num_coders
        folder.push(0x10 | EncoderMethod::ID_BCJ2.len() as u8);
        folder.extend_from_slice(EncoderMethod::ID_BCJ2);
        write_number(&mut folder, 4); // num_in_streams
        write_number(&mut folder, 1); // num_out_streams
        for index in 0..4 {
            write_number(&mut folder, index); // packed stream index
        }
        let range_coder = [0u8; 5];

        let make_source = || {
            let pack_sizes = [LARGE_SIZE, 0, 0, range_coder.len() as u64];
            let next_header = header(
                &pack_sizes,
                &folder,
                LARGE_SIZE,
                &[READ_LEN as u64, LARGE_SIZE - READ_LEN as u64],
                None,
            );
            let mut suffix = range_coder.to_vec();
            suffix.extend_from_slice(&next_header);
            SparseSource {
                prefix: signature_header(pack_sizes.iter().sum(), &next_header),
                zeros: LARGE_SIZE,
                suffix,
                pos: 0,
            }
        };

        let (via_reader, via_block) = read_first_entry(make_source);
        assert!(via_reader.iter().all(|b| *b == 0));
        assert!(via_block.iter().all(|b| *b == 0));
    }

    #[test]
    fn encoded_header_larger_than_4_gib_is_not_truncated() {
        let plain_header = [K_HEADER, K_END];
        let pack_size = plain_header.len() as u64;

        let mut next_header = vec![K_ENCODED_HEADER];
        next_header.extend_from_slice(&streams_info(
            &[pack_size],
            &simple_folder(EncoderMethod::ID_COPY, &[]),
            (1 << 32) + pack_size,
        ));
        next_header.push(K_END);

        let mut bytes = signature_header(pack_size, &next_header);
        bytes.extend_from_slice(&plain_header);
        bytes.extend_from_slice(&next_header);

        assert!(Archive::read(&mut Cursor::new(bytes), &Password::empty()).is_err());
    }
}
