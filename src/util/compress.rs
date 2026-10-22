//! 7z Compressor helper functions

use std::{
    fs::File,
    io::{Seek, Write},
    path::{Path, PathBuf},
};

use crate::{ArchiveEntry, ArchiveWriter, Error, writer::LazyFileReader};
#[cfg(feature = "aes256")]
use crate::{EncoderMethod, Password, encoder_options::AesEncoderOptions};

/// Compresses a source file or directory to a destination writer.
///
/// # Arguments
/// * `src` - Path to the source file or directory to compress
/// * `dest` - Writer that implements `Write + Seek` to write the compressed archive to
pub fn compress<W: Write + Seek>(src: impl AsRef<Path>, dest: W) -> Result<W, Error> {
    let mut archive_writer = ArchiveWriter::new(dest)?;
    let parent = if src.as_ref().is_dir() {
        src.as_ref()
    } else {
        src.as_ref().parent().unwrap_or(src.as_ref())
    };
    compress_path(src.as_ref(), parent, &mut archive_writer)?;
    Ok(archive_writer.finish()?)
}

/// Compresses a source file or directory to a destination writer with password encryption.
///
/// # Arguments
/// * `src` - Path to the source file or directory to compress
/// * `dest` - Writer that implements `Write + Seek` to write the compressed archive to
/// * `password` - Password to encrypt the archive with
#[cfg(feature = "aes256")]
pub fn compress_encrypted<W: Write + Seek>(
    src: impl AsRef<Path>,
    dest: W,
    password: Password,
) -> Result<W, Error> {
    let mut archive_writer = ArchiveWriter::new(dest)?;
    if !password.is_empty() {
        archive_writer.set_content_methods(vec![
            AesEncoderOptions::new(password).into(),
            EncoderMethod::LZMA2.into(),
        ]);
    }
    let parent = if src.as_ref().is_dir() {
        src.as_ref()
    } else {
        src.as_ref().parent().unwrap_or(src.as_ref())
    };
    compress_path(src.as_ref(), parent, &mut archive_writer)?;
    Ok(archive_writer.finish()?)
}

/// Compresses a source file or directory to a destination file path.
///
/// This is a convenience function that handles file creation automatically.
///
/// # Arguments
/// * `src` - Path to the source file or directory to compress
/// * `dest` - Path where the compressed archive will be created
pub fn compress_to_path(src: impl AsRef<Path>, dest: impl AsRef<Path>) -> Result<(), Error> {
    if let Some(path) = dest.as_ref().parent()
        && !path.exists()
    {
        std::fs::create_dir_all(path)
            .map_err(|e| Error::io_msg(e, format!("Create dir failed:{:?}", dest.as_ref())))?;
    }
    compress(
        src,
        File::create(dest.as_ref())
            .map_err(|e| Error::file_open(e, dest.as_ref().to_string_lossy().to_string()))?,
    )?;
    Ok(())
}

/// Compresses a source file or directory to a destination file path with password encryption.
///
/// This is a convenience function that handles file creation automatically.
///
/// # Arguments
/// * `src` - Path to the source file or directory to compress
/// * `dest` - Path where the encrypted compressed archive will be created
/// * `password` - Password to encrypt the archive with
#[cfg(feature = "aes256")]
pub fn compress_to_path_encrypted(
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
    password: Password,
) -> Result<(), Error> {
    if let Some(path) = dest.as_ref().parent()
        && !path.exists()
    {
        std::fs::create_dir_all(path)
            .map_err(|e| Error::io_msg(e, format!("Create dir failed:{:?}", dest.as_ref())))?;
    }
    compress_encrypted(
        src,
        File::create(dest.as_ref())
            .map_err(|e| Error::file_open(e, dest.as_ref().to_string_lossy().to_string()))?,
        password,
    )?;
    Ok(())
}

fn compress_path<W: Write + Seek, P: AsRef<Path>>(
    src: P,
    root: &Path,
    archive_writer: &mut ArchiveWriter<W>,
) -> Result<(), Error> {
    let entry_name = src
        .as_ref()
        .strip_prefix(root)
        .map_err(|e| Error::invalid_argument(e.to_string()))?
        .to_string_lossy()
        .to_string();
    let entry = ArchiveEntry::from_path(src.as_ref(), entry_name);
    let path = src.as_ref();
    if path.is_dir() {
        if path != root {
            archive_writer.push_archive_entry::<&[u8]>(entry, None)?;
        }
        for dir in path
            .read_dir()
            .map_err(|e| Error::io_msg(e, "error read dir"))?
        {
            let dir = dir?;
            let ftype = dir.file_type()?;
            if ftype.is_dir() || ftype.is_file() {
                compress_path(dir.path(), root, archive_writer)?;
            }
        }
    } else {
        archive_writer.push_archive_entry_send(
            entry,
            Some(
                File::open(path)
                    .map_err(|e| Error::file_open(e, path.to_string_lossy().to_string()))?,
            ),
        )?;
    }
    Ok(())
}

