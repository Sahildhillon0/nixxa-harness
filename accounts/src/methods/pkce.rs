//! PKCE (RFC 7636) with the `S256` method: a random verifier and its SHA-256 challenge.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use zeroize::Zeroizing;

/// A fresh verifier: 32 random bytes, base64url without padding (43 characters, all from the
/// RFC's unreserved set). Zeroised when dropped.
pub(crate) fn verifier() -> Result<Zeroizing<String>, String> {
    let mut bytes = Zeroizing::new([0_u8; 32]);
    SystemRandom::new().fill(bytes.as_mut()).map_err(|_| "the system random generator failed".to_string())?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(bytes.as_ref())))
}

/// `BASE64URL(SHA256(verifier))`, without padding.
pub(crate) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636, Appendix B.
    #[test]
    fn matches_the_rfc_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_long_unreserved_and_different() {
        let a = verifier().unwrap();
        let b = verifier().unwrap();
        assert_eq!(a.len(), 43);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')), "{}", a.as_str());
        assert_ne!(a.as_str(), b.as_str());
        assert_eq!(challenge(&a).len(), 43);
    }
}
