//! What `oms init` asks, and in which order.
//!
//! `collect` is the flow; it talks to the terminal only through `Prompter`, so
//! the flow is tested with scripted answers. `Terminal` is the dialoguer-backed
//! implementation the real command uses.

use std::io;

use super::rules::{
    check_admin_email, default_db_source, docker_fix_hint, docker_status_note, exposure_warning,
    parse_bind, DbSource, DockerStatus,
};
use super::Prompts;

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

/// The first named admin. `oms init` creates them and prints their one-time
/// enrolment link; they choose their own password and TOTP through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAccount {
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug)]
pub struct Answers {
    pub db: DbChoice,
    pub admin: AdminAccount,
    pub bind: String,
}

/// What Enter takes at each prompt, seeded from flags and the environment. A
/// `Some` in `superuser_password`, `admin_email` or `source` skips that prompt
/// (`admin_email` skips the name prompt too).
pub struct Defaults {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub superuser_password: Option<String>,
    pub admin_email: Option<String>,
    pub admin_name: Option<String>,
    pub bind: String,
    pub source: Option<DbSource>,
    /// Inside the published image (`OMS_CONTAINER=1`): compose supplies the
    /// database and publishes the port, so only the admin is asked.
    pub container: bool,
}

/// dialoguer reads stdin and draws on stderr; both must be a terminal.
pub fn can_prompt(stdin: bool, stderr: bool) -> bool {
    stdin && stderr
}

pub fn collect(p: &mut dyn Prompter, d: &Defaults, docker: DockerStatus) -> io::Result<Answers> {
    let source = match d.source {
        Some(s) => s,
        None if d.container => DbSource::Existing,
        None => choose_source(p, docker)?,
    };
    let db = match source {
        DbSource::Docker => DbChoice::Docker,
        DbSource::Existing if d.container => DbChoice::Existing(compose_postgres(p, d)?),
        DbSource::Existing => DbChoice::Existing(existing_postgres(p, d)?),
    };
    let admin = match &d.admin_email {
        Some(email) => AdminAccount { email: email.trim().to_string(), name: d.admin_name.clone() },
        None => admin_account(p, d.admin_name.as_deref())?,
    };
    if d.container {
        // Bound on all interfaces inside the container; compose publishes it on
        // the host's loopback only, so the exposure warning would be wrong.
        let bind = parse_bind(&d.bind).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        return Ok(Answers { db, admin, bind });
    }
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

/// The compose service's connection, from the environment compose sets. Asks
/// for the superuser password only if compose did not pass one.
fn compose_postgres(p: &mut dyn Prompter, d: &Defaults) -> io::Result<Prompts> {
    let password = match &d.superuser_password {
        Some(pw) => pw.clone(),
        None => superuser_password(p)?,
    };
    Ok(Prompts {
        host: d.host.clone(),
        port: d.port,
        database: d.database.clone(),
        username: d.username.clone(),
        password,
    })
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
        None => superuser_password(p)?,
    };
    Ok(Prompts {
        host: host.trim().to_string(),
        port: port.trim().parse().expect("validated above"),
        database: database.trim().to_string(),
        username: username.trim().to_string(),
        password,
    })
}

fn superuser_password(p: &mut dyn Prompter) -> io::Result<String> {
    loop {
        let pw = p.password("Superuser password", false)?;
        if !pw.is_empty() {
            return Ok(pw);
        }
        p.note("the superuser password is needed once, to create the database and the oms role");
    }
}

fn non_empty(v: &str, what: &str) -> Result<(), String> {
    if v.trim().is_empty() { Err(format!("the {what} cannot be empty")) } else { Ok(()) }
}

