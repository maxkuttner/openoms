fn main() {
    // The zero-config `tauri_build::build()` never autogenerates the
    // `allow-connect`/`deny-connect` permissions for our own `connect`
    // command - that only happens when `AppManifest::commands` names it
    // (confirmed against tauri-build 2.6.3's `src/acl.rs`). Without this,
    // `capabilities/default.json` referencing `allow-connect` fails the
    // build with "Permission connect not found".
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["connect"])),
    )
    .expect("error while configuring the tauri build");
}
