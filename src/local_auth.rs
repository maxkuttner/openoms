//! Local credential primitives: Argon2id password hashing, the passphrase
//! policy, TOTP enrollment/verification and one-time recovery codes.
//!
//! Pure library code — no routes, no database. Callers persist what these
//! functions return (PHC strings, sealed TOTP secrets, recovery-code hashes,
//! the last accepted TOTP step).
//!
//! The common-password list (`local_auth_common_passwords.txt`) is SecLists'
//! `Passwords/Common-Credentials/10k-most-common.txt`
//! (<https://github.com/danielmiessler/SecLists>), MIT licence, vendored
//! verbatim.

use std::collections::HashSet;
use std::sync::LazyLock;

use argon2::{Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use totp_rs::{Algorithm, Secret, Totp};
use zeroize::Zeroizing;

use crate::secrets::{self, MasterKey};

/// NIST SP 800-63B: single-factor passphrases need at least 15 characters.
pub const MIN_PASSWORD_CHARS: usize = 15;
/// Generous upper bound (NIST: permit 64+) that still caps hashing input.
pub const MAX_PASSWORD_CHARS: usize = 128;

/// Concurrent Argon2 computations. Each costs ~19 MiB and tens of ms of CPU;
/// bounding them keeps a login flood from exhausting memory or the blocking pool.
const HASH_CONCURRENCY: usize = 4;
static HASH_PERMITS: Semaphore = Semaphore::const_new(HASH_CONCURRENCY);

const COMMON_PASSWORDS_TXT: &str = include_str!("local_auth_common_passwords.txt");
static COMMON_PASSWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    COMMON_PASSWORDS_TXT.lines().map(str::trim).filter(|l| !l.is_empty()).collect()
});

/// AAD binding a sealed TOTP secret to its purpose, so a blob sealed for
/// something else (e.g. a broker credential) cannot be swapped in.
pub(crate) const TOTP_AAD: &str = "oms:local_auth:totp_secret";
const TOTP_DIGITS: u8 = 6;
const TOTP_STEP_SECS: u64 = 30;
const TOTP_SKEW_STEPS: u16 = 1;

const RECOVERY_CODE_COUNT: usize = 10;
const RECOVERY_CODE_BYTES: usize = 16; // 128 bits
const RECOVERY_ALPHABET: base32::Alphabet = base32::Alphabet::Rfc4648 { padding: false };

#[derive(Debug)]
pub enum LocalAuthError {
    /// The hashing library failed (should not happen with valid parameters).
    Hash(String),
    /// The blocking task running the hash panicked or was cancelled.
    Join,
}

impl std::fmt::Display for LocalAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalAuthError::Hash(m) => write!(f, "password hashing failed: {m}"),
            LocalAuthError::Join => f.write_str("password hashing task did not complete"),
        }
    }
}

impl std::error::Error for LocalAuthError {}

#[derive(Debug, PartialEq)]
pub enum TotpError {
    /// The sealed secret could not be opened or is not a usable TOTP secret.
    BadSecret,
    /// Wrong code, or outside the ±1 step window.
    Invalid,
    /// The code's step was already accepted (or is older than one that was).
    Replayed,
    /// Issuer or account name is not a valid otpauth label (e.g. contains `:`).
    InvalidLabel,
}

impl std::fmt::Display for TotpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TotpError::BadSecret => f.write_str("stored TOTP secret is unusable"),
            TotpError::Invalid => f.write_str("invalid TOTP code"),
            TotpError::Replayed => f.write_str("TOTP code already used"),
            TotpError::InvalidLabel => f.write_str("issuer or account name cannot be used in a TOTP label"),
        }
    }
}

impl std::error::Error for TotpError {}

/// Argon2id v0x13, m = 19456 KiB, t = 2, p = 1 (OWASP floor), stated explicitly
/// rather than inherited from crate defaults.
fn argon2() -> Result<Argon2<'static>, LocalAuthError> {
    let params = Params::new(19_456, 2, 1, None).map_err(|e| LocalAuthError::Hash(e.to_string()))?;
    Ok(Argon2::new(argon2::Algorithm::Argon2id, Version::V0x13, params))
}

