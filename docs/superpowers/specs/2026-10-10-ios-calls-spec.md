# iOS: audio and video calls (#355)

Status: approved by the owner (2026-10-10), who took the recommended answer to each question
in §8.

Review: Opus review at every stage (CLAUDE.md §2). Two phases (§3). Phase 1 is iOS and shared
Swift, plus one dev-only server file: the browser call harness gains an audio count (§7). No
route, protocol or auth change. Phase 2 (push) is only outlined here; it gets its own spec, with
the auth review.

Changed after the first review: hearing is proven by audio energy, not bytes, which touches the
dev harness (Done 2, §5, §7); whether CallKit runs in the simulator is settled by a spike at the
start of the plan, for every [sim] item (§7); the camera off while Brook is off screen is
announced to the others (Done 7); question 3 now names the shared code it changes; §1's sharing
sentence corrected; participant ids are #359; the iOS engine-test command is named (Done 12);
Done 9's simulator part is made checkable; phase 2 is an outline, not commitments (§2, §4), and
it reverses PROTOCOL §3a's foreground-socket rule (§4, Done 21); a push for a call already known
or over is reported anyway (Done 19); tying a registration to a sign-in, the APNs environment and
the cancel push are open questions (9 to 11); Done 7's wording.

## 1. Problem

The Mac joins calls in channels and DMs, mutes, turns its camera on and off, shares its
screen, and (in open PR #293) rings for an incoming DM call. The iPhone shows only a call
badge on a list row. A person on an iPhone cannot join the call the badge announces, and does
not learn that someone is calling them.

The call engine exists once, in `BrookMedia` (`bindings/apple/swift/BrookCore`), over core's
call signaling. It is built for macOS only today (`Package.swift`, `clients/ios/project.yml`).
The Mac's call logic is in `clients/macos/Brook/Calls`. `CallModel` and `CallStage` use no
AppKit, but their user-facing text is Mac-worded ("on this Mac", System Settings paths in
`JoinPlan`). `CallCenter` holds the quit handshake (`QuitCoordinator`, AppKit). So iOS can share
this logic once the wording and the quit handshake are split out, instead of copying it
(CLAUDE.md §7).

An iPhone differs from a Mac in four ways that shape this work:
- iOS suspends an app in the background, socket included. A call keeps running only while
  the app holds an active call audio session.
- The system owns the call experience: CallKit shows calls on the lock screen, arbitrates the
  audio with phone calls, and is the only way to ring.
- The camera stops whenever the app is not on screen.
- A suspended or closed app hears nothing from the socket. Ringing it needs a VoIP push
  (PushKit, through APNs) sent by the server. The server has no push today: `POST /devices`
  is one line in PROTOCOL.md §1 and §3a, and nothing in `services/` implements it.

## 2. Goal

From an iPhone, a person joins and leaves a channel's or a DM's call, hears and is heard, turns
the camera on and off, and sees the others' video and shared screens. The call keeps going with
the phone locked. An incoming DM call rings with the system's incoming-call screen: in phase 1
while Brook is open, in phase 2 also while it is in the background or closed. It looks like the
iOS the user has: the system call screen, system buttons and symbols, nothing branded.

"The Mac" below is a second account in the Mac app; in the agent's checks it is the browser call
harness (`/dev/call`) or the server API instead (§7). Each item says who can prove it:
**[sim]** the agent, in the simulator against the local stack; **[device]** only the owner, on
an iPhone against the live server. A [sim] item is also part of the owner's device check. Every
[sim] call item assumes CallKit runs in the simulator; §7 says what happens if it does not.

### Done when, phase 1

1. A conversation (channel or DM) has a call button in its navigation bar, showing the
   participant count while a call runs. Tapping it joins. The first join asks for the
   microphone. A call screen covers the conversation. [sim]
2. With the Mac in the call, each side hears the other within a few seconds. [sim: by audio
   energy, §7: a tone from the harness raises the phone's inbound audio energy; a tone played
   with `afplay` on the Mac, picked up by the Mac's microphone (which the simulator uses),
   raises the harness's; muting either side flattens it. The Simulator needs macOS microphone
   permission, a one-time step for the owner] [device: by ear, plus 2 minutes on the phone's speaker with no echo reported
   by the Mac side]
3. Mute: the Mac stops hearing the phone (the harness's inbound energy goes flat) and its tile
   shows the phone muted. Muting from the system call screen is the same switch, and Brook's
   button follows it. [sim: Brook's button] [device: the system screen]
4. Camera (§8 question 3): the call starts with the camera off. Turning it on asks for the
   camera the first time; the Mac then sees the phone's front camera. Turning it off stops the
   video on the Mac and the green camera dot goes away. [device]
5. The phone shows each other participant as a tile with their display name and camera video;
   a screen the Mac shares shows larger, above the tiles, as the Mac's stage does. [sim: frames
   decoded on the phone from the harness's video and its fake shared screen]
6. Leave ends the call: the Mac's roster drops the phone within a few seconds and the orange
   microphone dot goes away. Ending the call from the system call screen does the same. [sim:
   Leave] [device: the system screen, the dots]
7. In a call, lock the phone or go to the Home Screen for 2 minutes: audio keeps flowing both
   ways, and the call shows on the lock screen and in the status bar or Dynamic Island; the lock
   screen shows the conversation's title. Tapping it opens Brook's call screen. While Brook is
   off screen, a camera that was on is announced as off (`call.media`), so the Mac's roster shows
   the phone's camera off rather than a frozen frame; it is announced on again, and video comes
   back, on return. [device]
8. A phone or FaceTime call arrives during a Brook call: the system offers to end the Brook
   call and answer; accepting leaves the Brook call (the Mac sees it), declining keeps it. [device]
9. With Brook open, the Mac starts a DM call: the phone shows the system incoming-call screen
   naming the other person. Answer joins (§4 Ringing); Decline stops it on this phone only. The
   ring also stops when the Mac hangs up, when the call is answered on another device, and after
   45 s. A channel call never rings. [sim: a second account starts the DM call with `call.join`
   over the WebSocket, from the harness or another WebSocket client; the phone reports an incoming call to CallKit (the app's log and, if the simulator
   shows it, the screen), and the reports and their ends match each case above. Only if §7's
   spike shows the simulator's CallKit does this.] [device: all of it]
10. Microphone denied: the join goes on listen-only and says how to allow it in Settings; the
    mute button is disabled. Camera denied: the camera button says so and the call goes on.
    [sim: microphone] [device: camera]
11. Mid-call, the server's gateway stops for 10 s: the call screen shows "Reconnecting…", and the
    call resumes when it is back. Ended calls show the Mac's reasons, worded for an iPhone. [sim]
12. `clients/ios/build.sh test` exits 0, and its unit tests cover: one call at a time; a join
    and an answer go through one path; system mute and Brook's mute stay one state; every way a
    call ends (Leave, system end, server end, sign-out) ends both Brook's call and the system's;
    leaving the screen with the camera on announces it off and returning announces it on; the
    ring rule, shared with the Mac; a call id maps to one system call, so the same call never
    rings twice. `BrookMedia`'s engine tests run on the iOS simulator: first
    `bindings/apple/build-xcframework.sh --ios` (the default builds no iOS simulator slice), then,
    from `bindings/apple/swift/BrookCore`:
    `xcodebuild test -scheme BrookCore-Package -destination 'platform=iOS Simulator,name=iPhone 17' -only-testing:BrookMediaTests -skip-testing:BrookMediaTests/LiveCallTests`.
    `-only-testing` keeps out `BrookCoreTests`, whose integration classes skip without a server.
    `ScreenCaptureTests.swift` is compiled for iOS today, and ScreenCaptureKit is macOS only, so
    it needs an `#if os(macOS)` guard, which the plan lists. `LiveCallTests` needs a live server (it runs in `bindings/apple/itest.sh` on the Mac). The
    plan may fold this command into `build.sh test`. `clients/macos/build.sh test` still passes.
13. Code used by both apps exists once: no Swift file or function is copied between
    `clients/macos` and `clients/ios`.
14. The written record matches the code: `clients/ios/README.md` (calls, and "Later work"),
    the user guide (calls on iPhone), and FEATURES.md.

### Phase 2, outline for its own spec (not commitments)

What the phase-2 spec is expected to make true, to be settled there with the server area and
the auth review:

15. With Brook in the background, with the phone locked, and with Brook swiped away in the app
    switcher, a DM call from the Mac rings the phone with the system incoming-call screen. With
    Brook open, the same call rings once, not twice.
16. Answer from the lock screen joins with audio; opening Brook shows the call screen.
17. Decline, the Mac hanging up, an answer on another device, and 45 s each stop the ring.
18. A signed-out phone no longer rings (and, per question 9, perhaps one whose sign-in ended
    elsewhere).
19. Apple's rule (§4): a VoIP push for a call the phone already knows, has declined, or that
    is already over is still reported to CallKit, then ended or merged into the call it already
    shows. A unit test covers each of the three cases. This one is a must for whichever spec
    builds phase 2: missing it gets the app killed and its pushes stopped.
20. A server with no Apple push key works as in phase 1 and gives no error to any client.
    Server tests cover registration, who gets a push and who does not, gone tokens, and that a
    failed or slow push never delays `call.join`.
21. The record matches: PROTOCOL.md §1 (the device routes) and §3a (the push, what it carries,
    and the reversed foreground-socket rule, §4), the admin guide (the Apple key), the user guide.

## 3. Scope and non-goals

| Item | In or out | Why |
|---|---|---|
| Phase 1: join, leave, mute, camera, see video and shared screens, CallKit, background audio, permissions, DM ring while open | In | The issue, with no route or protocol change. |
| An audio count in the dev call harness | In, phase 1 | The only way the agent can prove hearing (§7). Dev-only page, off in production. |
| Phase 2: ring when Brook is in the background or closed (PushKit, APNs, server push) | Outlined; its own spec | Needs a server change, an Apple key and the auth review; §8 question 1. |
| Sharing the iPhone's screen | Out | §8 question 2. Viewing a shared screen is in. |
| Switching to the back camera | Out | Nice later. The front camera is what a call needs first. |
| Picture in picture outside Brook | Out | Nice later; iOS shows the call in the status bar and Dynamic Island already. |
| Hold, and two calls at once | Out | One call at a time, as the Mac. A second ring offers only "end and answer" (§6). |
| Telling the caller about a decline | Out | Device-only, as the Mac (#286 owner decision). |
| Push for messages and mentions | Out | PROTOCOL §3a's other half. Its own issue; Apple forbids VoIP pushes for anything but calls. |
| Android push (FCM) | Out | Comes with Android calls. |
| A push relay shared by many servers | Out | §8 question 8. |
| TURN | Out | Not built for any client. A phone on a network that blocks UDP to the server's media ports gets no media, as a Mac would. |
| Calls in the Phone app's Recents | Out | §6. |
| CallKit in mainland China | Out | Apple asks apps on the China App Store not to use CallKit. Brook is not on the App Store; noted, not solved. |
| iPad layouts | Out | iPhone only, as #272. |

## 4. Behavior

### Joining and the call screen
- The call button in the conversation's navigation bar reads as the Mac's "Join Call" (the
  count while a call runs). It is off while not connected, while joining, and while already in
  a call; an archived channel refuses the join (`bad_state`), and Brook says so.
- Each join is told to CallKit as an outgoing call named after the conversation's title, so
  the system shows it, keeps the app running in the background, and hands it the audio.
- Permissions are asked at join (microphone) and at the first camera tap (camera), never at
  launch. The usage strings say they are for calls in Brook.
- The call screen: tiles in a grid, self first (the Mac's order); names by display name (iOS
  has no Show usernames setting); a shared screen on a stage above the tiles; buttons for
  mute, camera, audio route (the system's picker) and leave. The banner shows "Reconnecting…"
  and the end reasons.
- Audio goes to the speaker unless headphones or Bluetooth are connected (§8 question 4).
- The user can leave the call screen without leaving the call (§8 question 5).

### In the background
- While in a call, Brook keeps running with the screen locked, and so does its socket, so
  signaling, the roster and `call.resume` keep working.
- The camera stops when Brook leaves the screen. Brook announces the camera off with
  `call.media` and, if it was on, announces it on and restarts it on return.
- Not in a call, Brook is suspended as today; in phase 1 it cannot ring then.

### Ringing
- The rule is the Mac's, from #293, shared rather than copied: a DM, a live call with one
  participant, this phone not in or joining it, not declined, not timed out. `channel.call`
  carries only a count, so a call started by the user's own other device rings too, as on the
  Mac. The fix is #359 (participant ids in `channel.call`).
- The ring is CallKit's incoming-call screen, the system ringtone, and the system's Silent and
  Focus rules. Brook adds no ring setting on iOS.
- Answer joins as the call button does, with the camera off. Decline ends the system call and
  marks the call declined on this phone.
- Already in a call: another DM call still rings, and the system offers to end the current
  call and answer (§6).

### Phase 2, outline
- The app registers its PushKit token with the server after sign-in and when iOS gives it a new
  one; sign-out removes it. Whether a registration also ends when its sign-in ends elsewhere is
  question 9.
- When a DM call gets its first participant, the server sends a VoIP push to the other member's
  registered phones: never to the caller's own devices, never for a channel call, never for
  anything else. The push carries only what the ring screen needs before any network (question 7).
- **This reverses PROTOCOL §3a**, which says a device with a live foreground socket gets no push.
  Here a phone gets the push even when open: the server does not track which phones are in the
  foreground, and the app rings once because the push and the socket event name the same call id.
- On a push the app reports the call to CallKit at once, then restores the session and connects;
  the `channel.call` snapshot after `ready` says whether to keep ringing. Whether that is enough
  to stop a ring, or the server also sends a cancel push, is question 11.
- A phone restarted and not yet unlocked rings, but cannot read its saved sign-in, so Answer
  fails and says to unlock the phone (the known limit in `clients/ios/README.md`).

### Apple rules that shape this
- Every VoIP push must be reported to CallKit in the same callback, even when the call is
  already known or over (report it, then end it or merge it). An app that does not is killed,
  and iOS stops delivering VoIP pushes to it after repeated misses. So the server pushes only
  for rings, and a push never waits on the network before reporting (Done 19).
- An app stays running in the background only while a call holds its audio session (the
  background audio mode). Phase 2 also needs the VoIP mode and the push entitlement, the
  app's first entitlements file, and a paid Apple Developer team.
- The APNs environment (sandbox or production) follows the signing profile's `aps-environment`,
  not how the app is launched (question 10).
- The simulator has no camera and gets no VoIP pushes, and its CallKit is uncertain (§7).

### Wire contract
- Phase 1: none. The same `call.*` commands and `channel.call` the Mac uses.
- Phase 2: `POST /devices` and `DELETE /devices/{id}` get a real definition (PROTOCOL.md §1),
  the push payload and the reversed foreground rule are written in §3a. No WebSocket change.
  The server's side designs these in the phase-2 spec.

## 5. Who builds what, and what the owner configures

| Part | Area | Phase |
|---|---|---|
| `BrookMedia` and WebRTC built for iOS; the iOS audio session handed over by CallKit; the macOS-only parts (screen capture, the macOS audio-device header) kept to macOS | Apple (`bindings/apple` Swift) | 1 |
| The call logic moved to `clients/apple-shared` (`CallModel`, `CallStage`, `CallCenter` without its quit handshake, the #293 ring rule), with its text per platform; the Mac's quit handshake and screen picker stay on the Mac | Apple | 1 |
| The iOS call screen, CallKit, Info.plist strings and background mode | Apple (`clients/ios`) | 1 |
| An audio-energy count in the dev call harness (`services/api/app/static/call_harness.html`), and a tone source for it | Server | 1 |
| Device registration and APNs push; config; tests | Server (`services/api`, `deploy/`) | 2 |
| Core and binding calls to register and remove a device | Core | 2 |
| PushKit registration and the push-to-CallKit path | Apple | 2 |
| PROTOCOL, admin guide, user guide, README, an ADR for push without a relay | Docs | 1, 2 |

What the owner configures for phase 2, on the Apple developer site and the server, never in
the repository: a paid Apple Developer team; Push Notifications on the app id
`me.madalin.brook`; an APNs auth key (`.p8`) with its key id and the team id; on the server,
the key as a file outside the repo and the ids in `deploy/.env` (gitignored), as the admin guide
will say. Without them the server sends no push and Brook rings only while open.

## 6. Decisions

Taken in this spec (the owner may override):
- Every call, channel or DM, goes through CallKit, so audio and background work the same way.
- Calls stay out of the Phone app's Recents: channel names would otherwise land in the phone's
  call history and its iCloud sync.
- No hold. A second ring offers "end and answer"; accepting leaves the current call.
- Decline is this phone only; 45 s ring; channel calls never ring. All as the Mac (#286).
- Display names on tiles and the ring screen.
- Mute sends silence with the microphone open, as the Mac's engine does.
- The camera stopped by iOS in the background is announced as off, not left as a frozen frame.

## 7. How it is checked

- **First step of the plan: a CallKit spike in the simulator.** Audio starts only when CallKit
  activates the call's audio session, so every [sim] call item (1 to 3, 5, 6, 9 to 11) depends
  on CallKit working there, not only item 9. The spike reports whether the simulator's CallKit
  takes an outgoing call, activates its audio session, and takes an incoming report. If it does
  not, items 1 to 3, 5, 6, 10 and 11 run in the simulator through a debug-only path that starts
  the audio session without CallKit. That proves the engine on iOS, the shared call logic, the
  screen and the media; it does not prove the CallKit wiring, the audio hand-over, or
  background survival, which then join the device check with item 9.
- **The agent**, in the simulator against the local stack (`deploy/`: `make up`, `make media`,
  `BROOK_DEV_HARNESS=true` in the local `.env` only), launched with `clients/ios/build.sh run`.
  The other side is the browser harness at `http://127.0.0.1:8080/dev/call` or the API, never
  the Mac app (it shares the owner's bundle id, defaults and Keychain). Media is proven by
  counters that a broken path would leave flat, not by bytes (silence and mute still send
  bytes): inbound audio energy (`totalAudioEnergy` or `audioLevel` in the WebRTC statistics)
  on the harness for the phone's audio, and on the phone, from the engine's statistics, for the
  harness's tone; decoded frames on the phone for the harness's video and fake shared screen.
  The harness counts video only today and captures a real microphone; it gains an audio-energy
  count and a tone source (§5). The simulator has no camera, so the phone's own video is a
  [device] item.
- **The owner**, on an iPhone (iOS 26) against the live server, in a test channel and a test DM,
  with the Mac app as the other side, Brook launched from the Home Screen (a debugger keeps the
  app from being suspended). Only this proves: items 2 (by ear, echo), 3 (system screen), 4, 6
  (system screen, indicators), 7, 8, 9, 10 (camera), and anything the spike moves.

## 8. Decided by the owner (2026-10-10)

1. **Phasing.** Phase 1 under #355; phase 2 as a new issue (server and iOS) with its own spec and
   plan, started after phase 1 merges. #355's "while in the background or closed" moves to it.
   *Decided: yes.*
2. **The iPhone sharing its screen.** It needs a ReplayKit broadcast extension: a second target
   running in its own process with about 50 MB of memory, its own connection to the call, and
   an app group. *Decided: out, its own issue.*
3. **Camera at join.** Off, with the permission asked at the first camera tap. A phone joined
   from a pocket or the lock screen should not start filming, and the camera cannot run in the
   background anyway. The engine already keeps a video track without running capture; what
   changes is shared code the Mac also uses: the engine's starting media intent (camera on
   today), `JoinPlan.resolve` asking for the camera at join, and `CallModel.toggleCamera`
   refusing when the join had no camera. *Decided: off on iPhone, the Mac's behavior
   unchanged; the plan says how the shared code takes both.*
4. **Audio route.** The speaker by default (the call screen is a video call), headphones or
   Bluetooth when connected, and the system's route picker on the call screen.
   *Decided: yes.*
5. **Leaving the call screen while in the call.** A button hides it, the call goes on, and a bar
   above the list and the conversation returns to it, so the user can read the chat during a
   call as on the Mac. *Decided: in.*
6. **Order with #293.** The ring rule lives in #293, open with conflicts. *Decided: #293
   lands first, its rule moves to shared code in this work; only item 9 waits for it.*
7. **What the push carries (phase 2).** The call id, the DM's channel id and the caller's display
   name, which the ring screen shows on a locked phone. No message content. *Decided: yes.*
8. **Push without a relay (phase 2).** Each server sends to APNs with its own key. Only the team
   that signs the app can push to it, so a third party's server can ring only an app built under
   its own Apple team. A shared relay is its own design. *Decided: direct for now, written
   in an ADR.*
9. **A registration's life (phase 2).** Should a push registration end when the sign-in it was
   made under ends elsewhere (password change, admin reset), so a lost phone stops showing
   caller names? Access tokens carry only the user, role and times, not the sign-in, so this
   needs the server to know which sign-in registered the token. *Decided: settled in the
   phase-2 spec with the auth review; at minimum, sign-out on the phone removes it.*
10. **How the iPhone build is distributed (phase 2).** The APNs environment follows the signing
    profile: development-signed builds get sandbox tokens, distribution-signed (TestFlight, App
    Store, ad hoc) get production. If the owner's builds are always development-signed from
    Xcode, the server needs one environment and the registration no field. *Decided: the owner
    states the distribution in the phase-2 spec; with one kind of build, one environment set in
    server config.*
11. **Stopping a ring (phase 2).** The plan above relies on the woken app staying awake while it
    rings, connecting, and reading `channel.call`. If iOS or a bad network keeps it from doing so,
    the ring runs to 45 s after the caller hung up or someone answered. The fallback is a cancel
    push from the server, which, as every VoIP push, the app must report to CallKit and then end
    at once. *Decided: no cancel push at first; the phase-2 device check measures it, and
    the cancel push is added only if rings outlive their calls.*
