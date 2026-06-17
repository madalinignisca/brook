//! Brook GNOME client — GTK4 + libadwaita shell over the shared Rust core.
//!
//! Phase 0: a login view that authenticates against the server via `brook-core`
//! and switches to a placeholder home view on success.
//!
//! Architecture: networking runs on a Tokio runtime; the UI is **reactive** — it
//! observes the core's [`AuthState`] watch channel rather than threading results
//! back by hand. All GTK widgets are captured by **weak** reference inside async
//! tasks and signal handlers so nothing keeps the window graph alive (no cycles).

use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{AuthState, BrookClient, CoreConfig};
use gtk::glib;

const APP_ID: &str = "dev.brook.Brook";
const DEFAULT_SERVER: &str = "https://localhost";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();

    // One multi-thread Tokio runtime drives all networking; kept alive for the
    // lifetime of the app (until `run()` returns).
    let runtime = tokio::runtime::Runtime::new().expect("create Tokio runtime");
    let handle = runtime.handle().clone();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, &handle));
    app.run()
}

fn build_ui(app: &adw::Application, runtime: &tokio::runtime::Handle) {
    let server = std::env::var("BROOK_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string());
    let client = match CoreConfig::new(&server).and_then(BrookClient::new) {
        Ok(client) => Arc::new(client),
        Err(err) => {
            tracing::error!(%err, "failed to initialize core client");
            return;
        }
    };

    // --- Login view ---
    let handle_row = adw::EntryRow::builder().title("Handle").build();
    let password_row = adw::PasswordEntryRow::builder().title("Password").build();
    let group = adw::PreferencesGroup::new();
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

    // --- Home (post-login placeholder) view ---
    let home = adw::StatusPage::builder()
        .icon_name("avatar-default-symbolic")
        .title("Signed in")
        .description("Chat, calls and files will live here.")
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&clamp, Some("login"));
    stack.add_named(&home, Some("home"));
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

    // Submit a login attempt on the Tokio runtime. Widgets are captured weakly so
    // this closure never forms a reference cycle with the button/entry that own it.
    let submit: Rc<dyn Fn()> = Rc::new({
        let client = client.clone();
        let runtime = runtime.clone();
        let handle_weak = handle_row.downgrade();
        let password_weak = password_row.downgrade();
        let error_weak = error_label.downgrade();
        let button_weak = login_button.downgrade();
        move || {
            let (Some(handle_row), Some(password_row), Some(error_label), Some(login_button)) = (
                handle_weak.upgrade(),
                password_weak.upgrade(),
                error_weak.upgrade(),
                button_weak.upgrade(),
            ) else {
                return;
            };
            // Ignore re-entrant triggers (e.g. Enter) while a login is in flight.
            if !login_button.is_sensitive() {
                return;
            }
            let handle = handle_row.text().to_string();
            let password = password_row.text().to_string();
            if handle.is_empty() || password.is_empty() {
                error_label.set_text("Enter your handle and password.");
                return;
            }
            error_label.set_text("");
            login_button.set_sensitive(false);

            let client = client.clone();
            runtime.spawn(async move {
                // Result is intentionally discarded: state transitions published by
                // `login` drive the UI via the state watcher below.
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

    // Reactive UI: apply the core's observable auth state on the GTK main loop.
    // Weak widget refs mean this future ends (and drops its client handle) once
    // the window is gone — no leak.
    glib::spawn_future_local({
        let client = client.clone();
        let home_weak = home.downgrade();
        let stack_weak = stack.downgrade();
        let error_weak = error_label.downgrade();
        let button_weak = login_button.downgrade();
        async move {
            let mut state = client.state();
            loop {
                let (Some(home), Some(stack), Some(error_label), Some(login_button)) = (
                    home_weak.upgrade(),
                    stack_weak.upgrade(),
                    error_weak.upgrade(),
                    button_weak.upgrade(),
                ) else {
                    break;
                };
                match &*state.borrow_and_update() {
                    AuthState::LoggedOut => login_button.set_sensitive(true),
                    AuthState::Authenticating => {
                        login_button.set_sensitive(false);
                        error_label.set_text("");
                    }
                    AuthState::LoggedIn(user) => {
                        home.set_title(&format!("Signed in as {}", user.display_name));
                        stack.set_visible_child_name("home");
                    }
                    AuthState::Failed(message) => {
                        error_label.set_text(message);
                        login_button.set_sensitive(true);
                    }
                }
                if state.changed().await.is_err() {
                    break;
                }
            }
        }
    });

    window.present();
}
