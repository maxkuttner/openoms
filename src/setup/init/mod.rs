//! `oms init` — the first-run command.
//!
//! Prompts for the connection to the operator's Postgres, generates everything
//! else, writes `oms.toml`, then delegates to `database init`. The generated
//! values are never chosen by a human: a password someone invents on the spot is
//! the weakest part of an otherwise encrypted store.

use base64::Engine;
use rand::RngCore;


use crate::config::{self, FileConfig};

pub mod docker;
pub mod prompts;
pub mod rules;

use prompts::{AdminPassword, Answers, DbChoice};
use rules::DbSource;

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
pub fn build_file_config(p: &Prompts, bind: &str) -> (FileConfig, String) {
    let role_password = generate_password();
    let mut cfg = FileConfig::default();
    cfg.database.host = Some(p.host.clone());
    cfg.database.port = Some(p.port);
    cfg.database.username = Some(p.username.clone());
    cfg.database.database = Some(p.database.clone());
    cfg.oms.password = Some(role_password.clone());
    cfg.oms.master_key = Some(generate_master_key());
    cfg.server.bind_addr = Some(bind.to_string());
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

/// Flag and environment values for `oms init`; in non-interactive mode they are
/// the whole answer, interactively they skip the matching prompt.
pub struct InitFlags {
    pub source: Option<DbSource>,
    pub admin_password: Option<String>,
    pub bind: Option<String>,
}

pub const DEFAULT_BIND: &str = "localhost:3001";

/// What skips the interactive superuser-password prompt. On a host only an
/// explicit `--password` does: `POSTGRES_PASSWORD` describes the database the
/// application connects to, not the one being provisioned. In the container it
/// is the provisioning password: compose passes the one it gave Postgres.
pub fn superuser_password_default(flag: Option<String>, env: Option<String>, container: bool) -> Option<String> {
    flag.or(if container { env } else { None })
}

/// Non-interactive answers: `--postgres` defaults to `existing` (what scripts
/// written before the flag existed expect), an absent admin password is
/// generated, an absent bind is `localhost:3001`. Values are checked by the same
/// rules the prompts use, so a script cannot write what a human could not type.
pub fn answers_from_flags(f: &InitFlags, db: Prompts) -> Result<Answers, String> {
    let db = match f.source.unwrap_or(DbSource::Existing) {
        DbSource::Docker => DbChoice::Docker,
        DbSource::Existing => DbChoice::Existing(db),
    };
    let admin = match &f.admin_password {
        Some(pw) => {
            rules::check_admin_password(pw)?;
            AdminPassword::Typed(pw.clone())
        }
        None => AdminPassword::Generated(generate_password()),
    };
    let bind = rules::parse_bind(f.bind.as_deref().unwrap_or(DEFAULT_BIND))?;
    Ok(Answers { db, admin, bind })
}

/// What to do when provisioning fails after `oms.toml` was written. For a
/// Docker database the superuser password was generated and never shown, so
/// the resume line fetches it from the container.
pub fn provision_failure_message(cfg_path: &std::path::Path, err: &str, docker: bool) -> String {
    let resume = if docker {
        format!(
            "POSTGRES_PASSWORD=\"$(docker exec {c} printenv POSTGRES_PASSWORD)\" oms database init --resume",
            c = docker::CONTAINER
        )
    } else {
        "oms database init --resume".to_string()
    };
    let mut m = format!(
        "error: the database could not be created: {err}\n\n\
         \x20        {} was already written and holds your generated credentials —\n\
         \x20        it has not been lost. Fix the cause above, then finish with:\n\n\
         \x20            {resume}",
        cfg_path.display()
    );
    if docker {
        m.push_str(&format!(
            "\n\n\x20        Or start over from nothing:\n\n\
             \x20            docker rm -f {c} && docker volume rm {v} && rm {p}",
            c = docker::CONTAINER,
            v = docker::VOLUME,
            p = cfg_path.display()
        ));
    }
    m
}

pub fn docker_cleanup_hint() -> String {
    format!(
        "The {c} container may still exist. To start over:\n\
         \x20     docker rm -f {c} && docker volume rm {v}",
        c = docker::CONTAINER,
        v = docker::VOLUME
    )
}

/// The whole `oms init` flow.
///
/// `interactive` controls only whether the generated cockpit password is printed:
/// true (a human typed the prompts) is the sole way that value ever reaches the
/// operator, so it must be shown; false (`--non-interactive`) is the mode most
/// likely to run under CI/Ansible with stdout captured into a durable log, which
/// is exactly what "secrets are never logged" rules out.
pub async fn run(
    answers: Answers,
    docker: &dyn docker::DockerOps,
    interactive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_timeout(
        answers,
        docker,
        interactive,
        docker::READY_TIMEOUT,
        config::path(),
        config::init_target(),
    )
    .await
}

/// `run`, with the readiness timeout and both config paths passed in so tests
/// never touch `OMS_CONFIG` or the working directory's `oms.toml`.
async fn run_with_timeout(
    answers: Answers,
    docker: &dyn docker::DockerOps,
    interactive: bool,
    ready: std::time::Duration,
    existing_cfg: std::path::PathBuf,
    target_cfg: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // Refuse over a config found in ANY searched location, not just the one
    // this run would write: a second file elsewhere would be silently shadowed
    // by (or shadow) the first, with a different master key in each.
    if existing_cfg.exists() {
        return Err(already_initialized_message(&config::absolute(existing_cfg)).into());
    }
    let cfg_path = config::absolute(target_cfg);

    let Answers { db, admin, bind } = answers;
    let started_docker = matches!(db, DbChoice::Docker);
    let prompts = match db {
        DbChoice::Existing(p) => p,
        DbChoice::Docker => {
            docker::preflight(docker).map_err(|e| format!("error: {e}"))?;
            println!("  {}", docker::starting_line());
            // A failed `docker run` can still have created the container (it is
            // created before the port is published), so say how to remove it.
            let (p, note) = docker::start(docker)
                .map_err(|e| format!("error: {e}\n\n{}", docker_cleanup_hint()))?;
            if let Some(note) = note {
                println!("  {note}");
            }
            docker::wait_ready(&p, ready)
                .await
                .map_err(|e| format!("error: {e}\n\n{}", docker_cleanup_hint()))?;
            println!("  {} is ready on 127.0.0.1:{}", docker::CONTAINER, p.port);
            p
        }
    };
    // Every failure from here on, after a container was started, also says how
    // to remove it: its superuser password was never shown to anyone.
    let cleanup = |e: String| -> Box<dyn std::error::Error> {
        if started_docker { format!("{e}\n\n{}", docker_cleanup_hint()).into() } else { e.into() }
    };

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
        .map_err(|e| cleanup(format!("error: could not connect to {}:{} — {e}", prompts.host, prompts.port)))?;
    conn.close().await.ok();
    println!("  connected to {}:{}", prompts.host, prompts.port);

    let (mut file_cfg, role_password) = build_file_config(&prompts, &bind);
    file_cfg.server.admin_password = Some(admin.value().to_string());

    // Written before provisioning: if provisioning fails halfway, the generated
    // secrets survive and `oms database init --resume` can finish the job.
    // Mapped to add the "error: " prefix every other failure path in this
    // function already carries — `ConfigError`'s `Display` does not include it,
    // since it is also used in contexts that add their own.
    if let Some(dir) = cfg_path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| cleanup(format!("error: could not create {}: {e}", dir.display())))?;
    }
    config::write_new(&cfg_path, &file_cfg).map_err(|e| cleanup(format!("error: {e}")))?;
    println!("  wrote {} (mode 0600)", cfg_path.display());
    // Only a file written into the current directory can land in a checkout.
    if std::env::current_dir().is_ok_and(|cwd| cfg_path.parent() == Some(cwd.as_path())) {
        ensure_gitignored(&cfg_path);
    }

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
    // `--resume` is what actually works here: `provision::provision` already ran
    // (it's the first thing `database::init` does after this same strictness
    // check), so a bare retry of `oms database init` would refuse on the role/
    // database it just created. `--resume` skips that and re-runs the remaining
    // steps, all of which tolerate re-application — see `database::init`'s doc
    // comment for why.
    crate::setup::database::init(overrides, Some(role_password), false)
        .await
        .map_err(|e| provision_failure_message(&cfg_path, &e.to_string(), started_docker))?;

    // Only a generated password is ever printed, and only to a human at a
    // terminal: one the operator typed they already know.
    let admin_line = match (&admin, interactive) {
        (AdminPassword::Typed(_), _) => "Cockpit login password: the one you chose".to_string(),
        (AdminPassword::Generated(pw), true) => format!("Cockpit login password: {pw}"),
        (AdminPassword::Generated(_), false) => "Cockpit login password: written to oms.toml".to_string(),
    };
    let container = rules::container_mode(std::env::var("OMS_CONTAINER").ok().as_deref());
    println!("{}", closing_lines(&cfg_path, &admin_line, &bind, container));
    Ok(())
}

