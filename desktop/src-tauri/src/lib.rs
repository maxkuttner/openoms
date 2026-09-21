// The trader desktop shell. For now this just opens a window showing the
// static connection page in ../ui/index.html; later tasks wire up the
// `/health` probe, persisted server address and window menu.

mod server;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running the openOMS trader desktop application");
}
