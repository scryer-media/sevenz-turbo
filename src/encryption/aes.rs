use std::{
    borrow::Cow,
    io::{Read, Seek},
};
use zeroize::{Zeroize, Zeroizing};

#[cfg(feature = "compress")]
use std::io::Write;

use super::MAX_AES_CYCLES_POWER;
use crate::Password;
use crate::crypto_backend::{
    AES_BLOCK_LEN, Aes256Cbc, Aes256CbcLike, AesError, Sha256, Sha256Like,
};
#[cfg(feature = "compress")]
use crate::crypto_backend::{Aes256CbcEnc, Aes256CbcEncLike};
#[cfg(feature = "compress")]
use crate::encoder_options::AesEncoderOptions;

/// The size of the pieces the cipher is handed, both ways, unless a caller's
/// own buffer is larger. A multiple of the block size. Per-call overhead in
/// either backend is a few blocks' worth, so this keeps it off the profile,
/// and it matches the buffer every other coder in the chain reads through.
const AES_CHUNK_LEN: usize = 64 << 10;

fn crypto_error(err: AesError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

/// The 7z `aes256` decoder.
///
/// The packed stream is decrypted **in the caller's own buffer**: a read fills
/// `buf` with ciphertext straight from the layer below and decrypts it there,
/// so a gigabyte of payload is copied zero extra times and the reads are as
/// large as the caller's buffer rather than a fixed small block. CBC needs no
/// more state than that — 16 ciphertext bytes that did not complete a block
/// (`carry`) — for a caller that reads [`AES_CHUNK_LEN`] or more at a time.
///
/// A caller that reads less — PPMd's range decoder behind its buffer, BCJ2's
/// side streams, `read_to_end`'s first probes — is served from `plain`, which
/// is filled [`AES_CHUNK_LEN`] at a time the same way, so the cipher and the
/// layer below are never driven a block per call.
pub(crate) struct Aes256Sha256Decoder<R> {
    cipher: Aes256Cbc,
    input: R,
    done: bool,
    /// Ciphertext that did not fill a block, waiting for the next read.
    carry: [u8; AES_BLOCK_LEN],
    carry_len: usize,
    /// Plaintext decrypted ahead for a caller whose buffer is smaller than
    /// [`AES_CHUNK_LEN`]. Empty until such a caller turns up.
    plain: Box<[u8]>,
    plain_start: usize,
    plain_end: usize,
    pos: usize,
}

impl<R> Drop for Aes256Sha256Decoder<R> {
    fn drop(&mut self) {
        self.plain.as_mut().zeroize();
    }
}

impl<R: Read> Aes256Sha256Decoder<R> {
    pub(crate) fn new(
        input: R,
        properties: &[u8],
        password: &Password,
        max_cycles_power: u8,
        max_kdf_rounds: u64,
    ) -> Result<Self, crate::Error> {
        let (aes_key, iv) = get_aes_key(properties, password, max_cycles_power, max_kdf_rounds)?;
        let cipher = Aes256Cbc::new(aes_key.as_ref(), &iv)
            .map_err(|err| crate::Error::other(err.to_string()))?;
        Ok(Self {
            input,
            cipher,
            done: false,
            carry: [0; AES_BLOCK_LEN],
            carry_len: 0,
            plain: Box::default(),
            plain_start: 0,
            plain_end: 0,
            pos: 0,
        })
    }

    /// The stream ended. A partial block left over is a damaged or truncated
    /// archive: 7z ciphertext is always a whole number of blocks.
    fn finish(&mut self) -> std::io::Result<usize> {
        self.done = true;
        if self.carry_len == 0 {
            Ok(0)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IllegalBlockSize",
            ))
        }
    }

    /// Decrypts up to [`AES_CHUNK_LEN`] into `plain`, for a caller reading
    /// less than that at a time. Only such callers pay for this copy.
    fn fill_plain(&mut self) -> std::io::Result<usize> {
        if self.plain.is_empty() {
            self.plain = vec![0; AES_CHUNK_LEN].into_boxed_slice();
        }
        let mut plain = std::mem::take(&mut self.plain);
        let decrypted = self.decrypt_into(&mut plain);
        self.plain = plain;
        let decrypted = decrypted?;
        self.plain_start = 0;
        self.plain_end = decrypted;
        Ok(decrypted)
    }

    /// Fills `buf` with ciphertext from below, as much as one read gives past
    /// the first block, and decrypts the whole blocks of it in place. Zero is
    /// the end of the stream. `buf` is at least a block long.
    fn decrypt_into(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let capacity = buf.len() - buf.len() % AES_BLOCK_LEN;
        buf[..self.carry_len].copy_from_slice(&self.carry[..self.carry_len]);
        let mut filled = self.carry_len;
        self.carry_len = 0;
        while filled < AES_BLOCK_LEN {
            let read = self.input.read(&mut buf[filled..capacity])?;
            if read == 0 {
                self.carry[..filled].copy_from_slice(&buf[..filled]);
                self.carry_len = filled;
                return self.finish();
            }
            filled += read;
        }

        let whole = filled - filled % AES_BLOCK_LEN;
        self.carry_len = filled - whole;
        self.carry[..self.carry_len].copy_from_slice(&buf[whole..filled]);
        self.cipher
            .decrypt(&mut buf[..whole])
            .map_err(crypto_error)?;
        Ok(whole)
    }

    /// Hands over whatever of `plain` is still undelivered.
    fn drain_plain(&mut self, buf: &mut [u8]) -> usize {
        let size = (self.plain_end - self.plain_start).min(buf.len());
        buf[..size].copy_from_slice(&self.plain[self.plain_start..self.plain_start + size]);
        self.plain_start += size;
        self.pos += size;
        size
    }
}

