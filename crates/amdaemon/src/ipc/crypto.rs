//! Session crypto of a Messenger (`amdaemon::ipc::message::Crypto`, `spec/ipc/messenger_crypto.md` §2.3, §6, §7).
//!
//! Each side generates three ephemeral ECDH P-256 key pairs (K0, K1, K2 in creation order) and sends one Key
//! message carrying the three public blobs in wire order `K0, K2, K1`. From the peer's message it derives
//!
//! * `S0 = SHA256(x(ECDH(K0, Q0)))` — its second half is the AES-128-GCM key,
//! * `N_tx = SHA256(x(ECDH(K1, Q1)))[16..28)` — nonce for records it sends,
//! * `N_rx = SHA256(x(ECDH(K2, Q2)))[16..28)` — nonce for records it receives,
//!
//! where `Q0, Q1, Q2` are the peer's wire slots. The Windows CNG provider is replaced by RustCrypto
//! (`p256`, `sha2`, `aes-gcm`). `BCryptDeriveKey(L"HASH", SHA256)` hashes the big-endian x coordinate; the GCM
//! tag is the CNG minimum of 12 bytes ([`TAG_LEN`]).
//!
//! The AES-128-CBC objects the original also keys (`Mode::Cbc`, `S0[0..16)` and the IVs) are never selected by
//! any caller in the binary (`encode`/`decode` are only ever called with mode 0), so they are not implemented.

use aes_gcm::AesGcm;
use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Nonce, Tag};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

/// AES-128-GCM with a 96-bit nonce and the 12-byte tag CNG reports as `AuthTagLength.dwMinLength`.
type Gcm12 = AesGcm<aes_gcm::aes::Aes128, U12, U12>;

/// GCM tag length appended to every record (`Crypto+0x210`, U1 in the spec).
pub const TAG_LEN: usize = 12;
/// Length of a GCM nonce.
pub const NONCE_LEN: usize = 12;
/// Length of one `ECCPUBLICBLOB` of a P-256 key: 8-byte header + X + Y.
pub const PUBLIC_BLOB_LEN: usize = 72;
/// Length of the Key message: three u32 lengths plus three blobs.
pub const KEY_MESSAGE_LEN: usize = 12 + 3 * PUBLIC_BLOB_LEN;
/// `BCRYPT_ECDH_PUBLIC_P256_MAGIC` (`"ECK1"`).
const ECDH_P256_PUBLIC_MAGIC: u32 = 0x314B_4345;
/// Size of a P-256 coordinate.
const COORDINATE_LEN: usize = 32;

/// Result of [`Crypto::decode`] (`Crypto::DecodeResult`, names are inferred, values are direct).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum DecodeResult {
    /// Record authenticated and unpadded.
    Success = 0,
    /// Session not established.
    NotReady = 1,
    /// Never produced by `decode`. The Messenger logs an error if it appears.
    Reserved = 2,
    /// Empty/short input, or the length footer is invalid.
    Malformed = 3,
    /// GCM tag mismatch (`STATUS_AUTH_TAG_MISMATCH`).
    AuthTagMismatch = 4,
}

/// Directional record keys of an established session.
struct Session {
    cipher: Gcm12,
    enc_nonce: [u8; NONCE_LEN],
    dec_nonce: [u8; NONCE_LEN],
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key schedule is deliberately not printed.
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

/// Crypto context: three ephemeral key pairs, the prepared Key message and the established session.
#[derive(Debug)]
pub struct Crypto {
    /// K0, K1, K2 in creation order (`Crypto+0x38`, `+0x70`, `+0xA8`).
    keys: [SecretKey; 3],
    /// Key message body (`Crypto+0x228`).
    pub_message: [u8; KEY_MESSAGE_LEN],
    /// `Some` exactly while `established` (`Crypto+0x240`).
    session: Option<Session>,
}

impl Crypto {
    /// Generates three fresh key pairs (`0x14007A800`). `None` when the OS random source fails, which the
    /// original reports as `valid == false` (the Messenger then stays in state 0).
    pub fn generate() -> Option<Self> {
        let mut keys = Vec::with_capacity(3);
        for _ in 0..3 {
            keys.push(random_secret_key()?);
        }
        let keys: [SecretKey; 3] = keys.try_into().ok()?;
        Some(Self::with_keys(keys))
    }

