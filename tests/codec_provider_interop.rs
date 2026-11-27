#![cfg(all(
    any(feature = "zstd", feature = "mzstd"),
    any(feature = "brotli", feature = "mbrotli")
))]

use sevenz_turbo::{ArchiveReader, Password};
use std::{fs::File, path::PathBuf};

fn directory() -> PathBuf {
    std::env::var_os("SEVENZ_INTEROP_DIR")
        .expect(
            "set SEVENZ_INTEROP_DIR to an existing directory shared by the \
             writing and the reading provider builds",
        )
        .into()
}

fn payload() -> Vec<u8> {
    (0u32..170_123)
        .map(|n| (n.wrapping_mul(2654435761) >> 22) as u8)
        .collect()
}

#[cfg(feature = "compress")]
#[test]
#[ignore = "writes archives for a separately compiled provider build"]
fn write_archives() {
    use sevenz_turbo::{
        ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod,
        encoder_options::BrotliOptions,
    };
    let payload = payload();
    let methods: [(&str, EncoderConfiguration); 3] = [
        ("zstd", EncoderMethod::ZSTD.into()),
        (
            "brotli",
            BrotliOptions::from_quality_window(3, 18)
                .with_skippable_frame_size(0)
                .into(),
        ),
        (
            "brotli-framed",
            BrotliOptions::from_quality_window(3, 18)
                .with_skippable_frame_size(65536)
                .into(),
        ),
    ];
    for (name, method) in methods {
        let mut archive =
            ArchiveWriter::new(File::create(directory().join(name)).unwrap()).unwrap();
        archive.set_content_methods(vec![method]);
        archive
            .push_archive_entry(ArchiveEntry::new_file("member"), Some(payload.as_slice()))
            .unwrap();
        archive.finish().unwrap();
    }
}

#[test]
#[ignore = "reads archives from a separately compiled provider build"]
fn read_archives() {
    let payload = payload();
    for name in ["zstd", "brotli", "brotli-framed"] {
        let mut archive = ArchiveReader::new(
            File::open(directory().join(name)).unwrap(),
            Password::empty(),
        )
        .unwrap();
        assert_eq!(archive.read_file("member").unwrap(), payload, "{name}");
    }
}
