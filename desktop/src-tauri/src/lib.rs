// The trader desktop shell. Shows the static connection page in
// ../ui/index.html until a server address has been validated, probed and
// saved, then navigates the window to that server's trade app. On launch,
// a previously saved address sends the window straight there.

mod server;
mod store;

use tauri::menu::{Menu, MenuItem, Submenu};
use tauri::{Manager, Runtime, Url};

/// Id of the "Change server..." menu item, matched in `on_menu_event`.
const CHANGE_SERVER_MENU_ID: &str = "change-server";

/// The origin the bundled connection page was actually served from at
/// launch — `tauri://localhost/index.html` in a release build, or whatever
/// `build.devUrl` points at under `cargo tauri dev`. Captured once in
/// `setup`, before any startup navigation, and managed as app state so the
/// "Change server…" menu handler has a real place to go back to instead of
/// a hardcoded guess.
struct ConnectionPage(Url);

/// Validate, probe and (only then) persist a server address, then navigate
/// the main window to its trade app. Never stores a URL that has not
/// already probed successfully.
///
/// Generic over `Runtime` (rather than the concrete Wry-backed
/// `tauri::AppHandle`) so it can be registered on `tauri::test::MockRuntime`
/// too — see the `the_remote_page_cannot_invoke_connect` test below.
#[tauri::command]
async fn connect<R: Runtime>(app: tauri::AppHandle<R>, url: String) -> Result<(), String> {
    let normalised = server::normalise(&url).map_err(|e| e.message().to_string())?;

    let http = server::ReqwestProbe::new();
    server::probe(&normalised, &http)
        .await
        .map_err(|e| e.message().to_string())?;

    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("Couldn't find a place to save settings: {e}"))?;
    store::save(&config_dir, &normalised)
        .map_err(|e| format!("Couldn't save the server address: {e}"))?;

    navigate_to_trade_app(&app, &normalised)
}

/// Return the saved server address, if any, with no probe and no
/// re-validation beyond what `startup_target` already applies at launch.
/// Used by the connection page to prefill its field and decide whether to
/// show a "Cancel" control, so two windows against different servers are
/// distinguishable instead of always presenting a blank field.
///
/// A command added here must ALSO be added to `AppManifest::commands` in
/// build.rs and granted in `capabilities/default.json` — miss either and it
/// still builds, then fails at runtime with "Command <name> not allowed by
/// ACL".
#[tauri::command]
fn stored_server<R: Runtime>(app: tauri::AppHandle<R>) -> Option<String> {
    let config_dir = app.path().app_config_dir().ok()?;
    store::load(&config_dir)
}

/// Navigate the main window to `{url}/trade/`, and set the window title to
/// `openOMS Trade — {host}` so two windows against different servers (prod
/// vs. UAT) are distinguishable.
fn navigate_to_trade_app<R: Runtime>(app: &tauri::AppHandle<R>, url: &str) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Internal error: no main window".to_string())?;
    let target = format!("{url}/trade/");
    let target_url = Url::parse(&target).map_err(|e| format!("Invalid server address: {e}"))?;
    window
        .navigate(target_url.clone())
        .map_err(|e| format!("Couldn't open the trade app: {e}"))?;
    if let Some(host) = target_url.host_str() {
        window
            .set_title(&format!("openOMS Trade — {host}"))
            .map_err(|e| format!("Couldn't set the window title: {e}"))?;
    }
    Ok(())
}

/// Navigate the main window back to the bundled connection page. Used by
/// the "Change server..." menu item so a trader who typed a wrong-but-
/// reachable address, or simply wants to point at a different server, has a
/// way back in without deleting the saved `server.json` by hand.
fn go_to_connection_page(app: &tauri::AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Internal error: no main window".to_string())?;
    let connection_page = app.state::<ConnectionPage>();
    window
        .navigate(connection_page.0.clone())
        .map_err(|e| format!("Couldn't return to the connection page: {e}"))
}

