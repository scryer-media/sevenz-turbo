#![cfg(all(feature = "util", not(target_arch = "wasm32")))]

use sevenz_turbo::*;
use std::{io::Cursor, path::Path};

const ARCHIVE: &[u8] = include_bytes!("resources/copy.7z");

fn archive_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/copy.7z")
}

#[test]
fn every_convenience_api_checks_limits_before_output_or_callback() {
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out");
    let limits = ArchiveLimits::new(u64::MAX, 0);
    let mut callback = |_: &ArchiveEntry,
                        _: &mut dyn std::io::Read,
                        _: &std::path::PathBuf|
     -> Result<bool, Error> {
        panic!("limits must be checked before calling the extractor")
    };
    assert!(decompress_with_limits(Cursor::new(ARCHIVE), &dest, limits).is_err());
    assert!(decompress_file_with_limits(archive_path(), &dest, limits).is_err());
    assert!(
        decompress_with_extract_fn_and_limits(Cursor::new(ARCHIVE), &dest, limits, &mut callback)
            .is_err()
    );
    assert!(
        decompress_file_with_extract_fn_and_limits(archive_path(), &dest, limits, &mut callback)
            .is_err()
    );
    #[cfg(feature = "aes256")]
    {
        assert!(
            decompress_with_password_and_limits(
                Cursor::new(ARCHIVE),
                &dest,
                Password::empty(),
                limits
            )
            .is_err()
        );
        assert!(
            decompress_file_with_password_and_limits(
                archive_path(),
                &dest,
                Password::empty(),
                limits
            )
            .is_err()
        );
        assert!(
            decompress_with_extract_fn_and_password_and_limits(
                Cursor::new(ARCHIVE),
                &dest,
                Password::empty(),
                limits,
                &mut callback
            )
            .is_err()
        );
    }
    assert!(!dest.exists());
}

#[test]
fn output_quota_is_enforced_before_extraction() {
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("out");
    let limits = ArchiveLimits::default().with_max_unpack_bytes(0);
    let err = decompress_with_limits(Cursor::new(ARCHIVE), &dest, limits).unwrap_err();
    assert_eq!(err.limit_hit(), Some(Limit::UnpackBytes));
    assert!(!dest.exists());
}

#[cfg(all(feature = "aes256", feature = "compress"))]
#[test]
fn shared_key_non_solid_archive_only_spends_one_derivation() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;
    let password = Password::new("fixture-pass");
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_content_methods(vec![
        AesEncoderOptions {
            password: password.clone(),
            iv: [1; 16],
            salt: [0; 16],
            num_cycles_power: 19,
        }
        .into(),
    ]);
    for index in 0..513 {
        writer
            .push_archive_entry(
                ArchiveEntry::new_file(&format!("file-{index}")),
                Some(&b"data"[..]),
            )
            .unwrap();
    }
    let encoded = writer.finish().unwrap().into_inner();
    // The reader's password is made afresh: a clone of the writer's would
    // carry the key the writer derived, and the budget would never be asked.
    let password = Password::new("fixture-pass");
    for limits in [
        ArchiveLimits::default(),
        ArchiveLimits {
            max_aes_kdf_rounds: 1 << 19,
            ..ArchiveLimits::default()
        },
    ] {
        let mut reader =
            ArchiveReader::with_limits(Cursor::new(&encoded), password.clone(), limits).unwrap();
        let mut count = 0;
        reader
            .for_each_entries(|_, input| {
                let mut bytes = Vec::new();
                input.read_to_end(&mut bytes)?;
                assert_eq!(bytes, b"data");
                count += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(count, 513);
    }
    let limits = ArchiveLimits {
        max_aes_kdf_rounds: (1 << 19) - 1,
        ..ArchiveLimits::default()
    };
    let err =
        ArchiveReader::with_limits(Cursor::new(&encoded), Password::new("fixture-pass"), limits)
            .err()
            .expect("encrypted header exceeds budget");
    assert_eq!(err.limit_hit(), Some(Limit::AesKdfRounds));
}

#[cfg(all(feature = "aes256", feature = "compress"))]
#[test]
fn payload_cache_miss_is_refused_before_exceeding_the_budget() {
    use sevenz_turbo::encoder_options::AesEncoderOptions;
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    writer.set_encrypt_header(false);
    for salt in [1, 2] {
        writer.set_content_methods(vec![
            AesEncoderOptions {
                password: Password::new("fixture-pass"),
                iv: [0; 16],
                salt: [salt; 16],
                num_cycles_power: 2,
            }
            .into(),
        ]);
        writer
            .push_archive_entry(
                ArchiveEntry::new_file(&format!("file-{salt}")),
                Some(&b"data"[..]),
            )
            .unwrap();
    }
    let encoded = writer.finish().unwrap().into_inner();
    let mut reader = ArchiveReader::with_limits(
        Cursor::new(encoded),
        Password::new("fixture-pass"),
        ArchiveLimits {
            max_aes_kdf_rounds: 4,
            ..ArchiveLimits::default()
        },
    )
    .unwrap();
    let mut completed = 0;
    let error = reader
        .for_each_entries(|_, input| {
            std::io::copy(input, &mut std::io::sink())?;
            completed += 1;
            Ok(true)
        })
        .unwrap_err();
    assert_eq!(completed, 1);
    assert_eq!(error.limit_hit(), Some(Limit::AesKdfRounds));
}
