//! The cockpit single-page app, compiled into the binary.
//!
//! `include_dir!` rather than serving a directory off disk, so a released `oms`
//! serves its own UI from wherever it was installed — no checkout, no npm, no
//! second process. Mirrors how `setup/database/assets.rs` carries the migrations.
//!
//! Unlike the migrations embed, an *empty* embed here is legitimate: it is what
//! every source build produces, because `cockpit/dist` is a gitignored build
//! output. So there is no release assert — the routes explain where the dev UI is.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use include_dir::{include_dir, Dir};

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/cockpit/dist");

/// Vite emits content-hashed asset filenames, so a given URL's bytes never change.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// `index.html` has a stable URL and changing contents — caching it hides upgrades.
const NO_CACHE: &str = "no-cache";

const NOT_BUNDLED: &str =
    "cockpit not bundled in this build — run `cd cockpit && npm run dev` for the dev UI on :5173\n";

pub struct Asset {
    pub body: &'static [u8],
    pub content_type: String,
    pub cache_control: &'static str,
}

/// Did this build embed a cockpit bundle?
pub fn is_bundled() -> bool {
    DIST.get_file("index.html").is_some()
}

/// Resolve a path *relative to* `/cockpit/` to an embedded file.
///
/// A miss on a path with no file extension is a client-side route such as
/// `/cockpit/orders`: only the SPA shell can answer it, so the shell is what a
/// reload of that URL must return. A miss on a path *with* an extension is a
/// genuinely absent file and stays a 404 — serving HTML there would hand the
/// browser an index.html labelled `application/javascript`.
pub fn asset_for(path: &str) -> Option<Asset> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    if let Some(file) = DIST.get_file(rel) {
        let cache_control = if rel.starts_with("assets/") { IMMUTABLE } else { NO_CACHE };
        return Some(Asset {
            body: file.contents(),
            content_type: mime_guess::from_path(rel).first_or_octet_stream().to_string(),
            cache_control,
        });
    }

    if std::path::Path::new(rel).extension().is_none() {
        let shell = DIST.get_file("index.html")?;
        return Some(Asset {
            body: shell.contents(),
            content_type: "text/html".to_string(),
            cache_control: NO_CACHE,
        });
    }

    None
}

pub fn respond(path: &str) -> Response {
    match asset_for(path) {
        Some(asset) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, asset.content_type),
                (header::CACHE_CONTROL, asset.cache_control.to_string()),
            ],
            asset.body,
        )
            .into_response(),
        None if !is_bundled() => (StatusCode::NOT_FOUND, NOT_BUNDLED).into_response(),
        None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Whether the embed has anything in it depends on whether `npm run build` ran
    // before `cargo build`. Both are legitimate: a contributor's source build has an
    // empty embed, CI and every release build a populated one. So each test asserts
    // the behaviour of the build it is actually running in, and CI runs both.

    #[test]
    fn a_source_build_says_where_the_dev_ui_is() {
        if is_bundled() {
            return;
        }
        assert!(asset_for("").is_none());
        let res = respond("");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_bundled_build_serves_the_shell() {
        if !is_bundled() {
            return;
        }
        let shell = asset_for("").expect("index.html");
        assert!(shell.content_type.starts_with("text/html"));
        assert_eq!(shell.cache_control, NO_CACHE);
        assert_eq!(respond("").status(), StatusCode::OK);
    }

    #[test]
    fn deep_links_fall_back_to_the_shell() {
        if !is_bundled() {
            return;
        }
        let shell = asset_for("").unwrap();
        let deep = asset_for("orders").expect("client-side route must serve the shell");
        assert_eq!(shell.body, deep.body);
    }

    #[test]
    fn a_missing_asset_stays_missing() {
        if !is_bundled() {
            return;
        }
        // Has an extension, so it is a real file request, not a client-side route.
        assert!(asset_for("assets/nope-00000000.js").is_none());
        assert_eq!(respond("assets/nope-00000000.js").status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn hashed_assets_are_cached_forever() {
        if !is_bundled() {
            return;
        }
        let first = DIST
            .get_dir("assets")
            .expect("vite emits an assets/ dir")
            .files()
            .next()
            .expect("at least one hashed asset");
        let name = first.path().to_string_lossy().to_string();
        assert_eq!(asset_for(&name).unwrap().cache_control, IMMUTABLE);
    }
}