/// Run `job` on the blocking pool under a permit of `permits`. The permit moves
/// into the blocking closure, so it is held until the job itself finishes —
/// dropping the returned future (client disconnect) does not free a slot while
/// the Argon2 work is still running. `None` if the job panicked.
async fn run_bounded<T, F>(permits: &'static Semaphore, job: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = permits.acquire().await.ok()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        job()
    })
    .await
    .ok()
}

/// Hash a password into an Argon2id PHC string with a fresh random salt.
pub async fn hash_password(pw: &str) -> Result<String, LocalAuthError> {
    let pw = Zeroizing::new(pw.to_owned());
    run_bounded(&HASH_PERMITS, move || {
        argon2()?
            .hash_password(pw.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| LocalAuthError::Hash(e.to_string()))
    })
    .await
    .ok_or(LocalAuthError::Join)?
}

/// Check a password against a stored PHC string. The library compares in
/// constant time; any malformed hash is simply a mismatch.
pub async fn verify_password(hash: &str, pw: &str) -> bool {
    let (hash, pw) = (hash.to_owned(), Zeroizing::new(pw.to_owned()));
    run_bounded(&HASH_PERMITS, move || {
        argon2().is_ok_and(|a| a.verify_password(pw.as_bytes(), hash.as_str()).is_ok())
    })
    .await
    .unwrap_or(false)
}

/// NIST-style passphrase policy: length bounds and a common-password
/// blocklist, no composition rules.
pub fn check_password_policy(pw: &str) -> Result<(), &'static str> {
    let chars = pw.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err("password must be at least 15 characters");
    }
    if chars > MAX_PASSWORD_CHARS {
        return Err("password must be at most 128 characters");
    }
    if COMMON_PASSWORDS.contains(pw) || COMMON_PASSWORDS.contains(pw.to_lowercase().as_str()) {
        return Err("password is too common");
    }
    Ok(())
}

/// What enrollment produces: the sealed secret to store and the `otpauth://`
/// URI to show (as QR) once, in an already-authenticated context.
pub struct TotpEnrollment {
    pub secret_enc: Vec<u8>,
    pub otpauth_uri: String,
}

// The URI embeds the secret in clear.
impl std::fmt::Debug for TotpEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TotpEnrollment(<redacted>)")
    }
}

fn totp_from_secret(secret: &[u8], issuer: Option<&str>, account: &str) -> Totp {
    // build_noncompliant: the issuer/account checks only matter for the URI,
    // which `new_totp` builds through the checked path.
    totp_rs::Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(TOTP_DIGITS)
        .with_step_duration(TOTP_STEP_SECS)
        .with_skew(TOTP_SKEW_STEPS)
        .with_secret(secret)
        .with_issuer(issuer)
        .with_account_name(account)
        .build_noncompliant()
}

/// Generate a fresh 160-bit TOTP secret (RFC 6238, SHA-1, 6 digits, 30 s).
///
/// `InvalidLabel` if `issuer` or `account` is not a valid otpauth label (a `:`
/// is forbidden, and is legal in a quoted email local part).
pub fn new_totp(issuer: &str, account: &str, master: &MasterKey) -> Result<TotpEnrollment, TotpError> {
    let secret = Secret::generate();
    let totp = totp_rs::Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(TOTP_DIGITS)
        .with_step_duration(TOTP_STEP_SECS)
        .with_skew(TOTP_SKEW_STEPS)
        .with_secret(secret.as_ref())
        .with_issuer(Some(issuer))
        .with_account_name(account)
        .build()
        .map_err(|_| TotpError::InvalidLabel)?;
    let otpauth_uri = totp.to_url().map_err(|_| TotpError::InvalidLabel)?;
    Ok(TotpEnrollment { secret_enc: secrets::seal(master, TOTP_AAD, secret.as_ref()), otpauth_uri })
}

/// Verify a TOTP code. Accepts the current step ±1; rejects any step at or
/// before `last_step` (replay). Returns the matched step, which the caller
/// must persist as the new `last_step`.
pub fn verify_totp(
    secret_enc: &[u8],
    master: &MasterKey,
    code: &str,
    last_step: Option<i64>,
    now: i64,
) -> Result<i64, TotpError> {
    let secret = Zeroizing::new(secrets::open(master, TOTP_AAD, secret_enc).map_err(|_| TotpError::BadSecret)?);
    let now = u64::try_from(now).map_err(|_| TotpError::Invalid)?;
    let totp = totp_from_secret(&secret, None, "");
    let step = totp.check(code.trim(), now).ok_or(TotpError::Invalid)?;
    let step = i64::try_from(step).map_err(|_| TotpError::Invalid)?;
    if last_step.is_some_and(|last| step <= last) {
        return Err(TotpError::Replayed);
    }
    Ok(step)
}

