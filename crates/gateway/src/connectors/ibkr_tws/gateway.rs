//! The IB Gateway container that the TWS API connector talks to.
//!
//! The decisions (adopt or start, which port, what the env file holds, how a
//! log tail reads) are pure over `GatewayDocker`, so they are tested without
//! Docker; `DockerCli` is the thin layer that shells out to it.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use super::IbkrTwsCredentials;

/// Pinned by tag and digest: the login automation (IBC) is tied to the gateway version.
pub const IMAGE: &str = "ghcr.io/gnzsnz/ib-gateway:10.51.1e@sha256:85975f493bcacc8341cb42e6946c460868d165e5f64d9924b1958c8f03e7bee4";
/// Where the image's socat exposes the API inside the container (the gateway
/// itself listens on 4002 / 4001 on the container's loopback only).
pub const CONTAINER_PORT_PAPER: u16 = 4004;
pub const CONTAINER_PORT_LIVE: u16 = 4003;
/// Deadlines for docker calls; `run` may pull the image first.
const QUICK: Duration = Duration::from_secs(30);
const PULL: Duration = Duration::from_secs(600);
const FIRST_HOST_PORT: u16 = 4100;
const LAST_HOST_PORT: u16 = 4199;

#[derive(Debug, Clone, PartialEq)]
pub enum GatewayState {
    Starting,
    Connected,
    AwaitingTwoFactor,
    LoginFailed(String),
    Disconnected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerInfo {
    pub running: bool,
    /// The host port the API is published on.
    pub api_port: Option<u16>,
    /// The container-side API port it maps to: paper or live.
    pub container_port: Option<u16>,
    /// Whether the API is published on 127.0.0.1 only.
    pub loopback_only: bool,
}

/// How to start the container. The password is not here: it travels in the
/// env file only, so this can be logged.
#[derive(Clone)]
pub struct RunSpec {
    pub name: String,
    pub image: &'static str,
    pub api_port: u16,
    /// The container-side API port: [`CONTAINER_PORT_PAPER`] or [`CONTAINER_PORT_LIVE`].
    pub container_port: u16,
    pub env_file: PathBuf,
}

impl fmt::Debug for RunSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("api_port", &self.api_port)
            .field("container_port", &self.container_port)
            .field("env_file", &self.env_file)
            .finish()
    }
}

pub trait GatewayDocker: Send + Sync {
    fn inspect(&self, name: &str) -> Option<ContainerInfo>;
    fn run(&self, spec: &RunSpec) -> Result<(), String>;
    fn stop_rm(&self, name: &str) -> Result<(), String>;
    fn logs_tail(&self, name: &str, lines: usize) -> String;
    fn available(&self) -> bool;
}

pub fn container_name(connection_code: &str) -> String {
    format!("oms-ibgw-{}", connection_code.to_lowercase())
}

/// Reads the IBC lines in a log tail; the latest recognised line wins.
pub fn classify_logs(tail: &str) -> Option<GatewayState> {
    tail.lines().rev().find_map(|line| {
        let l = line.to_lowercase();
        if l.contains("second factor authentication") {
            Some(GatewayState::AwaitingTwoFactor)
        } else if l.contains("login failed") || l.contains("invalid username or password") {
            Some(GatewayState::LoginFailed(line.trim().to_string()))
        } else {
            None
        }
    })
}

/// Deletes the env file when dropped, so it goes even if `run` fails.
struct EnvFile(PathBuf);

impl Drop for EnvFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_env_file(path: &Path, env: &str, creds: &IbkrTwsCredentials) -> Result<EnvFile, String> {
    // The env-file format is one `NAME=value` per line, so a newline cannot be carried.
    if [&creds.username, &creds.password]
        .iter()
        .any(|v| v.contains(['\n', '\r']))
    {
        return Err("IBKR username or password contains a line break".into());
    }
    let mode = if env.eq_ignore_ascii_case("live") {
        "live"
    } else {
        "paper"
    };
    // `create_new` (O_EXCL) never follows a symlink and never reuses a file, so
    // the mode below always applies; the removal clears a stale leftover first.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let _ = std::fs::remove_file(path); // removes a symlink itself, not its target
    let mut file = opts
        .open(path)
        .map_err(|e| format!("could not create the gateway env file: {e}"))?;
    let guard = EnvFile(path.to_path_buf());
    write!(
        file,
        "TWS_USERID={}\nTWS_PASSWORD={}\nTRADING_MODE={mode}\nREAD_ONLY_API=no\n",
        creds.username, creds.password
    )
    .map_err(|e| format!("could not write the gateway env file: {e}"))?;
    Ok(guard)
}

