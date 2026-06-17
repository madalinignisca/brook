# Brook — User Guide

Using the Brook desktop client. To run a server, see the
[Administrator Guide](admin-guide.md).

> **Status: Phase 0.** What works today: **connecting to a server and signing
> in**. Chat, channels, file transfer, calls, and bots are **planned** (see
> [ROADMAP.md](ROADMAP.md)); this guide grows as they ship.

## What is Brook?

A team-communication app that aims to feel **native to your desktop** rather than
a web page in a wrapper. The first client is for **GNOME** (Linux); clients for
KDE Plasma, macOS, Windows, Android, and iOS are planned, all sharing one core.

## What you need

- A **server address** from your administrator (e.g. `http://chat.example.lan:8080`
  on a LAN, or an `https://…` address).
- A **handle and password** — your admin creates your account (self-service
  sign-up is intentionally not offered).

## Getting the GNOME client

Packaged builds (Flatpak) are **planned**. For now, build from source:

**Prerequisites:** the Rust toolchain (`rustup`), plus GTK 4 and libadwaita
development libraries (e.g. on Arch: `gtk4 libadwaita`; on Fedora:
`gtk4-devel libadwaita-devel`; on Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev`).

```bash
git clone https://github.com/madalinignisca/brook.git
cd brook
BROOK_SERVER=https://chat.example.com cargo run -p brook-gnome
```

- `BROOK_SERVER` — your server's address.
- If your server is **plain HTTP** (e.g. a homelab test server without TLS yet),
  the client refuses it by default. Opt in for testing with
  `BROOK_ALLOW_INSECURE_HTTP=1` — note this sends your password in cleartext, so
  use it only on a trusted network:
  ```bash
  BROOK_SERVER=http://chat.example.lan:8080 BROOK_ALLOW_INSECURE_HTTP=1 cargo run -p brook-gnome
  ```

## Signing in

1. Launch Brook — you'll see the **login** screen.
2. Enter your **handle** and **password**, and click **Log in** (or press Enter).
3. On success the app switches to your home view. A wrong handle/password shows
   an inline error; fix it and try again.

If the app can't reach the server at all, it shows an error window explaining why
(check the address with your admin).

## Available now vs. planned

| Feature | Status |
|---|---|
| Sign in to a server | ✅ Available (Phase 0) |
| 1:1 and channel chat | ⏳ Planned (Phase 1) |
| File transfer | ⏳ Planned (Phase 2) |
| Bots / slash commands | ⏳ Planned (Phase 3) |
| Voice/video calls + screen share | ⏳ Planned (Phase 4) |

## See also

- [Administrator Guide](admin-guide.md) · [Roadmap](ROADMAP.md)
