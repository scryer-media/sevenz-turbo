use super::*;

/// An owned block decode that can move to a caller's worker pool.
///
/// Every decode owns an independent positional cursor. The archive and password
/// are shared without cloning their contents; sharing the same `Arc<Password>`
/// also shares its lifetime KDF budget and derived-key cache. No worker is
/// spawned until a codec's explicitly requested thread count requires one.
///
/// The source must provide the same archive image that was parsed. A streaming
/// consumer is responsible for admitting only covered packed ranges and for
/// invalidating or cancelling work when those bytes are replaced by repair.
/// Callback errors stop the decode and pass through unchanged, as they do for
/// [`BlockDecoder`]. Codec workers are joined when their decode stack drops.
pub struct PositionalBlockDecoder {
    archive: Arc<Archive>,
    password: Arc<Password>,
    source: Arc<dyn ReadAt>,
    block_index: usize,
    limits: ArchiveLimits,
    thread_count: u32,
    adaptive_lzma2: bool,
    verify_checksums: bool,
    lzma2: Arc<Lzma2Control>,
    on_sub_stream_complete: Option<Box<dyn FnMut(SubStreamCompletion) + Send>>,
}

impl PositionalBlockDecoder {
    /// Prepares one block over a parsed archive and an independent cursor.
    ///
    /// Defaults to one codec thread and checksum verification. This allocates
    /// no decoder dictionary and reads no packed bytes until the entry walk.
    ///
    /// # Errors
    /// Returns an error if `block_index` is outside the parsed archive or its
    /// data members are interspersed with no-stream entries. The latter cannot
    /// be represented by the underlying block walk and requires another path.
    pub fn new(
        archive: Arc<Archive>,
        password: Arc<Password>,
        source: Arc<dyn ReadAt>,
        block_index: usize,
    ) -> Result<Self, Error> {
        Self::with_limits(
            archive,
            password,
            source,
            block_index,
            ArchiveLimits::default(),
        )
    }

    /// As [`Self::new`], enforcing the same decoder and KDF limits as
    /// [`BlockDecoder::with_limits`]. Concurrent decodes each obey these limits;
    /// the caller must also bound their combined memory and worker count.
    ///
    /// # Errors
    /// As [`Self::new`].
    pub fn with_limits(
        archive: Arc<Archive>,
        password: Arc<Password>,
        source: Arc<dyn ReadAt>,
        block_index: usize,
        limits: ArchiveLimits,
    ) -> Result<Self, Error> {
        if block_index >= archive.blocks.len() {
            return Err(Error::invalid_argument("block index out of range"));
        }
        let start = *archive
            .stream_map
            .block_first_file_index
            .get(block_index)
            .ok_or_else(|| Error::corrupted("missing block file mapping"))?;
        let count = archive.blocks[block_index].num_unpack_sub_streams;
        let entries = start
            .checked_add(count)
            .and_then(|end| archive.files.get(start..end))
            .ok_or_else(|| Error::corrupted("invalid block file mapping"))?;
        if entries.iter().enumerate().any(|(offset, entry)| {
            !entry.has_stream
                || archive.stream_map.file_block_index.get(start + offset)
                    != Some(&Some(block_index))
        }) {
            return Err(Error::unsupported(
                "positional block walk requires contiguous data members",
            ));
        }
        Ok(Self {
            archive,
            password,
            source,
            block_index,
            limits,
            thread_count: 1,
            adaptive_lzma2: false,
            verify_checksums: true,
            lzma2: Arc::new(Lzma2Control::new(1)),
            on_sub_stream_complete: None,
        })
    }

    /// Sets this block's codec thread count, clamped to `1..=256`.
    pub fn set_threads(&mut self, threads: u32) {
        self.thread_count = threads.clamp(1, 256);
        self.lzma2.set_threads(self.thread_count);
    }

    /// [`Self::set_threads`] for a builder chain.
    #[must_use]
    pub fn with_threads(mut self, threads: u32) -> Self {
        self.set_threads(threads);
        self
    }

    /// Enables later LZMA2 widening through [`Self::lzma2_handle`].
    #[must_use]
    pub fn with_adaptive_lzma2(mut self) -> Self {
        self.adaptive_lzma2 = true;
        self
    }

    /// Controls checksum verification, as on [`BlockDecoder`].
    #[must_use]
    pub fn with_verify_checksums(mut self, verify: bool) -> Self {
        self.verify_checksums = verify;
        self
    }

    /// Returns the control handle that remains usable while this decode runs.
    pub fn lzma2_handle(&self) -> Lzma2Handle {
        Lzma2Handle {
            control: Arc::clone(&self.lzma2),
        }
    }

    /// Installs the verified sub-stream completion hook for this block.
    /// Hooks run on the thread executing the entry walk, in file order.
    pub fn set_sub_stream_complete_hook(
        &mut self,
        hook: impl FnMut(SubStreamCompletion) + Send + 'static,
    ) {
        self.on_sub_stream_complete = Some(Box::new(hook));
    }

    /// Entries in this block, in archive order.
    pub fn entries(&self) -> &[ArchiveEntry] {
        let start = self.archive.stream_map.block_first_file_index[self.block_index];
        let count = self.archive.blocks[self.block_index].num_unpack_sub_streams;
        &self.archive.files[start..start + count]
    }

    /// Decodes on the calling thread, with ordinary [`Read`] callbacks.
    /// Returns `false` when a callback stops the walk early.
    ///
    /// # Errors
    /// The same located codec/checksum errors and unchanged callback errors
    /// as [`BlockDecoder::for_each_entries`].
    pub fn for_each_entries<F>(self, each: &mut F) -> Result<bool, Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn Read) -> Result<bool, Error>,
    {
        self.for_each_entries_direct(&mut |entry, reader| each(entry, reader))
    }

    /// Decodes on the calling thread with direct output delivery.
    ///
    /// # Errors
    /// As [`Self::for_each_entries`].
    pub fn for_each_entries_direct<F>(mut self, each: &mut F) -> Result<bool, Error>
    where
        F: FnMut(&ArchiveEntry, &mut dyn EntryRead) -> Result<bool, Error>,
    {
        let mut cursor = ReadAtCursor::new(Arc::clone(&self.source));
        let decoder = BlockDecoder {
            thread_count: self.thread_count,
            adaptive_lzma2: self.adaptive_lzma2,
            // Positional ownership alone does not certify download coverage.
            input_complete: false,
            positional: None,
            verify_checksums: self.verify_checksums,
            lzma2: self.lzma2,
            block_index: self.block_index,
            archive: &self.archive,
            password: &self.password,
            source: &mut cursor,
            limits: self.limits,
            on_sub_stream_complete: self
                .on_sub_stream_complete
                .as_mut()
                .map(|hook| &mut **hook as &mut (dyn FnMut(SubStreamCompletion) + Send)),
        };
        decoder.for_each_entries_direct(each)
    }
}
