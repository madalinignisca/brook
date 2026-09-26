//! Attachment transfers: the network layer (MVP+ #65; server: attachments spec, #81, #90).
//!
//! Bytes stream through two traits, never through paths this module picks, so no
//! plaintext lands on disk unless a caller chooses it (offline cache spec §6):
//! - [`UploadSource`]: where an upload's bytes come from (a user's file, or later the
//!   outbox's encrypted snapshot, decrypted chunk by chunk);
//! - [`DownloadSink`]: where a download's bytes go (a user's Save target, or later the
//!   cache's encrypted chunk writer).
//!
//! Server contract handled here:
//! - upload = create (`POST /channels/{id}/files`, idempotent by `client_id`), then the raw
//!   bytes (`PUT /files/{id}/content`, streamed, `Content-Length` = declared size);
//! - `408 file.upload_stalled`, `409 file.upload_in_progress` and `429` are **transient**:
//!   retry after `Retry-After`; network errors and 5xx back off and retry;
//! - `409 file.already_committed` carries the committed file: done if its sha256 equals
//!   ours (our earlier attempt won), else a real conflict;
//! - download = `GET /files/{id}/content`; a resume sends `Range` from what the sink holds
//!   plus `If-Range: "<sha256>"` (the server's `ETag`), and a `200` instead of a `206`
//!   means the file changed: the sink restarts;
//! - the whole-file sha256 is checked before a download is reported done.
//!
//! Each transfer has a caller-chosen [`TransferId`]: progress arrives as [`TransferEvent`]s
//! on [`BrookClient::transfer_events`], and [`BrookClient::cancel_transfer`] stops it.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE,
    RETRY_AFTER,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::broadcast;

use crate::{BrookClient, Error, Result};

/// A committed (or pending) attachment as the server describes it (`FileOut`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FileInfo {
    /// File id.
    pub id: String,
    /// The channel it was uploaded to.
    pub channel_id: String,
    /// Who uploaded it.
    pub uploader_id: String,
    /// The sanitised ASCII name to **save** under (safe on every OS).
    pub filename: String,
    /// The name as typed: display text only, never a filesystem name.
    pub original_name: String,
    /// Size in bytes.
    pub size: u64,
    /// The declared type: untrusted, only for choosing a preview.
    pub content_type: String,
    /// `pending` or `committed`.
    pub status: String,
    /// Hex sha256 of the bytes, once committed.
    pub sha256: Option<String>,
}

/// Identifies one transfer for its events and for cancelling it. The caller makes it
/// (before starting, so it can listen from the first byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId(pub u64);

impl TransferId {
    /// A fresh random id.
    pub fn new() -> Self {
        let mut bytes = [0u8; 8];
        let _ = getrandom::fill(&mut bytes); // a collision only mixes two progress bars
        Self(u64::from_le_bytes(bytes))
    }
}

impl Default for TransferId {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a transfer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// A queued file is being copied into its encrypted snapshot (before any upload).
    Preparing,
    /// Bytes are moving.
    Running,
    /// Waiting before the next attempt (the server asked, or the network failed).
    Retrying {
        /// Seconds until the next attempt.
        after_secs: u64,
    },
    /// Finished and verified.
    Done,
    /// Stopped by [`BrookClient::cancel_transfer`].
    Cancelled,
    /// Gave up, with the reason's code.
    Failed(String),
}

/// Progress of one transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEvent {
    /// Which transfer.
    pub id: TransferId,
    /// Bytes done so far (for a resumed download, including what the sink already held).
    pub done: u64,
    /// Total bytes.
    pub total: u64,
    /// Where it is.
    pub state: TransferState,
}

/// Where an upload's bytes come from. Each attempt reads from the start again.
#[async_trait::async_trait]
pub trait UploadSource: Send + Sync {
    /// Exact size in bytes (the server holds the upload to it).
    fn len(&self) -> u64;
    /// No bytes at all (the server refuses an empty file).
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Hex sha256 of the bytes (to recognise our own earlier attempt as the committed one).
    async fn sha256(&self) -> io::Result<String>;
    /// A fresh reader positioned at the first byte.
    async fn reader(&self) -> io::Result<Box<dyn AsyncRead + Send + Unpin>>;
}

