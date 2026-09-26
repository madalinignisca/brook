//! Transfers against a mock server speaking the attachments contract (#81, #90).

use std::io;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::io::AsyncRead;
use wiremock::matchers::{body_bytes, header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::{BrookClient, CoreConfig, LoginOutcome};

const BYTES: &[u8] = b"hello attachment bytes";

fn sha(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn file_json(status: &str, sha256: Option<&str>) -> serde_json::Value {
    json!({
        "id": "f1", "channel_id": "c1", "uploader_id": "u1",
        "filename": "Stefan-raport.pdf", "original_name": "Ștefan–raport.pdf",
        "size": BYTES.len(), "content_type": "application/pdf",
        "status": status, "sha256": sha256, "created_at": "2026-09-25T00:00:00Z"
    })
}

async fn signed_in(server: &MockServer) -> BrookClient {
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a", "refresh_token": "r", "token_type": "bearer"
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/auth/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "u1", "handle": "alice", "display_name": "Alice", "global_role": "member"
        })))
        .mount(server)
        .await;
    let client = BrookClient::new(CoreConfig::new(&server.uri()).unwrap()).unwrap();
    assert!(matches!(
        client.login("alice", "pw").await.unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
    client
}

async fn mount_create(server: &MockServer, status: u16, file_status: &str) {
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/c1/files"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "file": file_json(file_status, None),
            "upload_url": "/api/v1/files/f1/content"
        })))
        .mount(server)
        .await;
}

/// Bytes from memory, for uploads.
struct MemSource(Vec<u8>);

#[async_trait::async_trait]
impl UploadSource for MemSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    async fn sha256(&self) -> io::Result<String> {
        Ok(sha(&self.0))
    }
    async fn reader(&self) -> io::Result<Box<dyn AsyncRead + Send + Unpin>> {
        Ok(Box::new(io::Cursor::new(self.0.clone())))
    }
}

/// Responds with each template in turn, then repeats the last.
struct Sequence(Mutex<Vec<ResponseTemplate>>);

impl Respond for Sequence {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut all = self.0.lock().unwrap();
        if all.len() > 1 {
            all.remove(0)
        } else {
            all[0].clone()
        }
    }
}

