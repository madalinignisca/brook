# Plan: the server tells users where its source is (#300)

Spec: [2026-10-08-agpl-source-url-spec.md](2026-10-08-agpl-source-url-spec.md) (owner-approved).
One PR, branch `300-agpl-source-url`, `Closes #300`. Every commit is signed off (`git commit -s`,
CLAUDE.md §8, the DCO check). Every new file starts with the SPDX header
(`SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors` /
`SPDX-License-Identifier: AGPL-3.0-or-later`) in its language's comment form.

**Revision 2 (2026-10-08), after the Opus review and the owner's decisions.** Both owner
decisions are recorded as decided (§5). Core now bounds the `/health` body (64 KiB) and has a
failing test for its status check; the server guard refuses the hostnames core's parser would
refuse (`<>^|%`, a bad numeric last label, a bad `xn--` label); the macOS About window
refreshes on every appearance, is not restored at launch and is kept out of the Window menu;
`AboutModel` is built in `BrookApp.init()`; the link text is core's string; the binding calls
`brook_core::server_info` by path; the S1 tests use a strong JWT key; D1 drops PROTOCOL's
wrong "also on sfu" claim.

**Revision 3 (2026-10-08), after the Opus re-review.** The `xn--` check decodes with the
`punycode` codec instead of `idna` (the `idna` codec is IDNA 2003 and refused valid hosts such
as `xn--fa-hia`, faß); `\` joins the refused host characters; the numeric-last-label check uses
ASCII digits only and ignores one trailing dot; the whitespace rule no longer claims to cover
zero-width characters (now a named risk, with `0x` hex labels); M1's `refresh` checks the
generation before every write to `line`, the error path included, with a test; Risks names the
api image's `HEALTHCHECK`; the raw chunked test server ignores write errors.

## 1. Approach

The server reads one more setting, `BROOK_SOURCE_URL`, checks it at startup next to the JWT and
keyring guards, and adds it to the `/health` answer. Core gets one free function,
`server_info(base_url, allow_insecure_http)`, that builds the same `CoreConfig` sign-in builds
(so the same address rules), fetches `/health` with its own short-timeout, no-redirect HTTP
client, and returns `version` and a checked, re-serialised `source_url`. It is a free function,
not a `BrookClient` method, because About must work with no client and no session (before
sign-in), and constructing a `BrookClient` brings a session store, a `Drop` that revokes, and
background-task plumbing that About does not need. The Apple binding exports it as a free async
function. The Mac gets an `AboutModel` (all logic, tested in BrookTests) and a custom SwiftUI
About window that replaces the app menu's "About Brook" (owner decision 1).

Three numbers the spec leaves to the plan (the first is new in revision 2):
- **`/health` body cap: 64 KiB** (`HEALTH_BODY_MAX_BYTES` in core). The real answer is under
  200 bytes, but a legal answer can be much bigger than the URL's 2048 bytes: JSON may write
  every character of `source_url` as a six-byte `\uXXXX` escape, so 2048 bytes of URL can be
  ~12 KiB of JSON. 64 KiB leaves room for that and for extra fields a later server adds, and
  stops a hostile server from making About buffer megabytes.

- **URL max length: 2048 bytes** of UTF-8, on both sides, checked on the raw string (server: the
  configured value; core: the string in the JSON, before parsing). 2048 is the long-standing
  safe URL length; a repo or tag URL is far shorter, so no real operator hits it, and it bounds
  what a hostile server can make the client parse and display. Both sides name it
  `SOURCE_URL_MAX_BYTES` and carry a comment pointing at the other.
- **About fetch timeout: 5 s**, the whole request (connect + answer), set through the existing
  `CoreConfig::with_request_timeout`. A healthy `/health` answers in well under a second on a
  LAN or the internet; 5 s is the same bound core already uses for restore's profile check
  (`profile_wait`), and short enough that About never looks hung.

The `/health` answer becomes exactly:

```json
{"status": "ok", "version": "0.0.0", "source_url": "https://github.com/madalinignisca/brook"}
```

All three are strings; `source_url` is the configured value unchanged (or the default).

Alternatives considered and dropped: a `BrookClient` method (needs a client before sign-in, see
above); validating the value with a pydantic `field_validator` (it would raise a pydantic
`ValidationError` from every `Settings()` construction, including the CLI and alembic, instead
of the guard's clear `RuntimeError` at server start); a shared table of URL test cases between
Python and Rust (two languages, one file of glue; the length constant is the only number that
must match, and comments plus the risk note below cover it).

## 2. Steps

Each step is one commit, builds, and passes its area's checks on its own. For every new test the
implementer breaks the code it guards, watches it fail, and restores it (CLAUDE.md §3); the
"mutant" line of each step names the break to try.

### Server (`brook-server-implementer`)

**S1. `api: BROOK_SOURCE_URL setting with a startup guard`**
- `services/api/app/config.py`:
  - `DEFAULT_SOURCE_URL = "https://github.com/madalinignisca/brook"` and
    `SOURCE_URL_MAX_BYTES = 2048` (module constants; comment: the same number is
    `SOURCE_URL_MAX_BYTES` in `core/src/server_info.rs`, so a value the server accepts is never
    refused by a client; change both together).
  - `source_url: str = DEFAULT_SOURCE_URL`, with a comment saying why it exists (AGPL §13: a
    modified server must point its users at its own source; spec §4.1).
  - A `field_validator("source_url", mode="before")` that maps `""` to `DEFAULT_SOURCE_URL`,
    same reasoning and comment style as `_unset_primary_is_none` (compose passes an unset
    `${VAR:-}` as `""`).
  - `_assert_source_url()`, called from `assert_secure()` (update its docstring). It refuses
    (`RuntimeError`) unless all hold:
    - `len(value.encode("utf-8")) <= SOURCE_URL_MAX_BYTES`;
    - no whitespace or control character anywhere (`c.isspace()`, `ord(c) < 0x20`, `0x7f`).
      Comment the real reason: core's `url` parser does not refuse all of these, it rewrites
      them (it strips leading/trailing C0 and spaces, removes tabs and newlines, and
      percent-encodes an inner space in the path), so the link a client shows would differ from
      the configured value. Refusing them here keeps "what the operator set" and "what users
      see" the same. Zero-width and other format characters (U+200B and the like) are not
      whitespace to Python and are not covered here; see Risks;
    - `urlsplit(value)` succeeds and reading `.port` does not raise (`ValueError` = refuse);
    - `scheme in ("http", "https")` (urlsplit lowercases it), `hostname` is truthy;
    - `"@" not in netloc` (catches `user@`, `user:pass@`, and the empty `@host` that
      `.username` reports as `""` rather than `None`);
    - the host would also pass core's WHATWG host parser, for the cases `urlsplit` lets
      through (checked against Python 3: `urlsplit` accepts all three below and returns the
      host unchanged). On `hostname` (already lowercased by `urlsplit`; skip these for an
      IPv6 literal, i.e. a `[` in netloc):
      - no character from `<>^|%\` (WHATWG forbidden host code points that `urlsplit` keeps;
        `%` also covers a percent-encoded host, which `url` decodes and then re-checks; `\` is
        a path separator to WHATWG for http(s), so core would show
        `https://good.example\evil/` as `https://good.example/evil/`, a different link);
      - numeric last label: let `host` be `hostname` with one trailing `.` removed (WHATWG
        ignores one trailing empty label, so `1.2.3.4.` is an IPv4 address and `example.123.`
        is refused). If `host`'s last dot-separated label is ASCII digits only
        (`label.isascii() and label.isdigit()`; plain `isdigit()` is also true for `²` or
        Arabic-Indic digits, which WHATWG does not treat as numbers),
        `ipaddress.IPv4Address(host)` must succeed (WHATWG parses such a host as IPv4 and
        refuses `1.2.3.256` or `example.123`). `0x` hex labels are not handled (see Risks);
      - every label of `host` starting with `xn--` must decode as punycode:
        `label[4:].encode("ascii").decode("punycode")` must not raise (`UnicodeError`), and
        the result must contain at least one non-ASCII character (an all-ASCII result, e.g.
        `xn--abc-`, is not a valid IDN label) and no C0 or C1 control character
        (`ord(c) < 0x20 or 0x7f <= ord(c) <= 0x9f`); `xn--a` decodes to U+0080 and is refused.
        Comment why not `.decode("idna")`: that codec is IDNA 2003 (nameprep) and refuses
        valid IDNA 2008 hosts that `url` accepts, such as `xn--fa-hia` (faß), `xn--zca` (ß)
        and `xn--nxasmm1c` (βόλος).
      Comment that this list is the known gap, not a full UTS 46 implementation (see Risks).
  - The message names the setting and the rule, and **never echoes the value** (a userinfo
    value may hold a password): `BROOK_SOURCE_URL must be an absolute http(s) URL with a host,
    no user or password, at most 2048 bytes. Unset it to use the upstream repository.`
  - `allow_insecure_auth` does not excuse a malformed value (say so in the comment: the dev
    hatch is about the JWT key and keyring only).
- `services/api/app/routers/health.py`: answer
  `{"status": "ok", "version": __version__, "source_url": get_settings().source_url}`; docstring
  says the route is public on purpose (AGPL §13, spec §4.2) and reads no token.
- Tests first:
  - `tests/test_config.py` (new tests; the file has the header already). Every test builds
    `Settings(jwt_signing_key="x" * 40, allow_insecure_auth=False, source_url=...)` (or sets the
    env and passes the same strong key) and calls `assert_secure()`, so an accepted case cannot
    fail on the JWT guard and a refused case cannot pass on the JWT guard's error. The keyring
    is already valid in every test (`conftest.py`'s autouse `_test_secret_keyring`).
    - empty env value means the default: `monkeypatch.setenv("BROOK_SOURCE_URL", "")`, then
      `Settings(jwt_signing_key="x" * 40).source_url == DEFAULT_SOURCE_URL` and
      `assert_secure()` passes.
      *Mutant: drop the validator → the guard refuses `""`, test red.*
    - unset means the default (`monkeypatch.delenv(..., raising=False)`).
    - refused, each with `pytest.raises(RuntimeError, match="BROOK_SOURCE_URL")`, parametrized:
      `"ftp://example.com/x"`, `"javascript:alert(1)"`, `"file:///etc/passwd"`,
      `"github.com/madalinignisca/brook"` (no scheme), `"https://"`, `"https:///path"`,
      `"https://user:secret-pw@example.com/"`, `"https://user@example.com/"`,
      `"https://@example.com/"`, `"https://exa mple.com/"`, `" https://example.com/"`,
      `"https://example.com:99999/"`, `"https://[::1/"`, `"https://a<b.com/"`,
      `"https://a%b.com/"`, `"https://good.example\\evil/"` (a Python literal, i.e. one
      backslash), `"https://1.2.3.256/"`, `"https://example.123/"`,
      `"https://example.123./"`, `"https://xn--a.com/"`, and
      `"https://e.com/" + "a" * 2035` (2049 bytes).
      *Mutants: remove the `@` check; remove the scheme check; change `<=` to `<` or drop the
      length check; drop the forbidden-character check, or only `\` from it; drop the
      numeric-label check; take the last label without removing the trailing dot
      (`example.123.` then passes); drop the `xn--` check — each turns at least one case red.*
    - accepted: `"http://example.com"`, `"https://bücher.example/brook"` (the server keeps it
      as is; core punycodes it), `"https://xn--bcher-kva.example/brook"` and
      `"https://xn--fa-hia.de/"` (valid punycode; the second is refused by the `idna` codec),
      `"https://192.0.2.1/brook"` and `"https://192.0.2.1./brook"` (valid IPv4 hosts),
      `"https://[2001:db8::1]/brook"` (IPv6, skipped by the host checks), and a URL of exactly
      2048 bytes (boundary).
      *Mutants: apply the numeric-label check without the `IPv4Address` test → the IPv4 cases
      go red; pass `hostname` instead of the dot-stripped `host` to `IPv4Address` →
      `192.0.2.1.` goes red; decode `xn--` labels with the `idna` codec → `xn--fa-hia.de`
      goes red.*
    (All cases above were checked against a sketch of this guard under Python 3: every refused
    case is refused and every accepted case accepted.)
    - the message never contains `"secret-pw"`.
    - `allow_insecure_auth=True` still refuses `"javascript:alert(1)"`.
  - `tests/test_health.py`:
    - default answer: body is exactly
      `{"status": "ok", "version": __version__, "source_url": DEFAULT_SOURCE_URL}` (exact dict,
      so an extra or renamed key fails).
    - configured: `monkeypatch.setenv("BROOK_SOURCE_URL", "https://git.example.org/fork")`,
      then `config.get_settings.cache_clear()` **inside the test** (the `client` fixture's
      `create_app()` already cached settings; httpx's ASGITransport does not run the lifespan),
      then `/health` answers that string unchanged.
      *Mutant: hardcode the default in `health.py` → red.*
    - public: a request with `Authorization: Bearer not-a-token` still gets 200 and the field.
  - `tests/test_config.py` (or `test_health.py`), wiring: with a malformed
    `BROOK_SOURCE_URL` in the env, `with TestClient(create_app()):` raises `RuntimeError`
    matching `BROOK_SOURCE_URL` (the lifespan runs `assert_secure`). Use the `sync_client`
    setup pattern from `conftest.py` (DB URL, JWT key, `get_settings.cache_clear()`), but expect
    the raise on enter.
    *Mutant: stop calling `_assert_source_url()` from `assert_secure()` → red.*
- Run (from `services/api`, after `uv sync --locked --extra dev`):
  `uv run pytest tests/test_config.py tests/test_health.py`, then before the push the full
  CLAUDE.md §3 line (ruff, ruff format, mypy, coverage pytest, bandit, pip-audit).
- Docs it makes wrong: PROTOCOL (`/health` row), admin guide (`/health` sample output, settings,
  troubleshooting). Fixed in D1.

**S2. `deploy: pass BROOK_SOURCE_URL through compose; mention it in api.env and .env.example`**
- `deploy/docker-compose.yml`, api `environment:`: `BROOK_SOURCE_URL: ${BROOK_SOURCE_URL:-}`,
  with a comment: an operator running a modified server sets it in `.env` (AGPL §13); empty
  means the upstream default, which lives only in the server (`app/config.py`).
- `deploy/.env.example`: a commented block, e.g.
  `# AGPL §13: if you run a MODIFIED Brook, set this to where its source is.` /
  `# BROOK_SOURCE_URL=https://git.example.org/you/brook`.
- `deploy/native/install.sh`: one commented line in the `api.env` heredoc written on first
  install (`# BROOK_SOURCE_URL=...` with the same one-line reason). The heredoc is unquoted
  (`<<EOF`), so the line must contain no `$` or backtick. Nothing is appended to an existing
  `api.env` (spec §3).
- Tests: no unit test (config text). Checks the implementer runs and pastes into the PR:
  - `cd deploy && POSTGRES_PASSWORD=x BROOK_JWT_SIGNING_KEY=$(head -c 40 /dev/zero | tr '\0' x) docker compose config | grep BROOK_SOURCE_URL`
    shows `BROOK_SOURCE_URL: ""`; with `BROOK_SOURCE_URL=https://git.example.org/fork`
    prepended it shows that value. (`config` starts nothing; nothing to clean up.)
  - `bash -n deploy/native/install.sh`, and read the generated heredoc text.
- Docs it makes wrong: admin guide (both deployments). Fixed in D1.

### Core and the Apple binding (`brook-core-implementer`)

**C1. `core: server_info fetches a server's version and source link with no session`**
- New `core/src/server_info.rs` (SPDX header, module doc saying why: AGPL §13, spec §4.3):
  - `pub const SOURCE_URL_MAX_BYTES: usize = 2048;` (comment pointing at
    `services/api/app/config.py`).
  - `const SERVER_INFO_TIMEOUT: Duration = Duration::from_secs(5);` with the reason above.
  - `pub struct ServerInfo { pub version: String, pub source_url: url::Url }`
    (`Debug, Clone, PartialEq, Eq`). `source_url` is a `Url`, so callers can only ever get the
    parsed form.
  - `pub async fn server_info(base_url: &str, allow_insecure_http: bool) -> Result<ServerInfo>`:
    `fetch(info_config(base_url, allow_insecure_http)?).await`.
  - `fn info_config(...) -> Result<CoreConfig>`: `CoreConfig::with_options(base_url,
    allow_insecure_http)?.with_request_timeout(SERVER_INFO_TIMEOUT)`. Same address rules as
    sign-in (spec §4.3): a refused address is `Error::Url`, `MissingHost` or
    `InsecureServerUrl`, before any network.
  - `pub(crate) async fn fetch(config: CoreConfig) -> Result<ServerInfo>`: a
    `reqwest::Client` with `redirect(Policy::none())` (same reason as `BrookClient::new`: the
    https rule only checks the configured URL) and `timeout(config.request_timeout)`;
    `GET base_url.join("health")`; no `Authorization` header (there is no token to send).
    Then, in this order:
    1. `if !status.is_success()` → `Error::UnexpectedResponse` (3xx included; no body is read
       or echoed: a redirect or proxy page is not an answer, even one that looks like
       health JSON).
    2. `if resp.content_length().is_some_and(|n| n > HEALTH_BODY_MAX_BYTES as u64)` →
       `UnexpectedResponse` without reading (an early exit only; the header can be absent or
       wrong, so step 3 is the real bound).
    3. Read with a `while let Some(chunk) = resp.chunk().await? { ... }` loop into a `Vec<u8>`;
       if the total would pass `HEALTH_BODY_MAX_BYTES` → `UnexpectedResponse` at once (stop
       reading, drop the response). `reqwest`'s `.json()` / `.bytes()` are not used because they
       buffer the whole body with no limit.
    4. `serde_json::from_slice::<Health>(&buf)` into a private
       `#[derive(Deserialize)] struct Health { version: String, source_url: String }` (extra
       fields ignored); a parse failure → `UnexpectedResponse`.
    Transport failure or timeout (the client timeout covers the body reads too) → `Error::Http`
    via `?`.
  - `const HEALTH_BODY_MAX_BYTES: usize = 64 * 1024;` with the reason from §1.
  - `fn parse_source_url(raw: &str) -> Option<Url>`: `None` unless `raw.len() <=
    SOURCE_URL_MAX_BYTES`, `Url::parse` succeeds, scheme is `http` or `https`, `host_str()` is
    `Some` and non-empty, `username()` is empty and `password()` is `None`. `None` →
    `UnexpectedResponse`. Comment: a hostile or broken server must not get a client to open a
    non-web link or one that hides its real host; the re-serialised form shows an IDN host in
    punycode (spec §4.3).
- `core/src/lib.rs`: `mod server_info;` and
  `pub use server_info::{server_info, ServerInfo, SOURCE_URL_MAX_BYTES};`.
- No new dependency (`reqwest`, `url`, `serde`, `serde_json`, `wiremock` and tokio's `net`
  in tests are already there). No new error
  kind.
- Tests first, in a `#[cfg(test)] mod tests` in the same file (wiremock, like `client.rs`):
  - ok: mock `GET /health` → `{"status":"ok","version":"1.2.3","source_url":"https://git.example.org/fork"}`;
    result has both fields. (No test that no `Authorization` header is sent: `fetch` has no
    token to send and builds its own client, so there is no code path that could add one.)
  - punycode: `"https://bücher.example/brook"` → `info.source_url.as_str() ==
    "https://xn--bcher-kva.example/brook"`. *Mutant: keep and return the raw string → red.*
  - userinfo refused: `"https://user:pw@evil.example/"` and `"https://user@evil.example/"` →
    `Err(Error::UnexpectedResponse)`. *Mutant: drop the userinfo check → red.*
  - other schemes refused: `"javascript:alert(1)"`, `"file:///etc/passwd"`, `"ftp://x.example/"`,
    `"mailto:a@b.example"`. *Mutant: drop the scheme check → red.*
  - length: a 2048-byte `https://` URL is accepted, 2049 bytes refused.
  - bad answers → `UnexpectedResponse`: missing `source_url`; `source_url: ""`; a non-JSON
    body; a 302 whose `Location` is a second mock server that would answer a valid body
    (assert that second server got **no** request).
  - status check: a **500** and a **404**, each whose body is the valid health JSON of the ok
    test → `UnexpectedResponse`. The body must be valid, or the parse step would refuse it and
    the test could not see the status check go missing.
    *Mutant: remove `if !status.is_success()` → both parse fine and return `Ok`, red.*
  - body cap:
    - exactly `HEALTH_BODY_MAX_BYTES` bytes (the ok JSON padded with trailing spaces, which
      JSON allows) via wiremock → `Ok` (boundary).
    - `HEALTH_BODY_MAX_BYTES + 1` bytes, same padding, **with no `Content-Length`**, so only
      the read loop can catch it. wiremock always sets `Content-Length`, so serve this one
      from a few lines of raw `tokio::net::TcpListener` on `127.0.0.1:0` (`net` and
      `io-util` are already in core's tokio features): read the request, write
      `HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n`
      and the body as chunks, then `0\r\n\r\n` → `UnexpectedResponse`. The server task ignores
      write errors (`let _ = sock.write_all(...).await;`): the client drops the connection
      as soon as it passes the cap, so the later writes fail by design and must not panic
      the task.
      *Mutant: drop the cap check in the read loop → the padded JSON parses, `Ok`, red.*
    - `HEALTH_BODY_MAX_BYTES + 1` bytes via wiremock (with `Content-Length`) →
      `UnexpectedResponse`. This one cannot tell the two checks apart; it only shows the
      early exit does not break the answer.
  - address rules: `server_info("http://chat.example.com", false)` → `InsecureServerUrl`
    without touching the network; `server_info("not a url", false)` → `Error::Url`.
  - timeout: `fetch(CoreConfig::new(&uri)?.with_request_timeout(200ms))` against a mock with
    `set_delay(2s)` → `Err(Error::Http(_))`. And `info_config("https://chat.example.com",
    false)?.request_timeout == SERVER_INFO_TIMEOUT` (so the 5 s is really applied).
    *Mutant: build the client without `.timeout(...)` → the delay test hangs past the
    test's own `tokio::time::timeout(3s, ...)` guard and fails; wrap the call in one.*
  - unreachable: a closed port (bind a `TcpListener` to `127.0.0.1:0`, take its port, drop it)
    → `Err(Error::Http(_))`.
- Run (repo root): `cargo test --locked -p brook-core server_info`, then the full
  `cargo fmt --all -- --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`.
- Docs it makes wrong: none yet (`core/README.md` if it lists the public surface; check and
  hand to D1 if so).

**C2. `core: Apple binding exports server_info`**
- `bindings/apple/src/types.rs`: `#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)] pub
  struct FfiServerInfo { pub version: String, pub source_url: String }` and
  `impl From<brook_core::ServerInfo>` using `source_url.as_str().to_owned()` (the parsed form,
  never the server's raw string). No change to `LoginError` or its `From<Error>` mapping: the
  existing arms already give `Network` / `UnexpectedResponse` / `InvalidServerUrl` /
  `InsecureServerUrl` (spec §4.3).
- `bindings/apple/src/client.rs`: a free function next to the `FfiBrookClient` impl:
  ```rust
  /// No session: works on the sign-in screen. Same address rules as `FfiBrookClient::new`.
  #[uniffi::export]
  pub async fn server_info(base_url: String, allow_insecure_http: bool)
      -> Result<FfiServerInfo, LoginError>
  ```
  whose body calls core by full path, `brook_core::server_info(&base_url,
  allow_insecure_http)`: a bare `server_info(...)` inside it would name the exported function
  itself (recursion), and a `use brook_core::server_info` would clash with it. It goes
  through the existing `run(...)` runtime hop (imported from `crate::call`) (Swift polls futures outside any Tokio
  runtime; without the hop reqwest panics, see the file's Test 1).
- `bindings/apple/src/lib.rs`: export `client::server_info` and `types::FfiServerInfo`.
- Tests first (`client.rs` tests module):
  - polled outside any Tokio runtime (copy the Test 1 pattern: a plain thread +
    `futures::executor::block_on`), against a wiremock `/health` → `Ok` with the exact
    `version` and a punycoded `source_url` from a Unicode host input. *Mutant: call core
    directly without `run` → panics, red.*
  - mapping: userinfo answer → `LoginError::UnexpectedResponse`; `"http://chat.example.com"`
    with `false` → `LoginError::InsecureServerUrl`; closed port → `LoginError::Network { .. }`.
- Run: `cargo test --locked -p brook-ffi server_info`, then the full Rust line above.
- On the owner's Mac only: `bindings/apple/build-xcframework.sh` regenerates the Swift
  (`serverInfo(baseUrl:allowInsecureHttp:)`, `FfiServerInfo`); `clients/macos/build.sh` runs it
  anyway in M1. No Swift test in the BrookCore package is added (the Mac model test covers the
  Swift side through an injected fetch).

### macOS (`brook-apple-implementer`)

This VM has no Mac and the macOS agent is offline. M1 can be **written** here, but
`clients/macos/build.sh test` and the manual checks below run only on the owner's Mac, and the
PR says so. All logic goes into `AboutModel` so BrookTests cover it without UI or network.

**M1. `mac: About shows the server's version and source link`**
- New `clients/macos/Brook/AboutModel.swift` (SPDX header):
  - `@MainActor @Observable final class AboutModel`.
  - `enum Line: Equatable { case noServer, loading, loaded(version: String, sourceText: String, sourceURL: URL), failed }`.
    `sourceText` is core's string (`info.sourceUrl`) unchanged and is what the view shows;
    `sourceURL` is only for opening it. Showing `URL.absoluteString` instead would let
    Foundation's own parsing and re-serialising change the text (core's re-serialised form is
    the one spec §4.3 promises, e.g. an IDN host in punycode). `failed` carries nothing, so a
    failure can never show a link or a cached or upstream URL (spec §4.4; say so in the
    comment).
  - `enum Target: Equatable { case none, invalid, server(String) }` and
    `static func target(signedInServer: String?, typed: String, remembered: String?) -> Target`,
    in this order:
    1. the signed-in server, if any → `.server(it)`;
    2. typed field empty after trimming → `.none`;
    3. **fresh install** (owner decision 2): `remembered == nil` and the trimmed field equals
       `Settings.fallbackServer` (`"https://localhost"`, what `Settings.serverPrefill` puts in
       the field when nothing was ever remembered) → `.none`, so About says "No server yet."
       and sends no request. Comment: the untouched prefill is not a server the user chose;
       once a sign-in succeeded, `lastGoodServer` is set and a typed `https://localhost` is
       fetched like any other address;
    4. the field via `ServerAddress.parse` (the sign-in screen's own check, so About never
       contacts an address sign-in would refuse): failure → `.invalid`; success →
       `.server(trimmed)`.
  - `typealias Fetch = @Sendable (_ address: String, _ allowInsecureHttp: Bool) async throws -> FfiServerInfo`,
    `static let live: Fetch = { try await serverInfo(baseUrl: $0, allowInsecureHttp: $1) }`.
  - `init(allowInsecureHTTP: @escaping () -> Bool, target: @escaping () -> Target, fetch: @escaping Fetch = AboutModel.live)`.
    `target` is a closure so the model reads the live store and form at each refresh (built in
    `BrookApp.init()`, below); tests pass a closure that returns a fixed target.
  - `private(set) var line: Line = .noServer`; `private var generation = 0`;
    `private(set) var opens = 0` and `func open() { opens += 1 }` (the command calls it; the
    view keys its refresh on it, below).
  - `func refresh() async`: `generation += 1; let mine = generation`; `switch target()`:
    `.none` → `.noServer`; `.invalid` → `.failed`; `.server(a)` → `.loading`,
    `await fetch(a, allowInsecureHTTP())`, then
    `.loaded(version: info.version, sourceText: info.sourceUrl, sourceURL: url)` when
    `URL(string: info.sourceUrl)` gives `url`, else `.failed`; `.failed` on any throw.
    **Every write to `line` after the `await` happens only if `generation == mine`**, the
    `catch` included: a newer open wins over a late answer, and an older run that was
    cancelled (`.task(id:)` cancels it, so the fetch throws `CancellationError`) or fails late
    must not overwrite a newer `.loaded` with `.failed`. Simplest form: one
    `guard generation == mine else { return }` at the top of both the success path and the
    `catch` block. Comment the why.
  - `enum Text` with the strings: "No server yet." / "Fetching…" / "Couldn't fetch this
    server's source link." and the labels "Server version" / "Server source".
- `clients/macos/Brook/SessionStore.swift`: `private(set) var server: String?`, set to
  `address` in `finishSignIn`, cleared in `end()`. (Today the signed-in address is only in
  `settings.lastGoodServer`, which a second instance can overwrite.)
- New `clients/macos/Brook/AboutView.swift` (SPDX header):
  - `AboutView(model:)`: app icon (`NSApp.applicationIconImage`), "Brook", the version from
    `CFBundleShortVersionString` / `CFBundleVersion`, and the `NSHumanReadableCopyright` string
    from `Bundle.main` (the #298 text stays defined once, in `project.yml`); then the server
    lines from `model.line`: `.loaded` shows "Server version: X" and "Server source:" with
    `Link(sourceText, destination: sourceURL)`, opened by the system browser; the app never
    fetches the link.
  - **Refresh on every appearance**: `.task(id: model.opens) { await model.refresh() }` on the
    view. SwiftUI runs it each time the window appears (however it was opened: the command,
    the Window menu, a restore) and again whenever `opens` changes while it is shown, cancelling
    the previous run (the generation check drops a cancelled run's late result). So the window
    can never show a server it fetched for an earlier open, and an open of a closed window
    fetches once, not twice (the command does not call `refresh` itself). Comment the why:
    a window that only refreshed on the command showed stale data when reopened from the
    Window menu or restored at launch (Opus review, revision 2).
  - `AboutCommand(model:)`: a `Button("About Brook")` reading `@Environment(\.openWindow)`,
    which calls `model.open()` then `openWindow(id: "about")`. When the window is already
    open, `open()` changes `opens`, so the view fetches again (spec §4.4: every open fetches).
- `clients/macos/Brook/BrookApp.swift`:
  - `@State private var about: AboutModel`, built in `init()` next to `store` and `form`
    (the closures need those instances, which a property initialiser cannot reach):
    ```swift
    let form = LoginForm(store: store)
    _form = State(initialValue: form)
    _about = State(initialValue: AboutModel(
        allowInsecureHTTP: { store.settings.allowInsecureHTTP },
        target: { AboutModel.target(signedInServer: store.server, typed: form.server,
                                    remembered: store.settings.lastGoodServer) }))
    ```
  - on the main `Window` scene:
    `.commands { CommandGroup(replacing: .appInfo) { AboutCommand(model: about) } }`;
  - a new `Window("About Brook", id: "about") { AboutView(model: about) }` with
    `.windowResizability(.contentSize)`, `.restorationBehavior(.disabled)` (a restored About
    would reopen at launch for whatever server is current then; not restoring it is simpler)
    and `.commandsRemoved()` (keeps the scene's own item out of the Window menu, so the app
    menu's "About Brook" is the one way in). Both are `Scene` modifiers available on the
    deployment target (`project.yml`: macOS 26.0; `restorationBehavior` is macOS 15+,
    `commandsRemoved` macOS 13+). The appearance refresh above stays even so: it is what makes
    any path that still shows the window correct.
- Tests first, new `clients/macos/BrookTests/AboutModelTests.swift` (SPDX header), with a fake
  `Fetch` (records its calls; returns a value, throws, or waits on a continuation) and a
  `target` closure returning the case under test:
  - `target`: signed-in server wins over the typed field; empty/whitespace field → `.none`;
    `"https://u:p@x.example"` → `.invalid` (same as the sign-in check); valid field →
    `.server`.
  - fresh install: `target(signedInServer: nil, typed: Settings.fallbackServer, remembered: nil)`
    → `.none`; the same with `remembered: "https://chat.example.com"` →
    `.server("https://localhost")`; and `typed: " https://localhost "` with `remembered: nil` →
    `.none` (compared after trimming). *Mutant: drop the fallback comparison → the first case
    is `.server`, red.*
  - loaded: fake returns `FfiServerInfo(version: "1.2.3", sourceUrl: "https://xn--bcher-kva.example/brook")`
    → `.loaded(version: "1.2.3", sourceText: "https://xn--bcher-kva.example/brook", sourceURL: URL(string: "https://xn--bcher-kva.example/brook")!)`.
  - **unreachable shows no link and no fallback**: fake throws `LoginError.Network(message: "x")`
    → `line == .failed`, exactly. Same for `.UnexpectedResponse` and `.InsecureServerUrl`.
    *Mutant: on failure set `.loaded` with the upstream URL → red.*
  - no server → `.noServer` and the fake was never called; `.invalid` → `.failed`, never
    called.
  - insecure flag passed through: `allowInsecureHTTP` returns true → the fake saw `true`.
  - newest open wins: start refresh A (gated), then refresh B (answers at once), then release
    A with another URL → `line` is B's. *Mutant: drop the generation check → red.*
  - a late failure does not win: start refresh A (gated), then refresh B (answers at once,
    `.loaded`), then release A by **throwing** (`CancellationError()`) → `line` is still B's
    `.loaded`. *Mutant: no generation check in the `catch` → `line == .failed`, red.*
  - every open fetches again: two `refresh` calls with the same target → two fake calls.
  - `open()` increments `opens` (the view's refresh key).
  - `SessionStoreTests`: after a successful sign-in `store.server == "https://chat.example.com"`,
    after sign-out `nil` (use the existing `FakeClient` / `FactoryRecorder` pattern).
- Run (owner's Mac): `clients/macos/build.sh test`.
- Manual checks on the Mac (in the PR's evidence, by the owner): About from the app menu
  (1) fresh install (no remembered server, field still `https://localhost`) → "No server
  yet.", and the server log shows no request; (2) signed out with a reachable server typed →
  version and link; (3) signed in → the signed-in server's values; (4) server stopped →
  "Couldn't fetch…", no link; (5) clicking the link opens the default browser; (6) with
  `BROOK_SOURCE_URL` changed and the server restarted, choosing About again while the window
  is open shows the new link; (7) the copyright and license line from #298 still shows;
  (8) About Brook is not listed in the Window menu; if it is, open it from there after
  switching servers (sign out, type a second server) and see the new server's link;
  (9) quit with About open, relaunch: About does not reopen by itself.
- Docs it makes wrong: user guide (About). Fixed in D1.

### Docs (`brook-docs-writer`, after the code works)

**D1. `docs: the server source link (AGPL §13)`**
- `docs/PROTOCOL.md`: move `GET /health` out of the §1 table whose base is `/api/v1` (it is
  served at `/health`) into a short note right under the table: public, no token read (a sent
  one is ignored), answers exactly the JSON shape in §1 of this plan; `source_url` is an
  absolute http(s) URL with no userinfo, at most 2048 bytes; clients display and open only the
  parsed form and treat anything else as a failed fetch; a client reads at most 64 KiB of it.
  Drop the row's "also on `sfu`": checked against `services/sfu`, the SFU is Janus with only
  the WebSocket transport configured (`janus.transport.websockets.jcfg`, admin API off) and
  serves no HTTP `/health`. The only `/health` is the api's; the gateway's Caddyfile proxies
  everything to `api:8000`, which is why `curl <gateway>/health` works.
- `docs/admin-guide.md`: a "Source link (AGPL §13)" section: the setting, its default, empty
  means default, the startup refusal and its message, the operator's duty for a modified server
  (Brook cannot check the URL), how to set it (compose `.env`; native `/etc/brook/api.env`
  then `systemctl restart brook-api`), and `curl <server>/health` to check. Update the
  existing `/health` sample output (line ~41) and add a troubleshooting row for the refusal.
- `docs/user-guide.md`: in the macOS section, where About is (app menu → About Brook) and what
  "Server version" and "Server source" mean, including "couldn't fetch".
- `README.md` §License: one sentence that a modified server must set `BROOK_SOURCE_URL` (link
  to the admin guide section).
- `deploy/README.md` / `deploy/native/README.md`: only if they show `/health` output; keep in
  line with the admin guide.
- Check: every `/health` mention in `docs/` and `deploy/` (`grep -rn "/health" docs deploy`)
  matches the new shape.

## 3. Migrations and production

No schema change. The setting has a default, so an existing deployment (the busuioc native
install, the compose test server) keeps working without touching its env file and starts
answering `source_url` with the upstream URL after the upgrade. Rollback: revert the PR; an
`api.env` or `.env` that gained `BROOK_SOURCE_URL` is harmless to the older server (pydantic
settings use `extra="ignore"`). Production is not touched by this PR; deploying it is a separate,
owner-approved step.

## 4. Risks

- **The server accepts a value core refuses.** Python's `urlsplit` and Rust's WHATWG parser
  differ. S1 closes the known gaps (whitespace and controls, the forbidden host characters
  `<>^|%\`, a bad numeric last label, bad `xn--` punycode), but the server does not run full
  UTS 46 processing on a Unicode host or match WHATWG fully. Left open, on purpose
  (CLAUDE.md §4): a host with characters UTS 46 disallows (some symbols, certain bidi mixes)
  or one whose mapped form is too long; zero-width and other format characters (U+200B,
  U+00AD and the like), which UTS 46 drops or maps, so core shows a different host than the
  one configured; full-width digits in the last label, which UTS 46 maps to ASCII digits;
  `0x` hex labels (`example.0x1` is refused by core's IPv4 parser, `0x7f.1` is rewritten to
  `127.0.0.1`); and an `xn--` label that decodes but fails IDNA 2008's other rules. Any of
  these can pass the server and be refused (or rewritten) by core. Then `/health` serves it and every About shows "couldn't fetch". Adding the
  `idna` package to the server would close it, at the cost of a dependency for a typo-class
  failure that the operator sees at once. Check: the admin guide tells operators to open About
  once after changing the value.
- **The 2048 constant drifts** between `config.py` and `server_info.rs`. Comments on both point
  at each other; the reviewer checks both in this PR.
- **Local Network permission on macOS.** The first connection to a LAN server can fail while the
  system prompt is up; About then says "couldn't fetch". Reopening About fetches again. Not
  fixed here (the sign-in screen has the same behavior and its own message).
- **Replacing the standard About panel** (owner decision 1) drops anything the system panel
  showed that the custom view forgets; manual check (7) covers the #298 line.
- **Untested wiring** (owner checks on the Mac): the command replacing `.appInfo`, the window
  scene and its `.restorationBehavior(.disabled)` / `.commandsRemoved()`, the `.task(id:)`
  refresh on appearance, the `Link` opening the browser, and the `target` closure in
  `BrookApp.init()` reading the live `store` and `form`. Manual checks (1), (6), (8), (9)
  cover them.
- **Liveness probes.** `/health` now reads settings; it was already behind startup, so no new
  failure mode. The compose `healthcheck` targets the postgres service, not the api body.
  `services/api/Dockerfile`'s `HEALTHCHECK` fetches `http://localhost:8000/health` and checks
  only for status 200, not the body, so the new field cannot make it fail; a bad
  `BROOK_SOURCE_URL` stops the server at startup, before any probe, as the JWT guard already
  does.

## 5. Decisions for the owner

Both decided by the owner on 2026-10-08:

1. **Decided: a custom SwiftUI About window replaces the app menu's About item.** Why: the
   standard panel (`orderFrontStandardAboutPanel`) is built once per open, so showing the
   link only after the fetch finishes would rely on undocumented re-display behavior; a
   SwiftUI view renders the link reliably whenever the answer arrives. (`AboutModel` is
   testable with either choice; testability was not the reason.) The window then needs the
   staleness fix in M1: refresh on every appearance, no restoration, not in the Window menu.
2. **Decided: a fresh install says "No server yet." and sends no request** when the Server
   field still holds the `https://localhost` fallback (`Settings.fallbackServer`, put there by
   `Settings.serverPrefill`) and no server was remembered (`lastGoodServer` is nil).
   Implemented in `AboutModel.target` step 3, with its tests (M1).

No open decisions. After the PR merges, the main agent opens the five per-client issues
in spec §6 (GNOME, KDE, iOS, Android, Windows).