/// Why a sink refused to finish.
#[derive(Debug)]
pub enum SinkError {
    /// Writing failed.
    Io(io::Error),
    /// The bytes don't match the expected sha256.
    Mismatch,
}

impl From<io::Error> for SinkError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Where a download's bytes go.
#[async_trait::async_trait]
pub trait DownloadSink: Send {
    /// Bytes already held from an earlier attempt: the download resumes from here.
    fn resume_offset(&self) -> u64;
    /// Discard everything held (the server's file changed): the download starts over.
    async fn restart(&mut self) -> io::Result<()>;
    /// Append the next bytes.
    async fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Every byte is in: check the whole content against `sha256` and make it final.
    async fn finish(&mut self, sha256: &str) -> std::result::Result<(), SinkError>;
    /// The download failed or was cancelled. The sink decides what stays: a Save target
    /// removes its partial file; a cache keeps its (encrypted) partial for a later resume.
    async fn abort(&mut self);
}

/// What stops a transfer: the user's cancel, or a pause (the outbox's session moved on:
/// the upload resumes later, the row stays pending).
#[derive(Debug, Default)]
pub(crate) struct Flags {
    pub(crate) cancel: AtomicBool,
    pub(crate) pause: AtomicBool,
}

impl Flags {
    /// The error a set flag ends a transfer with (a cancel wins over a pause).
    fn stopped(&self) -> Option<Error> {
        if self.cancel.load(Ordering::SeqCst) {
            Some(cancelled_error())
        } else if self.pause.load(Ordering::SeqCst) {
            Some(paused_error())
        } else {
            None
        }
    }
}

/// Per-client registry: event fan-out and the flags that stop transfers.
pub(crate) struct Transfers {
    events: broadcast::Sender<TransferEvent>,
    /// Flags of a lone transfer (`upload_file`, downloads), dropped when it ends.
    cancelled: Mutex<HashMap<TransferId, Arc<Flags>>>,
    /// Flags shared by every transfer of one outbox row, registered before any of them
    /// starts and removed with the row: cancelling any of its ids stops the row. A std
    /// mutex, never held across an await, so `cancel_transfer` stays synchronous.
    rows: Mutex<HashMap<TransferId, Arc<Flags>>>,
}

