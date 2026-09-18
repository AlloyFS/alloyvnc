//! VNC authentication (RFC 6143 section 7.2.2): a 16-byte challenge, which
//! the client returns DES-encrypted with the password as the key.
//!
//! Two quirks make this "VNC DES" rather than DES. The password is cut or
//! zero-padded to eight bytes, and every byte of that key has its bits
//! reversed, a mistake in the original implementation that every client
//! since has had to copy. It is also not strong: an eight-character key and
//! an unauthenticated challenge. A password keeps the casual out; anything on
//! a network that matters goes through TLS or a tunnel.

use des::Des;
use des::cipher::{BlockEncrypt, KeyInit};
use rand::RngCore;

pub const CHALLENGE_LEN: usize = 16;
pub const RESPONSE_LEN: usize = 16;
pub const KEY_LEN: usize = 8;

/// Sixteen random bytes for one attempt.
pub fn challenge() -> [u8; CHALLENGE_LEN] {
    let mut buf = [0u8; CHALLENGE_LEN];
    rand::rng().fill_bytes(&mut buf);
    buf
}

/// The DES key VNC derives from a password.
pub fn key(password: &str) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    for (k, &b) in key.iter_mut().zip(password.as_bytes().iter().take(KEY_LEN)) {
        *k = b.reverse_bits();
    }
    key
}

/// What a client holding `password` sends back for `challenge`.
pub fn expected_response(password: &str, challenge: &[u8; CHALLENGE_LEN]) -> [u8; RESPONSE_LEN] {
    let cipher = Des::new_from_slice(&key(password)).expect("an 8-byte key is what DES takes");
    let mut response = *challenge;
    for block in response.as_chunks_mut::<8>().0 {
        cipher.encrypt_block(block.into());
    }
    response
}

/// Constant-time check of a client's response.
pub fn verify(password: &str, challenge: &[u8; CHALLENGE_LEN], response: &[u8; RESPONSE_LEN]) -> bool {
    let expected = expected_response(password, challenge);
    expected
        .iter()
        .zip(response)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_reversed_and_padded() {
        assert_eq!(key("a"), [0x86, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(key("123456789"), key("12345678"));
    }

    #[test]
    fn verify_accepts_the_right_password_only() {
        let ch = challenge();
        let ok = expected_response("secret", &ch);
        assert!(verify("secret", &ch, &ok));
        assert!(!verify("secre", &ch, &ok));
        assert!(!verify("", &ch, &ok));
        let mut bad = ok;
        bad[15] ^= 1;
        assert!(!verify("secret", &ch, &bad));
        // Only the first eight characters count, as in every other VNC.
        assert!(verify("secretsecret", &ch, &expected_response("secretse", &ch)));
    }

    #[test]
    fn two_blocks_in_ecb_mode() {
        // The response is two independent DES blocks: identical challenge
        // halves encrypt to identical halves, and the cipher is not a no-op.
        let ch = [0u8; 16];
        let r = expected_response("password", &ch);
        assert_eq!(r[..8], r[8..]);
        assert_ne!(r[..8], [0u8; 8]);
        assert_ne!(expected_response("passwore", &ch)[..8], r[..8]);
    }
}