fn port_free(port: u16) -> bool {
    ["127.0.0.1", "0.0.0.0"]
        .iter()
        .all(|host| std::net::TcpListener::bind((*host, port)).is_ok())
}

/// Adopt the running gateway container for `code`, or start one. Returns the
/// host port its API is published on. Errors are the state to surface.
/// The docker CLI blocks, so each call runs on the blocking pool.
pub async fn ensure_running(
    docker: Arc<dyn GatewayDocker>,
    code: &str,
    env: &str,
    creds: &IbkrTwsCredentials,
    state_dir: &Path,
) -> Result<u16, GatewayState> {
    let failed = |m: String| GatewayState::LoginFailed(m);
    let blocking = |f: Box<dyn FnOnce() -> Result<Option<u16>, GatewayState> + Send>| async move {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| GatewayState::LoginFailed(format!("docker task failed: {e}")))?
    };
    let live = env.eq_ignore_ascii_case("live");
    let container_port = if live {
        CONTAINER_PORT_LIVE
    } else {
        CONTAINER_PORT_PAPER
    };
    let name = container_name(code);
    let (creds, state_dir, env) = (creds.clone(), state_dir.to_path_buf(), env.to_string());
    let adopted = {
        let (docker, name) = (docker.clone(), name.clone());
        blocking(Box::new(move || {
            if !docker.available() {
                return Err(GatewayState::LoginFailed(
                    "Docker is required for IBKR_TWS but is not running".into(),
                ));
            }
            let Some(info) = docker.inspect(&name) else {
                return Ok(None);
            };
            // Only a running container for this very environment, published on
            // loopback, is ours to reuse: paper and live are not interchangeable.
            if let (true, Some(port), true, true) = (
                info.running,
                info.api_port,
                info.container_port == Some(container_port),
                info.loopback_only,
            ) {
                return Ok(Some(port));
            }
            docker.stop_rm(&name).map_err(GatewayState::LoginFailed)?;
            Ok(None)
        }))
        .await?
    };
    if let Some(port) = adopted {
        return Ok(port);
    }
    blocking(Box::new(move || {
        let api_port = (FIRST_HOST_PORT..=LAST_HOST_PORT)
            .find(|&p| port_free(p))
            .ok_or_else(|| {
                failed(format!(
                    "no free port between {FIRST_HOST_PORT} and {LAST_HOST_PORT} on 127.0.0.1"
                ))
            })?;
        std::fs::create_dir_all(&state_dir)
            .map_err(|e| failed(format!("could not create {}: {e}", state_dir.display())))?;
        let env_path = state_dir.join(format!("{name}.env"));
        let _env_file = write_env_file(&env_path, &env, &creds).map_err(failed)?;
        let spec = RunSpec {
            name,
            image: IMAGE,
            api_port,
            container_port,
            env_file: env_path,
        };
        docker.run(&spec).map_err(failed)?;
        Ok(Some(api_port))
    }))
    .await
    .map(|p| p.expect("a start returns its port"))
}

pub struct DockerCli;

impl DockerCli {
    /// `docker <args>` with a deadline; the pipes are drained on threads so
    /// a large log tail cannot fill them while we wait.
    fn output(args: &[&str], timeout: Duration) -> Result<std::process::Output, String> {
        use std::io::Read;
        let mut child = Command::new("docker")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run docker: {e}"))?;
        fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = r.read_to_end(&mut buf);
                buf
            })
        }
        let out = drain(child.stdout.take().expect("piped"));
        let err = drain(child.stderr.take().expect("piped"));
        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Err(e) => return Err(format!("could not wait for docker: {e}")),
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "docker {} timed out",
                        args.first().copied().unwrap_or("")
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        Ok(std::process::Output {
            status,
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        })
    }
}

impl GatewayDocker for DockerCli {
    fn inspect(&self, name: &str) -> Option<ContainerInfo> {
        let out = Self::output(&["container", "inspect", name], QUICK)
            .ok()
            .filter(|o| o.status.success())?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let c = v.get(0)?;
        let running = c["State"]["Running"].as_bool().unwrap_or(false);
        let ports = &c["NetworkSettings"]["Ports"];
        let (container_port, binding) = [CONTAINER_PORT_PAPER, CONTAINER_PORT_LIVE]
            .iter()
            .find_map(|&p| Some((p, ports[format!("{p}/tcp")].as_array()?.first()?.clone())))
            .map_or((None, None), |(p, b)| (Some(p), Some(b)));
        let api_port = binding
            .as_ref()
            .and_then(|b| b["HostPort"].as_str()?.parse().ok());
        let loopback_only = ports.as_object().is_some_and(|m| {
            m.values()
                .filter_map(|v| v.as_array())
                .flatten()
                .all(|b| b["HostIp"] == "127.0.0.1")
        });
        Some(ContainerInfo {
            running,
            api_port,
            container_port,
            loopback_only,
        })
    }

