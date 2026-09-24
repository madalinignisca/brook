//! Small persisted client preferences, in `$XDG_CONFIG_HOME/brook/gnome.ini`.
//!
//! A `GKeyFile` rather than GSettings: GSettings needs an installed, compiled
//! schema, which a `cargo run` build doesn't have (it aborts without one).
//! Revisit when the app is packaged. Holds no secrets.

use std::path::PathBuf;

use gtk::glib;

const GROUP: &str = "login";
const SERVER: &str = "server";

fn path() -> PathBuf {
    glib::user_config_dir().join("brook").join("gnome.ini")
}

/// The server of the last successful login, if any.
pub fn saved_server() -> Option<String> {
    let file = glib::KeyFile::new();
    file.load_from_file(path(), glib::KeyFileFlags::NONE).ok()?;
    let server = file.string(GROUP, SERVER).ok()?.trim().to_string();
    (!server.is_empty()).then_some(server)
}

/// Remember `server` for the next launch. Failures are logged, not fatal.
pub fn save_server(server: &str) {
    if saved_server().as_deref() == Some(server) {
        return;
    }
    let path = path();
    let file = glib::KeyFile::new();
    // Keep any other keys already in the file.
    let _ = file.load_from_file(&path, glib::KeyFileFlags::KEEP_COMMENTS);
    file.set_string(GROUP, SERVER, server);
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .map_err(|e| e.to_string())
        .and_then(|_| file.save_to_file(&path).map_err(|e| e.to_string()));
    if let Err(err) = result {
        tracing::warn!(%err, path = %path.display(), "could not save the server");
    }
}
