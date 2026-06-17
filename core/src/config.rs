//! Core configuration.

use url::Url;

use crate::{Error, Result};

/// Configuration for a [`crate::BrookClient`].
#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// Base URL of the Brook server (e.g. `https://chat.example.com`).
    pub base_url: Url,
}

impl CoreConfig {
    /// Build a config from a server base URL.
    ///
    /// A trailing slash is added if missing so relative API paths join cleanly.
    /// **TLS is required** — only `https` is accepted, except plain `http` to a
    /// loopback host (`localhost`/`127.0.0.1`/`::1`) for local development and tests.
    /// This prevents ever sending credentials/tokens in cleartext.
    pub fn new(base_url: &str) -> Result<Self> {
        let mut s = base_url.to_string();
        if !s.ends_with('/') {
            s.push('/');
        }
        let url = Url::parse(&s)?;

        let host = url.host_str().ok_or(Error::MissingHost)?;
        let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
        match url.scheme() {
            "https" => {}
            "http" if is_loopback => {}
            _ => return Err(Error::InsecureServerUrl),
        }

        Ok(Self { base_url: url })
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
}