impl Transfers {
    pub(crate) fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            events,
            cancelled: Mutex::default(),
            rows: Mutex::default(),
        }
    }

    /// The flags that stop `id`: its row's, if registered, else its own.
    pub(crate) fn flag(&self, id: TransferId) -> Arc<Flags> {
        if let Some(row) = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
        {
            return row.clone();
        }
        self.cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(id)
            .or_default()
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn events_for_tests(&self) -> broadcast::Receiver<TransferEvent> {
        self.events.subscribe()
    }

    /// A lone transfer ended. A row's registrations stay until the row goes.
    fn forget(&self, id: TransferId) {
        self.cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    /// Stop every transfer of these ids through one set of flags.
    pub(crate) fn register(&self, ids: &[TransferId], flags: &Arc<Flags>) {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids {
            rows.insert(*id, flags.clone());
        }
    }

    pub(crate) fn unregister(&self, ids: &[TransferId]) {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids {
            rows.remove(id);
        }
    }

    pub(crate) fn emit(&self, id: TransferId, done: u64, total: u64, state: TransferState) {
        let _ = self.events.send(TransferEvent {
            id,
            done,
            total,
            state,
        });
    }
}

/// How long one transfer request may run at most. The client-wide 30 s timeout would cut
/// off any large file; the server drops an upload idle for 60 s on its own (#90).
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
/// Attempts before a transient failure becomes a failure.
const MAX_ATTEMPTS: u32 = 8;
/// Progress events at most this often (plus the final one).
const PROGRESS_EVERY: Duration = Duration::from_millis(150);
const CHUNK: usize = 64 * 1024;

fn paused_error() -> Error {
    Error::Api {
        code: "transfer.paused".into(),
        message: "the transfer was paused".into(),
    }
}

fn cancelled_error() -> Error {
    Error::Api {
        code: "transfer.cancelled".into(),
        message: "the transfer was cancelled".into(),
    }
}

fn io_error(err: &io::Error) -> Error {
    // The kind only: an io error's text can carry a path.
    Error::Api {
        code: "transfer.io".into(),
        message: format!("local storage failed ({:?})", err.kind()),
    }
}

fn integrity_error() -> Error {
    Error::Api {
        code: "transfer.integrity".into(),
        message: "the file's contents don't match its checksum".into(),
    }
}

/// Whether a transfer error is worth retrying later (the outbox keeps the row pending) or
/// final (the row fails with the code). One list for transfers and the outbox (C4).
pub fn is_transient(err: &Error) -> bool {
    match err {
        Error::Http(_) | Error::Timeout | Error::Disconnected => true,
        Error::Api { code, .. } => {
            matches!(
                code.as_str(),
                "file.upload_stalled"
                    | "file.upload_in_progress"
                    | "file.upload_expired"
                    | "rate_limited"
                    | "auth.rate_limited"
                    | "transfer.network"
                    | "transfer.paused"
                    // The server's disk is at its floor: an admin frees space (Retry-After 600).
                    | "file.no_space"
            ) || code.starts_with("http_5")
                // A timeout or rate limit with no code of ours (a proxy's): try later.
                || matches!(code.as_str(), "http_408" | "http_429")
        }
        _ => false,
    }
}

/// `Retry-After` in whole seconds, within 1..=60 (default 2).
fn retry_after(resp: &reqwest::Response) -> u64 {
    resp.headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(2)
        .clamp(1, 60)
}

fn backoff(attempt: u32) -> u64 {
    (1u64 << attempt.min(5)).min(30)
}

/// The error envelope with its `details` (already_committed carries the file there).
#[derive(Deserialize)]
struct Envelope {
    error: Option<EnvelopeError>,
}

#[derive(Deserialize)]
struct EnvelopeError {
    code: String,
    #[serde(default)]
    details: Option<serde_json::Value>,
}

async fn error_code(resp: reqwest::Response) -> (Option<String>, Option<serde_json::Value>) {
    let body = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<Envelope>(&body) {
        Ok(Envelope { error: Some(e) }) => (Some(e.code), e.details),
        _ => (None, None),
    }
}

fn api(status: StatusCode, code: Option<String>) -> Error {
    let code = code.unwrap_or_else(|| format!("http_{}", status.as_u16()));
    Error::Api {
        message: format!("the server refused the transfer ({code})"),
        code,
    }
}

#[derive(Deserialize)]
struct FileCreated {
    file: FileInfo,
    upload_url: String,
}

impl BrookClient {
    /// Progress of every transfer of this client (filter by [`TransferId`]).
    pub fn transfer_events(&self) -> broadcast::Receiver<TransferEvent> {
        self.transfers.events.subscribe()
    }

    /// Stop a transfer: it ends with [`TransferState::Cancelled`] at the next chunk. For a
    /// file of a queued message, this cancels the message's sending (it fails, and Retry
    /// resumes it).
    pub fn cancel_transfer(&self, id: TransferId) {
        self.transfers.flag(id).cancel.store(true, Ordering::SeqCst);
    }

    /// Upload an attachment to `channel_id`. `client_id` (a UUID the caller keeps for this
    /// file, e.g. in its outbox) makes a retry after a lost response find the same file.
    /// Returns the committed file, to attach to a message.
    ///
    /// A `401` is returned as [`Error::NotAuthenticated`]: the caller owns refreshing and
    /// retrying (with the same `client_id`, the retry finds the same file).
    pub async fn upload_file(
        &self,
        id: TransferId,
        channel_id: &str,
        filename: &str,
        content_type: &str,
        client_id: &str,
        source: &dyn UploadSource,
    ) -> Result<FileInfo> {
        let flags = self.transfers.flag(id);
        let total = source.len();
        let result = Uploader {
            http: &self.http,
            base: &self.base,
            transfers: &self.transfers,
            token: &CurrentToken(self),
        }
        .upload(
            id,
            &flags,
            channel_id,
            filename,
            content_type,
            client_id,
            source,
        )
        .await;
        self.finish_events(id, &result, total, total);
        self.transfers.forget(id);
        result
    }

    /// Download attachment `file_id` into `sink`, resuming from what it already holds.
    /// `sha256` and `size` come from the message's [`FileInfo`]. A `401` is returned as
    /// [`Error::NotAuthenticated`]: the caller refreshes and calls again (it resumes).
    pub async fn download_file(
        &self,
        id: TransferId,
        file_id: &str,
        sha256: &str,
        size: u64,
        sink: &mut dyn DownloadSink,
    ) -> Result<()> {
        let flags = self.transfers.flag(id);
        let result = Downloader {
            http: &self.http,
            base: &self.base,
            transfers: &self.transfers,
            token: &CurrentToken(self),
        }
        .download(id, &flags, file_id, sha256, size, sink)
        .await;
        if result.is_err() {
            sink.abort().await;
        }
        let done = sink.resume_offset();
        self.finish_events(id, &result, done, size);
        self.transfers.forget(id);
        result
    }

    fn finish_events(&self, id: TransferId, result: &Result<impl Sized>, done: u64, total: u64) {
        let state = match result {
            Ok(_) => TransferState::Done,
            Err(Error::Api { code, .. }) if code == "transfer.cancelled" => {
                TransferState::Cancelled
            }
            Err(Error::Api { code, .. }) => TransferState::Failed(code.clone()),
            Err(Error::NotAuthenticated) => TransferState::Failed("auth.not_authenticated".into()),
            Err(Error::Http(_)) => TransferState::Failed("transfer.network".into()),
            Err(_) => TransferState::Failed("transfer.failed".into()),
        };
        self.transfers.emit(id, done, total, state);
    }
}

/// Where an upload's bearer token comes from: the client's current session
/// (`upload_file`), or the outbox's, only while its session epoch is current.
#[async_trait::async_trait]
pub(crate) trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<String>;
}

