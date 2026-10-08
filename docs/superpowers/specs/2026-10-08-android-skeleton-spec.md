# Android: app skeleton that signs in and lists channels (#273): spec

Status: draft, after the owner's ruling on local-store crypto. Review dial: **Heavy** (sign-in, TOTP and a session kept on the
device). CLAUDE.md §2 asks for an `auth-reviewer` on any authentication change, but no such agent
is defined; the owner is asked to accept the Opus review in its place. Twin of the iOS skeleton
(#272).

**Changed after peer notes (Apple/core and server reviewers):** the core change is gated on
`target_os = "android"` only (macOS unchanged, iOS decided separately) and the ADR says why other
platforms keep SQLCipher; the token removal names `FakeClient.swift` and `Redaction.swift`, needs
the Mac's `build.sh test` to pass and the Apple/core reviewer's review; open question 1 notes the
Swift-shaped callback traits to check in Kotlin and that moving the crate is a later, separate PR;
the round-3 note no longer says the owner already accepts the consequences.

**Changed after review round 4 (Opus):** the consequences are now for the owner to confirm (not
yet seen by them); `2026-09-26-keep-offline-spec.md` joins the records to update; the round-2 note
on crypto for the offline cache is marked superseded.

**Changed after review round 3 (Opus):** "Local store on Android" now lists the consequences of
plain SQLite, for the owner to confirm (file keys sit next to their ciphertext, sign-out is no longer a crypto-erase, no
`Locked` for an unused key), the storage requirements (credential-encrypted, out of all backups),
the records the core change must update, and a note that the plan must say how the Android path is
tested. The CI bullet moved back under "Build and CI".

**Changed after the owner's ruling (2026-10-08):** old open question 2 (how core gets crypto on
Android) is decided: be as native as possible. On Android core's local store is plain SQLite in the
app's private no-backup directory, protected by the OS; no SQLCipher, no OpenSSL. See "Local store
on Android" in section 4. The questions are renumbered.

