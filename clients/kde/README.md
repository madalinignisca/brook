# KDE / Plasma client — Qt 6 + Kirigami (+ selective KF6)

A first-class native client for **KDE Plasma**, peer to the GNOME client. Rationale: on Linux there are **two desktops to respect** — a libadwaita app feels foreign on Plasma (different theming, decorations, menu conventions), which is exactly the "native nowhere" outcome Brook rejects. See [../../docs/CLIENT_PHILOSOPHY.md](../../docs/CLIENT_PHILOSOPHY.md).

## Stack
- **Qt 6 + QML/Kirigami**, with **selective KDE Frameworks 6**.
- Uses the shared Rust [`../../core`](../../core) via **CXX-Qt** (KDAB) — UI is thin; logic stays in `core`.
- Media: **GStreamer `webrtcbin` + `vah264enc`** and Wayland portal screen capture — **same media path as the GNOME client** (both Linux). See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Native UX commitments (Plasma)
- **Follows the Plasma theme + accent + dark/light out of the box** (Qt platform theme / KColorScheme) — the #1 "belongs here" signal.
- **Traditional, configurable layout** (correct divergence from the GNOME client): **menubar + shortcuts via `KXmlGui`**, optional toolbar/statusbar, **minimize-to-tray via `KStatusNotifierItem`**.
- **`KIO`** native (network-transparent) file dialog; **`KNotifications`** for Plasma notifications.
- Server-side decorations, native context menus.

## KDE Frameworks scope (only where they add real value)
`KXmlGui` (menus/shortcuts/toolbars), `KIO` (file dialog), `KNotifications`, `KStatusNotifierItem` (tray). Avoid pulling KF gratuitously; degrade gracefully off-Plasma.

## Secret storage
Tokens go through the **freedesktop Secret Service** API in `core` — works with **both KWallet and gnome-keyring**, so no toolkit-specific secret code.

## Sequencing
Built **second** among Linux clients (after the GNOME reference), which also **proves the CXX-Qt / Qt↔Rust binding** before the macOS/Windows clients tackle their own FFI.

## Packaging
Flatpak (KDE runtime).
