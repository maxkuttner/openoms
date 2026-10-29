//! What `oms init` asks, and in which order.
//!
//! `collect` is the flow; it talks to the terminal only through `Prompter`, so
//! the flow is tested with scripted answers. `Terminal` is the dialoguer-backed
//! implementation the real command uses.

use std::io;

use super::rules::{
    check_admin_password, default_db_source, docker_fix_hint, docker_status_note, exposure_warning,
    parse_bind, DbSource, DockerStatus,
};
use super::{generate_password, Prompts};

pub trait Prompter {
    fn select(&mut self, label: &str, items: &[String], default: usize) -> io::Result<usize>;
    fn input(
        &mut self,
        label: &str,
        default: &str,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> io::Result<String>;
    /// Without echo. Returns `""` when the user just pressed Enter.
    fn password(&mut self, label: &str, confirm: bool) -> io::Result<String>;
    fn note(&mut self, line: &str);
}

#[derive(Debug)]
pub enum DbChoice {
    Docker,
    Existing(Prompts),
}

pub enum AdminPassword {
    Typed(String),
    Generated(String),
}

// Hand-written: the password itself never reaches a `{:?}`.
impl std::fmt::Debug for AdminPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AdminPassword::Typed(_) => "Typed(<redacted>)",
            AdminPassword::Generated(_) => "Generated(<redacted>)",
        })
    }
}

impl AdminPassword {
    pub fn value(&self) -> &str {
        match self {
            AdminPassword::Typed(p) | AdminPassword::Generated(p) => p,
        }
    }
}

#[derive(Debug)]
pub struct Answers {
    pub db: DbChoice,
    pub admin: AdminPassword,
    pub bind: String,
}

/// What Enter takes at each prompt, seeded from flags and the environment. A
/// `Some` in `superuser_password`, `admin_password` or `source` skips that prompt.
pub struct Defaults {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub superuser_password: Option<String>,
    pub admin_password: Option<String>,
    pub bind: String,
    pub source: Option<DbSource>,
}

pub fn collect(p: &mut dyn Prompter, d: &Defaults, docker: DockerStatus) -> io::Result<Answers> {
    let source = match d.source {
        Some(s) => s,
        None => choose_source(p, docker)?,
    };
    let db = match source {
        DbSource::Docker => DbChoice::Docker,
        DbSource::Existing => DbChoice::Existing(existing_postgres(p, d)?),
    };
    let admin = match &d.admin_password {
        Some(pw) => AdminPassword::Typed(pw.clone()),
        None => admin_password(p)?,
    };
    let bind = p.input("Listen on", &d.bind, &|v| parse_bind(v).map(|_| ()))?;
    let bind = parse_bind(&bind).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if let Some(w) = exposure_warning(&bind) {
        p.note(&w);
    }
    Ok(Answers { db, admin, bind })
}

fn choose_source(p: &mut dyn Prompter, docker: DockerStatus) -> io::Result<DbSource> {
    let items = [
        format!("Start PostgreSQL 16 in Docker for me   ({})", docker_status_note(docker)),
        "Use an existing PostgreSQL 16".to_string(),
    ];
    let default = match default_db_source(docker) {
        DbSource::Docker => 0,
        DbSource::Existing => 1,
    };
    loop {
        if p.select("Database", &items, default)? == 1 {
            return Ok(DbSource::Existing);
        }
        match docker_fix_hint(docker) {
            None => return Ok(DbSource::Docker),
            Some(hint) => p.note(hint),
        }
    }
}

fn existing_postgres(p: &mut dyn Prompter, d: &Defaults) -> io::Result<Prompts> {
    let host = p.input("Postgres host", &d.host, &|v| non_empty(v, "host"))?;
    let port = p.input("Postgres port", &d.port.to_string(), &|v| {
        v.trim().parse::<u16>().map(|_| ()).map_err(|_| format!("{v:?} is not a port number"))
    })?;
    let database = p.input("Database name", &d.database, &|v| non_empty(v, "database name"))?;
    let username = p.input("Superuser name", &d.username, &|v| non_empty(v, "superuser name"))?;
    let password = match &d.superuser_password {
        Some(pw) => pw.clone(),
        None => loop {
            let pw = p.password("Superuser password", false)?;
            if !pw.is_empty() {
                break pw;
            }
            p.note("the superuser password is needed once, to create the database and the oms role");
        },
    };
    Ok(Prompts {
        host: host.trim().to_string(),
        port: port.trim().parse().expect("validated above"),
        database: database.trim().to_string(),
        username: username.trim().to_string(),
        password,
    })
}

