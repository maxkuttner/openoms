// The trader desktop shell. Shows the static connection page in
// ../ui/index.html until a server address has been validated, probed and
// saved, then navigates the window to that server's trade app. On launch,
// a previously saved address sends the window straight there.

mod server;
mod store;

use server::Probe as _;
use tauri::{Manager, Url};

/// Validate, probe and (only then) persist a server address, then navigate
/// the main window to its trade app. Never stores a URL that has not
/// already probed successfully.
#[tauri::command]
async fn connect(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let normalised = server::normalise(&url).map_err(|e| e.message().to_string())?;

    // `server::probe` takes `&dyn Probe`, and `dyn Probe` has no `Sync`
    // bound, so `&dyn Probe` is not `Send` — held across this `.await`
    // inside an async Tauri command, that makes the whole command future
    // non-Send, which `invoke_handler` requires. Calling `get_status` on
    // the concrete `ReqwestProbe` (not a trait object) and classifying the
    // result with `server::classify` uses the exact same tested pieces
    // `server::probe` composes, without the unsizing coercion that breaks
    // `Send`. `server.rs` is out of scope to change (owned by Task 2).
    let http = server::ReqwestProbe::new();
    let health_url = format!("{normalised}/health");
    server::classify(http.get_status(&health_url).await)
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![connect])
        .setup(|app| {
            // A previously saved, already-probed address sends the window
            // straight to the trade app; otherwise the bundled connection
            // page (already loaded) is left showing.
            if let Ok(config_dir) = app.path().app_config_dir() {
                if let Some(url) = store::load(&config_dir) {
                    let _ = navigate_to_trade_app(app.handle(), &url);
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the openOMS trader desktop application");
}
