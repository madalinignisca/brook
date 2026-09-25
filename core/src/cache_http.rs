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
