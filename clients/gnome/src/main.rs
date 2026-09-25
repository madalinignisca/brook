//! Brook GNOME client — GTK4 + libadwaita shell over the shared Rust core.
//!
//! A login view (server, handle, password) that authenticates via `brook-core`
//! and switches to the chat view on success. The server of the last successful
//! login is remembered (see [`prefs`]); `BROOK_SERVER` overrides it.
//!
//! Architecture: networking runs on a Tokio runtime; the UI is **reactive** — it
//! observes the core's [`AuthState`] watch channel rather than threading results
//! back by hand. All GTK widgets are captured by **weak** reference inside async
//! tasks and signal handlers so nothing keeps the window graph alive (no cycles).

mod account;
mod attachments;
mod call;
mod chat;
mod keyring;
mod prefs;
mod totp;
mod totp_ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{AuthState, BrookClient, CoreConfig, LoginOutcome, RestoreOutcome};
use gtk::glib;

const APP_ID: &str = "dev.brook.Brook";
const DEFAULT_SERVER: &str = "https://localhost";

fn main() -> glib::ExitCode {
    init_logging();

    // One multi-thread Tokio runtime drives all networking; kept alive for the
    // lifetime of the app (until `run()` returns).
    let runtime = tokio::runtime::Runtime::new().expect("create Tokio runtime");
    let handle = runtime.handle().clone();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, &handle));
    app.run()
}

/// Log to stderr, filtered by `RUST_LOG` (default `info`). The WebSocket
/// libraries are always capped at `info`, even under `RUST_LOG=trace`: at trace
/// they dump whole frames, which carry access and call resume tokens.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    for directive in ["tungstenite=info", "tokio_tungstenite=info"] {
        filter = filter.add_directive(directive.parse().expect("static directive"));
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn build_ui(app: &adw::Application, runtime: &tokio::runtime::Handle) {
    // Dev-only: a call with yourself through the media engine, no server needed.
    if std::env::var("BROOK_CALL_LOOPBACK").as_deref() == Ok("1") {
        call::present_loopback(app, runtime);
        return;
    }
    // Server: env override (dev/scripts) > last server that logged in > default.
    let initial_server = std::env::var("BROOK_SERVER")
        .ok()
        .or_else(prefs::saved_server)
        .unwrap_or_else(|| DEFAULT_SERVER.to_string());

    // --- Login view ---
    let server_row = adw::EntryRow::builder()
        .title("Server")
        .text(initial_server.as_str())
        .input_purpose(gtk::InputPurpose::Url)
        .build();
    let handle_row = adw::EntryRow::builder().title("Handle").build();
    let password_row = adw::PasswordEntryRow::builder().title("Password").build();
    let group = adw::PreferencesGroup::new();
    group.add(&server_row);
    group.add(&handle_row);
    group.add(&password_row);

    let login_button = gtk::Button::with_label("Log in");
    login_button.add_css_class("suggested-action");
    login_button.add_css_class("pill");

    let error_label = gtk::Label::new(None);
    error_label.add_css_class("error");
    error_label.set_wrap(true);

    let title = gtk::Label::new(Some("Welcome to Brook"));
    title.add_css_class("title-1");

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    column.append(&title);
    column.append(&group);
    column.append(&login_button);
    column.append(&error_label);

    let clamp = adw::Clamp::builder()
        .maximum_size(360)
        .child(&column)
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&clamp, Some("login"));
    stack.set_visible_child_name("login");

    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Brook")
        .default_width(420)
        .default_height(600)
        .content(&toolbar)
        .build();

    let ui = LoginUi {
        runtime: runtime.clone(),
        stack: stack.downgrade(),
        error_label: error_label.downgrade(),
        login_button: login_button.downgrade(),
        window: window.downgrade(),
        signed_out_by_user: Rc::default(),
        keyring: Rc::new(Keyring {
            slot: Arc::new(keyring::SecretServiceSlot::new()),
            usable: std::cell::Cell::new(false),
        }),
        signins: Rc::default(),
    };
    // The client for the server currently in use; replaced when the user logs
    // in to a different server. Dropping the old client ends its state watcher.
    let current: CurrentClient = Rc::default();

    // Submit a login attempt on the Tokio runtime. Widgets are captured weakly so
    // this closure never forms a reference cycle with the button/entry that own it.
    let submit: Rc<dyn Fn()> = Rc::new({
        let ui = ui.clone();
        let current = current.clone();
        let server_weak = server_row.downgrade();
        let handle_weak = handle_row.downgrade();
        let password_weak = password_row.downgrade();
        move || {
            let (Some(server_row), Some(handle_row), Some(password_row)) = (
                server_weak.upgrade(),
                handle_weak.upgrade(),
                password_weak.upgrade(),
            ) else {
                return;
            };
            let (Some(error_label), Some(login_button)) =
                (ui.error_label.upgrade(), ui.login_button.upgrade())
            else {
                return;
            };
            // Ignore re-entrant triggers (e.g. Enter) while a login is in flight.
            if !login_button.is_sensitive() {
                return;
            }
            let server = server_row.text().trim().to_string();
            let handle = handle_row.text().to_string();
            let password = password_row.text().to_string();
            if server.is_empty() || handle.is_empty() || password.is_empty() {
                error_label.set_text("Enter the server, your handle and password.");
                return;
            }

            let reuse = matches!(&*current.borrow(), Some((s, _)) if *s == server);
            if !reuse {
                match new_client(&server, &ui.keyring, &ui.runtime) {
                    Ok(client) => {
                        *current.borrow_mut() = Some((server.clone(), client.clone()));
                        watch_auth_state(&ui, &current, server.clone(), Arc::downgrade(&client));
                    }
                    Err(err) => {
                        error_label.set_text(&format!("Can't use “{server}”: {err}"));
                        return;
                    }
                }
            }
            let Some((_, client)) = current.borrow().clone() else {
                return;
            };
            error_label.set_text("");
            login_button.set_sensitive(false);
            let login = ui.runtime.spawn({
                let client = client.clone();
                async move { client.login(&handle, &password).await }
            });
            // Signed in, or failed: the published state drives the UI (the watcher).
            // Only the second step needs the result itself: its challenge.
            let ui = ui.clone();
            glib::spawn_future_local(async move {
                if let Ok(Ok(LoginOutcome::TotpRequired(challenge))) = login.await {
                    show_code_step(&ui, client, challenge);
                }
            });
        }
    });

    login_button.connect_clicked({
        let submit = submit.clone();
        move |_| (*submit)()
    });
    password_row.connect_entry_activated({
        let submit = submit.clone();
        move |_| (*submit)()
    });

    window.present();

    // Stay signed in (#58): with a usable keyring and a remembered server, sign in with
    // the stored session before the user has to type anything.
    restore_at_launch(&ui, &current, initial_server);
}

