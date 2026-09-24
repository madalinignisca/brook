//! The `LoginController` QObject: a thin Qt bridge over `brook-core`.
//!
//! Mirrors the GNOME client's reactive flow — QML calls `log_in(...)`, networking
//! runs on a Tokio runtime, and results are pushed back onto the Qt thread via
//! `cxx_qt::Threading` so the QML properties update on the GUI thread.

use core::pin::Pin;
use std::sync::Arc;

use brook_core::{AuthState, BrookClient, CoreConfig};
use cxx_qt::Threading;
use cxx_qt_lib::QString;

use crate::app;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(bool, busy)]
        #[qproperty(bool, logged_in)]
        #[qproperty(QString, error_text)]
        #[qproperty(QString, display_name)]
        type LoginController = super::LoginControllerRust;

        /// Attempt a login against `server` with `handle`/`password`.
        ///
        /// Exposed to QML as `logIn(...)` (CXX-Qt camelCases the name).
        #[qinvokable]
        fn log_in(self: Pin<&mut Self>, server: &QString, handle: &QString, password: &QString);
    }

    impl cxx_qt::Threading for LoginController {}
}

/// Backing state for [`qobject::LoginController`]. All fields default empty/false.
#[derive(Default)]
pub struct LoginControllerRust {
    busy: bool,
    logged_in: bool,
    error_text: QString,
    display_name: QString,
}

impl qobject::LoginController {
    /// Validate input, then run the login on the Tokio runtime and marshal the
    /// outcome back onto the Qt thread.
    fn log_in(mut self: Pin<&mut Self>, server: &QString, handle: &QString, password: &QString) {
        if *self.busy() {
            return; // ignore re-entrant clicks while a login is in flight
        }

        let server = server.to_string();
        let handle = handle.to_string();
        let password = password.to_string();
        if handle.is_empty() || password.is_empty() {
            self.as_mut()
                .set_error_text(QString::from("Enter your handle and password."));
            return;
        }

        self.as_mut().set_error_text(QString::default());
        self.as_mut().set_busy(true);

        let qt_thread = self.qt_thread();
        app::runtime().spawn(async move {
            // Dev-only: allow a plain-http LAN server (a homelab VM without TLS),
            // matching the GNOME client's BROOK_ALLOW_INSECURE_HTTP opt-in.
            let allow_insecure_http =
                std::env::var("BROOK_ALLOW_INSECURE_HTTP").as_deref() == Ok("1");
            let outcome = async {
                let config = CoreConfig::with_options(&server, allow_insecure_http)?;
                let client = Arc::new(BrookClient::new(config)?);
                let session = client.login(&handle, &password).await?;
                // Share the authenticated client with the chat controller.
                app::set_client(client.clone()).await;
                Ok::<_, brook_core::Error>((session, client))
            }
            .await;

            qt_thread
                .queue(move |mut this| {
                    this.as_mut().set_busy(false);
                    match outcome {
                        Ok((session, client)) => {
                            this.as_mut().set_display_name(QString::from(
                                session.user.display_name.as_str(),
                            ));
                            this.as_mut().set_logged_in(true);
                            watch_sign_out(this.qt_thread(), client);
                        }
                        Err(err) => {
                            this.as_mut()
                                .set_error_text(QString::from(err.to_string().as_str()));
                        }
                    }
                })
                .ok(); // the window may have closed; dropping the update is fine
        });
    }
}

/// Return to the login page when the session ends mid-use (core publishes
/// `LoggedOut` when a refresh is rejected). QML swaps the page on `logged_in`,
/// which destroys the chat page and its models; the chat listener exits on its
/// next event once a new page starts a newer one.
fn watch_sign_out(
    qt_thread: cxx_qt::CxxQtThread<qobject::LoginController>,
    client: Arc<BrookClient>,
) {
    let mut state = client.state();
    drop(client);
    app::runtime().spawn(async move {
        loop {
            if matches!(*state.borrow_and_update(), AuthState::LoggedOut) {
                break;
            }
            if state.changed().await.is_err() {
                return; // client gone (replaced by a newer login)
            }
        }
        app::clear_client().await;
        qt_thread
            .queue(|mut this| {
                this.as_mut().set_logged_in(false);
                this.as_mut()
                    .set_error_text(QString::from("You were signed out. Please log in again."));
            })
            .ok();
    });
}