/// The summary after a successful init: what to back up, how to start, where
/// to log in. In the published container the config lives in a compose
/// volume and the server is started by compose, so both commands differ.
pub fn closing_lines(cfg_path: &std::path::Path, admin_line: &str, bind: &str, container: bool) -> String {
    let url = cockpit_url(bind);
    if container {
        format!(
            "\nBack up oms.toml from the oms-config volume once the server runs:\n\
             \x20   docker compose cp oms:{} ./oms.toml.backup\n\
             The master key in it is the only thing that can decrypt stored broker\n\
             credentials — lose it and they are gone.\n\n\
             {admin_line}\n\n\
             Start the server:  docker compose up -d\n\
             Then open:         {url}",
            cfg_path.display()
        )
    } else {
        // Absolute (cfg_path already is): this message may be read after the
        // operator has `cd`ed elsewhere, and a relative path then points at nothing.
        format!(
            "\nBack up {}. The master key in it is the only thing that can\n\
             decrypt stored broker credentials — lose it and they are gone.\n\n\
             {admin_line}\n\n\
             Start the server:  oms\n\
             Then open:         {url}",
            cfg_path.display()
        )
    }
}

/// Where to point a browser for `bind`: a wildcard address is not one a
/// browser can open, so it becomes `localhost`.
pub fn cockpit_url(bind: &str) -> String {
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "3001"));
    let host = match host {
        "0.0.0.0" | "[::]" => "localhost",
        h => h,
    };
    format!("http://{host}:{port}/cockpit/")
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

    #[test]
    fn container_summary_names_compose_commands() {
        let out = closing_lines(std::path::Path::new("/config/oms.toml"), "Cockpit login password: x", "0.0.0.0:3001", true);
        assert!(out.contains("docker compose up -d"), "{out}");
        assert!(out.contains("docker compose cp oms:/config/oms.toml"), "{out}");
        assert!(out.contains("http://localhost:3001/cockpit/"), "{out}");
        assert!(!out.contains("Start the server:  oms\n"), "{out}");
    }

    #[test]
    fn host_summary_is_unchanged() {
        let out = closing_lines(std::path::Path::new("/home/me/.config/oms/oms.toml"), "Cockpit login password: x", "localhost:3001", false);
        assert!(out.contains("Back up /home/me/.config/oms/oms.toml."), "{out}");
        assert!(out.contains("Start the server:  oms"), "{out}");
        assert!(!out.contains("docker compose"), "{out}");
    }

    #[test]
    fn container_takes_superuser_password_from_env() {
        let env = || Some("from-env".to_string());
        assert_eq!(superuser_password_default(None, env(), true), Some("from-env".into()));
        assert_eq!(superuser_password_default(None, env(), false), None, "host: the env describes the app's database");
        assert_eq!(superuser_password_default(Some("flag".into()), env(), true), Some("flag".into()));
        assert_eq!(superuser_password_default(Some("flag".into()), env(), false), Some("flag".into()));
    }
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
        let (cfg, role_pw) = build_file_config(&p, "localhost:3001");

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
    fn sample_prompts() -> Prompts {
        Prompts {
            host: "localhost".into(),
            port: 5432,
            database: "ods".into(),
            username: "postgres".into(),
            password: "pw".into(),
        }
    }

    fn flags(admin: Option<&str>, bind: Option<&str>) -> InitFlags {
        InitFlags { source: None, admin_password: admin.map(Into::into), bind: bind.map(Into::into) }
    }

    #[test]
    fn build_file_config_writes_the_chosen_bind() {
        let (cfg, _) = build_file_config(&sample_prompts(), "0.0.0.0:4000");
        assert_eq!(cfg.server.bind_addr.as_deref(), Some("0.0.0.0:4000"));
    }

    #[test]
    fn non_interactive_defaults_to_existing_localhost_and_a_generated_password() {
        let a = answers_from_flags(&flags(None, None), sample_prompts()).unwrap();
        assert!(matches!(a.db, DbChoice::Existing(_)));
        assert!(matches!(a.admin, AdminPassword::Generated(_)));
        assert_eq!(a.bind, "localhost:3001");
    }

    #[test]
    fn non_interactive_docker_and_a_chosen_password_carry_through() {
        let f = InitFlags { source: Some(DbSource::Docker), ..flags(Some("twelve-chars-ok"), Some("0.0.0.0:3001")) };
        let a = answers_from_flags(&f, sample_prompts()).unwrap();
        assert!(matches!(a.db, DbChoice::Docker));
        assert!(matches!(a.admin, AdminPassword::Typed(ref p) if p == "twelve-chars-ok"));
        assert_eq!(a.bind, "0.0.0.0:3001");
    }

    #[test]
    fn short_admin_password_flag_is_refused() {
        let e = answers_from_flags(&flags(Some("short"), None), sample_prompts()).unwrap_err();
        assert!(e.contains("12"), "{e}");
    }

    #[test]
    fn a_bad_bind_flag_is_refused() {
        assert!(answers_from_flags(&flags(None, Some(":3001")), sample_prompts()).is_err());
    }

    /// A Docker that "starts" a container nobody listens for: readiness fails
    /// after the container exists, so the error must say how to clean up.
    struct StartsButNeverListens;

    impl docker::DockerOps for StartsButNeverListens {
        fn status(&self) -> rules::DockerStatus { rules::DockerStatus::Usable }
        fn container_exists(&self, _: &str) -> bool { false }
        fn volume_exists(&self, _: &str) -> bool { false }
        // Skip to a port nothing on this machine listens on.
        fn port_free(&self, port: u16) -> bool { port == 5531 }
        fn run_postgres(&self, _: u16, _: &str) -> Result<(), String> { Ok(()) }
    }

    #[tokio::test]
    async fn docker_failure_after_start_names_cleanup() {
        let tmp = std::env::temp_dir().join(format!("oms-init-test-{}", std::process::id()));
        let a = Answers {
            db: DbChoice::Docker,
            admin: AdminPassword::Generated("x".repeat(24)),
            bind: "localhost:3001".into(),
        };
        let e = run_with_timeout(
            a,
            &StartsButNeverListens,
            false,
            std::time::Duration::from_millis(500),
            tmp.join("none.toml"),
            tmp.join("oms.toml"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("docker rm -f oms-postgres && docker volume rm oms-pgdata"), "{e}");
        assert!(!tmp.join("oms.toml").exists(), "nothing written before the database answered");
    }

    #[tokio::test]
    async fn an_existing_config_is_refused_before_docker_runs() {
        struct Panics;
        impl docker::DockerOps for Panics {
            fn status(&self) -> rules::DockerStatus { rules::DockerStatus::Usable }
            fn container_exists(&self, _: &str) -> bool { panic!("docker touched") }
            fn volume_exists(&self, _: &str) -> bool { panic!("docker touched") }
            fn port_free(&self, _: u16) -> bool { panic!("docker touched") }
            fn run_postgres(&self, _: u16, _: &str) -> Result<(), String> { panic!("docker touched") }
        }
        let existing = std::env::temp_dir().join(format!("oms-init-existing-{}.toml", std::process::id()));
        std::fs::write(&existing, "").unwrap();
        let a = Answers { db: DbChoice::Docker, admin: AdminPassword::Generated("x".repeat(24)), bind: "localhost:3001".into() };
        let e = run_with_timeout(a, &Panics, false, std::time::Duration::from_millis(100), existing.clone(), existing.clone())
            .await
            .unwrap_err()
            .to_string();
        std::fs::remove_file(&existing).ok();
        assert!(e.contains("already exists"), "{e}");
    }

    #[test]
    fn the_cockpit_url_opens_in_a_browser() {
        assert_eq!(cockpit_url("localhost:3001"), "http://localhost:3001/cockpit/");
        assert_eq!(cockpit_url("0.0.0.0:4000"), "http://localhost:4000/cockpit/");
    }

    #[test]
    fn answers_debug_never_shows_the_admin_password() {
        let a = answers_from_flags(&flags(Some("very-secret-admin"), None), sample_prompts()).unwrap();
        let d = format!("{a:?}");
        assert!(!d.contains("very-secret-admin") && !d.contains("\"pw\""), "{d}");
    }

    #[test]
    fn a_docker_provisioning_failure_says_how_to_resume_and_how_to_start_over() {
        let cfg = std::path::Path::new("/home/u/.config/oms/oms.toml");
        let m = provision_failure_message(cfg, "boom", true);
        assert!(m.contains("POSTGRES_PASSWORD=\"$(docker exec oms-postgres printenv POSTGRES_PASSWORD)\" oms database init --resume"), "{m}");
        assert!(m.contains("docker rm -f oms-postgres && docker volume rm oms-pgdata && rm /home/u/.config/oms/oms.toml"), "{m}");
        let plain = provision_failure_message(cfg, "boom", false);
        assert!(plain.contains("oms database init --resume") && !plain.contains("printenv"), "{plain}");
    }

    struct RunFails;

    impl docker::DockerOps for RunFails {
        fn status(&self) -> rules::DockerStatus { rules::DockerStatus::Usable }
        fn container_exists(&self, _: &str) -> bool { false }
        fn volume_exists(&self, _: &str) -> bool { false }
        fn port_free(&self, _: u16) -> bool { true }
        fn run_postgres(&self, _: u16, _: &str) -> Result<(), String> { Err("docker run failed: port is already allocated".into()) }
    }

    #[tokio::test]
    async fn a_failed_docker_run_names_cleanup() {
        let tmp = std::env::temp_dir().join(format!("oms-init-runfails-{}", std::process::id()));
        let a = Answers { db: DbChoice::Docker, admin: AdminPassword::Generated("x".repeat(24)), bind: "localhost:3001".into() };
        let e = run_with_timeout(a, &RunFails, false, std::time::Duration::from_millis(100), tmp.join("none.toml"), tmp.join("oms.toml"))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("port is already allocated") && e.contains("docker rm -f oms-postgres"), "{e}");
    }

}