/// Check the keyring (off the UI thread: it may take up to its deadline), then restore
/// the remembered server's session if there is one.
fn restore_at_launch(ui: &LoginUi, current: &CurrentClient, server: String) {
    // The form stays insensitive until the probe and any restore settle: a Log In in
    // between would build a client without persistence, which the restore would then
    // replace (submit ignores clicks while the button is insensitive).
    let settle = {
        let (label, button) = (ui.error_label.clone(), ui.login_button.clone());
        move |note: &str| {
            if let (Some(label), Some(button)) = (label.upgrade(), button.upgrade()) {
                label.set_text(note);
                button.set_sensitive(true);
            }
        }
    };
    if let Some(button) = ui.login_button.upgrade() {
        button.set_sensitive(false);
    }
    let ui = ui.clone();
    let current = current.clone();
    let slot = ui.keyring.slot.clone();
    let probe = ui.runtime.spawn_blocking(move || slot.available());
    glib::spawn_future_local(async move {
        let usable = probe.await.unwrap_or(false);
        ui.keyring.usable.set(usable);
        if !usable || prefs::saved_server().is_none() {
            settle(""); // nothing to restore: the login form is ready
            return;
        }
        let Ok(client) = new_client(&server, &ui.keyring, &ui.runtime) else {
            settle("");
            return;
        };
        if let Some(label) = ui.error_label.upgrade() {
            label.set_text("Signing in…");
        }
        *current.borrow_mut() = Some((server.clone(), client.clone()));
        watch_auth_state(&ui, &current, server, Arc::downgrade(&client));
        let restore = ui.runtime.spawn({
            let client = client.clone();
            async move { client.restore().await }
        });
        let outcome = restore.await.unwrap_or(RestoreOutcome::Offline);
        // LoggedIn is published through the watcher (it opens the chat); the others
        // leave the login form, with a note where it helps.
        let note = match outcome {
            RestoreOutcome::Unavailable => {
                "Brook couldn't read your keyring, so you'll need to sign in."
            }
            RestoreOutcome::Offline => {
                "Can't reach the server right now. Your sign-in is kept for next time."
            }
            _ => "",
        };
        settle(note);
    });
}

