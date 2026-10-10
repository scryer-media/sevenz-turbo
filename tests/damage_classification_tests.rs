//! What a failed decode tells its caller: damage, or a password.
//!
//! A consumer that repairs damaged archives and asks for a password when one
//! is wrong decides between the two on the error it is given, and usually
//! holds one password for every archive of a job, encrypted or not.
//!
//! - A block that does not decrypt is damaged whatever password the caller
//!   holds. "Encrypted" used to mean "a password was supplied", so a job
//!   password turned every damaged plain block into a password question.
//! - A block that does decrypt stays a password question: a wrong key and a
//!   damaged ciphertext look the same from here.
//! - A stream that ends before the bytes its header declares is damage, and
//!   is reported. A store-mode block over a source cut short used to hand its
//!   files over short and report nothing.
#![cfg(feature = "compress")]

use std::io::{self, Cursor};

use sevenz_turbo::encoder_options::{DeltaOptions, Lzma2Options};
use sevenz_turbo::*;

/// Compressible bytes, different for every seed.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let word = b'a' + (x >> 40) as u8 % 26;
        out.extend(std::iter::repeat_n(word, 1 + (x % 7) as usize));
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// An archive of one block for each of `folders`, written with that folder's
/// methods and holding its files. The header is left unencrypted, so the
/// archive opens under any password and under none.
fn archive(folders: Vec<(Vec<EncoderConfiguration>, Vec<Vec<u8>>)>) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_encrypt_header(false);
    let mut next = 0;
    for (methods, files) in folders {
        writer.set_content_methods(methods);
        let entries = (next..next + files.len())
            .map(|i| ArchiveEntry::new_file(&format!("f{i}")))
            .collect();
        next += files.len();
        let readers = files
            .into_iter()
            .map(|file| SourceReader::new(Cursor::new(file)))
            .collect();
        writer.push_archive_entries(entries, readers).expect("push");
    }
    writer.finish().expect("finish").into_inner()
}

fn store() -> Vec<EncoderConfiguration> {
    vec![EncoderMethod::COPY.into()]
}

fn lzma2() -> Vec<EncoderConfiguration> {
    vec![Lzma2Options::from_level(1).into()]
}

/// LZMA2 in runs a megabyte long, which a parallel decoder gives a worker
/// each and whose checksums it folds.
fn lzma2_in_runs() -> Vec<EncoderConfiguration> {
    let mut options = Lzma2Options::from_level_mt(1, 4, 1 << 20);
    options.set_dictionary_size(1 << 20);
    vec![options.into()]
}

/// Flips `len` bytes of `bytes` from `at` on.
fn damaged(bytes: &[u8], at: usize, len: usize) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    for byte in &mut bytes[at..at + len] {
        *byte ^= 0xA5;
    }
    bytes
}

/// One way of reading an archive.
#[derive(Debug, Clone, Copy)]
struct Run {
    /// The password the caller holds, whatever the archive needs.
    password: Option<&'static str>,
    threads: u32,
    /// Whether the reader is given the source that lets folders decode on
    /// workers.
    positional: bool,
    verify: bool,
}

impl Run {
    fn password(self) -> Password {
        self.password.map_or_else(Password::empty, Password::from)
    }
}

