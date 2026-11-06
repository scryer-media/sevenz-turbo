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
                data.len() - 1,
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
            data.len(),
            &coder,
            &Password::empty(),
            &opts,
        )
        .unwrap();
        assert_eq!(opts.folding_checksums(), limit == u64::MAX);
        let mut actual = Vec::new();
        decoder.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, data);
        if limit == u64::MAX {
            for (start, stop) in [
                (0, 10_003),
                (10_003, 70_001),
                (70_001, data.len()),
                (0, data.len()),
            ] {
                assert_eq!(
                    control.folded(start as u64, (stop - start) as u64),
                    Some(lzma_turbo::crc::crc32(&data[start..stop]))
                );
            }
        }
    }
}
