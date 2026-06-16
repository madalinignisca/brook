# Brook

> *A brook is a small stream — quiet, clear, and always flowing. So is your team's conversation.*

An open-source, **resource-respecting** alternative to Slack-like services — **core features only**, with **truly native clients** on every platform. Self-hosted, you own your data.

**License:** [GNU AGPL-3.0](LICENSE) — see [§License](#license).

## Goals (and explicit non-goals)

**Goals**
- A focused team-communication tool: 1:1 + channel chat, 1:1 + group **video calls with screen sharing**, file transfer, and webhook bots. Nothing else.
- **Native feel per OS.** Each client uses its platform's real UI toolkit and follows its Human Interface Guidelines. The app should feel like it *belongs* to the OS — not like the same web app reskinned five times (the Electron/Slack approach we are deliberately rejecting).
- **Maximum optimization of the device and its hardware** — hardware video encode/decode, low memory, low idle CPU. Apple-ecosystem philosophy: respect the platform and the hardware.
- **Respect the user**: their resources, their attention, their data.

**Non-goals**
- Not trying to become *a brand*. No growth-hacking, no telemetry-for-marketing, no feature bloat.
- Not a uniform cross-platform look. We do **not** ship one UI everywhere.
- Not (initially) federation or a plugin marketplace.
- **Not E2EE / not zero-trust.** This is **self-hosted, own-your-data** software for small business — you run and trust your own server. Hiding data *from the server operator* is out of scope; for that, use Signal or alternatives. See [docs/SECURITY.md](docs/SECURITY.md).
- **Not a SaaS.** No vendor lock-in, no hosted-only model — operators self-host and own their data.

**Authentication:** local password + optional **TOTP**, **OIDC** (central auth; test target Keycloak), and **pure LDAP** (OpenLDAP-style, not the commercial AD/Workspace services). See [docs/AUTH.md](docs/AUTH.md).

**Stretch benchmark — the optimization litmus test:** a call should run on a **Raspberry Pi 4B** (running an up-to-date GTK4 desktop). If it does, we consider the optimization goal met. See [docs/MEDIA.md](docs/MEDIA.md).

## How this repository is organized

Each **client** and each **service** lives in its own directory with its own `README.md`, so anyone (human or agent) working on one component sees its scope, stack, and links to the global specs — without needing the whole tree in context.

```
Brook/
├── docs/                  ← global, cross-cutting specs (read these first)
│   ├── ARCHITECTURE.md        system overview, topology, the shared-core decision
│   ├── CLIENT_PHILOSOPHY.md   native-per-OS strategy + shared Rust core
│   ├── MEDIA.md               WebRTC/SFU, per-platform HW encode, the Pi 4B target
│   ├── AUTH.md                local+TOTP, OIDC (Keycloak), pure LDAP
│   ├── SECURITY.md            trust model (self-host, no E2EE), TLS, at-rest = operator, SSRF
│   ├── PROTOCOL.md            wire protocol (WebSocket events + REST)
│   ├── DATA_MODEL.md          entities and relationships
│   ├── ROADMAP.md             MVP phases / milestones
│   └── QUALITY.md             per-subproject testing, linting & CI standards
├── core/                  ← shared Rust core library (protocol, state, crypto, signaling)
├── services/             ← server side
│   ├── api/                   chat/channels/files/bots backend (Python + FastAPI)
│   ├── sfu/                   media server (Janus + VideoRoom)
│   ├── storage/               object storage (MinIO / S3)
│   └── gateway/               reverse proxy + TLS termination (Caddy)
├── clients/              ← one native app per platform, all on top of `core/`
│   ├── gnome/                 Rust + GTK4 + libadwaita   (also the Raspberry Pi target)
│   ├── kde/                   Qt6 + Kirigami (+ KF6) via CXX-Qt  (Plasma)
│   ├── macos/                 Swift + SwiftUI / AppKit
│   ├── windows/               C# / .NET + WinUI 3
│   ├── android/               Kotlin + Jetpack Compose (Material 3)
│   └── ios/                   Swift + SwiftUI / UIKit
└── deploy/               ← docker-compose for local dev, deployment notes
```

## The one architectural idea that makes this possible

**Shared Rust core + native UI per platform.** All non-UI logic — networking, protocol, state, file transfer, call signaling, crypto — lives once in `core/` (Rust). Each client is a *thin native UI* over that core, via language bindings (UniFFI for Swift/Kotlin, C ABI for C#). This is the only way to get five genuinely native UIs without writing (and bug-fixing) the hard logic five times. See [docs/CLIENT_PHILOSOPHY.md](docs/CLIENT_PHILOSOPHY.md).

## Status

Pre-implementation. This tree currently holds the **architecture spec**. Implementation proceeds in phases per [docs/ROADMAP.md](docs/ROADMAP.md).

## License

Brook is licensed under the **GNU Affero General Public License v3.0** ([AGPL-3.0](LICENSE)).

This is a deliberate choice for **network-oriented, self-hosted software**:
- The AGPL closes the "SaaS loophole" in the ordinary GPL — **anyone who runs a modified Brook as a network service must offer their modified source to its users.** You can't take Brook, improve it behind a hosted product, and keep those changes private.
- In short: **fork freely, but contribute your changes back.** That keeps Brook honest as a community-owned alternative and prevents it from being quietly absorbed into a closed commercial offering.

Copyright © Brook contributors. Contributions are accepted under the same license.
