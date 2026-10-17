use std::io::{Read, Seek};
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
    compact_push: bool,
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
            .map_err(|err| crate::Error::internal(err.to_string()))?;
        Ok(Self {
            input,
            cipher,
            done: false,
            compact_push: false,
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

    /// Decrypts into the owned plaintext buffer. Small reads allocate one
    /// chunk; bulk pushes can grow it to the bounded push size.
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

    /// Finite budgets keep owned delivery at the ordinary AES chunk size.
    /// A larger buffer buys little for this path while raising retained RSS.
    pub(crate) fn keep_push_buffer_small(&mut self) {
        self.compact_push = true;
    }

    fn reserve_plain(&mut self, want: usize) -> std::io::Result<()> {
        debug_assert_eq!(self.plain_start, self.plain_end);
        if self.plain.len() < want {
            // This buffer is fully consumed. Retire it before growing,
            // so small reads followed by pushes never retain two output
            // allocations. An allocation failure consumes no new input.
            self.plain.as_mut().zeroize();
            self.plain = Box::default();
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(want)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::OutOfMemory, error))?;
            buffer.resize(want, 0);
            self.plain = buffer.into_boxed_slice();
        }
        Ok(())
    }

    /// Delivers from the same owned plaintext the small-read path uses.
    /// The buffer grows lazily for bulk pushes, never beyond the fixed bound;
    /// mixed reads and pushes consume one cursor and retain partial blocks.
    pub(crate) fn push(
        &mut self,
        max: usize,
        sink: &mut dyn FnMut(&[u8]),
    ) -> std::io::Result<usize> {
        if max == 0 {
            return Ok(0);
        }
        if self.plain_start == self.plain_end {
            if self.done {
                return Ok(0);
            }
            let limit = if self.compact_push {
                AES_CHUNK_LEN
            } else {
                crate::decoder::PUSH_BUFFER_BYTES
            };
            let want = max.clamp(AES_CHUNK_LEN, limit);
            self.reserve_plain(want)?;
            if self.fill_plain()? == 0 {
                return Ok(0);
            }
        }
        let size = (self.plain_end - self.plain_start).min(max);
        let start = self.plain_start;
        self.plain_start += size;
        self.pos += size;
        sink(&self.plain[start..start + size]);
        Ok(size)
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
            let read = match self.input.read(&mut buf[filled..capacity]) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
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

/// Validated public parameters of a 7z AES-256-CBC coder.
///
/// These are header facts, not key material. The IV is zero-extended as the
/// format requires. Construct through [`Self::parse`] to preserve the bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AesProperties {
    cycles_power: u8,
    salt: [u8; 16],
    salt_len: u8,
    iv: [u8; 16],
}

impl AesProperties {
    /// Parses the coder's properties without deriving a key or allocating.
    pub fn parse(properties: &[u8]) -> Result<Self, crate::Error> {
        let Some(&b0) = properties.first() else {
            return Err(crate::Error::corrupted("AES256 properties too short"));
        };
        // Preserve the decoder's one-byte property compatibility.
        let b1 = properties.get(1).copied().unwrap_or(0);
        let iv_len = usize::from(((b0 >> 6) & 1) + (b1 & 15));
        let salt_len = usize::from(((b0 >> 7) & 1) + (b1 >> 4));
        if salt_len + iv_len != 0 && 2 + salt_len + iv_len > properties.len() {
            return Err(crate::Error::corrupted("Salt size + IV size too long"));
        }
        let mut salt = [0; 16];
        let mut iv = [0; 16];
        if salt_len != 0 {
            salt[..salt_len].copy_from_slice(&properties[2..2 + salt_len]);
        }
        if iv_len != 0 {
            iv[..iv_len].copy_from_slice(&properties[2 + salt_len..2 + salt_len + iv_len]);
        }
        Ok(Self {
            cycles_power: b0 & 63,
            salt,
            salt_len: salt_len as u8,
            iv,
        })
    }

    /// SHA-256 work factor; 63 selects the format's raw-key mode.
    pub fn cycles_power(&self) -> u8 {
        self.cycles_power
    }

    /// Salt bytes, at most sixteen.
    pub fn salt(&self) -> &[u8] {
        &self.salt[..usize::from(self.salt_len)]
    }

    /// The CBC initialization vector, zero-extended to sixteen bytes.
    pub fn iv(&self) -> &[u8; 16] {
        &self.iv
    }

    /// Derives a key through the selected backend and the password's cache.
    ///
    /// Enforces the decoder's hard ceiling, the supplied work-factor limit,
    /// and the password's cumulative KDF budget. Keep the same `Password`
    /// through header parsing and member admission to reuse both accounting
    /// and cached keys. A cache hit spends no additional rounds.
    pub fn derive_key(
        &self,
        password: &Password,
        limits: &crate::ArchiveLimits,
    ) -> Result<AesKey, crate::Error> {
        self.derive(
            password,
            limits.max_aes_cycles_power,
            limits.max_aes_kdf_rounds,
        )
        .map(AesKey)
    }

