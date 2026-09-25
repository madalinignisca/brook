//! The cache's network side (plan C3): `GET /sync` and `GET /channels/{id}/messages`,
//! with the signed-in session's access token. Errors carry the status only, never a body.

use serde_json::Value;
use url::Url;

use crate::cache::History;
use crate::session_store::SessionStore;
use crate::sync::{Fetch, Page};
use crate::Error;

/// The server's page size cap for history (`limit <= 100`).
pub(crate) const HISTORY_MAX: usize = 100;

pub(crate) struct Http {
    pub(crate) http: reqwest::Client,
    pub(crate) base: Url,
    pub(crate) session: SessionStore,
    /// The client's transfer registry (progress events, and the flags that stop them).
    pub(crate) transfers: std::sync::Arc<crate::transfer::Transfers>,
}

/// A token for the outbox's session `epoch` only: token and epoch come from one snapshot,
/// and a session that isn't that one answers `NotAuthenticated` (nothing is sent as the
/// next user).
struct EpochToken<'a> {
    session: &'a SessionStore,
    epoch: u64,
}

#[async_trait::async_trait]
impl crate::transfer::TokenSource for EpochToken<'_> {
    async fn token(&self) -> crate::Result<String> {
        match self.session.snapshot().await {
            (rev, Some(s)) if rev.epoch == self.epoch => Ok(s.access_token),
            _ => Err(crate::Error::NotAuthenticated),
        }
    }
}

#[async_trait::async_trait]
impl crate::outbox::Upload for Http {
    async fn upload(
        &self,
        id: crate::transfer::TransferId,
        flags: &std::sync::Arc<crate::transfer::Flags>,
        channel_id: &str,
        file: &crate::outbox::FileRow,
        source: &crate::snapshot::SnapshotSource,
        epoch: u64,
    ) -> Result<crate::transfer::FileInfo, crate::Error> {
        crate::transfer::Uploader {
            http: &self.http,
            base: &self.base,
            transfers: &self.transfers,
            token: &EpochToken {
                session: &self.session,
                epoch,
            },
        }
        .upload(
            id,
            flags,
            channel_id,
            &file.filename,
            &file.content_type,
            &file.file_client_id,
            source,
        )
        .await
    }
}

impl Http {
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<reqwest::Response, Error> {
        let token = self
            .session
            .access_token()
            .await
            .ok_or(Error::NotAuthenticated)?;
        let url = self.base.join(path)?;
        Ok(self
            .http
            .get(url)
            .query(query)
            .bearer_auth(token)
            .send()
            .await?)
    }
}

fn status_error(status: reqwest::StatusCode) -> Error {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Error::NotAuthenticated; // the refresh loop renews it; the next sync retries
    }
    Error::Api {
        code: format!("http_{}", status.as_u16()),
        message: format!("request failed with status {}", status.as_u16()),
    }
}

#[async_trait::async_trait]
impl Fetch for Http {
    async fn page(&self, since: &str) -> Result<Page, Error> {
        let resp = self
            .get("api/v1/sync", &[("since", since.to_string())])
            .await?;
        match resp.status() {
            reqwest::StatusCode::GONE => Ok(Page::Reset),
            s if s.is_success() => Ok(Page::Rows(
                resp.json::<Value>()
                    .await
                    .map_err(|_| Error::UnexpectedResponse)?,
            )),
            s => Err(status_error(s)),
        }
    }
}

#[async_trait::async_trait]
impl History for Http {
    async fn page(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>, Error> {
        let mut query = vec![("limit", limit.clamp(1, HISTORY_MAX).to_string())];
        if let Some(b) = before {
            query.push(("before", b.to_string()));
        }
        let resp = self
            .get(&format!("api/v1/channels/{channel_id}/messages"), &query)
            .await?;
        if !resp.status().is_success() {
            return Err(status_error(resp.status()));
        }
        resp.json::<Vec<Value>>()
            .await
            .map_err(|_| Error::UnexpectedResponse)
    }
}

/// The JSON a queued send posts: `reply_to_id` only when it's a reply.
pub(crate) fn send_body(msg: &crate::outbox::Outgoing, client_id: &str) -> Value {
    let mut v = serde_json::json!({ "body": msg.body, "client_id": client_id });
    if let Some(r) = &msg.reply_to_id {
        v["reply_to_id"] = Value::from(r.as_str());
    }
    v
}

#[async_trait::async_trait]
impl crate::outbox::Post for Http {
    /// `POST /channels/{id}/messages` with the outbox's `client_id`, under session `epoch`
    /// only: if the signed-in session isn't that one any more, nothing is sent.
    async fn send(
        &self,
        channel_id: &str,
        msg: &crate::outbox::Outgoing,
        client_id: &str,
        epoch: u64,
    ) -> Result<Value, crate::outbox::SendFailure> {
        use crate::outbox::SendFailure;
        let (rev, session) = self.session.snapshot().await;
        let Some(session) = session.filter(|_| rev.epoch == epoch) else {
            return Err(SendFailure::Transient { retry_after: None });
        };
        let url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/messages"))
            .map_err(|_| SendFailure::Refused {
                code: "client.bad_channel".into(),
            })?;
        let sent = self
            .http
            .post(url)
            .bearer_auth(&session.access_token)
            .json(&send_body(msg, client_id))
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(_) => return Err(SendFailure::Transient { retry_after: None }),
        };
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<Value>()
                .await
                .map_err(|_| SendFailure::Transient { retry_after: None });
        }
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|s| s.min(300));
        if status.is_server_error() || matches!(status.as_u16(), 401 | 408 | 429) {
            return Err(SendFailure::Transient { retry_after });
        }
        // Refused: the server's code only (never its message, which could echo input).
        let code = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| {
                v.pointer("/error/code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("http_{}", status.as_u16()));
        Err(SendFailure::Refused { code })
    }
}
