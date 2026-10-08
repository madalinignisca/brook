# iOS: app skeleton that signs in and lists channels (#272)

Review: `brook-reviewer`, plus a security review, because the session secret goes into the iOS
Keychain on a new platform. No server, protocol or core auth change.

Changed after the first review: the real dependency chain of `CacheFeed` and `LoginForm` (§5);
session persistence (on, with its data directory) kept apart from local data (off) (§3, §4);
stale call badges after suspension, fixed in the shared model for both apps (§4, Done 3); the
recovery-codes warning is required (§4); a stronger Done 6; the core cross-compile risk partly
retired and the shared build script's cost (§5); the exact insecure-http switches and the iOS
prefill (§4); Keychain items that outlive an app deletion (§4).

Changed after approval: the owner's answers to the open questions are recorded in §6
(bundle id, signing, where manual checks run, test accounts); §7 is now empty.

## 1. Problem

Brook has no iOS app. `clients/ios/` holds only a README. Someone on an iPhone cannot sign in to
their team's server or see their channels.

The Mac app already does both, over the shared core (`bindings/apple`, the `BrookCore` Swift
package). Its sign-in, session and channel-list logic is plain Swift that an iOS app needs too.
Copying it would give two copies to keep in step, which CLAUDE.md section 7 and
`docs/CLIENT_PHILOSOPHY.md` rule out.

## 2. Goal

An iPhone app that signs in to a Brook server, stays signed in across launches, signs out, and
shows the account's channels with live updates. It looks like the iOS the user has: system
navigation, lists and forms, Dynamic Type, light and dark, nothing branded.

Done when, each checked and written in the PR:

1. In the iOS simulator and on a real iPhone, the app signs in against chat.madalin.me
   (https), with server address, handle and password, and shows the account's channels
   and DMs.
2. An account with two-factor sign-in on asks for the 6-digit code after the password; a recovery
   code works in its place. A wrong code shows an error and lets the user try again.
3. While the list is on screen, a change made from another client shows within a few seconds
   without any action on the iPhone: a channel renamed, a channel the account is added to or
   removed from, and a call started in a channel (its call badge appears) and ended (it goes).
   Also: start a call, lock the phone, end the call from another client, unlock: the badge is
   gone once the list is back.
4. Quit the app from the app switcher and open it again: it is still signed in and shows the list.
5. Sign out returns to the sign-in screen. Quit and open the app again: it shows the sign-in
   screen, not the list.
6. `clients/ios/build.sh test` exits 0, and in that run:
   - the xcframework contains the slices `macos-arm64`, `ios-arm64` and `ios-arm64-simulator`;
   - the app builds for the simulator and for `generic/platform=iOS` with code signing off;
   - the unit tests run on a simulator and cover: the iOS persistence setup (data directory
     created, persistence on, local data off); no sign-in or sign-out message the iOS app can
     show contains "Mac" or "macOS"; the list is re-read when the app returns to the foreground;
     `liveCalls` is cleared on `ready`.

   `clients/macos/build.sh test` still passes, and covers the `liveCalls` reset too.
7. Code used by both apps exists once in the repository: no Swift file is copied between
   `clients/macos` and `clients/ios`.
8. `clients/ios/README.md` says how to build and run the app (simulator and device), and lists what
   is shared with the Mac app and what is iOS-only.

## 3. Scope and non-goals

In scope:
- iOS device and simulator slices of the core, through the existing `bindings/apple` and
  `BrookCore` package. No second binding layer.
- A SwiftUI iPhone app in `clients/ios`, built with the same tools as the Mac app (an XcodeGen
  `project.yml`, a `build.sh`).
- Sign-in: server address, handle, password, then the TOTP code or a recovery code when the
  account has two-factor sign-in on.
- The session kept in the Keychain (core's persistence, with its sign-out fences in a data
  directory), restored at launch, made unusable at sign-out.
- The channel list with live updates (changes, additions, removals, call badges).
- One fix in shared code that the Mac app gets too: call badges cleared on `ready` (§4).
- The README section on shared versus iOS-only code.

Non-goals, each its own ticket later:
- Messages UI. Tapping a channel opens nothing in this step.
- Calls and CallKit; joining a call. The badge only shows that a call is live.
- Push notifications and APNs. Live updates happen only while the app is in the foreground
  (iOS suspends sockets in the background; push is the fix, not part of this step).
