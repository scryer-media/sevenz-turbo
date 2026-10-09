mod counting_writer;
#[cfg(all(feature = "util", not(target_arch = "wasm32")))]
mod lazy_file_reader;
mod pack_info;
mod seq_reader;
mod source_reader;
mod unpack_info;

use std::{
    cell::Cell,
    io::{Read, Seek, Write},
    rc::Rc,
    sync::Arc,
};
#[cfg(not(target_arch = "wasm32"))]
use std::{fs::File, path::Path};

pub(crate) use counting_writer::CountingWriter;
use lzma_turbo::crc::{Crc32, crc32 as crc32_of};

#[cfg(all(feature = "util", not(target_arch = "wasm32")))]
pub(crate) use self::lazy_file_reader::LazyFileReader;
pub(crate) use self::seq_reader::SeqReader;
pub use self::source_reader::SourceReader;
use self::{
    pack_info::PackInfo,
    unpack_info::{Bcj2SideSizes, UnpackInfo},
};
use crate::{
    ArchiveEntry, AutoFinish, AutoFinisher, ByteWriter, Error,
    archive::*,
    bitset::{BitSet, write_bit_set},
    encoder,
};

macro_rules! write_times {
    //write_i64
    ($fn_name:tt, $nid:expr, $has_time:tt, $time:tt) => {
        write_times!($fn_name, $nid, $has_time, $time, write_u64);
    };
    ($fn_name:tt, $nid:expr, $has_time:tt, $time:tt, $write_fn:tt) => {
        fn $fn_name<H: Write>(&self, header: &mut H) -> std::io::Result<()> {
            let mut num = 0;
            for entry in self.files.iter() {
                if entry.$has_time {
                    num += 1;
                }
            }
            if num > 0 {
                header.write_u8($nid)?;
                let mut temp: Vec<u8> = Vec::with_capacity(128);
                let mut out = &mut temp;
                if num != self.files.len() {
                    out.write_u8(0)?;
                    let mut times = BitSet::with_capacity(self.files.len());
                    for i in 0..self.files.len() {
                        if self.files[i].$has_time {
                            times.insert(i);
                        }
                    }
                    write_bit_set(&mut out, &times)?;
                } else {
                    out.write_u8(1)?;
                }
                out.write_u8(0)?;
                for file in self.files.iter() {
                    if file.$has_time {
                        out.$write_fn((file.$time).into())?;
                    }
                }
                out.flush()?;
                write_u64(header, temp.len() as u64)?;
                header.write_all(&temp)?;
            }
            Ok(())
        }
    };
}

type Result<T> = std::result::Result<T, Error>;

/// Writes a 7z archive file.
pub struct ArchiveWriter<W: Write> {
    output: W,
    files: Vec<ArchiveEntry>,
    content_methods: Arc<Vec<EncoderConfiguration>>,
    pack_info: PackInfo,
    unpack_info: UnpackInfo,
    encrypt_header: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl ArchiveWriter<File> {
    /// Creates a file to write a 7z archive to.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::create(path.as_ref())
            .map_err(|e| Error::file_open(e, path.as_ref().to_string_lossy().to_string()))?;
        Self::new(file)
    }
}

/// `methods` for a folder of `size` bytes: every LZMA and LZMA2 coder is told the size (see
/// `EncoderConfiguration::sized_for`), so one whose dictionary is larger than the folder gets one
/// the folder's size, and a folder that fits one LZMA2 block skips the block-parallel coder. `None`
/// is `methods` itself: an unsized folder is coded exactly as it always was, and so, byte for byte,
/// is one no smaller than the dictionary.
fn sized_methods(
    methods: &Arc<Vec<EncoderConfiguration>>,
    size: Option<u64>,
) -> Arc<Vec<EncoderConfiguration>> {
    let Some(size) = size else {
        return Arc::clone(methods);
    };
    if methods.iter().all(|mc| mc.sized_for(size).is_none()) {
        return Arc::clone(methods);
    }
    Arc::new(
        methods
            .iter()
            .map(|mc| mc.sized_for(size).unwrap_or_else(|| mc.clone()))
            .collect(),
    )
}

/// The methods one new folder (or the encrypted header) is coded with: sized
/// for it by [`sized_methods`], and with a fresh random AES IV.
///
/// Every folder, and the header, draws its own IV, as 7-Zip does. The salt and
/// the cycle count stay those of the configured options, so the key is the
/// same for every folder; only the IV, which is written
/// into each folder's own coder properties, differs. Reusing one IV under one
/// key would make the ciphertexts of two folders that begin with the same
/// bytes begin the same.
fn folder_methods(
    methods: &Arc<Vec<EncoderConfiguration>>,
    size: Option<u64>,
) -> Result<Arc<Vec<EncoderConfiguration>>> {
    let sized = sized_methods(methods, size);
    #[cfg(feature = "aes256")]
    {
        use crate::encoder_options::EncoderOptions;
        let encrypted =
            |mc: &EncoderConfiguration| matches!(mc.options, Some(EncoderOptions::Aes(_)));
        if sized.iter().any(encrypted) {
            let mut fresh = Vec::clone(&sized);
            for mc in &mut fresh {
                if let Some(EncoderOptions::Aes(options)) = &mut mc.options {
                    getrandom::fill(&mut options.iv)
                        .map_err(|e| Error::other(format!("AES IV: {e}")))?;
                }
            }
            return Ok(Arc::new(fresh));
        }
    }
    Ok(sized)
}

/// What the entries of one folder declare they hold, or `None` unless every file declares it.
///
/// An entry's `size` before it is pushed is read as how many bytes its reader will yield.
/// [`ArchiveEntry::from_path`] fills it from the file's metadata; zero is "unknown". One file
/// saying zero makes the whole folder unknown - a sum over the rest would understate it and shrink
/// its coder - so the folder is sized by reading ahead instead; see [`folder_size`]. Directories
/// hold no bytes and declare nothing.
fn declared_size<'a>(entries: impl IntoIterator<Item = &'a ArchiveEntry>) -> Option<u64> {
    let mut total = 0u64;
    for entry in entries.into_iter().filter(|entry| !entry.is_directory) {
        if entry.size == 0 {
            return None;
        }
        total = total.saturating_add(entry.size);
    }
    (total > 0).then_some(total)
}

/// How far the writer reads into a folder whose size no entry declared, to learn it. A folder
/// that ends within it is sized exactly; one that does not is no smaller than this, which is past
/// where a dictionary-sized setup costs more than coding the bytes.
const READ_AHEAD: u64 = 1 << 20;

/// How much of an entry's source is read in one go and handed to the coder chain.
///
/// Large enough that a chain whose first coder hands its input to another thread pays one
/// message per mebibyte rather than one per page.
const SOURCE_READ_LEN: usize = 1 << 20;

/// What a folder's compressed bytes are gathered into before they reach the archive's writer.
///
/// Coders write what they produce as they produce it - PPMd a byte at a time, the BCJ2 range
/// coder a few - and the archive's writer is often a bare `File`, so without this a folder costs
/// one write call per byte or two of output. The buffer is flushed when the folder is done,
/// explicitly, so that a failed write is an error of that push and not lost in a drop.
const OUTPUT_BUF_LEN: usize = 256 << 10;

/// The folder's size, as the entries declare it or, failing that, as reading up to
/// [`READ_AHEAD`] bytes of it finds: the bytes read, which the caller codes first, and the size
/// when it is known.
fn folder_size<'a, R: Read>(
    entries: impl IntoIterator<Item = &'a ArchiveEntry>,
    reader: &mut R,
) -> std::io::Result<(Vec<u8>, Option<u64>)> {
    if let Some(size) = declared_size(entries) {
        return Ok((Vec::new(), Some(size)));
    }
    let mut head = Vec::new();
    let n = reader.by_ref().take(READ_AHEAD).read_to_end(&mut head)? as u64;
    Ok((head, (n < READ_AHEAD).then_some(n)))
}

/// Names of the entries in a block, for an error message. Truncated at ~512 bytes, since a solid
/// block can hold many thousands.
fn entries_names(entries: &[ArchiveEntry]) -> String {
    let mut names = String::with_capacity(512);
    for ele in entries.iter() {
        names.push_str(&ele.name);
        names.push(';');
        if names.len() > 512 {
            break;
        }
    }
    names
}

