use std::io::{Read, Write};
use std::sync::Arc;

use super::*;

#[test]
fn bzip2_rejects_decoded_bytes_beyond_the_declared_folder_length() {
    let data: Vec<_> = (0..210_001).map(|i| (i * 31 + i / 251) as u8).collect();
    let mut encoder = bzip2_turbo::Encoder::new(
        Vec::new(),
        bzip2_turbo::EncoderOptions {
            block_size: Some(100_000),
            ..Default::default()
        },
    );
    encoder.write_all(&data).unwrap();
    let packed = encoder.finish().unwrap();
    let mut coder = Coder::single_stream();
    coder.set_id_size(3);
    coder
        .decompression_method_id_mut()
        .copy_from_slice(EncoderMethod::ID_BZIP2);
    for threads in [1, 2, 8] {
        for limit in [u64::MAX, 16 << 20] {
            let limits = ArchiveLimits::memory(limit);
            let control = Arc::new(Lzma2Control::new(threads));
            let mut opts = DecodeOptions::header(&limits);
            opts.threads = threads;
            opts.lzma2_control = Some(&control);
            opts.checksum_splits = &[u64::MAX];
            // The packed stream is valid, but it produces one byte more than
            // the folder declaration. Test the adapter before container CRCs.
            let mut decoder = add_decoder(
                packed.as_slice(),
                data.len() as u64 - 1,
                &coder,
                &Password::empty(),
                &opts,
            )
            .unwrap();
            assert!(
                decoder.read_to_end(&mut Vec::new()).is_err(),
                "accepted surplus decoded byte with {threads} threads and limit {limit}"
            );
        }
    }
}

#[test]
fn bzip2_workers_fold_container_checksums_at_file_boundaries() {
    let data: Vec<_> = (0..210_001).map(|i| (i * 31 + i / 251) as u8).collect();
    let mut encoder = bzip2_turbo::Encoder::new(
        Vec::new(),
        bzip2_turbo::EncoderOptions {
            block_size: Some(100_000),
            ..Default::default()
        },
    );
    encoder.write_all(&data).unwrap();
    let packed = encoder.finish().unwrap();
    let mut coder = Coder::single_stream();
    coder.set_id_size(3);
    coder
        .decompression_method_id_mut()
        .copy_from_slice(EncoderMethod::ID_BZIP2);
    let splits = [10_003, 70_001, u64::MAX];
    for limit in [u64::MAX, 16 << 20] {
        let control = Arc::new(Lzma2Control::new(2));
        let limits = ArchiveLimits::memory(limit);
        let mut opts = DecodeOptions::header(&limits);
        opts.threads = 2;
        opts.lzma2_control = Some(&control);
        opts.checksum_splits = &splits;
        let mut decoder = add_decoder(
            packed.as_slice(),
            data.len() as u64,
            &coder,
            &Password::empty(),
            &opts,
        )
        .unwrap();
        // A finite limit takes the compact layout and keeps the worker.
        assert!(opts.folding_checksums(), "limit {limit}");
        let mut actual = Vec::new();
        decoder.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, data);
        for (start, stop) in [
            (0, 10_003),
            (10_003, 70_001),
            (70_001, data.len()),
            (0, data.len()),
        ] {
            assert_eq!(
                control.folded(start as u64, (stop - start) as u64),
                Some(lzma_turbo::crc::crc32(&data[start..stop])),
                "limit {limit}"
            );
        }
    }
}

#[test]
fn a_single_threaded_bzip2_coder_leaves_checksums_to_the_reader() {
    let data: Vec<_> = (0..150_001).map(|i| (i * 7 + i / 97) as u8).collect();
    let mut encoder = bzip2_turbo::Encoder::new(Vec::new(), Default::default());
    encoder.write_all(&data).unwrap();
    let packed = encoder.finish().unwrap();
    let mut coder = Coder::single_stream();
    coder.set_id_size(3);
    coder
        .decompression_method_id_mut()
        .copy_from_slice(EncoderMethod::ID_BZIP2);
    for limit in [u64::MAX, 16 << 20] {
        let control = Arc::new(Lzma2Control::new(1));
        let limits = ArchiveLimits::memory(limit);
        let mut opts = DecodeOptions::header(&limits);
        opts.threads = 1;
        opts.lzma2_control = Some(&control);
        opts.checksum_splits = &[u64::MAX];
        let mut decoder = add_decoder(
            packed.as_slice(),
            data.len() as u64,
            &coder,
            &Password::empty(),
            &opts,
        )
        .unwrap();
        assert!(!opts.folding_checksums(), "limit {limit}");
        let mut actual = Vec::new();
        decoder.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, data);
    }
}

