use crate::ByteWriter;
use zeroize::Zeroizing;

/// A password used for password protected, encrypted files.
///
/// Use [`Password::empty()`] to create an empty password when no
/// password is used.
///
/// You can convert strings easily into password using the Into/From traits:
///
/// ```rust
/// use sevenz_turbo::Password;
///
/// let password: Password = "a password string".into();
/// ```
#[derive(Default)]
pub struct Password {
    bytes: Zeroizing<Vec<u8>>,
    /// The keys derived from these bytes, shared by every clone: the bytes of
    /// a password never change after it is made, so a clone is the same
    /// password and the key derived for one of them is the key for all. The
    /// encoder clones its options for every folder and the header; without
    /// this each clone paid the whole derivation again.
    #[cfg(feature = "aes256")]
    pub(crate) key_cache: std::sync::Arc<std::sync::Mutex<super::aes::KeyCache>>,
    /// SHA-256 rounds this copy has spent deriving keys, against
    /// `ArchiveLimits::max_aes_kdf_rounds`. Per copy, not shared: a clone
    /// starts a fresh budget, as it always has. Only read and written with
    /// `key_cache` locked.
    #[cfg(feature = "aes256")]
    pub(crate) kdf_rounds: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password([REDACTED])")
    }
}

impl Clone for Password {
    fn clone(&self) -> Self {
        // Each copy owns its clearing buffer and its budget; the derived keys
        // travel with it.
        Self {
            bytes: Zeroizing::new(self.as_slice().to_vec()),
            #[cfg(feature = "aes256")]
            key_cache: std::sync::Arc::clone(&self.key_cache),
            #[cfg(feature = "aes256")]
            kdf_rounds: Default::default(),
        }
    }
}

impl PartialEq for Password {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Password {
    /// Creates a new [`Password`] from the given password string.
    ///
    /// Internally a password string is encoded as UTF-16.
    pub fn new(password: &str) -> Self {
        Self::from(password)
    }

    /// Creates a new [`Password`] from the given raw bytes.
    pub fn from_raw(bytes: &[u8]) -> Self {
        Self {
            bytes: Zeroizing::new(bytes.to_vec()),
            #[cfg(feature = "aes256")]
            key_cache: Default::default(),
            #[cfg(feature = "aes256")]
            kdf_rounds: Default::default(),
        }
    }

    /// Creates an empty password.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns the byte representation of the password.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns `true` if the password is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl AsRef<[u8]> for Password {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl From<&str> for Password {
    fn from(s: &str) -> Self {
        let mut result = Zeroizing::new(Vec::with_capacity(s.len() * 2));
        let utf16 = s.encode_utf16();
        for u in utf16 {
            let _ = result.write_u16(u);
        }
        Self {
            bytes: result,
            #[cfg(feature = "aes256")]
            key_cache: Default::default(),
            #[cfg(feature = "aes256")]
            kdf_rounds: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_exposes_password_bytes() {
        for password in [
            Password::new("secret"),
            Password::empty(),
            Password::from_raw(&[0, 255]),
        ] {
            assert_eq!(format!("{password:?}"), "Password([REDACTED])");
            assert_eq!(format!("{password:#?}"), "Password([REDACTED])");
            assert_eq!(password, password.clone());
        }
    }

    #[test]
    fn clones_own_independent_clearing_buffers() {
        let password = Password::new("test");
        let copy = password.clone();
        assert_ne!(password.as_slice().as_ptr(), copy.as_slice().as_ptr());
        drop(password);
        assert_eq!(copy.as_slice(), &[116, 0, 101, 0, 115, 0, 116, 0]);
    }
}