impl<W: Write + Seek> ArchiveWriter<W> {
    /// Prepares writer to write a 7z archive to.
    pub fn new(mut writer: W) -> Result<Self> {
        writer.seek(std::io::SeekFrom::Start(SIGNATURE_HEADER_SIZE))?;

        Ok(Self {
            output: writer,
            files: Default::default(),
            content_methods: Arc::new(vec![EncoderConfiguration::new(EncoderMethod::LZMA2)]),
            pack_info: Default::default(),
            unpack_info: Default::default(),
            encrypt_header: true,
        })
    }

    /// Returns a wrapper around `self` that will finish the stream on drop.
    pub fn auto_finish(self) -> AutoFinisher<Self> {
        AutoFinisher(Some(self))
    }

    /// Sets the default compression methods to use for entry data. Default is LZMA2.
    pub fn set_content_methods(&mut self, content_methods: Vec<EncoderConfiguration>) -> &mut Self {
        if content_methods.is_empty() {
            return self;
        }
        self.content_methods = Arc::new(content_methods);
        self
    }

    /// Whether to enable the encryption of the -header. Default is `true`.
    pub fn set_encrypt_header(&mut self, enabled: bool) {
        self.encrypt_header = enabled;
    }

    /// Non-solid compression - Adds an archive `entry` with data from `reader`.
    ///
    /// # Example
    /// ```no_run
    /// use std::{fs::File, path::Path};
    ///
    /// use sevenz_turbo::*;
    /// let mut sz = ArchiveWriter::create("path/to/dest.7z").expect("create writer ok");
    /// let src = Path::new("path/to/source.txt");
    /// let name = "source.txt".to_string();
    /// let entry = sz
    ///     .push_archive_entry(
    ///         ArchiveEntry::from_path(&src, name),
    ///         Some(File::open(src).unwrap()),
    ///     )
    ///     .expect("ok");
    /// let compressed_size = entry.compressed_size;
    /// sz.finish().expect("done");
    /// ```
    pub fn push_archive_entry<R: Read>(
        &mut self,
        entry: ArchiveEntry,
        reader: Option<R>,
    ) -> Result<&ArchiveEntry> {
        self.push_entry(entry, reader, &mut encoder::FolderCoder::default())
    }

    /// [`ArchiveWriter::push_archive_entry`], with `coder` kept by the caller
    /// from one folder to the next.
    fn push_entry<R: Read>(
        &mut self,
        mut entry: ArchiveEntry,
        reader: Option<R>,
        coder: &mut encoder::FolderCoder,
    ) -> Result<&ArchiveEntry> {
        if !entry.is_directory
            && let Some(mut r) = reader
        {
            let folder = encode_entry(
                &self.content_methods,
                entry,
                &mut r,
                &mut self.output,
                encoder::Bcj2Sides::Threads,
                coder,
            )?;
            return self.record_folder(folder);
        }
        entry.has_stream = false;
        entry.size = 0;
        entry.compressed_size = 0;
        entry.has_crc = false;
        self.files.push(entry);
        Ok(self.files.last().unwrap())
    }

    /// Records a folder [`encode_entry`] has written the main pack stream of:
    /// writes a BCJ2 folder's other three pack streams after it, and takes the
    /// pack, block and entry metadata.
    fn record_folder(&mut self, mut folder: EncodedFolder) -> Result<&ArchiveEntry> {
        let tail = match folder.bcj2.take() {
            Some(bcj2) => {
                for (bytes, _) in &bcj2.streams {
                    self.output
                        .write_all(bytes)
                        .map_err(|e| Error::io_msg(e, "write BCJ2 stream".to_string()))?;
                }
                Some(bcj2.written())
            }
            None => None,
        };
        self.record_written_folder(folder, tail)
    }

    /// [`ArchiveWriter::record_folder`] once every pack stream of the folder
    /// is in the output: `tail` is what its BCJ2 streams took, if it has them.
    fn record_written_folder(
        &mut self,
        folder: EncodedFolder,
        tail: Option<Bcj2Written>,
    ) -> Result<&ArchiveEntry> {
        let EncodedFolder {
            mut entry,
            methods,
            size,
            more_sizes,
            crc,
            compressed_len,
            compressed_crc,
            bcj2,
        } = folder;
        debug_assert!(bcj2.is_none(), "the BCJ2 streams are written before this");
        self.pack_info.add_stream(compressed_len, compressed_crc);
        let mut tail_len = 0;
        if let Some(tail) = &tail {
            for &(len, crc) in &tail.streams {
                self.pack_info.add_stream(len, crc);
                tail_len += len;
            }
        }
        entry.has_stream = true;
        entry.size = size;
        entry.crc = u64::from(crc);
        entry.has_crc = true;
        entry.compressed_crc = u64::from(compressed_crc);
        entry.compressed_size = compressed_len + tail_len;

        let mut sizes = more_sizes;
        sizes.push(size);

        self.unpack_info.add(methods, sizes, crc).bcj2 = tail.map(|t| t.sizes);

        self.files.push(entry);
        Ok(self.files.last().unwrap())
    }

    /// Non-solid compression of many entries, each a folder of its own, coded
    /// on up to `threads` worker threads and written in the order given.
    ///
    /// The archive is the one a loop of [`ArchiveWriter::push_archive_entry`]
    /// over `entries` writes - the same folders, in the same order, with the
    /// same bytes (but for the random IV each encrypted folder draws) - only
    /// built faster. `open(index, entry)` is called for each
    /// entry that is not a directory, on the thread that will code it, just
    /// before it is coded: it returns that entry's reader, or `None` for an
    /// entry with no data. So no more files are open at once than there are
    /// folders in flight. An entry's `size` is read, as by
    /// [`ArchiveEntry::from_path`], as how many bytes its reader will yield.
    ///
    /// Folders go to the workers when no coder of theirs starts block
    /// threads: always for a single-threaded method chain, and for a
    /// block-parallel LZMA2 one when the folder declares a size that fits one
    /// block. Any other folder, one whose coder would start block threads of
    /// its own, is coded alone on the calling thread, after every folder
    /// before it has been written, so the workers and a multi-threaded coder
    /// never run at once.
    ///
    /// **Memory.** Each worker stages at most two folders' compressed bytes,
    /// each at most 8 MiB, before it waits for the writer to take them, so
    /// what is held between the workers and the output is bounded by the
    /// thread count, whatever the folders' sizes; on top of that each worker
    /// holds one folder's coder. A folder larger than the stage streams
    /// through it as the writer catches up. A BCJ2 folder's three other pack
    /// streams are held whole by its coder until the folder ends, as
    /// [`ArchiveWriter::push_archive_entry`] holds them, and then count
    /// against the stage: a worker whose stage has no room for them waits
    /// for the writer rather than starting another folder, so no worker has
    /// more than one folder's waiting.
    ///
    /// **Threads.** A folder is coded by its worker and that worker's coder
    /// thread, which `threads` counts as one: the worker feeds the coder and
    /// waits for it. Every LZMA and LZMA2 coder in the chain has such a
    /// thread (but not with the `lzma-rust2-encoder` feature, whose coders
    /// run on the worker), so a chain with two of them has `threads / 2`
    /// folders in flight, and where that is one its folders are coded one at
    /// a time. A filter or AES-256 in such a chain runs on the worker between
    /// its waits and costs no thread. Any other compressor (PPMd, BZip2 and
    /// the rest) codes on the worker while the coder's thread runs, so beside
    /// an LZMA or LZMA2 coder it counts as a thread too. A BCJ2 folder's call
    /// and jump coders run on the worker itself here, where a folder coded
    /// alone gives each a thread.
    ///
    /// With `threads` at one, and on targets without threads, this is the
    /// loop of `push_archive_entry` itself.
    ///
    /// # Errors
    ///
    /// The first failure in entry order: what `open` returned, or what
    /// `push_archive_entry` would have returned for that entry. Folders before
    /// it have been written; none after it are.
    pub fn push_archive_entries_non_solid<R, F>(
        &mut self,
        entries: Vec<ArchiveEntry>,
        open: F,
        threads: u32,
    ) -> Result<&mut Self>
    where
        R: Read,
        F: Fn(usize, &ArchiveEntry) -> std::io::Result<Option<R>> + Sync,
    {
        let threads = threads.clamp(1, 256);
        // What a folder on a worker keeps busy, and so how many fit `threads`.
        let per_folder = encoder::folder_threads(&encoder::one_thread_each(&self.content_methods));
        let workers = threads / per_folder;
        let parallel = |entry: &ArchiveEntry| {
            entry.is_directory
                || encoder::folder_threads(&sized_methods(
                    &self.content_methods,
                    declared_size([entry]),
                )) <= per_folder
        };
        let eligible: Vec<bool> = entries.iter().map(parallel).collect();
        // The folders coded here, one after another, share one coder.
        let mut coder = encoder::FolderCoder::default();
        let mut index = 0;
        while index < entries.len() {
            let start = index;
            while index < entries.len() && eligible[index] {
                index += 1;
            }
            if workers > 1 && index - start > 1 {
                self.push_non_solid_parallel(&entries, start..index, &open, workers, &mut coder)?;
            } else {
                for at in start..index {
                    self.push_opened(&entries, at, &open, &mut coder)?;
                }
            }
            if index < entries.len() {
                self.push_opened(&entries, index, &open, &mut coder)?;
                index += 1;
            }
        }
        Ok(self)
    }

