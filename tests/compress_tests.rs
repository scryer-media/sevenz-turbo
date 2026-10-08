#[cfg(feature = "compress")]
use std::io::{Cursor, Read};
#[cfg(all(feature = "compress", feature = "util"))]
use std::{
    fs::File,
    hash::{Hash, Hasher},
};

#[cfg(feature = "compress")]
use sevenz_turbo::encoder_options::*;
#[cfg(feature = "compress")]
use sevenz_turbo::*;
#[cfg(all(feature = "compress", feature = "util"))]
use tempfile::*;

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_empty_file() {
    let temp_dir = tempdir().unwrap();
    let source = temp_dir.path().join("empty.txt");
    File::create(&source).unwrap();
    let dest = temp_dir.path().join("empty.7z");
    compress_to_path(source, &dest).expect("compress ok");

    let decompress_dest = temp_dir.path().join("decompress");
    decompress_file(dest, &decompress_dest).expect("decompress ok");
    assert!(decompress_dest.exists());
    let decompress_file = decompress_dest.join("empty.txt");
    assert!(decompress_file.exists());

    assert_eq!(std::fs::read_to_string(&decompress_file).unwrap(), "");
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_one_file_with_content() {
    let temp_dir = tempdir().unwrap();
    let source = temp_dir.path().join("file1.txt");
    std::fs::write(&source, "file1 with content").unwrap();
    let dest = temp_dir.path().join("file1.7z");
    compress_to_path(source, &dest).expect("compress ok");

    let decompress_dest = temp_dir.path().join("decompress");
    decompress_file(dest, &decompress_dest).expect("decompress ok");
    assert!(decompress_dest.exists());
    let decompress_file = decompress_dest.join("file1.txt");
    assert!(decompress_file.exists());

    assert_eq!(
        std::fs::read_to_string(&decompress_file).unwrap(),
        "file1 with content"
    );
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_empty_folder() {
    let temp_dir = tempdir().unwrap();
    let folder = temp_dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let dest = temp_dir.path().join("folder.7z");
    compress_to_path(&folder, &dest).expect("compress ok");

    let decompress_dest = temp_dir.path().join("decompress");
    decompress_file(dest, &decompress_dest).expect("decompress ok");
    assert!(decompress_dest.exists());
    assert!(decompress_dest.read_dir().unwrap().next().is_none());
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_folder_with_one_file() {
    let temp_dir = tempdir().unwrap();
    let folder = temp_dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("file1.txt"), "file1 with content").unwrap();
    let dest = temp_dir.path().join("folder.7z");
    compress_to_path(&folder, &dest).expect("compress ok");

    let decompress_dest = temp_dir.path().join("decompress");
    decompress_file(dest, &decompress_dest).expect("decompress ok");
    assert!(decompress_dest.exists());
    let decompress_file = decompress_dest.join("file1.txt");
    assert!(decompress_file.exists());

    assert_eq!(
        std::fs::read_to_string(&decompress_file).unwrap(),
        "file1 with content"
    );
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_folder_with_multi_file() {
    let temp_dir = tempdir().unwrap();
    let folder = temp_dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let mut files = Vec::with_capacity(100);
    let mut contents = Vec::with_capacity(100);
    for i in 1..=100 {
        let name = format!("file{i}.txt");
        let content = format!("file{i} with content");
        std::fs::write(folder.join(&name), &content).unwrap();
        files.push(name);
        contents.push(content);
    }
    let dest = temp_dir.path().join("folder.7z");
    compress_to_path(&folder, &dest).expect("compress ok");

    let decompress_dest = temp_dir.path().join("decompress");
    decompress_file(dest, &decompress_dest).expect("decompress ok");
    assert!(decompress_dest.exists());
    for i in 0..files.len() {
        let name = &files[i];
        let content = &contents[i];
        let decompress_file = decompress_dest.join(name);
        assert!(decompress_file.exists());
        assert_eq!(&std::fs::read_to_string(&decompress_file).unwrap(), content);
    }
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_folder_with_nested_folder() {
    let temp_dir = tempdir().unwrap();
    let folder = temp_dir.path().join("folder");
    let inner = folder.join("a/b/c");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("file1.txt"), "file1 with content").unwrap();
    let dest = temp_dir.path().join("folder.7z");
    compress_to_path(&folder, &dest).expect("compress ok");

    let decompress_dest = temp_dir.path().join("decompress");
    decompress_file(dest, &decompress_dest).expect("decompress ok");
    assert!(decompress_dest.exists());
    let decompress_file = decompress_dest.join("a/b/c/file1.txt");
    assert!(decompress_file.exists());

    assert_eq!(
        std::fs::read_to_string(&decompress_file).unwrap(),
        "file1 with content"
    );
}

#[cfg(all(feature = "compress", feature = "util", feature = "aes256"))]
#[test]
fn compress_one_file_with_random_content_encrypted() {
    use rand::prelude::*;
    for _ in 0..10 {
        let temp_dir = tempdir().unwrap();
        let source = temp_dir.path().join("file1.txt");
        let mut rng = rand::rng();
        let mut content = String::with_capacity(rng.random_range(1..10240));

        for _ in 0..content.capacity() {
            let c = rng.random_range(' '..'~');
            content.push(c);
        }
        std::fs::write(&source, &content).unwrap();
        let dest = temp_dir.path().join("file1.7z");

        compress_to_path_encrypted(source, &dest, "rust".into()).expect("compress ok");

        let decompress_dest = temp_dir.path().join("decompress");
        decompress_file_with_password(dest, &decompress_dest, "rust".into())
            .expect("decompress ok");
        assert!(decompress_dest.exists());
        let decompress_file = decompress_dest.join("file1.txt");
        assert!(decompress_file.exists());

        assert_eq!(std::fs::read_to_string(&decompress_file).unwrap(), content);
    }
}

#[cfg(all(feature = "compress", feature = "util"))]
fn test_compression_method(methods: &[EncoderConfiguration]) {
    let mut content = Vec::new();
    File::open("tests/resources/decompress_x86.exe")
        .unwrap()
        .read_to_end(&mut content)
        .unwrap();

    let mut bytes = Vec::new();

    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
        let file = ArchiveEntry::new_file("data/decompress_x86.exe");
        let directory = ArchiveEntry::new_directory("data");

        writer.set_content_methods(methods.to_vec());
        writer
            .push_archive_entry(file, Some(content.as_slice()))
            .unwrap();
        writer.push_archive_entry::<&[u8]>(directory, None).unwrap();
        writer.finish().unwrap();
    }

    let mut reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();

    assert_eq!(reader.archive().files.len(), 2);

    reader
        .archive()
        .files
        .iter()
        .filter(|file| !file.is_directory)
        .for_each(|file| {
            let mut file_methods = Vec::<EncoderMethod>::new();
            reader
                .file_compression_methods(file.name(), &mut file_methods)
                .expect("can't read compression method");

            for (file_method, method) in file_methods.iter().zip(methods) {
                assert_eq!(file_method.name(), method.method.name());
            }
        });

    assert!(
        reader
            .archive()
            .files
            .iter()
            .any(|file| file.name() == "data")
    );
    assert!(
        reader
            .archive()
            .files
            .iter()
            .any(|file| file.name() == "data/decompress_x86.exe")
    );

    let data = reader.read_file("data/decompress_x86.exe").unwrap();

    fn hash(data: &[u8]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        data.hash(&mut hasher);
        hasher.finish()
    }

    assert_eq!(hash(&content), hash(&data));
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_copy_algorithm() {
    test_compression_method(&[EncoderMethod::COPY.into()]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_delta_lzma_algorithm() {
    for i in 1..=4 {
        test_compression_method(&[
            EncoderMethod::LZMA.into(),
            DeltaOptions::from_distance(i).into(),
        ]);
    }
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_delta_lzma2_algorithm() {
    for i in 1..=4 {
        test_compression_method(&[
            EncoderMethod::LZMA2.into(),
            DeltaOptions::from_distance(i).into(),
        ]);
    }
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_x86_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_X86_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_arm_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_ARM_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_arm64_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_ARM64_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_arm_thumb_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_ARM_THUMB_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_ia64_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_IA64_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_sparc_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_SPARC_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_ppc_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_PPC_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_bcj_riscv_lzma2_algorithm() {
    test_compression_method(&[
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ_RISCV_FILTER.into(),
    ]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_lzma_algorithm() {
    test_compression_method(&[EncoderMethod::LZMA.into()]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_with_lzma2_algorithm() {
    test_compression_method(&[EncoderMethod::LZMA2.into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "ppmd"))]
#[test]
fn compress_with_ppmd_algorithm() {
    test_compression_method(&[EncoderMethod::PPMD.into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "brotli"))]
#[test]
fn compress_with_brotli_standard_algorithm() {
    test_compression_method(&[BrotliOptions::default().with_skippable_frame_size(0).into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "brotli"))]
#[test]
fn compress_with_brotli_skippable_algorithm() {
    test_compression_method(&[BrotliOptions::default()
        .with_skippable_frame_size(64 * 1024)
        .into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "bzip2"))]
#[test]
fn compress_with_bzip2_algorithm() {
    test_compression_method(&[EncoderMethod::BZIP2.into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "deflate"))]
#[test]
fn compress_with_deflate_algorithm() {
    test_compression_method(&[EncoderMethod::DEFLATE.into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "lz4"))]
#[test]
fn compress_with_lz4_algorithm() {
    test_compression_method(&[Lz4Options::default().with_skippable_frame_size(0).into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "lz4"))]
#[test]
fn compress_with_lz4_skippable_algorithm() {
    test_compression_method(&[Lz4Options::default()
        .with_skippable_frame_size(128 * 1024)
        .into()]);
}

#[cfg(all(feature = "compress", feature = "util", feature = "lz4"))]
#[test]
fn compress_with_zstd_algorithm() {
    test_compression_method(&[EncoderMethod::ZSTD.into()]);
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn anti_item_roundtrip() {
    let mut bytes = Vec::new();
    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
        let mut entry = ArchiveEntry::new_file("deleted.txt");
        entry.is_anti_item = true;
        writer.push_archive_entry::<&[u8]>(entry, None).unwrap();
        writer.finish().unwrap();
    }

    let reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    assert_eq!(reader.archive().files.len(), 1);
    let entry = &reader.archive().files[0];
    assert_eq!(entry.name(), "deleted.txt");
    assert!(entry.is_anti_item(), "entry should be an anti-item");
}

#[cfg(all(feature = "compress", feature = "aes256"))]
#[test]
fn encrypted_file_header_requires_password_to_read() {
    use std::io::Cursor;

    use sevenz_turbo::{
        Archive, ArchiveEntry, ArchiveWriter, Password,
        encoder_options::{AesEncoderOptions, Lzma2Options},
    };

    let content = std::fs::read("tests/resources/apache2.txt").unwrap();

    let mut bytes = Vec::new();

    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
        writer.set_content_methods(vec![
            AesEncoderOptions::new(Password::new("test")).into(),
            Lzma2Options::default().into(),
        ]);
        let entry = ArchiveEntry::new_file("apache2.txt");
        writer
            .push_archive_entry(entry, Some(content.as_slice()))
            .unwrap();
        writer.finish().unwrap();
    }

    let result = Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty());
    assert!(
        result.is_err(),
        "Reading an encrypted archive header without a password should not be possible"
    );
}

#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn compress_path_does_not_emit_root_dir_entry() {
    let temp_dir = tempdir().unwrap();
    let folder = temp_dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("file1.txt"), "hello").unwrap();
    std::fs::create_dir(folder.join("sub")).unwrap();
    std::fs::write(folder.join("sub").join("file2.txt"), "world").unwrap();

    let dest = temp_dir.path().join("folder.7z");
    compress_to_path(&folder, &dest).expect("compress ok");

    let reader = ArchiveReader::new(File::open(&dest).unwrap(), Password::empty()).unwrap();
    let names: Vec<String> = reader
        .archive()
        .files
        .iter()
        .map(|f| f.name().replace('\\', "/"))
        .collect();

    assert!(
        !names.iter().any(|n| n.is_empty() || n == "folder"),
        "root directory should not appear as an entry, got: {names:?}"
    );
    assert!(names.iter().any(|n| n == "file1.txt"));
    assert!(names.iter().any(|n| n == "sub"));
    assert!(names.iter().any(|n| n == "sub/file2.txt"));
}

/// A source that hands out its bytes in small, uneven reads, so that the
/// coder chain sees many writes and never one large one.
#[cfg(feature = "compress")]
struct Trickle<'a> {
    data: &'a [u8],
    pos: usize,
    step: usize,
}

#[cfg(feature = "compress")]
impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.step).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        self.step = (self.step * 7 + 3) % 4099 + 1;
        Ok(n)
    }
}

/// Entries larger than the encoder's input chunk, fed in small reads, come
/// back intact through each LZMA coder: raw LZMA, solid LZMA2, and LZMA2 in
/// parallel blocks.
#[cfg(feature = "compress")]
#[test]
fn large_entries_stream_through_the_lzma_coders() {
    let content: Vec<u8> = (0..(6usize << 20))
        .map(|i| {
            ((i % 253) as u8)
                .wrapping_mul(31)
                .wrapping_add((i >> 12) as u8)
        })
        .collect();

    let mut lzma2_mt = Lzma2Options::from_level_mt(3, 3, 1 << 20);
    lzma2_mt.set_dictionary_size(1 << 20);
    let configs: [EncoderConfiguration; 3] = [
        LzmaOptions::from_level(3).into(),
        Lzma2Options::from_level(3).into(),
        lzma2_mt.into(),
    ];

    for config in configs {
        let name = config.method.name();
        let mut bytes = Vec::new();
        {
            let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
            writer.set_content_methods(vec![config]);
            let source = Trickle {
                data: &content,
                pos: 0,
                step: 1,
            };
            writer
                .push_archive_entry(ArchiveEntry::new_file("large.bin"), Some(source))
                .unwrap();
            writer.finish().unwrap();
        }
        assert!(bytes.len() < content.len() / 4, "{name}: did not compress");

        let mut reader =
            ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
        let back = reader.read_file("large.bin").unwrap();
        assert!(back == content, "{name}: round trip differs");
    }
}

/// A dictionary that is not one the LZMA2 property byte can name exactly is
/// rounded up, never down: the decoder gets at least the window the encoder
/// used, and an entry that leans on all of it comes back.
#[cfg(feature = "compress")]
#[test]
fn a_non_canonical_lzma2_dictionary_round_trips() {
    // 5 MiB sits between the table's 4 MiB and 6 MiB. The entry is a block
    // that repeats 4.5 MiB later, so every match reaches past 4 MiB.
    let block: Vec<u8> = (0..(1usize << 19))
        .map(|i| ((i * 2654435761usize) >> 13) as u8)
        .collect();
    let mut content = block.clone();
    content.extend((0..(4usize << 20)).map(|i| (i % 7) as u8));
    content.extend_from_slice(&block);

    let mut options = Lzma2Options::from_level(3);
    options.set_dictionary_size(5 << 20);
    let mut bytes = Vec::new();
    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
        writer.set_content_methods(vec![options.into()]);
        writer
            .push_archive_entry(
                ArchiveEntry::new_file("far.bin"),
                Some(Cursor::new(content.as_slice())),
            )
            .unwrap();
        writer.finish().unwrap();
    }
    let mut reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    assert!(reader.read_file("far.bin").unwrap() == content);
}

/// A chunk size of zero means "the dictionary", as it always has, and still
/// compresses in parallel blocks rather than being refused or going solid.
#[cfg(feature = "compress")]
#[test]
fn a_zero_chunk_size_means_the_dictionary() {
    let content: Vec<u8> = (0..(3usize << 20)).map(|i| (i % 251) as u8).collect();
    let mut bytes = Vec::new();
    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
        writer.set_content_methods(vec![Lzma2Options::from_level_mt(1, 2, 0).into()]);
        writer
            .push_archive_entry(
                ArchiveEntry::new_file("blocks.bin"),
                Some(Cursor::new(content.as_slice())),
            )
            .unwrap();
        writer.finish().unwrap();
    }
    let mut reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    assert!(reader.read_file("blocks.bin").unwrap() == content);
}

// ---------------------------------------------------------------------------
// Per-folder dictionary sizing
// ---------------------------------------------------------------------------

/// The dictionary an LZMA2 coder record names.
#[cfg(all(feature = "compress", feature = "util"))]
fn lzma2_dictionary(archive: &Archive, block: usize) -> u64 {
    let coder = archive.blocks[block]
        .coders
        .iter()
        .find(|c| c.encoder_method_id() == EncoderMethod::ID_LZMA2)
        .expect("an LZMA2 coder");
    let prop = u64::from(coder.properties()[0]);
    (2 | (prop & 1)) << (prop / 2 + 11)
}

/// The smallest LZMA2 dictionary at least `size` (and 4 KiB) long: what a
/// coder sized to `size` records.
#[cfg(all(feature = "compress", feature = "util"))]
fn lzma2_dictionary_for(size: u64) -> u64 {
    (0..40u64)
        .map(|prop| (2 | (prop & 1)) << (prop / 2 + 11))
        .find(|&dict| dict >= size.max(4096))
        .expect("in the table")
}

/// Deterministic text-like bytes, different per `seed`.
#[cfg(all(feature = "compress", feature = "util"))]
fn small_member(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let word = b'a' + (state % 7) as u8;
        out.extend(std::iter::repeat_n(word, 1 + (state >> 8) as usize % 6));
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// A tree of `count` small files, plus one larger than `big` bytes.
#[cfg(all(feature = "compress", feature = "util"))]
fn small_tree(dir: &std::path::Path, count: usize, big: usize) -> Vec<(String, Vec<u8>)> {
    let mut members = Vec::new();
    for index in 0..count {
        members.push((
            format!("member{index:03}.txt"),
            small_member(512 + index * 977, index as u64),
        ));
    }
    members.push(("zz_big.txt".to_string(), small_member(big, 999)));
    for (name, bytes) in &members {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    members
}

/// Every member back out of `archive`, by name.
#[cfg(all(feature = "compress", feature = "util"))]
fn extract_all(
    archive: &std::path::Path,
    password: Password,
) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let mut reader = ArchiveReader::open(archive, password).unwrap();
    reader
        .for_each_entries(|entry, rd| {
            let mut buf = Vec::new();
            rd.read_to_end(&mut buf)?;
            out.insert(entry.name().to_string(), buf);
            Ok(true)
        })
        .unwrap();
    out
}

/// A non-solid archive of small files codes each folder with a dictionary its
/// own size, as 7-Zip does through `reduceSize`, instead of building the
/// level's whole dictionary for every file. A file larger than the dictionary
/// keeps it. Every file round-trips.
#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn non_solid_small_files_get_a_dictionary_their_own_size() {
    let temp_dir = tempdir().unwrap();
    let source = temp_dir.path().join("tree");
    std::fs::create_dir(&source).unwrap();
    let mut options = Lzma2Options::from_level(5);
    options.set_dictionary_size(1 << 16);
    let members = small_tree(&source, 40, 100_000);

    let dest = temp_dir.path().join("tree.7z");
    let mut writer = ArchiveWriter::create(&dest).unwrap();
    writer.set_content_methods(vec![options.into()]);
    writer
        .push_source_path_non_solid(&source, |_| true)
        .unwrap();
    writer.finish().unwrap();

    let archive = Archive::open(&dest).unwrap();
    let files: Vec<&ArchiveEntry> = archive.files.iter().filter(|f| f.has_stream).collect();
    assert_eq!(files.len(), members.len());
    assert_eq!(archive.blocks.len(), members.len());
    for (block, file) in files.iter().enumerate() {
        let want = lzma2_dictionary_for(file.size).min(1 << 16);
        assert_eq!(
            lzma2_dictionary(&archive, block),
            want,
            "{} ({} bytes)",
            file.name,
            file.size
        );
    }
    // The big member is past the 64 KiB dictionary, so it keeps it.
    let big = files.iter().position(|f| f.name == "zz_big.txt").unwrap();
    assert_eq!(lzma2_dictionary(&archive, big), 1 << 16);

    let back = extract_all(&dest, Password::empty());
    for (name, bytes) in &members {
        assert!(back[name] == *bytes, "{name} did not round-trip");
    }
}

/// Writes one solid block of `parts` with `options`, declaring each part's
/// size or not, and returns the archive.
#[cfg(all(feature = "compress", feature = "util"))]
fn solid_archive(parts: &[Vec<u8>], options: Lzma2Options, declared: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
        writer.set_content_methods(vec![options.into()]);
        let entries = parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut entry = ArchiveEntry::new_file(&format!("part{i}"));
                if declared {
                    entry.size = p.len() as u64;
                }
                entry
            })
            .collect();
        let readers = parts
            .iter()
            .map(|p| SourceReader::new(Cursor::new(p.as_slice())))
            .collect();
        writer.push_archive_entries(entries, readers).unwrap();
        writer.finish().unwrap();
    }
    let mut reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    for (i, p) in parts.iter().enumerate() {
        assert!(
            reader.read_file(&format!("part{i}")).unwrap() == *p,
            "part{i}"
        );
    }
    bytes
}

