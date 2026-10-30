//! `oms update` — replace this binary with a released one.
//!
//! The same contract `install.sh` keeps, done from inside the binary: fetch the
//! platform's tarball and the release's `SHA256SUMS`, refuse on a checksum
//! mismatch, prove the new binary runs and reports the expected version, and
//! only then swap it in. The swap is a `rename` within the binary's own
//! directory, so it is atomic: an interrupted update leaves the old binary in
//! place, never half a new one. A running server keeps executing the old inode
//! until it is restarted.
//!
//! Deliberately not done here: migrating the database and restarting the
//! server. Starting the server never mutates a database (see `serve()`), and
//! `oms database migrate` is the one command that does — an update that ran it
//! behind the operator's back would break that rule. Both are printed as the
//! next steps instead.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

const REPO: &str = "maxkuttner/openoms";

pub struct Args {
    /// Release tag to install (`v0.3.0` or `0.3.0`); `None` means the latest.
    pub version: Option<String>,
    /// Report whether an update exists without installing it.
    pub check: bool,
}

type BoxError = Box<dyn std::error::Error>;

/// The release artifact name for the platform this binary was built for. Must
/// agree with the matrix in `.github/workflows/release.yml` and with
/// `detect_target` in `install.sh`.
fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else {
        None
    }
}

fn parse_version(tag: &str) -> Result<semver::Version, BoxError> {
    semver::Version::parse(tag.trim().trim_start_matches('v'))
        .map_err(|e| format!("not a release version: {tag:?} ({e})").into())
}

/// A binary under a cargo `target/` directory was built from a checkout;
/// overwriting it with a release would be undone by the next `cargo build`, and
/// silently running release code from a dev tree is worse. Point at git instead.
fn is_cargo_build(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts
        .windows(2)
        .any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release"))
}

/// A binary inside a Homebrew Cellar belongs to brew: replacing it would break
/// brew's own checksums and be undone by the next `brew upgrade`.
fn is_homebrew_install(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts.windows(2).any(|w| w[0] == "Cellar" && w[1] == "openoms")
}

/// Set by the published Docker image. A container's binary is replaced by
/// pulling a new image, never in place.
fn in_container() -> bool {
    crate::setup::init::rules::container_mode(std::env::var("OMS_CONTAINER").ok().as_deref())
}

/// Why this binary must not replace itself, naming what updates it instead.
fn managed_refusal(exe: &Path, in_container: bool) -> Option<String> {
    if in_container {
        Some("oms runs in a container — update it with: docker compose pull && docker compose run --rm oms database migrate && docker compose up -d".into())
    } else if is_homebrew_install(exe) {
        Some("oms was installed by Homebrew — update it with: brew upgrade openoms".into())
    } else if is_cargo_build(exe) {
        Some(format!(
            "{} was built from source — update it with `git pull && cargo build --release` instead",
            exe.display()
        ))
    } else {
        None
    }
}

/// `--check` compares tags only; installing needs this platform's tarball.
fn needs_target(check: bool) -> bool {
    !check
}

/// `--check` only reads, so it reports whoever owns the binary.
fn update_refusal(exe: &Path, in_container: bool, check: bool) -> Option<String> {
    if check { None } else { managed_refusal(exe, in_container) }
}

/// The expected hash for `file` in a `sha256sum`-format listing. Accepts the
/// binary-mode `*name` marker too, since `shasum` may write it.
fn expected_sha256<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        (name == file).then_some(hash)
    })
}

/// Pull the `oms` entry out of a release tarball.
fn extract_binary(tarball: &[u8]) -> Result<Vec<u8>, BoxError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_os_str() == "oms" {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
    }
    Err("the release tarball does not contain an oms binary".into())
}

