#[cfg(target_os = "macos")]
use std::os::macos::fs::FileTimesExt;
#[cfg(windows)]
use std::os::windows::fs::FileTimesExt;
use std::{
    fs::FileTimes,
    io::{Read, Seek},
    path::{Path, PathBuf},
};

use crate::{Error, Password, *};

/// Decompresses an archive file to a destination directory.
///
/// This is a convenience function for decompressing archive files directly from the filesystem.
///
/// # Arguments
/// * `src_path` - Path to the source archive file
/// * `dest` - Path to the destination directory where files will be extracted
pub fn decompress_file(src_path: impl AsRef<Path>, dest: impl AsRef<Path>) -> Result<(), Error> {
    let file = std::fs::File::open(src_path.as_ref())
        .map_err(|e| Error::file_open(e, src_path.as_ref().to_string_lossy().to_string()))?;
    decompress(file, dest)
}

/// Decompresses an archive file to a destination directory with a custom extraction function.
///
/// The extraction function is called for each entry in the archive, allowing custom handling
/// of individual files and directories during extraction.
///
/// # Arguments
/// * `src_path` - Path to the source archive file
/// * `dest` - Path to the destination directory where files will be extracted
/// * `extract_fn` - Custom function to handle each archive entry during extraction
pub fn decompress_file_with_extract_fn(
    src_path: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    extract_fn: impl FnMut(&ArchiveEntry, &mut dyn Read, &PathBuf) -> Result<bool, Error>,
) -> Result<(), Error> {
    let file = std::fs::File::open(src_path.as_ref())
        .map_err(|e| Error::file_open(e, src_path.as_ref().to_string_lossy().to_string()))?;
    decompress_with_extract_fn(file, dest, extract_fn)
}

/// Decompresses an archive from a reader to a destination directory.
///
/// # Arguments
/// * `src_reader` - Reader containing the archive data
/// * `dest` - Path to the destination directory where files will be extracted
pub fn decompress<R: Read + Seek>(src_reader: R, dest: impl AsRef<Path>) -> Result<(), Error> {
    decompress_with_limits(src_reader, dest, ArchiveLimits::default())
}

/// Decompresses an archive from a reader to a destination directory with a custom extraction function.
///
/// This provides the most flexibility, allowing both custom input sources and custom extraction logic.
///
/// # Arguments
/// * `src_reader` - Reader containing the archive data
/// * `dest` - Path to the destination directory where files will be extracted
/// * `extract_fn` - Custom function to handle each archive entry during extraction
#[cfg(not(target_arch = "wasm32"))]
pub fn decompress_with_extract_fn<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    extract_fn: impl FnMut(&ArchiveEntry, &mut dyn Read, &PathBuf) -> Result<bool, Error>,
) -> Result<(), Error> {
    decompress_impl(
        src_reader,
        dest,
        Password::empty(),
        ArchiveLimits::default(),
        extract_fn,
    )
}

/// Decompresses an encrypted archive file with the given password.
///
/// # Arguments
/// * `src_path` - Path to the encrypted source archive file
/// * `dest` - Path to the destination directory where files will be extracted
/// * `password` - Password to decrypt the archive
#[cfg(all(feature = "aes256", not(target_arch = "wasm32")))]
pub fn decompress_file_with_password(
    src_path: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    password: Password,
) -> Result<(), Error> {
    let file = std::fs::File::open(src_path.as_ref())
        .map_err(|e| Error::file_open(e, src_path.as_ref().to_string_lossy().to_string()))?;
    decompress_with_password(file, dest, password)
}

/// Decompresses an encrypted archive from a reader with the given password.
///
/// # Arguments
/// * `src_reader` - Reader containing the encrypted archive data
/// * `dest` - Path to the destination directory where files will be extracted
/// * `password` - Password to decrypt the archive
#[cfg(all(feature = "aes256", not(target_arch = "wasm32")))]
pub fn decompress_with_password<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    password: Password,
) -> Result<(), Error> {
    decompress_default(src_reader, dest, password, ArchiveLimits::default())
}

