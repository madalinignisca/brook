# Mac notifications and live unread counts: spec

Status: spec, closed after review round 1. Review dial: **Standard** (a UI on existing events; one small binding field
addition; no auth, storage or wire change).

GTK notifies for new messages and bumps unread counts from live events
(`clients/gnome/src/chat.rs`, "Make it alive" #2b and #3, and #177). The Mac does neither:
without local data its badges never rise from live messages, and it never notifies. This
works now, without #79.

## Done means

1. **The bindings:** `FfiMessage` gains `mentions: [String]` and `mentionEveryone: Bool`,
   from core's `Message`. The mapping is tested, each field under a mutant.
2. **Live unread counts** (`ChannelsModel`): a `messageNew` from someone else, in a
   channel that isn't being read (not the open one, or the app isn't active, #179), adds
   one to that channel's badge. Opening the channel clears it, as today. A message being
   read (the open channel with the app active) never counts, and neither does your own, a
   deleted one, or anything while this user's id is unknown. With local data,
   the cache's `Channels` refresh stays authoritative and overwrites the live count.
3. **Notifications** (`UNUserNotificationCenter`), for the same messages as 2:
   - The title is the channel's title (a DM's name, as the list shows it). The body is
     "<author>: <text>", or "<author> mentioned you: <text>" when `mentions` contains
     this user's id or `mentionEveryone` is set.
   - A message with files and no text says "<author> sent a file". A deleted message is
     never notified.
   - One notification per channel: the identifier is the channel id, so a later one
     replaces it.
   - **Permission:** asked once, at the first message that would notify (not at launch).
     Before each post the current setting is read (`getNotificationSettings`), so turning
     notifications on later in System Settings works, and a denial just skips the post.
   - **The lock screen:** macOS hides a notification's text while the Mac is locked,
     following the user's "Show previews" setting ("When Unlocked" by default). The app
     doesn't override it.
   - **Clicking** a notification brings Brook to the front and opens that channel.
   - Opening a channel removes its delivered notification.
4. **Tests** (a pure `NotificationPlanner`, the channel model, and a fake poster; each
   watched failing under a mutant):
   - when to notify:
     - someone else's message in a channel that isn't open;
     - in the open channel only while the app is inactive;
     - never your own, never with your id unknown, never a deleted message;
   - the text: plain, mentioned (by id and via everyone), and files only;
   - replacing by channel id; opening a channel removes its notification;
   - the permission is asked once, a denial posts nothing, and a setting turned on later
     is honoured;
   - live unread counts rise under the same rules, never for a message being read, your
     own or a deleted one, and a cache refresh overrides them.

## Not doing

- Per-channel mute and notification settings (the server has no preference model yet).
- A Dock badge.
- Sounds beyond the system default.

## Where it fails

- **The user's id unknown** (a race at sign-in): nothing notifies or counts until it's
  known, as GTK decided after its review.
- **A permission prompt at a bad moment:** it appears only at the first real
  notification, which is when the user can relate it to something.

## Review round 1 (vibe; Standard)

Taken:
- **Messages being read, and deleted ones, neither count nor notify.** Stated, and tested.
- **The permission is read before each post** rather than latched, so enabling it later
  works.

Rebutted:
- **"Message text on the lock screen."** macOS hides previews while locked by default,
  following the user's own "Show previews" setting. That's the system's decision to
  follow, not the app's to override.
- **"A cache refresh makes the badge drift."** The cache applies the same live events, so
  its count already includes them and is authoritative.

Closed.
