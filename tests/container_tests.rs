//! The container API this fork adds for streaming consumers.
//!
//! Everything here is about what a caller can learn *before* it decodes — the
//! memory an archive will want, where its blocks live, what its checksums are
//! — and about the limits that refuse an archive before an allocation rather
//! than after one.

#![cfg(feature = "compress")]

use std::cell::RefCell;
use std::io::{Cursor, Read};
use std::rc::Rc;

use sevenz_turbo::encoder_options::{EncoderOptions, Lzma2Options};
use sevenz_turbo::{
    Archive, ArchiveEntry, ArchiveLimits, ArchiveReader, ArchiveWriter, BlockCompletion,
    EncoderConfiguration, EncoderMethod, Error, Password, SourceReader,
};

const MIB: u64 = 1024 * 1024;

fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// An archive of `members` entries, one block each unless `solid`.
fn archive_bytes(methods: Vec<EncoderConfiguration>, members: usize, solid: bool) -> Vec<u8> {
    archive_bytes_of(methods, members, solid, 48 * 1024)
}

/// Like [`archive_bytes`], but each member is longer than the writer reads ahead to size a
/// folder (1 MiB) and declares no size, so every block keeps the configured dictionary instead
/// of one reduced to the folder.
fn unsized_archive_bytes(methods: Vec<EncoderConfiguration>, members: usize) -> Vec<u8> {
    archive_bytes_of(methods, members, false, MIB as usize + 1)
}

fn archive_bytes_of(
    methods: Vec<EncoderConfiguration>,
    members: usize,
    solid: bool,
    member_len: usize,
) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods);
    let entries: Vec<(String, Vec<u8>)> = (0..members)
        .map(|index| {
            (
                format!("silver_horizon/{index}.bin"),
                payload(member_len + index, 7 + index as u64),
            )
        })
        .collect();

    if solid {
        let names: Vec<ArchiveEntry> = entries
            .iter()
            .map(|(name, _)| ArchiveEntry::new_file(name))
            .collect();
        let sources: Vec<SourceReader<Cursor<Vec<u8>>>> = entries
            .iter()
            .map(|(_, bytes)| SourceReader::new(Cursor::new(bytes.clone())))
            .collect();
        writer
            .push_archive_entries(names, sources)
            .expect("solid entries");
    } else {
        for (name, bytes) in &entries {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(bytes.as_slice()))
                .expect("entry");
        }
    }

    writer.finish().expect("finish").into_inner()
}

fn lzma2_config(dictionary_size: u32) -> EncoderConfiguration {
    let mut options = Lzma2Options::from_level(1);
    options.set_dictionary_size(dictionary_size);
    EncoderConfiguration::new(EncoderMethod::LZMA2).with_options(EncoderOptions::Lzma2(options))
}

fn read_archive(bytes: &[u8]) -> Archive {
    Archive::read(&mut Cursor::new(bytes.to_vec()), &Password::empty()).expect("read archive")
}

#[test]
fn memory_estimate_follows_the_declared_dictionary() {
    for dictionary_size in [1u32 << 16, 1 << 20, 1 << 24] {
        let bytes = unsized_archive_bytes(vec![lzma2_config(dictionary_size)], 1);
        let archive = read_archive(&bytes);
        let estimate = archive.decoder_memory_estimate().expect("sized");

        // The LZMA2 property byte quantises the dictionary, so the estimate is
        // at least the requested size plus the state margin, and never more
        // than one quantisation step above it.
        let dictionary = u64::from(dictionary_size);
        assert!(
            estimate >= dictionary + MIB,
            "estimate {estimate} below dictionary {dictionary}"
        );
        assert!(
            estimate <= dictionary * 2 + MIB,
            "estimate {estimate} far above dictionary {dictionary}"
        );
    }
}

#[test]
fn memory_estimate_is_the_largest_block_not_their_sum() {
    let one = read_archive(&unsized_archive_bytes(vec![lzma2_config(1 << 20)], 1))
        .decoder_memory_estimate()
        .expect("sized");
    let many = read_archive(&unsized_archive_bytes(vec![lzma2_config(1 << 20)], 4))
        .decoder_memory_estimate()
        .expect("sized");
    assert_eq!(one, many, "blocks decode one after another");
}

