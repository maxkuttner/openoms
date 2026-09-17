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
    extract::Path as UrlPath,
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
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

/// Resolve a path *relative to* its app's mount point to an embedded file.
///
/// `shell` is the fallback HTML filename for the app this path belongs to
/// (`index.html` for `/cockpit/`, `trade.html` for `/trade/`) — both apps
/// share the same asset tree, but each must fall back to its own shell.
///
/// A miss on a path with no file extension is a client-side route such as
/// `/cockpit/orders`: only the SPA shell can answer it, so the shell is what a
/// reload of that URL must return. A miss on a path *with* an extension is a
/// genuinely absent file and stays a 404 — serving HTML there would hand the
/// browser an index.html labelled `application/javascript`.
pub fn asset_for(shell: &str, path: &str) -> Option<Asset> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { shell } else { rel };

    if let Some(file) = DIST.get_file(rel) {
        let cache_control = if rel.starts_with("assets/") { IMMUTABLE } else { NO_CACHE };
        return Some(Asset {
            body: file.contents(),
            content_type: mime_guess::from_path(rel).first_or_octet_stream().to_string(),
            cache_control,
        });
    }

    // A miss with no extension is a client-side route; only the shell can answer
    // it. Which shell depends on which app the URL belongs to.
    if std::path::Path::new(rel).extension().is_none() {
        let shell_file = DIST.get_file(shell)?;
        return Some(Asset {
            body: shell_file.contents(),
            content_type: "text/html".to_string(),
            cache_control: NO_CACHE,
        });
    }

    None
}

pub fn respond(shell: &str, path: &str) -> Response {
    match asset_for(shell, path) {
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
    Redirect::temporary("/cockpit/")
}

async fn index() -> Response {
    respond("index.html", "")
}

async fn asset(UrlPath(path): UrlPath<String>) -> Response {
    respond("index.html", &path)
}

/// The shared asset tree. Both bundles emit into `dist/assets/`, and Vite's
/// `base` points every generated URL at `/ui/`.
async fn ui_asset(UrlPath(path): UrlPath<String>) -> Response {
    respond("index.html", &path)
}

async fn redirect_to_trade_slash() -> Redirect {
    Redirect::temporary("/trade/")
}

async fn trade_index() -> Response {
    respond("trade.html", "")
}

async fn trade_asset(UrlPath(path): UrlPath<String>) -> Response {
    respond("trade.html", &path)
}

/// Mounted outside the admin auth layer on purpose: the SPA shell has to load
/// before there is a token to send, and these routes carry no data — only the
/// static bundle. Authentication happens where it already did, on `/admin/*`.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/ui/*path", get(ui_asset))
        // axum's `*path` wildcard needs at least one character after the slash, so
        // `/cockpit/` itself gets its own route.
        .route("/cockpit", get(redirect_to_slash))
        .route("/cockpit/", get(index))
        .route("/cockpit/*path", get(asset))
}