/// Every thread count and source, under each of `passwords`.
fn runs(passwords: &[Option<&'static str>]) -> Vec<Run> {
    let mut runs = Vec::new();
    for &password in passwords {
        for threads in [1, 4] {
            for positional in [false, true] {
                runs.push(Run {
                    password,
                    threads,
                    positional,
                    verify: true,
                });
            }
        }
    }
    runs
}

/// A reader over `source` for the archive `whole` describes, after `tamper`
/// has had its say about the header. `source` is `whole` unless the test cuts
/// it short.
fn reader(
    whole: &[u8],
    source: &[u8],
    tamper: &dyn Fn(&mut Archive),
    run: Run,
) -> ArchiveReader<Cursor<Vec<u8>>> {
    let mut archive = Archive::read(&mut Cursor::new(whole), &run.password()).expect("parse");
    tamper(&mut archive);
    let mut reader =
        ArchiveReader::from_archive(archive, Cursor::new(source.to_vec()), run.password());
    if run.positional {
        reader.set_positional_source(source.to_vec());
    }
    reader.set_threads(run.threads);
    reader.set_verify_checksums(run.verify);
    reader
}

/// Reads every entry to its end.
fn decode(
    whole: &[u8],
    source: &[u8],
    tamper: &dyn Fn(&mut Archive),
    run: Run,
) -> Result<(), Error> {
    reader(whole, source, tamper, run).for_each_entries(|_, rd| {
        io::copy(rd, &mut io::sink())?;
        Ok(true)
    })
}

fn untouched(_: &mut Archive) {}

fn asks_for_a_password(error: &Error) -> bool {
    matches!(
        error,
        Error::PasswordRequired
            | Error::MaybeBadPassword(_)
            | Error::BlockDecode {
                kind: BlockErrorKind::Password,
                ..
            }
    )
}

/// The failure is damage, located in `block`: its kind and its message.
#[track_caller]
fn located_damage(result: Result<(), Error>, block: usize, run: Run) -> (BlockErrorKind, String) {
    match result {
        Err(Error::BlockDecode {
            block_index,
            kind,
            message,
            ..
        }) if kind != BlockErrorKind::Password => {
            assert_eq!(block_index, block, "{run:?}");
            (kind, message)
        }
        other => panic!("{run:?}: expected damage located in block {block}, got {other:?}"),
    }
}

#[track_caller]
fn checksum_mismatch(result: Result<(), Error>, block: usize, run: Run) {
    let (kind, _) = located_damage(result, block, run);
    assert_eq!(kind, BlockErrorKind::ChecksumMismatch, "{run:?}");
}

/// The failure is the stream ending early, located in `block`, with the I/O
/// kind a consumer reads out of the message.
#[track_caller]
fn ended_early(result: Result<(), Error>, block: usize, run: Run) {
    let (kind, message) = located_damage(result, block, run);
    assert_eq!(kind, BlockErrorKind::Io, "{run:?}: {message}");
    assert!(message.contains("UnexpectedEof"), "{run:?}: {message}");
}

#[track_caller]
fn password_question(result: Result<(), Error>, block: usize, run: Run) {
    match result {
        Err(Error::BlockDecode {
            block_index,
            kind: BlockErrorKind::Password,
            ..
        }) => assert_eq!(block_index, block, "{run:?}"),
        other => panic!("{run:?}: expected a password error in block {block}, got {other:?}"),
    }
}

const JOB: Option<&str> = Some("the password of the job");

/// Damage in a block with no AES coder is damage, on every path, whether the
/// caller holds a password or not.
#[test]
fn damage_in_a_plain_block_is_damage_under_any_password() {
    // A store-mode block: the packed bytes are the file, right after the
    // signature header.
    let stored = archive(vec![
        (store(), vec![payload(100_000, 1)]),
        (store(), vec![payload(50_000, 2)]),
    ]);
    let stored_flipped = damaged(&stored, 32 + 1000, 1);
    // A solid LZMA2 block, with a file's CRC wrong and with its stream broken.
    let solid = archive(vec![(
        lzma2(),
        vec![payload(40_000, 3), payload(40_000, 4), payload(40_000, 5)],
    )]);
    let solid_broken = damaged(&solid, 32 + 2000, 8);
    // The same over a stream the parallel decoder splits.
    let in_runs = archive(vec![(
        lzma2_in_runs(),
        vec![payload(5 << 20, 6), payload(3 << 20, 7)],
    )]);
    let in_runs_broken = damaged(&in_runs, 32 + 100_000, 8);
    let wrong_crc = |archive: &mut Archive| archive.files[1].crc ^= 1;

    for run in runs(&[None, JOB]) {
        checksum_mismatch(
            decode(&stored_flipped, &stored_flipped, &untouched, run),
            0,
            run,
        );
        checksum_mismatch(decode(&solid, &solid, &wrong_crc, run), 0, run);
        located_damage(
            decode(&solid_broken, &solid_broken, &untouched, run),
            0,
            run,
        );
        checksum_mismatch(decode(&in_runs, &in_runs, &wrong_crc, run), 0, run);
        located_damage(
            decode(&in_runs_broken, &in_runs_broken, &untouched, run),
            0,
            run,
        );
    }

    // `read_file` answers the same.
    for password in [None, JOB] {
        let run = Run {
            password,
            threads: 1,
            positional: false,
            verify: true,
        };
        checksum_mismatch(
            reader(&stored_flipped, &stored_flipped, &untouched, run)
                .read_file("f0")
                .map(drop),
            0,
            run,
        );
        checksum_mismatch(
            reader(&solid, &solid, &wrong_crc, run)
                .read_file("f1")
                .map(drop),
            0,
            run,
        );
        located_damage(
            reader(&solid_broken, &solid_broken, &untouched, run)
                .read_file("f2")
                .map(drop),
            0,
            run,
        );
    }
}

/// A block of one file that `read_file` decodes itself reports a broken
/// stream as it stands, and that is not a password error either.
#[test]
fn a_broken_stream_read_as_one_file_is_not_a_password_error() {
    let one = archive(vec![(lzma2(), vec![payload(120_000, 8)])]);
    let broken = damaged(&one, 32 + 2000, 8);
    for password in [None, JOB] {
        let run = Run {
            password,
            threads: 1,
            positional: false,
            verify: true,
        };
        let error = reader(&broken, &broken, &untouched, run)
            .read_file("f0")
            .expect_err("the stream is broken");
        assert!(!asks_for_a_password(&error), "{run:?}: {error:?}");
    }
}

/// A block that decrypts cannot tell a wrong key from damaged ciphertext, so
/// both stay a password error: under the wrong password, and under the right
/// one over damaged bytes.
#[cfg(feature = "aes256")]
#[test]
fn a_block_that_decrypts_stays_a_password_question() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;

    let aes = || EncoderConfiguration::from(AesEncoderOptions::new(Password::from("right")));
    let stored = archive(vec![
        (
            vec![aes(), EncoderMethod::COPY.into()],
            vec![payload(100_000, 9)],
        ),
        (
            vec![aes(), EncoderMethod::COPY.into()],
            vec![payload(50_000, 10)],
        ),
    ]);
    let solid = archive(vec![(
        vec![aes(), Lzma2Options::from_level(1).into()],
        vec![payload(40_000, 11), payload(40_000, 12)],
    )]);

    for bytes in [&stored, &solid] {
        for run in runs(&[Some("right")]) {
            decode(bytes, bytes, &untouched, run).expect("the right password decodes");
        }
        for run in runs(&[Some("wrong")]) {
            password_question(decode(bytes, bytes, &untouched, run), 0, run);
        }
        let flipped = damaged(bytes, 32 + 2000, 8);
        for run in runs(&[Some("right")]) {
            password_question(decode(&flipped, &flipped, &untouched, run), 0, run);
        }
        let run = Run {
            password: Some("wrong"),
            threads: 1,
            positional: false,
            verify: true,
        };
        password_question(
            reader(bytes, bytes, &untouched, run)
                .read_file("f0")
                .map(drop),
            0,
            run,
        );
    }
}

