//! Account settings: the Change Password dialog (docs/PROTOCOL.md §1.1).
//!
//! The wording lives in plain functions ([`check`], [`confirmation`], [`error_text`]) so it is
//! unit-tested; the dialog only wires them to widgets.

use std::sync::Arc;

use adw::prelude::*;
use brook_core::{BrookClient, Error};
use gtk::glib;
use tokio::runtime::Handle;

/// The server's bounds for a new password (PROTOCOL.md §1.1), counted in characters.
const MIN_LEN: usize = 8;
const MAX_LEN: usize = 256;

/// Local checks before anything is sent. Core maps every server 422 to one `validation` code,
/// so the rules the server would refuse with a reason are checked here, where the reason can
/// still be shown.
pub fn check(current: &str, new: &str, confirm: &str) -> Result<(), &'static str> {
    if current.is_empty() {
        return Err("Enter your current password.");
    }
    let len = new.chars().count();
    if len < MIN_LEN {
        return Err("The new password needs at least 8 characters.");
    }
    if len > MAX_LEN {
        return Err("The new password can have at most 256 characters.");
    }
    if new == current {
        return Err("The new password must differ from the current one.");
    }
    if new != confirm {
        return Err("The new passwords don't match.");
    }
    Ok(())
}

/// What happened to the other devices, from the server's answer, never from what was asked:
/// `None` is an older server that does not say (it revokes their refresh tokens; their access
/// tokens live out their 15 minutes).
pub fn confirmation(other_devices_signed_out: Option<bool>) -> &'static str {
    match other_devices_signed_out {
        Some(true) => "Your password was changed. Your other devices are signed out.",
        Some(false) => "Your password was changed. Your other devices stay signed in.",
        None => {
            "Your password was changed. Your other devices will be signed out within 15 minutes."
        }
    }
}

/// A failed change, in words. A timeout is not a "no": the server may have committed the
/// change after core stopped waiting, and then this device is signed out on its next refresh.
pub fn error_text(err: &Error) -> String {
    match err {
        Error::Api { code, .. } => match code.as_str() {
            "auth.invalid_credentials" => "The current password is wrong.".into(),
            "auth.rate_limited" => "Too many attempts. Try again later.".into(),
            "validation" => "The server refused the new password.".into(),
            _ => "The server refused the change.".into(),
        },
        Error::NotAuthenticated => "You were signed out. Sign in again and retry.".into(),
        Error::Timeout => "The server didn't answer in time. The change may have gone through: \
                           if you're signed out, sign in with the new password."
            .into(),
        // Only a failed connect proves nothing was sent; a later transport error may come
        // after the server committed.
        Error::Http(e) if e.is_connect() => {
            "Couldn't reach the server. Your password was not changed.".into()
        }
        Error::Http(_) => "The connection failed mid-way. The change may have gone through: \
                           if you're signed out, sign in with the new password."
            .into(),
        _ => "Something went wrong. Your password may not have changed.".into(),
    }
}