/// A solid block is sized by the sum of its entries' declared sizes. With
/// none declared, the writer reads up to 1 MiB ahead: a block that ends within
/// it is sized exactly, and one that does not keeps the configured dictionary.
#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn a_solid_block_is_sized_by_its_entries_or_by_reading_ahead() {
    let block_dict = |bytes: &[u8]| {
        let archive = Archive::read(&mut Cursor::new(bytes), &Password::empty()).unwrap();
        lzma2_dictionary(&archive, 0)
    };

    // Small: sized whether or not the entries say.
    let small: Vec<Vec<u8>> = (0..3)
        .map(|i| small_member(3000 + i * 100, i as u64))
        .collect();
    let small_total: u64 = small.iter().map(|p| p.len() as u64).sum();
    for declared in [true, false] {
        let bytes = solid_archive(&small, Lzma2Options::from_level(5), declared);
        assert_eq!(
            block_dict(&bytes),
            lzma2_dictionary_for(small_total),
            "declared: {declared}"
        );
    }

    // Past the read-ahead but inside a 4 MiB dictionary: only a declared size
    // shrinks it.
    let large: Vec<Vec<u8>> = (0..2).map(|i| small_member(700_000, 10 + i)).collect();
    let large_total: u64 = large.iter().map(|p| p.len() as u64).sum();
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(4 << 20);
    let declared = solid_archive(&large, options.clone(), true);
    assert_eq!(block_dict(&declared), lzma2_dictionary_for(large_total));
    let unknown = solid_archive(&large, options, false);
    assert_eq!(block_dict(&unknown), 4 << 20);
}