impl<W: Write + Seek> ArchiveWriter<W> {
    /// Adds a source path to the compression builder with a filter function using solid compression.
    ///
    /// The filter function allows selective inclusion of files based on their paths.
    /// Files are compressed using solid compression for better compression ratios.
    ///
    /// # Arguments
    /// * `path` - Path to add to the compression
    /// * `filter` - Function that returns `true` for paths that should be included
    pub fn push_source_path(
        &mut self,
        path: impl AsRef<Path>,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<&mut Self, Error> {
        encode_path(true, &path, self, filter)?;
        Ok(self)
    }

    /// Adds a source path to the compression builder with a filter function using non-solid compression.
    ///
    /// Non-solid compression allows individual file extraction without decompressing the entire archive,
    /// but typically results in larger archive sizes compared to solid compression.
    ///
    /// # Arguments
    /// * `path` - Path to add to the compression
    /// * `filter` - Function that returns `true` for paths that should be included
    pub fn push_source_path_non_solid(
        &mut self,
        path: impl AsRef<Path>,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<&mut Self, Error> {
        encode_path(false, &path, self, filter)?;
        Ok(self)
    }
}

fn collect_file_paths(
    src: impl AsRef<Path>,
    paths: &mut Vec<PathBuf>,
    filter: &dyn Fn(&Path) -> bool,
) -> std::io::Result<()> {
    let path = src.as_ref();
    if !filter(path) {
        return Ok(());
    }
    if path.is_dir() {
        for dir in path.read_dir()? {
            let dir = dir?;
            let ftype = dir.file_type()?;
            if ftype.is_file() || ftype.is_dir() {
                collect_file_paths(dir.path(), paths, filter)?;
            }
        }
    } else {
        paths.push(path.to_path_buf())
    }
    Ok(())
}

const MAX_BLOCK_SIZE: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

fn encode_path<W: Write + Seek>(
    solid: bool,
    src: impl AsRef<Path>,
    zip: &mut ArchiveWriter<W>,
    filter: impl Fn(&Path) -> bool,
) -> Result<(), Error> {
    let mut paths = Vec::new();
    collect_file_paths(&src, &mut paths, &filter).map_err(|e| {
        Error::io_msg(
            e,
            format!("Failed to collect entries from path:{:?}", src.as_ref()),
        )
    })?;

    if !solid {
        for ele in paths.into_iter() {
            let name = extract_file_name(&src, &ele)?;

            zip.push_archive_entry_send(
                ArchiveEntry::from_path(ele.as_path(), name),
                Some(File::open(ele.as_path())?),
            )?;
        }
        return Ok(());
    }
    encode_solid(&src, paths, zip, MAX_BLOCK_SIZE)
}

/// Writes `paths` as solid blocks that close before they would reach
/// `max_block` bytes; a file that size or larger is a block of its own, and
/// the block open before it is written first, so the archive keeps the order
/// of `paths`.
fn encode_solid<W: Write + Seek>(
    src: &impl AsRef<Path>,
    paths: Vec<PathBuf>,
    zip: &mut ArchiveWriter<W>,
    max_block: u64,
) -> Result<(), Error> {
    let mut entries = Vec::new();
    let mut files = Vec::new();
    let mut file_size = 0;
    for ele in paths.into_iter() {
        let size = ele.metadata()?.len();
        let name = extract_file_name(src, &ele)?;

        if size >= max_block {
            if !entries.is_empty() {
                zip.push_archive_entries_send(
                    std::mem::take(&mut entries),
                    std::mem::take(&mut files),
                )?;
                file_size = 0;
            }
            zip.push_archive_entry_send(
                ArchiveEntry::from_path(ele.as_path(), name),
                Some(File::open(ele.as_path())?),
            )?;
            continue;
        }
        if file_size + size >= max_block {
            zip.push_archive_entries_send(
                std::mem::take(&mut entries),
                std::mem::take(&mut files),
            )?;
            file_size = 0;
        }
        file_size += size;
        entries.push(ArchiveEntry::from_path(ele.as_path(), name));
        files.push(LazyFileReader::new(ele).into());
    }
    if !entries.is_empty() {
        zip.push_archive_entries_send(entries, files)?;
    }

    Ok(())
}

fn extract_file_name(src: &impl AsRef<Path>, ele: &PathBuf) -> Result<String, Error> {
    if ele == src.as_ref() {
        // Single file case: use just the filename.
        Ok(ele
            .file_name()
            .ok_or_else(|| {
                Error::io_msg(
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid filename"),
                    format!("Failed to get filename from {ele:?}"),
                )
            })?
            .to_string_lossy()
            .to_string())
    } else {
        // Directory case: remove path.
        Ok(ele.strip_prefix(src).unwrap().to_string_lossy().to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{ArchiveReader, Password};

    /// A file at the block limit is a block of its own, written after the
    /// block open before it, so the archive keeps the order it was given.
    #[test]
    fn a_block_sized_file_keeps_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let sizes = [("a", 10), ("b", 10), ("big", 100), ("c", 10)];
        let paths: Vec<PathBuf> = sizes
            .iter()
            .map(|(name, size)| {
                let path = dir.path().join(name);
                std::fs::write(&path, vec![b'x'; *size]).unwrap();
                path
            })
            .collect();
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        encode_solid(&dir.path(), paths, &mut writer, 100).unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let mut reader = ArchiveReader::new(Cursor::new(bytes), Password::empty()).unwrap();
        let archive = reader.archive();
        let names: Vec<&str> = archive.files.iter().map(|f| f.name()).collect();
        assert_eq!(names, ["a", "b", "big", "c"]);
        // a and b share a block, big is alone, and c starts the next one.
        assert_eq!(archive.blocks.len(), 3);
        for name in ["a", "big", "c"] {
            let size = sizes.iter().find(|(n, _)| *n == name).unwrap().1;
            assert_eq!(reader.read_file(name).unwrap(), vec![b'x'; size]);
        }
    }
}