/// Decide what a saved address is worth navigating to at launch. Re-runs
/// `normalise` over whatever `store::load` returned, so a hand-edited or
/// otherwise externally written `server.json` — valid JSON, a parseable but
/// no longer acceptable URL, another host, a `file://` path — fails closed
/// into `None` rather than being trusted the way `store::save` already
/// trusted it once, at connect time.
fn startup_target(stored: Option<String>) -> Option<String> {
    stored.and_then(|url| server::normalise(&url).ok())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // A command added here must also be added to `AppManifest::commands`
        // in build.rs and granted in `capabilities/default.json` — miss
        // either and it still builds, then fails at runtime with
        // "Command <name> not allowed by ACL".
        .invoke_handler(tauri::generate_handler![connect, stored_server])
        .menu(|handle| {
            // Keep the platform's standard menu (Quit, Edit, Window, ...)
            // and add one "Server" submenu on top of it, rather than
            // replacing the whole menu bar just to add one item.
            let menu = Menu::default(handle)?;
            menu.append(&Submenu::with_items(
                handle,
                "Server",
                true,
                &[&MenuItem::with_id(
                    handle,
                    CHANGE_SERVER_MENU_ID,
                    "Change server…",
                    true,
                    None::<&str>,
                )?],
            )?)?;
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id() == CHANGE_SERVER_MENU_ID {
                let _ = go_to_connection_page(app);
            }
        })
        .setup(|app| {
            // Capture the connection page's real URL before any startup
            // navigation below might replace it, so "Change server…" has
            // somewhere correct to go back to no matter how this build is
            // serving local assets (bundled `tauri://` in release, or
            // `build.devUrl` under `cargo tauri dev`).
            let window = app
                .get_webview_window("main")
                .ok_or("no \"main\" window at setup")?;
            app.manage(ConnectionPage(window.url()?));

            // A previously saved address that still passes `normalise`
            // sends the window straight to the trade app; otherwise the
            // bundled connection page (already loaded) is left showing.
            if let Ok(config_dir) = app.path().app_config_dir() {
                if let Some(url) = startup_target(store::load(&config_dir)) {
                    let _ = navigate_to_trade_app(app.handle(), &url);
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the openOMS trader desktop application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_stored_leaves_the_connection_page_showing() {
        assert_eq!(startup_target(None), None);
    }

    #[test]
    fn a_stored_value_that_still_passes_normalise_is_navigated_to() {
        assert_eq!(startup_target(Some("https://host/".to_string())), Some("https://host".to_string()));
    }

    #[test]
    fn a_stored_value_failing_normalise_is_not_navigated_to() {
        // A hand-edited server.json can hold valid JSON with a URL that no
        // longer passes normalise's stricter rules, or was never valid: a
        // query string, another host via userinfo, or a file:// path.
        // Launch must fail closed rather than navigate anyway.
        assert_eq!(startup_target(Some("https://host/?a=b".to_string())), None);
        assert_eq!(startup_target(Some("file:///etc/passwd".to_string())), None);
        assert_eq!(startup_target(Some("not a url at all".to_string())), None);
    }

    /// The permanent record of the reviewer's throwaway proof: this app's
    /// central security property is that only the bundled local connection
    /// page can invoke `connect`, never whatever remote `/trade/` page the
    /// window later navigates to. It also catches the failure mode where a
    /// command is added to `generate_handler!` but not to `build.rs`'s
    /// `AppManifest::commands` or to `capabilities/default.json` — that
    /// compiles and runs, then refuses the command for every origin,
    /// local included.
    #[test]
    fn the_remote_page_cannot_invoke_connect() {
        use tauri::ipc::{CallbackFn, InvokeBody};
        use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
        use tauri::webview::InvokeRequest;
        use tauri::WebviewWindowBuilder;

        fn request(origin: &str, body: serde_json::Value) -> InvokeRequest {
            InvokeRequest {
                cmd: "connect".into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: origin.parse().expect("test origin must parse as a URL"),
                body: InvokeBody::Json(body),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            }
        }

        // Build against this crate's real `tauri.conf.json` and
        // `capabilities/default.json` (via `generate_context!()`), on the
        // `MockRuntime` (via `mock_builder()`), so the ACL enforced here is
        // the one that ships, without needing a GUI. `test = true` only
        // skips codegen that cannot coexist with the real `generate_context!()`
        // already expanded in `run()` above within the same binary (macOS's
        // Info.plist embed uses a single fixed, non-mangled symbol name) —
        // it does not touch capabilities or ACL resolution.
        let app = mock_builder()
            .invoke_handler(tauri::generate_handler![connect])
            .build(tauri::generate_context!(test = true))
            .expect("the app builds against its real config under the mock runtime");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("a window labelled \"main\" is what capabilities/default.json grants");

        // Local origin: `capabilities/default.json` grants `allow-connect`
        // to the local page, so this gets past the ACL and reaches
        // `connect`'s own logic, which then fails on its own terms (an
        // unparseable "url" argument). Assert on that exact message
        // (`ConnectError::BadScheme`, positively) rather than only the
        // absence of an ACL-refusal string — a missing string would also
        // pass against an everything-denied configuration, proving nothing
        // about whether dispatch actually happened.
        let local_origin = if cfg!(any(windows, target_os = "android")) {
            "http://tauri.localhost"
        } else {
            "tauri://localhost"
        };
        let local_err = get_ipc_response(
            &webview,
            request(local_origin, serde_json::json!({ "url": "not a url" })),
        )
        .expect_err("connect(\"not a url\") always fails, just not by ACL refusal");
        let local_message = local_err.as_str().unwrap_or_default();
        assert_eq!(
            local_message,
            crate::server::ConnectError::BadScheme.message(),
            "expected connect's own BadScheme message, meaning it was actually dispatched, got: {local_message}"
        );

        // Remote origin: there is no `remote` block in
        // `capabilities/default.json`, so nothing extends `allow-connect`
        // to any server address the window might later navigate to. This
        // is the property the whole shell's security model rests on.
        //
        // This assertion is on a Tauri debug-build ACL-refusal string
        // ("not allowed on window ..."), which is not a stable, documented
        // API — it is the only signal `tauri::test` exposes for "refused by
        // ACL" versus any other failure, so it is kept, but a future Tauri
        // upgrade could change its wording.
        let remote_err = get_ipc_response(
            &webview,
            request(
                "https://evil.example.com/trade/",
                serde_json::json!({ "url": "https://oms.example.com" }),
            ),
        )
        .expect_err("a remote page must never be able to invoke connect");
        let remote_message = remote_err.as_str().unwrap_or_default();
        assert!(
            remote_message.contains("not allowed on window \"main\""),
            "expected an ACL refusal naming window \"main\", got: {remote_message}"
        );
    }
}