    /// One entry of [`ArchiveWriter::push_archive_entries_non_solid`], here.
    fn push_opened<R, F>(
        &mut self,
        entries: &[ArchiveEntry],
        index: usize,
        open: &F,
        coder: &mut encoder::FolderCoder,
    ) -> Result<()>
    where
        R: Read,
        F: Fn(usize, &ArchiveEntry) -> std::io::Result<Option<R>>,
    {
        let entry = entries[index].clone();
        let reader = if entry.is_directory {
            None
        } else {
            open(index, &entry)
                .map_err(|e| Error::io_msg(e, format!("Open entry:{}", entry.name())))?
        };
        self.push_entry(entry, reader, coder)?;
        Ok(())
    }

    /// A run of [`ArchiveWriter::push_archive_entries_non_solid`]'s folders,
    /// coded on workers and written here in order.
    #[cfg(not(target_arch = "wasm32"))]
    fn push_non_solid_parallel<R, F>(
        &mut self,
        entries: &[ArchiveEntry],
        run: std::ops::Range<usize>,
        open: &F,
        workers: u32,
        coder: &mut encoder::FolderCoder,
    ) -> Result<()>
    where
        R: Read,
        F: Fn(usize, &ArchiveEntry) -> std::io::Result<Option<R>> + Sync,
    {
        let methods = Arc::new(encoder::one_thread_each(&self.content_methods));
        let workers = (workers as usize).min(run.len());
        // A coder per worker, kept from one of its folders to the next: a
        // worker takes one at the start of a folder and puts it back at the
        // end, so there are never more than there are workers.
        let coders = std::sync::Mutex::new(Vec::<encoder::FolderCoder>::new());
        let first = run.start;
        let outcome = crate::ordered::run(
            run.len(),
            workers,
            ENCODE_WINDOW_PER_WORKER * workers,
            ENCODE_STAGE_BYTES,
            |job, tx| {
                let index = first + job;
                let entry = entries[index].clone();
                let result = if entry.is_directory {
                    Ok(FolderOut::Unstreamed(entry))
                } else {
                    match open(index, &entry) {
                        Err(e) => Err(Error::io_msg(e, format!("Open entry:{}", entry.name()))),
                        Ok(None) => Ok(FolderOut::Unstreamed(entry)),
                        Ok(Some(mut reader)) => {
                            let mut sink = StageSink {
                                tx: &mut *tx,
                                cancelled: false,
                            };
                            let mut coder = coders
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .pop()
                                .unwrap_or_default();
                            // The worker is the folder's thread: a BCJ2 chain's
                            // call and jump coders run on it, not beside it.
                            let encoded = encode_entry(
                                &methods,
                                entry,
                                &mut reader,
                                &mut sink,
                                encoder::Bcj2Sides::Inline,
                                &mut coder,
                            );
                            coders
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(coder);
                            if sink.cancelled {
                                return;
                            }
                            encoded.map(|folder| FolderOut::Encoded(folder, None))
                        }
                    }
                };
                // A BCJ2 folder's other three pack streams follow the main
                // one through the stage, in pieces no longer than the main
                // stream's and each charged its length, so the worker waits
                // on the stage for them exactly as it did for the main
                // stream's bytes. Only their checksums and sizes ride on
                // `Done`.
                let result = match result {
                    Ok(FolderOut::Encoded(mut folder, _)) => {
                        let shipped = match folder.bcj2.take() {
                            Some(bcj2) => {
                                match send_bcj2_tail(bcj2, OUTPUT_BUF_LEN, |m, w| tx.send(m, w)) {
                                    Ok(shipped) => Some(Box::new(shipped)),
                                    Err(crate::ordered::Cancelled) => return,
                                }
                            }
                            None => None,
                        };
                        Ok(FolderOut::Encoded(folder, shipped))
                    }
                    other => other,
                };
                let _ = tx.send(EncodeMessage::Done(Box::new(result)), 0);
            },
            |_, rx| {
                // What the folder's BCJ2 tail took, stream by stream, as it
                // was written.
                let mut tail_lens = [0u64; 3];
                loop {
                    match rx.recv() {
                        Some(EncodeMessage::Data(bytes)) => self
                            .output
                            .write_all(&bytes)
                            .map_err(|e| Error::io_msg(e, "write folder".to_string()))?,
                        Some(EncodeMessage::Tail { stream, bytes }) => {
                            self.output
                                .write_all(&bytes)
                                .map_err(|e| Error::io_msg(e, "write BCJ2 stream".to_string()))?;
                            tail_lens[stream] += bytes.len() as u64;
                        }
                        Some(EncodeMessage::Done(result)) => {
                            match (*result)? {
                                FolderOut::Encoded(folder, shipped) => {
                                    let tail = match shipped {
                                        Some(shipped) => Some(shipped.written(tail_lens)),
                                        None if tail_lens == [0; 3] => None,
                                        None => {
                                            return Err(Error::other(
                                                "a folder sent BCJ2 streams it did not declare",
                                            ));
                                        }
                                    };
                                    self.record_written_folder(folder, tail)?;
                                }
                                FolderOut::Unstreamed(entry) => {
                                    self.push_archive_entry::<&[u8]>(entry, None)?;
                                }
                            }
                            return Ok(());
                        }
                        None => {
                            return Err(Error::other(
                                "the worker coding this folder stopped before finishing it",
                            ));
                        }
                    }
                }
            },
        );
        match outcome {
            Some(result) => result,
            // Not one worker could be started: code the run here instead.
            None => {
                for at in run {
                    self.push_opened(entries, at, open, coder)?;
                }
                Ok(())
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn push_non_solid_parallel<R, F>(
        &mut self,
        entries: &[ArchiveEntry],
        run: std::ops::Range<usize>,
        open: &F,
        _workers: u32,
        coder: &mut encoder::FolderCoder,
    ) -> Result<()>
    where
        R: Read,
        F: Fn(usize, &ArchiveEntry) -> std::io::Result<Option<R>> + Sync,
    {
        for at in run {
            self.push_opened(entries, at, open, coder)?;
        }
        Ok(())
    }

    /// Append a block compressed elsewhere by [`prepare_block`].
    ///
    /// Only the parts that must happen in output order: write the bytes, record the pack and block
    /// metadata, take the entries. Everything expensive already happened on whatever thread built
    /// the [`PreparedBlock`].
    ///
    /// A block holding no entries is dropped rather than written, so an empty batch does not put a
    /// junk pack stream and a zero-substream block into the archive.
    pub fn push_prepared_block(&mut self, block: PreparedBlock) -> Result<&mut Self> {
        if block.is_empty() {
            return Ok(self);
        }

        let PreparedBlock {
            compressed,
            compressed_crc,
            entries,
            methods,
            sizes,
            crc,
            sub_stream_sizes,
            sub_stream_crcs,
            bcj2,
        } = block;

        let compressed_len = compressed.len() as u64;
        self.output
            .write_all(&compressed)
            .map_err(|e| Error::io_msg(e, "push_prepared_block: write".to_string()))?;

        self.pack_info.add_stream(compressed_len, compressed_crc);
        if let Some(bcj2) = &bcj2 {
            self.write_bcj2_tail(bcj2)?;
        }
        self.unpack_info
            .add_multiple(
                methods,
                sizes,
                crc,
                entries.len() as u64,
                sub_stream_sizes,
                sub_stream_crcs,
            )
            .bcj2 = bcj2.map(|b| b.sizes);
        self.files.extend(entries);
        Ok(self)
    }

    /// Solid compression - packs `entries` into one pack.
    ///
    /// # Panics
    /// * If `entries`'s length not equals to `reader.reader_len()`
    pub fn push_archive_entries<R: Read>(
        &mut self,
        entries: Vec<ArchiveEntry>,
        reader: Vec<SourceReader<R>>,
    ) -> Result<&mut Self> {
        let mut entries = entries;
        let mut r = SeqReader::new(reader);
        assert_eq!(r.reader_len(), entries.len());
        let mut compressed_len = 0;
        let mut compressed = CompressWrapWriter::new(&mut self.output, &mut compressed_len);
        let mut out = std::io::BufWriter::with_capacity(OUTPUT_BUF_LEN, &mut compressed);
        let (head, folder) = folder_size(&entries, &mut r)
            .map_err(|e| Error::io_msg(e, format!("Encode entries:{}", entries_names(&entries))))?;
        let content_methods = &folder_methods(&self.content_methods, folder)?;
        let mut more_sizes: Vec<Rc<Cell<usize>>> = Vec::with_capacity(content_methods.len() - 1);
        let mut bcj2 = None;
        let pulled = match content_methods.split_last() {
            Some((first, _)) => encoder::FolderCoder::plan(first)?,
            None => None,
        };

        let (crc, size) = if let Some(pulled) = pulled {
            // A one-thread coder pulls the members itself: no read buffer,
            // no hand-off chunks and no encoder thread.
            encode_pulled(
                content_methods,
                &pulled,
                &mut encoder::FolderCoder::default(),
                &mut head.as_slice().chain(&mut r),
                &mut out,
                &mut more_sizes,
                &mut bcj2,
                encoder::Bcj2Sides::Threads,
                |e| Error::io_msg(e, format!("Encode entries:{}", entries_names(&entries))),
            )?
        } else {
            let mut w = Self::create_writer(
                content_methods,
                &mut out,
                &mut more_sizes,
                &mut bcj2,
                encoder::Bcj2Sides::Threads,
            )?;
            let mut write_len = 0;
            let mut w = CompressWrapWriter::new(&mut w, &mut write_len);
            w.write_all(&head).map_err(|e| {
                Error::io_msg(e, format!("Encode entries:{}", entries_names(&entries)))
            })?;
            drop(head);
            let mut buf = vec![0u8; SOURCE_READ_LEN];

            loop {
                match r.read(&mut buf) {
                    Ok(n) => {
                        if n == 0 {
                            break;
                        }
                        w.write_all(&buf[..n]).map_err(|e| {
                            Error::io_msg(e, format!("Encode entries:{}", entries_names(&entries)))
                        })?;
                    }
                    Err(e) => {
                        return Err(Error::io_msg(
                            e,
                            format!("Encode entries:{}", entries_names(&entries)),
                        ));
                    }
                }
            }
            w.flush().map_err(|e| {
                let mut names = String::with_capacity(512);
                for ele in entries.iter() {
                    names.push_str(&ele.name);
                    names.push(';');
                    if names.len() > 512 {
                        break;
                    }
                }
                Error::io_msg(e, format!("Encode entry:{names}"))
            })?;
            w.write(&[]).map_err(|e| {
                Error::io_msg(e, format!("Encode entry:{}", entries_names(&entries)))
            })?;

            (w.crc_value(), write_len)
        };
        out.flush()
            .map_err(|e| Error::io_msg(e, format!("Encode entries:{}", entries_names(&entries))))?;
        drop(out);
        let compressed_crc = compressed.crc_value();
        let mut sub_stream_crcs = Vec::with_capacity(entries.len());
        let mut sub_stream_sizes = Vec::with_capacity(entries.len());
        for i in 0..entries.len() {
            let entry = &mut entries[i];
            let ri = &r[i];
            entry.crc = ri.crc_value() as u64;
            entry.size = ri.read_count() as u64;
            sub_stream_crcs.push(entry.crc as u32);
            sub_stream_sizes.push(entry.size);
            entry.has_crc = true;
        }

        self.pack_info
            .add_stream(compressed_len as u64, compressed_crc);
        let content_methods = Arc::clone(content_methods);
        let bcj2 = Bcj2Packed::take(bcj2);
        if let Some(bcj2) = &bcj2 {
            self.write_bcj2_tail(bcj2)?;
        }

        let mut sizes = Vec::with_capacity(more_sizes.len() + 1);
        sizes.extend(more_sizes.iter().map(|s| s.get() as u64));
        sizes.push(size as u64);

        self.unpack_info
            .add_multiple(
                content_methods,
                sizes,
                crc,
                entries.len() as u64,
                sub_stream_sizes,
                sub_stream_crcs,
            )
            .bcj2 = bcj2.map(|b| b.sizes);

        self.files.extend(entries);
        Ok(self)
    }

    /// Builds the coder chain for `methods` over `out`.
    ///
    /// A chain ending in BCJ2 is a four-stream block: the methods before it
    /// code the main stream, as a linear chain does, and BCJ2 brings its own
    /// coders for the call and jump streams. Its tail - the three pack
    /// streams that follow the main one - lands in `bcj2` once the chain is
    /// finished. `sides` is where those two coders run.
    fn create_writer<'a, O: Write + 'a>(
        methods: &[EncoderConfiguration],
        out: O,
        more_sized: &mut Vec<Rc<Cell<usize>>>,
        bcj2: &mut Option<encoder::Bcj2Slot>,
        sides: encoder::Bcj2Sides,
    ) -> Result<Box<dyn Write + 'a>> {
        let mut encoder: Box<dyn Write> = Box::new(out);
        let mut first = true;
        for (i, mc) in methods.iter().enumerate() {
            if mc.method.id() == EncoderMethod::ID_BCJ2 {
                Self::check_bcj2_methods(methods, i)?;
                let counting = CountingWriter::new(encoder);
                more_sized.push(counting.counting());
                let (bcj2_encoder, slot) = encoder::add_bcj2_encoder(counting, sides)?;
                *bcj2 = Some(slot);
                encoder = Box::new(bcj2_encoder);
                continue;
            }
            if !first {
                let counting = CountingWriter::new(encoder);
                more_sized.push(counting.counting());
                encoder = Box::new(encoder::add_encoder(counting, mc)?);
            } else {
                let counting = CountingWriter::new(encoder);
                encoder = Box::new(encoder::add_encoder(counting, mc)?);
            }
            first = false;
        }
        Ok(encoder)
    }

    /// BCJ2 is written only where 7-Zip puts it: last in the method list,
    /// which makes it the first coder the data meets, over at least one coder
    /// for its main stream. It is not combined with AES, which would leave
    /// the call, jump and rc streams unencrypted.
    fn check_bcj2_methods(methods: &[EncoderConfiguration], index: usize) -> Result<()> {
        if index + 1 != methods.len() {
            return Err(Error::unsupported(
                "BCJ2 must be the last content method: it is the first coder the data meets",
            ));
        }
        if index == 0 {
            return Err(Error::unsupported(
                "BCJ2 needs a coder for its main stream before it in the content methods",
            ));
        }
        if methods
            .iter()
            .any(|mc| mc.method.id() == EncoderMethod::ID_AES256_SHA256)
        {
            return Err(Error::unsupported(
                "BCJ2 cannot be combined with AES-256 encryption when writing",
            ));
        }
        Ok(())
    }

    /// Appends a BCJ2 block's rc, call and jump pack streams after its main
    /// one, and returns how many bytes they took.
    fn write_bcj2_tail(&mut self, bcj2: &Bcj2Packed) -> Result<u64> {
        let mut total = 0;
        for (bytes, crc) in &bcj2.streams {
            self.output
                .write_all(bytes)
                .map_err(|e| Error::io_msg(e, "write BCJ2 stream".to_string()))?;
            self.pack_info.add_stream(bytes.len() as u64, *crc);
            total += bytes.len() as u64;
        }
        Ok(total)
    }

    /// Finishes the compression.
    pub fn finish(mut self) -> std::io::Result<W> {
        let mut header: Vec<u8> = Vec::with_capacity(64 * 1024);
        self.write_encoded_header(&mut header)?;
        let header_pos = self.output.stream_position()?;
        self.output.write_all(&header)?;
        let crc32 = crc32_of(&header);
        let mut hh = [0u8; SIGNATURE_HEADER_SIZE as usize];
        {
            let mut hhw = hh.as_mut_slice();
            //sig
            hhw.write_all(SEVEN_Z_SIGNATURE)?;
            //version
            hhw.write_u8(0)?;
            hhw.write_u8(4)?;
            //placeholder for crc: index = 8
            hhw.write_u32(0)?;

            // start header
            hhw.write_u64(header_pos - SIGNATURE_HEADER_SIZE)?;
            hhw.write_u64(0xFFFFFFFF & header.len() as u64)?;
            hhw.write_u32(crc32)?;
        }
        let crc32 = crc32_of(&hh[12..]);
        hh[8..12].copy_from_slice(&crc32.to_le_bytes());

        self.output.seek(std::io::SeekFrom::Start(0))?;
        self.output.write_all(&hh)?;
        self.output.flush()?;
        Ok(self.output)
    }

    fn write_header<H: Write>(&mut self, header: &mut H) -> std::io::Result<()> {
        header.write_u8(K_HEADER)?;
        header.write_u8(K_MAIN_STREAMS_INFO)?;
        self.write_streams_info(header)?;
        self.write_files_info(header)?;
        header.write_u8(K_END)?;
        Ok(())
    }

    fn write_encoded_header<H: Write>(&mut self, header: &mut H) -> std::io::Result<()> {
        let mut raw_header = Vec::with_capacity(64 * 1024);
        self.write_header(&mut raw_header)?;
        let mut pack_info = PackInfo::default();

        let position = self.output.stream_position()?;
        let pos = position - SIGNATURE_HEADER_SIZE;
        pack_info.pos = pos;

        let mut more_sizes = vec![];
        let size = raw_header.len() as u64;
        let crc32 = crc32_of(&raw_header);
        let mut methods = vec![];

        let mut must_encrypt_header = false;

        if self.encrypt_header {
            for conf in self.content_methods.iter() {
                if conf.method.id() == EncoderMethod::AES256_SHA256.id() {
                    methods.push(conf.clone());
                    must_encrypt_header = true;
                    break;
                }
            }
        }

        methods.push(EncoderConfiguration::new(EncoderMethod::LZMA));

        // The header's length is known: a dictionary larger than it is never used.
        let methods =
            folder_methods(&Arc::new(methods), Some(size)).map_err(std::io::Error::other)?;

        let mut encoded_data = Vec::with_capacity(size as usize / 2);

        let mut compress_size = 0;
        let mut compressed = CompressWrapWriter::new(&mut encoded_data, &mut compress_size);
        {
            let mut encoder = Self::create_writer(
                &methods,
                &mut compressed,
                &mut more_sizes,
                &mut None,
                encoder::Bcj2Sides::Threads,
            )
            .map_err(std::io::Error::other)?;
            encoder.write_all(&raw_header)?;
            encoder.flush()?;
            let _ = encoder.write(&[])?;
        }

        let compress_crc = compressed.crc_value();
        let compress_size = *compressed.bytes_written;

        if !must_encrypt_header && compress_size as u64 + 20 >= size {
            // We have an unencrypted header and the compression made increased the data size,
            // so we write the raw header data without compressing it to save space.
            header.write_all(&raw_header)?;
            return Ok(());
        }
        self.output.write_all(&encoded_data[..compress_size])?;

        pack_info.add_stream(compress_size as u64, compress_crc);

        let mut unpack_info = UnpackInfo::default();
        let mut sizes = Vec::with_capacity(1 + more_sizes.len());
        sizes.extend(more_sizes.iter().map(|s| s.get() as u64));
        sizes.push(size);
        unpack_info.add(methods, sizes, crc32);

        header.write_u8(K_ENCODED_HEADER)?;

        pack_info.write_to(header)?;
        unpack_info.write_to(header)?;
        unpack_info.write_substreams(header)?;

        header.write_u8(K_END)?;

        Ok(())
    }

    fn write_streams_info<H: Write>(&mut self, header: &mut H) -> std::io::Result<()> {
        if self.pack_info.len() > 0 {
            self.pack_info.write_to(header)?;
            self.unpack_info.write_to(header)?;
        }
        self.unpack_info.write_substreams(header)?;

        header.write_u8(K_END)?;
        Ok(())
    }

    fn write_files_info<H: Write>(&self, header: &mut H) -> std::io::Result<()> {
        header.write_u8(K_FILES_INFO)?;
        write_u64(header, self.files.len() as u64)?;
        self.write_file_empty_streams(header)?;
        self.write_file_empty_files(header)?;
        self.write_file_anti_items(header)?;
        self.write_file_names(header)?;
        self.write_file_ctimes(header)?;
        self.write_file_atimes(header)?;
        self.write_file_mtimes(header)?;
        self.write_file_windows_attrs(header)?;
        header.write_u8(K_END)?;
        Ok(())
    }

    fn write_file_empty_streams<H: Write>(&self, header: &mut H) -> std::io::Result<()> {
        let mut has_empty = false;
        for entry in self.files.iter() {
            if !entry.has_stream {
                has_empty = true;
                break;
            }
        }
        if has_empty {
            header.write_u8(K_EMPTY_STREAM)?;
            let mut bitset = BitSet::with_capacity(self.files.len());
            for (i, entry) in self.files.iter().enumerate() {
                if !entry.has_stream {
                    bitset.insert(i);
                }
            }
            let mut temp: Vec<u8> = Vec::with_capacity(bitset.len() / 8 + 1);
            write_bit_set(&mut temp, &bitset)?;
            write_u64(header, temp.len() as u64)?;
            header.write_all(temp.as_slice())?;
        }
        Ok(())
    }

    fn write_file_empty_files<H: Write>(&self, header: &mut H) -> std::io::Result<()> {
        let mut has_empty = false;
        let mut empty_stream_counter = 0;
        let mut bitset = BitSet::new();
        for entry in self.files.iter() {
            if !entry.has_stream {
                let is_dir = entry.is_directory();
                has_empty |= !is_dir;
                if !is_dir {
                    bitset.insert(empty_stream_counter);
                }
                empty_stream_counter += 1;
            }
        }
        if has_empty {
            header.write_u8(K_EMPTY_FILE)?;

            let mut temp: Vec<u8> = Vec::with_capacity(bitset.len() / 8 + 1);
            write_bit_set(&mut temp, &bitset)?;
            write_u64(header, temp.len() as u64)?;
            header.write_all(&temp)?;
        }
        Ok(())
    }

    fn write_file_anti_items<H: Write>(&self, header: &mut H) -> std::io::Result<()> {
        let mut has_anti = false;
        let mut counter = 0;
        let mut bitset = BitSet::new();
        for entry in self.files.iter() {
            if !entry.has_stream {
                let is_anti = entry.is_anti_item();
                has_anti |= is_anti;
                if is_anti {
                    bitset.insert(counter);
                }
                counter += 1;
            }
        }
        if has_anti {
            header.write_u8(K_ANTI)?;

            let mut temp: Vec<u8> = Vec::with_capacity(bitset.len() / 8 + 1);
            write_bit_set(&mut temp, &bitset)?;
            write_u64(header, temp.len() as u64)?;
            header.write_all(temp.as_slice())?;
        }
        Ok(())
    }

    fn write_file_names<H: Write>(&self, header: &mut H) -> std::io::Result<()> {
        header.write_u8(K_NAME)?;
        let mut temp: Vec<u8> = Vec::with_capacity(128);
        let out = &mut temp;
        out.write_u8(0)?;
        for file in self.files.iter() {
            for c in file.name().encode_utf16() {
                let buf = c.to_le_bytes();
                out.write_all(&buf)?;
            }
            out.write_all(&[0u8; 2])?;
        }
        write_u64(header, temp.len() as u64)?;
        header.write_all(temp.as_slice())?;
        Ok(())
    }

    write_times!(
        write_file_ctimes,
        K_C_TIME,
        has_creation_date,
        creation_date
    );
    write_times!(write_file_atimes, K_A_TIME, has_access_date, access_date);
    write_times!(
        write_file_mtimes,
        K_M_TIME,
        has_last_modified_date,
        last_modified_date
    );
    write_times!(
        write_file_windows_attrs,
        K_WIN_ATTRIBUTES,
        has_windows_attributes,
        windows_attributes,
        write_u32
    );
}

impl<W: Write + Seek> AutoFinish for ArchiveWriter<W> {
    fn finish_ignore_error(self) {
        let _ = self.finish();
    }
}

pub(crate) fn write_u64<W: Write>(header: &mut W, mut value: u64) -> std::io::Result<()> {
    let mut first = 0;
    let mut mask = 0x80;
    let mut i = 0;
    while i < 8 {
        if value < (1u64 << (7 * (i + 1))) {
            first |= value >> (8 * i);
            break;
        }
        first |= mask;
        mask >>= 1;
        i += 1;
    }
    header.write_u8((first & 0xFF) as u8)?;
    while i > 0 {
        header.write_u8((value & 0xFF) as u8)?;
        value >>= 8;
        i -= 1;
    }
    Ok(())
}

/// One non-solid folder, coded: its main pack stream has gone to the output
/// it was coded into, and this is everything else the archive records.
struct EncodedFolder {
    entry: ArchiveEntry,
    methods: Arc<Vec<EncoderConfiguration>>,
    /// The folder's unpacked size: the entry's.
    size: u64,
    /// The unpacked sizes of the coders after the first, in chain order.
    more_sizes: Vec<u64>,
    crc: u32,
    compressed_len: u64,
    compressed_crc: u32,
    bcj2: Option<Bcj2Packed>,
}

/// Codes `entry`'s data from `r` as one folder with `content_methods`, sized
/// for the folder, writing the main pack stream to `output`. `sides` is where
/// a BCJ2 chain's call and jump coders run.
///
/// When the coder the data meets first is LZMA or LZMA2 on one thread,
/// `coder` runs it here, reading `r` itself, and keeps it for the next folder
/// this thread codes (see [`encoder::FolderCoder`]); the bytes are the ones the
/// chain [`ArchiveWriter::create_writer`] builds would write.
///
/// The checksums of the data and of the compressed bytes are taken here, on
/// the thread doing the coding, which is what lets a worker do all of it.
/// Codes one folder whose data-facing coder is `pulled`: that coder runs on
/// this thread and pulls `input` itself, and the rest of `methods` is built
/// after it as `create_writer` builds a chain. Returns the CRC-32 and length
/// of what was read.
#[allow(clippy::too_many_arguments)]
fn encode_pulled<W: Write>(
    methods: &[EncoderConfiguration],
    pulled: &encoder::PulledCoder,
    coder: &mut encoder::FolderCoder,
    input: &mut dyn Read,
    out: &mut W,
    more_sizes: &mut Vec<Rc<Cell<usize>>>,
    bcj2: &mut Option<encoder::Bcj2Slot>,
    sides: encoder::Bcj2Sides,
    error: impl Fn(std::io::Error) -> Error,
) -> Result<(u32, usize)> {
    // The data meets the last method first. The chain after it is built as
    // `create_writer` builds it, and the counter `create_writer` would put
    // under the pulled coder - the size of what it codes into the next
    // coder - is put there here.
    let rest = &methods[..methods.len() - 1];
    let mut chain: Box<dyn Write + '_> = if rest.is_empty() {
        Box::new(out)
    } else {
        let rest = ArchiveWriter::<std::io::Cursor<Vec<u8>>>::create_writer(
            rest, out, more_sizes, bcj2, sides,
        )?;
        let counting = CountingWriter::new(rest);
        more_sizes.push(counting.counting());
        Box::new(counting)
    };
    let mut read_len = 0;
    let mut input = CrcReader {
        inner: input,
        crc: Crc32::new(),
        read: &mut read_len,
    };
    coder
        .encode(pulled, &mut input, &mut chain)
        .map_err(&error)?;
    chain.flush().map_err(&error)?;
    // The empty write ends every coder after this one.
    chain.write(&[]).map_err(&error)?;
    drop(chain);
    Ok((input.crc.finalize(), read_len))
}

fn encode_entry<R: Read, O: Write>(
    content_methods: &Arc<Vec<EncoderConfiguration>>,
    entry: ArchiveEntry,
    r: &mut R,
    output: O,
    sides: encoder::Bcj2Sides,
    coder: &mut encoder::FolderCoder,
) -> Result<EncodedFolder> {
    let mut compressed_len = 0;
    let mut compressed = CompressWrapWriter::new(output, &mut compressed_len);
    let mut out = std::io::BufWriter::with_capacity(OUTPUT_BUF_LEN, &mut compressed);

    let mut more_sizes: Vec<Rc<Cell<usize>>> = Vec::with_capacity(content_methods.len() - 1);
    let (head, folder) = folder_size([&entry], r)
        .map_err(|e| Error::io_msg(e, format!("Encode entry:{}", entry.name())))?;
    let methods = folder_methods(content_methods, folder)?;
    let mut bcj2 = None;
    let pulled = match methods.split_last() {
        Some((first, _)) => encoder::FolderCoder::plan(first)?,
        None => None,
    };

    let (crc, size) = if let Some(pulled) = pulled {
        encode_pulled(
            &methods,
            &pulled,
            coder,
            &mut head.as_slice().chain(&mut *r),
            &mut out,
            &mut more_sizes,
            &mut bcj2,
            sides,
            |e| Error::io_msg(e, format!("Encode entry:{}", entry.name())),
        )?
    } else {
        let mut w = ArchiveWriter::<std::io::Cursor<Vec<u8>>>::create_writer(
            &methods,
            &mut out,
            &mut more_sizes,
            &mut bcj2,
            sides,
        )?;
        let mut write_len = 0;
        let mut w = CompressWrapWriter::new(&mut w, &mut write_len);
        w.write_all(&head)
            .map_err(|e| Error::io_msg(e, format!("Encode entry:{}", entry.name())))?;
        drop(head);
        let mut buf = vec![0u8; SOURCE_READ_LEN];
        loop {
            match r.read(&mut buf) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    w.write_all(&buf[..n])
                        .map_err(|e| Error::io_msg(e, format!("Encode entry:{}", entry.name())))?;
                }
                Err(e) => {
                    return Err(Error::io_msg(e, format!("Encode entry:{}", entry.name())));
                }
            }
        }
        w.flush()
            .map_err(|e| Error::io_msg(e, format!("Encode entry:{}", entry.name())))?;
        w.write(&[])
            .map_err(|e| Error::io_msg(e, format!("Encode entry:{}", entry.name())))?;

