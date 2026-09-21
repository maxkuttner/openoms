// The trader desktop shell. Shows the static connection page in
// ../ui/index.html until a server address has been validated, probed and
// saved, then navigates the window to that server's trade app. On launch,
// a previously saved address sends the window straight there.

mod server;
mod store;

use tauri::menu::{Menu, MenuItem, Submenu};
use tauri::{Manager, Url};

/// Id of the "Change server..." menu item, matched in `on_menu_event`.
const CHANGE_SERVER_MENU_ID: &str = "change-server";

/// Validate, probe and (only then) persist a server address, then navigate
/// the main window to its trade app. Never stores a URL that has not
/// already probed successfully.
#[tauri::command]
async fn connect(app: tauri::AppHandle, url: String) -> Result<(), String> {
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

/// Navigate the main window to `{url}/trade/`.
fn navigate_to_trade_app(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Internal error: no main window".to_string())?;
    let target = format!("{url}/trade/");
    let target_url = Url::parse(&target).map_err(|e| format!("Invalid server address: {e}"))?;
    window
        .navigate(target_url)
        .map_err(|e| format!("Couldn't open the trade app: {e}"))
}

/// The `tauri://` origin the bundled connection page is served from.
///
/// Tauri has no public API to resolve this: the equivalent internal helper,
/// `AppManager::get_app_url`, is `pub(crate)` (confirmed by reading tauri
/// 2.11.5's `src/manager/mod.rs`). This mirrors it directly. Everywhere but
/// Windows and Android it is `tauri://localhost`; there, since a custom
/// `tauri://` scheme can't be registered, wry serves the same content over
/// `http://tauri.localhost` instead (`https://` only if `useHttpsScheme` is
/// set in `tauri.conf.json`, which this app leaves unset).
fn local_page_url() -> Url {
    let origin = if cfg!(windows) {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    };
    Url::parse(&format!("{origin}/index.html")).expect("hardcoded local page URL must parse")
}

/// Navigate the main window back to the bundled connection page. Used by
/// the "Change server..." menu item so a trader who typed a wrong-but-
/// reachable address, or simply wants to point at a different server, has a
/// way back in without deleting the saved `server.json` by hand.
fn go_to_connection_page(app: &tauri::AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Internal error: no main window".to_string())?;
    window
        .navigate(local_page_url())
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
        .invoke_handler(tauri::generate_handler![connect])
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
}
