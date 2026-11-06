//! Build script.
//!
//! The OpenSSL link setup for the FIX engine lives in `crates/gateway/build.rs`,
//! next to the `quickfix` dependency it serves.

use std::path::Path;

fn main() {
    ensure_cockpit_dist();
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
