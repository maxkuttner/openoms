//! The single-page app (trading and the Admin Panel), compiled into the binary.
//!
//! `include_dir!` rather than serving a directory off disk, so a released `oms`
//! serves its own UI from wherever it was installed — no checkout, no npm, no
//! second process. Mirrors how `setup/database/assets.rs` carries the migrations.
//!
//! Unlike the migrations embed, an *empty* embed here is legitimate: it is what
//! every source build produces, because `clients/web/dist` is a gitignored build
//! output. So there is no release assert — the routes explain where the dev UI is.

use axum::{
    extract::Path as UrlPath,
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use include_dir::{include_dir, Dir};

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/clients/web/dist");

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

/// The app's HTML shell.
const SHELL: &str = "index.html";

/// Resolve a path *relative to* the app's mount point to an embedded file.
///
/// A miss on a path with no file extension is a client-side route such as
/// `/app/positions`: only the SPA shell can answer it, so the shell is what a
/// reload of that URL must return. A miss on a path *with* an extension is a
/// genuinely absent file and stays a 404 — serving HTML there would hand the
/// browser an index.html labelled `application/javascript`.
pub fn asset_for(path: &str) -> Option<Asset> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { SHELL } else { rel };

    if let Some(file) = DIST.get_file(rel) {
        let cache_control = if rel.starts_with("assets/") { IMMUTABLE } else { NO_CACHE };
        return Some(Asset {
            body: file.contents(),
            content_type: mime_guess::from_path(rel).first_or_octet_stream().to_string(),
            cache_control,
        });
    }

    // A miss with no extension is a client-side route; only the shell can answer it.
    if std::path::Path::new(rel).extension().is_none() {
        let shell_file = DIST.get_file(SHELL)?;
        return Some(Asset {
            body: shell_file.contents(),
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

async fn redirect_to_slash() -> Redirect {
    Redirect::temporary("/app/")
}

async fn index() -> Response {
    respond("")
}

async fn asset(UrlPath(path): UrlPath<String>) -> Response {
    respond(&path)
}

/// Mounted outside the auth layers on purpose: the SPA shell has to load
/// before there is a session, and these routes carry no data — only the
/// static bundle. The sign-in page is part of the app, and with no sign-in
/// method enabled it says so (from `/auth/methods`). Authentication happens
/// where it already did, on the API routes.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        // Someone opening `localhost:3001` lands in the app, not on a 404.
        .route("/", get(redirect_to_slash))
        // `base: '/ui/'` in vite.config.ts puts every generated asset URL here.
        .route("/ui/*path", get(asset))
        // axum's `*path` wildcard needs at least one character after the slash, so
        // `/app/` itself gets its own route.
        .route("/app", get(redirect_to_slash))
        .route("/app/", get(index))
        .route("/app/*path", get(asset))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Whether the embed has anything in it depends on whether `npm run build` ran
    // before `cargo build`. Both are legitimate: a contributor's source build has an
    // empty embed, CI and every release build a populated one. So each test asserts
    // the behaviour of the build it is actually running in, and CI runs both.

    /// Reads a response body to bytes for equality checks in tests below.
    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body readable")
            .to_vec()
    }

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

    #[tokio::test]
    async fn the_bare_path_redirects_to_the_trailing_slash() {
        // The router's basename is /app/; without the trailing slash it would not match.
        let res = redirect_to_slash().await.into_response();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers()[header::LOCATION], "/app/");
    }

    #[tokio::test]
    async fn the_index_route_serves_the_shell_or_explains_itself() {
        let res = index().await;
        let expected = if is_bundled() { StatusCode::OK } else { StatusCode::NOT_FOUND };
        assert_eq!(res.status(), expected);
    }

    #[tokio::test]
    async fn the_wildcard_route_resolves_a_client_side_route() {
        let res = asset(axum::extract::Path("orders".to_string())).await;
        let expected = if is_bundled() { StatusCode::OK } else { StatusCode::NOT_FOUND };
        assert_eq!(res.status(), expected);
    }

    // The tests above call the handlers directly, which is cheap but bypasses
    // axum's actual path matching — it would not catch an overlap between the
    // exact `/app/` route and the `/app/*path` wildcard. This test
    // binds a real listener and drives it with a real HTTP client so the route
    // table itself — not just the handler bodies — is under test.
    #[tokio::test]
    async fn the_router_dispatches_through_axums_real_route_table() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router::<()>().with_state(());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let base = format!("http://{addr}");
        let expected = if is_bundled() { reqwest::StatusCode::OK } else { reqwest::StatusCode::NOT_FOUND };

        // 0. The site root sends a browser to the app rather than a 404.
        let res = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers()[reqwest::header::LOCATION], "/app/");

        // 1. The bare path redirects to the trailing slash.
        let res = client.get(format!("{base}/app")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers()[reqwest::header::LOCATION], "/app/");

        // 2. The exact-match `/app/` route wins over the `/app/*path` wildcard.
        let res = client.get(format!("{base}/app/")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        // 3. A reload on a deep link serves the shell; the router takes it from there.
        let res = client.get(format!("{base}/app/admin/broker-connections")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        // 4. An extensioned miss is a genuine 404 in both build states.
        let res = client.get(format!("{base}/app/assets/nope-00000000.js")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);

        // 5. The asset tree: `base: '/ui/'` puts every generated asset URL here.
        let res = client.get(format!("{base}/ui/assets/nope-00000000.js")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
        let res = client.get(format!("{base}/ui/some-route")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        // 6. The two old apps are gone, not redirected (prerelease, no shims).
        for old in ["/cockpit", "/cockpit/", "/cockpit/orders", "/trade", "/trade/", "/trade/positions"] {
            let res = client.get(format!("{base}{old}")).send().await.unwrap();
            assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND, "{old} must be gone");
        }
    }
}
