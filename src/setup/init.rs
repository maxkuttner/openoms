//! `oms init` — the first-run command.
//!
//! Prompts for the connection to the operator's Postgres, generates everything
//! else, writes `oms.toml`, then delegates to `database init`. The generated
//! values are never chosen by a human: a password someone invents on the spot is
//! the weakest part of an otherwise encrypted store.

use base64::Engine;
use rand::RngCore;

/// 32 bytes — the AES-256 key size the credential store expects.
pub fn generate_master_key() -> String {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    format!("base64:{}", base64::engine::general_purpose::STANDARD.encode(raw))
}

/// A 24-character password from a URL-safe alphabet. It ends up inside a
/// `postgres://` URL and inside a TOML string, so restricting the alphabet keeps
/// both boring — no escaping, no percent-encoding surprises.
pub fn generate_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut raw = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// 32 bytes is the AES-256 key size the credential store will expect. Getting
    /// this wrong would surface much later, as a decryption failure.
    #[test]
    fn master_key_is_32_bytes_base64_prefixed() {
        let k = generate_master_key();
        let body = k.strip_prefix("base64:").expect("base64: prefix");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(body)
            .expect("valid base64");
        assert_eq!(raw.len(), 32, "master key must be 32 bytes, got {}", raw.len());
    }

    /// Two calls must not collide — the whole point is that these are not chosen
    /// by a human and not reused across installs.
    #[test]
    fn generated_values_are_unique() {
        assert_ne!(generate_master_key(), generate_master_key());
        assert_ne!(generate_password(), generate_password());
    }

    /// The password lands in a URL (`postgres://oms:<pw>@…`) and in a TOML string.
    /// Restricting the alphabet keeps both cases boring.
    #[test]
    fn password_is_url_safe_and_long_enough() {
        let p = generate_password();
        assert_eq!(p.len(), 24);
        assert!(
            p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "unexpected character in {p}"
        );
    }
}