/// Decompresses an encrypted archive from a reader with a custom extraction function and password.
///
/// This provides maximum flexibility for encrypted archives, allowing custom input sources,
/// custom extraction logic, and password decryption.
///
/// # Arguments
/// * `src_reader` - Reader containing the encrypted archive data
/// * `dest` - Path to the destination directory where files will be extracted
/// * `password` - Password to decrypt the archive
/// * `extract_fn` - Custom function to handle each archive entry during extraction
#[cfg(all(feature = "aes256", not(target_arch = "wasm32")))]
pub fn decompress_with_extract_fn_and_password<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    password: Password,
    extract_fn: impl FnMut(&ArchiveEntry, &mut dyn Read, &PathBuf) -> Result<bool, Error>,
) -> Result<(), Error> {
    decompress_impl(
        src_reader,
        dest,
        password,
        ArchiveLimits::default(),
        extract_fn,
    )
}

/// [`decompress`] with explicit limits, checked before allocation and extraction.
pub fn decompress_with_limits<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    limits: ArchiveLimits,
) -> Result<(), Error> {
    decompress_default(src_reader, dest, Password::empty(), limits)
}

/// [`decompress_file`] with explicit limits, checked before allocation and extraction.
pub fn decompress_file_with_limits(
    src_path: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    limits: ArchiveLimits,
) -> Result<(), Error> {
    let src_reader = std::fs::File::open(src_path.as_ref())
        .map_err(|e| Error::file_open(e, src_path.as_ref().to_string_lossy().to_string()))?;
    decompress_default(src_reader, dest, Password::empty(), limits)
}

/// [`decompress_with_extract_fn`] with explicit limits, checked before allocation and extraction.
/// The callback owns filesystem safety for any writes it performs.
pub fn decompress_with_extract_fn_and_limits<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    limits: ArchiveLimits,
    extract_fn: impl FnMut(&ArchiveEntry, &mut dyn Read, &PathBuf) -> Result<bool, Error>,
) -> Result<(), Error> {
    decompress_impl(src_reader, dest, Password::empty(), limits, extract_fn)
}

/// [`decompress_file_with_extract_fn`] with explicit limits, checked before allocation and extraction.
/// The callback owns filesystem safety for any writes it performs.
pub fn decompress_file_with_extract_fn_and_limits(
    src_path: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    limits: ArchiveLimits,
    extract_fn: impl FnMut(&ArchiveEntry, &mut dyn Read, &PathBuf) -> Result<bool, Error>,
) -> Result<(), Error> {
    let src_reader = std::fs::File::open(src_path.as_ref())
        .map_err(|e| Error::file_open(e, src_path.as_ref().to_string_lossy().to_string()))?;
    decompress_impl(src_reader, dest, Password::empty(), limits, extract_fn)
}

/// [`decompress_with_password`] with explicit limits, checked before allocation and extraction.
#[cfg(feature = "aes256")]
pub fn decompress_with_password_and_limits<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    password: Password,
    limits: ArchiveLimits,
) -> Result<(), Error> {
    decompress_default(src_reader, dest, password, limits)
}

/// [`decompress_file_with_password`] with explicit limits, checked before allocation and extraction.
#[cfg(feature = "aes256")]
pub fn decompress_file_with_password_and_limits(
    src_path: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    password: Password,
    limits: ArchiveLimits,
) -> Result<(), Error> {
    let src_reader = std::fs::File::open(src_path.as_ref())
        .map_err(|e| Error::file_open(e, src_path.as_ref().to_string_lossy().to_string()))?;
    decompress_default(src_reader, dest, password, limits)
}

/// [`decompress_with_extract_fn_and_password`] with explicit limits, checked before allocation and extraction.
/// The callback owns filesystem safety for any writes it performs.
#[cfg(feature = "aes256")]
pub fn decompress_with_extract_fn_and_password_and_limits<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    password: Password,
    limits: ArchiveLimits,
    extract_fn: impl FnMut(&ArchiveEntry, &mut dyn Read, &PathBuf) -> Result<bool, Error>,
) -> Result<(), Error> {
    decompress_impl(src_reader, dest, password, limits, extract_fn)
}