    fn run(&self, spec: &RunSpec) -> Result<(), String> {
        let publish = format!("127.0.0.1:{}:{}", spec.api_port, spec.container_port);
        let env_file = spec.env_file.to_string_lossy();
        // No restart policy: the supervisor owns restarts, and a bad login must
        // not be retried by Docker (repeated failures can lock the account).
        let out = Self::output(
            &[
                "run",
                "-d",
                "--name",
                &spec.name,
                "-p",
                &publish,
                "--env-file",
                &env_file,
                spec.image,
            ],
            PULL,
        )?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn stop_rm(&self, name: &str) -> Result<(), String> {
        let out = Self::output(&["rm", "-f", name], QUICK)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "docker rm failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn logs_tail(&self, name: &str, lines: usize) -> String {
        match Self::output(&["logs", "--tail", &lines.to_string(), name], QUICK) {
            // The container's stdout and stderr both carry IBC's lines.
            Ok(o) => format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(_) => String::new(),
        }
    }

    fn available(&self) -> bool {
        let Ok(mut child) = Command::new("docker")
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        // A stopped Docker Desktop can leave `docker info` hanging on its socket.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(st)) => return st.success(),
                Err(_) => return false,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeDocker {
        existing: Option<ContainerInfo>,
        unavailable: bool,
        fail_run: bool,
        runs: Mutex<Vec<RunSpec>>,
        removed: Mutex<Vec<String>>,
        /// (file mode, contents) of the env file as `run` saw it.
        seen_env: Mutex<Option<(u32, String)>>,
    }

    impl GatewayDocker for FakeDocker {
        fn inspect(&self, _: &str) -> Option<ContainerInfo> {
            self.existing.clone()
        }
        fn run(&self, spec: &RunSpec) -> Result<(), String> {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&spec.env_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            *self.seen_env.lock().unwrap() =
                Some((mode, std::fs::read_to_string(&spec.env_file).unwrap()));
            self.runs.lock().unwrap().push(spec.clone());
            if self.fail_run {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
        fn stop_rm(&self, name: &str) -> Result<(), String> {
            self.removed.lock().unwrap().push(name.to_string());
            Ok(())
        }
        fn logs_tail(&self, _: &str, _: usize) -> String {
            String::new()
        }
        fn available(&self) -> bool {
            !self.unavailable
        }
    }

    fn creds() -> IbkrTwsCredentials {
        IbkrTwsCredentials {
            username: "trader1".into(),
            password: "s3cret-pw".into(),
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oms-ibgw-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn a_running_container_is_adopted_not_recreated() {
        let fake = Arc::new(FakeDocker {
            existing: Some(ContainerInfo {
                running: true,
                api_port: Some(4123),
                container_port: Some(CONTAINER_PORT_PAPER),
                loopback_only: true,
            }),
            ..Default::default()
        });
        let port = ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &dir("adopt")).await;
        assert_eq!(port, Ok(4123));
        assert!(fake.runs.lock().unwrap().is_empty());
        assert!(fake.removed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_paper_container_is_replaced_when_live_is_requested() {
        let fake = Arc::new(FakeDocker {
            existing: Some(ContainerInfo {
                running: true,
                api_port: Some(4123),
                container_port: Some(CONTAINER_PORT_PAPER),
                loopback_only: true,
            }),
            ..Default::default()
        });
        ensure_running(fake.clone(), "IBKR_TWS", "live", &creds(), &dir("mismatch"))
            .await
            .unwrap();
        assert_eq!(fake.removed.lock().unwrap().len(), 1);
        assert_eq!(
            fake.runs.lock().unwrap()[0].container_port,
            CONTAINER_PORT_LIVE
        );
    }

    #[tokio::test]
    async fn a_container_not_bound_to_loopback_is_replaced() {
        let fake = Arc::new(FakeDocker {
            existing: Some(ContainerInfo {
                running: true,
                api_port: Some(4123),
                container_port: Some(CONTAINER_PORT_PAPER),
                loopback_only: false,
            }),
            ..Default::default()
        });
        ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &dir("exposed"))
            .await
            .unwrap();
        assert_eq!(fake.runs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_stale_env_file_and_a_symlink_do_not_leak_the_password() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir("stale");
        let path = d.join("oms-ibgw-ibkr_tws.env");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let fake = Arc::new(FakeDocker::default());
        ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &d)
            .await
            .unwrap();
        assert_eq!(fake.seen_env.lock().unwrap().clone().unwrap().0, 0o600);

        let target = d.join("target");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &d)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        assert_eq!(fake.seen_env.lock().unwrap().clone().unwrap().0, 0o600);
    }

    #[tokio::test]
    async fn a_stopped_container_is_replaced() {
        let fake = Arc::new(FakeDocker {
            existing: Some(ContainerInfo {
                running: false,
                api_port: Some(4123),
                container_port: Some(CONTAINER_PORT_PAPER),
                loopback_only: true,
            }),
            ..Default::default()
        });
        ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &dir("stopped"))
            .await
            .unwrap();
        assert_eq!(
            *fake.removed.lock().unwrap(),
            vec!["oms-ibgw-ibkr_tws".to_string()]
        );
        assert_eq!(fake.runs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_missing_container_is_started_on_localhost() {
        let fake = Arc::new(FakeDocker::default());
        let port = ensure_running(fake.clone(), "IBKR_TWS", "live", &creds(), &dir("start"))
            .await
            .unwrap();
        assert!((4100..=4199).contains(&port));
        let runs = fake.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].api_port, port);
        assert_eq!(runs[0].container_port, CONTAINER_PORT_LIVE);
        assert_eq!(runs[0].name, "oms-ibgw-ibkr_tws");
    }

    #[tokio::test]
    async fn the_env_file_is_gone_after_start_and_was_0600() {
        let fake = Arc::new(FakeDocker::default());
        ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &dir("envfile"))
            .await
            .unwrap();
        let (mode, contents) = fake.seen_env.lock().unwrap().clone().unwrap();
        assert_eq!(mode, 0o600);
        for line in [
            "TWS_USERID=trader1",
            "TWS_PASSWORD=s3cret-pw",
            "TRADING_MODE=paper",
            "READ_ONLY_API=no",
        ] {
            assert!(contents.lines().any(|l| l == line), "missing {line}");
        }
        assert!(!fake.runs.lock().unwrap()[0].env_file.exists());
    }

    #[tokio::test]
    async fn the_env_file_is_gone_when_run_fails() {
        let fake = Arc::new(FakeDocker {
            fail_run: true,
            ..Default::default()
        });
        let r = ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &dir("runfail")).await;
        assert_eq!(r, Err(GatewayState::LoginFailed("boom".into())));
        assert!(!fake.runs.lock().unwrap()[0].env_file.exists());
    }

    #[tokio::test]
    async fn the_password_is_not_in_the_run_spec_debug() {
        let fake = Arc::new(FakeDocker::default());
        ensure_running(fake.clone(), "IBKR_TWS", "paper", &creds(), &dir("debug"))
            .await
            .unwrap();
        let shown = format!("{:?}", fake.runs.lock().unwrap()[0]);
        assert!(!shown.contains("s3cret-pw"));
    }

    #[test]
    fn a_failed_login_is_classified_and_not_retried() {
        assert!(matches!(
            classify_logs("IBC: ... Login failed ..."),
            Some(GatewayState::LoginFailed(_))
        ));
        assert!(matches!(
            classify_logs("Invalid username or password"),
            Some(GatewayState::LoginFailed(_))
        ));
        assert_eq!(classify_logs("all quiet"), None);
    }

    #[test]
    fn two_factor_prompt_is_classified() {
        assert_eq!(
            classify_logs("IBC: Second Factor Authentication window"),
            Some(GatewayState::AwaitingTwoFactor)
        );
    }

    #[tokio::test]
    async fn no_docker_is_a_clear_error() {
        let fake = Arc::new(FakeDocker {
            unavailable: true,
            ..Default::default()
        });
        let r = ensure_running(
            fake.clone(),
            "IBKR_TWS",
            "paper",
            &creds(),
            &dir("nodocker"),
        )
        .await;
        assert_eq!(
            r,
            Err(GatewayState::LoginFailed(
                "Docker is required for IBKR_TWS but is not running".into()
            ))
        );
        assert!(fake.runs.lock().unwrap().is_empty());
    }
}
