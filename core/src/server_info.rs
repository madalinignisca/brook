// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What a server says about itself (`GET /health`): its version and where its source code is.
//!
//! Why this exists: Brook is AGPL-3.0, and §13 says users who talk to a modified server over a
//! network must be offered that server's source. The server publishes the link in `/health`
//! (`source_url`) and every client shows it in its About screen (spec
//! `docs/superpowers/specs/2026-10-08-agpl-source-url-spec.md` §4.3).
//!
//! This is a free function, not a `BrookClient` method, on purpose: About must work before
//! sign-in, with no session. A `BrookClient` brings a session store, a `Drop` that revokes the
//! token and background tasks, none of which About needs.
//!
//! The server is not trusted. Its answer is bounded in size and time, and the link is checked
//! and re-serialised before any UI may open it (see [`parse_source_url`]).

use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::config::CoreConfig;
use crate::error::{Error, Result};

/// Longest `source_url` we accept, in bytes of UTF-8, measured on the raw string from the JSON.
/// Must match `SOURCE_URL_MAX_BYTES` in `services/api/app/config.py`: the server refuses to
/// start with a longer one, so a longer one here can only come from a different or hostile
/// server. 2048 is the long-standing safe URL length; it bounds what we parse and display.
pub const SOURCE_URL_MAX_BYTES: usize = 2048;

/// The whole request (connect + answer + body), through `CoreConfig::with_request_timeout`.
/// A healthy `/health` answers in well under a second; 5 s is the bound core already uses for
/// restore's profile check, and short enough that About never looks hung (the default 30 s
/// would).
const SERVER_INFO_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest `/health` body we read. The real answer is under 200 bytes, but JSON may write every
/// character of a 2048-byte URL as a six-byte `\uXXXX` escape (~12 KiB), and a later server may
/// add fields. 64 KiB fits all that and stops a hostile server from making About buffer
/// megabytes.
const HEALTH_BODY_MAX_BYTES: usize = 64 * 1024;

/// Longest `version` we accept, in characters. Real ones look like `0.2.0-beta.7` (12).
/// 64 leaves room for a build suffix and keeps the About window's layout intact.
pub const VERSION_MAX_CHARS: usize = 64;

/// A server's version and a checked link to its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: String,
    /// A `Url`, so callers can only ever get the parsed, re-serialised form (an IDN host shows
    /// as punycode), never the server's raw string.
    pub source_url: Url,
}

/// The wire shape. Extra fields (`status`, future ones) are ignored.
#[derive(Deserialize)]
struct Health {
    version: String,
    source_url: String,
}

/// Fetch `/health` from `base_url`. Needs no session. The address rules are exactly sign-in's
/// (`CoreConfig::with_options`): a refused address fails before any network.
pub async fn server_info(base_url: &str, allow_insecure_http: bool) -> Result<ServerInfo> {
    fetch(info_config(base_url, allow_insecure_http)?).await
}

fn info_config(base_url: &str, allow_insecure_http: bool) -> Result<CoreConfig> {
    Ok(CoreConfig::with_options(base_url, allow_insecure_http)?
        .with_request_timeout(SERVER_INFO_TIMEOUT))
}

pub(crate) async fn fetch(config: CoreConfig) -> Result<ServerInfo> {
    // No redirects, same reason as `BrookClient::new`: the https rule only checks the
    // configured URL, so a followed redirect could leave it.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(config.request_timeout)
        .build()?;
    // No Authorization header: there is no token, and /health is public.
    let mut resp = http.get(config.base_url.join("health")?).send().await?;

    // A 3xx, 4xx or 5xx is not an answer, even if its body happens to look like health JSON
    // (a proxy error page, a redirect). The body is not read or echoed.
    if !resp.status().is_success() {
        return Err(Error::UnexpectedResponse);
    }
    // Early exit only: the header can be absent (chunked) or wrong, so the loop below is the
    // real bound.
    if resp
        .content_length()
        .is_some_and(|n| n > HEALTH_BODY_MAX_BYTES as u64)
    {
        return Err(Error::UnexpectedResponse);
    }
    // `.json()` / `.bytes()` buffer the whole body with no limit, so read chunk by chunk and
    // stop (dropping the response) as soon as the cap is passed.
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len() + chunk.len() > HEALTH_BODY_MAX_BYTES {
            return Err(Error::UnexpectedResponse);
        }
        buf.extend_from_slice(&chunk);
    }

    let health: Health = serde_json::from_slice(&buf).map_err(|_| Error::UnexpectedResponse)?;
    if !valid_version(&health.version) {
        return Err(Error::UnexpectedResponse);
    }
    let source_url = parse_source_url(&health.source_url).ok_or(Error::UnexpectedResponse)?;
    Ok(ServerInfo {
        version: health.version,
        source_url,
    })
}

