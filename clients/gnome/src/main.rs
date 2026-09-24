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

mod call;
mod chat;
mod prefs;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{AuthState, BrookClient, CoreConfig};
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
                match new_client(&server) {
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
            ui.runtime.spawn(async move {
                // Result is intentionally discarded: state transitions published by
                // `login` drive the UI via the state watcher.
                let _ = client.login(&handle, &password).await;
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
}

/// Build a core client for `server`. Plain http is only allowed for loopback,
/// or anywhere with the hidden dev opt-in `BROOK_ALLOW_INSECURE_HTTP=1`.
fn new_client(server: &str) -> brook_core::Result<Arc<BrookClient>> {
    let allow_insecure_http = std::env::var("BROOK_ALLOW_INSECURE_HTTP").as_deref() == Ok("1");
    let config = CoreConfig::with_options(server, allow_insecure_http)?;
    Ok(Arc::new(BrookClient::new(config)?))
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
                    if let Some(chat) = stack.child_by_name("chat") {
                        stack.set_visible_child_name("login");
                        stack.remove(&chat);
                        error_label.set_text("You were signed out. Please log in again.");
                    }
                    login_button.set_sensitive(true);
                }
                AuthState::Authenticating => {
                    login_button.set_sensitive(false);
                    error_label.set_text("");
                }
                AuthState::LoggedIn(user) => {
                    let Some(client) = client.upgrade() else {
                        break;
                    };
                    // Remember the server only once it accepted a login.
                    prefs::save_server(&server);
                    // Build the chat view once, then switch to it. The window
                    // grows to a comfortable chat size on first sign-in.
                    if stack.child_by_name("chat").is_none() {
                        let is_admin = user.global_role == "admin";
                        let view = chat::build(client, ui.runtime.clone(), is_admin);
                        stack.add_named(&view, Some("chat"));
                        if let Some(window) = ui.window.upgrade() {
                            window.set_default_size(900, 640);
                        }
                    }
                    stack.set_visible_child_name("chat");
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
