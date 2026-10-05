//! Add User (#265, admins only): the rules a new account's form is checked against before
//! anything is sent, the generated first password, and the wording. The server decides every
//! one of these (`POST /auth/register` with an admin's token: handle 2 to 64 of
//! `[A-Za-z0-9_.-]`, display name 1 to 64 (the schema says 128, the server refuses more than 64), password 8 to 256, `409 conflict` for a taken
//! handle, `403 authz.forbidden` for a non-admin); the form only says why a field is wrong
//! while it can still be fixed. Plain functions, so they are unit-tested; the dialog only wires
//! them to widgets.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{BrookClient, Error};
use gtk::glib;
use tokio::runtime::Handle;
use zeroize::Zeroize;

const HANDLE_MIN: usize = 2;
const HANDLE_MAX: usize = 64;
const NAME_MAX: usize = 64;
const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 256;

/// Whether the account menu offers Add User…: a global admin only. Members never see it.
pub fn offered(is_admin: bool) -> bool {
    is_admin
}

/// Local checks before anything is sent, counted in characters like the server. The display
/// name is trimmed (the server does too); the password is taken as typed.
pub fn check(
    handle: &str,
    display_name: &str,
    password: &str,
    confirm: &str,
    admin_password: &str,
) -> Result<(), &'static str> {
    let len = handle.chars().count();
    if !(HANDLE_MIN..=HANDLE_MAX).contains(&len) {
        return Err("The handle needs 2 to 64 characters.");
    }
    if !handle
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err("The handle can use letters, digits, and _ . - only.");
    }
    let name = display_name.trim().chars().count();
    if name == 0 {
        return Err("Enter a display name.");
    }
    if name > NAME_MAX {
        return Err("The display name can have at most 64 characters.");
    }
    let len = password.chars().count();
    if len < PASSWORD_MIN {
        return Err("The password needs at least 8 characters.");
    }
    if len > PASSWORD_MAX {
        return Err("The password can have at most 256 characters.");
    }
    if password != confirm {
        return Err("The passwords don't match.");
    }
    if admin_password.is_empty() {
        return Err("Enter your own password to confirm.");
    }
    Ok(())
}

/// Letters and digits without the look-alikes (no 0 O 1 l I), so a password read out or typed
/// from a screen survives.
const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const GENERATED_LEN: usize = 16;

/// A random first password from the operating system's randomness.
pub fn generate_password() -> String {
    generate_with(|buf| getrandom::fill(buf).expect("the operating system has no randomness"))
}

/// The same from any byte source (tests). Bytes that would bias the choice (the top of the
/// range that does not divide evenly by the alphabet) are skipped, not wrapped.
pub fn generate_with(mut fill: impl FnMut(&mut [u8])) -> String {
    let limit = 256 - 256 % ALPHABET.len();
    let mut out = String::with_capacity(GENERATED_LEN);
    let mut buf = [0u8; 32];
    while out.len() < GENERATED_LEN {
        fill(&mut buf);
        for &b in &buf {
            if (b as usize) < limit && out.len() < GENERATED_LEN {
                out.push(ALPHABET[b as usize % ALPHABET.len()] as char);
            }
        }
    }
    buf.zeroize();
    out
}

/// What the sheet says when the account was created.
pub fn added_text(handle: &str) -> String {
    format!("{handle} was added. Give them the password; they can change it under Change Password.")
}

/// A failed creation, in words. A timeout or an unreadable answer is not a "no": the server may
/// have created the account, so the text says to look before trying again; and a "taken" after
/// such an attempt probably means that attempt went through (`after_uncertain`).
pub fn error_text(err: &Error, after_uncertain: bool) -> String {
    match err {
        Error::Api { code, .. } => match code.as_str() {
            "conflict" if after_uncertain => {
                "That handle exists now: the earlier attempt probably \
                                              went through. Check the member list."
                    .into()
            }
            "conflict" => "That handle is already taken.".into(),
            "authz.forbidden" => {
                "You can't add users any more: your role changed. Sign in again.".into()
            }
            "auth.invalid_credentials" => "Your own password is wrong.".into(),
            "auth.rate_limited" => "Too many attempts. Try again later.".into(),
            "validation" | "validation.error" => "The server refused these details.".into(),
            _ => "The server refused to add the user.".into(),
        },
        Error::NotAuthenticated => "You were signed out. Sign in again and retry.".into(),
        Error::Timeout => "The server didn't answer in time. The user may have been added: \
                           check the member list before trying again."
            .into(),
        Error::Http(e) if e.is_connect() => {
            "Couldn't reach the server. The user was not added.".into()
        }
        _ => "Something went wrong. The user may have been added: check the member list \
              before trying again."
            .into(),
    }
}