impl<R: Read> Read for Aes256Sha256Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.plain_start < self.plain_end {
            return Ok(self.drain_plain(buf));
        }
        if self.done {
            return Ok(0);
        }
        if buf.len() < AES_CHUNK_LEN {
            if self.fill_plain()? == 0 {
                return Ok(0);
            }
            return Ok(self.drain_plain(buf));
        }

        // The bulk path: everything happens inside `buf`.
        let whole = self.decrypt_into(buf)?;
        self.pos += whole;
        Ok(whole)
    }
}

impl<R: Read + Seek> Seek for Aes256Sha256Decoder<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        // Only a forward skip inside what is already decrypted is supported,
        // which is what this was ever asked for.
        let len = self.plain_end - self.plain_start;
        match pos {
            std::io::SeekFrom::Start(p) => {
                let n = (p as i64 - self.pos as i64).min(len as i64);

                if n < 0 {
                    Ok(0)
                } else {
                    self.plain_start += n as usize;
                    Ok(p)
                }
            }
            std::io::SeekFrom::End(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Aes256 decoder unsupport seek from end",
            )),
            std::io::SeekFrom::Current(n) => {
                let n = n.min(len as i64);
                if n < 0 {
                    Ok(0)
                } else {
                    self.plain_start += n as usize;
                    Ok(self.pos as u64 + n as u64)
                }
            }
        }
    }
}

