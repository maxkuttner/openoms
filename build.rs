//! Build script.
//!
//! The `quickfix` crate is built with `build-with-ssl`, so the final link pulls in
//! `-lssl -lcrypto` (OpenSSL). macOS ships no OpenSSL in the default linker search
//! path, so locate a Homebrew (or env-provided) OpenSSL and add its `lib` dir. On
//! Linux the system OpenSSL is already on the default path, so this is a no-op.
//!
//! `OMS_OPENSSL_STATIC=1` switches the macOS link from dynamic to static. A
//! dynamically linked binary records `/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`
//! as an absolute load path, so it only runs on a Mac that has that exact Homebrew
//! OpenSSL — which a machine running `curl … | sh` generally does not. Release
//! builds set the variable (see `.github/workflows/release.yml`) so the published
//! binary carries no `/opt/homebrew` reference; ordinary `cargo build` leaves it
//! unset and links dynamically as before.
//!
//! The mechanism: Apple's linker prefers `.dylib` over `.a` when both sit in the
//! same directory, and the Homebrew prefix holds both. So instead of pointing the
//! link search at the prefix, copy just `libssl.a` and `libcrypto.a` into a private
//! directory under `OUT_DIR` and point at that. Only the archives are reachable, so
//! the archives win.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Set to a non-empty value to link OpenSSL statically on macOS (see module docs).
const STATIC_ENV: &str = "OMS_OPENSSL_STATIC";

fn main() {
    ensure_cockpit_dist();

    println!("cargo:rerun-if-env-changed={STATIC_ENV}");
    println!("cargo:rerun-if-env-changed=OPENSSL_DIR");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let want_static = std::env::var(STATIC_ENV).is_ok_and(|v| !v.is_empty());

    // Precedence: OPENSSL_DIR env → `brew --prefix openssl@3` → common install dirs.
    let mut roots: Vec<String> = Vec::new();
    if let Ok(dir) = std::env::var("OPENSSL_DIR") {
        roots.push(dir);
    }
    if let Ok(out) = Command::new("brew").args(["--prefix", "openssl@3"]).output() {
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                roots.push(s.trim().to_string());
            }
        }
    }
    roots.push("/opt/homebrew/opt/openssl@3".to_string());
    roots.push("/usr/local/opt/openssl@3".to_string());

    for root in roots {
        let lib = PathBuf::from(format!("{root}/lib"));
        if !lib.is_dir() {
            continue;
        }
        let search = if want_static {
            archives_only_dir(&lib)
        } else {
            lib
        };
        println!("cargo:rustc-link-search=native={}", search.display());
        return;
    }

    println!(
        "cargo:warning=OpenSSL lib dir not found; the quickfix SSL link may fail. \
         Set OPENSSL_DIR or `brew install openssl@3`."
    );
}

/// Copy `libssl.a`/`libcrypto.a` out of `lib` into a directory holding nothing else,
/// and return it. Linking against that directory picks the static archives, because
/// the `.dylib`s the linker would otherwise prefer are not in it.
fn archives_only_dir(lib: &Path) -> PathBuf {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("openssl-static");
    std::fs::create_dir_all(&out).expect("create openssl-static dir");

    for archive in ["libssl.a", "libcrypto.a"] {
        let src = lib.join(archive);
        assert!(
            src.is_file(),
            "{STATIC_ENV} is set but {} is missing — install an OpenSSL that ships \
             static archives (Homebrew's openssl@3 does) or unset {STATIC_ENV}",
            src.display()
        );
        // Homebrew ships the archives mode 0444, and `fs::copy` preserves that — so
        // a second build would fail to overwrite its own output. Clear it first.
        let dst = out.join(archive);
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
        println!("cargo:rerun-if-changed={}", src.display());
    }
    out
}

/// `cockpit/dist` is a build output and is gitignored, but `include_dir!` on a
/// missing directory fails to compile — so a fresh clone would not build until
/// someone ran npm. Create it empty instead. An empty embed is the normal state of
/// a source build and is handled, not an error (see `src/cockpit.rs`).
fn ensure_cockpit_dist() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest).join("cockpit").join("dist");
    std::fs::create_dir_all(&dist).expect("create cockpit/dist");
    println!("cargo:rerun-if-changed=cockpit/dist");
}