#[cfg(not(target_arch = "wasm32"))]
fn decompress_impl<R: Read + Seek>(
    mut src_reader: R,
    dest: impl AsRef<Path>,
    password: Password,
    limits: ArchiveLimits,
    mut extract_fn: impl FnMut(&ArchiveEntry, &mut dyn Read, &PathBuf) -> Result<bool, Error>,
) -> Result<(), Error> {
    use std::io::SeekFrom;

    let pos = src_reader.stream_position()?;
    src_reader.seek(SeekFrom::Start(pos))?;
    let mut seven = ArchiveReader::with_limits(src_reader, password, limits)?;
    let dest = PathBuf::from(dest.as_ref());
    if !dest.exists() {
        std::fs::create_dir_all(&dest)?;
    }
    seven.for_each_entries(|entry, reader| {
        let dest_path = safe_join(&dest, entry.name())?;
        extract_fn(entry, reader, &dest_path)
    })?;

    Ok(())
}

/// Joins an untrusted archive entry name onto `dest`, rejecting any path that would
/// escape the destination directory (Zip-Slip / CWE-22).
///
/// Both `/` and `\` are treated as separators so Windows-style names are validated on
/// every platform, and any `..`, root, or drive-prefix component causes rejection.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn safe_join(dest: &Path, entry_name: &str) -> Result<PathBuf, Error> {
    use std::path::Component;

    if let Some(reason) = crate::archive::unsafe_path_reason(entry_name) {
        return Err(Error::UnsafeEntryName {
            name: entry_name.to_owned(),
            reason,
        });
    }
    // Treat backslashes as separators too, so `..\..\x` from a Windows-authored
    // archive is caught when extracting on Unix.
    let normalized = entry_name.replace('\\', "/");
    let mut result = dest.to_path_buf();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::UnsafeEntryName {
                    name: entry_name.to_owned(),
                    reason: "the name walks out of the extraction directory",
                });
            }
        }
    }
    if result == dest {
        return Err(Error::UnsafeEntryName {
            name: entry_name.to_owned(),
            reason: "the name names no file",
        });
    }
    Ok(result)
}

/// Default extraction function that handles standard file and directory extraction.
///
/// # Security
/// `dest` must end in the validated archive entry name; its preceding path
/// selects the trusted extraction root. Existing symlinks below that root are
/// rejected and filesystem operations are confined to a directory handle.
/// Prefer [`decompress`] to keep one root handle open for the whole archive.
/// Custom callbacks that write files remain responsible for their own safety.
///
/// # Arguments
/// * `entry` - Archive entry being processed
/// * `reader` - Reader for the entry's data
/// * `dest` - Destination path for the entry (already validated by the caller)
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::ptr_arg)] // Preserve the upstream callback signature.
pub fn default_entry_extract_fn(
    entry: &ArchiveEntry,
    reader: &mut dyn Read,
    dest: &PathBuf,
) -> Result<bool, Error> {
    if dest
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(Error::invalid_argument(
            "unsafe destination contains a parent-directory component",
        ));
    }
    // Recover the caller-selected root only when the supplied path ends in the
    // validated archive name. Custom renaming should use a custom callback.
    let relative = safe_join(Path::new(""), entry.name())?;
    if !dest.ends_with(&relative) {
        return Err(Error::invalid_argument(
            "destination does not end in the archive entry path",
        ));
    }
    let mut root = dest.clone();
    for _ in relative.components() {
        root.pop();
    }
    let root = open_extract_root(&root)?;
    extract_in_root(&root, entry, reader, &relative)
}

pub(crate) fn open_extract_root(dest: &Path) -> Result<cap_std::fs::Dir, Error> {
    let dest = if dest.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dest
    };
    // Only the caller-selected root uses ambient authority. Every archive
    // component is resolved relative to the directory handle below.
    std::fs::create_dir_all(dest)?;
    Ok(cap_std::fs::Dir::open_ambient_dir(
        dest,
        cap_std::ambient_authority(),
    )?)
}

fn decompress_default<R: Read + Seek>(
    src_reader: R,
    dest: impl AsRef<Path>,
    password: Password,
    limits: ArchiveLimits,
) -> Result<(), Error> {
    let mut seven = ArchiveReader::with_limits(src_reader, password, limits)?;
    seven.extract_to(dest)
}

