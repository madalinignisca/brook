//! The Tokio runtime owned by the bindings.
//!
//! UniFFI polls exported async fns on Swift's executor, which has no Tokio reactor or
//! timers — and `reqwest` needs both. Every exported async fn therefore spawns its work
//! here and awaits the `JoinHandle`, which is executor-agnostic.

use std::sync::OnceLock;

use tokio::runtime::Runtime;

pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("brook-ffi")
            .enable_all()
            .build()
            // No runtime means no networking at all; there is nothing to fall back to.
            .expect("failed to start the brook-ffi Tokio runtime")
    })
}
