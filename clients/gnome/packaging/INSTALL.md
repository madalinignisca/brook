# Brook for Linux (GTK)

A native GTK 4 / libadwaita client. This build needs these libraries from your
distribution (GTK 4.14+, libadwaita 1.5+, GStreamer 1.20+, glibc 2.39+):

- **Fedora 40+:** `sudo dnf install gtk4 libadwaita gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugin-gtk4 gstreamer1-plugin-openh264 gstreamer1-plugin-libav libnice-gstreamer1 pipewire-gstreamer`
- **Ubuntu 24.04+ / Debian 13+:** `sudo apt install libgtk-4-1 libadwaita-1-0 gstreamer1.0-plugins-{base,good,bad,ugly} gstreamer1.0-libav gstreamer1.0-nice gstreamer1.0-gtk4 gstreamer1.0-pipewire`
- **Arch:** `sudo pacman -S gtk4 libadwaita gst-plugins-{base,good,bad,ugly} gst-libav gst-plugin-gtk4 gst-plugin-pipewire libnice`

Then:

```sh
./install.sh            # installs for your user (~/.local/bin, app launcher entry)
brook-gnome             # or start "Brook" from your app launcher
```

Enter your server as `https://your.server` and sign in.

Calls use H.264 (x264 or openh264, whichever is installed) and fall back to VP8.
`BROOK_HW_ENCODE=1 brook-gnome` tries GPU H.264 encoding (Intel/AMD, needs the
VA-API GStreamer plugin). Screen sharing uses your desktop's own picker.

Uninstall: `./install.sh --uninstall`.
