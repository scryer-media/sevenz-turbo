use std::{borrow::Cow, fmt::Display};

/// The error type of the crate.
#[derive(Debug)]
pub enum Error {
    /// Invalid 7z signature found in file header.
    BadSignature([u8; 6]),
    /// Unsupported 7z format version.
    UnsupportedVersion {
        /// Major version number.
        major: u8,
        /// Minor version number.
        minor: u8,
    },
    /// Checksum verification failed during decompression.
    ChecksumVerificationFailed,
    /// Next header CRC mismatch.
    NextHeaderCrcMismatch,
    /// IO error with optional context message.
    Io(std::io::Error, Cow<'static, str>),
    /// Error opening file.
    FileOpen(std::io::Error, String),
    /// Other error with description.
    Other(Cow<'static, str>),
    /// Bad terminated streams info.
    BadTerminatedStreamsInfo(u8),
    /// Bad terminated unpack info.
    BadTerminatedUnpackInfo,
    /// Bad terminated pack info.
    BadTerminatedPackInfo(u8),
    /// Bad terminated sub streams info.
    BadTerminatedSubStreamsInfo,
    /// Bad terminated header.
    BadTerminatedHeader(u8),
    /// External compression method not supported.
    ExternalUnsupported,
    /// Unsupported compression method.
    UnsupportedCompressionMethod(String),
    /// Memory limit exceeded.
    MaxMemLimited {
        /// Maximum allowed memory in KB.
        max_kb: usize,
        /// Actual required memory in KB.
        actaul_kb: usize,
    },
    /// Password required for encrypted archive.
    PasswordRequired,
    /// Feature or operation not supported.
    Unsupported(Cow<'static, str>),
    /// Possibly bad password for encrypted content.
    MaybeBadPassword(std::io::Error),
    /// File not found.
    FileNotFound,
    /// The archive's declared end header is larger than the caller's limit.
    ///
    /// The size comes out of the first 32 bytes of the file and the reader has
    /// to buffer that many bytes to parse the header, so it is checked before
    /// the allocation rather than after it. See [`ArchiveLimits`].
    ///
    /// [`ArchiveLimits`]: crate::ArchiveLimits
    EndHeaderTooLarge {
        /// The caller's limit, in bytes.
        limit_bytes: u64,
        /// What the archive declared, in bytes.
        declared_bytes: u64,
    },
    /// The archive's decoders need more memory than the caller's limit.
    ///
    /// Raised by [`ArchiveReader::with_limits`] from the estimate in
    /// [`Archive::decoder_memory_estimate`], before a dictionary is allocated.
    ///
    /// [`ArchiveReader::with_limits`]: crate::ArchiveReader::with_limits
    /// [`Archive::decoder_memory_estimate`]: crate::Archive::decoder_memory_estimate
    MemoryLimited {
        /// The caller's limit, in bytes.
        limit_bytes: u64,
        /// What the archive's coders need, in bytes.
        required_bytes: u64,
    },
    /// The archive asked for more than a limit in [`ArchiveLimits`] allows.
    ///
    /// Raised before the allocation or the work it bounds, so `requested` is
    /// what the *archive declared*, not what was reached: nothing of that size
    /// was allocated, decoded or derived. `what` names the bound, so a
    /// consumer can tell "this archive wants more memory than we give it" from
    /// "this archive claims four billion files".
    ///
    /// [`ArchiveLimits`]: crate::ArchiveLimits
    LimitExceeded {
        /// Which bound was hit.
        what: Limit,
        /// The limit in force, in the unit of `what`.
        limit: u64,
        /// What the archive declared, in the same unit.
        requested: u64,
    },
    /// An entry's stored name would escape the extraction directory, and the
    /// caller asked for such an archive to be refused.
    ///
    /// Only raised when [`ArchiveLimits::reject_unsafe_paths`] is set. A
    /// consumer that lists rather than extracts leaves it off and asks
    /// [`ArchiveEntry::is_unsafe_path`] per entry instead.
    ///
    /// [`ArchiveLimits::reject_unsafe_paths`]: crate::ArchiveLimits::reject_unsafe_paths
    /// [`ArchiveEntry::is_unsafe_path`]: crate::ArchiveEntry::is_unsafe_path
    UnsafeEntryName {
        /// The name, as the archive stores it.
        name: String,
        /// Which way it is unsafe.
        reason: &'static str,
    },
    /// A block failed to decode, with enough context to say which bytes.
    ///
    /// This is what separates "this archive is damaged, and here is where" from
    /// an I/O failure on the source or a method this build cannot decode; the
    /// [`kind`](BlockErrorKind) says which, and `packed_offset` is the absolute
    /// file offset of the block's first packed stream, so a caller can name the
    /// damaged region without re-parsing the header.
    BlockDecode {
        /// Index of the block (7-Zip calls it a folder) in [`Archive::blocks`].
        ///
        /// [`Archive::blocks`]: crate::Archive::blocks
        block_index: usize,
        /// Absolute offset of the block's first packed stream in the archive.
        packed_offset: u64,
        /// What went wrong.
        kind: BlockErrorKind,
        /// The underlying error, already rendered.
        message: String,
    },
}

/// Which bound an [`Error::LimitExceeded`] hit.
///
/// The field on [`ArchiveLimits`] of the same name documents what the bound is
/// for and what the default is.
///
/// [`ArchiveLimits`]: crate::ArchiveLimits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Limit {
    /// `max_end_header_bytes`, in bytes.
    EndHeaderBytes,
    /// `max_header_unpacked_bytes`, in bytes.
    HeaderUnpackedBytes,
    /// `max_header_depth`, in levels of nesting.
    HeaderDepth,
    /// `max_entries`, in entries — of files, blocks, pack streams,
    /// sub-streams, bind pairs or coders.
    Entries,
    /// `max_name_bytes`, in bytes of one stored name.
    NameBytes,
    /// `max_total_name_bytes`, in bytes of the whole names blob.
    TotalNameBytes,
    /// `max_coders_per_block`, in coders.
    CodersPerBlock,
    /// `max_streams_per_coder`, in streams of one coder.
    StreamsPerCoder,
    /// `max_total_coders`, in coders.
    TotalCoders,
    /// `memory_limit_bytes`, in bytes.
    MemoryBytes,
    /// `max_unpack_bytes`, in bytes.
    UnpackBytes,
    /// `max_unpack_ratio`, as output bytes per packed byte.
    UnpackRatio,
    /// `max_aes_cycles_power`, as the exponent itself.
    AesCyclesPower,
    /// `max_aes_kdf_rounds`, across the archive and encoded header.
    AesKdfRounds,
    /// The archive's own structure, rather than a caller's limit: a count,
    /// size or offset that the bytes present cannot possibly support.
    ///
    /// `limit` is what the archive could support and `requested` what it
    /// claimed.
    ArchiveBytes,
}

impl Limit {
    /// The name of the [`ArchiveLimits`] field, for messages.
    ///
    /// [`ArchiveLimits`]: crate::ArchiveLimits
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::EndHeaderBytes => "max_end_header_bytes",
            Self::HeaderUnpackedBytes => "max_header_unpacked_bytes",
            Self::HeaderDepth => "max_header_depth",
            Self::Entries => "max_entries",
            Self::NameBytes => "max_name_bytes",
            Self::TotalNameBytes => "max_total_name_bytes",
            Self::CodersPerBlock => "max_coders_per_block",
            Self::StreamsPerCoder => "max_streams_per_coder",
            Self::TotalCoders => "max_total_coders",
            Self::MemoryBytes => "memory_limit_bytes",
            Self::UnpackBytes => "max_unpack_bytes",
            Self::UnpackRatio => "max_unpack_ratio",
            Self::AesCyclesPower => "max_aes_cycles_power",
            Self::AesKdfRounds => "max_aes_kdf_rounds",
            Self::ArchiveBytes => "the bytes the archive has",
        }
    }
}

