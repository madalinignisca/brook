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