/// Replace the login form with the two-factor code step (the password was right).
fn show_code_step(ui: &LoginUi, client: Arc<BrookClient>, challenge: brook_core::TotpChallenge) {
    let Some(stack) = ui.stack.upgrade() else {
        return;
    };
    let back: Rc<dyn Fn(&str)> = Rc::new({
        let ui = ui.clone();
        move |message: &str| back_to_password(&ui, message)
    });
    let page = totp_ui::code_step(client, ui.runtime.clone(), challenge, back);
    if let Some(old) = stack.child_by_name("totp") {
        stack.remove(&old);
    }
    stack.add_named(&page, Some("totp"));
    stack.set_visible_child_name("totp");
}

/// Leave the code step for the password form, with `message` (empty for a plain Back).
fn back_to_password(ui: &LoginUi, message: &str) {
    let (Some(stack), Some(error_label), Some(login_button)) = (
        ui.stack.upgrade(),
        ui.error_label.upgrade(),
        ui.login_button.upgrade(),
    ) else {
        return;
    };
    stack.set_visible_child_name("login");
    if let Some(page) = stack.child_by_name("totp") {
        stack.remove(&page);
    }
    error_label.set_text(message);
    login_button.set_sensitive(true);
}

/// Build a core client for `server`. Plain http is only allowed for loopback,
/// or anywhere with the hidden dev opt-in `BROOK_ALLOW_INSECURE_HTTP=1`.
fn new_client(
    server: &str,
    keyring: &Keyring,
    runtime: &tokio::runtime::Handle,
) -> brook_core::Result<Arc<BrookClient>> {
    let allow_insecure_http = std::env::var("BROOK_ALLOW_INSECURE_HTTP").as_deref() == Ok("1");
    let config = CoreConfig::with_options(server, allow_insecure_http)?;
    let client = Arc::new(BrookClient::new(config)?);
    // Only with a keyring that answered at launch: otherwise every sign-in would try
    // (and fence) a store that isn't there. Sign-out fences live in the data dir.
    if keyring.usable.get() {
        let data_dir = glib::user_data_dir().join("brook");
        client.enable_persistence(keyring.slot.clone(), data_dir.clone());
        // The offline cache and outbox (#62), keyed in the same keyring. The signed-in
        // user's stores open on sign-in; until then (or if the key store is locked)
        // every cached call answers `local.unavailable` and the app works online.
        let (c, slot) = (client.clone(), keyring.slot.clone());
        runtime.spawn(async move {
            if !c.enable_local_data(slot, data_dir).await {
                tracing::info!("offline storage unavailable: online only");
            }
        });
    }
    Ok(client)
}

/// The desktop keyring, shared by every client this window creates.
struct Keyring {
    slot: Arc<keyring::SecretServiceSlot>,
    /// Whether it answered unlocked at launch (no keyring or a locked one: sign in by hand).
    usable: std::cell::Cell<bool>,
}

/// The server in use and its client.
type CurrentClient = Rc<RefCell<Option<(String, Arc<BrookClient>)>>>;

/// Weak handles to the login window's widgets, shared by the callbacks.
#[derive(Clone)]
struct LoginUi {
    runtime: tokio::runtime::Handle,
    stack: glib::WeakRef<gtk::Stack>,
    error_label: glib::WeakRef<gtk::Label>,
    login_button: glib::WeakRef<gtk::Button>,
    window: glib::WeakRef<adw::ApplicationWindow>,
    /// Set by Sign Out, so the login view doesn't call it "You were signed out".
    signed_out_by_user: Rc<std::cell::Cell<bool>>,
    keyring: Rc<Keyring>,
    /// Bumped on every completed sign-in, so a late note from an older sign-out can
    /// tell that a newer sign-in happened meanwhile.
    signins: Rc<std::cell::Cell<u64>>,
}