fn get_aes_key(
    properties: &[u8],
    password: &Password,
    max_cycles_power: u8,
    max_kdf_rounds: u64,
) -> Result<(Zeroizing<[u8; 32]>, [u8; 16]), crate::Error> {
    let properties = match properties.len() {
        0 => {
            return Err(crate::Error::other("AES256 properties too short"));
        }
        1 => {
            // It seems that there are encrypted files that include the K_END (0x00) symbol as a
            // property byte.
            let mut prop = vec![0u8; 2];
            prop[0] = properties[0];
            Cow::Owned(prop)
        }
        _ => Cow::Borrowed(properties),
    };

    let b0 = properties[0];
    let num_cycles_power = b0 & 63;
    let b1 = properties[1];
    let iv_size = (((b0 >> 6) & 1) + (b1 & 15)) as usize;
    let salt_size = (((b0 >> 7) & 1) + (b1 >> 4)) as usize;
    if 2 + salt_size + iv_size > properties.len() {
        return Err(crate::Error::other("Salt size + IV size too long"));
    }
    let mut salt = vec![0u8; salt_size];
    salt.copy_from_slice(&properties[2..(2 + salt_size)]);
    let mut iv = [0u8; 16];
    iv[0..iv_size].copy_from_slice(&properties[(2 + salt_size)..(2 + salt_size + iv_size)]);
    if password.is_empty() {
        return Err(crate::Error::PasswordRequired);
    }
    let aes_key = if num_cycles_power == 0x3F {
        // "Raw key" mode: the 32-byte key is `salt` followed by the password (both
        // truncated to fit). `salt_size` is at most 16, so copy only that prefix.
        // `aes_key.copy_from_slice(&salt)` would panic on the 32-vs-<=16 length mismatch.
        let mut aes_key = Zeroizing::new([0u8; 32]);
        aes_key[..salt_size].copy_from_slice(&salt[..salt_size]);
        let n = password.as_slice().len().min(aes_key.len() - salt_size);
        aes_key[salt_size..n + salt_size].copy_from_slice(&password.as_slice()[0..n]);
        aes_key
    } else {
        // Cap the work factor: `derive_key` runs `2^num_cycles_power` SHA-256 rounds, so
        // a crafted large power is a CPU-exhaustion DoS (and `1 << power` also overflows
        // the shift for power >= 32). The bound is the caller's
        // `ArchiveLimits::max_aes_cycles_power`, never above what keeps the shift
        // itself safe. No real archive uses a power above the default of 24.
        let cap = max_cycles_power.min(MAX_AES_CYCLES_POWER);
        if num_cycles_power > cap {
            return Err(crate::Error::limit(
                crate::Limit::AesCyclesPower,
                u64::from(cap),
                u64::from(num_cycles_power),
            ));
        }
        derive_key_with_budget(num_cycles_power, &salt, password, max_kdf_rounds)?
    };
    Ok((aes_key, iv))
}

fn derive_key(num_cycles_power: u8, salt: &[u8], password: &[u8]) -> [u8; 32] {
    derive_key_with::<Sha256>(num_cycles_power, salt, password)
}

/// `7zAes.c`'s derivation, over whichever SHA-256 the caller names. Generic so
/// that a build with both cryptography backends can check they agree; the
/// crate itself only ever instantiates it at [`Sha256`].
pub(crate) fn derive_key_with<S: Sha256Like>(
    num_cycles_power: u8,
    salt: &[u8],
    password: &[u8],
) -> [u8; 32] {
    let mut sha = S::new();
    let mut extra = [0u8; 8];
    for _ in 0..(1u64 << num_cycles_power) {
        sha.update(salt);
        sha.update(password);
        sha.update(&extra);
        for item in &mut extra {
            *item = item.wrapping_add(1);
            if *item != 0 {
                break;
            }
        }
    }
    sha.finalize()
}

/// A single cached key owned by one immutable Password, never by the process.
pub(crate) struct CachedKey {
    power: u8,
    salt: Vec<u8>,
    key: Zeroizing<[u8; 32]>,
}

#[derive(Default)]
pub(crate) struct KeyCache {
    pub(crate) key: Option<CachedKey>,
    rounds: u64,
}

#[cfg(test)]
fn derive_key_cached(power: u8, salt: &[u8], password: &Password) -> Zeroizing<[u8; 32]> {
    derive_key_with_budget(power, salt, password, u64::MAX).unwrap()
}

fn derive_key_with_budget(
    num_cycles_power: u8,
    salt: &[u8],
    password: &Password,
    max_rounds: u64,
) -> Result<Zeroizing<[u8; 32]>, crate::Error> {
    let mut cache = password.key_cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = cache.key.as_ref()
        && cached.power == num_cycles_power
        && cached.salt == salt
    {
        return Ok(cached.key.clone());
    }
    let rounds = cache
        .rounds
        .checked_add(1u64 << num_cycles_power)
        .ok_or_else(|| crate::Error::other("AES KDF work total overflow"))?;
    if rounds > max_rounds {
        return Err(crate::Error::limit(
            crate::Limit::AesKdfRounds,
            max_rounds,
            rounds,
        ));
    }
    // Charge before hashing. The same password spans the header and payload,
    // and retains the budget even when its one-entry cache is replaced.
    cache.rounds = rounds;
    let key = Zeroizing::new(derive_key(num_cycles_power, salt, password.as_slice()));
    cache.key = Some(CachedKey {
        power: num_cycles_power,
        salt: salt.to_vec(),
        key: key.clone(),
    });
    Ok(key)
}

