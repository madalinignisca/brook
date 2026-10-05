# Brook — User Guide

Using the Brook desktop client. To run a server, see the
[Administrator Guide](admin-guide.md).

> **Status: beta.** What works today: signing in (with a second factor if you turned
> one on), channels and direct messages, files, working offline, and calls (voice, video
> and screen sharing) on GNOME and macOS. Bots are **planned** (see [ROADMAP.md](ROADMAP.md)); this guide grows as they
> ship.

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
`brook-gnome-<version>-linux-<arch>.tar.gz` (x86_64 and aarch64; supported on Debian 13
and Ubuntu 26.04). Unpack one and
run `./install.sh`: it installs for your user only (no root), and
`./install.sh --uninstall` removes it. The tarball's `INSTALL.md` lists the GTK 4,
libadwaita and GStreamer packages your distribution needs.

A **Flatpak** comes next, from the same release pipeline. It will be the
recommended Linux install, because it is the only one where the system keeps other
apps out of Brook's local data (a native install can't promise that).

**From source:**

**Prerequisites:** the Rust toolchain (`rustup`), plus GTK 4 and libadwaita
development libraries (on Debian 13 and Ubuntu 26.04: `libgtk-4-dev libadwaita-1-dev`).

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
clients/macos/build.sh install   # Release build, installed as /Applications/Brook.app (quit Brook first)
```

This needs a signing setup in `Local.xcconfig` (see `clients/macos/README.md`): `build.sh`
refuses without `DEVELOPMENT_TEAM`, and for the app to **stay signed in and keep saved data**
you also need `BROOK_APP_PROFILE` naming a Developer ID provisioning profile for
`dev.brook.Brook` that authorises the keychain access group `<team id>.dev.brook.shared` (a
profile for the app id alone leaves saving data off). `CODE_SIGN_IDENTITY = Developer ID Application`
is part of the basic Release identity. Without that profile the app works but
keeps its keys in memory and signs in by hand each launch. A plain `build.sh` build is a Debug
build, ad-hoc signed without the keychain access group: it can't stay signed in and has no saved
data (no offline cache, no sending or opening files, no Keep available offline).

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

1. Launch Brook: you'll see the **login** screen.
2. Enter your **handle** and **password**, and click **Log In** on the Mac or **Log in** on GNOME (or press Enter).
3. If you turned on a second factor, enter the **code from your authenticator app**, or
   choose **Use a recovery code instead**. You can turn it on, get new recovery codes or
   turn it off under **Two-Factor Sign-In…** in the main menu (GNOME) or in the **Account**
   menu (macOS).
4. On success the app shows your conversations. A wrong handle or password shows an inline
   error; fix it and try again.

If the app can't reach the server, the login screen says why (check the address with your
admin).

### Staying signed in

Brook keeps you signed in between launches through your desktop's keyring (GNOME Keyring,
KWallet or the macOS Keychain), and never asks for the keyring's password itself. With no
keyring, or a locked one, you sign in each time, and everything that needs saved data is off:
the offline cache, sending and opening files, and keeping files offline.

### Signing out

**Sign Out** ends this device's sign-in. **Remove this device's data** (on by default) also
deletes the messages and files Brook saved on this computer, and any messages you had not
sent yet. Tick it off to keep them for your next sign-in. With the keyring locked, Brook
can't open the saved data, so it can't remove it either. To cut off a device you have
lost, change your password with **Sign out of other devices** on; messages already saved on that
device stay readable to anyone who can sign in to that computer.

## Conversations

- **Channels and people** are listed in the sidebar, channels first, each section with the
  newest message first (then the one you opened last, then by name). **Show usernames** names
  people by `@handle` instead of by name: in the main menu on GNOME, in Settings on the Mac.
- Send messages, **reply**, **edit** and **delete** your own, add **reactions**, and search.
  Messages that mention you are marked, and the sidebar shows unread and mention counts.
  You see when someone is typing.
- **Attach files** with the paperclip (**Add files** on GNOME, **Attach files** on the Mac); on
  GNOME you can also drag them into the window. A file you receive is opened or saved from its
  row: **Save…** asks where (on GNOME the dialog starts in your Downloads folder), **Open**
  hands a copy to another app. On GNOME an image shows a **Show preview** button (where the
  system can decode images safely: see the tarball's `INSTALL.md`), and **Show image previews**
  in the main menu makes small images preview by themselves; nothing is fetched for a preview
  until you ask, or turn that on. **Keep available offline** pins a file so you can open it
  without a connection.
- **Calls** (voice, video, screen share) work on macOS and on GNOME (on Linux, Debian 13 and
  Ubuntu 26.04 are the supported systems).

## Working offline

With the keyring available, Brook saves your conversations on this computer, encrypted with
a key only your keyring holds. Without a connection you can still read them and open pinned
files. A banner appears once you have been offline for a few seconds. Messages you write
meanwhile wait in an outbox and go out when the connection returns; each shows as pending,
with **Retry** and **Delete** if one fails. Known limit: Brook has to start with a connection
to sign you in, so this works only once it has started online; starting it offline shows the
login screen.

## Why do my file names look different?

When you share a file, Brook saves it under a **plain, safe name**: accents and other
scripts are transliterated to Latin letters (`Ștefan–raport.pdf` becomes
`Stefan-raport.pdf`, `日本語.txt` becomes `RiBenYu.txt`), and anything that could confuse
or harm a computer (hidden characters, paths, reserved names) is removed. That way a file
opens the same on Windows, macOS, Linux and phones. The name exactly as it was sent is
still shown next to the file, so nothing is lost. A file is only saved, to a place you choose, or opened in another app when you ask: the Save
dialog suggests the plain name (on GNOME it starts in your Downloads folder). On the Mac, small
images may preview by themselves, which fetches them.

## See also

- [Administrator Guide](admin-guide.md) · [Roadmap](ROADMAP.md)
