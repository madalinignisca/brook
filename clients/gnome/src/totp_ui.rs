//! Two-factor sign-in screens (PROTOCOL.md §1.2): the code step after the password,
//! and the settings dialog (turn on with a QR code, new recovery codes, turn off).
//!
//! Every network call runs on the Tokio runtime and its result comes back to the GTK
//! loop; widgets are held weakly by the async parts, so a closed dialog just drops them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{BrookClient, Error, SecondFactor, TotpChallenge};
use gtk::{gio, glib};
use tokio::runtime::Handle;

use crate::totp::{self, Factor};

/// Run `work` on the runtime and hand its result to `done` on the GTK loop.
fn run<T: Send + 'static>(
    runtime: &Handle,
    work: impl std::future::Future<Output = brook_core::Result<T>> + Send + 'static,
    done: impl FnOnce(brook_core::Result<T>) + 'static,
) {
    let handle = runtime.spawn(work);
    glib::spawn_future_local(async move {
        done(handle.await.unwrap_or(Err(Error::UnexpectedResponse)));
    });
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["title-2"])
        .wrap(true)
        .build()
}

fn body(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build()
}

fn error_label() -> gtk::Label {
    gtk::Label::builder()
        .wrap(true)
        .visible(false)
        .css_classes(["error"])
        .build()
}

fn show_error(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(true);
}

fn column() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build()
}

fn pill(label: &str, class: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .css_classes([class, "pill"])
        .halign(gtk::Align::Center)
        .build()
}

// ------------------------------------------------------------------ sign-in code step

