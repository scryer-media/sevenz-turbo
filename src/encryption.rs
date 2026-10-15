#[cfg(feature = "aes256")]
mod aes;
mod password;

#[cfg(feature = "aes256")]
pub(crate) use aes::*;
#[cfg(feature = "aes256")]
pub use aes::{AesKey, AesProperties};
pub use password::*;

// Hard decoder ceiling; raw-key mode (63) performs no derivation.
pub(crate) const MAX_AES_CYCLES_POWER: u8 = 24;
