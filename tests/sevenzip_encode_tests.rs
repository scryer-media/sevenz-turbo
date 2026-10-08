//! Every way this crate writes a block is one 7-Zip accepts.
//!
//! This crate's own reader is a lenient judge of what its writer produces: a
//! PPMd stream with five bytes too many after the range coder's end decoded
//! here, extracted with `7zz x`, and still failed `7zz t`, whose end-of-stream
//! check is strict. So each encode method is written here solid and
//! non-solid, with and without AES-256, and handed to `7zz t`, which has to
//! pass it; and `7zz x` has to give the inputs back.
//!
//! Brotli, LZ4 and Zstandard are not here: 7-Zip has no codec for them.
//! The file skips itself when neither `7zz` nor `7z` is on `PATH`.
#![cfg(feature = "compress")]

use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use sevenz_turbo::encoder_options::*;
use sevenz_turbo::*;

/// The first of `7zz`, `7z` that runs, if any.
fn seven_zip() -> Option<&'static str> {
    ["7zz", "7z"].into_iter().find(|bin| {
        Command::new(bin)
            .arg("i")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Text-like bytes that every coder compresses, and some that none does.
fn inputs() -> Vec<(String, Vec<u8>)> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut text = Vec::new();
    while text.len() < 300_000 {
        let v = next();
        let word = b'a' + (v >> 40) as u8 % 26;
        text.extend(std::iter::repeat_n(word, 1 + (v % 9) as usize));
        text.push(if v % 13 == 0 { b'\n' } else { b' ' });
    }
    let noise: Vec<u8> = (0..70_000).map(|_| (next() >> 32) as u8).collect();
    vec![
        ("text.txt".into(), text),
        ("noise.bin".into(), noise),
        ("tiny.txt".into(), b"seven".to_vec()),
    ]
}

fn write(methods: Vec<EncoderConfiguration>, solid: bool, files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
    writer.set_content_methods(methods);
    if solid {
        let entries = files
            .iter()
            .map(|(name, _)| ArchiveEntry::new_file(name))
            .collect();
        let readers = files
            .iter()
            .map(|(_, data)| SourceReader::new(Cursor::new(data.as_slice())))
            .collect();
        writer.push_archive_entries(entries, readers).expect("push");
    } else {
        for (name, data) in files {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_slice()))
                .expect("push");
        }
    }
    writer.finish().expect("finish").into_inner()
}

/// `7zz t` passes the archive and `7zz x` gives back every input.
fn check_with_7zip(bin: &str, dir: &Path, label: &str, archive: &[u8], password: Option<&str>) {
    let path = dir.join(format!("{label}.7z"));
    std::fs::write(&path, archive).expect("write archive");
    let pass = format!("-p{}", password.unwrap_or(""));
    let test = Command::new(bin)
        .args(["t", "-bso0", "-bsp0", &pass])
        .arg(&path)
        .output()
        .expect("run 7-Zip");
    assert!(
        test.status.success(),
        "{label}: `{bin} t` exited {:?}: {}",
        test.status.code(),
        String::from_utf8_lossy(&test.stderr),
    );
    let out = dir.join(format!("{label}.out"));
    let extract = Command::new(bin)
        .args(["x", "-bso0", "-bsp0", "-y", &pass])
        .arg(format!("-o{}", out.display()))
        .arg(&path)
        .output()
        .expect("run 7-Zip");
    assert!(extract.status.success(), "{label}: `{bin} x` failed");
    for (name, data) in inputs() {
        let got = std::fs::read(out.join(&name)).expect("extracted file");
        assert!(got == data, "{label}: {name} extracted differently");
    }
}

