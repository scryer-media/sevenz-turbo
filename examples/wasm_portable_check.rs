//! A report of what this crate extracts and writes, for comparing a
//! WebAssembly build with the native one.
//!
//! It opens every `.7z` archive in a fixture directory, in name order, and
//! prints one line per entry: its length and SHA-256, or the refusal a caller
//! would see. With the `compress` feature it then writes an archive of two of
//! the fixture files with each method the build has, prints the SHA-256 of
//! the archive, reads it back and prints the entries again. Nothing is
//! delegated to a host, so the program runs under a plain `wasmtime` or Node.
//!
//! CI builds it natively and for `wasm32-wasip1`, `wasm32-wasip2` and
//! `wasm32-unknown-emscripten`, and requires every report to equal the
//! native one byte for byte.
//!
//! Build (the feature set every lane uses):
//!   cargo build --release --no-default-features \
//!     --features aes256,native-crypto,bzip2,compress,ppmd \
//!     --target wasm32-wasip2 --example wasm_portable_check
//!
//! Run:
//!   wasmtime run --dir tests/resources::/fixtures wasm_portable_check.wasm /fixtures

use std::{
    io::{Cursor, Read, Seek, Write},
    path::{Path, PathBuf},
};

use sevenz_turbo::{ArchiveReader, Password};

/// The fixtures that need a password, and the password.
const PASSWORDS: &[(&str, &str)] = &[
    ("aes_small_test.7z", "iBlm8NTigvru0Jr0"),
    ("encrypted.7z", "sevenz-rust"),
];

fn fingerprint(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(64);
    for byte in sevenz_turbo::sha256(bytes) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// One line per entry of `archive`, each starting with `label`; a refusal is
/// one line too, after whatever entries came out before it.
fn extract<R: Read + Seek>(label: &str, archive: R, password: &str, lines: &mut Vec<String>) {
    let mut reader = match ArchiveReader::new(archive, Password::from(password)) {
        Ok(reader) => reader,
        Err(error) => {
            lines.push(format!("{label} | refused: {error}"));
            return;
        }
    };
    let result = reader.for_each_entries(|entry, entry_reader| {
        let mut contents = Vec::new();
        entry_reader.read_to_end(&mut contents)?;
        let kind = if entry.is_directory() { "dir" } else { "file" };
        lines.push(format!(
            "{label} | {kind} {} | {} bytes sha256 {}",
            entry.name(),
            contents.len(),
            fingerprint(&contents)
        ));
        Ok(true)
    });
    if let Err(error) = result {
        lines.push(format!("{label} | refused: {error}"));
    }
}

#[cfg(feature = "compress")]
fn write_rows(root: &Path, lines: &mut Vec<String>) -> std::io::Result<()> {
    use sevenz_turbo::{
        ArchiveEntry, ArchiveWriter, EncoderConfiguration,
        encoder_options::{AesEncoderOptions, Lzma2Options},
    };

    let sources = ["apache2.txt", "decompress_x86.exe"];
    let mut contents = Vec::new();
    for name in sources {
        contents.push(std::fs::read(root.join(name))?);
    }

    // Each method, whether its archive is the same bytes on every run, and
    // the password to read it back with. AES draws a random salt and IV, so
    // its archive differs each time and only its contents are comparable;
    // writing it is still what proves the target has a source of randomness.
    let mut methods: Vec<(&str, Vec<EncoderConfiguration>, bool, &str)> =
        vec![("lzma2", vec![Lzma2Options::default().into()], true, "")];
    #[cfg(feature = "bzip2")]
    methods.push((
        "bzip2",
        vec![sevenz_turbo::encoder_options::Bzip2Options::default().into()],
        true,
        "",
    ));
    #[cfg(feature = "ppmd")]
    methods.push((
        "ppmd",
        vec![sevenz_turbo::encoder_options::PpmdOptions::default().into()],
        true,
        "",
    ));
    methods.push((
        "aes+lzma2",
        vec![
            AesEncoderOptions::new(Password::new("portable")).into(),
            Lzma2Options::default().into(),
        ],
        false,
        "portable",
    ));

    for (method, configuration, reproducible, password) in methods {
        let label = format!("write {method}");
        let mut bytes = Vec::new();
        let written = (|| {
            let mut writer = ArchiveWriter::new(Cursor::new(&mut bytes))?;
            writer.set_content_methods(configuration);
            for (name, content) in sources.iter().zip(&contents) {
                writer
                    .push_archive_entry(ArchiveEntry::new_file(name), Some(content.as_slice()))?;
            }
            writer.finish()?;
            Ok::<(), sevenz_turbo::Error>(())
        })();
        if let Err(error) = written {
            lines.push(format!("{label} | refused: {error}"));
            continue;
        }
        if reproducible {
            lines.push(format!(
                "{label} | archive | {} bytes sha256 {}",
                bytes.len(),
                fingerprint(&bytes)
            ));
        }
        extract(&label, Cursor::new(bytes.as_slice()), password, lines);
    }
    Ok(())
}

fn report(root: &Path) -> std::io::Result<Vec<String>> {
    // Sorted: directory order differs between hosts and the report must not.
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(".7z") {
            names.push(name.to_owned());
        }
    }
    names.sort();

    let mut lines = Vec::new();
    for name in names {
        let password = PASSWORDS
            .iter()
            .find(|(fixture, _)| *fixture == name)
            .map_or("", |(_, password)| password);
        let file = std::fs::File::open(root.join(&name))?;
        extract(&format!("extract {name}"), file, password, &mut lines);
    }
    #[cfg(feature = "compress")]
    write_rows(root, &mut lines)?;
    Ok(lines)
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/fixtures"));
    let lines = match report(&root) {
        Ok(lines) => lines,
        Err(error) => {
            eprintln!("wasm_portable_check: {}: {error}", root.display());
            std::process::exit(2);
        }
    };
    let mut stdout = std::io::stdout();
    for line in &lines {
        let _ = writeln!(stdout, "{line}");
    }
    let _ = writeln!(stdout, "rows={}", lines.len());
    // Flushed here rather than relying on a flush at exit: the report is the
    // result.
    let _ = stdout.flush();
}