#[tokio::test]
async fn an_upload_creates_then_streams_the_exact_bytes() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    mount_create(&server, 201, "pending").await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/files/f1/content"))
        .and(header("content-length", BYTES.len().to_string().as_str()))
        .and(body_bytes(BYTES))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(file_json("committed", Some(&sha(BYTES)))),
        )
        .expect(1)
        .mount(&server)
        .await;
    let id = TransferId::new();
    let mut events = client.transfer_events();
    let file = client
        .upload_file(
            id,
            "c1",
            "report.pdf",
            "application/pdf",
            "cid-1",
            &MemSource(BYTES.to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(file.status, "committed");
    assert_eq!(file.filename, "Stefan-raport.pdf");
    let mut last = None;
    while let Ok(ev) = events.try_recv() {
        assert_eq!(ev.id, id);
        last = Some(ev);
    }
    let last = last.unwrap();
    assert_eq!(
        (last.done, last.total, last.state),
        (BYTES.len() as u64, BYTES.len() as u64, TransferState::Done)
    );
}

#[tokio::test]
async fn transient_refusals_wait_for_retry_after_then_succeed() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    mount_create(&server, 201, "pending").await;
    let busy = ResponseTemplate::new(409)
        .insert_header("retry-after", "1")
        .set_body_json(json!({"error": {"code": "file.upload_in_progress", "message": "x"}}));
    let stalled = ResponseTemplate::new(408)
        .insert_header("retry-after", "1")
        .set_body_json(json!({"error": {"code": "file.upload_stalled", "message": "x"}}));
    let ok = ResponseTemplate::new(200).set_body_json(file_json("committed", Some(&sha(BYTES))));
    Mock::given(method("PUT"))
        .and(path("/api/v1/files/f1/content"))
        .respond_with(Sequence(Mutex::new(vec![busy, stalled, ok])))
        .expect(3)
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    let file = client
        .upload_file(
            TransferId::new(),
            "c1",
            "r.pdf",
            "application/pdf",
            "cid",
            &MemSource(BYTES.to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(file.status, "committed");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "Retry-After ignored"
    );
}

#[tokio::test]
async fn already_committed_with_our_bytes_is_success_and_with_others_is_a_conflict() {
    for (theirs, ok) in [(sha(BYTES), true), (sha(b"someone else"), false)] {
        let server = MockServer::start().await;
        let client = signed_in(&server).await;
        mount_create(&server, 201, "pending").await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/files/f1/content"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": {
                "code": "file.already_committed", "message": "x",
                "details": file_json("committed", Some(&theirs))
            }})))
            .mount(&server)
            .await;
        let got = client
            .upload_file(
                TransferId::new(),
                "c1",
                "r.pdf",
                "application/pdf",
                "cid",
                &MemSource(BYTES.to_vec()),
            )
            .await;
        match (ok, got) {
            (true, Ok(file)) => assert_eq!(file.sha256.as_deref(), Some(theirs.as_str())),
            (false, Err(Error::Api { code, .. })) => assert_eq!(code, "file.already_committed"),
            (_, other) => panic!("unexpected: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_create_that_finds_our_committed_file_skips_the_upload() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    mount_create(&server, 200, "committed").await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let file = client
        .upload_file(
            TransferId::new(),
            "c1",
            "r.pdf",
            "application/pdf",
            "cid",
            &MemSource(BYTES.to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(file.status, "committed");
}

#[tokio::test]
async fn a_download_is_saved_and_verified() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    let digest = sha(BYTES);
    Mock::given(method("GET"))
        .and(path("/api/v1/files/f1/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", format!("\"{digest}\"").as_str())
                .set_body_bytes(BYTES),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("saved.pdf");
    let mut sink = FileSink::create(&dest).await.unwrap();
    client
        .download_file(
            TransferId::new(),
            "f1",
            &digest,
            BYTES.len() as u64,
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), BYTES);
}

#[tokio::test]
async fn a_mismatched_download_leaves_no_file_behind() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/files/f1/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered bytes".as_slice()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("saved.pdf");
    let mut sink = FileSink::create(&dest).await.unwrap();
    let err = client
        .download_file(TransferId::new(), "f1", &sha(BYTES), 14, &mut sink)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Api { ref code, .. } if code == "transfer.integrity"),
        "{err:?}"
    );
    assert!(
        !dest.exists(),
        "a wrong file was left under the user's chosen name"
    );
}

/// A sink that already holds the first bytes (a cache partial): the download resumes with
/// Range + If-Range, or starts over on a 200.
struct MemSink {
    held: Vec<u8>,
    restarts: Arc<Mutex<u32>>,
}

#[async_trait::async_trait]
impl DownloadSink for MemSink {
    fn resume_offset(&self) -> u64 {
        self.held.len() as u64
    }
    async fn restart(&mut self) -> io::Result<()> {
        self.held.clear();
        *self.restarts.lock().unwrap() += 1;
        Ok(())
    }
    async fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.held.extend_from_slice(bytes);
        Ok(())
    }
    async fn finish(&mut self, sha256: &str) -> std::result::Result<(), SinkError> {
        if sha(&self.held) == sha256 {
            Ok(())
        } else {
            Err(SinkError::Mismatch)
        }
    }
    async fn abort(&mut self) {}
}

