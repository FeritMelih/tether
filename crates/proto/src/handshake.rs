//! The handshake's arithmetic: nonces, and the proofs each side computes to show it holds the
//! token without sending it. The message flow itself is the host's and the client's.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const NONCE_BYTES: usize = 32;
pub const HOST_LABEL: &[u8] = b"tether host";
pub const CLIENT_LABEL: &[u8] = b"tether client";

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the operating system's random source");
    b
}

pub fn nonce() -> [u8; NONCE_BYTES] {
    random_bytes()
}

/// A fresh token: 32 random bytes as hex.
pub fn new_token() -> String {
    hex(&random_bytes::<32>())
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}

/// `HMAC-SHA256(token, label || client_nonce || host_nonce)`, keyed with the token's hex text.
pub fn proof(token: &str, label: &[u8], client_nonce: &[u8], host_nonce: &[u8]) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(label);
    mac.update(client_nonce);
    mac.update(host_nonce);
    mac.finalize().into_bytes().into()
}

/// Checks a proof in constant time.
pub fn verify(
    token: &str,
    label: &[u8],
    client_nonce: &[u8],
    host_nonce: &[u8],
    given: &[u8],
) -> bool {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(label);
    mac.update(client_nonce);
    mac.update(host_nonce);
    mac.verify_slice(given).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vector `spec/fixtures/handshake.json` carries, which the TypeScript SDK checks too.
    #[test]
    fn proofs_match_the_fixture() {
        let token = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let nc = unhex("0101010101010101010101010101010101010101010101010101010101010101").unwrap();
        let nh = unhex("0202020202020202020202020202020202020202020202020202020202020202").unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../spec/fixtures/handshake.json")).unwrap();
        assert_eq!(fixture["token"], token);
        assert_eq!(
            hex(&proof(token, HOST_LABEL, &nc, &nh)),
            fixture["hostProof"].as_str().unwrap()
        );
        assert_eq!(
            hex(&proof(token, CLIENT_LABEL, &nc, &nh)),
            fixture["clientProof"].as_str().unwrap()
        );
        assert!(verify(
            token,
            HOST_LABEL,
            &nc,
            &nh,
            &proof(token, HOST_LABEL, &nc, &nh)
        ));
        assert!(!verify(
            token,
            HOST_LABEL,
            &nc,
            &nh,
            &proof(token, CLIENT_LABEL, &nc, &nh)
        ));
        assert!(!verify(
            "other",
            HOST_LABEL,
            &nc,
            &nh,
            &proof(token, HOST_LABEL, &nc, &nh)
        ));
    }

    #[test]
    fn hex_round_trips() {
        let b = random_bytes::<17>();
        assert_eq!(unhex(&hex(&b)).unwrap(), b);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("abc"), None);
        assert_eq!(new_token().len(), 64);
    }
}