/// What went wrong in an [`Error::BlockDecode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockErrorKind {
    /// The packed bytes are not what the coder expected: the archive is
    /// damaged or truncated, or it is not the archive the caller thinks.
    Corrupted,
    /// The block decoded, but its CRC-32 does not match what the header says.
    ChecksumMismatch,
    /// The block uses a coder this build cannot decode. Not a defect in the
    /// archive: enable the feature, or use a different reader.
    UnsupportedMethod,
    /// The source reader failed. Nothing is known about the archive from this.
    Io,
    /// The password is missing or wrong.
    Password,
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::io_msg(value, "")
    }
}

impl Error {
    #[inline]
    pub(crate) fn other<S: Into<Cow<'static, str>>>(s: S) -> Self {
        Self::Other(s.into())
    }

    /// A limit check that failed, before whatever it bounds was attempted.
    #[inline]
    pub(crate) fn limit(what: Limit, limit: u64, requested: u64) -> Self {
        Self::LimitExceeded {
            what,
            limit,
            requested,
        }
    }

    /// Which bound this error reports, for any of the ways one is reported.
    ///
    /// [`Error::EndHeaderTooLarge`] and [`Error::MemoryLimited`] predate
    /// [`Error::LimitExceeded`] and are still raised in their own shapes so
    /// that existing consumers keep working; this maps all three onto the one
    /// enum, so a consumer that only wants to say which limit stopped it does
    /// not have to know which of them is older.
    #[must_use]
    pub fn limit_hit(&self) -> Option<Limit> {
        match self {
            Self::LimitExceeded { what, .. } => Some(*what),
            Self::EndHeaderTooLarge { .. } => Some(Limit::EndHeaderBytes),
            Self::MemoryLimited { .. } => Some(Limit::MemoryBytes),
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn unsupported<S: Into<Cow<'static, str>>>(s: S) -> Self {
        Self::Unsupported(s.into())
    }

    #[inline]
    pub(crate) fn io_msg(e: std::io::Error, msg: impl Into<Cow<'static, str>>) -> Self {
        Self::Io(e, msg.into())
    }

    pub(crate) fn bad_password(e: std::io::Error, encryped: bool) -> Self {
        if encryped {
            Self::MaybeBadPassword(e)
        } else {
            Self::io_msg(e, "")
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[inline]
    pub(crate) fn file_open(e: std::io::Error, filename: impl Into<Cow<'static, str>>) -> Self {
        Self::Io(e, filename.into())
    }

    pub(crate) fn maybe_bad_password(self, encryped: bool) -> Self {
        if !encryped {
            return self;
        }
        match self {
            Self::Io(e, s) if s.is_empty() => Self::MaybeBadPassword(e),
            _ => self,
        }
    }
}

impl Error {
    /// Whether this is a CRC-32 that did not match, however it travelled.
    ///
    /// A verifying reader sits inside a `Read` chain, so its verdict comes out
    /// as an [`std::io::Error`] carrying [`Error::ChecksumVerificationFailed`],
    /// and arrives here as [`Error::Io`] once `?` has converted it.
    pub(crate) fn is_checksum_failure(&self) -> bool {
        match self {
            Self::ChecksumVerificationFailed => true,
            Self::Io(e, _) => e
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<Error>())
                .is_some_and(|inner| matches!(inner, Self::ChecksumVerificationFailed)),
            _ => false,
        }
    }

    /// Adds block context to an error raised while decoding that block.
    pub(crate) fn in_block(self, block_index: usize, packed_offset: u64) -> Self {
        // Already located; do not re-wrap an inner block's context away.
        if matches!(self, Self::BlockDecode { .. }) {
            return self;
        }
        // A checksum that did not match is one error whichever verifying
        // reader caught it — the block's, a file's on the consuming thread,
        // or the workers' folded one — so it is reported as the same
        // `ChecksumMismatch`, with the same message, on every path. Raised
        // inside the `Read` chain it arrives wrapped in an `io::Error`, whose
        // kind (`Other`) says nothing about the archive.
        let error = if self.is_checksum_failure() {
            Self::ChecksumVerificationFailed
        } else {
            self
        };
        // A limit is a refusal, not damage: the block is fine, the caller's
        // budget is not. Keep it typed rather than rendering it into a
        // `BlockDecode` message, so `limit_hit` still answers.
        if matches!(error, Self::LimitExceeded { .. }) {
            return error;
        }
        let kind = match &error {
            Self::Io(..) | Self::FileOpen(..) => BlockErrorKind::Io,
            Self::UnsupportedCompressionMethod(..)
            | Self::Unsupported(..)
            | Self::ExternalUnsupported => BlockErrorKind::UnsupportedMethod,
            Self::ChecksumVerificationFailed | Self::NextHeaderCrcMismatch => {
                BlockErrorKind::ChecksumMismatch
            }
            Self::PasswordRequired | Self::MaybeBadPassword(..) => BlockErrorKind::Password,
            _ => BlockErrorKind::Corrupted,
        };
        Self::BlockDecode {
            block_index,
            packed_offset,
            kind,
            message: error.to_string(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self, f)
    }
}

impl std::error::Error for Error {}
