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

const SIDEBAR: &str = "sidebar";
const SHOW_USERNAMES: &str = "show_usernames";

/// "Show usernames": people are named `@handle` rather than by display name. Off by default;
/// per device.
pub fn show_usernames() -> bool {
    let file = glib::KeyFile::new();
    file.load_from_file(path(), glib::KeyFileFlags::NONE)
        .is_ok()
        && file.boolean(SIDEBAR, SHOW_USERNAMES).unwrap_or(false)
}

/// Remember the "Show usernames" choice. Failures are logged, not fatal.
pub fn save_show_usernames(on: bool) {
    let path = path();
    let file = glib::KeyFile::new();
    let _ = file.load_from_file(&path, glib::KeyFileFlags::KEEP_COMMENTS);
    file.set_boolean(SIDEBAR, SHOW_USERNAMES, on);
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .map_err(|e| e.to_string())
        .and_then(|_| file.save_to_file(&path).map_err(|e| e.to_string()));
    if let Err(err) = result {
        tracing::warn!(%err, path = %path.display(), "could not save the preference");
    }
}

/// The sidebar's "opened" ranks for one account: which conversation this device opened when
/// (higher is later), kept in a group named for the user so accounts don't mix.
fn opened_group(user_id: &str) -> String {
    format!("opened-{user_id}")
}

pub fn load_opened(user_id: &str) -> std::collections::HashMap<String, i64> {
    load_opened_from(&path(), user_id)
}

fn load_opened_from(
    path: &std::path::Path,
    user_id: &str,
) -> std::collections::HashMap<String, i64> {
    let file = glib::KeyFile::new();
    if user_id.is_empty() || file.load_from_file(path, glib::KeyFileFlags::NONE).is_err() {
        return Default::default();
    }
    let group = opened_group(user_id);
    let Ok(keys) = file.keys(&group) else {
        return Default::default();
    };
    keys.iter()
        .filter_map(|key| {
            let rank = i64::from(file.integer(&group, key).ok()?);
            Some((key.to_string(), rank))
        })
        .collect()
}

/// Save the ranks for `user_id` (replacing what was there). Failures are logged, not fatal.
pub fn save_opened(user_id: &str, ranks: &std::collections::HashMap<String, i64>) {
    save_opened_to(&path(), user_id, ranks);
}

fn save_opened_to(
    path: &std::path::Path,
    user_id: &str,
    ranks: &std::collections::HashMap<String, i64>,
) {
    if user_id.is_empty() {
        return;
    }
    let file = glib::KeyFile::new();
    let _ = file.load_from_file(path, glib::KeyFileFlags::KEEP_COMMENTS);
    let group = opened_group(user_id);
    let _ = file.remove_group(&group);
    for (id, rank) in ranks {
        // A key file integer is 32-bit: ranks only count conversations opened, never near it.
        file.set_integer(&group, id, i32::try_from(*rank).unwrap_or(i32::MAX));
    }
    let result = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .map_err(|e| e.to_string())
        .and_then(|_| file.save_to_file(path).map_err(|e| e.to_string()));
    if let Err(err) = result {
        tracing::warn!(%err, path = %path.display(), "could not save the sidebar order");
    }
}

/// Erase the ranks for `user_id` (their data left this device). Other accounts' stay.
pub fn forget_opened(user_id: &str) {
    forget_opened_in(&path(), user_id);
}

fn forget_opened_in(path: &std::path::Path, user_id: &str) {
    if user_id.is_empty() {
        return;
    }
    let file = glib::KeyFile::new();
    if file
        .load_from_file(path, glib::KeyFileFlags::KEEP_COMMENTS)
        .is_err()
        || file.remove_group(&opened_group(user_id)).is_err()
    {
        // No file, or no ranks for them: nothing to erase.
        return;
    }
    if let Err(err) = file.save_to_file(path) {
        tracing::warn!(%err, path = %path.display(), "could not erase the sidebar order");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{forget_opened_in, load_opened_from, save_opened_to};

    #[test]
    fn forgetting_an_account_leaves_the_others_ranks() {
        let dir = std::env::temp_dir().join(format!("brook-forget-test-{}", std::process::id()));
        let file = dir.join("brook").join("gnome.ini");
        let ann = HashMap::from([("c1".to_string(), 3)]);
        let bob = HashMap::from([("c1".to_string(), 1)]);
        forget_opened_in(&file, "ann"); // no file: no panic, no file made
        assert!(!file.exists());
        save_opened_to(&file, "ann", &ann);
        save_opened_to(&file, "bob", &bob);
        forget_opened_in(&file, "ann");
        assert!(load_opened_from(&file, "ann").is_empty());
        assert_eq!(load_opened_from(&file, "bob"), bob);
        forget_opened_in(&file, "");
        assert_eq!(load_opened_from(&file, "bob"), bob);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opened_ranks_are_saved_per_account_and_read_back() {
        let dir = std::env::temp_dir().join(format!("brook-prefs-test-{}", std::process::id()));
        let file = dir.join("brook").join("gnome.ini");
        let ann = HashMap::from([("c1".to_string(), 3), ("c2".to_string(), 9)]);
        let bob = HashMap::from([("c1".to_string(), 1)]);
        assert!(load_opened_from(&file, "ann").is_empty(), "no file yet");
        save_opened_to(&file, "ann", &ann);
        save_opened_to(&file, "bob", &bob);
        assert_eq!(load_opened_from(&file, "ann"), ann);
        assert_eq!(load_opened_from(&file, "bob"), bob);
        // Saving replaces an account's ranks (a pruned conversation is gone); others stay.
        save_opened_to(&file, "ann", &HashMap::from([("c2".to_string(), 9)]));
        assert_eq!(load_opened_from(&file, "ann").len(), 1);
        assert_eq!(load_opened_from(&file, "bob"), bob);
        // No user: nothing read or written.
        assert!(load_opened_from(&file, "").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