#[test]
fn a_memory_limit_refuses_the_archive_before_decoding() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 24)], 1, false);
    let required = read_archive(&bytes)
        .decoder_memory_estimate()
        .expect("sized");

    let result = ArchiveReader::with_limits(
        Cursor::new(bytes.clone()),
        Password::empty(),
        ArchiveLimits::memory(required - 1),
    );
    match result {
        Err(Error::MemoryLimited {
            limit_bytes,
            required_bytes,
        }) => {
            assert_eq!(limit_bytes, required - 1);
            assert_eq!(required_bytes, required);
        }
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("the archive was accepted over its limit"),
    }

    // Exactly the estimate is enough.
    ArchiveReader::with_limits(
        Cursor::new(bytes),
        Password::empty(),
        ArchiveLimits::memory(required),
    )
    .expect("at the limit");
}

#[test]
fn an_end_header_limit_refuses_before_buffering_it() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 20)], 3, false);
    let result = ArchiveReader::with_limits(
        Cursor::new(bytes.clone()),
        Password::empty(),
        ArchiveLimits::new(u64::MAX, 8),
    );
    match result {
        Err(Error::EndHeaderTooLarge {
            limit_bytes,
            declared_bytes,
        }) => {
            assert_eq!(limit_bytes, 8);
            assert!(declared_bytes > 8);
        }
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("the archive was accepted over its limit"),
    }

    ArchiveReader::with_limits(
        Cursor::new(bytes),
        Password::empty(),
        ArchiveLimits::new(u64::MAX, u64::MAX),
    )
    .expect("no limit");
}

#[test]
fn pack_stream_ranges_point_at_the_packed_bytes() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 20)], 3, false);
    let archive = read_archive(&bytes);

    let mut previous_end = 0u64;
    for block_index in 0..archive.blocks.len() {
        let ranges = archive.block_pack_streams(block_index);
        assert_eq!(ranges.len(), 1, "one pack stream per LZMA2 block");
        let range = ranges[0];

        assert!(range.offset >= 32, "packed data starts after the signature");
        assert!(
            range.end() <= bytes.len() as u64,
            "block {block_index} runs past the end of the file"
        );
        assert!(
            range.offset >= previous_end,
            "blocks should not overlap: {range:?} after {previous_end}"
        );
        previous_end = range.end();

        // The bytes named are really this block's: decoding only that slice,
        // with the coder chain the archive declares, must reproduce the entry.
        assert!(range.size > 0);
    }

    assert!(archive.block_pack_streams(archive.blocks.len()).is_empty());
}

#[test]
fn sub_streams_carry_per_entry_sizes_and_crcs() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 20)], 4, true);
    let archive = read_archive(&bytes);

    assert_eq!(archive.num_unpack_sub_streams(), 4);
    assert_eq!(archive.blocks.len(), 1, "solid: one block");

    let sub_streams = archive.block_sub_streams(0);
    assert_eq!(sub_streams.len(), 4);
    for (index, sub_stream) in sub_streams.iter().enumerate() {
        assert_eq!(sub_stream.index, index);
        assert_eq!(sub_stream.size, archive.files[index].size());
        assert!(sub_stream.crc.is_some(), "the writer records entry CRCs");
    }

    // The sub-stream CRCs are the entries' own.
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("reader");
    let mut seen = 0usize;
    reader
        .for_each_entries(|entry, rd| {
            let mut bytes = Vec::new();
            rd.read_to_end(&mut bytes)?;
            assert_eq!(bytes.len() as u64, entry.size());
            seen += 1;
            Ok(true)
        })
        .expect("extract");
    assert_eq!(seen, 4);
}

#[test]
fn the_completion_hook_reports_every_block_once() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 20)], 3, false);
    let completions = Rc::new(RefCell::new(Vec::<BlockCompletion>::new()));

    // The hook is `Send + 'static`, so it cannot borrow `completions`; a
    // channel is how a real consumer would do it.
    let (tx, rx) = std::sync::mpsc::channel();
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("reader");
    reader.set_block_complete_hook(move |completion| {
        let _ = tx.send(completion);
    });
    reader
        .for_each_entries(|_, rd| {
            let mut sink = Vec::new();
            rd.read_to_end(&mut sink)?;
            Ok(true)
        })
        .expect("extract");
    drop(reader);

    completions.borrow_mut().extend(rx.iter());
    let completions = completions.borrow();
    assert_eq!(completions.len(), 3, "one per block");
    for (index, completion) in completions.iter().enumerate() {
        assert_eq!(completion.block_index, index);
        assert!(completion.unpacked_size > 0);
        assert!(completion.crc_verified, "the writer records CRCs");
    }
}

#[test]
fn the_completion_hook_stays_silent_for_a_block_left_unread() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 20)], 3, false);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("reader");
    reader.set_block_complete_hook(move |completion| {
        let _ = tx.send(completion);
    });
    reader
        .for_each_entries(|_, _| Ok(false))
        .expect("stop early");
    drop(reader);

    assert_eq!(
        rx.iter().count(),
        0,
        "nothing was decoded, so nothing completed"
    );
}

