//! Process-wide shared state for the KDE client: the Tokio runtime and the
//! logged-in `BrookClient`, so the login and chat controllers operate on the
//! same authenticated client (which holds the session).

use std::sync::{Arc, OnceLock};

use brook_core::BrookClient;
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static CLIENT: OnceLock<RwLock<Option<Arc<BrookClient>>>> = OnceLock::new();

/// One multi-thread Tokio runtime drives all networking.
pub fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| Runtime::new().expect("create Tokio runtime"))
}

fn client_cell() -> &'static RwLock<Option<Arc<BrookClient>>> {
    CLIENT.get_or_init(|| RwLock::new(None))
}

/// Store the authenticated client after a successful login.
pub async fn set_client(client: Arc<BrookClient>) {
    *client_cell().write().await = Some(client);
}

/// The authenticated client, if logged in.
pub async fn client() -> Option<Arc<BrookClient>> {
    client_cell().read().await.clone()
}
