# iOS client

The native iOS app, in SwiftUI, over the shared Rust [`core`](../../core). It talks to the core
through the `BrookCore` Swift package. Most of its Swift code is shared with the macOS app, in
[`clients/apple-shared`](../apple-shared).

## Build and test

You need an Apple Silicon Mac with Xcode (iOS 26 SDK), Rust with the `aarch64-apple-ios` and
`aarch64-apple-ios-sim` targets (`rustup target add`), and XcodeGen (`brew install xcodegen`).

```bash
clients/ios/build.sh          # build for the simulator
clients/ios/build.sh test     # check the xcframework, build for a device, run the unit tests
clients/ios/build.sh run      # build, start the simulator, install and launch the app
```

- The simulator is "iPhone 17" by default. Set `BROOK_IOS_SIMULATOR` to use another.
- Each run rebuilds the `BrookCore` xcframework and regenerates `Brook.xcodeproj` from
  [`project.yml`](project.yml). Edit `project.yml`, never the generated project.

## Run on an iPhone

1. Create `clients/ios/Local.xcconfig`. It is gitignored, so your team id stays out of the
   repository:

   ```
   DEVELOPMENT_TEAM = <your team id>
   CODE_SIGN_STYLE = Automatic
   ```

   Use the same team as the Mac app.
2. Run `clients/ios/build.sh` **right before every Xcode device run**, even if you ran it a
   minute ago.
3. Open `clients/ios/Brook.xcodeproj` in Xcode, choose your iPhone, and press Run.

Why step 2: `build-xcframework.sh` deletes and rebuilds the `BrookCore` xcframework on every run.
If you ran `clients/macos/build.sh` since the last iOS build, the xcframework holds only the macOS
slice, and the device build fails to link.

## Test against a local server

The Brook server runs on your Mac in Docker. From `deploy/`:

```bash
make init    # first time only: creates deploy/.env with random secrets
make up      # postgres, api and Caddy on http://127.0.0.1:8080
make down    # stop, keep the data
```

- In the app, enter `http://127.0.0.1:8080` as the server. Plain http is allowed for loopback
  addresses, so no flag is needed. For plain http to any other address, run
  `BROOK_ALLOW_INSECURE_HTTP=1 clients/ios/build.sh run`.
- The simulator shares the Mac's network, so it reaches the server at that address.
- Create a test account as the [admin guide](../../docs/admin-guide.md#create-the-first-administrator)
  describes. The first account on a server becomes its admin. Use a placeholder handle and a
  password you choose; never put a real password in the repository or in a chat.
- For calls, also run `make media`, and set `BROOK_DEV_HARNESS=true` in `deploy/.env` for the
  browser call harness at `http://127.0.0.1:8080/dev/call`. Keep that setting in this local `.env`
  only.

## What is shared and what is iOS-only

**Shared** (the same files build for the Mac):
- The Rust core, through `bindings/apple/swift/BrookCore`.
- `clients/apple-shared/Brook`: among other files, the session store, the sign-in form, the server
  address, the settings, the channel model, the offline client, `Chat/Notifications.swift` and
  `PersonName.swift`, and the conversation models: `TimelineModel` and `ComposerModel`
  (`Chat/ChatModels.swift`), file staging and drop import (`Chat/Staging.swift`), pending messages,
  typing and search, `ScrollToLatest` and `MessageText`. iOS compiles the file staging, drop
  import, pending, save and search code, but no iOS screen uses it yet. The Mac sets its own
  paste importer (`PasteImport`, which needs AppKit) on its composer, in `ChatView.makeComposer`.
- `clients/apple-shared/BrookTests`: the shared tests, which run in both apps.

**iOS-only** (`clients/ios`):
- The screens: `BrookApp.swift`, `LoginView.swift`, `ChannelListView.swift`.
- The seam files, which give shared code the few platform facts it needs: `AppActivity.swift`,
  `ThisDevice.swift`, `SessionPersistence+iOS.swift`.
- `ConversationSession.swift` and `ConversationView.swift`: the open conversation's lifecycle
  (start, stop, re-read on return, close on removal) and its screen.
- `SignedInSession.swift`, which starts and stops one sign-in's channel model, and
  `ForegroundReload.swift`, which re-reads the list when the app comes back.
- The iOS tests in `clients/ios/BrookTests`.

**The seam rule.** Shared code never names the platform. When it needs one platform fact, it
uses a small type, and each app defines that type in its own file (`AppActivity`, `ThisDevice`,
`SessionPersistence.live()`). Shared files have no `#if os(...)` blocks. Use a protocol only when
one symbol is not enough. To add a platform fact, add a seam type; do not add a branch.

## What the app does today

- **Sign in** with a server address and a password. If the account has two-step sign-in, enter the
  6-digit code, or tap "Use a recovery code". The app warns when 2 or fewer recovery codes are left.
- **Stays signed in.** The session is kept in the Keychain, on this device only, and restored at
  launch.
- **Fresh installs.** Keychain items survive an app delete on iOS. On the first launch, the app
  removes the items an earlier install left, once. So a reinstall starts signed out.
- **Channel list.** Channels first, then direct messages. Within each, the newest activity comes
  first, then the most recently opened, then the name. The Mac uses the same order. Each row shows
  a red `@N` for unread mentions and a call badge with the participant count while a call is running.
  The list updates live, and re-reads when the app comes back to the foreground or reconnects.
- **Open a channel or DM** by tapping its row, and read it: the newest messages at the bottom,
  older ones loading as you scroll up, new, edited and deleted messages and reactions arriving
  live, and replies, files and reactions shown (read-only). A jump-to-latest button shows when you
  have scrolled up; a new message from someone else does not move the view then. What arrived while
  the app was away is re-read on return and after a reconnect, and the conversation is marked read.
  If the channel leaves the list (deleted, or you were removed), the app goes back to the list.
- **Sign out** from the list screen, after a confirmation.

## Known limits

- **No unread counts.** Only mention counts show. The server sends `unread_count` in
  `GET /channels`, but the Apple binding drops it (`FfiChannel` has no such field), so iOS cannot
  show it until the binding passes it on.
- **No cache.** Each opening of a conversation and each older page is fetched again, and with no
  network an open conversation shows only the error.
- **No message box yet.** A conversation is read-only until the message box lands.
- **A launch before the first unlock.** After a restart, the saved sign-in cannot be read until
  the phone is unlocked once. Brook says so. Unlock the phone, then quit Brook and open it again.
  A sign-out in that launch cannot remove the saved sign-in either, and Brook says that too.

## Later work

Not built yet:
- **Push.** iOS suspends background WebSockets, so always-on WSS is not the delivery path. The app
  will register an APNs token (`POST /devices`) and rely on push to wake the app and sync messages.
  See [PROTOCOL §3a](../../docs/PROTOCOL.md).
- **CallKit and Universal Links.** Incoming calls through CallKit. Universal Links for the OIDC
  redirect, with the custom scheme only as a fallback.
- **Calls.** `BrookMedia` and WebRTC are written for macOS and are not built for iOS yet.
- **Packaging.** App Store.
