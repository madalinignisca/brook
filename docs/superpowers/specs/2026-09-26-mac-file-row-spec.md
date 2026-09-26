# The Mac attachment row: Open, Keep available offline, previews (#67, #66): spec

Status: spec, closed after review round 1. Review dial: **Standard**. The security-sensitive parts are already reviewed
and merged: core's Open allowlist and private copies (#144), the pins (#148, #155), and the
sandboxed decoder with its validator (#157, #159). This is the row that uses them.

All three need local data, so, like #167 and #174, the row is built and unit-tested now and
comes alive with #79. Without local data the row is exactly today's (Save only). GTK's
`clients/gnome/src/attachments.rs` is the reference for texts and states.

## Done means

1. **Open** (a button beside Save):
   - `openFile(transferId, fileId)` returns the private copy's path, and the app opens
     exactly that path with `NSWorkspace.shared.open(URL(fileURLWithPath:))`, never
     renamed or copied (#149's review).
   - Progress and Cancel while it fetches, as Save.
   - Errors, as GTK's `open_error_text`:
     - `file.open_refused`: "Can't be opened from Brook. Save it instead";
     - `local.unavailable`: "Open needs this Mac's storage. Save it instead";
     - `file.unknown`: "Not available yet. Try again in a moment";
     - otherwise Save's texts. `file.gone` hides Open and Save.
   - Just before opening, the app checks the path still exists. Core clears copies only at
     sign-out and close, but if it's gone the row says "Not available yet. Try again in a
     moment".
   - If `NSWorkspace.open` returns false (no app for it), the row says "No app opens this.
     Save it instead". That's the gap noted on #146.
   - Without local data, Open isn't shown.
2. **Keep available offline** (a toggle), as GTK's `keep_view`:
   - The state comes from `fileState(fileId)`:
     - `pinned(cached: true)` means Kept, "Available offline";
     - `pinned(cached: false)` means Fetching, "Downloading for offline", with progress
       from its transfer id when there is one;
     - anything else means Off.
   - It's re-read on the feed's `Files(ids)` for its id, so the cache's word is final.
   - Toggling calls `pinFile` or `unpinFile`. The toggle is disabled until the call
     returns (#150's note: a quick on/off can't land out of order).
   - A failure shows GTK's `keep_error_text`, then **re-reads** `fileState` rather than
     setting the toggle back blindly, so a newer state (a `Files` event meanwhile) wins.
   - Reads are numbered, and only the newest applies.
   - Without local data, the toggle isn't shown.
3. **An inline preview** for a declared `image/png`, `image/jpeg`, `image/gif` or
   `image/webp`, with core's sniff still deciding:
   - Files of 4 MiB or less preview by themselves, unless `NWPathMonitor` reports an
     expensive or constrained connection and `fileState` isn't cached or kept.
   - Files up to `previewMaxBytes()` (16 MiB) get "Show preview". Larger ones get no
     preview.
   - `previewFile(transferId, fileId)` gives the bytes, `ImageDecoder.shared.thumbnail`
     decodes them (the broker, #159), with the row alive while it's shown, and the
     `CGImage` shows at most 360 × 240 points. Clicking it runs Open.
   - Any failure (a refusal, the decoder, the validator) means no preview and the plain
     row. It's logged once, not shown.
   - Without local data, no preview.
4. **Tests** (a `FileRowModel` against fakes, each watched failing under a mutant):
   - Open:
     - it opens exactly the returned path;
     - a path gone before the open says "Not available yet";
     - each error's text;
     - `file.gone` hides the buttons;
     - "no app" when the open fails;
     - no Open without local data.
   - Keep:
     - the three views from `fileState`;
     - the toggle is disabled while its call runs;
     - a failure shows the text and re-reads the state (a newer state wins);
     - a `Files` event re-reads it;
     - an older read never overwrites a newer one.
   - Preview:
     - the declared-type gate;
     - automatic up to 4 MiB, "Show preview" above, nothing over 16 MiB;
     - on an expensive connection, only a cached file previews by itself;
     - a failure means no preview;
     - a gone row's request is dropped (the decoder's queue, `alive`).

## Not doing

- A full-size image viewer: that's Open.
- A "Pinned files" management window, and a cache size setting.
- Previews of other kinds.

## Where it fails

- **The Open copy's lifetime:** core clears Open copies at sign-out and at close
  (`clear_open_copies`). The app opens what core returns and never keeps the path.
- **A row reused for another file** (a SwiftUI list): the model is per file id, and
  late answers are checked against it.

## Review round 1 (vibe; Standard)

Taken:
- **An existence check just before `NSWorkspace.open`**, and the text when it fails.
- **A failed pin or unpin re-reads the state** instead of setting the toggle back, so a newer
  `Files` state isn't overwritten.
- **Tests for both.**

Rebutted:
- **"Re-check the preview gates after the fetch."** The gates (size, expensive connection)
  exist to avoid the fetch's cost. Once the bytes are here, decoding them costs no network,
  and a row that's gone is still dropped by the decoder's `alive` check.

Closed.
