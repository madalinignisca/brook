# Mac: start conversations and manage channels: plan

Spec: `2026-09-30-mac-conversations-spec.md` (closed). Standard. Two PRs: bindings first (so they can
land and be reviewed alone), then the UI.

## PR 1: bindings
1. `bindings/apple/src/client.rs`: the seven exports, each `run(async …)` over core's call.
2. `types.rs`: `FfiChannel.topic`, `FfiChannel.is_public` (from `Channel.topic`, `Channel.public`).
3. Tests: the `FfiChannel` mapping carries both fields; the exports are compile-checked by the
   generated Swift (the Mac fakes gain the seven stubs, the recurring gap).
4. Each mapping under a mutant.

## PR 2: the UI
1. **`Chat/ConversationModels.swift`**, one small `@MainActor @Observable` model per sheet over a
   `ConversationClient` protocol (the seven calls; `FfiBrookClient` conforms):
   - `StartConversationModel(me handle, client)`: `handle` normalised by `Handle.clean` (trim, drop
     a leading `@`); `submit()` calls `openDm`, sets `opened: FfiChannel?`; local refusal of your
     own handle; error texts; `busy` blocks a second call.
   - `NewChannelModel(client)`: `name`, `topic`, `isPublic`; `problem` (empty or over 80 after
     trim); `submit()` → `created`.
   - `PublicChannelsModel(client)`: `load()` (drops archived), `join(id)` → `joined`, removes the row.
   - `ChannelManagementModel(channel row, powers, client)`: `canAddMember/canRename/canArchive/
     canDelete` (owner or admin, never a DM), `addMember(handle)`, `rename(name, topic)`,
     `setArchived(_)`, `delete()`, with texts.
2. **Selection after a call:** `ChannelsModel.select(afterCreating id:)` sets a `pendingSelection`
   that `SignedInView` applies once the row is listed (`reloadList`, `channelUpdate`); a read that
   doesn't list it within one reload drops it.
3. **Views:** the sidebar "+" menu (`New Message…`, `New Channel…` for admins, `Browse
   Channels…`) with three sheets; the channel toolbar's "…" menu (Add Member…, Rename…, Archive or
   Unarchive, Delete…) with confirmations.
4. **Tests:** each spec §4 bullet, each watched failing under a mutant.

**If it stops halfway:** every piece is a separate menu entry; PR 1 alone changes nothing visible.

**Where it fails:** the selection race (step 2): tested with a list that only lists the channel on
the second read, and with one that never does.

## Plan review, round 1 (vibe; Standard)

Taken:
- **A pending selection is consumed once:** applying it clears it, so a later update listing the
  same channel can't re-select it. Tested.
- **Every model has the busy guard,** not only the DM one (one shared `Busy` pattern). Tested per
  model.

Rebutted:
- **"Owner or admin disagrees with the server's rule."** It's the same rule: "admin" is the global
  role (`isAdmin`) and "owner" is the channel role from `members[].role`, which is what
  `ChannelPowers` already computes for Remove and offers (#187, #192).

Closed.

## Implementation notes

- One PR instead of two (bindings and UI together): the bindings are small and only make sense
  with the UI, and two reviewers read one diff instead of two.
- **`reveal(id)` replaced the pending-selection design:** it re-reads the list (twice at most)
  and answers whether the channel is in it, so a selection is consumed once by construction.
- The server's real limits and codes (name 128, topic 512, `validation.error` for an unknown
  handle, no name-taken error, no error for re-adding) are in the spec's "Found while
  implementing".
- The sheets, confirmation and alert live in a `ViewModifier` (`ConversationPresentation`):
  `SignedInView.body` was too big for the Swift type-checker with them inline.

Implementation review (vibe; Standard): no findings. Measured: 20 mutants, each caught (one as a
hang past 180 s; one rewritten because its first form didn't compile). 284 Mac tests pass.
Not unit-tested: the SwiftUI views and their wiring (menus, sheets, the modifier).

## PR review, round 1 (codex, Claude review (Opus), vibe earlier; Mac-only rule)

Taken:
- **A public channel's topic was thrown away:** core's `create_public_channel` sent no topic.
  Core gains `create_channel_with(name, topic, public)` (both old calls delegate to it), and the
  binding uses it. A core test checks the body for public and private.
- **Unarchive was unreachable:** the Mac hid archived channels, so after Archive nothing could
  reach the channel. Archived channels now stay in the list (marked "archived") and open
  read-only ("This channel is archived. An owner or admin can unarchive it."), as the spec and
  GTK say. (The spec's "the composer is already disabled" was wrong: it wasn't.)
- **`reveal` could give up wrongly:** a read superseded by a newer one applies nothing. It now
  counts only applied reads as misses (two), and reads up to four times. Tested with a list
  that has the channel only on the second read, and one that never has it.
- **A finished management call could clear a newer sheet's model** (`managing === model`), and a
  second call could start while one ran (menu items disabled while busy; `run` refuses when busy).
- **A channel deleted elsewhere** now clears its open sheet, confirmation and model.
- **Rename checked only the name:** the 512-character topic limit is checked too.
- The `open_dm` doc and the `not_found` comment now say what the server really answers.

Measured: 5 more mutants, each caught. 288 Mac tests, core test for the topic with 2 mutants.

## PR review, round 2 (codex and Claude review (Opus)); the last round

Both said CHANGES. The Mac's archived channels (kept in the list in round 1) needed finishing, and
two claimed fixes lacked tests. Taken:
- **Join Call is disabled in an archived channel** (the server refuses a call there).
- **Archived means read-only:** `ComposerModel.readOnly` refuses Reply, Edit, Send and attaching,
  the drop handler follows `canAttach`, and the message menu hides Reply and Edit. (Deleting your
  own message stays: the server allows it.) Unarchiving writes again. Tested on the model.
- **A channel deleted elsewhere no longer dismisses an unrelated sheet:** `ManagementRules` (pure
  functions, tested) clears only the Add Member and Rename sheets, the confirmation and the model,
  and a finished call only drops its own model.
- **`reveal` has a test that fails on the old two-read loop:** it takes the read as a parameter, so
  the test gives it two superseded reads and then an applied one.
- **A test through the binding** that a public channel's topic reaches the server (the core test
  called core directly).
- Not changed: offline, cached rows have no topic (`FfiCachedChannel` lacks it), so Rename opens with
  an empty topic there; saving sends nothing for the topic, so nothing is lost.
- Not unit-tested: the SwiftUI views (the menu hiding, the sidebar caption, the modifier).

Measured: 13 more mutants, each caught (3 rewritten because their first form didn't compile).
297 Mac tests, 35 binding tests.
