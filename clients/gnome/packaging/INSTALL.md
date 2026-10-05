# Brook for Linux (GTK)

A native GTK 4 / libadwaita client. It is not a static binary (no GUI toolkit can be:
graphics drivers are loaded from your system), so it needs these libraries from your
distribution: GTK 4.14+, libadwaita 1.5+, GStreamer 1.20+ and OpenSSL 3. Each release is
checked on Debian 13 and Ubuntu 26.04 before it is published, and **those two are the
supported distributions**, for now. The build is for glibc 2.39+. Anything else (Fedora, Arch,
Alpine, older Debian or Ubuntu, Ubuntu 24.04) is not supported and not checked; it may work if
its libraries are new enough. Ubuntu 24.04 is out by decision, and measured to fail anyway: its
archive has no `gstreamer1.0-gtk4` (so the apt line below fails and calls cannot start) and its
glycin loaders are 1.0, below the 2.0 image previews need.

Install the runtime packages (Debian 13 and Ubuntu 26.04):

```sh
sudo apt install libgtk-4-1 libadwaita-1-0 gstreamer1.0-plugins-{base,good,bad,ugly} gstreamer1.0-libav gstreamer1.0-nice gstreamer1.0-gtk4 gstreamer1.0-pipewire libssl3t64 liblcms2-2 libseccomp2 libfontconfig1
```

Then:

```sh
./install.sh            # installs for your user (~/.local/bin, app launcher entry)
brook-gnome             # or start "Brook" from your app launcher
```

Enter your server as `https://your.server` and sign in.

Brook keeps you signed in between launches through your desktop's keyring
(GNOME Keyring or KWallet), and never asks for the keyring's password itself. With
no keyring, or a locked one, you sign in each time.

Image previews are off until you ask: an image attachment shows a **Show preview** button,
and **Show image previews** in the main menu makes small images preview by themselves. A preview
is decoded only inside a sandbox by glycin, which needs glycin's loaders (2.0 or newer) and
bubblewrap: `glycin-loaders bubblewrap`. Without them (the loaders of Debian 13 are too old),
images show as plain attachments with Open and Save, and nothing is decoded outside the sandbox.

Calls use H.264 (x264 or openh264, whichever is installed) and fall back to VP8.
`BROOK_HW_ENCODE=1 brook-gnome` tries GPU H.264 encoding (Intel/AMD, needs the
VA-API GStreamer plugin). Screen sharing uses your desktop's own picker.

## Troubleshooting

Start Brook from a terminal to see its log on stderr. It logs at `info` by default:

```sh
RUST_LOG=debug brook-gnome          # more detail from Brook (and its core)
G_MESSAGES_DEBUG=all brook-gnome    # GTK, libadwaita and GLib messages
GST_DEBUG=3 brook-gnome             # GStreamer warnings, for calls (4 or 5 for more)
```

The WebSocket libraries stay at `info` even under `RUST_LOG=trace`. Debug logs can
name servers, users and channels: read one before posting it publicly.

Uninstall: `./install.sh --uninstall`.