/// Reactive UI: apply a client's observable auth state on the GTK main loop.
/// Holds the client weakly: the future ends once the window is gone or the
/// client was replaced (its state channel closes), so nothing leaks.
fn watch_auth_state(
    ui: &LoginUi,
    current: &CurrentClient,
    server: String,
    client: std::sync::Weak<BrookClient>,
) {
    let Some(mut state) = client.upgrade().map(|c| c.state()) else {
        return;
    };
    let ui = ui.clone();
    let current = Rc::downgrade(current);
    glib::spawn_future_local(async move {
        loop {
            // Stop once another server's client replaced this one (the chat
            // view may keep the old client alive, so its channel stays open).
            let is_current = current.upgrade().is_some_and(|c| {
                c.borrow()
                    .as_ref()
                    .is_some_and(|(_, c)| std::ptr::eq(Arc::as_ptr(c), client.as_ptr()))
            });
            if !is_current {
                break;
            }
            let (Some(stack), Some(error_label), Some(login_button)) = (
                ui.stack.upgrade(),
                ui.error_label.upgrade(),
                ui.login_button.upgrade(),
            ) else {
                break;
            };
            let auth = state.borrow_and_update().clone();
            match auth {
                AuthState::LoggedOut => {
                    // Signed out mid-session (session expired, refresh
                    // rejected): drop the chat view and its state, back to login.
                    // Consumed on every LoggedOut, so a flag from one Sign Out can
                    // never silence a later sign-out the user didn't ask for.
                    let asked = ui.signed_out_by_user.replace(false);
                    // A code step that ended (expired): back to the password form. Its
                    // own handler sets the message; don't overwrite it here.
                    if let Some(page) = stack.child_by_name("totp") {
                        stack.set_visible_child_name("login");
                        stack.remove(&page);
                    }
                    if let Some(chat) = stack.child_by_name("chat") {
                        stack.set_visible_child_name("login");
                        stack.remove(&chat);
                        // Sign Out needs no explanation; anything else (a refresh
                        // rejected, a password changed elsewhere) does.
                        error_label.set_text(if asked {
                            ""
                        } else {
                            "You were signed out. Please log in again."
                        });
                    }
                    login_button.set_sensitive(true);
                }
                AuthState::Authenticating => {
                    login_button.set_sensitive(false);
                    error_label.set_text("");
                }
                AuthState::LoggedIn(user) => {
                    ui.signins.set(ui.signins.get() + 1);
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    // Remember the server only once it accepted a login.
                    prefs::save_server(&server);
                    // Build the chat view once, then switch to it. The window
                    // grows to a comfortable chat size on first sign-in.
                    if stack.child_by_name("chat").is_none() {
                        let is_admin = user.global_role == "admin";
                        let sign_out: Rc<dyn Fn(bool)> = Rc::new({
                            let (client, runtime) = (client.clone(), ui.runtime.clone());
                            let asked = ui.signed_out_by_user.clone();
                            let error_label = ui.error_label.clone();
                            let signins = ui.signins.clone();
                            move |remove_data: bool| {
                                asked.set(true);
                                let at_sign_out = signins.get();
                                let signins = signins.clone();
                                let client = client.clone();
                                // Core ends the session at once and publishes
                                // LoggedOut; the watcher above goes back to login.
                                // "Remove this device's data" erases this user's
                                // cache and outbox first, even with no network.
                                let done = runtime.spawn(async move {
                                    if remove_data {
                                        if let Err(err) = client.sign_out_and_forget().await {
                                            tracing::warn!(%err, "erasing local data failed");
                                        }
                                    } else {
                                        client.logout().await;
                                    }
                                    client.sign_out_complete()
                                });
                                let error_label = error_label.clone();
                                glib::spawn_future_local(async move {
                                    // Both the keyring delete and its fallback failed:
                                    // the stored sign-in may still be usable here.
                                    // Only while no newer sign-in has completed.
                                    let forgot = done.await;
                                    let stale = signins.get() != at_sign_out;
                                    if let (Ok(false), false) = (forgot, stale) {
                                        if let Some(label) = error_label.upgrade() {
                                            label.set_text(
                                                "Signed out, but Brook couldn't forget this \
                                                 sign-in on this computer.",
                                            );
                                        }
                                    }
                                });
                            }
                        });
                        let view = chat::build(client, ui.runtime.clone(), is_admin, sign_out);
                        stack.add_named(&view, Some("chat"));
                        if let Some(window) = ui.window.upgrade() {
                            window.set_default_size(900, 640);
                        }
                    }
                    stack.set_visible_child_name("chat");
                    if let Some(page) = stack.child_by_name("totp") {
                        stack.remove(&page);
                    }
                }
                AuthState::Failed(message) => {
                    error_label.set_text(&message);
                    login_button.set_sensitive(true);
                }
            }
            if state.changed().await.is_err() {
                break;
            }
        }
    });
}
