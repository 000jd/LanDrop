//! Wire encryption for transfer payloads.
//!
//! Each transfer uses a fresh ephemeral X25519 key pair on both sides. The
//! shared secret is mixed with the receiver's transfer id into a ChaCha20 key
//! and nonce (RFC 8439, no poly1305 — the SHA-256 checksum and the offer/upload
//! token already authenticate the transfer). The stream cipher is seekable: the
//! sender and receiver can apply the keystream at any plaintext byte offset,
//! which makes encryption compatible with resumable uploads.
//!
//! The raw shared secret also seeds the short authentication string (SAS) PIN.
//! A man-in-the-middle on the LAN would establish different secrets with each
//! side and therefore show different PINs on the two screens.

use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// Bytes in a ChaCha20 key.
pub const KEY_LEN: usize = 32;
/// Bytes in a ChaCha20 nonce.
pub const NONCE_LEN: usize = 12;
/// Bytes in an X25519 public key / secret.
pub const DH_LEN: usize = 32;

/// An ephemeral X25519 key pair. `secret` is never logged: the custom `Debug`
/// implementation redacts it.
#[derive(Clone)]
pub struct KeyPair {
    pub secret: [u8; DH_LEN],
    /// Lower-case hex of the matching public key.
    pub public_hex: String,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("public_hex", &self.public_hex)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl KeyPair {
    /// Generate a fresh key from the operating system's random source.
    pub fn generate() -> Option<Self> {
        let mut secret = [0u8; DH_LEN];
        getrandom::getrandom(&mut secret).ok()?;
        Some(Self::from_secret(secret))
    }

    /// Build a key pair from raw secret bytes (clamping happens in X25519).
    pub fn from_secret(secret: [u8; DH_LEN]) -> Self {
        let public = PublicKey::from(&StaticSecret::from(secret));
        Self {
            secret,
            public_hex: hex(&public.to_bytes()),
        }
    }
}

/// A derived per-transfer session. Cloning is cheap and safe; the raw shared
/// secret is kept only for the SAS PIN and is redacted from `Debug`.
#[derive(Clone)]
pub struct SessionKey {
    key: [u8; KEY_LEN],
    nonce: [u8; NONCE_LEN],
    shared: [u8; DH_LEN],
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(<redacted>)")
    }
}

/// Derive the per-transfer session from our secret and the peer's public key.
///
/// Returns `None` for a malformed public key or the all-zero shared secret
/// produced by low-order points.
pub fn derive_session(
    my_secret: &[u8; DH_LEN],
    peer_public_hex: &str,
    transfer_id: &str,
) -> Option<SessionKey> {
    let peer_bytes = decode_hex_32(peer_public_hex)?;
    let shared = StaticSecret::from(*my_secret)
        .diffie_hellman(&PublicKey::from(peer_bytes))
        .to_bytes();
    if shared.iter().all(|b| *b == 0) {
        return None;
    }

    let mut hasher = Sha256::new();
    hasher.update(b"landrop-session-v1:");
    hasher.update(shared);
    hasher.update(transfer_id.as_bytes());
    let digest = hasher.finalize();
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&digest);

    let mut hasher = Sha256::new();
    hasher.update(b"landrop-nonce-v1:");
    hasher.update(shared);
    hasher.update(transfer_id.as_bytes());
    let digest = hasher.finalize();
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&digest[..NONCE_LEN]);

    Some(SessionKey { key, nonce, shared })
}

impl SessionKey {
    /// Encrypt or decrypt `buf` in place at plaintext byte `offset`.
    ///
    /// The same call decrypts: XORing the same keystream twice restores the
    /// plaintext. A resume simply passes the number of plaintext bytes already
    /// written, so the keystream stays aligned across retries.
    pub fn apply_keystream(&self, offset: u64, buf: &mut [u8]) {
        // The ChaCha20 counter is 32 bits; offsets beyond ~256 GiB wrap. That is
        // far beyond any LAN transfer and matches the classic non-ZIP64 limits
        // used elsewhere in the app.
        let mut counter = (offset / 64) as u32;
        let mut skip = (offset % 64) as usize;
        let mut pos = 0;
        while pos < buf.len() {
            let block = chacha20_block(&self.key, &self.nonce, counter);
            let take = (64 - skip).min(buf.len() - pos);
            for i in 0..take {
                buf[pos + i] ^= block[skip + i];
            }
            pos += take;
            counter = counter.wrapping_add(1);
            skip = 0;
        }
    }

    /// Cryptographic short authentication string (`NNN-NNN`).
    ///
    /// Both sides derive the same value only when they hold the same X25519
    /// shared secret, so comparing the PINs out loud defeats a LAN MITM.
    pub fn sas_pin(&self, sender_id: &str, receiver_id: &str, key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"landrop-sas:");
        hasher.update(self.shared);
        hasher.update(sender_id.as_bytes());
        hasher.update(receiver_id.as_bytes());
        hasher.update(key.as_bytes());
        let digest = hasher.finalize();
        let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
        format!("{:03}-{:03}", n / 1000, n % 1000)
    }
}

