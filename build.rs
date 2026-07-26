//! Build script.
//!
//! The `quickfix` crate is built with `build-with-ssl`, so the final link pulls in
//! `-lssl -lcrypto` (OpenSSL). macOS ships no OpenSSL in the default linker search
//! path, so locate a Homebrew (or env-provided) OpenSSL and add its `lib` dir. On
//! Linux the system OpenSSL is already on the default path, so this is a no-op.

use std::path::Path;
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

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
        let lib = format!("{root}/lib");
        if Path::new(&lib).is_dir() {
            println!("cargo:rustc-link-search=native={lib}");
            return;
        }
    }

    println!(
        "cargo:warning=OpenSSL lib dir not found; the quickfix SSL link may fail. \
         Set OPENSSL_DIR or `brew install openssl@3`."
    );
}
