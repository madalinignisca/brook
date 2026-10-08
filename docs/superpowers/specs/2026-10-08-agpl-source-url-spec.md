# AGPL §13: the server tells users where its source is (#300)

Brook is AGPL-3.0-or-later, `Copyright © 2026 Madalin Ignisca and Brook contributors`.
Owner decisions (2026-10-08, not reopened here): the server has one config value, its source
URL, defaulting to `https://github.com/madalinignisca/brook`; a small public endpoint returns
it; each client's About box shows it as a link. An operator running a modified server sets it
to where their modified source is.

**Revision (2026-10-08).** The owner answered every open question (now §7, Decisions), and the
Opus review findings are fixed: an empty value counts as unset (§4.1); native deploys need no
passthrough change (§3, §4.5); the false claim that clients already call `/health` is gone
(§4.2); core rejects URLs with userinfo and clients show core's parsed form (§4.3); the Apple
binding wrapper is named and the call returns `version` too (§4.3); the reason for the startup
refusal is corrected (§4.1); a known-limits section is added (§8).

## 1. Problem

AGPL-3.0 §13 says: if you modify the program and people use it over a network, you must offer
those users the source of *your* version. A Brook server is used over the network by every
person who signs in, so this applies to anyone who runs a changed Brook server.

Today nothing tells a user where the server's source is. The clients have no About box at all
(no macOS About panel content, no GNOME About dialog). An operator who forks Brook has no
supported place to put their source link, so even an operator who wants to comply can't.

Who it is for:
- **Users**: they can find the source of the server they are actually talking to.
- **Operators who modify Brook**: one setting makes their server compliant.
- **Operators who don't**: nothing to do; the default already points at upstream.

## 2. Goal

Done means, as things a person can check:
1. On a server with no setting (or an empty one), `GET /health` answers the upstream URL in
   `source_url`.
2. With the setting changed, it answers the new URL, after a restart.
3. A malformed value (not an `http`/`https` URL, or with a user/password in it) stops the
   server at startup with a message naming the setting, like the existing JWT and keyring
   guards.
4. `/health` still needs no sign-in and no token.
5. In the macOS client, About shows "Server source" as a link to that URL, for the server the
   client is using, before and after sign-in. Clicking it opens the system browser.
6. When the server can't be reached, About says the link couldn't be fetched and shows no link.
7. PROTOCOL, the admin guide and the user guide say all of the above.

## 3. Scope

In, all in one PR:
- One server setting, `BROOK_SOURCE_URL`. `deploy/docker-compose.yml` lists every api env var
  by hand, so it gains `BROOK_SOURCE_URL: ${BROOK_SOURCE_URL:-}`; the default lives only in the
  server. The native deploy needs no passthrough change: `brook-api.service` already reads
  `EnvironmentFile=/etc/brook/api.env`, so an operator adds the line there. `install.sh` may
  write it as a commented line in the `api.env` it creates on first install. This change does
  not add `BROOK_SOURCE_URL` to an existing `api.env` (unlike the keyring lines `install.sh`
  appends); an operator who wants it adds the line by hand. Unset means the default, so there
  is no migration step.
- `source_url` added to the `/health` answer (§4.2).
- One core call that fetches it for a server address, with no session (§4.3), and its wrapper
  in the Apple binding (`bindings/apple/src/client.rs`). Shared logic goes in core first
  (CLAUDE.md §7); each client only adds its UI.
- About in the macOS client, showing the server source link next to the static license and
  copyright from #298.
- Docs (§5) and the per-client issues (§6).

Non-goals, and why:
- **The license text and copyright line in About.** That is #298: static text, no server
  involved. This spec only adds the server line next to it.
- **The client's own source.** The clients are AGPL too, but a client is *conveyed* (§6 of the
  license: shipped with or pointing at its source), not used over a network. Its About can link
  the upstream repo statically under #298. Not this spec.
- **Checking that the URL is real** (reachable, matches the running code). The server can't know
  that; it is the operator's legal duty. The admin guide says so.
- **A per-version link** (a tag or commit for the running build). The version is `0.0.0` today;
  the repo root is linked now. Revisit at 1.0.
- **Changing the URL without a restart**, an admin UI for it, or storing it in the database.
- **Caching the last answer in the client** so About can show it offline.
- **A new route.** The field joins `/health`.
- **Old clients and old servers.** Pre-1.0: a client may assume every server answers the field.

## 4. Behavior

### 4.1 Server setting
- `BROOK_SOURCE_URL`. Default `https://github.com/madalinignisca/brook`.
- Read once at startup, like the other settings in `services/api/app/config.py`.
- Unset **or empty** means the default. Compose passes an unset variable as `""`
  (`${VAR:-}`), so an empty string must count as unset; this follows the existing precedent
  in `config.py` (`_unset_primary_is_none`).
- Any other value must be an absolute `http` or `https` URL with a host, no user or password
  part, and bounded in length (the plan picks the limit). Anything else refuses to start.
- Why refuse instead of falling back to upstream: to catch an operator's typo early, at the
  moment they set it, rather than silently serving a link they didn't mean. (A fork that never
  sets the value still serves the upstream default; the refusal does not, and cannot, enforce
  §13.)

### 4.2 Wire contract
What exists: `GET /health` (unauthenticated, at the root, **not** under `/api/v1`) answers
`{"status":"ok","version":"..."}`. Today only `install.sh`, the deploy READMEs and the
`bindings/apple` test scripts call it; no client does. There is no other public server-info
route; `GET /auth/methods` is in PROTOCOL but not in the code.

