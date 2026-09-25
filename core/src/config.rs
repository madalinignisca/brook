//! Core configuration.

use std::time::Duration;

use url::Url;

use crate::{Error, Result};

/// Configuration for a [`crate::BrookClient`].
#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// Base URL of the Brook server (e.g. `https://chat.example.com`).
    pub base_url: Url,
    /// Upper bound on any one REST request. Holders of the refresh lock (refresh, login,
    /// password change) make REST requests, so an unbounded request could block sign-in forever.
    pub(crate) request_timeout: Duration,
}

impl CoreConfig {
    /// Build a config from a server base URL.
    ///
    /// A trailing slash is added if missing so relative API paths join cleanly.
    /// **TLS is required** — only `https` is accepted, except plain `http` to a
    /// loopback host (`localhost`/`127.0.0.1`/`::1`) for local development and tests.
    /// This prevents ever sending credentials/tokens in cleartext.
    pub fn new(base_url: &str) -> Result<Self> {
        Self::with_options(base_url, false)
    }

    /// Like [`CoreConfig::new`], but `allow_insecure_http` additionally permits
    /// plain `http` to **any** host.
    ///
    /// **Development only** (e.g. a homelab LAN server without TLS yet) — it sends
    /// credentials in cleartext. The UI gates this behind an explicit opt-in.
    pub fn with_options(base_url: &str, allow_insecure_http: bool) -> Result<Self> {
        let mut s = base_url.to_string();
        if !s.ends_with('/') {
            s.push('/');
        }
        let url = Url::parse(&s)?;

        let host = url.host_str().ok_or(Error::MissingHost)?;
        let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
        match url.scheme() {
            "https" => {}
            "http" if is_loopback || allow_insecure_http => {}
            _ => return Err(Error::InsecureServerUrl),
        }

        Ok(Self {
            base_url: url,
            request_timeout: Duration::from_secs(30),
        })
    }

    /// Override the REST request bound (tests use a short one).
    #[doc(hidden)]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https() {
        assert!(CoreConfig::new("https://chat.example.com").is_ok());
    }

    #[test]
    fn accepts_http_only_for_loopback() {
        assert!(CoreConfig::new("http://127.0.0.1:8080").is_ok());
        assert!(CoreConfig::new("http://localhost").is_ok());
    }

    #[test]
    fn rejects_plain_http_to_remote() {
        assert!(matches!(
            CoreConfig::new("http://chat.example.com"),
            Err(Error::InsecureServerUrl)
        ));
    }

    #[test]
    fn allows_remote_http_only_with_explicit_opt_in() {
        assert!(CoreConfig::with_options("http://192.168.1.50:8080", true).is_ok());
        assert!(matches!(
            CoreConfig::with_options("http://192.168.1.50:8080", false),
            Err(Error::InsecureServerUrl)
        ));
    }
}