/// Every method, solid and not, with and without AES.
#[test]
fn seven_zip_tests_and_extracts_every_encode_method() {
    let Some(bin) = seven_zip() else {
        eprintln!("skipping: neither 7zz nor 7z is on PATH");
        return;
    };
    // Mutated only by the feature-gated pushes below.
    #[allow(unused_mut)]
    let mut methods: Vec<(&str, Vec<EncoderConfiguration>)> = vec![
        ("copy", vec![EncoderMethod::COPY.into()]),
        ("lzma", vec![LzmaOptions::from_level(5).into()]),
        ("lzma2", vec![Lzma2Options::from_level(5).into()]),
        (
            "lzma2-bcj",
            vec![
                Lzma2Options::from_level(5).into(),
                EncoderMethod::BCJ_X86_FILTER.into(),
            ],
        ),
        (
            "lzma2-delta",
            vec![
                Lzma2Options::from_level(5).into(),
                DeltaOptions::from_distance(4).into(),
            ],
        ),
    ];
    #[cfg(feature = "ppmd")]
    methods.push(("ppmd", vec![PpmdOptions::from_level(6).into()]));
    #[cfg(feature = "bzip2")]
    methods.push(("bzip2", vec![Bzip2Options::from_level(9).into()]));
    #[cfg(feature = "deflate")]
    methods.push(("deflate", vec![DeflateOptions::from_level(6).into()]));

    let tmp = tempfile::tempdir().expect("tempdir");
    let files = inputs();
    for (name, chain) in &methods {
        for solid in [false, true] {
            let shape = if solid { "solid" } else { "nonsolid" };
            let label = format!("{name}-{shape}");
            let bytes = write(chain.clone(), solid, &files);
            check_with_7zip(bin, tmp.path(), &label, &bytes, None);

            #[cfg(feature = "aes256")]
            {
                let mut encrypted = vec![AesEncoderOptions::new(Password::from("pw")).into()];
                encrypted.extend(chain.iter().cloned());
                let bytes = write(encrypted, solid, &files);
                check_with_7zip(bin, tmp.path(), &format!("{label}-aes"), &bytes, Some("pw"));
            }
        }
    }
}

/// Every folder, and the encrypted header, is encrypted under its own random
/// IV, as 7-Zip's own writer does, however the folders were coded; and 7-Zip
/// extracts the result.
#[cfg(feature = "aes256")]
#[test]
fn every_aes_folder_and_the_header_has_its_own_iv() {
    use std::collections::HashSet;

    let files = inputs();
    let salt = [0x5C; 16];
    let configured = [0x11; 16];
    let methods = || -> Vec<EncoderConfiguration> {
        vec![
            AesEncoderOptions {
                password: Password::from("pw"),
                iv: configured,
                salt,
                num_cycles_power: 10,
            }
            .into(),
            Lzma2Options::from_level(5).into(),
        ]
    };
    let sequential = write(methods(), false, &files);
    let parallel = {
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("writer");
        writer.set_content_methods(methods());
        let entries = files
            .iter()
            .map(|(name, data)| {
                let mut entry = ArchiveEntry::new_file(name);
                entry.size = data.len() as u64;
                entry
            })
            .collect();
        writer
            .push_archive_entries_non_solid(
                entries,
                |index, _| Ok(Some(files[index].1.as_slice())),
                4,
            )
            .expect("push");
        writer.finish().expect("finish").into_inner()
    };
    let solid = write(methods(), true, &files);

    let mut seen = HashSet::new();
    for (label, bytes, folders) in [
        ("sequential", &sequential, files.len()),
        ("parallel", &parallel, files.len()),
        ("solid", &solid, 1),
    ] {
        let archive =
            Archive::read(&mut Cursor::new(bytes.as_slice()), &Password::from("pw")).expect(label);
        assert_eq!(archive.blocks.len(), folders, "{label}");
        let mut ivs = Vec::new();
        for block in &archive.blocks {
            for coder in &block.coders {
                if coder.encoder_method_id() == EncoderMethod::AES256_SHA256.id() {
                    let props = coder.properties();
                    assert_eq!(
                        &props[2..18],
                        &salt,
                        "{label}: the salt is the configured one"
                    );
                    ivs.push(<[u8; 16]>::try_from(&props[18..34]).unwrap());
                }
            }
        }
        assert_eq!(ivs.len(), folders, "{label}: one AES coder per folder");
        // The encoded header's own coder is outside the encryption: the only
        // place the salt appears in the plain bytes, followed by its IV.
        let at = bytes
            .windows(16)
            .rposition(|w| w == salt)
            .expect("the encoded header's AES properties");
        ivs.push(<[u8; 16]>::try_from(&bytes[at + 16..at + 32]).unwrap());
        for iv in ivs {
            assert_ne!(iv, configured, "{label}: a folder reused the options' IV");
            assert!(seen.insert(iv), "{label}: an IV was used twice");
        }
    }

    let Some(bin) = seven_zip() else {
        eprintln!("skipping the 7-Zip half: neither 7zz nor 7z is on PATH");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    check_with_7zip(
        bin,
        tmp.path(),
        "aes-iv-sequential",
        &sequential,
        Some("pw"),
    );
    check_with_7zip(bin, tmp.path(), "aes-iv-parallel", &parallel, Some("pw"));
    check_with_7zip(bin, tmp.path(), "aes-iv-solid", &solid, Some("pw"));
}
