//! `oms init` — the first-run command.
//!
//! Prompts for the connection to the operator's Postgres, generates everything
//! else, writes `oms.toml`, then delegates to `database init`. The generated
//! values are never chosen by a human: a password someone invents on the spot is
//! the weakest part of an otherwise encrypted store.

use base64::Engine;
use rand::RngCore;
use std::io::{BufRead, Write};

use crate::config::{self, FileConfig};

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
pub struct Prompts {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
}

// Hand-written so a stray `{:?}` — in a log line, a panic message, an
// `expect` on a Result<Prompts, _> — cannot print the superuser password.
// That credential can drop the database.
impl std::fmt::Debug for Prompts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompts")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
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
        let n = input.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("unexpected end of input while reading {label} — use `oms init --non-interactive` for scripted setup"),
            ));
        }
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

/// Assemble the file from the prompts plus freshly generated secrets. Returns the
/// config and the generated role password, which the caller also needs to pass to
/// `database init`.
///
/// The superuser password is deliberately absent: it was used to connect, and
/// that is all. Persisting a credential that can `DROP DATABASE` is not worth
/// saving one prompt on the rare `migrate`.
///
/// The cockpit admin password is deliberately NOT generated here: `run()` needs
/// the raw value for its success message, and generating it there — then
/// assigning it into the config — is one step, versus generating it here and
/// making the caller claw it back out of an `Option` afterwards.
pub fn build_file_config(p: &Prompts) -> (FileConfig, String) {
    let role_password = generate_password();
    let mut cfg = FileConfig::default();
    cfg.database.host = Some(p.host.clone());
    cfg.database.port = Some(p.port);
    cfg.database.username = Some(p.username.clone());
    cfg.database.database = Some(p.database.clone());
    cfg.oms.password = Some(role_password.clone());
    cfg.oms.master_key = Some(generate_master_key());
    cfg.server.bind_addr = Some("localhost:3001".to_string());
    (cfg, role_password)
}

/// The refusal shown when `oms init` finds a config already in place. Shared by
/// `run()`'s own check and by the dispatch arm in `main.rs`, which checks the
/// same thing earlier — before a superuser password is typed at a no-echo prompt,
/// or `config::load()` has a chance to exit the process over a malformed file
/// before this message ever gets printed.
pub fn already_initialized_message(path: &std::path::Path) -> String {
    format!(
        "error: {} already exists\n\
         \x20        This machine is already initialized. Use `oms database migrate`\n\
         \x20        to upgrade it, or delete the file to start over — but note the\n\
         \x20        master key in it decrypts your stored credentials.",
        path.display()
    )
}

/// The whole `oms init` flow.
///
/// `interactive` controls only whether the generated cockpit password is printed:
/// true (a human typed the prompts) is the sole way that value ever reaches the
/// operator, so it must be shown; false (`--non-interactive`) is the mode most
/// likely to run under CI/Ansible with stdout captured into a durable log, which
/// is exactly what "secrets are never logged" rules out.
pub async fn run(prompts: Prompts, interactive: bool) -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = config::path();
    if cfg_path.exists() {
        return Err(already_initialized_message(&cfg_path).into());
    }

    // Test connectivity before writing anything: a wrong password should cost
    // nothing, not leave a half-made install behind. `PgConnection` (a single
    // connection, explicitly closed) rather than `PgPool` matches the idiom
    // `provision::inspect`/`provision::provision` already use for this same
    // one-shot probe against the `postgres` maintenance database.
    use sqlx::Connection;
    let probe = crate::setup::database::config::PostgresConfig {
        host: prompts.host.clone(),
        port: prompts.port,
        username: prompts.username.clone(),
        password: prompts.password.clone(),
        database: "postgres".to_string(),
    };
    let conn = sqlx::PgConnection::connect(&probe.url_for("postgres"))
        .await
        .map_err(|e| format!("error: could not connect to {}:{} — {e}", prompts.host, prompts.port))?;
    conn.close().await.ok();
    println!("  connected to {}:{}", prompts.host, prompts.port);

    let (mut file_cfg, role_password) = build_file_config(&prompts);
    let admin_password = generate_password();
    file_cfg.server.admin_password = Some(admin_password.clone());

    // Written before provisioning: if provisioning fails halfway, the generated
    // secrets survive and `oms database init` can finish the job.
    config::write_new(&cfg_path, &file_cfg)?;
    println!("  wrote {} (mode 0600)", cfg_path.display());
    ensure_gitignored(&cfg_path);

    let overrides = crate::setup::database::config::PostgresOverrides {
        host: Some(prompts.host.clone()),
        port: Some(prompts.port),
        username: Some(prompts.username.clone()),
        password: Some(prompts.password.clone()),
        database: Some(prompts.database.clone()),
    };
    // The write above already happened: a failure here must not read as if
    // nothing was saved. That guarantee is the entire reason for the
    // write-before-provision ordering, so it has to reach the operator, not
    // just live in a comment.
    crate::setup::database::init(overrides, Some(role_password)).await.map_err(|e| {
        format!(
            "error: the database could not be created: {e}\n\n\
             \x20        {} was already written and holds your generated credentials —\n\
             \x20        it has not been lost. Fix the cause above, then finish with:\n\n\
             \x20            oms database init",
            cfg_path.display()
        )
    })?;

    let admin_line = if interactive {
        format!("Cockpit login password: {admin_password}")
    } else {
        "Cockpit login password: written to oms.toml".to_string()
    };
    println!(
        "\nBack up {}. The master key in it is the only thing that can\n\
         decrypt stored broker credentials — lose it and they are gone.\n\n\
         {admin_line}\n\n\
         Start the server:  oms",
        cfg_path.display()
    );
    Ok(())
}