Decision: **add `source_url` to the `/health` answer**:
`{"status":"ok","version":"...","source_url":"..."}`. Why: no new route and no new auth rule.
The cost: a liveness probe now also carries product info, and it sits outside the versioned
`/api/v1` base. Accepted.

- Public: no token is read; a sent token is ignored, never refused.
- The value is exactly the configured string (or the default), unchanged.
- No new error codes.

PROTOCOL fix found while reading: `GET /health` is listed in the §1 table whose base is
`https://<host>/api/v1`, but it is served at `/health`. The PROTOCOL change for this spec
corrects that line.

### 4.3 Core and the Apple binding
- One core call that takes a server address and returns **both** `version` and `source_url`
  (the server always sends `version`, so About can show "Server version" too). It needs no
  session, so it works on the sign-in screen, before or after login.
- `bindings/apple/src/client.rs` gets a wrapper for it, so Swift can call it (core
  implementer's area).
- It uses the same address rules as sign-in (the same HTTPS-only default and the same
  allow-insecure-HTTP switch), so About never talks to a server the sign-in screen would refuse.
- It accepts `source_url` only as an absolute `http`/`https` URL with a host and **no
  userinfo** (`user@` or `user:pass@`). Anything else (another scheme such as `file:` or
  `javascript:`, userinfo, no host, too long, missing field, unparseable body) is an error,
  not a link: a hostile or broken server must not get a client to open a non-web link or a
  link that hides its real host.
- It returns the URL in its parsed, re-serialised form (host in punycode). Clients display and
  open that form, never the raw string from the server, so a look-alike Unicode host shows as
  what it really is.
- Errors use the existing kinds; nothing new to translate. In core, a connection failure or
  an HTTP timeout is `Error::Http`, which the Apple binding maps to `LoginError::Network`; a
  bad answer (including a `source_url` refused above) is `UnexpectedResponse`; a refused server
  address is `InvalidServerUrl` or `InsecureServerUrl`. The `Timeout` kind is for realtime
  commands and is not used here (`core/src/error.rs`, `bindings/apple/src/types.rs`).
- Short timeout; About must never hang. Core's default request timeout is 30 s
  (`core/src/config.rs`) and `with_request_timeout` already exists; the plan picks the shorter
  value.
- The URL length limit is the same number on the server (§4.1) and in core, so a value the
  server accepts is never refused by a client. The plan picks it.

### 4.4 Clients: what the user sees
About (on macOS, the app menu's "About Brook"; the platform equivalent elsewhere) shows:
- the static lines from #298 (name, version, copyright, license);
- **Server version** and **Server source**: the source URL as a link, for the server address
  in use.

Which server: the signed-in server when signed in; otherwise the address on the sign-in screen,
if one is entered or remembered. With no address at all, the line says there is no server yet.
The line shows before sign-in too, because the call needs no session and a person may want to
know whose code they are about to sign in to.

Fetched each time About opens. While it loads, the line says so. On failure (unreachable,
timeout, refused address, bad answer) the line says the server's source link couldn't be
fetched, and shows **no** link and no cached value. It never falls back to the upstream URL
for the server line: on a fork that would name the wrong code.

Clicking the link opens the system browser. The client never fetches the URL itself.

### 4.5 Operators
- Unmodified server: nothing to do.
- Modified server: set `BROOK_SOURCE_URL` to where users can get the source of the version
  that is running, and keep it current. Brook can't check this.
  - Compose: in the `.env` next to `docker-compose.yml`.
  - Native: add a line to `/etc/brook/api.env`, then restart `brook-api`.
- Check it with `curl <server>/health`, as the admin guide already does for `/health`.

## 5. Docs to update (same PR, through `brook-docs-writer`)
- `docs/PROTOCOL.md`: the `source_url` field, that `/health` is public, and the `/health`
  base-path fix.
- `docs/admin-guide.md`: the setting, its default, that empty means default, the startup
  refusal on a malformed value, the operator's duty under §13, and how to check it. Both
  deployments (compose `.env`, native `api.env`).
- `docs/user-guide.md`: where About is in each client and what "Server source" means.
- `README.md` §License: one sentence that a modified server must set the source link (the
  copyright line itself is #298).

## 6. Issues for the other clients (CLAUDE.md §7)
After the macOS PR merges, one issue each for GNOME (`area:gtk`), KDE (`area:kde`), iOS
(`area:ios`), Android (`area:android`), Windows (`area:windows`). Each links the merged PR and
this spec and says: "About shows Server version and Server source, the latter as a link to the
source URL the connected server reports, before and after sign-in; when the server can't be
reached it says so and shows no link." Not-yet-started clients get one too. None is skipped:
every platform has an About.

## 7. Decisions (owner, 2026-10-08)
1. **Client in this PR**: macOS builds About, with the server, core and Apple binding changes
   in the same PR. The server line shows before sign-in too (for the address on the sign-in
   screen).
2. **Wire**: the field joins `/health` as `source_url`. No new route.
3. **Offline**: "couldn't fetch", no link, no cache.
4. **Invalid value**: refuses to start (empty counts as unset, §4.1).
5. **Link target**: the repo root now; an exact-version link is deferred, revisit at 1.0.

No open questions remain.

## 8. Known limits
Today every user reaches the server through a client, so About is the only place the link
must appear. Bots, webhooks and a web UI do not exist yet. When any of them arrives, the
public `/health` already carries `source_url`, and that new surface must show it to its users
then.