#[cfg(feature = "compress")]
/// The 7z `aes256` encoder.
///
/// Plaintext is gathered into `buffer` and encrypted and written
/// [`AES_CHUNK_LEN`] at a time, so the cipher is handed large buffers and the
/// writer below sees one write per chunk rather than one per block. The
/// final partial block is zero-padded when the stream is finished.
pub(crate) struct Aes256Sha256Encoder<W> {
    output: W,
    enc: Aes256CbcEnc,
    buffer: Vec<u8>,
    finished: bool,
}

#[cfg(feature = "compress")]
impl<W> Drop for Aes256Sha256Encoder<W> {
    fn drop(&mut self) {
        self.buffer.zeroize();
    }
}

#[cfg(feature = "compress")]
impl<W> Aes256Sha256Encoder<W> {
    pub(crate) fn new(output: W, options: &AesEncoderOptions) -> Result<Self, crate::Error> {
        let (key, iv) = crate::encryption::aes::get_aes_key(
            &options.properties(),
            &options.password,
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )?;

        Ok(Self {
            output,
            enc: Aes256CbcEnc::new(key.as_ref(), &iv)
                .map_err(|e| crate::Error::other(e.to_string()))?,
            buffer: Vec::new(),
            finished: false,
        })
    }

    /// Encrypts and writes the whole blocks at the front of `buffer`, keeping
    /// the partial block after them for the next write.
    fn write_whole_blocks(&mut self) -> std::io::Result<()>
    where
        W: Write,
    {
        let whole = self.buffer.len() - self.buffer.len() % AES_BLOCK_LEN;
        if whole == 0 {
            return Ok(());
        }
        self.enc
            .encrypt(&mut self.buffer[..whole])
            .map_err(crypto_error)?;
        self.output.write_all(&self.buffer[..whole])?;
        self.buffer.copy_within(whole.., 0);
        self.buffer.truncate(self.buffer.len() - whole);
        Ok(())
    }
}

#[cfg(feature = "compress")]
impl<W: Write> Write for Aes256Sha256Encoder<W> {
    fn write(&mut self, mut buf: &[u8]) -> std::io::Result<usize> {
        if self.finished && !buf.is_empty() {
            return Ok(0);
        }
        if buf.is_empty() {
            self.finished = true;
            self.flush()?;
            return self.output.write(buf);
        }
        let len = buf.len();
        if self.buffer.capacity() < AES_CHUNK_LEN {
            self.buffer.reserve_exact(AES_CHUNK_LEN - self.buffer.len());
        }
        while !buf.is_empty() {
            let take = (AES_CHUNK_LEN - self.buffer.len()).min(buf.len());
            self.buffer.extend_from_slice(&buf[..take]);
            buf = &buf[take..];
            if self.buffer.len() == AES_CHUNK_LEN {
                self.write_whole_blocks()?;
            }
        }
        Ok(len)
    }

    /// Writes every whole block gathered so far, and once the stream is
    /// finished the zero-padded last one too.
    fn flush(&mut self) -> std::io::Result<()> {
        if self.finished && !self.buffer.len().is_multiple_of(AES_BLOCK_LEN) {
            let padded = self.buffer.len().next_multiple_of(AES_BLOCK_LEN);
            self.buffer.resize(padded, 0);
        }
        self.write_whole_blocks()
    }
}

#[cfg(test)]
mod key_derivation_tests {
    use super::*;

    const CYCLES: u8 = 4;