// With both BZip2 features on, `bzip2-turbo` is selected and `libbz2-rs-sys`
// is built beside it; each must read the other's streams.
#[cfg(feature = "bzip2-permissive")]
#[test]
fn bzip2_turbo_and_libbz2_rs_sys_read_each_others_streams() {
    let data: Vec<_> = (0..250_003).map(|i| (i * 13 + i / 89) as u8).collect();

    let mut turbo = bzip2_turbo::Encoder::new(Vec::new(), Default::default());
    turbo.write_all(&data).unwrap();
    let packed = turbo.finish().unwrap();
    let mut decoded = Vec::new();
    libbzip2::read::BzDecoder::new(packed.as_slice())
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, data, "libbz2-rs-sys reading bzip2-turbo");

    let mut permissive = libbzip2::write::BzEncoder::new(Vec::new(), libbzip2::Compression::best());
    permissive.write_all(&data).unwrap();
    let packed = permissive.finish().unwrap();
    let mut decoded = Vec::new();
    bzip2_turbo::Decoder::new(packed.as_slice())
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, data, "bzip2-turbo reading libbz2-rs-sys");
}

/// The plain length of each block of `packed`: the workers cut every block's
/// output into 64 KiB chunks and end it with a shorter one.
fn block_lengths(packed: &[u8]) -> Vec<u64> {
    let chunks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&chunks);
    let mut decoder = bzip2_turbo::Decoder::with_output_callback(packed, 2, move |bytes| {
        sink.lock().unwrap().push(bytes.len() as u64);
    });
    decoder.read_to_end(&mut Vec::new()).unwrap();
    drop(decoder);
    let mut blocks = Vec::new();
    let mut length = 0;
    for &chunk in chunks.lock().unwrap().iter() {
        length += chunk;
        if chunk < 65536 {
            blocks.push(length);
            length = 0;
        }
    }
    assert_eq!(length, 0);
    blocks
}

#[test]
fn bzip2_worker_checksums_fold_at_splits_inside_and_on_block_boundaries() {
    let data: Vec<_> = (0..520_003).map(|i| (i * 31 + i / 251) as u8).collect();
    let mut encoder = bzip2_turbo::Encoder::new(
        Vec::new(),
        bzip2_turbo::EncoderOptions {
            block_size: Some(100_000),
            ..Default::default()
        },
    );
    encoder.write_all(&data).unwrap();
    let packed = encoder.finish().unwrap();
    let blocks = block_lengths(&packed);
    assert!(blocks.len() >= 5);
    let first = blocks[0];
    let second = first + blocks[1];
    let mut splits = vec![
        // Several files inside one 64 KiB chunk of the first block.
        10,
        11,
        4_000,
        // On the first block's chunk boundary, and one byte either side.
        65_535,
        65_536,
        65_537,
        // One byte before, exactly on and one byte after a block boundary.
        first - 1,
        first,
        first + 1,
        // Exactly on the next block boundary.
        second,
        // A file spanning two whole blocks and a part of a third.
        second + blocks[2] + blocks[3] + 1_234,
    ];
    splits.push(u64::MAX);
    let mut coder = Coder::single_stream();
    coder.set_id_size(3);
    coder
        .decompression_method_id_mut()
        .copy_from_slice(EncoderMethod::ID_BZIP2);
    for threads in [2, 3, 8] {
        for limit in [u64::MAX, 32 << 20] {
            let control = Arc::new(Lzma2Control::new(threads));
            let limits = ArchiveLimits::memory(limit);
            let mut opts = DecodeOptions::header(&limits);
            opts.threads = threads;
            opts.lzma2_control = Some(&control);
            opts.checksum_splits = &splits;
            let mut decoder = add_decoder(
                packed.as_slice(),
                data.len() as u64,
                &coder,
                &Password::empty(),
                &opts,
            )
            .unwrap();
            assert!(opts.folding_checksums(), "limit {limit}");
            let mut actual = Vec::new();
            decoder.read_to_end(&mut actual).unwrap();
            assert!(actual == data, "threads {threads} limit {limit}");
            let mut bounds = vec![0];
            bounds.extend(splits.iter().copied().filter(|&s| s != u64::MAX));
            bounds.push(data.len() as u64);
            for pair in bounds.windows(2) {
                let (start, stop) = (pair[0], pair[1]);
                assert_eq!(
                    control.folded(start, stop - start),
                    Some(lzma_turbo::crc::crc32(&data[start as usize..stop as usize])),
                    "file {start}..{stop}, threads {threads}, limit {limit}"
                );
            }
        }
    }
}