/// The code a sealed secret produces at `unix` (tests only).
#[cfg(test)]
pub(crate) fn code_at(secret_enc: &[u8], master: &MasterKey, unix: u64) -> String {
    let secret = secrets::open(master, TOTP_AAD, secret_enc).expect("opens");
    totp_from_secret(&secret, None, "").generate(unix).to_string()
}

/// Canonical form a code is hashed in: uppercase base32, no separators.
fn normalize_recovery_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn hash_recovery_code(code: &str) -> String {
    let digest = Sha256::digest(normalize_recovery_code(code).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ten one-time recovery codes. Returns `(shown_once, sha256_hex_hashes)`;
/// only the hashes are stored. Each code is 128 random bits, base32 (26
/// chars), grouped `XXXXX-XXXXX-XXXXX-XXXXX-XXXXXX` for transcription. A
/// plain SHA-256 suffices: with 128 bits of entropy there is nothing to
/// brute-force, unlike a password.
pub fn new_recovery_codes() -> (Vec<String>, Vec<String>) {
    let mut rng = rand::thread_rng();
    (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            let mut bytes = [0u8; RECOVERY_CODE_BYTES];
            rng.fill_bytes(&mut bytes);
            let raw = base32::encode(RECOVERY_ALPHABET, &bytes);
            let code = format!("{}-{}-{}-{}-{}", &raw[0..5], &raw[5..10], &raw[10..15], &raw[15..20], &raw[20..]);
            let hash = hash_recovery_code(&code);
            (code, hash)
        })
        .unzip()
}

/// Check a user-entered recovery code (case and separators ignored) against
/// one stored hash. Constant-time comparison.
pub fn verify_recovery_code(code: &str, hash: &str) -> bool {
    hash_recovery_code(code).as_bytes().ct_eq(hash.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        secrets::parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("key")
    }

    /// Recover the clear secret through the same path verification uses, to
    /// compute expected codes.
    fn clear_totp(e: &TotpEnrollment) -> Totp {
        let secret = secrets::open(&key(), TOTP_AAD, &e.secret_enc).expect("opens");
        totp_from_secret(&secret, None, "")
    }

    #[tokio::test]
    async fn argon2_hash_verifies_and_rejects_wrong() {
        let pw = "correct horse battery staple";
        let h = hash_password(pw).await.expect("hash");
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(verify_password(&h, pw).await);
        assert!(!verify_password(&h, "correct horse battery stapler").await);
        assert!(!verify_password("not a phc string", pw).await);
        // Unique salt per credential.
        assert_ne!(h, hash_password(pw).await.expect("hash"));
    }

    #[tokio::test]
    async fn bounded_job_keeps_permit_after_caller_is_dropped() {
        static PERMITS: Semaphore = Semaphore::const_new(4);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let caller = tokio::spawn(run_bounded(&PERMITS, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            done_tx.send(()).unwrap();
        }));
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap()).await.unwrap();
        caller.abort(); // client disconnect: the outer future is dropped mid-flight
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(PERMITS.available_permits(), 3, "permit must stay held while the job runs");
        release_tx.send(()).unwrap();
        tokio::task::spawn_blocking(move || done_rx.recv().unwrap()).await.unwrap();
        for _ in 0..100 {
            if PERMITS.available_permits() == 4 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("permit not released after the job finished");
    }

    #[test]
    fn new_totp_rejects_colon_label_without_panic() {
        assert_eq!(new_totp("OpenOMS", "\"a:b\"@example.com", &key()).unwrap_err(), TotpError::InvalidLabel);
        assert_eq!(new_totp("Open:OMS", "alice", &key()).unwrap_err(), TotpError::InvalidLabel);
    }

    #[test]
    fn password_policy_length_and_common() {
        assert!(check_password_policy("fourteen chars").is_err()); // 14
        assert!(check_password_policy("fifteen chars!!").is_ok()); // 15
        assert!(check_password_policy(&"ä".repeat(15)).is_ok()); // counts chars, not bytes
        assert!(check_password_policy(&"a".repeat(128)).is_ok());
        assert!(check_password_policy(&"a".repeat(129)).is_err());
        // The one ≥15-char entry in the vendored list.
        assert_eq!(check_password_policy("films+pic+galeries"), Err("password is too common"));
        assert_eq!(check_password_policy("FILMS+PIC+GALERIES"), Err("password is too common"));
        assert!(COMMON_PASSWORDS.len() >= 9_990, "list loaded: {}", COMMON_PASSWORDS.len());
        // No composition rules: all-lowercase letters pass.
        assert!(check_password_policy("thequickbrownfoxjumps").is_ok());
    }

    #[test]
    fn totp_accepts_current_and_adjacent_step() {
        let e = new_totp("OpenOMS", "alice@example.com", &key()).expect("enroll");
        assert!(e.otpauth_uri.starts_with("otpauth://totp/OpenOMS:alice%40example.com?"), "{}", e.otpauth_uri);
        let t = clear_totp(&e);
        let now: i64 = 1_800_000_015; // mid-step
        let cur = now / 30;
        for (offset, expect) in [(-1i64, true), (0, true), (1, true), (-2, false), (2, false)] {
            let code = t.generate(((cur + offset) * 30) as u64).to_string();
            let got = verify_totp(&e.secret_enc, &key(), &code, None, now);
            if expect {
                assert_eq!(got, Ok(cur + offset), "offset {offset}");
            } else {
                assert_eq!(got, Err(TotpError::Invalid), "offset {offset}");
            }
        }
        assert_eq!(verify_totp(&e.secret_enc, &key(), "abcdef", None, now), Err(TotpError::Invalid));
    }

    #[test]
    fn totp_rejects_replayed_step() {
        let e = new_totp("OpenOMS", "bob", &key()).expect("enroll");
        let t = clear_totp(&e);
        let now: i64 = 1_800_000_015;
        let code = t.generate(now as u64).to_string();
        let step = verify_totp(&e.secret_enc, &key(), &code, None, now).expect("first use");
        assert_eq!(verify_totp(&e.secret_enc, &key(), &code, Some(step), now), Err(TotpError::Replayed));
        // An older (still in-window) step after a newer one was accepted.
        let prev = t.generate((now - 30) as u64).to_string();
        assert_eq!(verify_totp(&e.secret_enc, &key(), &prev, Some(step), now), Err(TotpError::Replayed));
        // The next step is fine.
        let next = t.generate((now + 30) as u64).to_string();
        assert_eq!(verify_totp(&e.secret_enc, &key(), &next, Some(step), now + 30), Ok(step + 1));
    }

    #[test]
    fn totp_secret_is_encrypted_at_rest() {
        let e = new_totp("OpenOMS", "carol", &key()).expect("enroll");
        let t = clear_totp(&e);
        let b32 = t.secret().to_base32();
        assert!(e.otpauth_uri.contains(&b32));
        let raw = t.secret().as_ref().to_vec();
        assert!(!e.secret_enc.windows(b32.len()).any(|w| w == b32.as_bytes()));
        assert!(!e.secret_enc.windows(raw.len()).any(|w| w == raw.as_slice()));
        // Wrong key cannot open it.
        let other = secrets::parse_master_key("base64:q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s=").expect("key");
        assert_eq!(verify_totp(&e.secret_enc, &other, "000000", None, 1_800_000_015), Err(TotpError::BadSecret));
        assert_eq!(format!("{e:?}"), "TotpEnrollment(<redacted>)");
    }

    #[test]
    fn recovery_codes_unique_and_hashed() {
        let (codes, hashes) = new_recovery_codes();
        assert_eq!(codes.len(), 10);
        assert_eq!(hashes.len(), 10);
        assert_eq!(codes.iter().collect::<HashSet<_>>().len(), 10);
        for (c, h) in codes.iter().zip(&hashes) {
            assert_eq!(c.len(), 26 + 4, "{c}");
            assert_eq!(normalize_recovery_code(c).len(), 26);
            assert_eq!(h.len(), 64);
            assert!(!h.contains(&normalize_recovery_code(c)));
            assert!(verify_recovery_code(c, h));
            assert!(verify_recovery_code(&c.to_lowercase().replace('-', " "), h));
        }
        assert!(!verify_recovery_code(&codes[0], &hashes[1]));
        assert!(!verify_recovery_code("AAAAA-AAAAA-AAAAA-AAAAA-AAAAAA", &hashes[0]));
    }
}