/// The question is asked of the block: in one archive, under one password, a
/// damaged plain block is damage and a damaged encrypted block is a password
/// error.
#[cfg(feature = "aes256")]
#[test]
fn the_question_is_asked_of_the_block_and_not_of_the_caller() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;

    let aes = EncoderConfiguration::from(AesEncoderOptions::new(Password::from("right")));
    // Block 0 is stored as it is, so its packed bytes are the 100 000 after
    // the signature header and block 1's follow them.
    let mixed = archive(vec![
        (store(), vec![payload(100_000, 13)]),
        (
            vec![aes, EncoderMethod::COPY.into()],
            vec![payload(50_000, 14)],
        ),
    ]);
    let plain_damaged = damaged(&mixed, 32 + 1000, 1);
    let encrypted_damaged = damaged(&mixed, 32 + 100_000 + 1000, 1);

    for run in runs(&[Some("right")]) {
        decode(&mixed, &mixed, &untouched, run).expect("the right password decodes");
        checksum_mismatch(
            decode(&plain_damaged, &plain_damaged, &untouched, run),
            0,
            run,
        );
        password_question(
            decode(&encrypted_damaged, &encrypted_damaged, &untouched, run),
            1,
            run,
        );
    }
    for run in runs(&[Some("wrong")]) {
        // The plain block needs no password and its damage is still damage;
        // undamaged, the decode gets as far as the block that decrypts.
        checksum_mismatch(
            decode(&plain_damaged, &plain_damaged, &untouched, run),
            0,
            run,
        );
        password_question(decode(&mixed, &mixed, &untouched, run), 1, run);
    }
}

/// An archive of many members, whose header is long enough that the writer
/// compresses it.
fn many_members(methods: Vec<EncoderConfiguration>, encrypt_header: bool) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_encrypt_header(encrypt_header);
    writer.set_content_methods(methods);
    for i in 0..64 {
        writer
            .push_archive_entry(
                ArchiveEntry::new_file(&format!("member-{i:03}.bin")),
                Some(Cursor::new(payload(256, 100 + i))),
            )
            .expect("push");
    }
    writer.finish().expect("finish").into_inner()
}

