//! The decisions `oms init` makes, kept free of I/O so each one is tested
//! directly: which database source to offer first, what counts as an
//! acceptable admin password, and what a listen address may look like.

/// Whether `oms init` can start PostgreSQL in Docker on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerStatus {
    Usable,
    NotFound,
    NotRunning,
}

/// Where the OMS database lives: a PostgreSQL `oms init` starts in Docker, or
/// one the operator already runs.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbSource {
    Docker,
    Existing,
}

pub fn default_db_source(s: DockerStatus) -> DbSource {
    match s {
        DockerStatus::Usable => DbSource::Docker,
        DockerStatus::NotFound | DockerStatus::NotRunning => DbSource::Existing,
    }
}

/// Long enough that a cockpit reachable from a network is not guessed; the
/// generated one is 32 characters.
pub const ADMIN_PASSWORD_MIN: usize = 12;

pub fn check_admin_password(pw: &str) -> Result<(), String> {
    if pw.chars().count() < ADMIN_PASSWORD_MIN {
        return Err(format!("the admin password needs at least {ADMIN_PASSWORD_MIN} characters"));
    }
    Ok(())
}

/// `host:port`, as `server.bind_addr` takes it. The host may be a name
/// (`localhost`, `oms.internal`), an IPv4 address, or a bracketed IPv6 one.
/// Returns the trimmed value.
pub fn parse_bind(s: &str) -> Result<String, String> {
    let s = s.trim();
    let bad = || format!("{s:?} is not host:port, e.g. localhost:3001 or 0.0.0.0:3001");
    let (host, port) = s.rsplit_once(':').ok_or_else(bad)?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(bad());
    }
    let ok_host = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    };
    if !ok_host {
        return Err(bad());
    }
    Ok(s.to_string())
}

/// One line when `bind` makes the cockpit and trading API reachable from other
/// machines; `None` for loopback.
pub fn exposure_warning(bind: &str) -> Option<String> {
    let host = bind.rsplit_once(':').map_or(bind, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost"
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    (!loopback).then(|| {
        format!(
            "warning: listening on {bind} makes the cockpit and trading API reachable from the network — \
             prefer an SSH tunnel, Tailscale, or a reverse proxy with TLS"
        )
    })
}

pub fn docker_status_note(s: DockerStatus) -> &'static str {
    match s {
        DockerStatus::Usable => "Docker found",
        DockerStatus::NotFound => "Docker not found",
        DockerStatus::NotRunning => "Docker not running",
    }
}

pub fn docker_fix_hint(s: DockerStatus) -> Option<&'static str> {
    match s {
        DockerStatus::Usable => None,
        DockerStatus::NotFound => Some(
            "Docker is not installed. Install Docker Desktop (macOS) or Docker Engine (Linux), \
             or choose an existing PostgreSQL 16.",
        ),
        DockerStatus::NotRunning => Some(
            "`docker info` failed: start the docker daemon, or add your user to the `docker` \
             group (then log in again), or choose an existing PostgreSQL 16.",
        ),
    }
}

/// `OMS_CONTAINER=1`, set by the published Docker image: compose supplies the
/// database and publishes the port, so `oms init` asks only the admin password
/// and `oms update` defers to `docker compose pull`.
pub fn container_mode(var: Option<&str>) -> bool {
    var == Some("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_mode_is_only_an_exact_1() {
        assert!(container_mode(Some("1")));
        assert!(!container_mode(None));
        assert!(!container_mode(Some("0")));
        assert!(!container_mode(Some("")));
    }

    #[test]
    fn docker_is_the_default_only_when_usable() {
        assert_eq!(default_db_source(DockerStatus::Usable), DbSource::Docker);
        assert_eq!(default_db_source(DockerStatus::NotFound), DbSource::Existing);
        assert_eq!(default_db_source(DockerStatus::NotRunning), DbSource::Existing);
    }

    #[test]
    fn admin_password_needs_twelve_chars() {
        assert!(check_admin_password("12345678901").is_err());
        assert!(check_admin_password("123456789012").is_ok());
    }

    #[test]
    fn bind_accepts_host_port_forms() {
        for ok in ["localhost:3001", "0.0.0.0:3001", "127.0.0.1:8080", "[::1]:3001", "oms.internal:3001", " localhost:3001 "] {
            assert!(parse_bind(ok).is_ok(), "{ok}");
        }
        assert_eq!(parse_bind(" localhost:3001 ").unwrap(), "localhost:3001");
    }

    #[test]
    fn bind_rejects_missing_host() {
        for bad in [":3001", "3001", "localhost", "localhost:", "localhost:99999", ""] {
            assert!(parse_bind(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_non_loopback_binds_warn() {
        for quiet in ["localhost:3001", "127.0.0.1:3001", "[::1]:3001"] {
            assert!(exposure_warning(quiet).is_none(), "{quiet}");
        }
        let w = exposure_warning("0.0.0.0:3001").unwrap();
        assert!(w.contains("network") && w.contains("Tailscale"), "{w}");
    }

    #[test]
    fn every_unusable_docker_state_has_a_fix_hint() {
        assert!(docker_fix_hint(DockerStatus::Usable).is_none());
        assert!(docker_fix_hint(DockerStatus::NotFound).is_some());
        assert!(docker_fix_hint(DockerStatus::NotRunning).unwrap().contains("docker"));
    }
}
