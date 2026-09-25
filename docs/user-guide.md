# Brook — User Guide

Using the Brook desktop client. To run a server, see the
[Administrator Guide](admin-guide.md).

> **Status: Phase 0.** What works today: **connecting to a server and signing
> in**. Chat, channels, file transfer, calls, and bots are **planned** (see
> [ROADMAP.md](ROADMAP.md)); this guide grows as they ship.

## What is Brook?

A team-communication app that aims to feel **native to your desktop** rather than
a web page in a wrapper. Clients exist for **GNOME** (Linux) and **macOS**; clients
for KDE Plasma, Windows, Android, and iOS are planned, all sharing one core.

## What you need

- A **server address** from your administrator (e.g. `http://chat.example.lan:8080`
  on a LAN, or an `https://…` address).
- A **handle and password** — your admin creates your account (self-service
  sign-up is intentionally not offered).

## Getting the GNOME client

**Release builds** (stable and beta) are on the GitHub Releases page as
`brook-gnome-<version>-linux-<arch>.tar.gz` (x86_64 and aarch64). Unpack one and
run `./install.sh`: it installs for your user only (no root), and
`./install.sh --uninstall` removes it. The tarball's `INSTALL.md` lists the GTK 4,
libadwaita and GStreamer packages your distribution needs.

A **Flatpak** comes next, from the same release pipeline. It will be the
recommended Linux install, because it is the only one where the system keeps other
apps out of Brook's local data (a native install can't promise that).

**From source:**

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

## Getting the macOS client

Signed, notarized builds are **planned**. For now, build from source on an Apple
Silicon Mac with **macOS 26** or later:

**Prerequisites:** Xcode, the Rust toolchain (`rustup` plus
`rustup target add aarch64-apple-darwin`), and XcodeGen (`brew install xcodegen`).

```bash
git clone https://github.com/madalinignisca/brook.git
cd brook
clients/macos/build.sh
open clients/macos/build/Build/Products/Debug/Brook.app
```

- Type your **server address** on the sign-in screen. Brook remembers it (once a
  sign-in succeeds) and fills it in next time.
- On a **LAN server**, macOS may ask to allow Brook to find devices on your local
  network. Allow it; if the first attempt failed while the prompt was showing, just
  sign in again.
- **Plain-HTTP servers** (e.g. a test server without TLS yet) are refused by default.
  To allow them for testing, run the command below. The sign-in screen then shows a
  warning, because your password is sent unencrypted. Turn it off again with
  `defaults delete dev.brook.Brook AllowInsecureHTTP`.
  ```bash
  defaults write dev.brook.Brook AllowInsecureHTTP -bool YES
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