    fn derive(
        &self,
        password: &Password,
        max_cycles_power: u8,
        max_kdf_rounds: u64,
    ) -> Result<Zeroizing<[u8; 32]>, crate::Error> {
        if password.is_empty() {
            return Err(crate::Error::PasswordRequired);
        }
        if self.cycles_power == 63 {
            let mut key = Zeroizing::new([0; 32]);
            let salt = self.salt();
            key[..salt.len()].copy_from_slice(salt);
            let n = password.as_slice().len().min(32 - salt.len());
            key[salt.len()..salt.len() + n].copy_from_slice(&password.as_slice()[..n]);
            return Ok(key);
        }
        let cap = max_cycles_power.min(MAX_AES_CYCLES_POWER);
        if self.cycles_power > cap {
            return Err(crate::Error::limit(
                crate::Limit::AesCyclesPower,
                u64::from(cap),
                u64::from(self.cycles_power),
            ));
        }
        derive_key_with_budget(self.cycles_power, self.salt(), password, max_kdf_rounds)
    }
}

/// An AES-256 key whose owned bytes are cleared on drop and never debug-printed.
#[derive(Clone)]
pub struct AesKey(Zeroizing<[u8; 32]>);

impl AesKey {
    /// Borrows the key for an AES backend. Copies made by the caller must be
    /// kept secret and cleared by that caller when no longer needed.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for AesKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AesKey([REDACTED])")
    }
}

impl crate::Coder {
    /// The input-stream count declared by this coder.
    ///
    /// Available with `aes256` for consumers validating an encrypted folder's
    /// complete stream graph before mapping ciphertext to output files.
    pub fn num_input_streams(&self) -> u64 {
        self.num_in_streams()
    }

    /// The output-stream count declared by this coder.
    /// Available with `aes256`, like [`Self::num_input_streams`].
    pub fn num_output_streams(&self) -> u64 {
        self.num_out_streams()
    }

    /// Parses this coder's AES facts, or returns `None` for another method.
    pub fn aes_properties(&self) -> Result<Option<AesProperties>, crate::Error> {
        if self.encoder_method_id() != crate::EncoderMethod::ID_AES256_SHA256 {
            return Ok(None);
        }
        AesProperties::parse(self.properties()).map(Some)
    }
}

impl crate::Archive {
    /// Returns the AES coder facts of a folder in coder order.
    ///
    /// This does not establish that the folder is stored: consumers must
    /// validate its full coder graph, stream geometry and member count.
    pub fn block_aes_properties(&self, block: usize) -> Result<Vec<AesProperties>, crate::Error> {
        let block = self
            .blocks
            .get(block)
            .ok_or_else(|| crate::Error::invalid_argument("AES block index out of range"))?;
        block
            .coders
            .iter()
            .filter_map(|coder| coder.aes_properties().transpose())
            .collect()
    }
}

fn get_aes_key(
    properties: &[u8],
    password: &Password,
    max_cycles_power: u8,
    max_kdf_rounds: u64,
) -> Result<(Zeroizing<[u8; 32]>, [u8; 16]), crate::Error> {
    let properties = AesProperties::parse(properties)?;
    Ok((
        properties.derive(password, max_cycles_power, max_kdf_rounds)?,
        properties.iv,
    ))
}

fn derive_key(num_cycles_power: u8, salt: &[u8], password: &[u8]) -> [u8; 32] {
    derive_key_with::<Sha256>(num_cycles_power, salt, password)
}

/// How many rounds of the derivation go to the hash in one call, as a power
/// of two: `7zAes.cpp`'s `kUnrPow`.
const KDF_UNROLL_POWER: u8 = 6;

/// The most bytes of rounds laid out for one call to the hash. A round is as
/// long as the password, which is the caller's and of any length, so the
/// layout is bounded here and not by it. A password of up to about a
/// kilobyte keeps the whole `2^KDF_UNROLL_POWER` rounds a call.
const KDF_BATCH_BYTES: usize = 64 << 10;

/// How many rounds of `round_len` bytes are laid out for one call, as a power
/// of two: [`KDF_UNROLL_POWER`], or as many fewer as keeps the layout within
/// [`KDF_BATCH_BYTES`]. `None` for a round longer than that by itself, which
/// is not laid out at all.
fn kdf_unroll_power(num_cycles_power: u8, round_len: usize) -> Option<u8> {
    if round_len > KDF_BATCH_BYTES {
        return None;
    }
    let mut power = num_cycles_power.min(KDF_UNROLL_POWER);
    while round_len > KDF_BATCH_BYTES >> power {
        power -= 1;
    }
    Some(power)
}