    #[test]
    fn budget_charges_misses_including_evictions_but_not_hits() {
        let password = Password::new("test");
        for _ in 0..513 {
            derive_key_with_budget(2, b"same", &password, 4).unwrap();
        }
        assert_eq!(password.key_cache.lock().unwrap().rounds, 4);
        assert_eq!(
            derive_key_with_budget(2, b"other", &password, 4)
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesKdfRounds)
        );
        derive_key_with_budget(2, b"other", &password, 8).unwrap();
        assert_eq!(
            derive_key_with_budget(2, b"same", &password, 8)
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesKdfRounds)
        );
        password.key_cache.lock().unwrap().rounds = u64::MAX;
        assert!(derive_key_with_budget(0, b"new", &password, u64::MAX).is_err());
    }

    #[test]
    fn cached_derivation_matches_reference() {
        let password = Password::new("pass");
        let expected = derive_key(CYCLES, b"salt", password.as_slice());
        assert_eq!(*derive_key_cached(CYCLES, b"salt", &password), expected);
        assert_eq!(*derive_key_cached(CYCLES, b"salt", &password), expected);
        assert!(password.key_cache.lock().unwrap().key.is_some());
        assert!(password.clone().key_cache.lock().unwrap().key.is_none());
    }

    #[test]
    fn cache_never_crosses_inputs() {
        let a = Password::new("pw-a");
        let b = Password::new("pw-b");
        for (salt, password) in [
            (b"salt-a", &a),
            (b"salt-a", &b),
            (b"salt-c", &a),
            (b"salt-a", &a),
        ] {
            assert_eq!(
                *derive_key_cached(CYCLES, salt, password),
                derive_key(CYCLES, salt, password.as_slice())
            );
        }
    }

    /// Builds an AES coder property blob: `num_cycles_power` in the low six
    /// bits of the first byte, the sizes split across the top two bits of the
    /// first byte and the two nibbles of the second, then salt and IV.
    fn properties(num_cycles_power: u8, salt: &[u8], iv: &[u8]) -> Vec<u8> {
        assert!(salt.len() <= 16 && iv.len() <= 16);
        // Each size is a top bit plus a nibble, so 16 is `1 + 15`.
        let iv_high = u8::from(iv.len() == 16);
        let salt_high = u8::from(salt.len() == 16);
        let b0 = (num_cycles_power & 63) | (iv_high << 6) | (salt_high << 7);
        let b1 = ((salt.len() as u8 - salt_high) << 4) | (iv.len() as u8 - iv_high);
        let mut out = vec![b0, b1];
        out.extend_from_slice(salt);
        out.extend_from_slice(iv);
        out
    }

    /// `7zAes.c` treats `0x3F` as "the key is the salt followed by the
    /// password", with no hashing at all — so it is backend-independent by
    /// construction, which is why the backend differential test skips it.
    #[test]
    fn raw_key_mode_concatenates_salt_and_password() {
        let salt = b"0123456789abcdef";
        let password = b"p\0a\0s\0s\0";
        let (key, iv) = get_aes_key(
            &properties(0x3F, salt, &[0u8; 16]),
            &Password::from_raw(password),
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )
        .expect("key");

        let mut expected = [0u8; 32];
        expected[..16].copy_from_slice(salt);
        expected[16..16 + password.len()].copy_from_slice(password);
        assert_eq!(*key, expected);
        assert_eq!(iv, [0u8; 16]);
    }

    /// A cycle count above the cap is a CPU-exhaustion attempt, not an
    /// archive: `2^power` SHA-256 rounds, and above 31 the shift itself is
    /// undefined. It has to be refused before any hashing starts.
    #[test]
    fn absurd_cycle_counts_are_refused() {
        let salt = b"salt";
        for power in [MAX_AES_CYCLES_POWER + 1, 40, 62] {
            assert!(
                get_aes_key(
                    &properties(power, salt, &[0u8; 16]),
                    &Password::from_raw(b"pw"),
                    MAX_AES_CYCLES_POWER,
                    u64::MAX
                )
                .is_err(),
                "cycle count {power} was accepted"
            );
        }
        // The cap itself is 2^24 SHA-256 rounds, far too slow for a test; that
        // an ordinary count is accepted is covered by the round-trip tests.
        assert!(
            get_aes_key(
                &properties(4, salt, &[0u8; 16]),
                &Password::from_raw(b"pw"),
                MAX_AES_CYCLES_POWER,
                u64::MAX
            )
            .is_ok()
        );
    }

    #[test]
    fn cycle_count_is_part_of_the_identity() {
        let (salt, password) = (b"salt".as_slice(), Password::from_raw(b"pw"));
        let k4 = derive_key_cached(4, salt, &password);
        let k5 = derive_key_cached(5, salt, &password);
        assert_ne!(k4, k5);
        assert_eq!(derive_key_cached(4, salt, &password), k4);
    }
}