/// `upload_file`'s: whatever session is current.
struct CurrentToken<'a>(&'a BrookClient);

#[async_trait::async_trait]
impl TokenSource for CurrentToken<'_> {
    async fn token(&self) -> Result<String> {
        self.0.access_token().await
    }
}

/// The upload core, with the token from the caller: `upload_file` and the outbox share it.
/// It emits progress and `Retrying`, never an end state (that's the caller's: a transient
/// end is `Retrying` for the outbox, `Failed` for `upload_file`).
pub(crate) struct Uploader<'a> {
    pub(crate) http: &'a reqwest::Client,
    pub(crate) base: &'a url::Url,
    pub(crate) transfers: &'a Transfers,
    pub(crate) token: &'a dyn TokenSource,
}

impl Uploader<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn upload(
        &self,
        id: TransferId,
        flags: &Arc<Flags>,
        channel_id: &str,
        filename: &str,
        content_type: &str,
        client_id: &str,
        source: &dyn UploadSource,
    ) -> Result<FileInfo> {
        let total = source.len();
        if source.is_empty() {
            return Err(Error::Api {
                code: "file.empty".into(),
                message: "an empty file can't be sent".into(),
            });
        }
        let created = self
            .create_upload(flags, channel_id, filename, content_type, total, client_id)
            .await?;
        if created.file.status == "committed" {
            return Ok(created.file); // an earlier attempt of ours already finished
        }
        let mut url = self.base.join(created.upload_url.trim_start_matches('/'))?;
        let mut attempt = 0u32;
        let mut recreated = false;
        loop {
            if let Some(stop) = flags.stopped() {
                return Err(stop);
            }
            attempt += 1;
            self.transfers.emit(id, 0, total, TransferState::Running);
            let reader = source.reader().await.map_err(|e| io_error(&e))?;
            let body = progress_body(
                reader,
                total,
                id,
                flags.clone(),
                self.transfers.events.clone(),
            );
            let token = self.token.token().await?;
            let sent = self
                .http
                .put(url.clone())
                .bearer_auth(token)
                .header(CONTENT_LENGTH, total)
                .timeout(TRANSFER_TIMEOUT)
                .body(body)
                .send()
                .await;
            let wait = match sent {
                Err(err) => {
                    if let Some(stop) = flags.stopped() {
                        return Err(stop);
                    }
                    if attempt >= MAX_ATTEMPTS {
                        return Err(Error::Http(err));
                    }
                    backoff(attempt)
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp.json().await.map_err(|_| Error::UnexpectedResponse);
                    }
                    if status == StatusCode::UNAUTHORIZED {
                        return Err(Error::NotAuthenticated);
                    }
                    if status == StatusCode::NOT_FOUND && !recreated {
                        // The pending row was swept: create it again, once, with the same
                        // client_id (a committed answer means an earlier attempt finished).
                        recreated = true;
                        let again = self
                            .create_upload(
                                flags,
                                channel_id,
                                filename,
                                content_type,
                                total,
                                client_id,
                            )
                            .await?;
                        if again.file.status == "committed" {
                            return Ok(again.file);
                        }
                        url = self.base.join(again.upload_url.trim_start_matches('/'))?;
                        continue;
                    }
                    let after = retry_after(&resp);
                    let (code, details) = error_code(resp).await;
                    match (status.as_u16(), code.as_deref()) {
                        (409, Some("file.already_committed")) => {
                            let theirs: FileInfo = details
                                .and_then(|d| serde_json::from_value(d).ok())
                                .ok_or(Error::UnexpectedResponse)?;
                            let ours = source.sha256().await.map_err(|e| io_error(&e))?;
                            if theirs.sha256.as_deref() == Some(ours.as_str()) {
                                return Ok(theirs); // our earlier attempt committed it
                            }
                            return Err(api(status, code));
                        }
                        // upload_expired: the part was swept (an hour idle) and the row is
                        // pending again; a fresh PUT to the same URL starts it over.
                        (408, _)
                        | (409, Some("file.upload_in_progress" | "file.upload_expired"))
                        | (429, _) => {
                            if attempt >= MAX_ATTEMPTS {
                                return Err(api(status, code));
                            }
                            after
                        }
                        (500..=599, _) if attempt < MAX_ATTEMPTS => backoff(attempt),
                        _ => return Err(api(status, code)),
                    }
                }
            };
            self.transfers
                .emit(id, 0, total, TransferState::Retrying { after_secs: wait });
            wait_or_stop(wait, flags).await?;
        }
    }

    async fn create_upload(
        &self,
        flags: &Flags,
        channel_id: &str,
        filename: &str,
        content_type: &str,
        size: u64,
        client_id: &str,
    ) -> Result<FileCreated> {
        let url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/files"))?;
        let mut attempt = 0u32;
        loop {
            if let Some(stop) = flags.stopped() {
                return Err(stop);
            }
            attempt += 1;
            let token = self.token.token().await?;
            let sent = self
                .http
                .post(url.clone())
                .bearer_auth(token)
                .json(&json!({
                    "filename": filename,
                    "size": size,
                    "content_type": content_type,
                    "client_id": client_id,
                }))
                .send()
                .await;
            match sent {
                Err(err) if attempt >= MAX_ATTEMPTS => return Err(Error::Http(err)),
                Err(_) => wait_or_stop(backoff(attempt), flags).await?,
                Ok(resp) if resp.status().is_success() => {
                    return resp.json().await.map_err(|_| Error::UnexpectedResponse)
                }
                Ok(resp) if resp.status() == StatusCode::UNAUTHORIZED => {
                    return Err(Error::NotAuthenticated)
                }
                Ok(resp)
                    if (resp.status() == StatusCode::TOO_MANY_REQUESTS
                        || resp.status().is_server_error())
                        && attempt < MAX_ATTEMPTS =>
                {
                    let wait = if resp.status().is_server_error() {
                        backoff(attempt)
                    } else {
                        retry_after(&resp)
                    };
                    wait_or_stop(wait, flags).await?;
                }
                Ok(resp) => {
                    let status = resp.status();
                    let (code, _) = error_code(resp).await;
                    return Err(api(status, code));
                }
            }
        }
    }
}

