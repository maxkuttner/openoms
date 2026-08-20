//! `oms init` — the first-run command.
//!
//! Prompts for the connection to the operator's Postgres, generates everything
//! else, writes `oms.toml`, then delegates to `database init`. The generated
//! values are never chosen by a human: a password someone invents on the spot is
//! the weakest part of an otherwise encrypted store.

use base64::Engine;
use rand::RngCore;
use std::io::{BufRead, Write};

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

/// What `oms init` asks for. Everything here describes the operator's existing
/// Postgres; nothing generated appears in this struct.
#[derive(Debug)]
pub struct Prompts {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
}

/// Prompt against arbitrary input, with the password read through a supplied
/// closure. Split this way so the whole flow is testable — `rpassword` reads the
/// tty directly and cannot be driven from a test.
pub fn prompt_with<R: BufRead>(
    input: &mut R,
    read_password: impl FnOnce() -> std::io::Result<String>,
) -> std::io::Result<Prompts> {
    fn ask<R: BufRead>(input: &mut R, label: &str, default: &str) -> std::io::Result<String> {
        print!("{label} [{default}]: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        input.read_line(&mut line)?;
        let trimmed = line.trim();
        Ok(if trimmed.is_empty() { default.to_string() } else { trimmed.to_string() })
    }

    let host = ask(input, "Postgres host     ", "localhost")?;
    // A typo here should cost one field, not the whole session.
    let port = ask(input, "Postgres port     ", "5432")?.parse().unwrap_or(5432);
    let database = ask(input, "Database name     ", "ods")?;
    let username = ask(input, "Superuser name    ", "postgres")?;
    let password = read_password()?;

    Ok(Prompts { host, port, database, username, password })
}

/// The real thing: stdin plus a no-echo password read.
pub fn prompt() -> std::io::Result<Prompts> {
    let stdin = std::io::stdin();
    let mut locked = stdin.lock();
    prompt_with(&mut locked, || rpassword::prompt_password("Superuser password: "))
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

    use std::io::BufReader;

    /// Pressing Enter through every prompt must yield a working localhost setup —
    /// that is what makes the happy path zero-decision.
    #[test]
    fn empty_input_takes_every_default() {
        let mut input = BufReader::new(&b"\n\n\n\n"[..]);
        let p = prompt_with(&mut input, || Ok("secret".into())).expect("prompt");
        assert_eq!(p.host, "localhost");
        assert_eq!(p.port, 5432);
        assert_eq!(p.database, "ods");
        assert_eq!(p.username, "postgres");
        assert_eq!(p.password, "secret");
    }

    #[test]
    fn typed_values_override_the_defaults() {
        let mut input = BufReader::new(&b"db.internal\n6543\ntrading\nadmin\n"[..]);
        let p = prompt_with(&mut input, || Ok("pw".into())).expect("prompt");
        assert_eq!(p.host, "db.internal");
        assert_eq!(p.port, 6543);
        assert_eq!(p.database, "trading");
        assert_eq!(p.username, "admin");
    }

    /// Surrounding whitespace is a paste artefact, not part of a hostname.
    #[test]
    fn trims_surrounding_whitespace() {
        let mut input = BufReader::new(&b"  db.internal  \n\n\n\n"[..]);
        let p = prompt_with(&mut input, || Ok("pw".into())).expect("prompt");
        assert_eq!(p.host, "db.internal");
    }

    /// A non-numeric port falls back to the default rather than aborting the whole
    /// interactive session over one typo.
    #[test]
    fn unparseable_port_falls_back_to_the_default() {
        let mut input = BufReader::new(&b"\nnot-a-number\n\n\n"[..]);
        let p = prompt_with(&mut input, || Ok("pw".into())).expect("prompt");
        assert_eq!(p.port, 5432);
    }
}