#[tokio::test]
async fn a_partial_resumes_with_range_and_if_range() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    let digest = sha(BYTES);
    Mock::given(method("GET"))
        .and(path("/api/v1/files/f1/content"))
        .and(header("range", "bytes=6-"))
        .and(header("if-range", format!("\"{digest}\"").as_str()))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("etag", format!("\"{digest}\"").as_str())
                .insert_header(
                    "content-range",
                    format!("bytes 6-{}/{}", BYTES.len() - 1, BYTES.len()).as_str(),
                )
                .set_body_bytes(&BYTES[6..]),
        )
        .expect(1)
        .mount(&server)
        .await;
    let restarts = Arc::new(Mutex::new(0));
    let mut sink = MemSink {
        held: BYTES[..6].to_vec(),
        restarts: restarts.clone(),
    };
    client
        .download_file(
            TransferId::new(),
            "f1",
            &digest,
            BYTES.len() as u64,
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(sink.held, BYTES);
    assert_eq!(*restarts.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_200_to_a_resume_starts_over() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    let digest = sha(BYTES);
    Mock::given(method("GET"))
        .and(path("/api/v1/files/f1/content"))
        .and(header_exists("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", format!("\"{digest}\"").as_str())
                .set_body_bytes(BYTES),
        )
        .mount(&server)
        .await;
    let restarts = Arc::new(Mutex::new(0));
    let mut sink = MemSink {
        held: b"stale-".to_vec(),
        restarts: restarts.clone(),
    };
    client
        .download_file(
            TransferId::new(),
            "f1",
            &digest,
            BYTES.len() as u64,
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(sink.held, BYTES, "the stale partial was kept");
    assert_eq!(*restarts.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_cancelled_transfer_stops_and_says_so() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    mount_create(&server, 201, "pending").await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(409)
                .insert_header("retry-after", "30")
                .set_body_json(
                    json!({"error": {"code": "file.upload_in_progress", "message": "x"}}),
                ),
        )
        .mount(&server)
        .await;
    let id = TransferId::new();
    let client = Arc::new(client);
    let c = client.clone();
    let task = tokio::spawn(async move {
        c.upload_file(
            id,
            "c1",
            "r.pdf",
            "application/pdf",
            "cid",
            &MemSource(BYTES.to_vec()),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    client.cancel_transfer(id);
    let err = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .expect("cancel didn't stop the wait")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(err, Error::Api { ref code, .. } if code == "transfer.cancelled"),
        "{err:?}"
    );
}

#[tokio::test]
async fn attachments_arrive_on_messages() {
    let message: crate::Message = serde_json::from_value(json!({
        "id": "m1", "channel_id": "c1", "author_id": "u1", "body": "see file",
        "created_at": "2026-09-25T00:00:00Z",
        "attachments": [file_json("committed", Some("ab"))]
    }))
    .unwrap();
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].original_name, "Ștefan–raport.pdf");
}

#[tokio::test]
async fn an_over_long_body_is_refused_before_it_is_written() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    let digest = sha(BYTES);
    let mut long = BYTES.to_vec();
    long.extend(std::iter::repeat_n(b'x', 1 << 20)); // a megabyte more than declared
    Mock::given(method("GET"))
        .and(path("/api/v1/files/f1/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", format!("\"{digest}\"").as_str())
                .set_body_bytes(long),
        )
        .mount(&server)
        .await;
    let restarts = Arc::new(Mutex::new(0));
    let mut sink = MemSink {
        held: Vec::new(),
        restarts,
    };
    let err = client
        .download_file(
            TransferId::new(),
            "f1",
            &digest,
            BYTES.len() as u64,
            &mut sink,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Api { ref code, .. } if code == "transfer.integrity"),
        "{err:?}"
    );
    assert!(
        sink.held.len() as u64 <= BYTES.len() as u64,
        "wrote {} bytes past the size",
        sink.held.len()
    );
}