pub(crate) fn extract_in_root(
    root: &cap_std::fs::Dir,
    entry: &ArchiveEntry,
    reader: &mut dyn Read,
    relative: &Path,
) -> Result<bool, Error> {
    use std::io::{BufWriter, Write};

    let Some(file) = create_extract_entry(root, entry, relative)? else {
        return Ok(true);
    };
    let mut writer = BufWriter::new(file);
    std::io::copy(reader, &mut writer)?;
    writer.flush()?;
    set_extract_times(writer.get_ref(), entry);
    Ok(true)
}

pub(crate) fn create_extract_entry(
    root: &cap_std::fs::Dir,
    entry: &ArchiveEntry,
    relative: &Path,
) -> Result<Option<std::fs::File>, Error> {
    use std::io::ErrorKind;

    // Reject existing links, including links to locations within the root.
    // This check sets policy; cap-std provides confinement if the tree changes
    // between the check and any later operation.
    let mut prefix = PathBuf::new();
    for part in relative.components() {
        prefix.push(part);
        match root.symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::invalid_argument("symbolic link in extraction path"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if entry.is_directory() {
        root.create_dir_all(relative)?;
        return Ok(None);
    }
    if let Some(parent) = relative.parent().filter(|p| !p.as_os_str().is_empty()) {
        root.create_dir_all(parent)?;
    }
    // Replace the directory entry instead of truncating an existing inode.
    // This also avoids modifying data through a pre-existing hard link.
    match root.remove_file(relative) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let file = root
        .open_with(
            relative,
            cap_std::fs::OpenOptions::new().write(true).create_new(true),
        )?
        .into_std();
    Ok(Some(file))
}

pub(crate) fn set_extract_times(file: &std::fs::File, entry: &ArchiveEntry) {
    let file_times = FileTimes::new()
        .set_accessed(entry.access_date().into())
        .set_modified(entry.last_modified_date().into());
    #[cfg(any(windows, target_os = "macos"))]
    let file_times = file_times.set_created(entry.creation_date().into());
    let _ = file.set_times(file_times);
}

#[cfg(test)]
mod confinement_tests {
    use super::*;

    #[test]
    fn normal_files_replace_entries_and_nested_directories_work() {
        let temp = tempfile::tempdir().unwrap();
        let root = open_extract_root(temp.path()).unwrap();
        let entry = ArchiveEntry::new_file("nested/file");
        for data in [b"first".as_slice(), b"next".as_slice()] {
            extract_in_root(&root, &entry, &mut &data[..], Path::new(entry.name())).unwrap();
            assert_eq!(root.read(entry.name()).unwrap(), data);
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_links_are_rejected_at_every_position() {
        let temp = tempfile::tempdir().unwrap();
        let root = open_extract_root(temp.path()).unwrap();
        root.create_dir("real").unwrap();
        root.write("real/file", b"unchanged").unwrap();
        root.symlink("real", "linked-directory").unwrap();
        root.symlink("real/file", "linked-file").unwrap();
        for name in ["linked-directory/file", "linked-file"] {
            let entry = ArchiveEntry::new_file(name);
            assert!(extract_in_root(&root, &entry, &mut &b"data"[..], Path::new(name)).is_err());
        }
        assert_eq!(root.read("real/file").unwrap(), b"unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn extraction_keeps_the_opened_root_when_its_name_changes() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let renamed = temp.path().join("renamed");
        let root = open_extract_root(&original).unwrap();
        std::fs::rename(&original, &renamed).unwrap();
        std::fs::create_dir(&original).unwrap();
        let entry = ArchiveEntry::new_file("file");
        extract_in_root(&root, &entry, &mut &b"data"[..], Path::new("file")).unwrap();
        assert_eq!(std::fs::read(renamed.join("file")).unwrap(), b"data");
        assert!(!original.join("file").exists());
    }

    #[test]
    fn paths_require_a_relative_normal_component() {
        for name in ["", ".", "./", "C:/file", "../file", "/file"] {
            assert!(safe_join(Path::new("root"), name).is_err());
        }
        assert_eq!(
            safe_join(Path::new("root"), "dir/file").unwrap(),
            Path::new("root/dir/file")
        );
    }
}
