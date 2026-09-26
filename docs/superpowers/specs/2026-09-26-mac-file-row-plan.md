# The Mac attachment row: plan

Spec: `2026-09-26-mac-file-row-spec.md` (#175, closed). Standard. One PR.

1. **`OfflineClient`** gains `openFile`, `fileState`, `pinFile`, `unpinFile` and
   `previewFile` (all in the bindings since #149 and #155), and `FakeChat` implements them.
2. **`Chat/FileRowModel.swift`** (`@MainActor @Observable`, one per file id):
   - injected `opener: (URL) -> Bool` (`NSWorkspace.shared.open`), `exists: (String) ->
     Bool`, `expensive: () -> Bool` (a shared `NWPathMonitor` reading) and a `decode`
     function (`ImageDecoder.shared.thumbnail`);
   - `open()`: `openFile`, then an existence check, then the opener, with states and texts
     per the spec;
   - `keep`: `.off`, `.fetching(UInt64?)` or `.kept`. `reloadKeep()` is numbered, and only
     the newest applies. `toggleKeep()` sets `keepBusy`, calls, then re-reads, and on a
     failure it sets the text and re-reads;
   - `preview`: `.none`, `.offer` ("Show preview"), `.loading`, `.shown(CGImage)` or
     `.failed`. `startPreview(auto:)` follows the gate, and the decode's `alive` is "this
     model is still on screen";
   - `hasLocalData` comes from a `fileState` answer that isn't `local.unavailable`. Without
     it, the model offers Save only.
3. **`CacheFeed`** keeps a weak registry of row models by file id. `.files(ids)` makes each
   one call `reloadKeep()`.
4. **`AttachmentRow`:**
   - Open and the Keep toggle (when `hasLocalData`);
   - the thumbnail, or "Show preview";
   - click to Open;
   - `onAppear` and `onDisappear` set the model's on-screen flag.
5. **Tests:** one per spec bullet (§4), against `FakeChat`, with a fake opener,
   existence check and decode. Each is watched failing under a mutant.

**If it stops halfway:** each piece shows only once `hasLocalData` is true, so today's row
(Save) is untouched until step 4, and after it without #79.

## Plan review, round 1 (vibe; Standard)

Taken:
- **`CacheFeed`'s weak registry purges empty entries** on each `files(ids)` event and on
  registration. Tested.
- **Tests:** rapid toggles (the toggle is disabled while busy, and only one call runs);
  a row model that goes away drops out of the registry.

Rebutted:
- **"Callbacks off the main actor."** The model is `@MainActor`, and `openFile`,
  `fileState`, `pinFile`, `previewFile` and the decode are `async` calls whose awaits resume
  on the main actor. No callback touches the model.
- **"Reads not checked by number."** Step 2 already applies only the newest read.
- **"Fakes on background threads."** Same reason: Swift concurrency brings each await back
  to the model's actor, so a test from any thread exercises the same code.

Closed.

## Implementation review, round 1 (vibe; Standard)

Taken:
- **The decoder's `alive` check could crash.** `ImageDecoder` calls it from its own actor,
  and the closure used `MainActor.assumeIsolated`, which traps off the main thread. It now
  reads a lock-guarded `Flag` that `onScreen` updates. Tested by calling it from a detached
  task: the old closure crashes the test host.
- **"Show preview" does nothing for a row that's off screen** (no fetch).

Rebutted:
- **"`previewMaxBytes()` doesn't exist"** and **"`.cached?` is invalid".** Both compile:
  the first is the bindings' 16 MiB cap, and the second matches an optional.
- **"`hasLocalData` only when cached."** It means this Mac's storage answered, which a
  not-cached file does too (it can still be opened or pinned).
- **"Clear the message after a reload."** An Open error stays until the next Open. A reload
  of the toggle's state isn't an answer to it.
- **"`gone` sticks."** `file.gone` is permanent: the server deleted the file.
- **"One size gate."** They differ on purpose: 4 MiB automatic, 16 MiB on request.

Measured: 14 mutants, each caught (one as a hang past 180 s, one as a test-host crash).
215 Mac tests pass.