fn chacha20_block(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], counter: u32) -> [u8; 64] {
    let mut state = [0u32; 16];
    state[0] = 0x6170_7865; // "expa"
    state[1] = 0x3320_646e; // "nd 3"
    state[2] = 0x7962_2d32; // "2-by"
    state[3] = 0x6b20_6574; // "te k"
    for i in 0..8 {
        state[4 + i] =
            u32::from_le_bytes([key[i * 4], key[i * 4 + 1], key[i * 4 + 2], key[i * 4 + 3]]);
    }
    state[12] = counter;
    for i in 0..3 {
        state[13 + i] = u32::from_le_bytes([
            nonce[i * 4],
            nonce[i * 4 + 1],
            nonce[i * 4 + 2],
            nonce[i * 4 + 3],
        ]);
    }

    let mut working = state;
    for _ in 0..10 {
        quarter_round(&mut working, 0, 4, 8, 12);
        quarter_round(&mut working, 1, 5, 9, 13);
        quarter_round(&mut working, 2, 6, 10, 14);
        quarter_round(&mut working, 3, 7, 11, 15);
        quarter_round(&mut working, 0, 5, 10, 15);
        quarter_round(&mut working, 1, 6, 11, 12);
        quarter_round(&mut working, 2, 7, 8, 13);
        quarter_round(&mut working, 3, 4, 9, 14);
    }
    for i in 0..16 {
        working[i] = working[i].wrapping_add(state[i]);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&working[i].to_le_bytes());
    }
    out
}

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_hex_32(text: &str) -> Option<[u8; DH_LEN]> {
    let text = text.trim();
    if text.len() != DH_LEN * 2 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; DH_LEN];
    for i in 0..DH_LEN {
        out[i] = (hex_nibble(bytes[i * 2])? << 4) | hex_nibble(bytes[i * 2 + 1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_session() -> SessionKey {
        SessionKey {
            key: [7u8; KEY_LEN],
            nonce: [9u8; NONCE_LEN],
            shared: [3u8; DH_LEN],
        }
    }

    #[test]
    fn x25519_peers_derive_the_same_secret() {
        let alice = KeyPair::from_secret([1u8; DH_LEN]);
        let bob = KeyPair::from_secret([2u8; DH_LEN]);
        let a = derive_session(&alice.secret, &bob.public_hex, "transfer-1").unwrap();
        let b = derive_session(&bob.secret, &alice.public_hex, "transfer-1").unwrap();

        let mut from_a = b"shared secret".to_vec();
        let mut from_b = from_a.clone();
        a.apply_keystream(0, &mut from_a);
        b.apply_keystream(0, &mut from_b);
        assert_eq!(from_a, from_b, "both sides must agree on the keystream");
        assert_eq!(
            a.sas_pin("alice", "bob", "file.bin:12"),
            b.sas_pin("alice", "bob", "file.bin:12")
        );
    }

    #[test]
    fn sas_pin_changes_with_the_secret_and_the_context() {
        let alice = KeyPair::from_secret([10u8; DH_LEN]);
        let bob = KeyPair::from_secret([20u8; DH_LEN]);
        let eve = KeyPair::from_secret([30u8; DH_LEN]);
        let a = derive_session(&alice.secret, &bob.public_hex, "t").unwrap();
        let mitm = derive_session(&alice.secret, &eve.public_hex, "t").unwrap();
        assert_ne!(
            a.sas_pin("alice", "bob", "key"),
            mitm.sas_pin("alice", "bob", "key"),
            "a different peer key must produce a different PIN"
        );
        assert_ne!(
            a.sas_pin("alice", "bob", "key"),
            a.sas_pin("alice", "bob", "other-key")
        );
        assert_ne!(
            a.sas_pin("alice", "bob", "key"),
            a.sas_pin("alice", "carol", "key")
        );
        let pin = a.sas_pin("alice", "bob", "key");
        assert_eq!(pin.len(), 7);
        assert_eq!(pin.as_bytes()[3], b'-');
    }

    #[test]
    fn derive_session_rejects_bad_keys() {
        let key = [1u8; DH_LEN];
        assert!(derive_session(&key, "not-hex", "t").is_none());
        assert!(derive_session(&key, &"0".repeat(64), "t").is_none());
        assert!(derive_session(&key, &"zz".repeat(32), "t").is_none());
        assert!(derive_session(&key, &hex(&[0u8; DH_LEN]), "t").is_none());
    }

    #[test]
    fn chacha20_matches_rfc_8439_test_vector() {
        // RFC 8439 §2.4.2: key 00..1f, nonce 000000000000004a00000000,
        // counter 1, applied at plaintext offset 64.
        let mut key = [0u8; KEY_LEN];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce[7] = 0x4a;
        let session = SessionKey {
            key,
            nonce,
            shared: [0u8; DH_LEN],
        };
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let expected = "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b\
                        f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8\
                        07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736\
                        5af90bbf74a35be6b40b8eedf2785e42874d";
        let mut buf = plaintext.to_vec();
        session.apply_keystream(64, &mut buf);
        assert_eq!(hex(&buf), expected.replace([' ', '\n'], ""));
    }

    #[test]
    fn keystream_seeks_to_arbitrary_offsets_and_resumes() {
        let session = fixed_session();
        let data: Vec<u8> = (0..1000u32)
            .map(|i| (i.wrapping_mul(37) % 251) as u8)
            .collect();

        let mut whole = data.clone();
        session.apply_keystream(0, &mut whole);

        // Encrypt the same buffer in uneven chunks, each at its own offset.
        let mut split = data.clone();
        let chunks = [
            (0usize, 1usize),
            (1, 7),
            (8, 63),
            (71, 1),
            (72, 29),
            (101, 163),
            (264, 736),
        ];
        for (offset, len) in chunks {
            session.apply_keystream(offset as u64, &mut split[offset..offset + len]);
        }
        assert_eq!(split, whole, "chunk boundaries must not matter");

        // A resume starts exactly where the first attempt stopped.
        let mut resumed = data.clone();
        session.apply_keystream(0, &mut resumed[..123]);
        session.apply_keystream(123, &mut resumed[123..]);
        assert_eq!(resumed, whole, "resume must stay keystream-aligned");

        // XORing twice (decrypt) restores the plaintext.
        session.apply_keystream(0, &mut whole);
        assert_eq!(whole, data);
    }
}
