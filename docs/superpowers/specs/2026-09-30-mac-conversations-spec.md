# Mac: start conversations and manage channels: spec

Status: spec, closed after review round 1. Review dial: **Standard**. Core already has every call (`open_dm`, `create_channel`,
`list_public_channels`, `join_channel`, `add_member`, `update_channel`, `delete_channel`); this
binds them and adds the UI. No auth, storage or wire change. The permissions are the server's,
and the UI only offers what it allows.

The Mac can't start a conversation today: no DM, no new channel, no public channels to join, no
add-member, and no way to rename, archive or delete (the ownership sheet even says owners can).
GTK has all of it (`clients/gnome/src/chat.rs`), which is the reference for texts.

## Done means

1. **Bindings** (`FfiBrookClient`): `openDm(handle) -> FfiChannel`,
   `createChannel(name, topic?, isPublic) -> FfiChannel`, `listPublicChannels() -> [FfiChannel]`,
   `joinChannel(id) -> FfiChannel`, `addMember(channelId, handle)`,
   `updateChannel(id, name?, topic?, archived?) -> FfiChannel`, `deleteChannel(id)`.
   `FfiChannel` gains `topic: String?` and `isPublic: Bool`. Refusals keep the server's code.
   Tested through the wiremock-free mapping tests the bindings already use, each under a mutant.
2. **The sidebar's "+" menu** (above the channel list):
   - **New Message…**: a sheet with a handle field. It opens (or finds) the DM and selects it.
     `validation.error` (what the server answers for an unknown handle) or `not_found`: "No one has
     that handle."; a handle that's yours: "That's you."; otherwise the generic text.
   - **New Channel…** (global admins only, as the server requires): name (1 to 128 characters,
     trimmed, the server's limit), optional topic (up to 512), a "Public" toggle. It creates the
     channel and selects it. `authz.forbidden`: "Only admins can create channels."; otherwise
     the generic text.
   - **Browse Channels…**: public, **non-archived** channels the user hasn't joined (the server
     lists only those, and the Mac filters archived ones out again), each with "Join"; joining
     selects the channel. An empty list says "No channels to join."
3. **Channel management** (the chat toolbar's channel menu, channels only, never a DM):
   - **Add Member…** (owners and admins): a handle sheet. Errors: `validation.error` or
     `not_found`: "No one has that handle."; `authz.forbidden`: "Only an owner or admin can add
     members.". Adding someone already in is not an error on the server, and isn't one here.
   - **Rename…** (owners and admins): name and topic, prefilled.
   - **Archive** / **Unarchive** (owners and admins): a confirmation. An archived channel stays
     in the list, read-only (the composer is already disabled for one).
   - **Delete…** (owners and admins): a confirmation that says the history goes too ("Delete
     #name and its messages for everyone? This can't be undone.").
   - The list changes only through the server's events (`channelUpdate`, `channelDelete`), never
     optimistically, except that a created, joined or DM'd channel is selected as soon as the
     call returns (the row arrives with the next list read or update).
4. **Tests**: a model per sheet against fakes (`StartConversationModel`, `NewChannelModel`,
   `PublicChannelsModel`, `ChannelManagement`), each watched failing under a mutant:
   - the handle is trimmed, a leading `@` is dropped, and an empty handle sends nothing;
   - yours is refused locally; each error's text; one call at a time;
   - New Channel: only admins are offered it; name limits (128, trimmed); the call carries topic and public;
   - Browse: join selects the channel and a failure shows its text; the joined channel leaves the
     list;
   - management is offered to owners and admins only, never on a DM; archive and delete ask
     first; errors show.

## Not doing

- A user directory or search (see the open question to the server: a handle is typed by hand).
- Leaving public channels again, inviting by email, channel topics editing beyond Rename.
- Reactions, typing, search and the mention tint: the next pieces.

## Where it fails

- **A DM with someone who isn't there:** the server's `not_found`, shown as "No one has that
  handle." (the server may answer the same for a disabled account).
- **Selecting a channel the list hasn't got yet:** the row arrives with the list read or
  `channelUpdate`; the selection is held until it does (a short retry, then dropped).
- **A channel deleted under an open sheet or a selection:** `channelDelete` closes it as today.

## Review round 1 (vibe; Standard)

Taken:
- **Browse lists non-archived channels only,** stated, and filtered on the Mac too. Tested.

Closed.

## Found while implementing

The spec's first draft said 80 characters and a name-taken error; the server's real limits are
128 (name) and 512 (topic), it has no name-taken error, and adding someone already in raises none.
Corrected above.