/// Append the config filename to `.gitignore` if it is not already covered.
/// Best-effort: not being in a git repository is normal and not an error.
fn ensure_gitignored(cfg_path: &std::path::Path) {
    let Some(name) = cfg_path.file_name().and_then(|n| n.to_str()) else { return };
    let gitignore = std::path::Path::new(".gitignore");
    if !gitignore.exists() {
        return;
    }
    let current = std::fs::read_to_string(gitignore).unwrap_or_default();
    if current.lines().any(|l| l.trim() == name) {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(gitignore) {
        use std::io::Write;
        let _ = writeln!(f, "\n# openoms bootstrap config — holds the master key\n{name}");
        println!("  added {name} to .gitignore");
    }
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

    /// Debug output must not print the superuser password. A stray `{:?}` in a log
    /// line, panic message, or `expect` on a Result<Prompts, _> cannot reveal a
    /// credential that can drop the database.
    #[test]
    fn debug_output_redacts_password() {
        let p = Prompts {
            host: "db.example.com".into(),
            port: 5432,
            database: "mydb".into(),
            username: "superuser".into(),
            password: "super-secret-password-12345".into(),
        };
        let rendered = format!("{p:?}");
        assert!(!rendered.contains("super-secret-password-12345"), "password leaked into {rendered}");
        // Verify other fields are present
        assert!(rendered.contains("db.example.com"));
        assert!(rendered.contains("5432"));
        assert!(rendered.contains("mydb"));
        assert!(rendered.contains("superuser"));
    }

    /// EOF (premature stdin closure) must be treated as an error, not as pressing
    /// Enter. A script with closed stdin must not silently walk through all
    /// prompts taking defaults — that would provision a database without asking.
    #[test]
    fn eof_before_all_prompts_returns_error() {
        let mut input = BufReader::new(&b"\n\n"[..]);
        let err = prompt_with(&mut input, || Ok("pw".into())).expect_err("should be an error");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("unexpected end of input"));
    }

    /// The file we hand to `database init` must contain the generated secrets and
    /// the prompted connection — and must NOT contain the superuser password,
    /// which is the credential that can drop the database.
    #[test]
    fn builds_a_file_config_without_the_superuser_password() {
        let p = Prompts {
            host: "db.internal".into(),
            port: 6543,
            database: "trading".into(),
            username: "admin".into(),
            password: "super-secret".into(),
        };
        let (cfg, role_pw) = build_file_config(&p);

        assert_eq!(cfg.database.host.as_deref(), Some("db.internal"));
        assert_eq!(cfg.database.port, Some(6543));
        assert_eq!(cfg.database.username.as_deref(), Some("admin"));
        assert_eq!(cfg.database.database.as_deref(), Some("trading"));

        assert_eq!(cfg.oms.password.as_deref(), Some(role_pw.as_str()));
        assert!(cfg.oms.master_key.as_deref().unwrap().starts_with("base64:"));
        assert_eq!(cfg.server.bind_addr.as_deref(), Some("localhost:3001"));

        let rendered = toml::to_string_pretty(&cfg).expect("render");
        assert!(
            !rendered.contains("super-secret"),
            "superuser password must never reach the file:\n{rendered}"
        );
    }
}