/// The code step after a password that needs a second factor. `back(message)` returns
/// to the password view (an empty message for a plain Back). A success needs nothing
/// here: core publishes `LoggedIn` and the auth watcher opens the chat.
pub fn code_step(
    client: Arc<BrookClient>,
    runtime: Handle,
    challenge: TotpChallenge,
    back: Rc<dyn Fn(&str)>,
) -> gtk::Widget {
    let factor = Rc::new(Cell::new(Factor::Code));
    let title = heading("Two-Factor Sign-In");
    let hint = body("Enter the 6-digit code from your authenticator app.");
    let entry = adw::EntryRow::builder()
        .title("Code")
        .input_purpose(gtk::InputPurpose::Digits)
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&entry);
    let error = error_label();
    let sign_in = pill("Sign In", "suggested-action");
    sign_in.set_sensitive(false);
    let switch = gtk::Button::builder()
        .label("Use a recovery code instead")
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .build();
    let back_button = gtk::Button::builder()
        .label("Back")
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .build();

    let page = column();
    for w in [
        title.upcast_ref::<gtk::Widget>(),
        hint.upcast_ref(),
        group.upcast_ref(),
        error.upcast_ref(),
        sign_in.upcast_ref(),
        switch.upcast_ref(),
        back_button.upcast_ref(),
    ] {
        page.append(w);
    }
    let clamp = adw::Clamp::builder().maximum_size(360).child(&page).build();

    let cleaned = {
        let (entry, factor) = (entry.clone(), factor.clone());
        move || match factor.get() {
            Factor::Code => totp::clean_code(&entry.text()),
            Factor::Recovery => totp::clean_recovery(&entry.text()),
        }
    };
    entry.connect_changed({
        let (sign_in, cleaned) = (sign_in.clone(), cleaned.clone());
        move |_| sign_in.set_sensitive(cleaned().is_some())
    });
    switch.connect_clicked({
        let (factor, entry, hint, error) =
            (factor.clone(), entry.clone(), hint.clone(), error.clone());
        move |switch| {
            let to_recovery = factor.get() == Factor::Code;
            factor.set(if to_recovery {
                Factor::Recovery
            } else {
                Factor::Code
            });
            entry.set_text("");
            error.set_visible(false);
            if to_recovery {
                entry.set_title("Recovery code");
                entry.set_input_purpose(gtk::InputPurpose::FreeForm);
                hint.set_text("Enter one of your unused recovery codes.");
                switch.set_label("Use a code from the app instead");
            } else {
                entry.set_title("Code");
                entry.set_input_purpose(gtk::InputPurpose::Digits);
                hint.set_text("Enter the 6-digit code from your authenticator app.");
                switch.set_label("Use a recovery code instead");
            }
        }
    });
    back_button.connect_clicked({
        let (client, runtime, challenge, back) = (
            client.clone(),
            runtime.clone(),
            challenge.clone(),
            back.clone(),
        );
        move |_| {
            let (client, challenge) = (client.clone(), challenge.clone());
            runtime.spawn(async move { client.cancel_totp(&challenge).await });
            back("");
        }
    });

    let submit = {
        let (sign_in, error, factor) = (sign_in.clone(), error.clone(), factor.clone());
        let (client, runtime, challenge, back) = (
            client.clone(),
            runtime.clone(),
            challenge.clone(),
            back.clone(),
        );
        let page_weak = clamp.downgrade();
        move || {
            let Some(input) = cleaned() else { return };
            if !sign_in.is_sensitive() {
                return; // one completion at a time (Enter while one is in flight)
            }
            sign_in.set_sensitive(false);
            error.set_visible(false);
            let used = factor.get();
            let (client, challenge) = (client.clone(), challenge.clone());
            let (sign_in, error, back, page_weak) = (
                sign_in.clone(),
                error.clone(),
                back.clone(),
                page_weak.clone(),
            );
            run(
                &runtime,
                async move {
                    match used {
                        Factor::Code => client.complete_totp(&challenge, &input).await,
                        Factor::Recovery => client.complete_recovery(&challenge, &input).await,
                    }
                },
                move |result| match result {
                    Ok(left) => {
                        // Signed in: the watcher opens the chat. Warn when codes run low.
                        if let Some(notice) = left.and_then(totp::low_codes_notice) {
                            let window = page_weak.upgrade().and_then(|p| p.root());
                            let alert =
                                adw::AlertDialog::new(Some("Recovery Codes"), Some(&notice));
                            alert.add_response("ok", "OK");
                            if let Some(window) = window {
                                alert.present(Some(&window));
                            }
                        }
                    }
                    // Back, a newer login or a sign-out won: nothing to do here.
                    Err(Error::ChallengeSuperseded) => {}
                    Err(Error::Api { ref code, .. }) if code == "auth.totp_expired" => {
                        back("That took too long. Sign in again.");
                    }
                    Err(err) => {
                        show_error(&error, &totp::step_error_text(&err, used));
                        sign_in.set_sensitive(true);
                    }
                },
            );
        }
    };
    let submit = Rc::new(submit);
    sign_in.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    entry.connect_entry_activated(move |_| submit());

    // The server forgets the challenge after its lifetime: go back before a submit
    // would fail. Stops once the page is gone (signed in, Back, a new login).
    let page_weak = clamp.downgrade();
    glib::timeout_add_seconds_local(1, move || {
        let Some(page) = page_weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if page.root().is_none() {
            return glib::ControlFlow::Break;
        }
        if challenge.seconds_left() == 0 {
            let (client, challenge) = (client.clone(), challenge.clone());
            runtime.spawn(async move { client.cancel_totp(&challenge).await });
            back("That took too long. Sign in again.");
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });

    entry.grab_focus();
    clamp.upcast()
}

// ------------------------------------------------------------------ settings dialog

/// The dialog's working state: the page stack and the password typed at "turn on",
/// kept only while the dialog is open (a setup that expires re-enrols with it).
struct Settings {
    client: Arc<BrookClient>,
    runtime: Handle,
    stack: gtk::Stack,
    dialog: glib::WeakRef<adw::Dialog>,
    password: RefCell<Option<String>>,
}

/// Two-Factor Sign-In settings: shows the current state from `/auth/me`, then turn on
/// (QR code, key, code, recovery codes), new recovery codes, or turn off.
pub fn settings_dialog(parent: &impl IsA<gtk::Widget>, client: Arc<BrookClient>, runtime: Handle) {
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::SlideLeftRight)
        .vhomogeneous(false)
        .build();
    let loading = column();
    loading.append(&gtk::Spinner::builder().spinning(true).build());
    stack.add_named(&loading, Some("loading"));

    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&stack));
    let dialog = adw::Dialog::builder()
        .title("Two-Factor Sign-In")
        .content_width(420)
        .child(&view)
        .build();
    let state = Rc::new(Settings {
        client,
        runtime,
        stack,
        dialog: dialog.downgrade(),
        password: RefCell::new(None),
    });
    dialog.connect_closed({
        let state = state.clone();
        move |_| {
            state.password.replace(None); // never outlives the dialog
        }
    });
    load_status(&state);
    dialog.present(Some(parent));
}

