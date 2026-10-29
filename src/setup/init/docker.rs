//! Start PostgreSQL 16 in Docker for `oms init`.
//!
//! The decisions (refuse over an existing container or volume, which port,
//! what the connection looks like) are pure over `DockerOps`, so they are
//! tested without Docker; `Cli` is the thin layer that shells out to it.

use std::process::Command;
use std::time::Duration;

use super::rules::DockerStatus;
use super::{generate_password, Prompts};

pub const CONTAINER: &str = "oms-postgres";
pub const VOLUME: &str = "oms-pgdata";
pub const IMAGE: &str = "postgres:16";
/// Counted from when `docker run` returns, so an image pull never eats into it.
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);

pub trait DockerOps {
    fn status(&self) -> DockerStatus;
    fn container_exists(&self, name: &str) -> bool;
    fn volume_exists(&self, name: &str) -> bool;
    fn port_free(&self, port: u16) -> bool;
    fn run_postgres(&self, port: u16, superuser_password: &str) -> Result<(), String>;
}

pub struct Cli;

impl DockerOps for Cli {
    fn status(&self) -> DockerStatus {
        let child = Command::new("docker")
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DockerStatus::NotFound,
            Err(_) => return DockerStatus::NotRunning,
        };
        // A stopped Docker Desktop can leave `docker info` hanging on its socket.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(st)) if st.success() => return DockerStatus::Usable,
                Ok(Some(_)) | Err(_) => return DockerStatus::NotRunning,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return DockerStatus::NotRunning;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    fn container_exists(&self, name: &str) -> bool {
        quiet(&["container", "inspect", name])
    }

    fn volume_exists(&self, name: &str) -> bool {
        quiet(&["volume", "inspect", name])
    }

    fn port_free(&self, port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    }

