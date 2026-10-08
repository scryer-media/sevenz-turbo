//! Writing BCJ2 blocks.
//!
//! BCJ2 is asked for the way the single-stream filters are: as the last of
//! the content methods, after the coder for its main stream. Every archive
//! here is read back by this crate; where `7zz` is on `PATH` it is also
//! tested and extracted by 7-Zip and the extraction compared with the input.
//! Without `7zz` those checks skip themselves, as `differential_7zz_tests.rs`
//! does.

#![cfg(feature = "compress")]

use std::io::{Cursor, Read};
use std::path::Path;
use std::process::Command;

use sevenz_turbo::encoder_options::Lzma2Options;
use sevenz_turbo::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod, Error,
    Password, SourceReader, prepare_block,
};

/// Barely compressible bytes with an x86 branch every eleven bytes. The
/// targets are small, so they are inside the encoder's relative limit and
/// converted: both the call and the jump stream get some of them.
fn pseudo_x86(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out: Vec<u8> = (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect();
    let mut i = 0;
    while i + 5 <= out.len() {
        out[i] = if i % 3 == 0 { 0xE9 } else { 0xE8 };
        out[i + 1..i + 5].copy_from_slice(&((i as u32 * 3) % 0x4000).to_le_bytes());
        i += 11;
    }
    out
}

fn bcj2_methods() -> Vec<EncoderConfiguration> {
    vec![
        Lzma2Options::from_level(6).into(),
        EncoderMethod::BCJ2_FILTER.into(),
    ]
}

/// The members most tests write: the repository's x86 test executable, two
/// executable-shaped ones, an empty one, a one-byte one, a lone call opcode
/// and a call with its target cut short.
fn members() -> Vec<(String, Vec<u8>)> {
    vec![
        (
            "cobalt_ridge/bin/decompress_x86.exe".into(),
            std::fs::read("tests/resources/decompress_x86.exe").expect("x86 fixture"),
        ),
        (
            "cobalt_ridge/bin/tool.exe".into(),
            pseudo_x86(180 * 1024, 7),
        ),
        ("cobalt_ridge/empty.bin".into(), Vec::new()),
        ("cobalt_ridge/one.bin".into(), vec![0x41]),
        ("cobalt_ridge/call.bin".into(), vec![0xE8]),
        (
            "cobalt_ridge/short.bin".into(),
            vec![0xE8, 0x10, 0x00, 0x00],
        ),
        (
            "cobalt_ridge/lib/helper.dll".into(),
            pseudo_x86(70 * 1024 + 3, 11),
        ),
    ]
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    /// One block per member, through `push_archive_entry`.
    PerEntry,
    /// One solid block, through `push_archive_entries`.
    Solid,
    /// One solid block, through `prepare_block` and `push_prepared_block`.
    Prepared,
}

/// Hands out at most `step` bytes per read, so the writer sees the data in
/// pieces that split branch markers.
struct Trickle {
    data: Vec<u8>,
    at: usize,
    step: usize,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.step).min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fn write_archive(
    methods: Vec<EncoderConfiguration>,
    members: &[(String, Vec<u8>)],
    layout: Layout,
    step: usize,
) -> Result<Vec<u8>, Error> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new()))?;
    writer.set_content_methods(methods.clone());
    let trickle = |bytes: &Vec<u8>| Trickle {
        data: bytes.clone(),
        at: 0,
        step,
    };
    match layout {
        Layout::PerEntry => {
            for (name, bytes) in members {
                writer.push_archive_entry(ArchiveEntry::new_file(name), Some(trickle(bytes)))?;
            }
        }
        Layout::Solid | Layout::Prepared => {
            let entries = members
                .iter()
                .map(|(name, _)| ArchiveEntry::new_file(name))
                .collect();
            let sources = members
                .iter()
                .map(|(_, bytes)| SourceReader::new(trickle(bytes)))
                .collect();
            if let Layout::Solid = layout {
                writer.push_archive_entries(entries, sources)?;
            } else {
                let block = prepare_block(methods.into(), entries, sources)?;
                writer.push_prepared_block(block)?;
            }
        }
    }
    Ok(writer.finish()?.into_inner())
}

/// Reads every member back with `threads` threads and compares it.
fn assert_round_trip(bytes: &[u8], members: &[(String, Vec<u8>)], threads: u32) {
    let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("open");
    reader.set_thread_count(threads);
    for (name, data) in members {
        let got = reader.read_file(name).expect("read member");
        assert!(
            &got == data,
            "{name} differs ({} vs {} bytes)",
            got.len(),
            data.len()
        );
    }
}

