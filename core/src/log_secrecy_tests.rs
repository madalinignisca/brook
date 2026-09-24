//! Tokens and resume tokens must never reach a log line — with the dependency cap every
//! client is required to set (`tungstenite`/`tokio_tungstenite` at `info`). The same scenario
//! without the cap must leak, which proves the capture really sees dependency logs and that
//! the cap is what protects them.

use std::io::Write;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use base64::Engine as _;
use serde_json::json;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

use crate::test_support::TestServer;

/// The filter clients install: everything at trace, dependencies that log frames capped.
pub(crate) const CLIENT_FILTER: &str = "trace,tungstenite=info,tokio_tungstenite=info";
const RESUME_TOKEN: &str = "RESUME-SECRET-7f3a9c";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

static LOG_BRIDGE: Once = Once::new();

/// Run auth, re-auth, join and resume with a subscriber using `filter`; return all output.
async fn scenario(filter: &str) -> String {
    LOG_BRIDGE.call_once(|| {
        let _ = tracing_log::LogTracer::init(); // forward `log` records (tungstenite) to tracing
    });
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut server = TestServer::start().await;
    let client = server.client();
    client.login("alice", "pw").await.unwrap(); // access-1 / refresh-1
    client.start_realtime().await.unwrap();
    let mut peer = server.accept().await;
    peer.accept_auth().await;
    client.commands.conn().wait_for(|c| c.ready).await.unwrap();

    client.refresh_now().await.unwrap(); // access-2 / refresh-2 → re-auth frame
    let reauth = peer.recv().await;
    peer.send(json!({ "type": "ready", "re": reauth["id"], "data": {} }))
        .await;

    let generation = client.commands.conn().borrow().generation;
    let (route, _mailbox) = tokio::sync::mpsc::unbounded_channel();
    let c = client.commands.clone();
    let join = tokio::spawn(async move {
        c.request(
            generation,
            json!({"type": "call.join", "data": {"channel_id": "ch"}}),
            "call.joined",
            Some(route),
        )
        .await
    });
    let f = peer.recv().await;
    peer.send(json!({ "type": "call.joined", "re": f["id"], "data": {
        "call_id": "k1", "self": { "participant_id": "me", "resume_token": RESUME_TOKEN }, "participants": [] }}))
        .await;
    join.await.unwrap().unwrap();
    // Echo the resume token back as a client command, as `call.resume` does.
    let c = client.commands.clone();
    tokio::spawn(async move {
        c.request(generation, json!({"type": "call.resume", "data": {"call_id": "k1", "participant_id": "me", "resume_token": RESUME_TOKEN}}), "call.joined", None).await
    });
    peer.recv().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let bytes = capture.0.lock().unwrap().clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn secrets() -> Vec<&'static str> {
    vec![
        "access-1",
        "access-2",
        "refresh-1",
        "refresh-2",
        RESUME_TOKEN,
    ]
}

/// Every representation of `secret` a log line could plausibly contain.
fn encodings(secret: &str) -> Vec<String> {
    let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    let spaced_hex: String = secret.bytes().map(|b| format!("{b:02x} ")).collect();
    vec![
        secret.to_string(),
        hex,
        spaced_hex.trim_end().to_string(),
        base64::engine::general_purpose::STANDARD.encode(secret),
        format!("{:?}", secret.as_bytes()), // e.g. [97, 99, …]
    ]
}

#[tokio::test]
async fn no_token_reaches_the_logs_with_the_client_filter() {
    let logs = scenario(CLIENT_FILTER).await;
    assert!(
        logs.contains("websocket"),
        "capture saw nothing — test is not observing core"
    );
    for secret in secrets() {
        for enc in encodings(secret) {
            assert!(!logs.contains(&enc), "`{secret}` leaked as `{enc}`");
        }
    }
}

#[tokio::test]
async fn without_the_dependency_cap_tokens_do_leak() {
    let logs = scenario("trace").await;
    let leaked = secrets()
        .iter()
        .any(|s| encodings(s).iter().any(|e| logs.contains(e.as_str())));
    assert!(leaked, "no leak without the cap: the capture does not see dependency logs, so the other test proves nothing");
}
