//! Persist the one validated server address the trader has connected to.
//!
//! A `server.json` holding a single field lives in Tauri's app-config
//! directory. `load` is deliberately infallible in its return type: a
//! missing, unreadable or malformed file all collapse to `None` rather than
//! an error or a panic, because the only sensible response to an unreadable
//! setting is to ask the user again on the connection page — a user who
//! cannot reach that page has no way to fix anything.

use std::path::Path;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "server.json";

#[derive(Serialize, Deserialize)]
struct Settings {
    url: String,
}

/// Read the saved server address, if any. Returns `None` for a missing
/// file, an unreadable one, or one that fails to parse as the expected
/// shape — never an `Err`, never a panic.
pub fn load(dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(dir.join(FILE_NAME)).ok()?;
    let settings: Settings = serde_json::from_str(&contents).ok()?;
    Some(settings.url)
}

/// Write the server address, replacing whatever was saved before. Creates
/// `dir` if it does not already exist.
pub fn save(dir: &Path, url: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let settings = Settings { url: url.to_string() };
    let contents = serde_json::to_string(&settings)
        .expect("Settings holding a String always serializes");
    std::fs::write(dir.join(FILE_NAME), contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_url_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "https://oms.example.com").unwrap();
        assert_eq!(load(dir.path()).as_deref(), Some("https://oms.example.com"));
    }

    #[test]
    fn nothing_saved_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);
    }

    #[test]
    fn a_later_save_replaces_the_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "https://one.example.com").unwrap();
        save(dir.path(), "https://two.example.com").unwrap();
        assert_eq!(load(dir.path()).as_deref(), Some("https://two.example.com"));
    }

    #[test]
    fn a_corrupt_file_reads_as_nothing_rather_than_panicking() {
        // A half-written or hand-edited file must send the user to the
        // connection page, not crash the app on launch.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.json"), b"{ not json").unwrap();
        assert_eq!(load(dir.path()), None);
    }
}