    /// Builds a context around caller-chosen key pairs (deterministic tests, vectors of the spec §8.2).
    pub(crate) fn with_keys(keys: [SecretKey; 3]) -> Self {
        let mut pub_message = [0u8; KEY_MESSAGE_LEN];
        let blob_len = PUBLIC_BLOB_LEN as u32;
        for slot in 0..3 {
            pub_message[slot * 4..slot * 4 + 4].copy_from_slice(&blob_len.to_le_bytes());
        }
        // Wire order is K0, K2, K1 (`0x14007A800`).
        for (slot, key) in [0usize, 2, 1].into_iter().enumerate() {
            let start = 12 + slot * PUBLIC_BLOB_LEN;
            pub_message[start..start + PUBLIC_BLOB_LEN].copy_from_slice(&export_public_blob(&keys[key].public_key()));
        }
        Self { keys, pub_message, session: None }
    }

    /// The Key message to send in a Type-1 frame (`Crypto+0x228`, 228 bytes).
    pub fn pub_message(&self) -> &[u8] {
        &self.pub_message
    }

    /// `established`: record keys and nonces are valid.
    pub fn is_established(&self) -> bool {
        self.session.is_some()
    }

    /// Processes the peer's Key message (`processKey`, `0x140079450`). `established` is cleared first and set
    /// only when everything succeeds.
    ///
    /// Deviation: the original validates the three lengths with a wrapping 32-bit sum and then reads the blobs
    /// without checking each one against the buffer; here a blob that does not fit is rejected.
    pub fn process_key(&mut self, message: &[u8]) -> bool {
        self.session = None;
        let Some(header) = message.get(..12) else { return false };
        let word = |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
        let lens = [word(0), word(4), word(8)];
        if lens.contains(&0) {
            return false;
        }
        let wrapped_total = lens[0].wrapping_add(lens[1]).wrapping_add(lens[2]);
        if (message.len() as u64) < u64::from(wrapped_total) + 12 {
            return false;
        }
        let mut offset = 12usize;
        let mut peer = Vec::with_capacity(3);
        for len in lens {
            let Some(blob) = message.get(offset..offset.saturating_add(len as usize)) else { return false };
            let Some(key) = import_public_blob(blob) else { return false };
            peer.push(key);
            offset += len as usize;
        }
        let main = kdf(&self.keys[0], &peer[0]);
        let send = kdf(&self.keys[1], &peer[1]);
        let recv = kdf(&self.keys[2], &peer[2]);
        let Ok(cipher) = Gcm12::new_from_slice(&main[16..32]) else { return false };
        let mut enc_nonce = [0u8; NONCE_LEN];
        enc_nonce.copy_from_slice(&send[16..16 + NONCE_LEN]);
        let mut dec_nonce = [0u8; NONCE_LEN];
        dec_nonce.copy_from_slice(&recv[16..16 + NONCE_LEN]);
        self.session = Some(Session { cipher, enc_nonce, dec_nonce });
        true
    }

    /// Encrypts one mention (`Crypto::encode`, `0x1400799E0`, mode 0). `out` receives
    /// `ciphertext[M] ‖ tag[12]` with `M = (len + 19) & !15`; the plaintext is padded with zeros and ends in a
    /// little-endian `u32` length. The send nonce advances after success. `false` when not established.
    pub fn encode(&mut self, plain: &[u8], out: &mut Vec<u8>) -> bool {
        let Some(session) = self.session.as_mut() else { return false };
        let Ok(plain_len) = u32::try_from(plain.len()) else { return false };
        let padded = (plain.len() + 19) & !15;
        out.clear();
        out.extend_from_slice(plain);
        out.resize(padded, 0);
        out[padded - 4..].copy_from_slice(&plain_len.to_le_bytes());
        let nonce = Nonce::<U12>::from(session.enc_nonce);
        let Ok(tag) = session.cipher.encrypt_in_place_detached(&nonce, b"", out) else { return false };
        out.extend_from_slice(&tag);
        increment_nonce(&mut session.enc_nonce);
        true
    }

