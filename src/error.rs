use std::{borrow::Cow, fmt::Display};

/// The error type of the crate.
#[derive(Debug)]
#[non_exhaustive]
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
    /// An error the caller's own code raised, such as a callback that gives
    /// up, passed through as it is. The crate never raises it itself.
    Other(Cow<'static, str>),
    /// The archive is malformed: a header field out of range or inconsistent
    /// with another, a stream map that does not add up, a coder's properties
    /// too short, or a stream that does not end where it should. The string
    /// says which; it is for people, not for matching.
    Corrupted(Cow<'static, str>),
    /// The caller asked for something this call cannot do with what it was
    /// given: an index past the end, an entry with no data where data is
    /// needed, arguments that do not agree. The archive is not at fault.
    InvalidArgument(Cow<'static, str>),
    /// The work stopped before it finished: the caller's cancel flag was
    /// set, or whoever was to receive its output had gone.
    Cancelled,
    /// A worker thread panicked or stopped early, or an invariant of this
    /// crate's own failed. Neither the archive nor the caller is at fault.
    Internal(Cow<'static, str>),
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
    /// An entry's stored name would escape the extraction directory or names
    /// no file there.
    ///
    /// The reader raises it only when [`ArchiveLimits::reject_unsafe_paths`]
    /// is set; a consumer that lists rather than extracts leaves that off and
    /// asks [`ArchiveEntry::is_unsafe_path`] per entry instead. The
    /// extraction helpers refuse such an entry with it whatever the setting.
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
    ///
    /// The error it locates is `cause`, which [`source`] returns and the
    /// [`Display`] rendering leaves out. [`Error::in_block`] builds one and
    /// derives `kind` from the cause.
    ///
    /// [`source`]: std::error::Error::source
    #[non_exhaustive]
    BlockDecode {
        /// Index of the block (7-Zip calls it a folder) in [`Archive::blocks`].
        ///
        /// [`Archive::blocks`]: crate::Archive::blocks
        block_index: usize,
        /// Absolute offset of the block's first packed stream in the archive.
        packed_offset: u64,
        /// What went wrong.
        kind: BlockErrorKind,
        /// The error this one locates. [`Error::io_error`],
        /// [`Error::is_password_required`] and [`Error::has_checksum_mismatch`]
        /// answer through it.
        cause: Box<Error>,
    },
    /// An earlier push on this `ArchiveWriter` failed after it had put bytes
    /// in the output that the archive's header does not account for, so no
    /// header it could still write would describe the file. Every later push
    /// and `finish` returns this instead of writing; the string is the
    /// earlier failure, rendered.
    WriterPoisoned(String),
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
    pub(crate) fn corrupted<S: Into<Cow<'static, str>>>(s: S) -> Self {
        Self::Corrupted(s.into())
    }

    #[inline]
    pub(crate) fn invalid_argument<S: Into<Cow<'static, str>>>(s: S) -> Self {
        Self::InvalidArgument(s.into())
    }

    #[inline]
    pub(crate) fn internal<S: Into<Cow<'static, str>>>(s: S) -> Self {
        Self::Internal(s.into())
    }

    /// This error, to travel up a `Read` or `Write` chain; [`Error::in_block`]
    /// takes it back out. Its I/O kind is the nearest std one, except that a
    /// cancellation is `Other`: std's read and copy helpers retry an
    /// `Interrupted` read, so a cancelled chain would spin instead of stop.
    pub(crate) fn into_io(self) -> std::io::Error {
        let kind = match self {
            Self::Corrupted(_) | Self::ChecksumVerificationFailed => {
                std::io::ErrorKind::InvalidData
            }
            Self::InvalidArgument(_) => std::io::ErrorKind::InvalidInput,
            _ => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, self)
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

    /// Locates an error raised while decoding a block: the
    /// [`Error::BlockDecode`] whose cause it is, with the
    /// [`kind`](BlockErrorKind) its cause implies.
    ///
    /// One of this crate's errors raised inside a `Read` chain, which arrives
    /// as an [`Error::Io`] carrying it, is taken back out first, so a checksum,
    /// a cancellation or a failed worker is reported the same way on every
    /// path. A result that is not damage in the block is returned as it is:
    /// a block error already, [`Error::LimitExceeded`] (the caller's budget),
    /// [`Error::Cancelled`], [`Error::InvalidArgument`] and
    /// [`Error::Internal`].
    #[must_use]
    pub fn in_block(self, block_index: usize, packed_offset: u64) -> Self {
        let error = self.out_of_io();
        if matches!(
            error,
            Self::BlockDecode { .. }
                | Self::LimitExceeded { .. }
                | Self::Cancelled
                | Self::InvalidArgument(_)
                | Self::Internal(_)
        ) {
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
            cause: Box::new(error),
        }
    }

    /// The crate's own error an [`Error::Io`] carries, if it carries one, or
    /// the class of a coder's error that is not damage in its stream.
    ///
    /// A cancellation or failed invariant comes out of
    /// [`Error::MaybeBadPassword`] too: no password causes either.
    fn out_of_io(self) -> Self {
        match self {
            Self::Io(io, context) => match io.downcast::<Error>() {
                Ok(error) => error.out_of_io(),
                Err(io) => Self::from_coder(&io).unwrap_or(Self::Io(io, context)),
            },
            Self::MaybeBadPassword(io) => {
                let own_fault = io
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<Error>())
                    .is_some_and(|error| matches!(error, Self::Cancelled | Self::Internal(_)));
                if own_fault {
                    return io
                        .downcast::<Error>()
                        .unwrap_or_else(Self::MaybeBadPassword);
                }
                Self::from_coder(&io).unwrap_or(Self::MaybeBadPassword(io))
            }
            error => error,
        }
    }

    /// A cancellation or failed invariant that a coder raised inside a `Read`
    /// chain, as this crate reports it. Damage the coder found stays the I/O
    /// error it is, of the kind the coder gave it.
    fn from_coder(io: &std::io::Error) -> Option<Self> {
        if let Some(error) = crate::codec::lzma_turbo::io_fault(io) {
            return Some(error);
        }
        #[cfg(feature = "ppmd")]
        if let Some(error) = io.get_ref()?.downcast_ref::<ppmd_turbo::Error>() {
            return matches!(error.kind(), ppmd_turbo::ErrorKind::Internal(_))
                .then(|| Self::internal(error.to_string()));
        }
        None
    }

    /// The I/O error this one reports, if an I/O error is what it reports:
    /// the source's own, one a coder raised inside the `Read` chain, or the
    /// one behind a block error or a possibly wrong password.
    ///
    /// Its [`kind`](std::io::Error::kind), OS code and payload are the
    /// reader's, untouched.
    #[must_use]
    pub fn io_error(&self) -> Option<&std::io::Error> {
        match self {
            Self::Io(error, _) | Self::FileOpen(error, _) | Self::MaybeBadPassword(error) => {
                Some(error)
            }
            Self::BlockDecode { cause, .. } => cause.io_error(),
            _ => None,
        }
    }

    /// Whether the archive is encrypted and no password was given, here or as
    /// the cause of a block error.
    ///
    /// A password that was given and may be wrong is
    /// [`Error::MaybeBadPassword`], which is not this: its bytes failed to
    /// decode or verify, and damage fails the same way.
    #[must_use]
    pub fn is_password_required(&self) -> bool {
        match self {
            Self::PasswordRequired => true,
            Self::BlockDecode { cause, .. } => cause.is_password_required(),
            _ => false,
        }
    }

    /// Whether a checksum the archive carries disagreed with its bytes,
    /// wherever that verdict travelled: raised here, carried inside an I/O
    /// error from a `Read` chain, behind a possibly wrong password, or as the
    /// cause of a block error. A CRC-32 of a block or file, the next header's
    /// CRC, and an LZ4 frame's own header, block or content checksum all
    /// count.
    #[must_use]
    pub fn has_checksum_mismatch(&self) -> bool {
        match self {
            Self::ChecksumVerificationFailed | Self::NextHeaderCrcMismatch => true,
            Self::BlockDecode {
                kind: BlockErrorKind::ChecksumMismatch,
                ..
            } => true,
            Self::BlockDecode { cause, .. } => cause.has_checksum_mismatch(),
            Self::Io(error, _) | Self::FileOpen(error, _) | Self::MaybeBadPassword(error) => {
                io_error_has_checksum_mismatch(error)
            }
            _ => false,
        }
    }
}