        (w.crc_value(), write_len)
    };
    out.flush()
        .map_err(|e| Error::io_msg(e, format!("Encode entry:{}", entry.name())))?;
    drop(out);
    let compressed_crc = compressed.crc_value();
    drop(compressed);
    Ok(EncodedFolder {
        entry,
        methods,
        size: size as u64,
        more_sizes: more_sizes.iter().map(|s| s.get() as u64).collect(),
        crc,
        compressed_len: compressed_len as u64,
        compressed_crc,
        bcj2: Bcj2Packed::take(bcj2),
    })
}

/// What each encode worker may stage, compressed, before it waits for the
/// writer: per folder, with up to [`ENCODE_WINDOW_PER_WORKER`] folders per
/// worker.
#[cfg(not(target_arch = "wasm32"))]
const ENCODE_STAGE_BYTES: usize = 8 << 20;

/// Folders each encode worker may be ahead of the writer: the one it is
/// coding and one finished and waiting.
#[cfg(not(target_arch = "wasm32"))]
const ENCODE_WINDOW_PER_WORKER: usize = 2;

/// A folder a worker coded, or an entry with no data to code.
#[cfg(not(target_arch = "wasm32"))]
enum FolderOut {
    /// The folder, with what its BCJ2 streams declared once they were sent.
    Encoded(EncodedFolder, Option<Box<Bcj2Shipped>>),
    Unstreamed(ArchiveEntry),
}

