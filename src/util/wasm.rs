use std::io::{Read, Seek, SeekFrom, Write};

use js_sys::*;
use wasm_bindgen::prelude::*;

use crate::*;

/// Decompresses a 7z archive in WebAssembly environment.
///
/// This function is specifically designed for WASM targets and uses JavaScript interop
/// to handle the decompression process with a callback function.
///
/// # Arguments
/// * `src` - Uint8Array containing the compressed archive data
/// * `pwd` - Password string for encrypted archives (use empty string for unencrypted)
/// * `f` - JavaScript callback function to handle extracted entries
#[wasm_bindgen]
pub fn decompress(src: Uint8Array, pwd: &str, f: &Function) -> Result<(), String> {
    decompress_with_limits(src, pwd, f, ArchiveLimits::default())
}

/// Returns the default limits, with mutable fields for JavaScript callers.
#[wasm_bindgen]
pub fn default_archive_limits() -> ArchiveLimits {
    ArchiveLimits::default()
}

/// Decompresses with explicit allocation, output and key-derivation limits.
#[wasm_bindgen]
pub fn decompress_with_limits(
    src: Uint8Array,
    pwd: &str,
    f: &Function,
    limits: ArchiveLimits,
) -> Result<(), String> {
    let mut src_reader = Uint8ArrayStream::new(src);
    let pos = src_reader.stream_position().map_err(|e| e.to_string())?;
    src_reader
        .seek(SeekFrom::Start(pos))
        .map_err(|e| e.to_string())?;
    let mut seven = ArchiveReader::with_limits(src_reader, Password::from(pwd), limits)
        .map_err(|e| e.to_string())?;
    seven
        .for_each_entries(|entry, reader| {
            if !entry.is_directory() {
                let path = sanitize_entry_name(entry.name()).map_err(std::io::Error::other)?;

                if entry.size() > 0 {
                    let mut writer = Vec::new();
                    std::io::copy(reader, &mut writer)?;
                    let _ = f.call2(
                        &JsValue::NULL,
                        &JsValue::from(path),
                        &Uint8Array::from(&writer[..]),
                    );
                }
            }
            Ok(true)
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Validates an untrusted archive entry name and returns a safe *relative* path.
///
/// Both `/` and `\` are treated as separators, and any `..`, root, or drive-prefix
/// component is rejected so the host cannot be tricked into writing outside its
/// destination directory.
fn sanitize_entry_name(entry_name: &str) -> Result<String, String> {
    use std::path::{Component, Path, PathBuf};

    if let Some(reason) = crate::archive::unsafe_path_reason(entry_name) {
        return Err(format!("unsafe entry path: {reason}"));
    }
    let normalized = entry_name.replace('\\', "/");
    let mut result = PathBuf::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "unsafe entry path escapes destination: {entry_name}"
                ));
            }
        }
    }
    if result.as_os_str().is_empty() {
        return Err("entry path has no normal components".into());
    }
    Ok(result.to_string_lossy().into_owned())
}

/// Compresses multiple entries into a 7z archive in WebAssembly environment.
///
/// This function creates a compressed archive from multiple file entries,
/// designed specifically for WASM targets.
///
/// # Arguments
/// * `entries` - Vector of JavaScript strings representing file names/paths
/// * `datas` - Vector of Uint8Arrays containing the file data corresponding to entries
///
/// Present only with the `compress` feature: the writer half of the crate
/// (`ArchiveWriter`, `SourceReader`) lives behind it, so a decode-only wasm guest
/// built with `util` exports `decompress` and nothing else.
#[cfg(feature = "compress")]
#[wasm_bindgen]
pub fn compress(entries: Vec<JsString>, datas: Vec<Uint8Array>) -> Result<Uint8Array, String> {
    let output = Uint8Array::new_with_length(32);
    let writer = Uint8ArrayStream::new(output);

    let mut sz = ArchiveWriter::new(writer).map_err(|e| e.to_string())?;
    let reader: Vec<SourceReader<_>> = datas
        .into_iter()
        .map(Uint8ArrayStream::new)
        .map(SourceReader::new)
        .collect();
    let entries = entries
        .into_iter()
        .map(|name| ArchiveEntry {
            name: name.into(),
            has_stream: true,
            ..Default::default()
        })
        .collect();

    sz.push_archive_entries(entries, reader)
        .map_err(|e| e.to_string())?;
    let out = sz.finish().map_err(|e| e.to_string())?;

    Ok(out.data)
}

struct Uint8ArrayStream {
    data: Uint8Array,
    pos: usize,
}

impl Seek for Uint8ArrayStream {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match pos {
            SeekFrom::Start(n) => {
                self.pos = n as usize;
            }
            SeekFrom::End(i) => {
                let posi = self.data.length() as i64 + i;
                if posi < 0 {
                    self.pos = 0;
                } else if posi >= self.data.length() as i64 {
                    self.pos = self.data.length() as usize;
                } else {
                    self.pos = posi as usize;
                }
            }
            SeekFrom::Current(i) => {
                if i != 0 {
                    let posi = self.pos as i64 + i;
                    if posi < 0 {
                        self.pos = 0;
                    } else if posi >= self.data.length() as i64 {
                        self.pos = self.data.length() as usize;
                    } else {
                        self.pos = posi as usize;
                    }
                }
            }
        }
        Ok(self.pos as u64)
    }
}

impl Uint8ArrayStream {
    fn new(data: Uint8Array) -> Self {
        Self { data, pos: 0 }
    }
}

impl Read for Uint8ArrayStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = (self.pos + buf.len()).min(self.data.length() as usize);
        let len = end - self.pos;
        if len == 0 {
            return Ok(0);
        }
        self.data
            .slice(self.pos as u32, end as u32)
            .copy_to(&mut buf[..len]);
        self.pos = end;
        Ok(len)
    }
}

impl Write for Uint8ArrayStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let end = (self.pos + buf.len()).min(self.data.length() as usize);
        let len = end - self.pos;
        if len == 0 {
            return Ok(0);
        }
        self.data
            .slice(self.pos as u32, end as u32)
            .copy_from(&buf[..len]);
        self.pos = end;
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod path_tests {
    use super::sanitize_entry_name;

    #[test]
    fn rejects_names_unsafe_for_host_filesystems() {
        for name in [
            "C:/outside/file",
            "C:relative",
            "C:\\outside\\file",
            "",
            ".",
            "./",
            "dir/\0file",
            "../file",
            "/file",
            "\\\\server\\share",
        ] {
            assert!(sanitize_entry_name(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn normalizes_safe_relative_names() {
        for name in ["dir/file", "./dir/file", "dir\\file"] {
            assert_eq!(sanitize_entry_name(name).unwrap(), "dir/file");
        }
    }
}