    /// Decrypts one mention (`Crypto::decode`, `0x140079DE0`, mode 0). On success `out` holds exactly the
    /// original mention. The receive nonce advances once the tag verified, even if the length footer is then
    /// rejected.
    pub fn decode(&mut self, input: &[u8], out: &mut Vec<u8>) -> DecodeResult {
        let Some(session) = self.session.as_mut() else { return DecodeResult::NotReady };
        if input.len() <= TAG_LEN {
            return DecodeResult::Malformed;
        }
        let (ciphertext, tag) = input.split_at(input.len() - TAG_LEN);
        out.clear();
        out.extend_from_slice(ciphertext);
        let nonce = Nonce::<U12>::from(session.dec_nonce);
        let tag: &Tag<U12> = tag.into();
        if session.cipher.decrypt_in_place_detached(&nonce, b"", out, tag).is_err() {
            out.clear();
            return DecodeResult::AuthTagMismatch;
        }
        increment_nonce(&mut session.dec_nonce);
        let Some(footer) = out.len().checked_sub(4) else { return DecodeResult::Malformed };
        let declared = u32::from_le_bytes([out[footer], out[footer + 1], out[footer + 2], out[footer + 3]]);
        if footer < declared as usize {
            return DecodeResult::Malformed;
        }
        out.truncate(declared as usize);
        DecodeResult::Success
    }
}

/// Draws a uniformly distributed non-zero scalar from the OS random source.
fn random_secret_key() -> Option<SecretKey> {
    let mut bytes = [0u8; COORDINATE_LEN];
    loop {
        OsRng.try_fill_bytes(&mut bytes).ok()?;
        // Rejects zero and values >= the group order (probability about 2^-32).
        if let Ok(key) = SecretKey::from_slice(&bytes) {
            return Some(key);
        }
    }
}

/// `BCryptExportKey(L"ECCPUBLICBLOB")`: `{magic "ECK1", cbKey 32} ‖ X ‖ Y` (big-endian coordinates).
fn export_public_blob(key: &PublicKey) -> [u8; PUBLIC_BLOB_LEN] {
    let point = key.to_encoded_point(false);
    let mut blob = [0u8; PUBLIC_BLOB_LEN];
    blob[..4].copy_from_slice(&ECDH_P256_PUBLIC_MAGIC.to_le_bytes());
    blob[4..8].copy_from_slice(&(COORDINATE_LEN as u32).to_le_bytes());
    // An uncompressed encoding of a valid public key always carries both coordinates.
    if let (Some(x), Some(y)) = (point.x(), point.y()) {
        blob[8..8 + COORDINATE_LEN].copy_from_slice(x);
        blob[8 + COORDINATE_LEN..].copy_from_slice(y);
    }
    blob
}

/// `BCryptImportKeyPair(L"ECCPUBLICBLOB")`: wrong magic, `cbKey`, size or a point off the curve fail.
fn import_public_blob(blob: &[u8]) -> Option<PublicKey> {
    if blob.len() != PUBLIC_BLOB_LEN {
        return None;
    }
    let magic = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]);
    let key_len = u32::from_le_bytes([blob[4], blob[5], blob[6], blob[7]]);
    if magic != ECDH_P256_PUBLIC_MAGIC || key_len != COORDINATE_LEN as u32 {
        return None;
    }
    let mut sec1 = [0u8; 1 + 2 * COORDINATE_LEN];
    sec1[0] = 0x04;
    sec1[1..].copy_from_slice(&blob[8..]);
    PublicKey::from_sec1_bytes(&sec1).ok()
}

/// `makeSecretAgreementKey` with `BCryptDeriveKey(L"HASH", KDF_HASH_ALGORITHM = L"SHA256")`:
/// `SHA256(big-endian x of the ECDH point)`.
fn kdf(own: &SecretKey, peer: &PublicKey) -> [u8; 32] {
    let shared = p256::ecdh::diffie_hellman(own.to_nonzero_scalar(), peer.as_affine());
    Sha256::digest(shared.raw_secret_bytes()).into()
}