#[cfg(not(target_arch = "wasm32"))]
enum EncodeMessage {
    /// The next compressed bytes of the folder's main pack stream.
    Data(Vec<u8>),
    /// The next bytes of one of a BCJ2 folder's other pack streams: `stream`
    /// is 0 for rc, 1 for call, 2 for jump, and they come in that order,
    /// after the main stream.
    Tail { stream: usize, bytes: Vec<u8> },
    /// The folder is coded.
    Done(Box<Result<FolderOut>>),
}

/// What a BCJ2 folder's other three pack streams declared once a worker had
/// sent their bytes through the stage: their checksums, in pack order, and
/// the call and jump streams' sizes before their coders.
#[cfg(not(target_arch = "wasm32"))]
struct Bcj2Shipped {
    crcs: [u32; 3],
    sizes: Bcj2SideSizes,
}

#[cfg(not(target_arch = "wasm32"))]
impl Bcj2Shipped {
    /// The streams as the writer wrote them, `lens` long.
    fn written(self, lens: [u64; 3]) -> Bcj2Written {
        Bcj2Written {
            streams: [0, 1, 2].map(|i| (lens[i], self.crcs[i])),
            sizes: self.sizes,
        }
    }
}

/// Sends a BCJ2 folder's rc, call and jump streams through the stage, in
/// that order, in pieces of at most `chunk` bytes, each charged its length.
#[cfg(not(target_arch = "wasm32"))]
fn send_bcj2_tail(
    bcj2: Bcj2Packed,
    chunk: usize,
    mut send: impl FnMut(EncodeMessage, usize) -> std::result::Result<(), crate::ordered::Cancelled>,
) -> std::result::Result<Bcj2Shipped, crate::ordered::Cancelled> {
    let Bcj2Packed { streams, sizes } = bcj2;
    let mut crcs = [0u32; 3];
    for (stream, (bytes, crc)) in streams.into_iter().enumerate() {
        crcs[stream] = crc;
        for piece in bytes.chunks(chunk.max(1)) {
            send(
                EncodeMessage::Tail {
                    stream,
                    bytes: piece.to_vec(),
                },
                piece.len(),
            )?;
        }
    }
    Ok(Bcj2Shipped { crcs, sizes })
}