/// A block built off the writer by `prepare_block` is sized the same way.
#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn a_prepared_block_is_sized_by_its_entries() {
    let part = small_member(10_000, 5);
    let mut entry = ArchiveEntry::new_file("prepared.txt");
    entry.size = part.len() as u64;
    let block = prepare_block(
        std::sync::Arc::new(vec![Lzma2Options::from_level(5).into()]),
        vec![entry],
        vec![SourceReader::new(Cursor::new(part.as_slice()))],
    )
    .unwrap();
    let mut bytes = Vec::new();
    {
        let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
        writer.push_prepared_block(block).unwrap();
        writer.finish().unwrap();
    }
    let archive = Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap();
    assert_eq!(lzma2_dictionary(&archive, 0), lzma2_dictionary_for(10_000));
    let mut reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    assert!(reader.read_file("prepared.txt").unwrap() == part);
}

/// `ArchiveEntry::from_path` records the file's length, which is what lets
/// the path-based writers size each folder.
#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn from_path_records_the_file_length() {
    let temp_dir = tempdir().unwrap();
    let file = temp_dir.path().join("five.txt");
    std::fs::write(&file, b"12345").unwrap();
    assert_eq!(ArchiveEntry::from_path(&file, "five.txt".into()).size, 5);
    assert_eq!(
        ArchiveEntry::from_path(temp_dir.path(), "dir".into()).size,
        0
    );
}