**Changed after review round 2 (Opus):** open question 2 option (c) restated: the stored session
does not need SQLCipher (core writes it as JSON straight into the `KeySlot`), so its real cost is
making `rusqlite` optional across core, and the later offline cache still needs crypto
(superseded by the owner's ruling: on Android the store is plain SQLite). The token
change now names the BrookCore Swift integration tests it forces to be rewritten.

**Changed after review round 1 (Opus, changes requested):** core does not build for Android today
(SQLCipher needs OpenSSL), so that is now an open question instead of "nothing needed in core";
tokens must stop crossing the FFI, because Kotlin cannot redact them; the list order is stated as
what this scope can actually do (channels, then DMs, alphabetical); the Start chat button and the
search bar are now an owner question, not decided; sign-out warns when the stored session could
not be forgotten; every restore outcome is spelled out; nothing is promised about the background;
the Keystore rules follow core's `KeySlot` contract; open question 1 gains option (c).

## 1. Problem

Brook has no Android app. `clients/android` holds only a README. An Android user cannot sign in or
see their channels, and nothing yet proves that the shared Rust `core` runs on Android through
UniFFI's Kotlin bindings. Every later Android feature (messages, calls, FCM push) rests on that.

## 2. Goal

An Android app, built from `clients/android`, that installs on an emulator and a phone, signs in to
a Brook server through `core`, stays signed in across restarts, signs out, and shows the account's
channels and DMs, updating live while the app is open. CI builds its debug APK.

Done means:
1. Against the test server or chat.madalin.me, a person signs in with server address, handle and
   password, and sees the same channels and DMs the Mac and GNOME clients show for that account.
2. An account with TOTP is asked for a code (or a recovery code) and signs in with it.
3. After force-stopping and reopening the app, it opens on the list without asking again.
4. Sign out returns to the sign-in screen; reopening the app does not sign back in.
5. While the list is open, a channel created, renamed or left from another client appears, changes
   or disappears without a manual refresh.
6. CI builds the debug APK for arm64-v8a and x86_64 on every PR that touches the Android app or
   what it is built from.
7. `clients/android/README.md` says how to build and run it: SDK, NDK, Rust targets, the commands.

## 3. Scope and non-goals

**Decided by the owner (2026-10-08):**
- **Minimum Android 13 (API 33).** No code paths for older versions.
- **Look and feel starts close to Google Messages**, done the Android way, not a copy of iOS:
  Jetpack Compose, Material 3, dynamic color, the system's dark theme.
- **Scope is exactly #273**: sign in (server address, handle, password, TOTP when required),
  session in the Android Keystore, sign out, live channel list.
- **CI builds the debug APK.** ABIs: arm64-v8a (phones) and x86_64 (emulator).

**In scope:** building `core` and its Kotlin bindings for both ABIs, wired into the Gradle build
(this needs changes outside `clients/android`: open question 1, and the core change in "Local
store on Android"); the Compose app; the
CI job; the README.

**Non-goals** (each gets its own ticket later):
- Opening a conversation, messages, sending. Rows in the list are not tappable yet.
- Starting a chat or a channel, joining public channels.
- Calls, FCM push, notifications, attachments, offline cache and outbox, unread badges.
- Ordering by recent activity (needs activity data this scope does not load).
- Tablet and foldable layouts, landscape-specific layout.
- Settings, profile, password change, TOTP enrolment, admin tools.
- Play Store packaging (AAB, signing). Only a debug APK.
- Any server or wire-contract change. None is needed.

## 4. Behavior

### Launch
- With persistence on and a last server remembered, the app shows "Signing in..." and asks core to
  `restore`. Each outcome:
  - **LoggedIn**: the list.
  - **NotSignedIn**: the sign-in screen, no message.
  - **Unavailable**: the sign-in screen with "Your saved sign-in couldn't be read. Sign in again."
  - **Offline**: the sign-in screen with "Couldn't reach the server to resume your session. It's
    kept for next time; you can also sign in again."
  - **Superseded**: the sign-in screen, no message (a newer attempt owns the session).
- The last server address that worked is remembered (not a secret) and prefilled.

### Sign in
- One screen: server address, handle, password, a "Sign in" button. The address must be a bare
  `https://host` (or `http` for loopback); credentials, a query or a fragment in it are refused
  with a message, as on Mac and GNOME. The password is sent exactly as typed.
- While signing in the button is disabled and shows progress. Wrong credentials, an unreachable
  server, a bad address and rate limiting each get one short message under the form; the form keeps
  what was typed except the password.
- **TOTP**, same behavior as the other clients (totp-clients design): when the account has it, a
  second step asks for the 6-digit code, with "Use a recovery code instead" and Back. A wrong code
  says so and asks again; an expired challenge goes back to the password with a note; Back cancels
  the challenge. The submit button is disabled while a code is checked.
- Password and code fields use the right Android input types, so password managers and one-time
  code autofill work.

### Session in the Android Keystore
- The app gives core its `KeySlot`. Core makes, uses and wipes the keys; the platform only stores
  bytes under a named slot. On Android the bytes are encrypted with a non-exportable key held in the
  Android Keystore, and the encrypted bytes live in the app's no-backup storage: not copied by
  Android backup or device transfer, gone after uninstall.
- The Keystore key does **not** require user authentication (no unlock prompt), because core calls
  the slot without a UI and every call must answer within seconds or report `Unavailable`.
- Core's contract holds exactly: only a slot that truly does not exist is **absent**. Bytes that
  cannot be decrypted, or a missing Keystore key while the bytes exist, are `Unavailable` or
  `Fatal`, never absent, so a fault never makes core create a new key over the old one. `create`
  is create-only (`Exists` if taken); `replace` is atomic.
- **Tokens stop crossing the FFI** (auth point, for the Opus review). Today `FfiSession` carries the
  access and refresh tokens to the app. UniFFI's Kotlin data classes print every field in
  `toString`, and Kotlin cannot override that from outside the generated file (Swift redacts with
  `Redaction.swift`; Kotlin has no equivalent). The Mac does not read the tokens and core already
  persists the session through `KeySlot`, so the tokens leave the FFI record. This is a Rust change
  in the shared wrapper that also touches Mac and iOS; where it lands depends on open question 1.
  The BrookCore Swift tests do read the tokens (`LoginIntegrationTests.swift:71-77`,
  `SignOutIntegrationTests.swift:41`, `TotpIntegrationTests.swift:178,182`, and `RedactionTests`
  builds an `FfiSession` with them), so those tests are rewritten in the same change, e.g. getting
  tokens over plain HTTP as other tests there already do, and `RedactionTests` loses its token case.
  Also touched: `clients/macos/BrookTests/FakeClient.swift` (builds an `FfiSession`) and
  `bindings/apple/swift/BrookCore/Sources/BrookCore/Redaction.swift`. The Mac's `build.sh test`
  must pass, and the Apple/core reviewer reviews that PR.
- Passwords and codes never appear in logs or crash text.

### Sign out
- In an account menu in the top bar: the signed-in name and "Sign out". Sign out is immediate,
  ends the realtime connection and the stored session, and shows the sign-in screen with the
  server address prefilled.
- After sign-out the app checks `sign_out_complete()`. If false, the sign-in screen also says:
  "This phone couldn't forget your saved sign-in, so Brook may sign you in again at the next
  launch. Sign in and out again to retry." (as Mac and GNOME do).
- If the server ends the session (refresh rejected, password changed elsewhere), the app returns
  to the sign-in screen the same way, with a note.

### The channel list
- One Material 3 screen with a top bar and a conversation list. Each row has a round avatar with
  the label's first letter, colored from the theme (stable per conversation), and the label from
  core's `conversation_label`, so a DM reads the other person's display name.
- **Order:** channels first, then DMs, each alphabetical by label. Ordering by recent activity
  needs data this scope does not load (no cache, no message events, and `FfiChannel` has no last
  message).
- Start chat button and search bar: pending the owner (open question 3).
- Empty account: "No conversations yet". A list that cannot load: an error message with "Try again".
- **Live:** the app listens to core's events, opens the realtime connection, then loads the list.
  A changed channel replaces its row; a channel not in the list makes the list reload (the server's
  word decides); a deleted or left channel disappears; `Ready` after a reconnect and `Resync` reload
  the list.
- **Background:** the app does nothing special in this step. The FFI has no way to stop realtime,
  and adding one is out of scope; Android may suspend the connection, and the `Ready` after it
  reconnects reloads the list. Background delivery is the FCM ticket's job.
- Back from the list leaves the app (predictive back on).

### Build and CI
- `core` is built for `aarch64-linux-android` and `x86_64-linux-android` with the NDK, and UniFFI
  generates the Kotlin bindings, both from the Gradle build, so one command builds the APK.
- **Core does not build for Android today.** `core` uses `rusqlite` with `bundled-sqlcipher`. In
  `libsqlite3-sys` 0.35, a non-Apple target with no OpenSSL found links `dylib=crypto` and compiles
  SQLCipher against OpenSSL headers. The NDK has no OpenSSL headers, and Android's own libcrypto is
  private to the system. Decided by the owner: no SQLCipher on Android (next section).
- A new CI job builds the debug APK. It runs no device or emulator tests in this step; the sign-in
  against a real server is run by hand and its result pasted in the PR, like the Mac's `itest.sh`.
- Note for the plan: CI runs core's tests on the host only, which is the SQLCipher path. The plan
  must say how the Android (plain SQLite) path is tested, or say plainly that it is untested.

### Local store on Android (decided by the owner)
- Be as native as possible and use what the device does better. Android gives each app's private
  storage its own UID sandbox and encrypts it at rest (file-based encryption, mandatory on every
  Android 13 device). So on Android core's local store is **plain SQLite**, protected by the OS.
- Core change it implies (what, not how): on Android targets core builds `rusqlite` with plain
  bundled SQLite instead of `bundled-sqlcipher`, and its store opens databases without the
  SQLCipher key pragmas (today `store.rs` `connect()` sets `cipher_log_level`, `PRAGMA key` and
  `cipher_memory_security`). The store's key check and the per-store keys it keeps in the
  `KeyStore` may be unneeded there.
- The change is gated on `target_os = "android"` only. The key pragmas and the `Locked`-state code
  are compiled out only there; macOS keeps SQLCipher and its keychain-locked path unchanged. iOS is
  not part of this ruling: its local data is decided separately when the iOS client starts (#272
  keeps local data off).
- **Requirements**, without which the OS protection does not hold:
  - the databases live in **credential-encrypted** storage (unlocked only after the user's first
    unlock), never in device-protected storage;
  - they stay **out of every backup**: in the no-backup directory, with `allowBackup` and
    `dataExtractionRules` keeping them out of cloud backup and device-to-device transfer.
- **Consequences, for the owner to confirm with this ruling:**
  - Protection on Android is the app sandbox plus file-based encryption, nothing more.
  - The store's wrong-key detection does not apply.
  - File sealing adds nothing there: each downloaded file's own key (made in `files.rs`, used for
    AES-256-GCM in `snapshot.rs`) is kept in a `key BLOB` column of the store (`store.rs`), so on
    Android it sits in plain SQLite next to the file it seals.
  - Sign-out wipe is no longer a crypto-erase. Today `store::reset` destroys the store's key first,
    then deletes the files; on Android there is no key to destroy, so the wipe relies on deleting
    the files.
  - A store never reports `Locked` because of a key it does not use (today `open_inner` reports
    `Locked` when the key storage is unavailable). How is the plan's business.
- The session itself is unaffected: core writes it as JSON straight into the `KeySlot`, which stays
  protected by the Android Keystore as above.
- Every other platform keeps SQLCipher; the desktops have no per-app sandbox. The ADR says why.
- The change lands in `core` through the core implementer, before the Android app needs it.
- **Records updated with the core change:** a new ADR, `docs/adr/0001-...` (the first; the folder
  does not exist yet); Android notes in `2026-09-25-client-local-data-encryption-design.md` (today
  only on the `docs/client-local-encryption` branch, not on `main`) and in
  `2026-09-25-offline-cache-design.md` §3, and in `2026-09-26-keep-offline-spec.md` (line 6, "the
  encrypted, disposable file cache", and the chunk sealing around lines 105-115); the "encrypted" lines in `docs/FEATURES.md` (line 39)
  and `docs/ROADMAP.md` (line 18); "What lives in `core`" in `docs/CLIENT_PHILOSOPHY.md`; the
  `rusqlite` comment in `core/Cargo.toml`; and the module doc of `core/src/store.rs`.

## 5. Open questions for the owner

1. **Where the FFI crate lives and how Kotlin bindings are built.** `bindings/apple` (`brook-ffi`)
   already wraps all of `core` for UniFFI, with nothing Apple-only in its Rust. Options:
   (a) rename it to a shared crate (e.g. `bindings/uniffi`) used by Apple and Android; changes paths
   the Apple scripts and CI use.
   (b) a new `bindings/android` crate that wraps `core` again; duplicates the wrapper.
   (c) build `brook-ffi`'s cdylib unchanged for the Android targets and generate Kotlin from it
   (it is already `staticlib`, `cdylib` and `lib`).
   **Recommended: (c)**, then (a); not (b). #272 asks for no second binding layer. The token change
   above lands in this crate either way, and it is owned by the core/Apple side. Moving or renaming
   the crate out of `bindings/apple` is a separate later PR, not part of #273.
   Some callback traits were shaped around Swift (the call engine, async traits); their Kotlin
   generation must be checked before anything relies on them. This skeleton needs none of the call
   ones.
2. **Channels and DMs: one list or two sections?** Messages has one list ordered by activity. That
   needs activity data this scope does not load (handling `message.new`, or a last-message field
   on the channel) plus a core order rule shared with Mac and GNOME: scope growth. Without it:
   channels then DMs, alphabetical, shown as one list or as two sections with headers.
3. **Messages chrome in the skeleton.** Your #273 comment names a Start chat button. Should the
   Start chat button and the search bar be in the skeleton, or added when their features land?
   Recommendation: leave both out until they work. A dead button is worse than none, and a
   device-only search filter is scope growth.
4. **Application id and app name** shown on the device (the package name cannot change after a
   Play Store release).
5. **Plain `http` test servers.** Core refuses `http` to anything but loopback unless a dev opt-in
   is on (GNOME: an environment variable; Mac: a hidden setting), and the emulator reaches the host
   through `10.0.2.2`, which is not loopback. A labelled dev opt-in in the debug build, or `https`
   servers only?