/// Every block with data is a BCJ2 block: four coders ending in BCJ2, four
/// pack streams.
fn assert_bcj2_blocks(bytes: &[u8]) -> usize {
    let reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).expect("open");
    let archive = reader.archive();
    for (index, block) in archive.blocks.iter().enumerate() {
        let coders = archive.block_coders(index);
        assert_eq!(coders.len(), 4, "block {index}");
        assert_eq!(coders[3].encoder_method_id(), EncoderMethod::ID_BCJ2);
        assert_eq!(coders[2].encoder_method_id(), EncoderMethod::ID_LZMA2);
        assert_eq!(archive.block_pack_streams(index).len(), 4, "block {index}");
        assert_eq!(
            block.get_unpack_size(),
            block.get_unpack_size_at_index(3),
            "block {index}: BCJ2's output is the block's"
        );
    }
    archive.blocks.len()
}

#[test]
fn bcj2_round_trips_in_every_layout() {
    let members = members();
    for layout in [Layout::PerEntry, Layout::Solid, Layout::Prepared] {
        let bytes = write_archive(bcj2_methods(), &members, layout, usize::MAX).expect("write");
        let blocks = assert_bcj2_blocks(&bytes);
        match layout {
            // An empty member still gets a block of its own here, as it does
            // under every other method.
            Layout::PerEntry => assert_eq!(blocks, members.len()),
            Layout::Solid | Layout::Prepared => assert_eq!(blocks, 1),
        }
        for threads in [1, 4] {
            assert_round_trip(&bytes, &members, threads);
        }
    }
}

#[test]
fn branches_fill_the_call_and_jump_streams() {
    let members = vec![(
        "cobalt_ridge/bin/tool.exe".to_string(),
        pseudo_x86(256 * 1024, 3),
    )];
    let bytes = write_archive(bcj2_methods(), &members, Layout::PerEntry, usize::MAX).unwrap();
    let reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    let block = &reader.archive().blocks[0];
    // Coder 0 is the jump stream's LZMA and coder 1 the call stream's; their
    // outputs are what BCJ2 took out of the main stream.
    let jump = block.get_unpack_size_at_index(0);
    let call = block.get_unpack_size_at_index(1);
    let main = block.get_unpack_size_at_index(2);
    assert!(call > 4096 && jump > 4096, "call {call} jump {jump}");
    assert_eq!(main + call + jump, 256 * 1024);
    let packed = reader.archive().block_pack_streams(0);
    assert!(packed.iter().all(|p| p.size > 0), "{packed:?}");
    // The pack streams follow each other in the file: main, rc, call, jump.
    for pair in packed.windows(2) {
        assert_eq!(pair[0].end(), pair[1].offset);
    }
}

#[test]
fn empty_and_tiny_inputs_round_trip() {
    for data in [
        Vec::new(),
        vec![0x00],
        vec![0xE8],
        vec![0x0F, 0x80],
        vec![0xE9, 0x01, 0x02, 0x03],
        vec![0xE8, 0x00, 0x00, 0x00, 0x00],
        b"short".to_vec(),
    ] {
        let members = vec![("cobalt_ridge/tiny.bin".to_string(), data.clone())];
        for layout in [Layout::PerEntry, Layout::Solid, Layout::Prepared] {
            let bytes = write_archive(bcj2_methods(), &members, layout, usize::MAX).unwrap();
            assert_bcj2_blocks(&bytes);
            assert_round_trip(&bytes, &members, 1);
        }
    }
}

/// How the writer's reads split the data cannot change the archive.
#[test]
fn the_archive_does_not_depend_on_how_the_source_is_read() {
    let members = members();
    let whole = write_archive(bcj2_methods(), &members, Layout::Solid, usize::MAX).unwrap();
    for step in [1, 3, 5, 4093] {
        let split = write_archive(bcj2_methods(), &members, Layout::Solid, step).unwrap();
        assert!(split == whole, "step {step}");
    }
}

#[test]
fn lzma_can_code_the_main_stream() {
    let members = members();
    let bytes = write_archive(
        vec![
            EncoderMethod::LZMA.into(),
            EncoderMethod::BCJ2_FILTER.into(),
        ],
        &members,
        Layout::Solid,
        usize::MAX,
    )
    .unwrap();
    let reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    let coders = reader.archive().block_coders(0);
    assert_eq!(coders[2].encoder_method_id(), EncoderMethod::ID_LZMA);
    assert_eq!(coders[3].encoder_method_id(), EncoderMethod::ID_BCJ2);
    assert_round_trip(&bytes, &members, 1);
}