/// The per-folder dictionaries are what 7-Zip itself accepts: `7zz t` passes
/// the non-solid small-file archive, plain and encrypted. Skips when `7zz` is
/// not on `PATH`, as the CI runners have none.
#[cfg(all(feature = "compress", feature = "util", feature = "aes256"))]
#[test]
fn sevenzip_tests_archives_with_per_folder_dictionaries() {
    let have_7zz = std::process::Command::new("7zz")
        .arg("i")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !have_7zz {
        eprintln!("skipping: 7zz is not on PATH");
        return;
    }
    let temp_dir = tempdir().unwrap();
    let source = temp_dir.path().join("tree");
    std::fs::create_dir(&source).unwrap();
    let members = small_tree(&source, 24, 200_000);

    for password in [None, Some("pale-orchid")] {
        let dest = temp_dir
            .path()
            .join(format!("tree-{}.7z", password.is_some()));
        let mut writer = ArchiveWriter::create(&dest).unwrap();
        let mut methods = Vec::new();
        if let Some(password) = password {
            methods.push(AesEncoderOptions::new(Password::from(password)).into());
        }
        methods.push(Lzma2Options::from_level(5).into());
        writer.set_content_methods(methods);
        writer
            .push_source_path_non_solid(&source, |_| true)
            .unwrap();
        writer.finish().unwrap();

        let mut command = std::process::Command::new("7zz");
        command.args(["t", "-bso0", "-bsp0"]);
        if let Some(password) = password {
            command.arg(format!("-p{password}"));
        }
        let output = command.arg(&dest).output().unwrap();
        assert!(
            output.status.success(),
            "7zz t failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let back = extract_all(&dest, password.map_or_else(Password::empty, Password::from));
        assert_eq!(back.len(), members.len());
        for (name, bytes) in &members {
            assert!(back[name] == *bytes, "{name} did not round-trip");
        }
    }
}

/// An entry that does not say how large it is - `new_file`, or an empty file
/// from `from_path` - is sized by reading ahead, so an empty or tiny one does
/// not build the level's whole dictionary.
#[cfg(all(feature = "compress", feature = "util"))]
#[test]
fn an_undeclared_entry_is_sized_by_reading_ahead() {
    for content in [Vec::new(), small_member(2000, 3)] {
        let mut bytes = Vec::new();
        {
            let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes)).unwrap();
            writer
                .push_archive_entry(
                    ArchiveEntry::new_file("undeclared.txt"),
                    Some(Cursor::new(content.as_slice())),
                )
                .unwrap();
            writer.finish().unwrap();
        }
        let archive =
            Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::empty()).unwrap();
        assert_eq!(
            lzma2_dictionary(&archive, 0),
            lzma2_dictionary_for(content.len() as u64)
        );
        let mut reader =
            ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
        assert!(reader.read_file("undeclared.txt").unwrap() == content);
    }
}
