# Mac: leave a channel, remove a member, edit your profile (#183, #184): spec

Status: spec, closed after review round 1 (vibe: no findings). Review dial: **Standard**. The UI is new, and the calls already exist in
core (#184). The only binding change is adding members to `FfiChannel`. Nothing touches
auth, storage or the wire.

## Done means

1. **The bindings:**
   - `FfiChannel` gains `members: [FfiMember]` (core's `Channel.members`), so the Mac can
     list them without local data.
   - `FfiServerEvent` gains `channelUpdate { channel }` and `channelDelete { channelId }`.
     Core already parses both, but the bindings dropped them, so without local data the
     Mac never saw a leave or a removal. (Found while planning; the review had missed it.)
   - The Mac handles `channelDelete` like the cache's removal (the row goes, and an open
     channel closes), and `channelUpdate` by replacing that row.
   - Each mapping is tested under a mutant.
2. **Leave channel** (the channel's context menu in the sidebar, and the chat toolbar's
   menu), for a channel but never a DM:
   - A confirmation first: "Leave <title>?". You'll stop receiving its messages, and a
     private channel needs an invitation to rejoin.
   - If the roles say you're its only owner, the confirmation says so up front ("You're
     its last owner…", as below) and Leave is disabled. The server's 409 still decides
     when roles are missing or stale.
   - On success, the channel leaves the list and the selection clears. That happens
     through the server's `channel.delete` (item 1). The model doesn't
     remove it a second time.
   - Errors, as text in the confirmation:
     - `channel.last_owner`: "You're its last owner. Delete the channel instead." (No route
       makes someone else an owner, so the text doesn't suggest one.)
     - `authz.forbidden`: "You can't leave this channel";
     - `not_found`: nothing is shown, since you're already out;
     - otherwise the generic text.
3. **Members and Remove** (a "Members" popover from the chat toolbar, for a channel):
   - It lists each member's display name with the handle beside it (#183: names aren't
     unique). You come first, marked "(you)".
   - "Remove" appears beside each member other than you, for a global admin, and for a
     channel owner except beside another owner (only an admin removes an owner). Roles
     come from #183's `members[].role` (#185). With no role (an older server) only an
     admin gets Remove.
   - Owners are marked "Owner".
   - There's a confirmation ("Remove <name> from <title>?"). The list refreshes from the
     server's `channel.update`, never optimistically.
   - Errors:
     - `channel.last_owner`: "They're its last owner";
     - `authz.forbidden`: "You can't remove members here";
     - `not_found`: silently refresh.
4. **Edit profile** (the Account menu: "Edit Profile…"), a sheet with Display name and
   Status:
   - Fields start from `me()`. Only changed fields are sent (unchanged means `nil`), and
     clearing Status sends `""`.
   - Local checks mirror the server so the button can say why before sending: a name of 1
     to 64 characters after trimming, and a status of up to 100. The server's
     `profile.invalid` still decides, and its text is shown ("That name or status can't
     be used. Avoid invisible or control characters").
   - Save is disabled while it runs and when nothing changed. On success the sheet
     closes.
   - The signed-in header ("Signed in as …") shows the name `updateProfile` returned
     until the next sign-in, when the stored user is refreshed. That's #184's known gap,
     covered here.
5. **Tests** (models against fakes, each watched failing under a mutant):
   - Leave: no Leave for a DM; the confirmation's error texts; `not_found` is silent;
     the list is changed only by the event.
   - Members: you're first and marked; Remove for an admin, and for an owner except
     beside another owner, never for yourself, and nobody else without a role; the
     error texts.
   - Leave: the last-owner warning from the roles.
   - Profile: only changed fields are sent; clearing sends `""`; the local limits; Save
     disabled when nothing changed and while running; the returned name replaces the
     header's.

## Not doing

- Transferring ownership, and banning. The server says removal isn't a ban.
- Rejoining public channels (a channel browser is its own feature).
- Avatars.
- Updating the stored session user (#184's known gap).

## Where it fails

- **An admin removes someone while the list is stale:** the server decides, and a
  `not_found` just refreshes.
- **Leaving from another device:** the `channel.delete` event already closes it here.
- **A name the server refuses that the local check allows** (invisible characters): the
  server's text is shown, and the local check exists only for fast feedback.

## After review: roles (#183, 8389556)

The server added member roles after the review closed. Items 2, 3 and 5 now use them:
Remove goes to owners too, and the last owner is warned before trying. Without a role,
the behaviour is the one reviewed (admins only, and the server's 409).