/// Increments a nonce as three little-endian `u32` words, word 2 (bytes 8..12) least significant, carrying
/// into word 1 and then word 0 (`0x1400799E0`).
fn increment_nonce(nonce: &mut [u8; NONCE_LEN]) {
    for word in (0..3).rev() {
        let range = word * 4..word * 4 + 4;
        let value = u32::from_le_bytes([
            nonce[range.start],
            nonce[range.start + 1],
            nonce[range.start + 2],
            nonce[range.start + 3],
        ]);
        let (next, carry) = value.overflowing_add(1);
        nonce[range].copy_from_slice(&next.to_le_bytes());
        if !carry {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn keys(d: [&str; 3]) -> [SecretKey; 3] {
        d.map(|d| SecretKey::from_slice(&hex(d)).unwrap())
    }

    /// Side A and B of the model vector of `spec/ipc/messenger_crypto.md` §8.2.
    fn model_pair() -> (Crypto, Crypto) {
        let a = Crypto::with_keys(keys([
            "a62d2b4c07206f2377f4cc94b272c05a17245579b4d33017e2584d185ee272f5",
            "a6b3ab607c8d1f284aba30d9ffc718a96772ba1d7c9871817e42976fb01f85a8",
            "11dead01d0ce195ad821d32df69c68be5ff85915fbb762b84bc774dd232626ee",
        ]));
        let b = Crypto::with_keys(keys([
            "18f74369f440c86cc849a7b1c20c14172f7b3583cd8ddd99be5be035481bfc7f",
            "7d7f7d220af45b3939ebf0c6c92ff8807a8c66c9f566b6c3a705af7422f90167",
            "6ee59e59ffa4cb40090d1950177ab2c3a3932c12e6df168cf758ed04c1c49284",
        ]));
        (a, b)
    }

    fn established_pair() -> (Crypto, Crypto) {
        let (mut a, mut b) = model_pair();
        assert!(a.process_key(b.pub_message()));
        assert!(b.process_key(a.pub_message()));
        (a, b)
    }

    #[test]
    fn key_message_layout_matches_model_vector() {
        let (a, b) = model_pair();
        let expected_a = hex("48000000 48000000 48000000
            45434b31 20000000 098d5a2a22aec457dd60556967e5880d5e4607548daebdce8d9b4c14d5f7836e b5f628b832c357a7838c4c7ba4d85803b0f3a7a7461c0ee4c164228daf923704
            45434b31 20000000 2ee06ad19b0130c6ef81cc901064c183817647bfc4643e84d1ec19f00438361e f87f7a3c8eb3ceced4cdc61fc2205673bc9fce864c3375c9c13fc722c85a0aa5
            45434b31 20000000 1f75fbe5a7918bdd610931f9c50988e8841953ffbff48df9bacb4fb7a34b54f5 e58740f61648966c8e5a153ae48ebc825a78db3b63c15b3fc34c35da5880af78");
        assert_eq!(a.pub_message(), &expected_a[..]);
        assert_eq!(a.pub_message().len(), 228);
        let expected_b_first = hex(
            "48000000 48000000 48000000 45434b31 20000000 0e20bd5b8a7f9437a4a7f02076f1d727b0f0fcb5412af716fa2321e84a3a5c66",
        );
        assert_eq!(&b.pub_message()[..expected_b_first.len()], &expected_b_first[..]);
    }

    #[test]
    fn both_sides_derive_the_model_keys_and_directional_nonces() {
        let (a, b) = established_pair();
        let (sa, sb) = (a.session.as_ref().unwrap(), b.session.as_ref().unwrap());
        assert_eq!(sa.enc_nonce.to_vec(), hex("1bc000457ce6379e7bd00829"));
        assert_eq!(sa.dec_nonce.to_vec(), hex("9c69409a634f750f4765a8e4"));
        // Each side's transmit nonce is the other side's receive nonce.
        assert_eq!(sa.enc_nonce, sb.dec_nonce);
        assert_eq!(sa.dec_nonce, sb.enc_nonce);
    }

    #[test]
    fn encode_matches_model_ciphertext_and_receiver_follows_the_nonce_stream() {
        let (mut a, mut b) = established_pair();
        let (mut hello, mut empty, mut plain) = (Vec::new(), Vec::new(), Vec::new());
        assert!(a.encode(b"Hello", &mut hello));
        assert_eq!(hello, hex("18b39ae1576f74ffaeb5bed3ce0acc68 4f5e1e927f32dca5d7d2f8f2"));
        assert!(a.encode(b"", &mut empty));
        assert_eq!(empty, hex("ccda1308ed04524d62342d446dc2b28c 4e0eb9ee3196564e80ef06b3"));
        assert_eq!(a.session.as_ref().unwrap().enc_nonce.to_vec(), hex("1bc000457ce6379e7dd00829"));

        assert_eq!(b.decode(&hello, &mut plain), DecodeResult::Success);
        assert_eq!(plain, b"Hello");
        assert_eq!(b.decode(&empty, &mut plain), DecodeResult::Success);
        assert!(plain.is_empty());
    }

    #[test]
    fn record_round_trip_covers_padding_boundaries() {
        let (mut a, mut b) = established_pair();
        let mut wire = Vec::new();
        let mut plain = Vec::new();
        for len in [0usize, 1, 11, 12, 13, 15, 16, 17, 31, 32, 1000] {
            let message: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert!(a.encode(&message, &mut wire));
            assert_eq!(wire.len(), ((len + 19) & !15) + TAG_LEN, "len {len}");
            assert_eq!(b.decode(&wire, &mut plain), DecodeResult::Success);
            assert_eq!(plain, message);
        }
        // The reverse direction uses the other nonce stream.
        assert!(b.encode(b"reply", &mut wire));
        assert_eq!(a.decode(&wire, &mut plain), DecodeResult::Success);
        assert_eq!(plain, b"reply");
    }

    #[test]
    fn tampered_replayed_and_reordered_records_fail_with_tag_mismatch() {
        let (mut a, mut b) = established_pair();
        let (mut first, mut second, mut plain) = (Vec::new(), Vec::new(), Vec::new());
        assert!(a.encode(b"one", &mut first));
        assert!(a.encode(b"two", &mut second));

        let mut flipped = first.clone();
        flipped[2] ^= 0x01;
        assert_eq!(b.decode(&flipped, &mut plain), DecodeResult::AuthTagMismatch);
        let mut bad_tag = first.clone();
        *bad_tag.last_mut().unwrap() ^= 0x80;
        assert_eq!(b.decode(&bad_tag, &mut plain), DecodeResult::AuthTagMismatch);
        // The nonce did not move on failure: the genuine first record still decodes.
        assert_eq!(b.decode(&first, &mut plain), DecodeResult::Success);
        // Replay of the same record, then the stream continues with the next one.
        assert_eq!(b.decode(&first, &mut plain), DecodeResult::AuthTagMismatch);
        assert_eq!(b.decode(&second, &mut plain), DecodeResult::Success);
        assert_eq!(plain, b"two");
    }

    #[test]
    fn decode_result_codes_for_unready_short_and_bad_footer() {
        let (mut a, mut b) = established_pair();
        let mut plain = Vec::new();
        let mut unready = model_pair().0;
        assert_eq!(unready.decode(&[0u8; 40], &mut plain), DecodeResult::NotReady);
        assert!(!unready.encode(b"x", &mut plain));
        assert_eq!(b.decode(&[], &mut plain), DecodeResult::Malformed);
        assert_eq!(b.decode(&[0u8; TAG_LEN], &mut plain), DecodeResult::Malformed);

        // A valid tag over a footer claiming more bytes than present: Malformed, but the nonce advanced.
        let session = a.session.as_mut().unwrap();
        let mut forged = vec![0u8; 16];
        forged[12..].copy_from_slice(&13u32.to_le_bytes());
        let nonce = Nonce::<U12>::from(session.enc_nonce);
        let tag = session.cipher.encrypt_in_place_detached(&nonce, b"", &mut forged).unwrap();
        forged.extend_from_slice(&tag);
        increment_nonce(&mut session.enc_nonce);
        assert_eq!(b.decode(&forged, &mut plain), DecodeResult::Malformed);
        let mut after = Vec::new();
        assert!(a.encode(b"ok", &mut after));
        assert_eq!(b.decode(&after, &mut plain), DecodeResult::Success);
    }

    #[test]
    fn nonce_increment_carries_from_word_two_into_word_zero() {
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&hex("11111111 ffffffff ffffffff"));
        increment_nonce(&mut nonce);
        assert_eq!(nonce.to_vec(), hex("12111111 00000000 00000000"));
        nonce.copy_from_slice(&hex("01000000 02000000 ffffffff"));
        increment_nonce(&mut nonce);
        assert_eq!(nonce.to_vec(), hex("01000000 03000000 00000000"));
        nonce.copy_from_slice(&hex("00000000 00000000 00000000"));
        increment_nonce(&mut nonce);
        assert_eq!(nonce.to_vec(), hex("00000000 00000000 01000000"));
    }

    #[test]
    fn malformed_key_messages_are_rejected_and_clear_established() {
        let (mut a, b) = established_pair();
        assert!(a.is_established());
        let good = b.pub_message().to_vec();

        assert!(!a.process_key(&good[..11]));
        assert!(!a.is_established(), "established is cleared before validation");

        let mut zero_len = good.clone();
        zero_len[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(!a.process_key(&zero_len));

        assert!(!a.process_key(&good[..good.len() - 1]), "truncated third blob");

        let mut bad_magic = good.clone();
        bad_magic[12] ^= 0xFF;
        assert!(!a.process_key(&bad_magic));

        let mut off_curve = good.clone();
        off_curve[12 + 8 + 63] ^= 0x01;
        assert!(!a.process_key(&off_curve));

        let mut wrong_len = good.clone();
        wrong_len[0..4].copy_from_slice(&71u32.to_le_bytes());
        assert!(!a.process_key(&wrong_len));

        assert!(a.process_key(&good));
        assert!(a.is_established());
    }
}