fn http_client() -> Result<reqwest::Client, BoxError> {
    // api.github.com rejects requests without a User-Agent.
    Ok(reqwest::Client::builder()
        .user_agent(concat!("oms/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, BoxError> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(format!("download failed: {url} ({})", resp.status()).into());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// The tag of the newest published release, from `{api_base}/repos/{REPO}/releases/latest`.
async fn latest_tag(client: &reqwest::Client, api_base: &str) -> Result<String, BoxError> {
    let url = format!("{api_base}/repos/{REPO}/releases/latest");
    let resp = client.get(&url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("no release of {REPO} has been published yet").into());
    }
    if !resp.status().is_success() {
        return Err(format!("could not look up the latest release: {url} ({})", resp.status()).into());
    }
    let body: serde_json::Value = resp.json().await?;
    body["tag_name"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{url} returned no tag_name").into())
}

/// Download `oms-{target}.tar.gz` from `base`, verify it, and atomically
/// replace `exe` with the binary inside — but only once that binary has run and
/// reported `expected`.
async fn install(
    client: &reqwest::Client,
    base: &str,
    target: &str,
    expected: &semver::Version,
    exe: &Path,
) -> Result<(), BoxError> {
    let tarball_name = format!("oms-{target}.tar.gz");
    let tarball = fetch(client, &format!("{base}/{tarball_name}")).await?;
    let sums = String::from_utf8(fetch(client, &format!("{base}/SHA256SUMS")).await?)?;

    let expected_hash = expected_sha256(&sums, &tarball_name)
        .ok_or_else(|| format!("{tarball_name} is not listed in SHA256SUMS"))?;
    let actual_hash = hex(&Sha256::digest(&tarball));
    if !expected_hash.eq_ignore_ascii_case(&actual_hash) {
        return Err(format!(
            "checksum mismatch for {tarball_name}\n  expected {expected_hash}\n  got      {actual_hash}"
        )
        .into());
    }

    let binary = extract_binary(&tarball)?;

    // Staged next to the target so the final rename never crosses a filesystem.
    let dir = exe.parent().ok_or("the oms binary has no parent directory")?;
    let staged = dir.join(format!(".oms-update-{}", std::process::id()));
    let result = stage_and_swap(&binary, &staged, expected, exe);
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

fn stage_and_swap(
    binary: &[u8],
    staged: &Path,
    expected: &semver::Version,
    exe: &Path,
) -> Result<(), BoxError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(staged, binary).map_err(|e| {
        format!(
            "cannot write to {}: {e} — rerun with permission to replace {}",
            staged.parent().unwrap_or(staged).display(),
            exe.display()
        )
    })?;
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))?;

    let out = std::process::Command::new(staged)
        .arg("--version")
        .output()
        .map_err(|e| format!("the downloaded binary will not run: {e}"))?;
    let reported = String::from_utf8_lossy(&out.stdout);
    let reported = reported.split_whitespace().last().unwrap_or("");
    if !out.status.success() || reported != expected.to_string() {
        return Err(format!(
            "the downloaded binary reports {reported:?}, expected {expected}; nothing was replaced"
        )
        .into());
    }

    std::fs::rename(staged, exe)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn run(args: Args) -> Result<(), BoxError> {
    let current = parse_version(env!("CARGO_PKG_VERSION"))?;
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    if let Some(refusal) = update_refusal(&exe, in_container(), args.check) {
        return Err(refusal.into());
    }
    // An arm64 container has no release tarball but can still --check.
    let target = match (needs_target(args.check), target()) {
        (true, None) => return Err("no prebuilt release exists for this platform".into()),
        (_, t) => t,
    };
    let client = http_client()?;

    let tag = match &args.version {
        Some(v) => format!("v{}", v.trim_start_matches('v')),
        None => latest_tag(&client, "https://api.github.com").await?,
    };
    let wanted = parse_version(&tag)?;

    // An explicit --version may go backwards (a rollback); "latest" never does.
    let up_to_date = if args.version.is_some() { wanted == current } else { wanted <= current };
    if up_to_date {
        println!("oms {current} is up to date");
        return Ok(());
    }
    if args.check {
        println!("oms {wanted} is available (installed: {current}) — run `oms update`");
        return Ok(());
    }

    let target = target.expect("resolved above whenever not --check");
    println!("updating oms {current} → {wanted} ({target})");
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    install(&client, &base, target, &wanted, &exe).await?;

    println!("checksum ok; replaced {}", exe.display());
    println!(
        "\nnext:\n\n    oms database migrate   # applies any new migrations — the server never migrates on its own\n\nthen restart the running server so it picks up {wanted}."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn versions_parse_with_or_without_the_v() {
        assert_eq!(parse_version("v0.2.0").unwrap(), parse_version("0.2.0").unwrap());
        assert!(parse_version("latest").is_err());
    }

    #[test]
    fn a_cargo_build_is_recognised() {
        assert!(is_cargo_build(Path::new("/src/oms/target/release/oms")));
        assert!(is_cargo_build(Path::new("/src/oms/target/debug/oms")));
        assert!(!is_cargo_build(Path::new("/home/me/.local/bin/oms")));
        assert!(!is_cargo_build(Path::new("/opt/target/oms")));
    }

    #[test]
    fn a_homebrew_install_is_refused_with_brew_upgrade() {
        let m = managed_refusal(Path::new("/opt/homebrew/Cellar/openoms/0.6.0/bin/oms"), false).unwrap();
        assert!(m.contains("brew upgrade openoms"), "{m}");
        assert!(managed_refusal(Path::new("/home/linuxbrew/.linuxbrew/Cellar/openoms/0.6.0/bin/oms"), false).is_some());
        assert!(managed_refusal(Path::new("/home/me/.local/bin/oms"), false).is_none());
        assert!(managed_refusal(Path::new("/opt/Cellar-backup/oms"), false).is_none());
    }

    #[test]
    fn a_container_install_is_refused_with_compose_pull() {
        let m = managed_refusal(Path::new("/usr/local/bin/oms"), true).unwrap();
        assert!(
            m.contains("docker compose pull && docker compose run --rm oms database migrate && docker compose up -d"),
            "the server never migrates on its own: {m}"
        );
    }

    #[test]
    fn a_cargo_build_is_still_refused_with_git_pull() {
        let m = managed_refusal(Path::new("/src/oms/target/release/oms"), false).unwrap();
        assert!(m.contains("git pull && cargo build --release"), "{m}");
    }

    #[test]
    fn check_needs_no_release_target() {
        // --check only compares tags; an arm64 container has no tarball target.
        assert!(needs_target(false));
        assert!(!needs_target(true));
    }

    #[test]
    fn check_still_reports_when_managed() {
        let cellar = Path::new("/opt/homebrew/Cellar/openoms/0.6.0/bin/oms");
        assert!(update_refusal(cellar, true, true).is_none(), "--check only reads");
        assert!(update_refusal(cellar, false, false).is_some());
    }

    #[test]
    fn checksums_are_found_by_exact_file_name() {
        let sums = "aaa  oms-aarch64-apple-darwin.tar.gz\nbbb *oms-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(expected_sha256(sums, "oms-aarch64-apple-darwin.tar.gz"), Some("aaa"));
        assert_eq!(expected_sha256(sums, "oms-x86_64-unknown-linux-gnu.tar.gz"), Some("bbb"));
        assert_eq!(expected_sha256(sums, "oms-aarch64-apple-darwin.tar"), None);
    }

    #[test]
    fn this_platform_has_a_release_target() {
        // The two platforms we build and develop on. A third would need a
        // release.yml matrix entry, an install.sh case and an arm here.
        assert!(target().is_some());
    }

    /// A tarball holding one `oms` entry: a shell script that prints `version`
    /// the way the real binary's `--version` does.
    fn fake_release(version: &str) -> Vec<u8> {
        let script = format!("#!/bin/sh\necho \"oms {version}\"\n");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "oms", script.as_bytes()).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// Serves `tarball` and `sums` the way a GitHub release download URL does.
    async fn serve_release(tarball: Vec<u8>, sums: String) -> String {
        use axum::{routing::get, Router};
        let app = Router::new()
            .route("/oms-test-target.tar.gz", get(move || async move { tarball }))
            .route("/SHA256SUMS", get(move || async move { sums }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn installed(dir: &Path) -> PathBuf {
        let exe = dir.join("oms");
        std::fs::write(&exe, "old binary").unwrap();
        exe
    }

    #[tokio::test]
    async fn a_verified_release_replaces_the_binary() {
        let tarball = fake_release("9.9.9");
        let sums = format!("{}  oms-test-target.tar.gz\n", hex(&Sha256::digest(&tarball)));
        let base = serve_release(tarball, sums).await;
        let dir = tempfile::tempdir().unwrap();
        let exe = installed(dir.path());

        install(&http_client().unwrap(), &base, "test-target", &parse_version("9.9.9").unwrap(), &exe)
            .await
            .unwrap();

        let out = std::process::Command::new(&exe).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "oms 9.9.9");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "staging file left behind");
    }

    #[tokio::test]
    async fn a_checksum_mismatch_leaves_the_binary_alone() {
        let base = serve_release(fake_release("9.9.9"), "deadbeef  oms-test-target.tar.gz\n".into()).await;
        let dir = tempfile::tempdir().unwrap();
        let exe = installed(dir.path());

        let err = install(&http_client().unwrap(), &base, "test-target", &parse_version("9.9.9").unwrap(), &exe)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_binary_reporting_the_wrong_version_is_not_swapped_in() {
        let tarball = fake_release("1.0.0");
        let sums = format!("{}  oms-test-target.tar.gz\n", hex(&Sha256::digest(&tarball)));
        let base = serve_release(tarball, sums).await;
        let dir = tempfile::tempdir().unwrap();
        let exe = installed(dir.path());

        let err = install(&http_client().unwrap(), &base, "test-target", &parse_version("9.9.9").unwrap(), &exe)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("nothing was replaced"), "{err}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old binary");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "staging file left behind");
    }
}