fn show_page(state: &Rc<Settings>, name: &str, page: &gtk::Box) {
    if let Some(old) = state.stack.child_by_name(name) {
        state.stack.remove(&old);
    }
    let clamp = adw::Clamp::builder().maximum_size(360).child(page).build();
    state.stack.add_named(&clamp, Some(name));
    state.stack.set_visible_child_name(name);
}

fn load_status(state: &Rc<Settings>) {
    let client = state.client.clone();
    let state2 = state.clone();
    run(
        &state.runtime,
        async move { client.me().await },
        move |result| {
            let page = column();
            let error = error_label();
            match result {
                Ok(me) if me.totp_enabled => {
                    page.append(&heading("Two-Factor Sign-In Is On"));
                    let left = me.recovery_codes_left.unwrap_or(0);
                    let text = totp::low_codes_notice(left).unwrap_or_else(|| {
                    format!("Signing in asks for a code from your authenticator app. You have {left} unused recovery codes.")
                });
                    page.append(&body(&text));
                    let new_codes = pill("New Recovery Codes…", "suggested-action");
                    let turn_off = pill("Turn Off…", "destructive-action");
                    page.append(&new_codes);
                    page.append(&turn_off);
                    new_codes.connect_clicked({
                        let state = state2.clone();
                        move |_| confirm_page(&state, Confirm::NewCodes)
                    });
                    turn_off.connect_clicked({
                        let state = state2.clone();
                        move |_| confirm_page(&state, Confirm::TurnOff)
                    });
                }
                Ok(_) => {
                    page.append(&heading("Two-Factor Sign-In"));
                    page.append(&body(
                        "After your password, signing in will also ask for a code from an \
                     authenticator app on your phone. Turning it on signs out your other devices.",
                    ));
                    let password = adw::PasswordEntryRow::builder().title("Password").build();
                    let group = adw::PreferencesGroup::new();
                    group.add(&password);
                    let next = pill("Continue", "suggested-action");
                    page.append(&group);
                    page.append(&error);
                    page.append(&next);
                    let go = {
                        let (state, password, error, next) = (
                            state2.clone(),
                            password.clone(),
                            error.clone(),
                            next.clone(),
                        );
                        move || {
                            let pw = password.text().to_string();
                            if pw.is_empty() || !next.is_sensitive() {
                                return;
                            }
                            state.password.replace(Some(pw));
                            next.set_sensitive(false);
                            enroll(&state, &error, Some(next.clone()));
                        }
                    };
                    let go = Rc::new(go);
                    next.connect_clicked({
                        let go = go.clone();
                        move |_| go()
                    });
                    password.connect_entry_activated(move |_| go());
                }
                Err(err) => {
                    page.append(&heading("Two-Factor Sign-In"));
                    show_error(&error, &totp::manage_error_text(&err));
                    page.append(&error);
                }
            }
            show_page(&state2, "status", &page);
        },
    );
}

/// Start (or restart, after it expired) enrolment with the password typed at Continue.
fn enroll(state: &Rc<Settings>, error: &gtk::Label, retry: Option<gtk::Button>) {
    let Some(pw) = state.password.borrow().clone() else {
        return;
    };
    let client = state.client.clone();
    let (state2, error) = (state.clone(), error.clone());
    run(
        &state.runtime,
        async move { client.totp_enroll(&pw).await },
        move |result| {
            if let Some(button) = &retry {
                button.set_sensitive(true);
            }
            match result {
                Ok(enrollment) => scan_page(&state2, enrollment.otpauth_uri()),
                Err(err) => show_error(&error, &totp::manage_error_text(&err)),
            }
        },
    );
}