/// Where the header that follows the packed streams starts.
fn next_header(bytes: &[u8]) -> usize {
    32 + u64::from_le_bytes(bytes[12..20].try_into().unwrap()) as usize
}

/// A compressed header that does not decrypt is read the same under any
/// password: damaged, it is damage.
#[test]
fn a_damaged_plain_header_is_damage_under_any_password() {
    let bytes = many_members(store(), false);
    let header = next_header(&bytes);
    assert_eq!(bytes[header], 0x17, "the header is not an encoded one");
    // The header's packed stream ends where the header that describes it
    // starts.
    let broken = damaged(&bytes, header - 24, 8);

    let open = |password: Password| {
        ArchiveReader::new(Cursor::new(broken.clone()), password)
            .map(drop)
            .expect_err("the header is damaged")
    };
    let without = open(Password::empty());
    let with = open(Password::from("the password of the job"));
    assert!(!asks_for_a_password(&without), "{without:?}");
    assert!(!asks_for_a_password(&with), "{with:?}");
    assert_eq!(format!("{with:?}"), format!("{without:?}"));
}

/// A header that does decrypt is still a password question under the wrong
/// one.
#[cfg(feature = "aes256")]
#[test]
fn an_encrypted_header_under_the_wrong_password_is_a_password_question() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;

    let bytes = many_members(
        vec![
            AesEncoderOptions::new(Password::from("right")).into(),
            EncoderMethod::COPY.into(),
        ],
        true,
    );
    ArchiveReader::new(Cursor::new(bytes.clone()), Password::from("right")).expect("opens");
    for password in [Password::empty(), Password::from("wrong")] {
        let error = ArchiveReader::new(Cursor::new(bytes.clone()), password)
            .map(drop)
            .expect_err("the header does not decrypt");
        assert!(asks_for_a_password(&error), "{error:?}");
    }
}

/// Every way a caller can ask for less checking of a store-mode archive.
fn unchecked_runs() -> Vec<Run> {
    let mut runs = runs(&[None]);
    let unverified = runs.clone().into_iter().map(|run| Run {
        verify: false,
        ..run
    });
    runs.extend(unverified.collect::<Vec<_>>());
    runs
}

/// Says the files and blocks carry no CRCs at all.
fn without_crcs(archive: &mut Archive) {
    for file in &mut archive.files {
        file.has_crc = false;
    }
    for block in &mut archive.blocks {
        block.has_crc = false;
    }
}

/// A store-mode block over a source that ends inside it, or before it, is
/// damage located in that block. Copy has no count of its own, so the files
/// used to come out short with `Ok`: with CRCs and without, verifying and not.
#[test]
fn a_store_block_cut_short_is_located_damage_and_never_a_short_file() {
    for methods in [
        store(),
        // A filter hands on what its input gives it, as Copy does.
        vec![
            DeltaOptions::from_distance(4).into(),
            EncoderMethod::COPY.into(),
        ],
    ] {
        let bytes = archive(vec![
            (methods.clone(), vec![payload(100_000, 15)]),
            (methods, vec![payload(50_000, 16)]),
        ]);
        // Inside the first block, at the boundary between the two, and inside
        // the second.
        for (cut, block) in [
            (32 + 60_000, 0),
            (32 + 100_000, 1),
            (32 + 100_000 + 20_000, 1),
        ] {
            let short = &bytes[..cut];
            for run in unchecked_runs() {
                ended_early(decode(&bytes, short, &untouched, run), block, run);
                ended_early(decode(&bytes, short, &without_crcs, run), block, run);
            }
        }

        // `read_file` decodes the block itself and reports the same end.
        let run = Run {
            password: None,
            threads: 1,
            positional: false,
            verify: true,
        };
        let short = &bytes[..32 + 60_000];
        for tamper in [&untouched as &dyn Fn(&mut Archive), &without_crcs] {
            match reader(&bytes, short, tamper, run).read_file("f0") {
                Err(Error::Io(error, _)) => {
                    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
                }
                other => panic!("expected the stream to end early, got {other:?}"),
            }
        }
    }
}

/// A file its header says is longer than the block that holds it ends early
/// too, and the failure is the block's.
#[test]
fn a_file_longer_than_its_block_is_located_damage() {
    let bytes = archive(vec![(store(), vec![payload(100_000, 17)])]);
    let longer = |archive: &mut Archive| {
        without_crcs(archive);
        archive.files[0].size += 100;
    };
    for run in unchecked_runs() {
        ended_early(decode(&bytes, &bytes, &longer, run), 0, run);
    }
}