/// The output an encode worker codes into: the folder's stage.
#[cfg(not(target_arch = "wasm32"))]
struct StageSink<'t, 'a> {
    tx: &'t mut crate::ordered::Sender<'a, EncodeMessage>,
    cancelled: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl Write for StageSink<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self
            .tx
            .send(EncodeMessage::Data(buf.to_vec()), buf.len())
            .is_err()
        {
            self.cancelled = true;
            return Err(std::io::Error::other("folder encode cancelled"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A folder's input on its way to a coder that reads it: the checksum and
/// length of what was read, as [`CompressWrapWriter`] takes them of what is
/// written into a coder.
struct CrcReader<'a, R> {
    inner: R,
    crc: Crc32,
    read: &'a mut usize,
}

impl<R: Read> Read for CrcReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc.update(&buf[..n]);
        *self.read += n;
        Ok(n)
    }
}

struct CompressWrapWriter<'a, W> {
    writer: W,
    crc: Crc32,
    bytes_written: &'a mut usize,
}

impl<'a, W: Write> CompressWrapWriter<'a, W> {
    pub fn new(writer: W, bytes_written: &'a mut usize) -> Self {
        Self {
            writer,
            crc: Crc32::new(),
            bytes_written,
        }
    }

    pub fn crc_value(&mut self) -> u32 {
        let crc = std::mem::replace(&mut self.crc, Crc32::new());
        crc.finalize()
    }
}