fn scan_page(state: &Rc<Settings>, uri: &str) {
    let page = column();
    page.append(&heading("Scan the Code"));
    page.append(&body(
        "Scan this with your authenticator app, then enter the 6-digit code it shows.",
    ));
    if let Some(texture) = totp::qr_texture(uri) {
        let picture = gtk::Picture::for_paintable(&texture);
        picture.set_size_request(240, 240);
        picture.set_can_shrink(true);
        picture.set_halign(gtk::Align::Center);
        page.append(&picture);
    }
    if let Some(key) = totp::secret_from_uri(uri) {
        page.append(&body("Or type this key into the app:"));
        page.append(
            &gtk::Label::builder()
                .label(totp::group4(&key))
                .selectable(true)
                .wrap(true)
                .css_classes(["monospace"])
                .build(),
        );
    }
    let code = adw::EntryRow::builder()
        .title("Code")
        .input_purpose(gtk::InputPurpose::Digits)
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&code);
    let error = error_label();
    let turn_on = pill("Turn On", "suggested-action");
    turn_on.set_sensitive(false);
    page.append(&group);
    page.append(&error);
    page.append(&turn_on);
    code.connect_changed({
        let turn_on = turn_on.clone();
        move |row| turn_on.set_sensitive(totp::clean_code(&row.text()).is_some())
    });
    let go = {
        let (state, code, error, turn_on) =
            (state.clone(), code.clone(), error.clone(), turn_on.clone());
        move || {
            let Some(c) = totp::clean_code(&code.text()) else {
                return;
            };
            if !turn_on.is_sensitive() {
                return;
            }
            turn_on.set_sensitive(false);
            error.set_visible(false);
            let client = state.client.clone();
            let (state, error, turn_on) = (state.clone(), error.clone(), turn_on.clone());
            run(
                &state.runtime.clone(),
                async move { client.totp_activate(&c).await },
                move |result| {
                    match result {
                        Ok(codes) => {
                            state.password.replace(None);
                            codes_page(&state, &codes, true);
                        }
                        // The setup lapsed: enrol again (a new secret) and show the new QR code.
                        Err(Error::Api { ref code, .. })
                            if code == "auth.totp_enrollment_expired" =>
                        {
                            show_error(&error, "The setup expired. Scan the new code.");
                            enroll(&state, &error, None);
                        }
                        Err(err) => {
                            show_error(&error, &totp::manage_error_text(&err));
                            turn_on.set_sensitive(true);
                        }
                    }
                },
            );
        }
    };
    let go = Rc::new(go);
    turn_on.connect_clicked({
        let go = go.clone();
        move |_| go()
    });
    code.connect_entry_activated(move |_| go());
    show_page(state, "scan", &page);
}

/// The recovery codes, shown once. Done needs "I've saved these" first.
fn codes_page(state: &Rc<Settings>, codes: &[String], just_turned_on: bool) {
    let page = column();
    page.append(&heading(if just_turned_on {
        "Two-Factor Sign-In Is On"
    } else {
        "New Recovery Codes"
    }));
    let mut intro = String::from(
        "Save these recovery codes somewhere safe. Each one signs you in once if you lose \
         your phone. They are shown only now.",
    );
    if just_turned_on {
        intro.push_str(" Your other devices are now signed out.");
    } else {
        intro.push_str(" Your old recovery codes no longer work.");
    }
    page.append(&body(&intro));
    let text = codes.join("\n");
    page.append(
        &gtk::Label::builder()
            .label(&text)
            .selectable(true)
            .css_classes(["monospace", "card"])
            .margin_top(6)
            .margin_bottom(6)
            .build(),
    );
    let actions = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    let copy = gtk::Button::with_label("Copy");
    let save = gtk::Button::with_label("Save…");
    actions.append(&copy);
    actions.append(&save);
    page.append(&actions);
    let saved = gtk::CheckButton::with_label("I've saved these codes");
    saved.set_halign(gtk::Align::Center);
    let done = pill("Done", "suggested-action");
    done.set_sensitive(false);
    page.append(&saved);
    page.append(&done);

    copy.connect_clicked({
        let text = text.clone();
        move |button| button.clipboard().set_text(&text)
    });
    save.connect_clicked(move |button| {
        let window = button.root().and_downcast::<gtk::Window>();
        let file_dialog = gtk::FileDialog::builder()
            .title("Save Recovery Codes")
            .initial_name("brook-recovery-codes.txt")
            .build();
        let text = format!("Brook recovery codes (each works once)\n\n{text}\n");
        file_dialog.save(window.as_ref(), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return }; // cancelled
                                                   // Owner-only: the codes sign in without the phone.
            if let Err(err) = file.replace_contents(
                text.as_bytes(),
                None,
                false,
                gio::FileCreateFlags::PRIVATE | gio::FileCreateFlags::REPLACE_DESTINATION,
                gio::Cancellable::NONE,
            ) {
                tracing::warn!(%err, "saving recovery codes failed");
            }
        });
    });
    saved.connect_toggled({
        let done = done.clone();
        move |check| done.set_sensitive(check.is_active())
    });
    done.connect_clicked({
        let dialog = state.dialog.clone();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        }
    });
    show_page(state, "codes", &page);
}