/// True for 1 to `VERSION_MAX_CHARS` printable ASCII characters (0x20..=0x7E).
///
/// The version is shown to the user in About, and the server is not trusted: without this a
/// hostile one could fill the window with 60 KB of text, break the layout with newlines, or use
/// bidi control characters (U+202E) to make the text read as something else. A real version is
/// plain ASCII, so there is no reason to allow anything wider.
fn valid_version(v: &str) -> bool {
    (1..=VERSION_MAX_CHARS).contains(&v.chars().count())
        && v.chars().all(|c| matches!(c, '\u{20}'..='\u{7E}'))
}

/// `None` unless `raw` is a short http(s) URL with a host and no userinfo.
///
/// A hostile or broken server must not get a client to open a non-web link (`javascript:`,
/// `file:`) or one that hides its real host (`https://trusted.example@evil.example/`). The
/// returned `Url` re-serialises an IDN host in punycode, so a look-alike Unicode host is shown
/// as what it really is.
fn parse_source_url(raw: &str) -> Option<Url> {
    if raw.len() > SOURCE_URL_MAX_BYTES {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if url.host_str().is_none_or(str::is_empty) {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn health(source_url: &str) -> serde_json::Value {
        json!({"status": "ok", "version": "1.2.3", "source_url": source_url})
    }

    async fn serve(server: &MockServer, status: u16, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn info_of(server: &MockServer) -> Result<ServerInfo> {
        server_info(&server.uri(), false).await
    }

    async fn refused(source_url: &str) -> bool {
        let server = MockServer::start().await;
        serve(&server, 200, health(source_url)).await;
        matches!(info_of(&server).await, Err(Error::UnexpectedResponse))
    }

    /// The valid health JSON padded with trailing spaces (legal JSON) to exactly `len` bytes.
    fn padded(len: usize) -> Vec<u8> {
        let mut body = health("https://git.example.org/fork")
            .to_string()
            .into_bytes();
        assert!(body.len() <= len);
        body.resize(len, b' ');
        body
    }

    #[tokio::test]
    async fn returns_version_and_source_url() {
        let server = MockServer::start().await;
        serve(&server, 200, health("https://git.example.org/fork")).await;
        let info = info_of(&server).await.unwrap();
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.source_url.as_str(), "https://git.example.org/fork");
    }

    async fn version_accepted(version: &str) -> bool {
        let server = MockServer::start().await;
        let body = json!({"status": "ok", "version": version,
                          "source_url": "https://git.example.org/fork"});
        serve(&server, 200, body).await;
        match info_of(&server).await {
            Ok(info) => {
                assert_eq!(info.version, version);
                true
            }
            Err(Error::UnexpectedResponse) => false,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[tokio::test]
    async fn version_accepts_real_and_max_length() {
        assert!(version_accepted("0.2.0-beta.7").await);
        assert!(version_accepted(&"a".repeat(VERSION_MAX_CHARS)).await);
    }

    #[tokio::test]
    async fn version_refuses_empty_long_and_control_characters() {
        assert!(!version_accepted("").await);
        assert!(!version_accepted(&"a".repeat(VERSION_MAX_CHARS + 1)).await);
        assert!(!version_accepted("1.0\n2.0").await);
        assert!(!version_accepted("1.0\u{202E}0.1").await);
    }

    #[tokio::test]
    async fn unicode_host_comes_back_as_punycode() {
        let server = MockServer::start().await;
        serve(&server, 200, health("https://bücher.example/brook")).await;
        let info = info_of(&server).await.unwrap();
        assert_eq!(
            info.source_url.as_str(),
            "https://xn--bcher-kva.example/brook"
        );
    }

    #[tokio::test]
    async fn userinfo_is_refused() {
        assert!(refused("https://user:pw@evil.example/").await);
        assert!(refused("https://user@evil.example/").await);
    }

    #[tokio::test]
    async fn only_http_and_https_are_accepted() {
        for raw in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://x.example/",
            "mailto:a@b.example",
        ] {
            assert!(refused(raw).await, "{raw} must be refused");
        }
    }

    #[tokio::test]
    async fn length_limit_is_2048_bytes() {
        let make = |len: usize| {
            let prefix = "https://x.example/";
            format!("{prefix}{}", "a".repeat(len - prefix.len()))
        };
        let server = MockServer::start().await;
        serve(&server, 200, health(&make(SOURCE_URL_MAX_BYTES))).await;
        assert!(info_of(&server).await.is_ok());
        assert!(refused(&make(SOURCE_URL_MAX_BYTES + 1)).await);
    }

    #[tokio::test]
    async fn bad_answers_are_unexpected_response() {
        let server = MockServer::start().await;
        serve(&server, 200, json!({"status": "ok", "version": "1"})).await;
        assert!(matches!(
            info_of(&server).await,
            Err(Error::UnexpectedResponse)
        ));

        assert!(refused("").await);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
            .mount(&server)
            .await;
        assert!(matches!(
            info_of(&server).await,
            Err(Error::UnexpectedResponse)
        ));
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        // The target would answer a valid body; it must never be asked.
        let target = MockServer::start().await;
        serve(&target, 200, health("https://git.example.org/fork")).await;
        let front = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/health", target.uri())),
            )
            .mount(&front)
            .await;

        assert!(matches!(
            info_of(&front).await,
            Err(Error::UnexpectedResponse)
        ));
        assert!(target.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_error_status_with_a_valid_body_is_refused() {
        for status in [500, 404] {
            let server = MockServer::start().await;
            serve(&server, status, health("https://git.example.org/fork")).await;
            assert!(
                matches!(info_of(&server).await, Err(Error::UnexpectedResponse)),
                "status {status}"
            );
        }
    }

    #[tokio::test]
    async fn body_of_exactly_the_cap_is_accepted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(padded(HEALTH_BODY_MAX_BYTES), "application/json"),
            )
            .mount(&server)
            .await;
        assert!(info_of(&server).await.is_ok());
    }

    #[tokio::test]
    async fn body_over_the_cap_with_content_length_is_refused() {
        // Shows the early exit does not break the answer; it cannot tell it from the loop.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(padded(HEALTH_BODY_MAX_BYTES + 1), "application/json"),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            info_of(&server).await,
            Err(Error::UnexpectedResponse)
        ));
    }

    /// wiremock always sets Content-Length, so a chunked answer (no length: only the read loop
    /// can catch an oversize body) comes from a few lines of raw TCP.
    #[tokio::test]
    async fn chunked_body_over_the_cap_is_refused_by_the_read_loop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            // Write errors are ignored: the client hangs up once it passes the cap, so the
            // later writes fail by design and must not panic this task.
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                      Transfer-Encoding: chunked\r\n\r\n",
                )
                .await;
            for part in padded(HEALTH_BODY_MAX_BYTES + 1).chunks(8 * 1024) {
                let _ = sock
                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                    .await;
                let _ = sock.write_all(part).await;
                let _ = sock.write_all(b"\r\n").await;
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
        });

        let result = server_info(&format!("http://{addr}"), false).await;
        assert!(
            matches!(result, Err(Error::UnexpectedResponse)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn address_rules_are_signins() {
        // Refused before any network: nothing listens on chat.example.com here.
        assert!(matches!(
            server_info("http://chat.example.com", false).await,
            Err(Error::InsecureServerUrl)
        ));
        assert!(matches!(
            server_info("not a url", false).await,
            Err(Error::Url(_))
        ));
    }

    #[test]
    fn the_five_second_timeout_is_applied() {
        let config = info_config("https://chat.example.com", false).unwrap();
        assert_eq!(config.request_timeout, SERVER_INFO_TIMEOUT);
    }

    #[tokio::test]
    async fn a_slow_server_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(health("https://git.example.org/fork"))
                    .set_delay(Duration::from_secs(2)),
            )
            .mount(&server)
            .await;
        let config = CoreConfig::new(&server.uri())
            .unwrap()
            .with_request_timeout(Duration::from_millis(200));
        // The outer guard makes a missing client timeout fail instead of hang.
        let result = tokio::time::timeout(Duration::from_secs(3), fetch(config))
            .await
            .expect("fetch ignored its timeout");
        assert!(matches!(result, Err(Error::Http(_))), "{result:?}");
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_network_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let result = server_info(&format!("http://{addr}"), false).await;
        assert!(matches!(result, Err(Error::Http(_))), "{result:?}");
    }
}