/// The trader app's shell and assets. Kept as a *separate* router from
/// `router()` so the caller can mount it only where the trade app can
/// actually be used — see `main.rs`, which merges this inside the same
/// `if oidc_settings.is_some()` gate as `/auth/me`: the trade app
/// authenticates by session cookie only, and a session can only ever be
/// minted by the OIDC callback, so with no `[auth.oidc]` configured `/trade/`
/// would otherwise be a screen nobody could ever log into.
pub fn trade_router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/trade", get(redirect_to_trade_slash))
        .route("/trade/", get(trade_index))
        .route("/trade/*path", get(trade_asset))
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
        assert!(asset_for("index.html", "").is_none());
        let res = respond("index.html", "");
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_bundled_build_serves_the_shell() {
        if !is_bundled() {
            return;
        }
        let shell = asset_for("index.html", "").expect("index.html");
        assert!(shell.content_type.starts_with("text/html"));
        assert_eq!(shell.cache_control, NO_CACHE);
        assert_eq!(respond("index.html", "").status(), StatusCode::OK);
    }

    #[test]
    fn deep_links_fall_back_to_the_shell() {
        if !is_bundled() {
            return;
        }
        let shell = asset_for("index.html", "").unwrap();
        let deep = asset_for("index.html", "orders").expect("client-side route must serve the shell");
        assert_eq!(shell.body, deep.body);
    }

    #[test]
    fn a_missing_asset_stays_missing() {
        if !is_bundled() {
            return;
        }
        // Has an extension, so it is a real file request, not a client-side route.
        assert!(asset_for("index.html", "assets/nope-00000000.js").is_none());
        assert_eq!(respond("index.html", "assets/nope-00000000.js").status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_trade_deep_link_falls_back_to_the_trade_shell_not_the_cockpit_one() {
        if !is_bundled() {
            return; // source build: nothing embedded to serve
        }
        if DIST.get_file("trade.html").is_none() {
            // Task 4 hasn't built trade.html yet — nothing to assert against.
            return;
        }
        let cockpit = respond("index.html", "orders");
        let trade = respond("trade.html", "orders");

        assert_eq!(cockpit.status(), StatusCode::OK);
        assert_eq!(trade.status(), StatusCode::OK);
        // Both are HTML shells, but they must not be the SAME shell — serving the
        // cockpit's bundle under /trade/ would load the admin app at the trader's
        // URL, with the admin token attached.
        assert_ne!(
            body_bytes(cockpit).await,
            body_bytes(trade).await,
            "each app must fall back to its own shell"
        );
    }

    #[test]
    fn a_missing_asset_stays_missing_for_either_shell() {
        if !is_bundled() {
            return;
        }
        if DIST.get_file("trade.html").is_none() {
            return;
        }
        assert_eq!(respond("trade.html", "assets/nope.js").status(), StatusCode::NOT_FOUND);
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
        assert_eq!(asset_for("index.html", &name).unwrap().cache_control, IMMUTABLE);
    }

    #[tokio::test]
    async fn the_bare_path_redirects_to_the_trailing_slash() {
        // The SPA's asset URLs are relative to /cockpit/ (vite `base`), so without
        // the trailing slash the browser resolves them one level too high.
        let res = redirect_to_slash().await.into_response();
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers()[header::LOCATION], "/cockpit/");
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
    // axum's actual path matching — it would not have caught an overlap between
    // the exact `/cockpit/` route and the `/cockpit/*path` wildcard, which is
    // exactly the axum-0.7 hazard `router()`'s doc comment calls out. This test
    // binds a real listener and drives it with a real HTTP client so the route
    // table itself — not just the handler bodies — is under test.
    #[tokio::test]
    async fn the_router_dispatches_through_axums_real_route_table() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Both routers, mounted together exactly as main.rs mounts them when
        // OIDC is configured: `/ui/*path`, `/cockpit*` and `/trade*` share one
        // route table, and an overlap between them would only ever show up here.
        let app = router::<()>().merge(trade_router::<()>()).with_state(());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let base = format!("http://{addr}");

        // 1. The bare path is reached and redirects to the trailing slash.
        let res = client.get(format!("{base}/cockpit")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers()[reqwest::header::LOCATION], "/cockpit/");

        let expected = if is_bundled() { reqwest::StatusCode::OK } else { reqwest::StatusCode::NOT_FOUND };

        // 2. The exact-match `/cockpit/` route wins over the `/cockpit/*path`
        // wildcard (it would also match an empty capture, if axum let it).
        let res = client.get(format!("{base}/cockpit/")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        // 3. The wildcard resolves an extensionless, client-side deep link.
        let res = client.get(format!("{base}/cockpit/orders")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        // 4. An extensioned miss is a genuine 404 in both build states, never the
        // shell fallback.
        let res = client
            .get(format!("{base}/cockpit/assets/nope-00000000.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);

        // 5. The shared asset tree. `base: '/ui/'` puts every generated asset
        // URL here, for BOTH bundles — if this route stopped dispatching, both
        // apps would load a shell with no JavaScript behind it.
        let res = client.get(format!("{base}/ui/assets/nope-00000000.js")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
        let res = client.get(format!("{base}/ui/some-route")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        // 6. The trade app's three routes, the same way round as the cockpit's:
        // bare path redirects, the exact `/trade/` route wins over the
        // `/trade/*path` wildcard, and a deep link falls back to the trade
        // shell.
        let res = client.get(format!("{base}/trade")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers()[reqwest::header::LOCATION], "/trade/");

        let res = client.get(format!("{base}/trade/")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        let res = client.get(format!("{base}/trade/positions")).send().await.unwrap();
        assert_eq!(res.status(), expected);

        let res = client
            .get(format!("{base}/trade/assets/nope-00000000.js"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);

        // And the two shells are genuinely different documents — a `/trade/`
        // that served the cockpit's index.html would load the admin app, with
        // its localStorage admin token, at the trader's URL. Only checkable in
        // a bundled build; in a source build both are the same 404 text.
        if is_bundled() {
            let cockpit_shell = client
                .get(format!("{base}/cockpit/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            let trade_shell = client
                .get(format!("{base}/trade/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_ne!(cockpit_shell, trade_shell);
        }
    }
}