impl<W: Write> Write for CompressWrapWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = self.writer.write(buf)?;
        self.crc.update(&buf[..len]);
        *self.bytes_written += len;
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// A solid block compressed away from any writer, ready to be appended by
/// [`ArchiveWriter::push_prepared_block`].
///
/// **This holds the entire compressed block in memory** until it is pushed, where
/// [`ArchiveWriter::push_archive_entries`] streams into the output as it encodes. That is inherent
/// to preparing a block off-thread, and it is what a caller sizing its batches is signing up for:
/// peak memory is roughly the compressed size of every block in flight at once.
#[derive(Debug)]
pub struct PreparedBlock {
    compressed: Vec<u8>,
    compressed_crc: u32,
    entries: Vec<ArchiveEntry>,
    methods: Arc<Vec<EncoderConfiguration>>,
    sizes: Vec<u64>,
    crc: u32,
    sub_stream_sizes: Vec<u64>,
    sub_stream_crcs: Vec<u32>,
    bcj2: Option<Bcj2Packed>,
}

/// The three pack streams a BCJ2 block has after its main one, in the order
/// 7-Zip writes them - rc, call, jump - each with its CRC, and the call and
/// jump streams' sizes before their coders.
#[derive(Debug)]
struct Bcj2Packed {
    streams: [(Vec<u8>, u32); 3],
    sizes: Bcj2SideSizes,
}

impl Bcj2Packed {
    /// Takes the tail a finished BCJ2 chain left in its slot, checksumming it
    /// here, on the thread that encoded the block.
    fn take(slot: Option<encoder::Bcj2Slot>) -> Option<Self> {
        let tail = slot?.borrow_mut().take()?;
        let sizes = Bcj2SideSizes {
            call: tail.call_size,
            jump: tail.jump_size,
        };
        let streams = [tail.rc, tail.call, tail.jump].map(|bytes| {
            let crc = crc32_of(&bytes);
            (bytes, crc)
        });
        Some(Self { streams, sizes })
    }

    fn len(&self) -> usize {
        self.streams.iter().map(|(bytes, _)| bytes.len()).sum()
    }

    /// What the streams take once written.
    fn written(&self) -> Bcj2Written {
        Bcj2Written {
            streams: self
                .streams
                .each_ref()
                .map(|(bytes, crc)| (bytes.len() as u64, *crc)),
            sizes: self.sizes,
        }
    }
}

/// A BCJ2 block's rc, call and jump pack streams once written: each one's
/// length and CRC, in pack order, and the call and jump streams' sizes before
/// their coders.
struct Bcj2Written {
    streams: [(u64, u32); 3],
    sizes: Bcj2SideSizes,
}

impl PreparedBlock {
    /// Compressed size in bytes, before it is appended: every pack stream
    /// the block will add, which for a BCJ2 block is four.
    pub fn compressed_len(&self) -> usize {
        self.compressed.len() + self.bcj2.as_ref().map_or(0, Bcj2Packed::len)
    }

    /// Number of entries in the block.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the block holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Compress `entries` into one solid block using `methods`, without a writer.
///
/// Mirrors the encoding half of [`ArchiveWriter::push_archive_entries`], writing into memory instead
/// of the archive. Safe to call on a worker thread; the result is appended later with
/// [`ArchiveWriter::push_prepared_block`], which is what fixes the order.
///
/// Returns an error if `methods` is empty, or if `entries` and `reader` differ in length.
pub fn prepare_block<R: Read>(
    methods: Arc<Vec<EncoderConfiguration>>,
    entries: Vec<ArchiveEntry>,
    reader: Vec<SourceReader<R>>,
) -> Result<PreparedBlock> {
    if methods.is_empty() {
        return Err(Error::other("prepare_block: `methods` must not be empty"));
    }

    let mut entries = entries;
    let mut r = SeqReader::new(reader);
    if r.reader_len() != entries.len() {
        return Err(Error::other(format!(
            "prepare_block: {} entries against {} readers",
            entries.len(),
            r.reader_len()
        )));
    }

    let (head, folder) = folder_size(&entries, &mut r).map_err(|e| {
        Error::io_msg(
            e,
            format!("prepare_block: read source:{}", entries_names(&entries)),
        )
    })?;
    let methods = folder_methods(&methods, folder)?;
    let mut out: Vec<u8> = Vec::new();
    let mut more_sizes: Vec<Rc<Cell<usize>>> = Vec::with_capacity(methods.len() - 1);
    let mut bcj2 = None;

    let (crc, size, compressed_crc) = {
        // Outer wrapper: the CRC of the compressed bytes, computed as they are produced on this
        // thread. `push_prepared_block` would otherwise have to make a second pass over the whole
        // buffer on the serialized side, which is the work this function exists to move off it.
        let mut compressed_len = 0;
        let mut compressed = CompressWrapWriter::new(&mut out, &mut compressed_len);
        let (crc, size) = {
            let mut w = ArchiveWriter::<std::io::Cursor<Vec<u8>>>::create_writer(
                &methods,
                &mut compressed,
                &mut more_sizes,
                &mut bcj2,
                encoder::Bcj2Sides::Threads,
            )?;
            let mut write_len = 0;
            let mut w = CompressWrapWriter::new(&mut w, &mut write_len);
            w.write_all(&head).map_err(|e| {
                Error::io_msg(
                    e,
                    format!("prepare_block: encode:{}", entries_names(&entries)),
                )
            })?;
            drop(head);
            let mut buf = vec![0u8; SOURCE_READ_LEN];
            loop {
                let n = r.read(&mut buf).map_err(|e| {
                    Error::io_msg(
                        e,
                        format!("prepare_block: read source:{}", entries_names(&entries)),
                    )
                })?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n]).map_err(|e| {
                    Error::io_msg(
                        e,
                        format!("prepare_block: encode:{}", entries_names(&entries)),
                    )
                })?;
            }
            w.flush().map_err(|e| {
                Error::io_msg(
                    e,
                    format!("prepare_block: flush:{}", entries_names(&entries)),
                )
            })?;
            w.write(&[]).map_err(|e| {
                Error::io_msg(
                    e,
                    format!("prepare_block: finish:{}", entries_names(&entries)),
                )
            })?;
            (w.crc_value(), write_len)
        };
        (crc, size, compressed.crc_value())
    };

    let mut sub_stream_crcs = Vec::with_capacity(entries.len());
    let mut sub_stream_sizes = Vec::with_capacity(entries.len());
    for i in 0..entries.len() {
        let entry = &mut entries[i];
        let ri = &r[i];
        entry.crc = ri.crc_value() as u64;
        entry.size = ri.read_count() as u64;
        sub_stream_crcs.push(entry.crc as u32);
        sub_stream_sizes.push(entry.size);
        entry.has_crc = true;
    }

    let mut sizes = Vec::with_capacity(more_sizes.len() + 1);
    sizes.extend(more_sizes.iter().map(|s| s.get() as u64));
    sizes.push(size as u64);

    Ok(PreparedBlock {
        compressed: out,
        compressed_crc,
        entries,
        methods,
        sizes,
        crc,
        sub_stream_sizes,
        sub_stream_crcs,
        bcj2: Bcj2Packed::take(bcj2),
    })
}

