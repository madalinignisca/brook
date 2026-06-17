//! The Brook core client.

use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;
use url::Url;

use crate::{AuthState, CoreConfig, Error, Result, Session, User};

/// Shared client: networking + observable auth state.
///
/// Cheap to clone-by-`Arc` from the UI; safe to call from any async task.
pub struct BrookClient {
    base: Url,
    http: reqwest::Client,
    state_tx: watch::Sender<AuthState>,
    state_rx: watch::Receiver<AuthState>,
}

impl BrookClient {
    /// Create a client for the given configuration.
    pub fn new(config: CoreConfig) -> Result<Self> {
        let http = reqwest::Client::builder().build()?;
        let (state_tx, state_rx) = watch::channel(AuthState::LoggedOut);
        Ok(Self {
            base: config.base_url,
            http,
            state_tx,
            state_rx,
        })
    }

    /// A receiver the UI can watch for [`AuthState`] transitions.
    pub fn state(&self) -> watch::Receiver<AuthState> {
        self.state_rx.clone()
    }

    /// Log in with a local handle + password, publishing state transitions.
    pub async fn login(&self, handle: &str, password: &str) -> Result<Session> {
        // `send` only fails if all receivers are dropped; `self` holds `state_rx`,
        // so it can never fail here. Ignoring the result is safe.
        let _ = self.state_tx.send(AuthState::Authenticating);
        let result = self.do_login(handle, password).await;
        let next = match &result {
            Ok(session) => AuthState::LoggedIn(session.user.clone()),
            Err(err) => AuthState::Failed(err.to_string()),
        };
        let _ = self.state_tx.send(next);
        result
    }

    async fn do_login(&self, handle: &str, password: &str) -> Result<Session> {
        let url = self.base.join("api/v1/auth/login")?;
        let resp = self
            .http
            .post(url)
            .json(&json!({ "handle": handle, "password": password }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        let tokens: TokenPair = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
        let user = self.fetch_me(&tokens.access_token).await?;
        Ok(Session {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            user,
        })
    }

    async fn fetch_me(&self, access_token: &str) -> Result<User> {
        let url = self.base.join("api/v1/auth/me")?;
        let resp = self.http.get(url).bearer_auth(access_token).send().await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        resp.json().await.map_err(|_| Error::UnexpectedResponse)
    }
}

#[derive(Deserialize)]
struct TokenPair {
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: Option<ApiErrorContent>,
}

#[derive(Deserialize)]
struct ApiErrorContent {
    code: String,
    message: String,
}

/// Map a non-2xx response into a structured [`Error`].
async fn api_error(resp: reqwest::Response) -> Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_else(|err| {
        tracing::warn!(%err, "failed to read error response body");
        String::new()
    });
    if let Ok(ApiErrorEnvelope {
        error: Some(content),
    }) = serde_json::from_str::<ApiErrorEnvelope>(&body)
    {
        return Error::Api {
            code: content.code,
            message: content.message,
        };
    }
    // Non-JSON / unstructured error (e.g. a proxy 502 HTML page): keep the status
    // and a short body snippet so 5xx debugging isn't blind.
    let snippet: String = body.chars().take(120).collect();
    Error::Api {
        code: format!("http_{}", status.as_u16()),
        message: if snippet.trim().is_empty() {
            format!("request failed with status {}", status.as_u16())
        } else {
            snippet
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client_for(server: &MockServer) -> BrookClient {
        BrookClient::new(CoreConfig::new(&server.uri()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn login_success_resolves_user_and_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "a", "refresh_token": "r", "token_type": "bearer"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "u1", "handle": "alice", "display_name": "Alice", "global_role": "admin"
            })))
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        let mut state = client.state();
        let session = client.login("alice", "supersecret").await.unwrap();

        assert_eq!(session.user.handle, "alice");
        assert_eq!(session.access_token, "a");
        assert_eq!(
            *state.borrow_and_update(),
            AuthState::LoggedIn(session.user.clone())
        );
    }

    #[tokio::test]
    async fn login_rejected_maps_api_error_and_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": { "code": "auth.invalid_credentials", "message": "Invalid handle or password" }
            })))
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        let err = client.login("alice", "wrong").await.unwrap_err();

        match err {
            Error::Api { code, .. } => assert_eq!(code, "auth.invalid_credentials"),
            other => panic!("expected Api error, got {other:?}"),
        }
        assert!(matches!(*client.state().borrow(), AuthState::Failed(_)));
    }
}