fn non_empty(v: &str, what: &str) -> Result<(), String> {
    if v.trim().is_empty() { Err(format!("the {what} cannot be empty")) } else { Ok(()) }
}

fn admin_password(p: &mut dyn Prompter) -> io::Result<AdminPassword> {
    loop {
        let pw = p.password("Cockpit admin password (Enter = generate)", true)?;
        if pw.is_empty() {
            return Ok(AdminPassword::Generated(generate_password()));
        }
        match check_admin_password(&pw) {
            Ok(()) => return Ok(AdminPassword::Typed(pw)),
            Err(e) => p.note(&e),
        }
    }
}

/// The real terminal, through dialoguer.
pub struct Terminal {
    theme: dialoguer::theme::ColorfulTheme,
}

impl Terminal {
    pub fn new() -> Self {
        Self { theme: dialoguer::theme::ColorfulTheme::default() }
    }
}

fn io_err(e: dialoguer::Error) -> io::Error {
    match e {
        dialoguer::Error::IO(e) => e,
    }
}

impl Prompter for Terminal {
    fn select(&mut self, label: &str, items: &[String], default: usize) -> io::Result<usize> {
        dialoguer::Select::with_theme(&self.theme)
            .with_prompt(label)
            .items(items)
            .default(default)
            .interact()
            .map_err(io_err)
    }

    fn input(
        &mut self,
        label: &str,
        default: &str,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> io::Result<String> {
        dialoguer::Input::<String>::with_theme(&self.theme)
            .with_prompt(label)
            .default(default.to_string())
            .validate_with(|v: &String| validate(v))
            .interact_text()
            .map_err(io_err)
    }

    fn password(&mut self, label: &str, confirm: bool) -> io::Result<String> {
        let mut prompt = dialoguer::Password::with_theme(&self.theme);
        prompt = prompt.with_prompt(label).allow_empty_password(true);
        if confirm {
            prompt = prompt.with_confirmation("Confirm", "The passwords don't match — try again");
        }
        prompt.interact().map_err(io_err)
    }

    fn note(&mut self, line: &str) {
        println!("  {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    enum Reply {
        /// `None` takes the offered default.
        Select(Option<usize>),
        Input(String),
        Password(String),
    }

    #[derive(Default)]
    struct Script {
        replies: VecDeque<Reply>,
        select_defaults: Vec<usize>,
        selects: usize,
        passwords: usize,
        notes: Vec<String>,
    }

    impl Script {
        fn new() -> Self {
            Self::default()
        }
        fn select(mut self, i: usize) -> Self {
            self.replies.push_back(Reply::Select(Some(i)));
            self
        }
        fn select_default(mut self) -> Self {
            self.replies.push_back(Reply::Select(None));
            self
        }
        fn input(mut self, v: &str) -> Self {
            self.replies.push_back(Reply::Input(v.into()));
            self
        }
        fn inputs<const N: usize>(mut self, vs: [&str; N]) -> Self {
            for v in vs {
                self.replies.push_back(Reply::Input(v.into()));
            }
            self
        }
        fn password(mut self, v: &str) -> Self {
            self.replies.push_back(Reply::Password(v.into()));
            self
        }
    }

    impl Prompter for Script {
        fn select(&mut self, label: &str, items: &[String], default: usize) -> io::Result<usize> {
            self.selects += 1;
            self.select_defaults.push(default);
            match self.replies.pop_front() {
                Some(Reply::Select(i)) => {
                    let i = i.unwrap_or(default);
                    assert!(i < items.len(), "{label}: no item {i}");
                    Ok(i)
                }
                _ => panic!("unexpected select: {label}"),
            }
        }
        fn input(
            &mut self,
            label: &str,
            _default: &str,
            validate: &dyn Fn(&str) -> Result<(), String>,
        ) -> io::Result<String> {
            match self.replies.pop_front() {
                Some(Reply::Input(v)) => {
                    // dialoguer never returns a value its validator rejected.
                    validate(&v).unwrap_or_else(|e| panic!("{label}: scripted {v:?} rejected: {e}"));
                    Ok(v)
                }
                _ => panic!("unexpected input: {label}"),
            }
        }
        fn password(&mut self, label: &str, _confirm: bool) -> io::Result<String> {
            self.passwords += 1;
            match self.replies.pop_front() {
                Some(Reply::Password(v)) => Ok(v),
                _ => panic!("unexpected password: {label}"),
            }
        }
        fn note(&mut self, line: &str) {
            self.notes.push(line.to_string());
        }
    }

    fn defaults() -> Defaults {
        Defaults {
            host: "localhost".into(),
            port: 5432,
            database: "ods".into(),
            username: "postgres".into(),
            superuser_password: None,
            admin_password: None,
            bind: "localhost:3001".into(),
            source: None,
        }
    }

    #[test]
    fn enter_everywhere_with_docker_takes_docker_a_generated_password_and_localhost() {
        let mut s = Script::new().select_default().password("").input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert_eq!(s.select_defaults, vec![0]);
        assert!(matches!(a.db, DbChoice::Docker));
        assert!(matches!(a.admin, AdminPassword::Generated(ref p) if p.len() >= 24));
        assert_eq!(a.bind, "localhost:3001");
        assert!(s.replies.is_empty());
    }

    #[test]
    fn without_docker_the_menu_defaults_to_existing() {
        let mut s = Script::new()
            .select_default()
            .inputs(["db.internal", "5433", "ods", "postgres"])
            .password("su-pw")
            .password("")
            .input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::NotFound).unwrap();
        assert_eq!(s.select_defaults, vec![1]);
        assert!(matches!(a.db, DbChoice::Existing(ref p)
            if p.host == "db.internal" && p.port == 5433 && p.password == "su-pw"));
    }

    #[test]
    fn choosing_unusable_docker_explains_and_asks_again() {
        let mut s = Script::new()
            .select(0)
            .select(1)
            .inputs(["localhost", "5432", "ods", "postgres"])
            .password("su")
            .password("")
            .input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::NotRunning).unwrap();
        assert!(matches!(a.db, DbChoice::Existing(_)));
        assert!(s.notes.iter().any(|n| n.contains("docker")), "{:?}", s.notes);
    }