/// `7zAes.c`'s derivation, over whichever SHA-256 the caller names. Generic so
/// that a build with both cryptography backends can check they agree; the
/// crate itself only ever instantiates it at [`Sha256`].
///
/// Each round hashes `salt || password || counter`, the counter an 8-byte
/// little-endian round number. The rounds are laid out
/// `2^KDF_UNROLL_POWER` at a time in one buffer and handed to the hash in one
/// call, then every counter in it is advanced by that many, as 7-Zip does:
/// the hash sees the same bytes, in the same order, as three calls a round,
/// and the per-call cost (a foreign call, with AWS-LC) is paid 192 times less
/// often.
///
/// The buffer is at most [`KDF_BATCH_BYTES`] whatever the password's length:
/// longer rounds are laid out fewer at a time, and a round longer than the
/// bound by itself goes to the hash as its three pieces, with no copy made.
/// The per-call cost that the layout saves is nothing beside a round that
/// long.
pub(crate) fn derive_key_with<S: Sha256Like>(
    num_cycles_power: u8,
    salt: &[u8],
    password: &[u8],
) -> [u8; 32] {
    let mut sha = S::new();
    let round_len = salt.len().saturating_add(password.len()).saturating_add(8);
    let Some(unroll_power) = kdf_unroll_power(num_cycles_power, round_len) else {
        for counter in 0..(1u64 << num_cycles_power) {
            sha.update(salt);
            sha.update(password);
            sha.update(&counter.to_le_bytes());
        }
        return sha.finalize();
    };
    let unroll = 1u64 << unroll_power;
    // It holds the password, so it is cleared when dropped.
    let mut rounds = Zeroizing::new(Vec::with_capacity(round_len << unroll_power));
    for counter in 0..unroll {
        rounds.extend_from_slice(salt);
        rounds.extend_from_slice(password);
        rounds.extend_from_slice(&counter.to_le_bytes());
    }
    for _ in 0..(1u64 << (num_cycles_power - unroll_power)) {
        sha.update(&rounds);
        for round in rounds.chunks_exact_mut(round_len) {
            let counter = &mut round[round_len - 8..];
            let next =
                u64::from_le_bytes(counter.try_into().expect("8 bytes")).wrapping_add(unroll);
            counter.copy_from_slice(&next.to_le_bytes());
        }
    }
    sha.finalize()
}

/// How many derived keys a password keeps. More than one, because the copies
/// of one password share them and may be reading different archives (one key
/// per salt); a handful, because each is a key that outlives its archive.
const KEY_CACHE_ENTRIES: usize = 4;

/// One derived key and what it was derived from. The password is the cache's
/// owner (see [`KeyCache`]), so salt and work factor complete the identity.
pub(crate) struct CachedKey {
    power: u8,
    salt: Vec<u8>,
    key: Zeroizing<[u8; 32]>,
}

/// The keys derived from one password, shared by its clones and by nothing
/// else, never by the process: [`Password`] holds it behind an `Arc` that only
/// `clone` copies, and a clone's bytes are the original's.
///
/// The lock is held across a derivation, so copies of one password asking for
/// the same key at once (an encoder's folders, a reader's blocks) derive it
/// once: the first derives, the rest wait and find it.
#[derive(Default)]
pub(crate) struct KeyCache {
    /// Least recently used first.
    entries: Vec<CachedKey>,
    capacity: usize,
    retained: std::collections::BTreeMap<(u8, Vec<u8>), Zeroizing<[u8; 32]>>,
    /// Derivations actually run, for the tests that hold the count to one.
    #[cfg(test)]
    pub(crate) derivations: u64,
}