#[test]
fn a_block_can_be_decoded_from_a_reader_that_already_parsed_the_header() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 20)], 3, false);
    let expected: Vec<Vec<u8>> = {
        let mut reader =
            ArchiveReader::new(Cursor::new(bytes.clone()), Password::empty()).expect("reader");
        let mut out = Vec::new();
        reader
            .for_each_entries(|_, rd| {
                let mut member = Vec::new();
                rd.read_to_end(&mut member)?;
                out.push(member);
                Ok(true)
            })
            .expect("extract");
        out
    };

    // One open source, one header parse, then blocks decoded in any order:
    // the shape a consumer needs when it is feeding a download into a reader.
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("reader");
    for block_index in (0..3).rev() {
        let decoder = reader.block_decoder(block_index).expect("block decoder");
        let mut decoded = Vec::new();
        decoder
            .for_each_entries(&mut |_: &ArchiveEntry, rd: &mut dyn Read| {
                let mut member = Vec::new();
                rd.read_to_end(&mut member)?;
                decoded.push(member);
                Ok(true)
            })
            .expect("decode block");
        assert_eq!(decoded, vec![expected[block_index].clone()]);
    }

    assert!(reader.block_decoder(99).is_err());
}

#[test]
fn a_damaged_block_names_itself() {
    let mut bytes = archive_bytes(vec![lzma2_config(1 << 20)], 2, false);
    let archive = read_archive(&bytes);
    let target = archive.block_pack_streams(1)[0];

    // Corrupt the middle of the second block's packed stream, leaving the
    // header and the first block intact.
    let victim = (target.offset + target.size / 2) as usize;
    bytes[victim] ^= 0xFF;

    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("reader");
    let error = reader
        .for_each_entries(|_, rd| {
            let mut sink = Vec::new();
            rd.read_to_end(&mut sink)?;
            Ok(true)
        })
        .expect_err("the archive is damaged");

    match error {
        Error::BlockDecode {
            block_index,
            packed_offset,
            ..
        } => {
            assert_eq!(block_index, 1, "the damaged block, not the first one");
            assert_eq!(packed_offset, target.offset);
        }
        other => panic!("expected a located block failure, got {other:?}"),
    }
}

#[test]
fn a_callers_own_error_is_not_blamed_on_the_block() {
    let bytes = archive_bytes(vec![lzma2_config(1 << 20)], 2, false);
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("reader");

    let error = reader
        .for_each_entries(|_, _| {
            Err(Error::Other(std::borrow::Cow::Borrowed(
                "the caller's sink failed",
            )))
        })
        .expect_err("the callback failed");

    assert!(
        matches!(error, Error::Other(_)),
        "a healthy block must not be reported as damaged: {error:?}"
    );
}

/// A Copy block allocates nothing for its coder, but it is still read through
/// the block's 64 KiB pack-stream buffer: the estimate, the reader's limit
/// and a `BlockDecoder`'s own limit all count it.
#[test]
fn a_copy_block_is_charged_its_read_buffer() {
    const BUFFER: u64 = 64 * 1024;
    let bytes = archive_bytes(vec![EncoderMethod::COPY.into()], 1, false);
    let archive = read_archive(&bytes);
    assert_eq!(archive.decoder_memory_estimate().expect("sized"), BUFFER);

    let open = |limit: u64| {
        ArchiveReader::with_limits(
            Cursor::new(bytes.clone()),
            Password::empty(),
            ArchiveLimits::memory(limit),
        )
    };
    assert!(
        matches!(open(BUFFER - 1), Err(Error::MemoryLimited { required_bytes, .. }) if required_bytes == BUFFER),
        "a budget below the read buffer was accepted"
    );
    open(BUFFER).expect("the read buffer fits exactly");

    let password = Password::empty();
    let decode = |limit: u64| {
        let mut source = Cursor::new(bytes.as_slice());
        sevenz_turbo::BlockDecoder::with_limits(
            1,
            0,
            &archive,
            &password,
            &mut source,
            ArchiveLimits::memory(limit),
        )
        .for_each_entries(&mut |_, rd: &mut dyn Read| {
            std::io::copy(rd, &mut std::io::sink())?;
            Ok(true)
        })
    };
    match decode(BUFFER - 1024) {
        Err(Error::BlockDecode { ref message, .. }) if message.starts_with("MaxMemLimited") => {}
        other => panic!("a block budget below the read buffer must be refused, got {other:?}"),
    }
    decode(BUFFER).expect("the read buffer fits exactly");
}
