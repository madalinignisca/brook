# Mac: an incoming DM call rings (#286)

Dial: Standard (Mac only, no server or protocol change).

## Done when

1. When a call starts in a DM and this Mac is not in it, the Mac rings: a system sound repeated every ~3 s, and a banner across the top of the main window, whatever chat is open: "<DM title> is calling" with **Answer** and **Decline**.
2. Answer opens the call window and joins, as the toolbar's Join does; the ring stops.
3. Decline stops the ring and hides the banner **on this Mac only** (owner decision: the caller is not told). The same call never rings again here; the sidebar's "Call · 1" chip stays, so joining later still works.
4. The ring stops by itself when: the call ends; a second person is in it (the callee answered on another device: a DM has two members); this Mac joins that call by any route; 45 s pass (then it is a missed ring: banner gone, chip stays).
5. App in the background: a notification "<DM title> is calling" is posted when the ring starts and removed when it stops; clicking it opens the DM (with the banner).
6. Settings: "Ring for incoming calls" (default on). Off: no sound; the banner and notification remain.
7. Channel calls, and any call this Mac starts or is joining, never ring.

## Not done

- Telling the caller about a decline (owner: device-only).
- Answer/Decline buttons inside the notification (needs notification categories; clicking opens the DM instead).
- Do Not Disturb / Focus for the in-app sound: the app cannot read Focus without the Communication Notifications entitlement; the notification itself follows Focus. The setting in 6 is the way to silence it.
- A call I start from another of my devices: the server's `channel.call` carries only a count, not who is in it, so my Mac rings for it like for anyone. Fixing this needs participant ids in `channel.call` (server + core); raised separately.
- A bundled ring sound: a system sound is used (no asset to license).

## Rule (one place, `IncomingCalls`)

Ring for `(channelId, callId)` iff: the channel is a DM, `callId != nil`, `count == 1`, this Mac is not in or joining that channel's call, and `callId` is not in `finished` (declined, timed out, answered or seen with count ≥ 2). Any later event for that channel that breaks the rule stops it, and adds the call id to `finished` (so a count 2 → 1 after the callee leaves does not ring again).

## Plan

1. `Brook/Calls/IncomingCalls.swift`: `@MainActor @Observable final class IncomingCalls` with `ringing: Ringing?`, `observe(channelId:callId:count:isDM:)`, `localCall(channelId: String?)`, `decline()`, `answer() -> Ringing?`; timeout via an injected `Duration` (tests use milliseconds); a `Ringer` protocol (start/stop) and an `alert` hook (post/remove notification). Tests in `BrookTests/IncomingCallsTests.swift` for every line of the rule and of "Done when" 3, 4, 7.
2. `CallCenter.channelId`: the channel of the call being joined or live, set at join start (before the server can announce it) and cleared when the call is gone.
3. `ChannelsModel` owns `incoming` and feeds it from `.channelCall` (with the row's `kind == "dm"`).
4. `SignedInView`: banner above the split view; Answer runs the same join as the toolbar (factored into one function); `onChange(of: calls.channelId)` → `incoming.localCall`. Notification via `MacNotifier` (id `call-<channelId>`, click opens the DM).
5. `SystemRinger` (NSSound "Ping"... chosen on screen) + `Settings.ringForCallsKey` + toggle in SettingsView.

Review round 1 (codex), all taken:
- A snapshot can arrive before the channel list (realtime starts first): `IncomingCalls` keeps the last announcement per channel and re-evaluates when the list loads (`isDM` unknown → wait, not drop).
- The local-join check is synchronous: `observe` asks `CallCenter.channelId` through a closure, and the one join function tells `incoming` before it calls `CallCenter.join` (no `onChange`).
- Teardown: `ChannelsModel.stop()` stops the ring, its timer and its notification.

Fails halfway: steps 1–3 alone change nothing on screen (no banner); 4 without 5 gives a silent banner. Each is safe to stop after.