    fn run_postgres(&self, port: u16, superuser_password: &str) -> Result<(), String> {
        let publish = format!("127.0.0.1:{port}:5432");
        let volume = format!("{VOLUME}:/var/lib/postgresql/data");
        // `-e NAME` with no value makes docker read it from our environment, so
        // the password never appears in this process's argv (visible in `ps`).
        let out = Command::new("docker")
            .args(["run", "-d", "--name", CONTAINER, "--restart", "unless-stopped"])
            .args(["-e", "POSTGRES_PASSWORD", "-p", &publish, "-v", &volume, IMAGE])
            .env("POSTGRES_PASSWORD", superuser_password)
            .output()
            .map_err(|e| format!("could not run docker: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("docker run failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
        }
    }
}

fn quiet(args: &[&str]) -> bool {
    Command::new("docker")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Refuse over a container or volume of ours that already exists: it may hold
/// someone's data, so it is never reused or removed silently.
pub fn preflight(ops: &dyn DockerOps) -> Result<(), String> {
    let container = ops.container_exists(CONTAINER);
    let volume = ops.volume_exists(VOLUME);
    if !container && !volume {
        return Ok(());
    }
    let found = match (container, volume) {
        (true, true) => format!("a container named {CONTAINER} and a volume named {VOLUME} already exist"),
        (true, false) => format!("a container named {CONTAINER} already exists"),
        _ => format!("a volume named {VOLUME} already exists"),
    };
    Err(format!(
        "{found}. They may hold an earlier database, so oms init will not reuse or remove them.\n\
         \x20 Use it as an existing PostgreSQL instead, or remove both and run oms init again:\n\
         \x20     docker rm -f {CONTAINER}\n\
         \x20     docker volume rm {VOLUME}"
    ))
}

const FIRST_PORT: u16 = 5432;
const LAST_PORT: u16 = 5532;

pub fn choose_port(ops: &dyn DockerOps) -> Result<u16, String> {
    (FIRST_PORT..=LAST_PORT)
        .find(|&p| ops.port_free(p))
        .ok_or_else(|| format!("no free port between {FIRST_PORT} and {LAST_PORT} on 127.0.0.1"))
}

/// Run the container and return how to reach it, plus a note when the port
/// had to move off 5432. Does not wait for readiness: see `wait_ready`.
pub fn start(ops: &dyn DockerOps) -> Result<(Prompts, Option<String>), String> {
    preflight(ops)?;
    let port = choose_port(ops)?;
    let password = generate_password();
    ops.run_postgres(port, &password)?;
    let note = (port != FIRST_PORT).then(|| format!("port {FIRST_PORT} is taken — using {port}"));
    let prompts = Prompts {
        host: "127.0.0.1".into(),
        port,
        database: "ods".into(),
        username: "postgres".into(),
        password,
    };
    Ok((prompts, note))
}

/// Retry connecting until the new server accepts logins or `timeout` passes.
pub async fn wait_ready(p: &Prompts, timeout: Duration) -> Result<(), String> {
    use sqlx::Connection;
    let url = crate::setup::database::config::PostgresConfig {
        host: p.host.clone(),
        port: p.port,
        username: p.username.clone(),
        password: p.password.clone(),
        database: "postgres".into(),
    }
    .url_for("postgres");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let attempt = tokio::time::timeout_at(deadline, sqlx::PgConnection::connect(&url)).await;
        let last = match attempt {
            Ok(Ok(conn)) => {
                conn.close().await.ok();
                return Ok(());
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "timed out".to_string(),
        };
        if tokio::time::Instant::now() + Duration::from_millis(500) >= deadline {
            return Err(format!(
                "PostgreSQL in {CONTAINER} did not accept connections within {}s ({last}).\n\
                 \x20 See why: docker logs {CONTAINER}",
                timeout.as_secs().max(1)
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub(crate) struct Fake {
        pub container: bool,
        pub volume: bool,
        pub taken: Vec<u16>,
        pub ran: RefCell<Option<u16>>,
    }

    impl DockerOps for Fake {
        fn status(&self) -> DockerStatus {
            DockerStatus::Usable
        }
        fn container_exists(&self, name: &str) -> bool {
            self.container && name == CONTAINER
        }
        fn volume_exists(&self, name: &str) -> bool {
            self.volume && name == VOLUME
        }
        fn port_free(&self, port: u16) -> bool {
            !self.taken.contains(&port)
        }
        fn run_postgres(&self, port: u16, _pw: &str) -> Result<(), String> {
            *self.ran.borrow_mut() = Some(port);
            Ok(())
        }
    }

    #[test]
    fn an_existing_container_or_volume_is_refused_by_name() {
        let e = preflight(&Fake { container: true, ..Fake::default() }).unwrap_err();
        assert!(e.contains("docker rm -f oms-postgres") && e.contains("docker volume rm oms-pgdata"), "{e}");
        assert!(preflight(&Fake { volume: true, ..Fake::default() }).is_err());
        assert!(preflight(&Fake::default()).is_ok());
    }

    #[test]
    fn port_falls_back_from_5432() {
        assert_eq!(choose_port(&Fake::default()).unwrap(), 5432);
        assert_eq!(choose_port(&Fake { taken: vec![5432, 5433], ..Fake::default() }).unwrap(), 5434);
    }

    #[test]
    fn no_free_port_is_an_error() {
        let all: Vec<u16> = (5432..=5532).collect();
        assert!(choose_port(&Fake { taken: all, ..Fake::default() }).is_err());
    }

    #[test]
    fn start_runs_on_loopback_defaults_and_notes_a_moved_port() {
        let f = Fake { taken: vec![5432], ..Fake::default() };
        let (p, note) = start(&f).unwrap();
        assert_eq!(
            (p.host.as_str(), p.port, p.database.as_str(), p.username.as_str()),
            ("127.0.0.1", 5433, "ods", "postgres")
        );
        assert!(p.password.len() >= 24);
        assert_eq!(*f.ran.borrow(), Some(5433));
        assert!(note.unwrap().contains("5433"));
        assert!(start(&Fake::default()).unwrap().1.is_none(), "5432 needs no note");
    }

    #[test]
    fn start_never_runs_after_a_refused_preflight() {
        let f = Fake { container: true, ..Fake::default() };
        assert!(start(&f).is_err());
        assert!(f.ran.borrow().is_none());
    }

    #[tokio::test]
    async fn wait_ready_gives_up_naming_docker_logs() {
        let p = Prompts {
            host: "127.0.0.1".into(),
            port: 1,
            database: "ods".into(),
            username: "postgres".into(),
            password: "x".into(),
        };
        let e = wait_ready(&p, Duration::from_millis(800)).await.unwrap_err();
        assert!(e.contains("docker logs oms-postgres"), "{e}");
    }

    #[test]
    fn readiness_timeout_starts_after_run_returns() {
        // `start` returns once `run_postgres` (which includes any pull) finished;
        // `wait_ready` carries its own clock. Pin that `start` does not poll.
        let t = std::time::Instant::now();
        start(&Fake::default()).unwrap();
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    /// Needs Docker, and no `oms-postgres` container or `oms-pgdata` volume.
    #[tokio::test]
    #[ignore]
    async fn real_docker_postgres_comes_up() {
        let (p, _) = start(&Cli).unwrap();
        let ready = wait_ready(&p, READY_TIMEOUT).await;
        let _ = Command::new("docker").args(["rm", "-f", CONTAINER]).output();
        let _ = Command::new("docker").args(["volume", "rm", VOLUME]).output();
        ready.unwrap();
    }
}