fn admin_account(p: &mut dyn Prompter, name: Option<&str>) -> io::Result<AdminAccount> {
    let email = p.input("Admin email", "", &check_admin_email)?;
    let name = p.input("Admin name (optional)", name.unwrap_or(""), &|_| Ok(()))?;
    let name = name.trim();
    Ok(AdminAccount { email: email.trim().to_string(), name: (!name.is_empty()).then(|| name.to_string()) })
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
        // An empty default shows no "[]" and lets Enter through to the
        // validator (which re-asks for a required value).
        let input = dialoguer::Input::<String>::with_theme(&self.theme).with_prompt(label).allow_empty(true);
        let input = if default.is_empty() { input } else { input.default(default.to_string()) };
        input.validate_with(|v: &String| validate(v)).interact_text().map_err(io_err)
    }

    /// With `confirm`, the password is asked twice, and confirmed only when
    /// something was typed.
    fn password(&mut self, label: &str, confirm: bool) -> io::Result<String> {
        let ask = |prompt: &str| {
            dialoguer::Password::with_theme(&self.theme)
                .with_prompt(prompt)
                .allow_empty_password(true)
                .interact()
                .map_err(io_err)
        };
        loop {
            let pw = ask(label)?;
            if !confirm || pw.is_empty() || ask("Confirm")? == pw {
                return Ok(pw);
            }
            println!("  The passwords don't match — try again");
        }
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
            admin_email: None,
            admin_name: None,
            bind: "localhost:3001".into(),
            source: None,
            container: false,
        }
    }

    #[test]
    fn container_asks_only_the_admin() {
        let d = Defaults {
            container: true,
            host: "postgres".into(),
            superuser_password: Some("pg-secret".into()),
            bind: "0.0.0.0:3001".into(),
            ..defaults()
        };
        let mut s = Script::new().inputs(["ada@example.com", "Ada"]);
        let a = collect(&mut s, &d, DockerStatus::NotFound).unwrap();
        match &a.db {
            DbChoice::Existing(p) => {
                assert_eq!(p.host, "postgres");
                assert_eq!(p.password, "pg-secret");
            }
            other => panic!("expected the compose database, got {other:?}"),
        }
        assert_eq!(a.admin, AdminAccount { email: "ada@example.com".into(), name: Some("Ada".into()) });
        assert_eq!(a.bind, "0.0.0.0:3001");
        assert_eq!((s.selects, s.passwords), (0, 0), "only the admin is asked");
        assert!(s.replies.is_empty());
        assert!(s.notes.is_empty(), "no exposure warning in a container: {:?}", s.notes);
    }

    #[test]
    fn enter_everywhere_with_docker_takes_docker_and_localhost() {
        let mut s = Script::new().select_default().inputs(["ada@example.com", ""]).input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert_eq!(s.select_defaults, vec![0]);
        assert!(matches!(a.db, DbChoice::Docker));
        assert_eq!(a.admin, AdminAccount { email: "ada@example.com".into(), name: None });
        assert_eq!(a.bind, "localhost:3001");
        assert!(s.replies.is_empty());
    }

    #[test]
    fn without_docker_the_menu_defaults_to_existing() {
        let mut s = Script::new()
            .select_default()
            .inputs(["db.internal", "5433", "ods", "postgres"])
            .password("su-pw")
            .inputs(["ada@example.com", ""])
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
            .inputs(["ada@example.com", ""])
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
            .inputs(["ada@example.com", ""])
            .input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert!(matches!(a.db, DbChoice::Existing(ref p) if p.password == "su"));
    }

    #[test]
    fn the_admin_is_asked_after_the_database() {
        let mut s = Script::new().select(0).inputs(["  ada@example.com ", " Ada Lovelace "]).input("localhost:3001");
        let a = collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert_eq!(a.admin, AdminAccount { email: "ada@example.com".into(), name: Some("Ada Lovelace".into()) });
        assert_eq!(s.passwords, 0, "no admin password is asked");
    }

    #[test]
    fn flag_values_skip_their_prompts() {
        let d = Defaults {
            source: Some(DbSource::Docker),
            admin_email: Some("ada@example.com".into()),
            admin_name: Some("Ada".into()),
            ..defaults()
        };
        let mut s = Script::new().input("localhost:3001");
        let a = collect(&mut s, &d, DockerStatus::Usable).unwrap();
        assert_eq!(a.admin, AdminAccount { email: "ada@example.com".into(), name: Some("Ada".into()) });
        assert_eq!(s.selects + s.passwords, 0);
    }

    #[test]
    fn a_supplied_superuser_password_skips_its_prompt() {
        let d = Defaults { source: Some(DbSource::Existing), superuser_password: Some("given".into()), ..defaults() };
        let mut s = Script::new().inputs(["localhost", "5432", "ods", "postgres", "ada@example.com", ""]).input("localhost:3001");
        let a = collect(&mut s, &d, DockerStatus::Usable).unwrap();
        assert!(matches!(a.db, DbChoice::Existing(ref p) if p.password == "given"));
        assert_eq!(s.passwords, 0, "the superuser password is not asked again");
    }

    #[test]
    fn a_network_bind_warns_once() {
        let mut s = Script::new().select(0).inputs(["ada@example.com", ""]).input("0.0.0.0:3001");
        collect(&mut s, &defaults(), DockerStatus::Usable).unwrap();
        assert_eq!(s.notes.iter().filter(|n| n.contains("network")).count(), 1);
    }
    #[test]
    fn prompting_needs_stdin_and_stderr_on_a_terminal() {
        assert!(can_prompt(true, true));
        assert!(!can_prompt(false, true));
        assert!(!can_prompt(true, false));
    }


}
