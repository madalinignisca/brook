# Roadmap

> Phase overview (the "when"). For the detailed, status-tracked feature catalog
> (the "what", item by item) see [FEATURES.md](FEATURES.md).

Incremental: each phase is independently usable and proves a piece of the architecture. The **GNOME/Linux client + `core`** lead every phase (Rust-only, fastest loop, and the Raspberry Pi target).

## Phase 0 — Foundations
- Repo + `docker-compose` (Postgres, MinIO, Janus, Caddy, api skeleton).
- `core` crate skeleton: config, async runtime, error types, state container.
- `api`: health, **local auth** (login/refresh, Argon2id), users; **first user bootstraps as global `admin`**.
- GNOME client: GTK4 + libadwaita shell, logs in, shows empty state.
- Single-node realtime (in-process WS hub) — scale-out (sticky + Redis/NATS) deferred.
- **Exit:** a user logs in (local account) from the native GNOME app over TLS.

## Phase 0b — Auth methods
- **TOTP** enrolment/verify + recovery codes (local accounts).
- **OIDC** RP (Authorization Code + PKCE, system browser) — validate against **Keycloak**; JIT provisioning.
- **LDAP** (LDAPS/StartTLS) bind + attribute mapping; JIT provisioning.
- `identities` table; operator config to enable methods. See [AUTH.md](AUTH.md).
- **Auth hardening** (deferred from Phase 0, flagged in review): per-IP/per-handle **rate limiting + lockout** on login/refresh; refresh-token **family lineage** so reuse of a revoked token revokes the whole family (theft response); harden the first-user bootstrap against concurrent registration; optional refresh **grace window** for dropped-response retries.
- **Exit:** the same client logs in via local+TOTP, via Keycloak (OIDC), and via OpenLDAP.

## Phase 1 — Chat (the core loop)
- Channels + DMs, membership.
- Message send/persist + WebSocket fan-out; history pagination.
- `core` WS client (reconnect/backoff, offline queue), presence, typing.
- GNOME UI: `AdwOverlaySplitView` sidebar, message list, composer, `AdwAvatar`.
- **Exit:** real-time 1:1 and channel chat between two native clients.

## Phase 2 — File transfer
- Presigned PUT/GET via `api` + MinIO; **upload states** (`pending`→`committed` via `/files/{id}/commit`) + orphan sweep; attachment metadata.
- `core` transfer orchestration (progress, resume); native **file picker** per platform.
- **Exit:** upload in one client, pull from another when online (feature 5).

## Phase 3 — Bots & webhooks (quick, motivating win)
- Bot registry + signing secrets; `channel_bots`.
- Inbound webhook (post as bot) + `/botname` outbound (HMAC-signed, **SSRF-guarded**).
- **Exit:** a bot participates in a channel both ways (features 6 & 7).

## Phase 4 — Calls (the hard part)
- Janus VideoRoom integration; **`api`-proxied signaling** (api owns Janus sessions; Admin API internal-only).
- `core` signaling state machine; `MediaEngine` trait.
- GNOME media engine: GStreamer `webrtcbin` + `vah264enc`; screen share via portal + `pipewiresrc`.
- **`coturn`** (UDP/TCP/TLS:443) + ICE-server/credential delivery — see [MEDIA.md](MEDIA.md) §6a.
- **Single-layer H.264 baseline**; H.264 simulcast is a separate **spike** (prove Janus⇄GStreamer interop before relying on it).
- 1:1 → group calls; screen sharing (features 3 & 4).
- **Exit:** 1:1 and group video+screen-share calls between native clients.

## Phase 5 — The Raspberry Pi 4B litmus test
- Build/run GNOME client on Pi 4B; `v4l2h264enc` path; 720p30 single-layer; **camera-or-screen** (one encode at a time).
- Run the **Pi acceptance test** in [MEDIA.md](MEDIA.md) §6.
- **Exit:** a sustained call runs well on a Pi 4B → optimization goal met.

## Phase 6 — Second native client
- Bring up one more platform on the **same `core`** to validate the binding strategy (likely **macOS**, SwiftUI + UniFFI — shares most with iOS).
- **Exit:** two genuinely native clients, one shared core.

## Phase 7+ — Remaining clients & hardening
- Windows (WinUI 3), Android (Compose), iOS (SwiftUI).
- **Mobile push & background**: device registration, APNs/FCM, push-wake for messages, **call invites via CallKit / ConnectionService** (see [PROTOCOL.md](PROTOCOL.md) §3a).
- **Scale-out** (if needed): sticky WS + Redis/NATS pub-sub, node-affine call signaling.
- Hardening, account recovery, admin/operator tooling (user management, deactivation).
- Packaging: Flatpak (Linux), notarized .app/.dmg (macOS), MSIX (Windows), Play/App Store.

> Note: **E2EE is a non-goal** (see [SECURITY.md](SECURITY.md)) — intentionally absent from this roadmap.

## Cross-cutting (every phase)
- Security controls from [SECURITY.md](SECURITY.md) applied as features land (not bolted on later).
- No telemetry; resource budgets respected; measure CPU/RAM per client.
