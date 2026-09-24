//! The client object exported to Swift.

use std::sync::Arc;

use brook_core::{BrookClient, CoreConfig};

use crate::runtime::runtime;
use crate::types::{LoginError, LoginResult};

/// Swift-facing wrapper around [`BrookClient`].
#[derive(uniffi::Object)]
pub struct FfiBrookClient {
    inner: Arc<BrookClient>,
}

#[uniffi::export]
impl FfiBrookClient {
    /// `allow_insecure_http` permits plain `http` to **any** host (dev only: the password
    /// and tokens travel in cleartext). Without it, `http` is accepted only for loopback.
    #[uniffi::constructor]
    pub fn new(base_url: String, allow_insecure_http: bool) -> Result<Arc<Self>, LoginError> {
        let config = CoreConfig::with_options(&base_url, allow_insecure_http)?;
        Ok(Arc::new(Self {
            inner: Arc::new(BrookClient::new(config)?),
        }))
    }

    /// Log in with a local handle + password.
    pub async fn login(&self, handle: String, password: String) -> Result<LoginResult, LoginError> {
        let inner = Arc::clone(&self.inner);
        let task = runtime().spawn(async move { inner.login(&handle, &password).await });
        match task.await {
            Ok(result) => Ok(LoginResult::LoggedIn {
                session: result?.into(),
            }),
            // A panic inside core must not cross the FFI as a crash.
            Err(_) => Err(LoginError::UnexpectedResponse),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    // Distinct sentinels: a swapped mapping cannot pass by accident.
    const ACCESS: &str = "access-sentinel-A";
    const REFRESH: &str = "refresh-sentinel-R";

    async fn mock_login_ok() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": ACCESS, "refresh_token": REFRESH, "token_type": "bearer"
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
        server
    }

    /// Test 1: Swift polls our futures on its own executor, with no Tokio runtime entered.
    /// A plain thread + `block_on` reproduces that; without the runtime hop, reqwest panics.
    #[test]
    fn login_works_when_polled_outside_any_tokio_runtime() {
        let mock_rt = tokio::runtime::Runtime::new().unwrap();
        let server = mock_rt.block_on(mock_login_ok());
        let uri = server.uri();

        let result = std::thread::spawn(move || {
            let client = FfiBrookClient::new(uri, false).unwrap();
            futures::executor::block_on(client.login("alice".into(), "pw".into()))
        })
        .join()
        .expect("login panicked when polled outside a Tokio runtime");

        assert!(matches!(result, Ok(LoginResult::LoggedIn { .. })));
        drop(server);
    }

    /// Test 2: tokens cross the mapping intact and unswapped.
    #[tokio::test]
    async fn login_success_carries_exact_tokens_and_user() {
        let server = mock_login_ok().await;
        let client = FfiBrookClient::new(server.uri(), false).unwrap();

        let LoginResult::LoggedIn { session } =
            client.login("alice".into(), "pw".into()).await.unwrap();

        assert_eq!(session.access_token, ACCESS);
        assert_eq!(session.refresh_token, REFRESH);
        assert_eq!(session.user.handle, "alice");
    }

    /// Test 3: the server's error code reaches Swift verbatim.
    #[tokio::test]
    async fn rejected_login_maps_to_api_error_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": { "code": "auth.invalid_credentials", "message": "Invalid handle or password" }
            })))
            .mount(&server)
            .await;
        let client = FfiBrookClient::new(server.uri(), false).unwrap();

        let err = client
            .login("alice".into(), "wrong".into())
            .await
            .unwrap_err();

        assert_eq!(
            err,
            LoginError::Api {
                code: "auth.invalid_credentials".into(),
                message: "Invalid handle or password".into(),
            }
        );
    }

    /// Test 8: the insecure-http opt-in is passed through, not assumed.
    #[test]
    fn insecure_http_to_a_remote_host_requires_the_explicit_flag() {
        let lan = "http://192.168.1.50:8080".to_string();
        assert!(matches!(
            FfiBrookClient::new(lan.clone(), false),
            Err(LoginError::InsecureServerUrl)
        ));
        assert!(FfiBrookClient::new(lan, true).is_ok());
    }
}