fn io_error_has_checksum_mismatch(error: &std::io::Error) -> bool {
    let Some(inner) = error.get_ref() else {
        return false;
    };
    if let Some(inner) = inner.downcast_ref::<Error>() {
        return inner.has_checksum_mismatch();
    }
    #[cfg(feature = "lz4")]
    if let Some(inner) = inner.downcast_ref::<lz4_flex::frame::Error>() {
        return matches!(
            inner,
            lz4_flex::frame::Error::HeaderChecksumError
                | lz4_flex::frame::Error::BlockChecksumError
                | lz4_flex::frame::Error::ContentChecksumError
        );
    }
    false
}

/// A copy of an I/O error, for a failure that is reported more than once: the
/// same kind, with its OS code, or with a copy of its payload when it is one
/// of this crate's errors, a coder's, a refused reservation, or a checksum
/// failure. Any other payload is copied as its message.
pub(crate) fn copy_io(error: &std::io::Error) -> std::io::Error {
    if let Some(code) = error.raw_os_error() {
        return std::io::Error::from_raw_os_error(code);
    }
    let kind = error.kind();
    let Some(inner) = error.get_ref() else {
        let bare = std::io::Error::from(kind);
        let message = error.to_string();
        return if message == bare.to_string() {
            bare
        } else {
            std::io::Error::new(kind, message)
        };
    };
    if let Some(inner) = inner.downcast_ref::<Error>() {
        return std::io::Error::new(kind, inner.copy());
    }
    if let Some(copy) = crate::codec::lzma_turbo::copy_io(kind, inner) {
        return copy;
    }
    #[cfg(feature = "ppmd")]
    if let Some(inner) = inner.downcast_ref::<ppmd_turbo::Error>() {
        return std::io::Error::new(kind, *inner);
    }
    #[cfg(feature = "lz4")]
    if let Some(copy) = crate::codec::lz4::copy_io(kind, inner) {
        return copy;
    }
    if let Some(inner) = inner.downcast_ref::<std::collections::TryReserveError>() {
        return std::io::Error::new(kind, inner.clone());
    }
    if io_error_has_checksum_mismatch(error) {
        return std::io::Error::new(kind, Error::ChecksumVerificationFailed);
    }
    std::io::Error::new(kind, inner.to_string())
}