    #[test]
    fn an_empty_superuser_password_is_re_asked() {
        let mut s = Script::new()
            .select(1)
            .inputs(["localhost", "5432", "ods", "postgres"])
            .password("")
            .password("su")
            .password("")
            .input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert!(matches!(a.db, DbChoice::Existing(ref p) if p.password == "su"));
    }

    #[test]
    fn a_short_admin_password_is_refused_and_re_asked() {
        let mut s = Script::new()
            .select(0)
            .password("short")
            .password("long-enough-pw")
            .input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert!(matches!(a.admin, AdminPassword::Typed(ref p) if p == "long-enough-pw"));
        assert!(s.notes.iter().any(|n| n.contains("12")), "{:?}", s.notes);
    }

    #[test]
    fn flag_values_skip_their_prompts() {
        let d = Defaults {
            source: Some(DbSource::Docker),
            admin_password: Some("from-the-flag-1".into()),
            ..defaults()
        };
        let mut s = Script::new().input("localhost:3001");
        let a = collect(&mut s, &d, DockerStatus::Usable).unwrap();
        assert!(matches!(a.admin, AdminPassword::Typed(_)));
        assert_eq!(s.selects + s.passwords, 0);
    }

    #[test]
    fn a_supplied_superuser_password_skips_its_prompt() {
        let d = Defaults { source: Some(DbSource::Existing), superuser_password: Some("given".into()), ..defaults() };
        let mut s = Script::new().inputs(["localhost", "5432", "ods", "postgres"]).password("").input("localhost:3001");
        let a = collect(&mut s, &d, DockerStatus::Usable).unwrap();
        assert!(matches!(a.db, DbChoice::Existing(ref p) if p.password == "given"));
        assert_eq!(s.passwords, 1, "only the admin password is asked");
    }

    #[test]
    fn a_network_bind_warns_once() {
        let mut s = Script::new().select(0).password("").input("0.0.0.0:3001");
        collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert_eq!(s.notes.iter().filter(|n| n.contains("network")).count(), 1);
    }
}