#[derive(Clone, Copy)]
enum Confirm {
    NewCodes,
    TurnOff,
}

/// Password plus a current code (or a recovery code) to confirm a change.
fn confirm_page(state: &Rc<Settings>, action: Confirm) {
    let page = column();
    page.append(&heading(match action {
        Confirm::NewCodes => "New Recovery Codes",
        Confirm::TurnOff => "Turn Off Two-Factor Sign-In",
    }));
    page.append(&body(match action {
        Confirm::NewCodes => {
            "Enter your password and a code from your app. Every old recovery code stops working."
        }
        Confirm::TurnOff => "Enter your password and a code from your app, or a recovery code.",
    }));
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    let code = adw::EntryRow::builder()
        .title("Code or recovery code")
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&password);
    group.add(&code);
    let error = error_label();
    let go_button = match action {
        Confirm::NewCodes => pill("Create New Codes", "suggested-action"),
        Confirm::TurnOff => pill("Turn Off", "destructive-action"),
    };
    let back = gtk::Button::builder()
        .label("Back")
        .css_classes(["flat"])
        .halign(gtk::Align::Center)
        .build();
    page.append(&group);
    page.append(&error);
    page.append(&go_button);
    page.append(&back);
    back.connect_clicked({
        let state = state.clone();
        move |_| state.stack.set_visible_child_name("status")
    });
    let go = {
        let (state, password, code, error, go_button) = (
            state.clone(),
            password.clone(),
            code.clone(),
            error.clone(),
            go_button.clone(),
        );
        move || {
            let pw = password.text().to_string();
            // Six digits is a code from the app; anything else a recovery code.
            let factor = match (
                totp::clean_code(&code.text()),
                totp::clean_recovery(&code.text()),
            ) {
                (Some(c), _) => SecondFactor::Code(c),
                (None, Some(r)) => SecondFactor::Recovery(r),
                (None, None) => return,
            };
            if pw.is_empty() || !go_button.is_sensitive() {
                return;
            }
            go_button.set_sensitive(false);
            error.set_visible(false);
            let client = state.client.clone();
            let (state, error, go_button) = (state.clone(), error.clone(), go_button.clone());
            match action {
                Confirm::NewCodes => run(
                    &state.runtime.clone(),
                    async move { client.totp_regenerate_recovery_codes(&pw, factor).await },
                    move |result| match result {
                        Ok(codes) => codes_page(&state, &codes, false),
                        Err(err) => {
                            show_error(&error, &totp::manage_error_text(&err));
                            go_button.set_sensitive(true);
                        }
                    },
                ),
                Confirm::TurnOff => run(
                    &state.runtime.clone(),
                    async move { client.totp_disable(&pw, factor).await },
                    move |result| match result {
                        Ok(()) => {
                            let page = column();
                            page.append(&heading("Two-Factor Sign-In Is Off"));
                            page.append(&body("Signing in now needs only your password."));
                            show_page(&state, "off", &page);
                        }
                        Err(err) => {
                            show_error(&error, &totp::manage_error_text(&err));
                            go_button.set_sensitive(true);
                        }
                    },
                ),
            }
        }
    };
    let go = Rc::new(go);
    go_button.connect_clicked({
        let go = go.clone();
        move |_| go()
    });
    code.connect_entry_activated(move |_| go());
    show_page(state, "confirm", &page);
}