impl Error {
    /// A copy of this error, with any I/O error in it copied by [`copy_io`].
    pub(crate) fn copy(&self) -> Self {
        match self {
            Self::BadSignature(signature) => Self::BadSignature(*signature),
            Self::UnsupportedVersion { major, minor } => Self::UnsupportedVersion {
                major: *major,
                minor: *minor,
            },
            Self::ChecksumVerificationFailed => Self::ChecksumVerificationFailed,
            Self::NextHeaderCrcMismatch => Self::NextHeaderCrcMismatch,
            Self::Io(error, context) => Self::Io(copy_io(error), context.clone()),
            Self::FileOpen(error, name) => Self::FileOpen(copy_io(error), name.clone()),
            Self::Other(message) => Self::Other(message.clone()),
            Self::Corrupted(detail) => Self::Corrupted(detail.clone()),
            Self::InvalidArgument(detail) => Self::InvalidArgument(detail.clone()),
            Self::Cancelled => Self::Cancelled,
            Self::Internal(detail) => Self::Internal(detail.clone()),
            Self::BadTerminatedStreamsInfo(id) => Self::BadTerminatedStreamsInfo(*id),
            Self::BadTerminatedUnpackInfo => Self::BadTerminatedUnpackInfo,
            Self::BadTerminatedPackInfo(id) => Self::BadTerminatedPackInfo(*id),
            Self::BadTerminatedSubStreamsInfo => Self::BadTerminatedSubStreamsInfo,
            Self::BadTerminatedHeader(id) => Self::BadTerminatedHeader(*id),
            Self::ExternalUnsupported => Self::ExternalUnsupported,
            Self::UnsupportedCompressionMethod(method) => {
                Self::UnsupportedCompressionMethod(method.clone())
            }
            Self::MaxMemLimited { max_kb, actaul_kb } => Self::MaxMemLimited {
                max_kb: *max_kb,
                actaul_kb: *actaul_kb,
            },
            Self::PasswordRequired => Self::PasswordRequired,
            Self::Unsupported(detail) => Self::Unsupported(detail.clone()),
            Self::MaybeBadPassword(error) => Self::MaybeBadPassword(copy_io(error)),
            Self::FileNotFound => Self::FileNotFound,
            Self::EndHeaderTooLarge {
                limit_bytes,
                declared_bytes,
            } => Self::EndHeaderTooLarge {
                limit_bytes: *limit_bytes,
                declared_bytes: *declared_bytes,
            },
            Self::MemoryLimited {
                limit_bytes,
                required_bytes,
            } => Self::MemoryLimited {
                limit_bytes: *limit_bytes,
                required_bytes: *required_bytes,
            },
            Self::LimitExceeded {
                what,
                limit,
                requested,
            } => Self::LimitExceeded {
                what: *what,
                limit: *limit,
                requested: *requested,
            },
            Self::UnsafeEntryName { name, reason } => Self::UnsafeEntryName {
                name: name.clone(),
                reason,
            },
            Self::BlockDecode {
                block_index,
                packed_offset,
                kind,
                cause,
            } => Self::BlockDecode {
                block_index: *block_index,
                packed_offset: *packed_offset,
                kind: *kind,
                cause: Box::new(cause.copy()),
            },
            Self::WriterPoisoned(message) => Self::WriterPoisoned(message.clone()),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The cause is the error's source, so it is not rendered here too.
            Self::BlockDecode {
                block_index,
                packed_offset,
                kind,
                cause: _,
            } => f
                .debug_struct("BlockDecode")
                .field("block_index", block_index)
                .field("packed_offset", packed_offset)
                .field("kind", kind)
                .finish(),
            Self::Other(message) => f.write_str(message),
            Self::Corrupted(detail) => write!(f, "corrupted archive: {detail}"),
            Self::InvalidArgument(detail) => write!(f, "invalid argument: {detail}"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Internal(detail) => write!(f, "internal error: {detail}"),
            _ => std::fmt::Debug::fmt(&self, f),
        }
    }
}

impl std::error::Error for Error {
    /// A block error's cause. The variants that hold an [`std::io::Error`]
    /// render it, so they return none, as `io::Error` does for its own
    /// payload; [`Error::io_error`] reaches it instead.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BlockDecode { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure reported twice is the same failure the second time: its
    /// kind, its OS code, and this crate's error inside it.
    #[test]
    fn a_copied_io_error_keeps_its_code_and_payload() {
        use std::io::{Error as IoError, ErrorKind};

        let copy = copy_io(&IoError::from_raw_os_error(5));
        assert_eq!(copy.raw_os_error(), Some(5));

        let copy = copy_io(&IoError::from(ErrorKind::UnexpectedEof));
        assert_eq!(copy.kind(), ErrorKind::UnexpectedEof);
        assert!(copy.get_ref().is_none());

        let original = IoError::new(
            ErrorKind::InvalidData,
            Error::ChecksumVerificationFailed.in_block(1, 32),
        );
        let copy = copy_io(&original);
        assert_eq!(copy.kind(), ErrorKind::InvalidData);
        assert!(Error::from(copy).has_checksum_mismatch());

        let copy = copy_io(&IoError::other("worker said no"));
        assert_eq!(copy.kind(), ErrorKind::Other);
        assert_eq!(copy.to_string(), "worker said no");

        let refused = Vec::<u8>::new().try_reserve_exact(usize::MAX).unwrap_err();
        let copy = copy_io(&IoError::new(ErrorKind::OutOfMemory, refused.clone()));
        assert_eq!(copy.kind(), ErrorKind::OutOfMemory);
        assert_eq!(
            copy.get_ref()
                .and_then(|inner| inner.downcast_ref::<std::collections::TryReserveError>()),
            Some(&refused)
        );
    }