#[test]
fn bcj2_is_refused_where_seven_zip_would_not_put_it() {
    let members = members();
    let refused = |methods: Vec<EncoderConfiguration>| {
        let result = write_archive(methods.clone(), &members, Layout::PerEntry, usize::MAX);
        assert!(
            matches!(result, Err(Error::Unsupported(_))),
            "{methods:?}: {result:?}"
        );
    };
    // Nothing to code the main stream.
    refused(vec![EncoderMethod::BCJ2_FILTER.into()]);
    // Not the first coder the data meets.
    refused(vec![
        EncoderMethod::BCJ2_FILTER.into(),
        EncoderMethod::LZMA2.into(),
    ]);
    #[cfg(feature = "aes256")]
    refused(vec![
        sevenz_turbo::encoder_options::AesEncoderOptions::new(Password::from("pw")).into(),
        EncoderMethod::LZMA2.into(),
        EncoderMethod::BCJ2_FILTER.into(),
    ]);
}

/// The default content method is unchanged: an executable is not given BCJ2
/// unless it is asked for.
#[test]
fn bcj2_is_opt_in() {
    let members = [(
        "cobalt_ridge/bin/tool.exe".to_string(),
        pseudo_x86(16 * 1024, 5),
    )];
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer
        .push_archive_entry(
            ArchiveEntry::new_file(&members[0].0),
            Some(members[0].1.as_slice()),
        )
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
    let ids: Vec<&[u8]> = reader
        .archive()
        .block_coders(0)
        .iter()
        .map(|c| c.encoder_method_id())
        .collect();
    assert_eq!(ids, [EncoderMethod::ID_LZMA2]);
}

// --- 7-Zip -------------------------------------------------------------------

/// The first of `7zz`, `7z` that runs, if any.
fn seven_zip() -> Option<&'static str> {
    ["7zz", "7z"].into_iter().find(|bin| {
        Command::new(bin)
            .arg("i")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn run(bin: &str, cwd: &Path, args: &[&str]) -> String {
    let output = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run 7-Zip");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{bin} {args:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// 7-Zip tests and extracts what this crate wrote, in every layout, and the
/// extraction is the input.
#[test]
fn seven_zip_extracts_what_this_crate_wrote() {
    let Some(bin) = seven_zip() else {
        eprintln!("skipping: neither 7zz nor 7z is on PATH");
        return;
    };
    let members = members();
    let dir = tempfile::tempdir().unwrap();
    for (n, layout) in [Layout::PerEntry, Layout::Solid, Layout::Prepared]
        .into_iter()
        .enumerate()
    {
        let bytes = write_archive(bcj2_methods(), &members, layout, usize::MAX).unwrap();
        let name = format!("written_{n}.7z");
        std::fs::write(dir.path().join(&name), &bytes).unwrap();

        run(bin, dir.path(), &["t", &name]);
        let listing = run(bin, dir.path(), &["l", "-slt", &name]);
        assert!(
            listing
                .lines()
                .any(|l| l.starts_with("Method = ") && l.contains("BCJ2")),
            "{layout:?}: 7-Zip does not see BCJ2:\n{listing}"
        );

        let out = format!("out_{n}");
        run(bin, dir.path(), &["x", "-y", &format!("-o{out}"), &name]);
        for (member, data) in &members {
            let got = std::fs::read(dir.path().join(&out).join(member)).unwrap();
            assert!(&got == data, "{layout:?}: {member} differs");
        }
    }
}

/// An archive 7-Zip writes with `-mf=BCJ2` has the same block shape as ours:
/// the same coder methods in the same order, with the same call and jump
/// coder properties apart from the dictionary 7-Zip shrinks to the input,
/// and four pack streams.
#[test]
fn the_block_matches_the_one_seven_zip_writes() {
    let Some(bin) = seven_zip() else {
        eprintln!("skipping: neither 7zz nor 7z is on PATH");
        return;
    };
    let data = pseudo_x86(96 * 1024, 13);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tool.exe"), &data).unwrap();
    run(
        bin,
        dir.path(),
        &["a", "-mf=BCJ2", "-m0=LZMA2", "ref.7z", "tool.exe"],
    );
    let theirs = std::fs::read(dir.path().join("ref.7z")).unwrap();
    let members = vec![("tool.exe".to_string(), data)];
    let ours = write_archive(bcj2_methods(), &members, Layout::PerEntry, usize::MAX).unwrap();

    let shape = |bytes: &[u8]| {
        let reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
        let archive = reader.archive();
        let coders: Vec<(Vec<u8>, Option<u8>)> = archive
            .block_coders(0)
            .iter()
            .map(|c| {
                let lzma = c.encoder_method_id() == EncoderMethod::ID_LZMA;
                (
                    c.encoder_method_id().to_vec(),
                    lzma.then(|| c.properties()[0]),
                )
            })
            .collect();
        (coders, archive.block_pack_streams(0).len())
    };
    assert_eq!(shape(&ours), shape(&theirs));
    assert_round_trip(&theirs, &members, 1);
    assert_round_trip(&ours, &members, 1);
}