/// The download core, with the token from the caller: `download_file` (the current session)
/// and the file cache (only the store's own session, paused when it ends) share it. It emits
/// progress and `Retrying`, never an end state (that's the caller's).
pub(crate) struct Downloader<'a> {
    pub(crate) http: &'a reqwest::Client,
    pub(crate) base: &'a url::Url,
    pub(crate) transfers: &'a Transfers,
    pub(crate) token: &'a dyn TokenSource,
}

impl Downloader<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download(
        &self,
        id: TransferId,
        flags: &Arc<Flags>,
        file_id: &str,
        sha256: &str,
        size: u64,
        sink: &mut dyn DownloadSink,
    ) -> Result<()> {
        let url = self.base.join(&format!("api/v1/files/{file_id}/content"))?;
        let validator = format!("\"{sha256}\"");
        let mut attempt = 0u32;
        let mut no_range = false;
        loop {
            if let Some(stop) = flags.stopped() {
                return Err(stop);
            }
            attempt += 1;
            let offset = sink.resume_offset();
            if offset >= size && size > 0 {
                return sink.finish(sha256).await.map_err(|e| match e {
                    SinkError::Io(err) => io_error(&err),
                    SinkError::Mismatch => integrity_error(),
                });
            }
            self.transfers
                .emit(id, offset, size, TransferState::Running);
            let token = self.token.token().await?;
            let mut request = self
                .http
                .get(url.clone())
                .bearer_auth(token)
                // Never a content coding: it rewrites the ETag and answers a Range with a
                // coded body under an identity Content-Range (a corrupt resume).
                .header(ACCEPT_ENCODING, "identity")
                .timeout(TRANSFER_TIMEOUT);
            if offset > 0 && !no_range {
                request = request
                    .header(RANGE, format!("bytes={offset}-"))
                    .header(IF_RANGE, &validator);
            }
            let wait = match request.send().await {
                Err(err) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(Error::Http(err));
                    }
                    backoff(attempt)
                }
                Ok(resp) => {
                    let status = resp.status();
                    match status {
                        StatusCode::OK | StatusCode::PARTIAL_CONTENT
                            if has_content_coding(&resp) =>
                        {
                            // A coded body is not the file's bytes: drop anything held (a
                            // new key, from 0) and try again without a range.
                            sink.restart().await.map_err(|e| io_error(&e))?;
                            if attempt >= MAX_ATTEMPTS {
                                return Err(Error::Api {
                                    code: "transfer.encoded".into(),
                                    message: "the server kept compressing the file".into(),
                                });
                            }
                            // At once the first time (a range is often what a proxy codes),
                            // then with the usual backoff.
                            if std::mem::replace(&mut no_range, true) {
                                backoff(attempt)
                            } else {
                                0
                            }
                        }
                        StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                            // The server's ETag is the sha256 we expect: a different one
                            // means a different file, never "almost this one".
                            let etag = resp.headers().get(ETAG).and_then(|v| v.to_str().ok());
                            // A weak ETag (W/"…", a compressing proxy) promises nothing about
                            // bytes: a range can't be trusted, so start over without one and
                            // let the final sha256 decide.
                            if etag.is_some_and(|e| e.starts_with("W/")) {
                                if status == StatusCode::PARTIAL_CONTENT || offset > 0 {
                                    sink.restart().await.map_err(|e| io_error(&e))?;
                                    no_range = true;
                                    continue;
                                }
                            } else if etag.is_some_and(|e| e != validator) {
                                return Err(integrity_error());
                            }
                            if status == StatusCode::OK && offset > 0 {
                                // If-Range didn't match (or no Range support): start over.
                                sink.restart().await.map_err(|e| io_error(&e))?;
                            }
                            if status == StatusCode::PARTIAL_CONTENT
                                && !content_range_starts_at(&resp, offset)
                            {
                                // Not the continuation of what's held: never append it.
                                // Start over (a new key) without a range.
                                sink.restart().await.map_err(|e| io_error(&e))?;
                                no_range = true;
                                continue;
                            }
                            match self.stream_into(id, flags, resp, sink, size).await {
                                Ok(()) => {
                                    return sink.finish(sha256).await.map_err(|e| match e {
                                        SinkError::Io(err) => io_error(&err),
                                        SinkError::Mismatch => integrity_error(),
                                    })
                                }
                                Err(Streamed::Stopped(stop)) => return Err(stop),
                                Err(Streamed::Sink(err)) => return Err(io_error(&err)),
                                Err(Streamed::TooLong) => return Err(integrity_error()),
                                Err(Streamed::Network) if attempt < MAX_ATTEMPTS => {
                                    backoff(attempt) // resume from what the sink holds
                                }
                                Err(Streamed::Network) => {
                                    return Err(Error::Api {
                                        code: "transfer.network".into(),
                                        message: "the download kept failing".into(),
                                    })
                                }
                            }
                        }
                        StatusCode::UNAUTHORIZED => return Err(Error::NotAuthenticated),
                        // Deleted (with its message, or on its own): gone for good.
                        StatusCode::NOT_FOUND => return Err(gone_error()),
                        StatusCode::RANGE_NOT_SATISFIABLE => {
                            // What the sink holds doesn't fit this file: start over.
                            sink.restart().await.map_err(|e| io_error(&e))?;
                            0
                        }
                        s if s.is_server_error() && attempt < MAX_ATTEMPTS => backoff(attempt),
                        _ => {
                            let (code, _) = error_code(resp).await;
                            return Err(api(status, code));
                        }
                    }
                }
            };
            if wait > 0 {
                self.transfers.emit(
                    id,
                    sink.resume_offset(),
                    size,
                    TransferState::Retrying { after_secs: wait },
                );
                wait_or_stop(wait, flags).await?;
            }
        }
    }

    async fn stream_into(
        &self,
        id: TransferId,
        flags: &Flags,
        resp: reqwest::Response,
        sink: &mut dyn DownloadSink,
        size: u64,
    ) -> std::result::Result<(), Streamed> {
        let mut stream = resp.bytes_stream();
        let mut last = Instant::now();
        while let Some(chunk) = stream.next().await {
            if let Some(stop) = flags.stopped() {
                return Err(Streamed::Stopped(stop));
            }
            let chunk = chunk.map_err(|_| Streamed::Network)?;
            // Never write past the declared size: an over-long body would otherwise fill
            // the disk before the sha256 check at the end could refuse it.
            if sink.resume_offset() + chunk.len() as u64 > size {
                return Err(Streamed::TooLong);
            }
            sink.write_chunk(&chunk).await.map_err(Streamed::Sink)?;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                self.transfers
                    .emit(id, sink.resume_offset(), size, TransferState::Running);
            }
        }
        Ok(())
    }
}