/// Whether a failed attempt may nevertheless have created the account (the answer was lost).
pub fn left_uncertain(err: &Error) -> bool {
    match err {
        Error::Timeout | Error::UnexpectedResponse => true,
        Error::Http(e) => !e.is_connect(),
        _ => false,
    }
}

/// The Add User… sheet over `parent`: handle, display name, password (with a generate button)
/// and its confirmation. It creates the account through core and says so plainly; the password
/// is never logged, kept or shown again, and is wiped from the fields and from memory after use.
pub fn add_user_dialog(parent: &impl IsA<gtk::Widget>, client: Arc<BrookClient>, runtime: Handle) {
    let handle = adw::EntryRow::builder().title("Handle").build();
    let name = adw::EntryRow::builder().title("Display name").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    let confirm = adw::PasswordEntryRow::builder()
        .title("Repeat password")
        .build();
    let generate = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Generate a password")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    password.add_suffix(&generate);
    // The admin's own password, asked again for this (the server re-authenticates them).
    let admin = adw::PasswordEntryRow::builder()
        .title("Your password")
        .build();

    let who = adw::PreferencesGroup::new();
    who.add(&handle);
    who.add(&name);
    let secret = adw::PreferencesGroup::new();
    secret.add(&password);
    secret.add(&confirm);
    let you = adw::PreferencesGroup::builder()
        .description("Your own password confirms that it is you adding someone.")
        .build();
    you.add(&admin);

    let error = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .css_classes(["error"])
        .build();
    let spinner = gtk::Spinner::new();
    let add = gtk::Button::builder()
        .label("Add User")
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
    column.append(&who);
    column.append(&secret);
    column.append(&you);
    column.append(&error);
    column.append(&spinner);
    column.append(&add);

    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&column));
    let dialog = adw::Dialog::builder()
        .title("Add User")
        .content_width(420)
        .child(&view)
        .build();

    // The button is enabled only for input that passes the local checks; the reason shows once
    // every field has something in it.
    let revalidate = {
        let (handle, name) = (handle.clone(), name.clone());
        let (password, confirm, admin) = (password.clone(), confirm.clone(), admin.clone());
        let (add, error) = (add.clone(), error.clone());
        move || {
            let verdict = check(
                &handle.text(),
                &name.text(),
                &password.text(),
                &confirm.text(),
                &admin.text(),
            );
            add.set_sensitive(verdict.is_ok());
            let filled = !handle.text().is_empty()
                && !name.text().trim().is_empty()
                && !password.text().is_empty()
                && !confirm.text().is_empty()
                && !admin.text().is_empty();
            match verdict {
                Err(reason) if filled => {
                    error.set_text(reason);
                    error.set_visible(true);
                }
                _ => error.set_visible(false),
            }
        }
    };
    for row in [&handle, &name] {
        let revalidate = revalidate.clone();
        row.connect_changed(move |_| revalidate());
    }
    for row in [&password, &confirm, &admin] {
        let revalidate = revalidate.clone();
        row.connect_changed(move |_| revalidate());
    }
    // A generated password fills both fields, so the admin can read it (the eye on the row) and
    // pass it on.
    generate.connect_clicked({
        let (password, confirm) = (password.clone(), confirm.clone());
        move |_| {
            let mut generated = generate_password();
            password.set_text(&generated);
            confirm.set_text(&generated);
            generated.zeroize();
        }
    });

    // Whatever was typed is wiped when the sheet goes away, sent or cancelled.
    dialog.connect_closed({
        let (password, confirm, admin) = (password.clone(), confirm.clone(), admin.clone());
        move |_| {
            password.set_text("");
            confirm.set_text("");
            admin.set_text("");
        }
    });

    // An earlier attempt whose answer was lost: a "taken" now probably means it went through.
    let uncertain = Rc::new(Cell::new(false));
    let dialog_weak = dialog.downgrade();
    add.connect_clicked(move |button| {
        if check(
            &handle.text(),
            &name.text(),
            &password.text(),
            &confirm.text(),
            &admin.text(),
        )
        .is_err()
        {
            return;
        }
        let (new_handle, new_name) = (handle.text().to_string(), name.text().trim().to_string());
        let mut new_password = password.text().to_string();
        let mut admin_password = admin.text().to_string();
        // One attempt at a time; the fields stay as typed so a taken handle can be changed
        // without retyping the rest.
        button.set_sensitive(false);
        spinner.set_spinning(true);
        error.set_visible(false);
        let request = runtime.spawn({
            let (client, handle) = (client.clone(), new_handle.clone());
            async move {
                let result = client
                    .create_user(&handle, &new_name, &new_password, &admin_password)
                    .await;
                new_password.zeroize();
                admin_password.zeroize();
                result
            }
        });
        let (button, spinner, error) = (button.clone(), spinner.clone(), error.clone());
        let (password, confirm, admin) = (password.clone(), confirm.clone(), admin.clone());
        let dialog_weak = dialog_weak.clone();
        let uncertain = uncertain.clone();
        glib::spawn_future_local(async move {
            let result = request.await.unwrap_or(Err(Error::UnexpectedResponse));
            spinner.set_spinning(false);
            match result {
                Ok(_) => {
                    // Gone from the fields as soon as it is done with.
                    password.set_text("");
                    confirm.set_text("");
                    admin.set_text("");
                    let Some(dialog) = dialog_weak.upgrade() else {
                        return;
                    };
                    let parent = dialog.parent();
                    dialog.close();
                    let done =
                        adw::AlertDialog::new(Some("User Added"), Some(&added_text(&new_handle)));
                    done.add_response("ok", "OK");
                    done.present(parent.as_ref());
                }
                Err(err) => {
                    tracing::warn!(%err, "adding a user failed");
                    error.set_text(&error_text(&err, uncertain.get()));
                    if left_uncertain(&err) {
                        uncertain.set(true);
                    }
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

    const OK: (&str, &str, &str, &str) = ("alice", "Alice", "a-long-password", "a-long-password");
    const ADMIN: &str = "my-own-password";

    fn run(handle: &str, name: &str, pw: &str, confirm: &str) -> Result<(), &'static str> {
        check(handle, name, pw, confirm, ADMIN)
    }

    #[test]
    fn only_an_admin_is_offered_the_entry() {
        assert!(offered(true));
        assert!(!offered(false));
    }

    #[test]
    fn a_complete_form_passes() {
        assert_eq!(run(OK.0, OK.1, OK.2, OK.3), Ok(()));
    }

    #[test]
    fn the_handle_follows_the_servers_rules() {
        assert!(run("a", OK.1, OK.2, OK.3).unwrap_err().contains("2 to 64"));
        assert!(run("", OK.1, OK.2, OK.3).is_err());
        assert!(run(&"a".repeat(65), OK.1, OK.2, OK.3).is_err());
        assert_eq!(run(&"a".repeat(64), OK.1, OK.2, OK.3), Ok(()));
        assert_eq!(run("a.b_c-d9", OK.1, OK.2, OK.3), Ok(()));
        for bad in ["al ice", "al@ice", "alice!", "ălice", "al/ice"] {
            assert!(
                run(bad, OK.1, OK.2, OK.3).unwrap_err().contains("only"),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_display_name_is_trimmed_and_bounded() {
        assert!(run(OK.0, "   ", OK.2, OK.3)
            .unwrap_err()
            .contains("display name"));
        assert_eq!(run(OK.0, "  Ana  ", OK.2, OK.3), Ok(()));
        assert_eq!(run(OK.0, &"ă".repeat(64), OK.2, OK.3), Ok(()));
        assert!(run(OK.0, &"ă".repeat(65), OK.2, OK.3).is_err());
    }

    #[test]
    fn the_password_is_counted_in_characters_and_must_match() {
        assert!(run(OK.0, OK.1, "short", "short")
            .unwrap_err()
            .contains("at least 8"));
        assert_eq!(run(OK.0, OK.1, "ăăăăăăăă", "ăăăăăăăă"), Ok(()));
        let long = "x".repeat(257);
        assert!(run(OK.0, OK.1, &long, &long)
            .unwrap_err()
            .contains("at most 256"));
        let at_max = "x".repeat(256);
        assert_eq!(run(OK.0, OK.1, &at_max, &at_max), Ok(()));
        assert!(run(OK.0, OK.1, "a-long-password", "a-long-passwore")
            .unwrap_err()
            .contains("match"));
    }

    #[test]
    fn the_admin_must_confirm_with_their_own_password() {
        assert!(check(OK.0, OK.1, OK.2, OK.3, "")
            .unwrap_err()
            .contains("your own password"));
        assert_eq!(check(OK.0, OK.1, OK.2, OK.3, "x"), Ok(()));
    }

    #[test]
    fn a_generated_password_passes_the_checks_and_avoids_lookalikes() {
        let mut next = 0u8;
        let pw = generate_with(|buf| {
            for b in buf.iter_mut() {
                *b = next;
                next = next.wrapping_add(7);
            }
        });
        assert_eq!(pw.chars().count(), GENERATED_LEN);
        assert_eq!(check("alice", "Alice", &pw, &pw, ADMIN), Ok(()));
        assert!(pw.chars().all(|c| !"0O1lI".contains(c)), "{pw}");
        assert!(pw.chars().all(|c| c.is_ascii_alphanumeric()), "{pw}");
    }

    #[test]
    fn a_generated_password_skips_bytes_that_would_bias_the_choice() {
        // 256 % 56 = 32: bytes 224..=255 would favour the first 32 characters. A source of
        // nothing but those yields no characters until a usable byte comes.
        let mut calls = 0;
        let pw = generate_with(|buf| {
            calls += 1;
            let byte = if calls < 3 { 255 } else { 0 };
            buf.fill(byte);
        });
        assert!(calls >= 3, "the biased bytes were used");
        assert_eq!(pw, "a".repeat(GENERATED_LEN));
    }

    #[test]
    fn two_generated_passwords_differ() {
        assert_ne!(generate_password(), generate_password());
    }

    #[test]
    fn the_confirmation_names_the_handle_and_not_the_password() {
        let text = added_text("alice");
        assert!(text.starts_with("alice was added."));
        assert!(text.contains("Change Password"));
    }

    #[test]
    fn failures_say_why() {
        let api = |code: &str| Error::Api {
            code: code.into(),
            message: String::new(),
        };
        assert!(error_text(&api("conflict"), false).contains("already taken"));
        assert!(error_text(&api("authz.forbidden"), false).contains("role changed"));
        assert!(error_text(&api("auth.rate_limited"), false).contains("Too many"));
        assert!(error_text(&api("validation"), false).contains("refused these details"));
        assert!(error_text(&api("validation.error"), false).contains("refused these details"));
        assert_eq!(
            error_text(&api("auth.invalid_credentials"), false),
            "Your own password is wrong."
        );
        assert!(error_text(&api("something.new"), false).contains("refused to add"));
        assert!(error_text(&Error::NotAuthenticated, false).contains("signed out"));
    }

    #[test]
    fn a_timeout_never_says_the_user_was_not_added() {
        for err in [Error::Timeout, Error::UnexpectedResponse] {
            let text = error_text(&err, false);
            assert!(text.contains("may have been added"), "{text}");
            assert!(!text.contains("was not added"), "{text}");
        }
    }

    #[test]
    fn a_taken_handle_after_a_lost_answer_says_it_probably_went_through() {
        let taken = Error::Api {
            code: "conflict".into(),
            message: String::new(),
        };
        assert!(error_text(&taken, true).contains("probably went through"));
        assert!(error_text(&taken, false).contains("already taken"));
        assert!(left_uncertain(&Error::Timeout));
        assert!(left_uncertain(&Error::UnexpectedResponse));
        assert!(!left_uncertain(&taken));
        assert!(!left_uncertain(&Error::NotAuthenticated));
    }
}