    /// An lz4 frame error is copied as itself, so a failure reported twice
    /// can still be downcast to it.
    #[cfg(feature = "lz4")]
    #[test]
    fn a_copied_io_error_keeps_its_lz4_payload() {
        use lz4_flex::block::DecompressError;
        use lz4_flex::frame::Error as Lz4Error;

        for (original, check) in [
            (
                std::io::Error::from(Lz4Error::UnsupportedVersion(9)),
                (|e: &Lz4Error| matches!(e, Lz4Error::UnsupportedVersion(9)))
                    as fn(&Lz4Error) -> bool,
            ),
            (
                std::io::Error::from(Lz4Error::DecompressionError(
                    DecompressError::OffsetOutOfBounds,
                )),
                |e| {
                    matches!(
                        e,
                        Lz4Error::DecompressionError(DecompressError::OffsetOutOfBounds)
                    )
                },
            ),
            (
                std::io::Error::from(Lz4Error::ContentLengthError {
                    expected: 5,
                    actual: 4,
                }),
                |e| {
                    matches!(
                        e,
                        Lz4Error::ContentLengthError {
                            expected: 5,
                            actual: 4
                        }
                    )
                },
            ),
        ] {
            let copy = copy_io(&original);
            assert_eq!(copy.kind(), original.kind());
            let inner = copy
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<Lz4Error>())
                .unwrap_or_else(|| panic!("{copy} lost its lz4 payload"));
            assert!(check(inner), "{inner:?}");
        }
    }