fn has_content_coding(resp: &reqwest::Response) -> bool {
    resp.headers().get_all(CONTENT_ENCODING).iter().any(|v| {
        !v.to_str()
            .unwrap_or("x")
            .trim()
            .eq_ignore_ascii_case("identity")
    })
}

pub(crate) fn gone_error() -> Error {
    Error::Api {
        code: "file.gone".into(),
        message: "the file was deleted".into(),
    }
}

enum Streamed {
    /// A cancel or a pause (the error to end with).
    Stopped(Error),
    Network,
    Sink(io::Error),
    /// More bytes than the file's size.
    TooLong,
}

fn content_range_starts_at(resp: &reqwest::Response, offset: u64) -> bool {
    resp.headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes "))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse::<u64>().ok())
        == Some(offset)
}

/// Sleep `secs`, or end early with the stop (cancel, or pause: never one for the other).
async fn wait_or_stop(secs: u64, flags: &Flags) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Some(stop) = flags.stopped() {
            return Err(stop);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// The request body: the source's bytes, counted for progress and stopped by a cancel.
fn progress_body(
    reader: Box<dyn AsyncRead + Send + Unpin>,
    total: u64,
    id: TransferId,
    flags: Arc<Flags>,
    events: broadcast::Sender<TransferEvent>,
) -> reqwest::Body {
    struct State {
        reader: Box<dyn AsyncRead + Send + Unpin>,
        sent: u64,
        last: Instant,
    }
    let stream = futures_util::stream::unfold(
        State {
            reader,
            sent: 0,
            last: Instant::now(),
        },
        move |mut st| {
            let (flags, events) = (flags.clone(), events.clone());
            async move {
                if flags.stopped().is_some() {
                    return Some((
                        Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
                        st,
                    ));
                }
                let mut buf = vec![0u8; CHUNK];
                match st.reader.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        st.sent += n as u64;
                        if st.last.elapsed() >= PROGRESS_EVERY || st.sent == total {
                            st.last = Instant::now();
                            let _ = events.send(TransferEvent {
                                id,
                                done: st.sent,
                                total,
                                state: TransferState::Running,
                            });
                        }
                        Some((Ok::<Vec<u8>, io::Error>(buf), st))
                    }
                    Err(err) => Some((Err(err), st)),
                }
            }
        },
    );
    reqwest::Body::wrap_stream(stream)
}