#[tokio::test]
async fn a_weak_etag_on_a_resume_starts_over_without_a_range() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    let digest = sha(BYTES);
    // With a Range: a proxy answers 206 with a weak ETag (bytes not trustworthy).
    Mock::given(method("GET"))
        .and(path("/api/v1/files/f1/content"))
        .and(header_exists("range"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("etag", format!("W/\"{digest}\"").as_str())
                .insert_header("content-range", "bytes 6-21/22")
                .set_body_bytes(&BYTES[6..]),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Without one: the whole file.
    Mock::given(method("GET"))
        .and(path("/api/v1/files/f1/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
        .mount(&server)
        .await;
    let restarts = Arc::new(Mutex::new(0));
    let mut sink = MemSink {
        held: BYTES[..6].to_vec(),
        restarts: restarts.clone(),
    };
    client
        .download_file(
            TransferId::new(),
            "f1",
            &digest,
            BYTES.len() as u64,
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(sink.held, BYTES);
    assert_eq!(*restarts.lock().unwrap(), 1);
}

#[test]
fn transient_errors_are_the_retryable_ones() {
    let api = |code: &str| Error::Api {
        code: code.into(),
        message: String::new(),
    };
    for code in [
        "file.upload_stalled",
        "file.upload_in_progress",
        "rate_limited",
        "transfer.network",
        "transfer.paused",
        "file.no_space", // "try later" since #126 (Retry-After: 600)
        "http_408",
        "http_429",
        "http_502",
    ] {
        assert!(is_transient(&api(code)), "{code}");
    }
    for code in [
        "file.already_committed",
        "file.too_large",
        "transfer.integrity",
        "transfer.cancelled",
        "http_404",
    ] {
        assert!(!is_transient(&api(code)), "{code}");
    }
    assert!(!is_transient(&Error::NotAuthenticated));
}

#[tokio::test]
async fn an_expired_upload_is_put_again() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    mount_create(&server, 201, "pending").await;
    let expired = ResponseTemplate::new(409)
        .insert_header("retry-after", "1")
        .set_body_json(json!({"error": {"code": "file.upload_expired", "message": "x"}}));
    let ok = ResponseTemplate::new(200).set_body_json(file_json("committed", Some(&sha(BYTES))));
    Mock::given(method("PUT"))
        .and(path("/api/v1/files/f1/content"))
        .respond_with(Sequence(Mutex::new(vec![expired, ok])))
        .expect(2)
        .mount(&server)
        .await;
    let file = client
        .upload_file(
            TransferId::new(),
            "c1",
            "r.pdf",
            "application/pdf",
            "cid",
            &MemSource(BYTES.to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(file.status, "committed");
    assert!(is_transient(&Error::Api {
        code: "file.upload_expired".into(),
        message: String::new()
    }));
}

#[tokio::test]
async fn a_swept_pending_row_is_created_again_once() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/c1/files"))
        .respond_with(Sequence(Mutex::new(vec![
            ResponseTemplate::new(201).set_body_json(json!({
                "file": file_json("pending", None), "upload_url": "/api/v1/files/f1/content"
            })),
            ResponseTemplate::new(201).set_body_json(json!({
                "file": file_json("pending", None), "upload_url": "/api/v1/files/f2/content"
            })),
        ])))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/files/f1/content"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"code": "not_found", "message": "x"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/files/f2/content"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(file_json("committed", Some(&sha(BYTES)))),
        )
        .expect(1)
        .mount(&server)
        .await;
    let file = client
        .upload_file(
            TransferId::new(),
            "c1",
            "r.pdf",
            "application/pdf",
            "cid",
            &MemSource(BYTES.to_vec()),
        )
        .await
        .unwrap();
    assert_eq!(file.status, "committed");
}

// ---- Step 2 of the outbox attachments plan: caller's token, row flags ----

/// A token source that refuses: the session it belongs to is gone.
struct Gone;

#[async_trait::async_trait]
impl TokenSource for Gone {
    async fn token(&self) -> Result<String> {
        Err(Error::NotAuthenticated)
    }
}

struct Fixed(&'static str);

#[async_trait::async_trait]
impl TokenSource for Fixed {
    async fn token(&self) -> Result<String> {
        Ok(self.0.to_string())
    }
}

fn uploader<'a>(c: &'a BrookClient, token: &'a dyn TokenSource) -> Uploader<'a> {
    Uploader {
        http: &c.http,
        base: &c.base,
        transfers: &c.transfers,
        token,
    }
}

/// The caller's token decides: a source that refuses sends nothing at all, and one that
/// answers is the bearer on every request (not the client's current session).
#[tokio::test]
async fn an_upload_uses_only_the_callers_token() {
    let server = MockServer::start().await;
    let client = signed_in(&server).await;
    let flags = Arc::new(Flags::default());
    let err = uploader(&client, &Gone)
        .upload(
            TransferId::new(),
            &flags,
            "c1",
            "r.pdf",
            "x",
            "cid",
            &MemSource(BYTES.to_vec()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotAuthenticated), "{err:?}");
    let sent = server.received_requests().await.unwrap();
    assert!(
        !sent.iter().any(|r| r.url.path().contains("/files")),
        "a request went out without the caller's token"
    );
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/c1/files"))
        .and(header("authorization", "Bearer mine"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "file": file_json("pending", None), "upload_url": "/api/v1/files/f1/content"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header("authorization", "Bearer mine"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(file_json("committed", Some(&sha(BYTES)))),
        )
        .expect(1)
        .mount(&server)
        .await;
    uploader(&client, &Fixed("mine"))
        .upload(
            TransferId::new(),
            &flags,
            "c1",
            "r.pdf",
            "x",
            "cid",
            &MemSource(BYTES.to_vec()),
        )
        .await
        .unwrap();
}

/// A pause stops a wait as a pause (the row stays pending), never as a cancel.
#[tokio::test]
async fn a_pause_is_not_a_cancel() {
    let server = MockServer::start().await;
    let client = Arc::new(signed_in(&server).await);
    mount_create(&server, 201, "pending").await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(409)
                .insert_header("retry-after", "30")
                .set_body_json(
                    json!({"error": {"code": "file.upload_in_progress", "message": "x"}}),
                ),
        )
        .mount(&server)
        .await;
    let flags = Arc::new(Flags::default());
    let (c, f) = (client.clone(), flags.clone());
    let task = tokio::spawn(async move {
        uploader(&c, &Fixed("a"))
            .upload(
                TransferId::new(),
                &f,
                "c1",
                "r.pdf",
                "x",
                "cid",
                &MemSource(BYTES.to_vec()),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    flags.pause.store(true, Ordering::SeqCst);
    let err = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .expect("the pause didn't stop the wait")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(err, Error::Api { ref code, .. } if code == "transfer.paused"),
        "{err:?}"
    );
    assert!(is_transient(&err), "a pause must leave the row pending");
}

/// A create waiting out a Retry-After is stopped too (Delete never waits behind it).
#[tokio::test]
async fn a_create_waiting_to_retry_stops_on_cancel() {
    let server = MockServer::start().await;
    let client = Arc::new(signed_in(&server).await);
    Mock::given(method("POST"))
        .and(path("/api/v1/channels/c1/files"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
        .mount(&server)
        .await;
    let flags = Arc::new(Flags::default());
    let (c, f) = (client.clone(), flags.clone());
    let task = tokio::spawn(async move {
        uploader(&c, &Fixed("a"))
            .upload(
                TransferId::new(),
                &f,
                "c1",
                "r.pdf",
                "x",
                "cid",
                &MemSource(BYTES.to_vec()),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    flags.cancel.store(true, Ordering::SeqCst);
    let err = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .expect("the create kept waiting")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(err, Error::Api { ref code, .. } if code == "transfer.cancelled"),
        "{err:?}"
    );
}

/// Ids registered for a row share its flags: cancelling any one stops the row, and a lone
/// transfer's end doesn't drop the registration.
#[test]
fn a_rows_ids_share_its_flags() {
    let t = Transfers::new();
    let (a, b) = (TransferId::new(), TransferId::new());
    let row = Arc::new(Flags::default());
    t.register(&[a, b], &row);
    t.flag(b).cancel.store(true, Ordering::SeqCst);
    assert!(
        row.cancel.load(Ordering::SeqCst),
        "the row didn't see the cancel"
    );
    t.forget(a);
    assert!(
        Arc::ptr_eq(&t.flag(a), &row),
        "forget dropped a row's registration"
    );
    t.unregister(&[a, b]);
    assert!(!Arc::ptr_eq(&t.flag(a), &row));
}

#[test]
fn a_full_server_disk_is_worth_waiting_for() {
    assert!(is_transient(&Error::Api {
        code: "file.no_space".into(),
        message: String::new()
    }));
}