impl KeyCache {
    pub(crate) fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        let keep = self.capacity();
        if keep > KEY_CACHE_ENTRIES {
            for entry in self.entries.drain(..) {
                self.retained.insert((entry.power, entry.salt), entry.key);
            }
            while self.retained.len() > keep {
                self.retained.pop_first();
            }
        } else {
            for ((power, salt), key) in std::mem::take(&mut self.retained) {
                self.entries.push(CachedKey { power, salt, key });
            }
            if self.entries.len() > keep {
                self.entries.drain(..self.entries.len() - keep);
            }
        }
    }

    fn capacity(&self) -> usize {
        if self.capacity == 0 {
            KEY_CACHE_ENTRIES
        } else {
            self.capacity
        }
    }
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
    use std::sync::atomic::Ordering::Relaxed;

    let mut cache = password.key_cache.lock().unwrap_or_else(|e| e.into_inner());
    if !cache.retained.is_empty()
        && let Some(key) = cache.retained.get(&(num_cycles_power, salt.to_vec()))
    {
        return Ok(key.clone());
    }
    if let Some(hit) = cache
        .entries
        .iter()
        .position(|cached| cached.power == num_cycles_power && cached.salt == salt)
    {
        let cached = cache.entries.remove(hit);
        let key = cached.key.clone();
        cache.entries.push(cached);
        return Ok(key);
    }
    // `kdf_rounds` is only touched under the lock, so the ordering is the
    // lock's.
    let rounds = password
        .kdf_rounds
        .load(Relaxed)
        .checked_add(1u64 << num_cycles_power)
        .ok_or_else(|| crate::Error::corrupted("AES KDF work total overflow"))?;
    if rounds > max_rounds {
        return Err(crate::Error::limit(
            crate::Limit::AesKdfRounds,
            max_rounds,
            rounds,
        ));
    }
    // Charge before hashing. The same password spans the header and payload,
    // and retains the budget even when its cache is replaced.
    password.kdf_rounds.store(rounds, Relaxed);
    let key = Zeroizing::new(derive_key(num_cycles_power, salt, password.as_slice()));
    #[cfg(test)]
    {
        cache.derivations += 1;
    }
    if cache.capacity() > KEY_CACHE_ENTRIES {
        if cache.retained.len() == cache.capacity() {
            cache.retained.pop_first();
        }
        cache
            .retained
            .insert((num_cycles_power, salt.to_vec()), key.clone());
    } else {
        if cache.entries.len() == cache.capacity() {
            cache.entries.remove(0);
        }
        cache.entries.push(CachedKey {
            power: num_cycles_power,
            salt: salt.to_vec(),
            key: key.clone(),
        });
    }
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
        // Before the properties are built: they keep six bits of the value.
        options.checked_num_cycles_power()?;
        let (key, iv) = crate::encryption::aes::get_aes_key(
            &options.properties(),
            &options.password,
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )?;

        Ok(Self {
            output,
            enc: Aes256CbcEnc::new(key.as_ref(), &iv)
                .map_err(|e| crate::Error::internal(e.to_string()))?,
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

    #[cfg(feature = "compress")]
    #[test]
    fn public_folder_aes_facts_decode_writer_ciphertext() {
        use crate::{
            Archive, ArchiveEntry, ArchiveLimits, ArchiveWriter, EncoderConfiguration,
            EncoderMethod,
        };
        use std::io::Cursor;
        let password = Password::new("fixture-key");
        let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(vec![
            EncoderConfiguration::from(crate::encoder_options::AesEncoderOptions::new(
                password.clone(),
            )),
            EncoderConfiguration::new(EncoderMethod::COPY),
        ]);
        let payloads = [vec![7; 19], vec![31; 33]];
        for (index, payload) in payloads.iter().enumerate() {
            let mut entry = ArchiveEntry::new_file(&format!("member-{index}"));
            entry.size = payload.len() as u64;
            writer
                .push_archive_entry(entry, Some(Cursor::new(payload)))
                .unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let archive = Archive::read(&mut Cursor::new(&bytes), &password).unwrap();
        assert_eq!(archive.blocks.len(), payloads.len());
        for (block, payload) in payloads.iter().enumerate() {
            let facts = archive.block_aes_properties(block).unwrap();
            assert_eq!(facts.len(), 1);
            let key = facts[0]
                .derive_key(&password, &ArchiveLimits::default())
                .unwrap();
            let range = archive.block_pack_streams(block)[0];
            let mut cipher =
                bytes[range.offset as usize..(range.offset + range.size) as usize].to_vec();
            Aes256Cbc::new(key.as_bytes(), facts[0].iv())
                .unwrap()
                .decrypt(&mut cipher)
                .unwrap();
            assert_eq!(&cipher[..payload.len()], payload);
            assert_eq!(cipher.len(), payload.len().div_ceil(16) * 16);
            assert!(cipher[payload.len()..].iter().all(|byte| *byte == 0));
        }
        assert!(archive.block_aes_properties(archive.blocks.len()).is_err());
    }

    #[test]
    fn public_aes_facts_cover_short_full_and_invalid_properties() {
        let empty_salt = AesProperties::parse(&[4]).unwrap();
        assert_eq!(empty_salt.cycles_power(), 4);
        assert_eq!(empty_salt.salt(), b"");
        assert_eq!(empty_salt.iv(), &[0; 16]);
        for properties in [vec![], vec![0xC4], vec![0xC4, 0xFF], vec![0x44, 0x0F, 0]] {
            assert!(AesProperties::parse(&properties).is_err());
        }
        let mut bytes = vec![0xC4, 0xFF];
        bytes.extend(0..32);
        let facts = AesProperties::parse(&bytes).unwrap();
        assert_eq!(facts.salt(), &(0..16).collect::<Vec<u8>>());
        assert_eq!(facts.iv().as_slice(), &(16..32).collect::<Vec<u8>>());
    }

    #[test]
    fn public_aes_derivation_shares_decoder_cache_and_limits() {
        let password = Password::new("fixture-password").with_aes_key_cache_capacity(8);
        let limits = crate::ArchiveLimits {
            max_aes_cycles_power: 4,
            max_aes_kdf_rounds: 8 * 16,
            ..Default::default()
        };
        for salt in 0..8 {
            let properties = [0x84, 0, salt];
            let (decoder_key, _) = get_aes_key(&properties, &password, 4, 8 * 16).unwrap();
            let facts = AesProperties::parse(&properties).unwrap();
            let public_key = facts.derive_key(&password, &limits).unwrap();
            assert_eq!(public_key.as_bytes(), &*decoder_key);
            assert_eq!(format!("{public_key:?}"), "AesKey([REDACTED])");
        }
        for salt in 0..8 {
            AesProperties::parse(&[0x84, 0, salt])
                .unwrap()
                .derive_key(&password, &limits)
                .unwrap();
        }
        assert_eq!(derivations(&password), 8);
        assert_eq!(spent(&password), 128);
        assert_eq!(
            AesProperties::parse(&[0x84, 0, 8])
                .unwrap()
                .derive_key(&password, &limits)
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesKdfRounds)
        );
        assert_eq!(
            AesProperties::parse(&[5])
                .unwrap()
                .derive_key(&password, &limits)
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesCyclesPower)
        );
        assert!(matches!(
            AesProperties::parse(&[0])
                .unwrap()
                .derive_key(&Password::empty(), &limits),
            Err(crate::Error::PasswordRequired)
        ));
        let raw = AesProperties::parse(&[0xBF, 0, 7])
            .unwrap()
            .derive_key(&password, &limits)
            .unwrap();
        assert_eq!(raw.as_bytes()[0], 7);
        assert_eq!(&raw.as_bytes()[1..], &password.as_slice()[..31]);
        assert_eq!(spent(&password), 128);
    }

    fn spent(password: &Password) -> u64 {
        password
            .kdf_rounds
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn derivations(password: &Password) -> u64 {
        password.key_cache.lock().unwrap().derivations
    }

    #[test]
    fn budget_charges_misses_including_evictions_but_not_hits() {
        let password = Password::new("test");
        for _ in 0..513 {
            derive_key_with_budget(2, b"same", &password, 4).unwrap();
        }
        assert_eq!(spent(&password), 4);
        assert_eq!(
            derive_key_with_budget(2, b"other", &password, 4)
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesKdfRounds)
        );
        derive_key_with_budget(2, b"other", &password, 8).unwrap();
        // Enough other salts to push "same" out of the cache: asking for it
        // again is a derivation, and is charged like one.
        for salt in [b"a", b"b", b"c"] {
            derive_key_with_budget(2, salt, &password, u64::MAX).unwrap();
        }
        assert_eq!(spent(&password), 20);
        assert_eq!(
            derive_key_with_budget(2, b"same", &password, 20)
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesKdfRounds)
        );
        password
            .kdf_rounds
            .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        assert!(derive_key_with_budget(0, b"new", &password, u64::MAX).is_err());
    }

    #[test]
    fn public_cache_capacity_changes_preserve_keys_and_existing_budgets() {
        let password = Password::new("cache-fixture");
        for salt in 0..4 {
            derive_key_with_budget(2, &[salt], &password, 16).unwrap();
        }
        let password = password.with_aes_key_cache_capacity(8);
        for salt in 0..8 {
            derive_key_with_budget(2, &[salt], &password, 32).unwrap();
        }
        assert_eq!(derivations(&password), 8);
        assert_eq!(spent(&password), 32);
        let copy = password.clone();
        assert_eq!(spent(&copy), 0);
        derive_key_with_budget(2, &[7], &copy, 0).unwrap();
        assert_eq!(spent(&copy), 0);
        assert_eq!(spent(&password), 32);
        let password = password.with_aes_key_cache_capacity(2);
        assert_eq!(spent(&password), 32);
        derive_key_with_budget(2, &[7], &password, 32).unwrap();
        assert_eq!(
            derive_key_with_budget(2, &[0], &password, 32)
                .unwrap_err()
                .limit_hit(),
            Some(crate::Limit::AesKdfRounds)
        );
        derive_key_with_budget(2, &[0], &copy, 4).unwrap();
        assert_eq!(spent(&copy), 4);
        assert_eq!(spent(&password), 32);
        let password = password.with_aes_key_cache_capacity(0);
        assert_eq!(password.key_cache.lock().unwrap().capacity(), 4);
        assert_eq!(spent(&password), 32);
    }

    #[test]
    fn cached_derivation_matches_reference() {
        let password = Password::new("pass");
        let expected = derive_key(CYCLES, b"salt", password.as_slice());
        assert_eq!(*derive_key_cached(CYCLES, b"salt", &password), expected);
        assert_eq!(*derive_key_cached(CYCLES, b"salt", &password), expected);
        assert_eq!(derivations(&password), 1);
    }

    /// The derivation 7zAes.c spells out: three hash calls a round.
    fn derive_key_round_by_round(power: u8, salt: &[u8], password: &[u8]) -> [u8; 32] {
        let mut sha = Sha256::new();
        let mut extra = [0u8; 8];
        for _ in 0..(1u64 << power) {
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

    /// Below, at and above the unroll width, with and without salt, and a
    /// password long enough that a batch is many hash blocks.
    #[test]
    fn batched_rounds_hash_the_same_bytes() {
        let long = vec![0xA5u8; 300];
        for power in [0, 1, 5, 6, 7, 11] {
            for salt in [&b""[..], b"s", b"0123456789abcdef"] {
                for password in [&b"p\0w\0"[..], long.as_slice()] {
                    assert_eq!(
                        derive_key(power, salt, password),
                        derive_key_round_by_round(power, salt, password),
                        "power {power}, salt {} bytes, password {} bytes",
                        salt.len(),
                        password.len()
                    );
                }
            }
        }
    }

    /// The rounds laid out for a call stay within the bound whatever a round's
    /// length, and a round longer than the bound is not laid out.
    #[test]
    fn the_rounds_laid_out_for_a_call_are_bounded() {
        // An ordinary password: every round the unroll width allows.
        assert_eq!(kdf_unroll_power(19, 64), Some(KDF_UNROLL_POWER));
        assert_eq!(kdf_unroll_power(3, 64), Some(3));
        assert_eq!(kdf_unroll_power(0, 64), Some(0));
        // The longest round that keeps them all, and the first that does not.
        let full = KDF_BATCH_BYTES >> KDF_UNROLL_POWER;
        assert_eq!(kdf_unroll_power(19, full), Some(KDF_UNROLL_POWER));
        assert_eq!(kdf_unroll_power(19, full + 1), Some(KDF_UNROLL_POWER - 1));
        assert_eq!(kdf_unroll_power(19, KDF_BATCH_BYTES), Some(0));
        assert_eq!(kdf_unroll_power(19, KDF_BATCH_BYTES + 1), None);
        assert_eq!(kdf_unroll_power(19, usize::MAX), None);
        for round_len in [
            8,
            9,
            100,
            full - 1,
            full,
            full + 1,
            3 * full,
            KDF_BATCH_BYTES,
        ] {
            for power in [0, 1, 5, 6, 7, 24] {
                let unroll = kdf_unroll_power(power, round_len).expect("laid out");
                assert!(unroll <= power.min(KDF_UNROLL_POWER));
                assert!(
                    round_len << unroll <= KDF_BATCH_BYTES,
                    "{round_len} << {unroll}"
                );
            }
        }
    }

    /// A hash that only measures what it is handed.
    #[derive(Default)]
    struct Measured {
        bytes: u64,
        calls: u64,
        widest: usize,
    }

    thread_local! {
        /// What the last [`Measured`] of this thread was handed.
        static MEASURED: std::cell::Cell<(u64, u64, usize)> =
            const { std::cell::Cell::new((0, 0, 0)) };
    }

    impl Sha256Like for Measured {
        fn new() -> Self {
            Self::default()
        }

        fn update(&mut self, data: &[u8]) {
            self.bytes += data.len() as u64;
            self.calls += 1;
            self.widest = self.widest.max(data.len());
        }

        fn finalize(self) -> [u8; 32] {
            MEASURED.with(|cell| cell.set((self.bytes, self.calls, self.widest)));
            [0; 32]
        }
    }

    /// The bytes, the calls and the widest call of one derivation.
    fn measured(power: u8, salt: &[u8], password: &[u8]) -> (u64, u64, usize) {
        derive_key_with::<Measured>(power, salt, password);
        MEASURED.with(std::cell::Cell::get)
    }

    /// The password is the caller's and of any length; what is laid out for
    /// the hash is not sized by it.
    #[test]
    fn a_long_password_is_not_laid_out_sixty_four_times() {
        let salt = b"0123456789abcdef";
        // Rounds of a kilobyte: all 64 to a call, 128 rounds in two calls.
        let password = vec![0x5Au8; (KDF_BATCH_BYTES >> KDF_UNROLL_POWER) - 24];
        assert_eq!(
            measured(7, salt, &password),
            (128 * 1024, 2, KDF_BATCH_BYTES)
        );
        // A byte longer: 32 to a call, and no call wider than the bound.
        let password = vec![0x5Au8; (KDF_BATCH_BYTES >> KDF_UNROLL_POWER) - 23];
        assert_eq!(measured(7, salt, &password), (128 * 1025, 4, 32 * 1025));
        // A round longer than the bound by itself: its three pieces, as they
        // are, and the widest of them is the password where it lies.
        let password = vec![0x5Au8; KDF_BATCH_BYTES];
        let round = (salt.len() + password.len() + 8) as u64;
        assert_eq!(
            measured(7, salt, &password),
            (128 * round, 3 * 128, password.len())
        );
    }

    /// The same key at every width of layout, down to none.
    #[test]
    fn bounded_rounds_hash_the_same_bytes() {
        let full = KDF_BATCH_BYTES >> KDF_UNROLL_POWER;
        for len in [full - 8, full - 7, KDF_BATCH_BYTES - 8, KDF_BATCH_BYTES - 7] {
            let password = vec![0xC3u8; len];
            for power in [0, 6, 7] {
                assert_eq!(
                    derive_key(power, b"", &password),
                    derive_key_round_by_round(power, b"", &password),
                    "power {power}, password {len} bytes"
                );
            }
        }
        let password = vec![0xC3u8; 3 * full];
        assert_eq!(
            derive_key(9, b"0123456789abcdef", &password),
            derive_key_round_by_round(9, b"0123456789abcdef", &password)
        );
    }

    /// A clone is the same password: the key one copy derived, every copy
    /// has, and a hit costs the copy that asked nothing. The budget stays
    /// each copy's own.
    #[test]
    fn clones_share_derived_keys_but_not_the_budget() {
        let password = Password::new("pass");
        let key = derive_key_cached(CYCLES, b"salt", &password);
        let copy = password.clone();
        let copy_of_copy = copy.clone();
        assert_eq!(derive_key_cached(CYCLES, b"salt", &copy), key);
        assert_eq!(derive_key_cached(CYCLES, b"salt", &copy_of_copy), key);
        assert_eq!(derivations(&password), 1);
        assert_eq!(spent(&password), 1 << CYCLES);
        assert_eq!(spent(&copy), 0);
        // A key only the copy needed is charged to the copy alone.
        derive_key_cached(CYCLES, b"other", &copy);
        assert_eq!((spent(&password), spent(&copy)), (1 << CYCLES, 1 << CYCLES));
        // An equal password made separately is not a clone and shares nothing.
        let separate = Password::new("pass");
        derive_key_cached(CYCLES, b"salt", &separate);
        assert_eq!(derivations(&separate), 1);
        assert_eq!(derivations(&password), 2);
    }

    /// Copies asking for one key at the same time derive it once.
    #[test]
    fn concurrent_clones_derive_once() {
        let password = Password::new("pass");
        let keys: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let copy = password.clone();
                    scope.spawn(move || *derive_key_cached(10, b"salt", &copy))
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });
        assert!(keys.iter().all(|key| *key == keys[0]));
        assert_eq!(derivations(&password), 1);
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

    fn derivations(password: &Password) -> u64 {
        password.key_cache.lock().unwrap().derivations
    }

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
    fn bulk_aes_read_uses_the_copy_callers_buffer_without_plaintext_staging() {
        let original = long_input();
        let (encoded, options, password) = encode(&original);
        let mut decoder = Aes256Sha256Decoder::new(
            encoded.as_slice(),
            &options.properties(),
            &password,
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )
        .unwrap();
        // COPY calls this Read path with its own push buffer. AES decrypts
        // directly into it and retains only the partial cipher block.
        let mut buffer = vec![0; crate::decoder::PUSH_BUFFER_BYTES];
        let size = decoder.read(&mut buffer).unwrap();
        assert_eq!(&buffer[..original.len()], original);
        assert_eq!(size, encoded.len());
        assert!(decoder.plain.is_empty());
        assert_eq!(decoder.read(&mut buffer).unwrap(), 0);
        assert!(decoder.plain.is_empty());
    }

    #[test]
    fn failed_plaintext_growth_releases_spent_storage_without_consuming_input() {
        let original = long_input();
        let (encoded, options, password) = encode(&original);
        let mut decoder = Aes256Sha256Decoder::new(
            FragmentedReader::new(encoded.as_slice(), &[AES_CHUNK_LEN - 7]),
            &options.properties(),
            &password,
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )
        .unwrap();
        let mut actual = Vec::new();
        let mut prefix = [0; 13];
        decoder.read_exact(&mut prefix).unwrap();
        actual.extend_from_slice(&prefix);
        decoder
            .push(AES_CHUNK_LEN, &mut |bytes| actual.extend_from_slice(bytes))
            .unwrap();
        assert_eq!(decoder.plain_start, decoder.plain_end);
        assert_eq!(decoder.plain.len(), AES_CHUNK_LEN);
        let input_left = decoder.input.inner.len();
        let position = decoder.pos;
        assert_ne!(decoder.carry_len, 0);
        let carry = decoder.carry;
        let carry_len = decoder.carry_len;
        let error = decoder.reserve_plain(usize::MAX).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::OutOfMemory);
        assert!(decoder.plain.is_empty());
        assert_eq!(decoder.input.inner.len(), input_left);
        assert_eq!(decoder.pos, position);
        assert_eq!(decoder.carry, carry);
        assert_eq!(decoder.carry_len, carry_len);
        while decoder
            .push(usize::MAX, &mut |bytes| actual.extend_from_slice(bytes))
            .unwrap()
            != 0
        {}
        assert_eq!(&actual[..original.len()], original);
        assert!(actual[original.len()..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn pushes_preserve_partial_blocks_across_interrupted_reads() {
        struct Interrupted<R> {
            inner: R,
            interrupt: bool,
        }
        impl<R: Read> Read for Interrupted<R> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.interrupt = !self.interrupt;
                if self.interrupt {
                    Err(std::io::ErrorKind::Interrupted.into())
                } else {
                    self.inner.read(bytes)
                }
            }
        }
        let original = long_input();
        let (encoded, options, password) = encode(&original);
        let input = Interrupted {
            inner: FragmentedReader::new(encoded.as_slice(), &[1, 7, 17, 4093]),
            interrupt: false,
        };
        let mut decoder = Aes256Sha256Decoder::new(
            input,
            &options.properties(),
            &password,
            MAX_AES_CYCLES_POWER,
            u64::MAX,
        )
        .unwrap();
        assert_eq!(decoder.push(0, &mut |_| panic!("empty push")).unwrap(), 0);
        assert!(decoder.plain.is_empty());
        let mut actual = Vec::new();
        let mut prefix = [0; 13];
        decoder.read_exact(&mut prefix).unwrap();
        actual.extend_from_slice(&prefix);
        while decoder
            .push(usize::MAX, &mut |bytes| actual.extend_from_slice(bytes))
            .unwrap()
            != 0
        {}
        assert_eq!(&actual[..original.len()], original);
        assert!(actual[original.len()..].iter().all(|&byte| byte == 0));
        assert_eq!(decoder.plain.len(), crate::decoder::PUSH_BUFFER_BYTES);
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

    /// A work factor the six bits of the coder properties cannot hold is
    /// refused where the encoder is built, before anything is written: cut
    /// down to those bits, 64 would be one round and 127 no derivation at
    /// all.
    #[test]
    fn a_work_factor_the_properties_cannot_hold_is_refused() {
        for power in [MAX_AES_CYCLES_POWER + 1, 62, 64, 64 + 19, 127, 128, 255] {
            let options = AesEncoderOptions::new(Password::new("pw")).with_num_cycles_power(power);
            let refused = Aes256Sha256Encoder::new(Vec::<u8>::new(), &options)
                .err()
                .unwrap_or_else(|| panic!("a work factor of {power} was accepted"));
            assert_eq!(
                refused.limit_hit(),
                Some(crate::Limit::AesCyclesPower),
                "{power}: {refused:?}"
            );

            // Through the writer too: the folder is refused, not written weak.
            let mut writer = crate::ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
            writer.set_content_methods(vec![options.into()]);
            let pushed = writer.push_archive_entry(
                crate::ArchiveEntry::new_file("member.txt"),
                Some(b"member bytes".as_slice()),
            );
            assert_eq!(
                pushed.err().and_then(|e| e.limit_hit()),
                Some(crate::Limit::AesCyclesPower),
                "{power} through the writer"
            );
        }
        // A work factor the format can say is written as it was given.
        let options = AesEncoderOptions::new(Password::new("pw")).with_num_cycles_power(6);
        assert_eq!(options.properties()[0] & 0x3F, 6);
        assert!(Aes256Sha256Encoder::new(Vec::<u8>::new(), &options).is_ok());
    }

    /// The key is derived once per archive, however many folders it has: the
    /// encoder clones its options for each folder it sizes and for the
    /// encrypted header, and every clone carries the key. Reading it back
    /// with an equal password made separately is one derivation too, with
    /// the reader's blocks sharing it.
    #[test]
    fn a_many_folder_archive_derives_its_key_once() {
        const FOLDERS: usize = 24;
        let password = Password::new("many folders");
        let options = AesEncoderOptions::new(password.clone());
        assert_eq!(
            options.num_cycles_power,
            AesEncoderOptions::DEFAULT_NUM_CYCLES_POWER
        );
        let mut writer = crate::ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        writer.set_content_methods(vec![
            options.into(),
            crate::encoder_options::Lzma2Options::from_level(1).into(),
        ]);
        writer.set_encrypt_header(true);
        let member = |i: usize| format!("member {i} of the many-folder archive\n").repeat(40 + i);
        for i in 0..FOLDERS {
            let data = member(i);
            // A declared size is what makes the encoder size, and so clone,
            // the folder's methods.
            let mut entry = crate::ArchiveEntry::new_file(&format!("m{i:02}.txt"));
            entry.size = data.len() as u64;
            writer
                .push_archive_entry(entry, Some(data.as_bytes()))
                .unwrap();
        }
        let archive = writer.finish().unwrap().into_inner();
        assert_eq!(derivations(&password), 1);

        let reading = Password::new("many folders");
        let mut reader = crate::ArchiveReader::new(Cursor::new(archive), reading.clone()).unwrap();
        assert_eq!(reader.archive().blocks.len(), FOLDERS);
        let mut seen = 0;
        reader
            .for_each_entries(|entry, data| {
                let mut bytes = String::new();
                data.read_to_string(&mut bytes).unwrap();
                assert_eq!(entry.name(), format!("m{seen:02}.txt"));
                assert_eq!(bytes, member(seen));
                seen += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(seen, FOLDERS);
        assert_eq!(derivations(&reading), 1);
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