    /// A block error keeps the error it locates, so its I/O kind, its OS code
    /// and whether it was a password problem are answered from the error and
    /// not from its wording. The cause is its source and is rendered once
    /// along the chain: by the cause, not by the block error as well.
    #[test]
    fn a_block_error_keeps_its_cause() {
        let short =
            Error::from(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)).in_block(2, 32);
        assert!(matches!(
            short,
            Error::BlockDecode {
                kind: BlockErrorKind::Io,
                ..
            }
        ));
        assert_eq!(
            short.io_error().map(std::io::Error::kind),
            Some(std::io::ErrorKind::UnexpectedEof)
        );
        let mut chain = vec![short.to_string()];
        let mut next = std::error::Error::source(&short);
        while let Some(error) = next {
            chain.push(error.to_string());
            next = error.source();
        }
        assert_eq!(
            chain,
            [
                "BlockDecode { block_index: 2, packed_offset: 32, kind: Io }",
                "Io(Kind(UnexpectedEof), \"\")",
            ]
        );

        let os = Error::from(std::io::Error::from_raw_os_error(24)).in_block(0, 32);
        assert_eq!(
            os.io_error().and_then(std::io::Error::raw_os_error),
            Some(24)
        );
        assert!(!os.has_checksum_mismatch());

        let required = Error::PasswordRequired.in_block(0, 32);
        assert!(required.is_password_required());
        assert!(required.io_error().is_none());

        let maybe =
            Error::MaybeBadPassword(std::io::Error::other(Error::ChecksumVerificationFailed))
                .in_block(0, 32);
        assert!(matches!(
            maybe,
            Error::BlockDecode {
                kind: BlockErrorKind::Password,
                ..
            }
        ));
        assert!(!maybe.is_password_required());
        assert!(maybe.has_checksum_mismatch());

        let crc =
            Error::from(std::io::Error::other(Error::ChecksumVerificationFailed)).in_block(0, 32);
        assert!(crc.has_checksum_mismatch());
        assert!(Error::NextHeaderCrcMismatch.has_checksum_mismatch());
        assert!(
            !Error::corrupted("damaged")
                .in_block(0, 32)
                .has_checksum_mismatch()
        );
    }

    #[cfg(feature = "lz4")]
    #[test]
    fn an_lz4_frame_checksum_is_a_checksum_mismatch() {
        for frame in [
            lz4_flex::frame::Error::HeaderChecksumError,
            lz4_flex::frame::Error::BlockChecksumError,
            lz4_flex::frame::Error::ContentChecksumError,
        ] {
            let error = Error::from(std::io::Error::from(frame)).in_block(0, 32);
            assert!(error.has_checksum_mismatch(), "{error}");
        }
        let other = Error::from(std::io::Error::from(
            lz4_flex::frame::Error::UnsupportedVersion(9),
        ))
        .in_block(0, 32);
        assert!(!other.has_checksum_mismatch());
    }
}
