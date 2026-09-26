# Brook for Linux (GTK)

A native GTK 4 / libadwaita client. This build needs these libraries from your
distribution (GTK 4.14+, libadwaita 1.5+, GStreamer 1.20+, OpenSSL 3, glibc 2.39+):

- **Fedora 40+:** `sudo dnf install gtk4 libadwaita gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugin-gtk4 gstreamer1-plugin-openh264 gstreamer1-plugin-libav libnice-gstreamer1 pipewire-gstreamer openssl-libs lcms2 libseccomp fontconfig`
- **Ubuntu 24.04+ / Debian 13+:** `sudo apt install libgtk-4-1 libadwaita-1-0 gstreamer1.0-plugins-{base,good,bad,ugly} gstreamer1.0-libav gstreamer1.0-nice gstreamer1.0-gtk4 gstreamer1.0-pipewire libssl3t64 liblcms2-2 libseccomp2 libfontconfig1`
- **Arch:** `sudo pacman -S gtk4 libadwaita gst-plugins-{base,good,bad,ugly} gst-libav gst-plugin-gtk4 gst-plugin-pipewire libnice openssl lcms2 libseccomp fontconfig`

Then:

```sh
./install.sh            # installs for your user (~/.local/bin, app launcher entry)
brook-gnome             # or start "Brook" from your app launcher
```

Enter your server as `https://your.server` and sign in.

Brook keeps you signed in between launches through your desktop's keyring
(GNOME Keyring or KWallet), and never asks for the keyring's password itself. With
no keyring, or a locked one, you sign in each time.

Image attachments show a preview, decoded only inside a sandbox by glycin. That needs
glycin's loaders (2.0 or newer) and bubblewrap: `glycin-loaders bubblewrap` on Fedora and
Debian/Ubuntu, `glycin bubblewrap` on Arch. Without them (Ubuntu 24.04's loaders are too
old), images show as plain attachments with Open and Save, and nothing is decoded outside
the sandbox.

Calls use H.264 (x264 or openh264, whichever is installed) and fall back to VP8.
`BROOK_HW_ENCODE=1 brook-gnome` tries GPU H.264 encoding (Intel/AMD, needs the
VA-API GStreamer plugin). Screen sharing uses your desktop's own picker.

Uninstall: `./install.sh --uninstall`.