#[cfg(all(test, feature = "compress"))]
mod tests {
    use std::io::{Cursor, Read};

    use super::*;

    struct FragmentedReader<R> {
        inner: R,
        read_sizes: &'static [usize],
        next_read: usize,
    }

    impl<R> FragmentedReader<R> {
        fn new(inner: R, read_sizes: &'static [usize]) -> Self {
            Self {
                inner,
                read_sizes,
                next_read: 0,
            }
        }
    }

    impl<R: Read> Read for FragmentedReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read_size = self.read_sizes[self.next_read % self.read_sizes.len()];
            self.next_read += 1;
            let len = read_size.min(buf.len());
            self.inner.read(&mut buf[..len])
        }
    }

    fn encode(original: &[u8]) -> (Vec<u8>, AesEncoderOptions, Password) {
        let mut encoded = vec![];
        let writer = Cursor::new(&mut encoded);
        let password: Password = "1234".into();
        let options = AesEncoderOptions::new(password.clone());
        let mut enc = Aes256Sha256Encoder::new(writer, &options).unwrap();
        enc.write_all(original).expect("encode data");
        let _ = enc.write(&[]).unwrap();
        drop(enc);
        (encoded, options, password)
    }

    fn assert_decodes<R: Read>(input: R, properties: &[u8], password: &Password, original: &[u8]) {
        let mut dec =
            Aes256Sha256Decoder::new(input, properties, password, MAX_AES_CYCLES_POWER, u64::MAX)
                .unwrap();

        let mut decoded = vec![];
        let _ = std::io::copy(&mut dec, &mut decoded).unwrap();
        assert_eq!(&decoded[..original.len()], original);
    }

    #[test]
    fn test_aes_codec() {
        let original = include_bytes!("aes.rs");
        let (encoded, options, password) = encode(original);
        let properties = options.properties();
        assert_decodes(
            Cursor::new(encoded.as_slice()),
            &properties,
            &password,
            original,
        );
    }

    /// More than a few chunks of bytes, none of them repeating.
    fn long_input() -> Vec<u8> {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        (0..5 * AES_CHUNK_LEN + 1234)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    /// The caller's buffer is where decryption happens once it is a chunk or
    /// more, so its size is a code path of its own: under a chunk it goes
    /// through the staging buffer, and a size that is not a multiple of 16 has
    /// to leave the odd tail for the next call rather than lose it.
    #[test]
    fn a_caller_reading_in_odd_sizes_gets_the_same_bytes() {
        let original = long_input();
        let (encoded, options, password) = encode(&original);
        let properties = options.properties();

        for read_size in [
            1usize,
            3,
            15,
            16,
            17,
            31,
            4096,
            AES_CHUNK_LEN - 1,
            AES_CHUNK_LEN,
            AES_CHUNK_LEN + 17,
            3 * AES_CHUNK_LEN,
        ] {
            let mut dec = Aes256Sha256Decoder::new(
                Cursor::new(encoded.as_slice()),
                &properties,
                &password,
                MAX_AES_CYCLES_POWER,
                u64::MAX,
            )
            .unwrap();

            let mut decoded = Vec::new();
            let mut buf = vec![0u8; read_size];
            loop {
                let n = dec.read(&mut buf).expect("decrypt");
                if n == 0 {
                    break;
                }
                decoded.extend_from_slice(&buf[..n]);
            }
            assert_eq!(
                &decoded[..original.len()],
                original.as_slice(),
                "reading {read_size} bytes at a time"
            );
        }
    }

    /// A small read is served from a chunk decrypted ahead, so the layer
    /// below is read a chunk at a time however small the caller's reads are.
    #[test]
    fn a_small_reader_still_reads_the_layer_below_in_chunks() {
        struct Counting<R> {
            inner: R,
            reads: usize,
        }
        impl<R: Read> Read for Counting<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                self.inner.read(buf)
            }
        }
        let original = long_input();
        let (encoded, options, password) = encode(&original);
        let mut dec = Aes256Sha256Decoder::new(
            Counting {
                inner: Cursor::new(encoded.as_slice()),
                reads: 0,
            },
            &options.properties(),
            &password,
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )
        .unwrap();
        let mut decoded = Vec::new();
        let mut byte = [0u8; 1];
        while dec.read(&mut byte).expect("decrypt") == 1 {
            decoded.push(byte[0]);
        }
        assert_eq!(&decoded[..original.len()], original.as_slice());
        assert!(
            dec.input.reads <= encoded.len().div_ceil(AES_CHUNK_LEN) + 1,
            "{} reads below for {} bytes",
            dec.input.reads,
            encoded.len()
        );
    }

    /// The ciphertext does not depend on how the plaintext was split into
    /// writes, and only whole chunks reach the writer below until the end.
    #[test]
    fn the_ciphertext_does_not_depend_on_the_write_sizes() {
        struct Writes(Vec<u8>, Vec<usize>);
        impl Write for Writes {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if !buf.is_empty() {
                    self.1.push(buf.len());
                }
                self.0.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let original = long_input();
        let options = AesEncoderOptions::new("1234".into());
        let mut reference = None;
        for piece in [
            original.len(),
            1,
            15,
            17,
            4096,
            AES_CHUNK_LEN,
            AES_CHUNK_LEN + 1,
        ] {
            let mut enc =
                Aes256Sha256Encoder::new(Writes(Vec::new(), Vec::new()), &options).unwrap();
            for part in original.chunks(piece) {
                enc.write_all(part).expect("encode");
            }
            let _ = enc.write(&[]).unwrap();
            let Writes(bytes, sizes) =
                std::mem::replace(&mut enc.output, Writes(Vec::new(), Vec::new()));
            assert_eq!(bytes.len(), original.len().next_multiple_of(AES_BLOCK_LEN));
            let (last, rest) = sizes.split_last().expect("something was written");
            assert!(
                rest.iter().all(|&n| n == AES_CHUNK_LEN),
                "{piece}: {sizes:?}"
            );
            assert!(*last <= AES_CHUNK_LEN);
            match &reference {
                None => reference = Some(bytes),
                Some(first) => assert!(*first == bytes, "writing {piece} bytes at a time"),
            }
        }
        let password: Password = "1234".into();
        assert_decodes(
            Cursor::new(reference.expect("encoded").as_slice()),
            &options.properties(),
            &password,
            &original,
        );
    }

    /// A stream that ends mid-block is a damaged archive, not a short read.
    #[test]
    fn a_truncated_final_block_is_refused() {
        let original = include_bytes!("aes.rs");
        let (encoded, options, password) = encode(original);
        let properties = options.properties();
        let truncated = &encoded[..encoded.len() - 5];

        let mut dec = Aes256Sha256Decoder::new(
            Cursor::new(truncated),
            &properties,
            &password,
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )
        .unwrap();
        let mut sink = Vec::new();
        let err = std::io::copy(&mut dec, &mut sink).expect_err("truncated ciphertext");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_aes_codec_with_fragmented_input() {
        let original = include_bytes!("aes.rs");
        let (encoded, options, password) = encode(original);
        let properties = options.properties();

        for read_sizes in [&[1, 511][..], &[15, 17], &[7, 503, 31, 511]] {
            let input = FragmentedReader::new(Cursor::new(encoded.as_slice()), read_sizes);
            assert_decodes(input, &properties, &password, original);
        }
    }
}
