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

/// Every writer of this file runs on the GTK main thread (each rewrites the whole file).
/// At the first-sync wipe, erase every account's ranks but `me`'s: those whose data is being
/// wiped, and any orphaned ones (a remove-data sign-out before ranks were erased, ranks
/// written with persistence off, a sign-out before the user was known) (#235). Nothing once
/// this session has ended (the wipe can't run then) or with no known user.
pub fn forget_others_than(ended: bool, me: &str) {
    forget_others_in(&path(), ended, me);
}

fn forget_others_in(path: &std::path::Path, ended: bool, me: &str) {
    if ended || me.is_empty() {
        return;
    }
    let file = glib::KeyFile::new();
    if file
        .load_from_file(path, glib::KeyFileFlags::KEEP_COMMENTS)
        .is_err()
    {
        return;
    }
    let mine = opened_group(me);
    let groups = file.groups();
    let mut changed = false;
    for group in groups.iter().filter(|g| g.starts_with("opened-")) {
        if group.as_str() != mine {
            changed |= file.remove_group(group).is_ok();
        }
    }
    if changed {
        if let Err(err) = file.save_to_file(path) {
            tracing::warn!(%err, path = %path.display(), "could not erase the sidebar orders");
        }
    }
}

/// Whether the opened ranks may be written: not once the user chose to remove this device's
/// data (a late save from the ended session would bring them back), and only for a known user.
pub fn may_save_opened(forgotten: bool, user_id: &str) -> bool {
    !forgotten && !user_id.is_empty()
}

/// What the sign-out choice does to the ranks of `user_id`: erased with "Remove this
/// device's data", kept otherwise. Returns whether they were erased.
pub fn sign_out_forgets(remove_data: bool, user_id: &str) -> bool {
    sign_out_forgets_in(&path(), remove_data, user_id)
}

fn sign_out_forgets_in(path: &std::path::Path, remove_data: bool, user_id: &str) -> bool {
    if remove_data {
        forget_opened_in(path, user_id);
    }
    remove_data
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
    use gtk::glib;
    use std::collections::HashMap;

    use super::{
        forget_opened_in, forget_others_in, load_opened_from, may_save_opened, save_opened_to,
        sign_out_forgets_in,
    };

    fn two_accounts(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("brook-{tag}-{}", std::process::id()));
        let file = dir.join("brook").join("gnome.ini");
        save_opened_to(&file, "ann", &HashMap::from([("c1".to_string(), 3)]));
        save_opened_to(&file, "bob", &HashMap::from([("c1".to_string(), 1)]));
        (dir, file)
    }

    #[test]
    fn forgetting_an_account_leaves_the_others_ranks() {
        let (dir, file) = two_accounts("forget");
        let kf = glib::KeyFile::new();
        kf.load_from_file(&file, glib::KeyFileFlags::NONE).unwrap();
        kf.set_string("login", "server", "https://x");
        kf.set_boolean("sidebar", "show-usernames", true);
        kf.save_to_file(&file).unwrap();
        forget_opened_in(&file, "ann");
        assert!(load_opened_from(&file, "ann").is_empty());
        assert_eq!(load_opened_from(&file, "bob").len(), 1);
        assert!(file_has_group(&file, "login") && file_has_group(&file, "sidebar"));
        let _ = std::fs::remove_dir_all(&dir);
        forget_opened_in(&file, "ann"); // no file: no panic, none made
        assert!(!file.exists());
    }

    #[test]
    fn an_empty_user_id_erases_nothing() {
        let (dir, file) = two_accounts("empty");
        // A group literally named "opened-" would be the one an empty id points at.
        let kf = glib::KeyFile::new();
        let _ = kf.load_from_file(&file, glib::KeyFileFlags::NONE);
        kf.set_integer("opened-", "c9", 5);
        kf.save_to_file(&file).unwrap();
        forget_opened_in(&file, "");
        assert!(file_has_group(&file, "opened-"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn file_has_group(file: &std::path::Path, group: &str) -> bool {
        let kf = glib::KeyFile::new();
        kf.load_from_file(file, glib::KeyFileFlags::NONE).is_ok() && kf.has_group(group)
    }

    #[test]
    fn signing_out_erases_the_ranks_only_with_remove_data() {
        let (dir, file) = two_accounts("signout");
        assert!(!sign_out_forgets_in(&file, false, "ann"));
        assert_eq!(load_opened_from(&file, "ann").len(), 1, "kept");
        assert!(sign_out_forgets_in(&file, true, "ann"));
        assert!(load_opened_from(&file, "ann").is_empty(), "erased");
        assert_eq!(load_opened_from(&file, "bob").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_late_save_after_remove_data_is_refused() {
        assert!(may_save_opened(false, "ann"));
        assert!(!may_save_opened(true, "ann"));
        assert!(!may_save_opened(false, ""));
    }

    #[test]
    fn the_wipe_sweep_keeps_mine_and_everything_that_isnt_ranks() {
        let (dir, file) = two_accounts("sweep");
        let kf = glib::KeyFile::new();
        kf.load_from_file(&file, glib::KeyFileFlags::NONE).unwrap();
        kf.set_string("login", "server", "https://x");
        kf.set_boolean("sidebar", "show-usernames", true);
        kf.set_integer("opened-orphan", "c1", 2);
        // "ann" is not a prefix match for "anna": the sweep compares whole group names.
        kf.set_integer("opened-anna", "c1", 4);
        kf.save_to_file(&file).unwrap();
        // An ended session, or no known user, erases nothing.
        forget_others_in(&file, true, "ann");
        forget_others_in(&file, false, "");
        assert!(file_has_group(&file, "opened-bob"));
        forget_others_in(&file, false, "ann");
        assert_eq!(load_opened_from(&file, "ann").len(), 1, "mine stays");
        assert!(!file_has_group(&file, "opened-bob"));
        assert!(!file_has_group(&file, "opened-orphan"));
        assert!(!file_has_group(&file, "opened-anna"));
        assert!(file_has_group(&file, "login"));
        assert!(file_has_group(&file, "sidebar"));
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
