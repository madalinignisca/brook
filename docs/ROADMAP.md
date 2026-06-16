# Roadmap

Incremental: each phase is independently usable and proves a piece of the architecture. The **GNOME/Linux client + `core`** lead every phase (Rust-only, fastest loop, and the Raspberry Pi target).

## Phase 0 — Foundations
- Repo + `docker-compose` (Postgres, MinIO, Janus, Caddy, api skeleton).
- `core` crate skeleton: config, async runtime, error types, state container.
- `api`: health, **local auth** (login/refresh, Argon2id), users.
- GNOME client: GTK4 + libadwaita shell, logs in, shows empty state.
- **Exit:** a user logs in (local account) from the native GNOME app over TLS.

## Phase 0b — Auth methods
- **TOTP** enrolment/verify + recovery codes (local accounts).
- **OIDC** RP (Authorization Code + PKCE, system browser) — validate against **Keycloak**; JIT provisioning.
- **LDAP** (LDAPS/StartTLS) bind + attribute mapping; JIT provisioning.
- `identities` table; operator config to enable methods. See [AUTH.md](AUTH.md).
- **Exit:** the same client logs in via local+TOTP, via Keycloak (OIDC), and via OpenLDAP.

## Phase 1 — Chat (the core loop)
- Channels + DMs, membership.
- Message send/persist + WebSocket fan-out; history pagination.
- `core` WS client (reconnect/backoff, offline queue), presence, typing.
- GNOME UI: `AdwOverlaySplitView` sidebar, message list, composer, `AdwAvatar`.
- **Exit:** real-time 1:1 and channel chat between two native clients.

## Phase 2 — File transfer
- Presigned PUT/GET via `api` + MinIO; attachment metadata.
- `core` transfer orchestration (progress, resume); native **file picker** per platform.
- **Exit:** upload in one client, pull from another when online (feature 5).

## Phase 3 — Bots & webhooks (quick, motivating win)
- Bot registry + signing secrets; `channel_bots`.
- Inbound webhook (post as bot) + `/botname` outbound (HMAC-signed, **SSRF-guarded**).
- **Exit:** a bot participates in a channel both ways (features 6 & 7).

## Phase 4 — Calls (the hard part)
- Janus VideoRoom integration; `api` mints room-scoped join tokens.
- `core` signaling state machine; `MediaEngine` trait.
- GNOME media engine: GStreamer `webrtcbin` + `vah264enc`; screen share via portal + `pipewiresrc`.
- `coturn` for restrictive NATs.
- 1:1 → group calls; screen sharing (features 3 & 4).
- **Exit:** 1:1 and group video+screen-share calls between native clients.

## Phase 5 — The Raspberry Pi 4B litmus test
- Build/run GNOME client on Pi 4B; `v4l2h264enc` path; 720p30 single-layer + reduced-fps screen share.
- Run the **Pi acceptance test** in [MEDIA.md](MEDIA.md) §6.
- **Exit:** a sustained call runs well on a Pi 4B → optimization goal met.

## Phase 6 — Second native client
- Bring up one more platform on the **same `core`** to validate the binding strategy (likely **macOS**, SwiftUI + UniFFI — shares most with iOS).
- **Exit:** two genuinely native clients, one shared core.

## Phase 7+ — Remaining clients & hardening
- Windows (WinUI 3), Android (Compose), iOS (SwiftUI).
- Hardening, account recovery, admin/operator tooling.
- Packaging: Flatpak (Linux), notarized .app/.dmg (macOS), MSIX (Windows), Play/App Store.

> Note: **E2EE is a non-goal** (see [SECURITY.md](SECURITY.md)) — intentionally absent from this roadmap.

## Cross-cutting (every phase)
- Security controls from [SECURITY.md](SECURITY.md) applied as features land (not bolted on later).
- No telemetry; resource budgets respected; measure CPU/RAM per client.