/// Present the Change Password dialog over `parent`.
pub fn change_password_dialog(
    parent: &impl IsA<gtk::Widget>,
    client: Arc<BrookClient>,
    runtime: Handle,
) {
    let current = adw::PasswordEntryRow::builder()
        .title("Current password")
        .build();
    let new = adw::PasswordEntryRow::builder()
        .title("New password")
        .build();
    let confirm = adw::PasswordEntryRow::builder()
        .title("Repeat new password")
        .build();
    let sign_out = adw::SwitchRow::builder()
        .title("Sign out of other devices")
        .subtitle("Right away. This device stays signed in.")
        .active(true)
        .build();

    let passwords = adw::PreferencesGroup::new();
    passwords.add(&current);
    passwords.add(&new);
    passwords.add(&confirm);
    let devices = adw::PreferencesGroup::new();
    devices.add(&sign_out);

    let error = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .css_classes(["error"])
        .build();
    let spinner = gtk::Spinner::new();
    let change = gtk::Button::builder()
        .label("Change Password")
        .sensitive(false)
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .build();

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    column.append(&passwords);
    column.append(&devices);
    column.append(&error);
    column.append(&spinner);
    column.append(&change);

    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&column));
    let dialog = adw::Dialog::builder()
        .title("Change Password")
        .content_width(420)
        .child(&view)
        .build();

    // The button is enabled only for input that passes the local checks; the reason shows
    // once all three fields have something in them.
    let revalidate = {
        let (current, new, confirm) = (current.clone(), new.clone(), confirm.clone());
        let (change, error) = (change.clone(), error.clone());
        move || {
            let verdict = check(&current.text(), &new.text(), &confirm.text());
            change.set_sensitive(verdict.is_ok());
            let filled =
                !current.text().is_empty() && !new.text().is_empty() && !confirm.text().is_empty();
            match verdict {
                Err(reason) if filled => {
                    error.set_text(reason);
                    error.set_visible(true);
                }
                _ => error.set_visible(false),
            }
        }
    };
    for row in [&current, &new, &confirm] {
        let revalidate = revalidate.clone();
        row.connect_changed(move |_| revalidate());
    }

    let dialog_weak = dialog.downgrade();
    change.connect_clicked(move |button| {
        if check(&current.text(), &new.text(), &confirm.text()).is_err() {
            return;
        }
        let (old_pw, new_pw) = (current.text().to_string(), new.text().to_string());
        let sign_out_others = sign_out.is_active();
        // One attempt at a time; the form stays as typed so a wrong current password can be
        // corrected without retyping the new one.
        button.set_sensitive(false);
        spinner.set_spinning(true);
        error.set_visible(false);
        let handle = runtime.spawn({
            let client = client.clone();
            async move {
                client
                    .change_password(&old_pw, &new_pw, sign_out_others)
                    .await
            }
        });
        let (button, spinner, error) = (button.clone(), spinner.clone(), error.clone());
        let dialog_weak = dialog_weak.clone();
        glib::spawn_future_local(async move {
            let result = handle.await.unwrap_or(Err(Error::UnexpectedResponse));
            spinner.set_spinning(false);
            let Some(dialog) = dialog_weak.upgrade() else {
                return; // closed meanwhile: core still finished (or bounded) the change
            };
            match result {
                Ok(outcome) => {
                    let parent = dialog.parent();
                    dialog.close();
                    let done = adw::AlertDialog::new(
                        Some("Password Changed"),
                        Some(confirmation(outcome)),
                    );
                    done.add_response("ok", "OK");
                    done.present(parent.as_ref());
                }
                Err(err) => {
                    tracing::warn!(%err, "password change failed");
                    error.set_text(&error_text(&err));
                    error.set_visible(true);
                    button.set_sensitive(true);
                }
            }
        });
    });

    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_checks_explain_what_is_wrong() {
        assert_eq!(
            check("", "new-pass-1", "new-pass-1"),
            Err("Enter your current password.")
        );
        assert!(check("old", "short", "short")
            .unwrap_err()
            .contains("at least 8"));
        let long = "x".repeat(257);
        assert!(check("old", &long, &long)
            .unwrap_err()
            .contains("at most 256"));
        assert!(check("same-pass", "same-pass", "same-pass")
            .unwrap_err()
            .contains("differ"));
        assert!(check("old", "new-pass-1", "new-pass-2")
            .unwrap_err()
            .contains("match"));
        assert_eq!(check("old", "new-pass-1", "new-pass-1"), Ok(()));
    }

    #[test]
    fn length_is_counted_in_characters_like_the_server() {
        // 8 characters, 16 bytes: the server's min_length counts characters.
        assert_eq!(check("old", "ăăăăăăăă", "ăăăăăăăă"), Ok(()));
        let at_max = "ă".repeat(256);
        assert_eq!(check("old", &at_max, &at_max), Ok(()));
    }

    #[test]
    fn confirmation_follows_the_server_answer() {
        assert!(confirmation(Some(true)).contains("are signed out"));
        assert!(confirmation(Some(false)).contains("stay signed in"));
        assert!(confirmation(None).contains("within 15 minutes"));
    }

    #[test]
    fn a_timeout_never_says_the_password_is_unchanged() {
        let text = error_text(&Error::Timeout);
        assert!(text.contains("may have gone through"));
        assert!(!text.contains("not changed"));
    }

    #[test]
    fn server_codes_have_their_own_words() {
        let api = |code: &str| Error::Api {
            code: code.into(),
            message: String::new(),
        };
        assert!(error_text(&api("auth.invalid_credentials")).contains("current password is wrong"));
        assert!(error_text(&api("auth.rate_limited")).contains("Too many attempts"));
        assert!(error_text(&api("validation")).contains("refused the new password"));
        assert!(error_text(&api("something.new")).contains("refused the change"));
        assert!(error_text(&Error::NotAuthenticated).contains("signed out"));
    }
}
