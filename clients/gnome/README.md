# GNOME / Linux client — Rust + GTK4 + libadwaita

**Reference client** and **Raspberry Pi 4B target.** On Linux the whole stack is Rust, so this client is built first and is where the `core` API and `MediaEngine` are proven.

## Stack
- **GTK4 + libadwaita** (`gtk4-rs`, `libadwaita-rs`), Rust.
- Uses [`../../core`](../../core) **directly** (Rust↔Rust, no FFI).
- Media: **GStreamer `webrtcbin`** + hardware H.264 (`vah264enc` on x86, `v4l2h264enc` on the Pi); screen share via xdg-desktop-portal ScreenCast + `pipewiresrc`. See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Native UX commitments (GNOME HIG)
- `AdwOverlaySplitView` collapsible channel/DM sidebar; `AdwAvatar`; `AdwToastOverlay`; `AdwStatusPage` empty states; `AdwAlertDialog` confirmations; `AdwPreferencesDialog` settings.
- **System file picker** via `gtk::FileDialog` → portal.
- Follows system **light/dark + accent** automatically via `AdwStyleManager` (integrates with the user's darkman autoswitch).

## Why libadwaita (not plain GTK4)
~5 MiB on top of GTK4 (which is needed anyway); it is **not** "half of GNOME." Gives the current HIG look, adaptive widgets, and automatic theme/accent following — see [../../docs/CLIENT_PHILOSOPHY.md](../../docs/CLIENT_PHILOSOPHY.md).

## Raspberry Pi 4B notes
Same codebase, no special build. Encode path uses `v4l2h264enc`; target **720p30 single-layer** + reduced-fps screen share; cap decoded tiles (active-speaker view). Acceptance test in [../../docs/MEDIA.md](../../docs/MEDIA.md) §6.

## Packaging
Flatpak (with the GNOME runtime, which provides GTK4/libadwaita/GStreamer).