/// Hex sha256 of a stream.
async fn sha256_of(mut reader: Box<dyn AsyncRead + Send + Unpin>) -> io::Result<String> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Upload from a file the user picked. It's read as-is; nothing is written anywhere.
pub struct FileSource {
    path: PathBuf,
    len: u64,
}

impl FileSource {
    /// Open `path` for upload (its size is taken now: the server holds the upload to it).
    pub async fn new(path: &Path) -> io::Result<Self> {
        let len = tokio::fs::metadata(path).await?.len();
        Ok(Self {
            path: path.to_path_buf(),
            len,
        })
    }
}

#[async_trait::async_trait]
impl UploadSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }
    async fn sha256(&self) -> io::Result<String> {
        sha256_of(self.reader().await?).await
    }
    async fn reader(&self) -> io::Result<Box<dyn AsyncRead + Send + Unpin>> {
        // Held to the size taken at `new`: a file that grew meanwhile can't overrun it.
        let file = tokio::fs::File::open(&self.path).await?;
        Ok(Box::new(file.take(self.len)))
    }
}

/// Save a download straight into the file the user chose (the platform save dialog's
/// target; under Flatpak the document portal grants that one file, so there is no
/// temporary sibling to rename). A failed or cancelled download truncates and removes it.
pub struct FileSink {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    written: u64,
    hash: ring::digest::Context,
}