- Local notifications for new messages.
- Local data (#62): the SQLCipher offline cache and outbox (`enableLocalData`, `CacheFeed`).
  The list comes from the network only; with no network the list shows an error, not cached
  channels. This is not session persistence, which stays on.
- Attachments, settings screen, profile, account screens, two-factor setup (including "New
  Recovery Codes"), admin screens.
- iPad layouts. The app is iPhone-only; an iPad runs it as an iPhone app.
- App Store packaging, TestFlight, app icon artwork.
- CI for Apple platforms (standing owner rule: none). The local `build.sh test` replaces the
  issue's "CI builds it for the simulator".

## 4. Behavior

### Launch
- No stored session: the sign-in screen. The server field is prefilled with the last server that
  signed in successfully on this device; on a first launch it is empty with the placeholder
  `https://chat.example.com` (the Mac's `https://localhost` fallback means nothing on a phone).
- A stored session: "Signing in..." and then the list, with no form flashing first. If the server
  cannot be reached, the sign-in screen says so and keeps the stored session for next time (the
  Mac app's rule). If the Keychain cannot be read, the sign-in screen says so.

### Sign-in
- A grouped system form: Server, Handle, Password, and a Log In button. The keyboard types for
  each field and password autofill work as the system offers them.
- The same checks and messages as the Mac app: missing handle or password, an invalid address, an
  address with more than scheme and host, wrong handle or password, server unreachable, an
  `http://` address refused unless insecure connections are allowed.
- No message the iOS app shows says "Mac" or "macOS". The local-network hint names the iPhone's
  prompt instead (iOS also asks before an app reaches a LAN server).
- The password field clears after each attempt that reached the server.
- Code step: a field for the 6-digit code (the system can fill it from the Passwords app), a
  switch to a recovery code, Back to the password, and Verify. Wrong, already-used and expired
  codes give the Mac app's messages.

### Signed in
- A system navigation stack with a list of the account's channels and DMs, in the sidebar order
  core already defines (`sidebarOrder`): channels first, then DMs.
- Each row: the conversation label (`#name`, or the other person for a DM, as core's
  `conversationLabel`), and a call badge with the participant count while a call is live. The
  badge is readable by VoiceOver, not only a coloured mark.
- Live: a renamed channel relabels in place; a channel the account joins or is added to appears;
  one it leaves or is removed from disappears; a call starting or ending shows or hides the badge.
- Back in the foreground after being suspended, the list is re-read, so changes made meanwhile
  show.
- Call badges after a reconnect: the server sends one `channel.call` per running call right
  after `ready` (PROTOCOL.md, `ready`), and nothing for a call that ended while the socket was
  down. Today `ChannelsModel` keeps `liveCalls` across a reconnect (`reloadList` does not touch
  it, `.ready` does not clear it), so an ended call keeps its badge. Requirement: the shared
  `ChannelsModel` clears `liveCalls` when `ready` arrives, before the re-sent `channel.call`
  events apply. This fixes the same stale badge on the Mac. Test: a shared
  `ChannelEventsTests.testReadyClearsLiveCalls` (call live, `ready`, no re-send: no badge; call
  live, `ready`, re-sent `channel.call`: badge back), run by both apps' `build.sh test`.
- The list cannot load: an error in place of the list, and pull to refresh tries again.
- After a recovery-code sign-in with 2 or fewer codes left, a one-line warning above the list:
  "You have N recovery code(s) left.", as on the Mac. No "New Recovery Codes" action: two-factor
  setup is out of scope.
- Sign Out is reachable from the list screen (a toolbar button), with a system confirmation. No
  "remove this device's data" choice: there is no local data to remove.

### Session in the Keychain
- Persistence is on: core's `enablePersistence(slot:dataDir:)` gets a `KeychainSlot` and a
  writable directory in the app's container. Core keeps its sign-out fences there, and
  `signOutComplete()` depends on them. If the directory cannot be created, persistence is off
  for that launch (the user signs in by hand), as on the Mac.
- Local data is off: `enableLocalData` is never called and no SQLCipher store is opened.
- The session is stored by core in the data-protection Keychain, this device only, never synced,
  readable after first unlock. The iOS app uses its own default access group; it does not share
  Keychain items with the Mac app.
- Sign out calls core's `logout` (forget the stored copy, then revoke). If the stored session
  could not be made unusable, the sign-in screen warns that the app may sign in again at next
  launch (the Mac app's rule).
- A remote sign-out (password changed elsewhere, an admin reset) returns to the sign-in screen
  with "You're signed out. Sign in again."
- App deleted and installed again: iOS keeps Keychain items, but removes the defaults and the
  fence directory, so an old session could come back. Requirement: on a launch with no saved
  defaults (a first launch), the app deletes every session item it may have stored (core's
  `session:<server>` slots) before anything restores. For the security review.

### Insecure http
- No UI for it. As on the Mac, plain http is allowed by the `BROOK_ALLOW_INSECURE_HTTP=1`
  environment variable or the `AllowInsecureHTTP` defaults key (both read by `Settings`). On iOS
  the variable works in the simulator and for runs started from Xcode; the defaults key has no
  `defaults write` route on a device. The warning under the form shows while it is on. Core does
  its own networking (rustls), so iOS App Transport Security does not apply and needs no
  exception.

### Platform and look
- Minimum iOS 26, iPhone only. Plain SwiftUI, no custom colours, fonts or styles.
- Info.plist carries the local-network usage text (a LAN server triggers the prompt).
- No wire-contract change: same routes, codes and events as the Mac app.

## 5. Sharing with the Mac app (input for the plan)

Requirement: code both apps need is shared, not copied. How it is shared (a folder both
`project.yml` files list, a Swift package, or moving code into `BrookCore`) is for the plan. The
shared code must keep working, and keep its tests, on the Mac.

The core and bindings side:
- `bindings/apple/build-xcframework.sh` builds `aarch64-apple-darwin` only. Its header says the
  iOS slices `aarch64-apple-ios` and `aarch64-apple-ios-sim` join `SLICES` when iOS starts. It
  exports only `MACOSX_DEPLOYMENT_TARGET`; iOS needs `IPHONEOS_DEPLOYMENT_TARGET=26.0` too.
- Cross-compile risk, partly retired: both iOS slices of brook-ffi built
  (`IPHONEOS_DEPLOYMENT_TARGET=26.0 cargo build --release --locked -p brook-ffi --target
  aarch64-apple-ios` and `--target aarch64-apple-ios-sim`, exit 0, minos 26.0), so SQLCipher and
  ring compile for iOS. Still unproven: linking into the app and running it.
- Known cost: the script is shared, so adding the iOS slices unconditionally makes
  `clients/macos/build.sh` slower and makes it need the iOS Rust targets installed; and the two
  apps' scripts would race on one output (`bindings/apple/build`, the xcframework in the package).
  The plan decides whether the slices depend on the caller.
- `BrookCore/Package.swift` declares `platforms: [.macOS(.v26)]` ("iOS joins later"). The iOS app
  needs the `BrookCore` product only. `BrookMedia`, WebRTC and `WebRTCAudioDevice` (written for
  macOS) are not needed in this step and must not block the iOS build.
- `KeychainSlot` and `AuthStateObserver` are already in `BrookCore` and use only Foundation and
  Security: shared as they are.

Mac Swift files in `clients/macos/Brook/` that an iOS skeleton needs. A file that imports only
Foundation can still reach AppKit through the types it uses; the "reaches" column follows that.

| File | Its own imports | What it reaches, and what blocks sharing |
|---|---|---|
| `ServerAddress.swift` | Foundation | nothing |
| `Settings.swift` | Foundation | nothing in code; comments name the Mac (`defaults write`, the Settings window); the `https://localhost` prefill fallback is wrong for iOS (§4) |
| `PersonName.swift` | SwiftUI, BrookCore | nothing; needed by `NotificationPlanner` |
| `Chat/OfflineClient.swift` (`ChannelRow`, `OfflineClient`) | Foundation | nothing |
| `LoginForm.swift` | Observation | holds a `SessionStore` (LoginForm.swift:19), so it inherits the whole `SessionStore` chain below |
| `SessionStore.swift` | Foundation | `CacheFeed` → `TimelineModel` (`Chat/ChatModels.swift`), `PendingModel`, `FileRowModel` → AppKit through `Chat/Staging.swift` (`import AppKit`, `NSPasteboard` at ~255) and `Chat/NSWorkspaceBridge.swift` (via `NSWorkspaceOpener` in `FileRowModel.swift`). Also: Mac wording in `Message` ("If macOS asked...", "This Mac couldn't forget...", "Brook is already open"); starts local data (`startLocalData`, ~246/268) whenever persistence is on, which iOS must not; calls `ChannelsModel.eraseRanks`; knows `.secondInstance` |
| `SessionPersistence.swift` | Foundation, Security, Darwin | `SecTaskCreateFromSelf` (reads the signed `keychain-access-groups`) is macOS-only API; `InstanceLock` (`flock`, one Brook per user) has no use on iOS; `.on` ties the Keychain slot and the data dir to persistence, which iOS keeps, while local data is a separate switch it must not turn on |
| `Calls/ChannelsModel.swift` | Foundation | default `isActive` reads `AppActivity` (AppKit `NSApp`); holds a `TimelineModel` (same AppKit chain as above); uses `NotificationPlanner`/`Notifying` from `Chat/Notifications.swift`; `liveCalls` not cleared on `ready` (§4) |
| `Chat/Notifications.swift` | Foundation, UserNotifications | `NotificationPlanner` and `Notifying` are neutral; `MacNotifier` calls `NSAppActivator` (AppKit) |
| `Chat/CacheFeed.swift` | Foundation | `TimelineModel`, `PendingModel`, `FileRowModel` (the AppKit chain); local data is out of scope |
| `AppActivity.swift` | AppKit | macOS only (`NSApp.isActive`, `NSApp.activate()`) |
| `LoginView.swift` | SwiftUI | Mac-shaped: `.buttonStyle(.link)` is macOS-only; fixed frame and default-action shortcut are desktop habits. UI is per platform: iOS gets its own view |
| `BrookApp.swift`, `SignedInView.swift`, `SettingsView.swift`, `Account/SignOutSheet.swift`, `AppTitle.swift` | AppKit / SwiftUI | Mac app shell: `NSApplicationDelegate`, `NSRunningApplication`, `Window` scenes, the call window |

Not needed here: the rest of `Chat/`, `Calls/` (except `ChannelsModel`), `Account/`,
`Previews/`, `Shared/` (image decoder), the decoder and worker targets. Breaking the chains
above (for example, `SessionStore` not knowing `CacheFeed`'s concrete type, `ChannelsModel` not
holding a concrete `TimelineModel`) is the plan's job; the spec only requires that the iOS app
does not compile AppKit code or the chat models.

Tests: the Mac tests that cover shared code (`LoginFormTests`, `ServerAddressTests`,
`SettingsTests`, `SessionStoreTests`, `ChannelEventsTests`, `ChannelOrderTests`, and others) stay
green. The plan decides where they run; they are not duplicated.

## 6. Decisions

Owner decisions:
- Minimum iOS 26, iPhone only. The Mac app targets macOS 26, so shared Swift compiles for both
  without availability checks, and plain SwiftUI on iOS 26 gives the current system look.
- No CI for Apple platforms. `clients/ios/build.sh test` runs locally, like
  `clients/macos/build.sh test`.
- The smallest thing that works (CLAUDE.md section 4).
- `liveCalls` is cleared on `ready` in the shared `ChannelsModel`, fixing the Mac too.
- The recovery-codes warning is shown on iOS, without the "New Recovery Codes" action.
- Bundle id `me.madalin.brook` for iOS, matching the Android app's id (iPhone and Android ids
  don't clash). The Mac app keeps `dev.brook.Brook`.
- Device runs use the same signing team as the Mac app, set in a gitignored
  `clients/ios/Local.xcconfig`, like `clients/macos/Local.xcconfig`.
- Manual checks run against chat.madalin.me (https), where the owner uses the app. The local
  test server is optional, for automated tests only.
- Accounts: a dedicated testing account on chat.madalin.me, created by the server side on
  request; the owner helps with the two-factor checks where needed. No credentials go into the
  repository, this spec, or the tests' source.

Taken in this spec (the owner may override):
- Session persistence on, local data off; the list is network-only.
- No settings screen; Show usernames stays at its default (off). Insecure http only through the
  environment variable or the defaults key.
- The iOS app keeps its own Keychain items; nothing is shared with the Mac app's Keychain group.
- Sign-out has no "remove this device's data" choice (nothing to remove without local data).
- First-launch prefill is empty, with a placeholder.

## 7. Open questions

None. The owner answered them; the answers are in §6.