#[cfg(test)]
mod bcj2_tests {
    use std::io::Cursor;

    use crate::{
        ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod, Password,
        block::BindPair, encoder_options::DeltaOptions,
    };

    fn pseudo_x86(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
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
            out[i + 1..i + 5].copy_from_slice(&((i as u32 * 5) % 0x8000).to_le_bytes());
            i += 11;
        }
        out
    }

    fn archive(methods: Vec<EncoderConfiguration>, data: &[u8]) -> Vec<u8> {
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(methods);
        writer
            .push_archive_entry(ArchiveEntry::new_file("amber_quarry/tool.exe"), Some(data))
            .unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn bp(in_index: u64, out_index: u64) -> BindPair {
        BindPair {
            in_index,
            out_index,
        }
    }

    /// The folder is the one 7-Zip writes for `-mf=BCJ2`: two LZMA coders
    /// with `lc0 lp2` and a 1 MiB dictionary for jump and call, the main
    /// coder, then BCJ2 with four inputs; BCJ2's inputs bound to coders 2, 1
    /// and 0; pack streams main, rc, call, jump.
    #[test]
    fn the_folder_is_seven_zips_four_stream_layout() {
        let data = pseudo_x86(200 * 1024);
        let bytes = archive(
            vec![
                EncoderMethod::LZMA2.into(),
                EncoderMethod::BCJ2_FILTER.into(),
            ],
            &data,
        );
        let reader = ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
        let block = &reader.archive().blocks[0];

        let ids: Vec<&[u8]> = block.coders.iter().map(|c| c.encoder_method_id()).collect();
        assert_eq!(
            ids,
            [
                EncoderMethod::ID_LZMA,
                EncoderMethod::ID_LZMA,
                EncoderMethod::ID_LZMA2,
                EncoderMethod::ID_BCJ2
            ]
        );
        for side in &block.coders[..2] {
            assert_eq!(side.properties(), [0x6C, 0x00, 0x00, 0x10, 0x00]);
            assert_eq!((side.num_in_streams, side.num_out_streams), (1, 1));
        }
        let bcj2 = &block.coders[3];
        assert_eq!((bcj2.num_in_streams, bcj2.num_out_streams), (4, 1));
        assert!(bcj2.properties().is_empty());
        assert_eq!(block.total_input_streams, 7);
        assert_eq!(block.total_output_streams, 4);
        assert_eq!(block.bind_pairs, [bp(5, 0), bp(4, 1), bp(3, 2)]);
        assert_eq!(block.packed_streams, [2, 6, 1, 0]);

        let [jump, call, main, total] = block.unpack_sizes[..] else {
            panic!("four unpack sizes: {:?}", block.unpack_sizes);
        };
        assert_eq!(total, data.len() as u64);
        assert!(jump > 0 && call > 0, "jump {jump} call {call}");
        assert_eq!(jump % 4, 0);
        assert_eq!(call % 4, 0);
        // Every converted branch moves its four-byte target out of main.
        assert_eq!(main + call + jump, total);
        assert_eq!(reader.archive().block_pack_streams(0).len(), 4);
    }

    /// Two coders on the main stream chain to each other below BCJ2, and the
    /// archive still decodes.
    #[test]
    fn a_main_chain_of_two_coders_is_bound_in_line() {
        let data = pseudo_x86(64 * 1024);
        let bytes = archive(
            vec![
                EncoderMethod::LZMA2.into(),
                DeltaOptions::from_distance(4).into(),
                EncoderMethod::BCJ2_FILTER.into(),
            ],
            &data,
        );
        let mut reader =
            ArchiveReader::new(Cursor::new(bytes.as_slice()), Password::empty()).unwrap();
        let block = &reader.archive().blocks[0];
        assert_eq!(block.coders.len(), 5);
        assert_eq!(block.coders[4].num_in_streams, 4);
        assert_eq!(block.bind_pairs, [bp(6, 0), bp(5, 1), bp(4, 3), bp(3, 2)]);
        assert_eq!(block.packed_streams, [2, 7, 1, 0]);
        assert!(reader.read_file("amber_quarry/tool.exe").unwrap() == data);
    }
}

#[cfg(test)]
mod folder_size_tests {
    use super::declared_size;
    use crate::ArchiveEntry;

    fn file(size: u64) -> ArchiveEntry {
        let mut entry = ArchiveEntry::new_file("f");
        entry.size = size;
        entry
    }

    #[test]
    fn a_folder_is_sized_only_when_every_file_declares_its_size() {
        assert_eq!(declared_size(&[file(10), file(20)]), Some(30));
        assert_eq!(
            declared_size(&[file(10), ArchiveEntry::new_directory("d"), file(5)]),
            Some(15)
        );
        // `new_file`'s zero is "unknown": the folder may hold far more than 10 bytes.
        assert_eq!(declared_size(&[file(10), file(0)]), None);
        assert_eq!(declared_size(&[file(0)]), None);
        assert_eq!(declared_size(&[ArchiveEntry::new_directory("d")]), None);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod bcj2_tail_stage_tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::{Bcj2Packed, EncodeMessage, send_bcj2_tail};
    use crate::{ordered::run, writer::unpack_info::Bcj2SideSizes};

    fn tail(job: usize) -> Bcj2Packed {
        let stream = |len: usize, salt: u8| -> (Vec<u8>, u32) {
            (
                (0..len).map(|i| (i as u8) ^ salt ^ job as u8).collect(),
                salt.into(),
            )
        };
        Bcj2Packed {
            streams: [
                stream(40_000 + job, 1),
                stream(3_000, 2),
                stream(25_000 + 7 * job, 3),
            ],
            sizes: Bcj2SideSizes::default(),
        }
    }

    /// A BCJ2 folder's three side streams go through the stage like its main
    /// stream: whatever the writer's pace, what the workers have parked never
    /// passes the window times the cap, plus the one piece each worker and
    /// the writer may be holding between the queue and the count. Before,
    /// the streams rode whole on the folder's last message, which an empty
    /// queue takes at once, so each folder could park all of them.
    #[test]
    fn a_bcj2_tail_never_parks_more_than_the_window_times_the_cap() {
        let staged = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let (window, cap, chunk, workers, jobs) = (4, 8_192, 2_048, 2, 10);
        let got = Mutex::new(vec![Vec::new(); jobs]);
        let outcome = run::<EncodeMessage, (), _, _>(
            jobs,
            workers,
            window,
            cap,
            |job, tx| {
                let shipped = send_bcj2_tail(tail(job), chunk, |message, weight| {
                    assert!(weight <= chunk);
                    let now = staged.fetch_add(weight, Ordering::SeqCst) + weight;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tx.send(message, weight)
                });
                assert!(shipped.is_ok());
            },
            |job, rx| {
                let mut streams = [Vec::new(), Vec::new(), Vec::new()];
                while let Some(message) = rx.recv() {
                    let EncodeMessage::Tail { stream, bytes } = message else {
                        unreachable!("only tails are sent here");
                    };
                    staged.fetch_sub(bytes.len(), Ordering::SeqCst);
                    streams[stream].extend_from_slice(&bytes);
                }
                got.lock().unwrap()[job] = streams.to_vec();
                Ok(())
            },
        );
        assert!(matches!(outcome, Some(Ok(()))));
        assert!(
            peak.load(Ordering::SeqCst) <= window * cap + (workers + 1) * chunk,
            "parked {} bytes",
            peak.load(Ordering::SeqCst)
        );
        // The writer gets every stream back whole and in pack order.
        let got = got.into_inner().unwrap();
        for (job, streams) in got.iter().enumerate() {
            let want = tail(job);
            for (i, (bytes, _)) in want.streams.iter().enumerate() {
                assert!(streams[i] == *bytes, "job {job} stream {i}");
            }
        }
    }
}