impl FileSink {
    /// Create (or truncate) `path` for the download.
    pub async fn create(path: &Path) -> io::Result<Self> {
        let file = tokio::fs::File::create(path).await?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            written: 0,
            hash: ring::digest::Context::new(&ring::digest::SHA256),
        })
    }

    fn file(&mut self) -> io::Result<&mut tokio::fs::File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "sink closed"))
    }
}

#[async_trait::async_trait]
impl DownloadSink for FileSink {
    fn resume_offset(&self) -> u64 {
        self.written
    }
    async fn restart(&mut self) -> io::Result<()> {
        let file = self.file()?;
        file.set_len(0).await?;
        file.rewind().await?;
        self.written = 0;
        self.hash = ring::digest::Context::new(&ring::digest::SHA256);
        Ok(())
    }
    async fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file()?.write_all(bytes).await?;
        self.hash.update(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }
    async fn finish(&mut self, sha256: &str) -> std::result::Result<(), SinkError> {
        let got = hex(self.hash.clone().finish().as_ref());
        if !got.eq_ignore_ascii_case(sha256) {
            return Err(SinkError::Mismatch);
        }
        let file = self.file()?;
        file.flush().await?;
        file.sync_all().await?;
        self.file = None;
        Ok(())
    }
    async fn abort(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.set_len(0).await; // never leave a partial or wrong file behind
            drop(file);
        }
        let _ = tokio::fs::remove_file(&self.path).await;
    }
}

#[cfg(test)]
#[path = "transfer_tests.rs"]
mod tests;
