# Mac: leave, remove, edit profile: plan

Spec: `2026-09-26-mac-membership-profile-spec.md`. Standard. Two PRs, so the binding
change can land (and serve iOS) before the UI.

## PR 1: bindings

1. `FfiChannel.members: [FfiMember]`, mapped from `Channel.members`.
2. `FfiServerEvent::ChannelUpdate { channel: FfiChannel }` and
   `ChannelDelete { channel_id }`, mapped in `client.rs`'s event conversion.
3. Mac, so the tree still builds:
   - `ChannelsModel.handle`: `channelDelete` goes to `cacheRemoved([id])`;
     `channelUpdate` replaces the row (and keeps its unread count and name rules; an
     archived channel is dropped, as `reloadList` does);
   - `TimelineModel.apply` ignores both;
   - fakes and constructors gain `members: []`.
4. Tests:
   - Rust: both events map and the members carry over;
   - Mac: a delete removes the row and closes the open channel, an update replaces the
     row and keeps unread, and an archiving update drops it.
   - Each watched failing under a mutant.

## PR 2: the UI

1. `ChatClient`-style protocol `MembershipClient` (`removeMember`, `leaveChannel`) and
   `AccountClient.updateProfile`; `FfiBrookClient` conforms, and the fakes implement it.
2. `Chat/MembershipModel.swift`:
   - `LeaveModel(channel, client)`: `confirm()`, `error` text by code; `not_found` counts
     as done; `done` closes the confirmation. The list itself changes only by the event.
   - `MembersModel(channel members, me, isAdmin, client)`: `rows` with you first and
     marked; `canRemove(row)`; `remove(id)` with per-code texts. `not_found` is silent.
     The rows come from the `ChannelRow`'s members, updated by `channelUpdate`.
3. `Account/ProfileModel.swift`: loads `me()`, keeps `name` and `status`, and has
   `changes` (the two optionals to send), `problem` (local limits), `canSave`, `save()`
   (running flag, error text, and on success the returned `FfiUser` handed to
   `onSaved`).
4. Views:
   - the sidebar row's `contextMenu` ("Leave Channel…", not for a DM);
   - the chat toolbar's "Members" popover with Remove and confirmations;
   - "Edit Profile…" in the Account menu, opening `ProfileSheet`.
   - `SignedInView` keeps `displayName` in `@State`, starting from `user` and replaced
     by `onSaved`.
5. Tests: one per spec §5 bullet, each watched failing under a mutant.

**If it stops halfway:** PR 1 alone improves the Mac, since removals now show without
local data. PR 2's pieces are separate menu entries.

**Where it fails:**
- **`channelUpdate` for a channel not in the list** (added to one): reload the list
  rather than insert a partial row. The event carries the full channel, but unread
  comes from the list.
- **The event arriving before `leaveChannel` returns:** the row is already gone, and the
  model's `done` only closes its sheet.

## Plan review, round 1 (vibe; Standard)

Taken:
- **A channel you left is never brought back by an update.** `channelUpdate` replaces a
  row only if it's in the list. A channel that isn't (including one just removed)
  triggers a list reload, and the server's list decides. A channel returns only if
  you're a member again (re-added). Tested: update after delete leaves it gone unless
  the reload lists it.

Closed.

## PR 1 implementation review, round 1 (vibe; Standard)

Taken:
- **An archived open channel stayed selected.** It now closes the way a removal does
  (`closed`, the timeline dropped), without recording a removal: a list read already
  filters archived channels. Tested under a mutant.

Also in PR 1: the server's member `role` (#183, 8389556), as core's
`ChannelMember.role: Option<String>` (defaulted) and `FfiMember.role`. The cache keeps
whole channel JSON, so cached channels carry it too.

Measured: 7 Swift and 5 Rust mutants, each caught. 230 Mac tests, 462 core tests and 31
binding tests pass.
