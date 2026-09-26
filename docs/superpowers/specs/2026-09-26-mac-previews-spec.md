# Inline image previews on the Mac (#66): spec

Status: spec, closed after review round 2. Review dial: **Heavy**. Decoding bytes a stranger chose
is the attack surface here, as for GTK (`2026-09-26-image-previews.md`).

The UI waits on local data, and so on the provisioning profile (#79): `previewFile` reads
only from the encrypted cache. The decoder service and its tests don't wait on anything.

## The rule

No image decoder runs in the app's process. `NSImage`, `CGImageSource` and anything else
built on ImageIO decode in-process, so the app never hands attachment bytes to them. It
only builds a `CGImage` from raw RGBA that it has checked itself, in a pixel format it
chose itself.

## Done means

1. **The decoder, an XPC service** (`BrookImageDecoder.xpc` in `Contents/XPCServices`,
   signed by the same team with the hardened runtime):
   - **Sandboxed, with no entitlements beyond the sandbox key.** That gives it no user data
     and no network. It still has its own container and the base profile's reads of the
     system and its Mach lookups. It inherits nothing from the app.
   - **One process per decode, enforced by the app.** launchd keeps one instance of a
     service and routes every connection to it, so the service exits after each request.
     The app doesn't trust it to:
     - it records each connection's `processIdentifier`;
     - it opens no new connection until that pid has exited, watched with kqueue
       `EVFILT_PROC` / `NOTE_EXIT`;
     - a connection that reports a pid which has already served a request is refused, and
       previews are off for the session.
     So a compromised decoder that skips its exit never gets a second image.
   - **Exiting without losing the reply.** The service exits from its connection's
     `invalidationHandler`, which runs once the app has the reply and invalidates. A
     backstop `_exit` 2 s after the reply covers an app that never invalidates. The app
     treats the interruption that follows as normal.
   - **A watchdog armed first.** When a request arrives, before any ImageIO call, the
     service arms a timer (8 s) on its own dispatch queue, separate from the decode's, that
     calls `_exit(1)`. It also sets a hard `RLIMIT_CPU` of 10 s, so the kernel ends a hang
     even if the timer never runs. `invalidate()` from the app only drops the connection.
   - **Only the four formats can decode:**
     - At start, once and before its listener resumes, the service calls
       `CGImageSourceSetAllowableTypes` with the PNG, JPEG, GIF and WebP UTIs, which turns
       every other ImageIO plugin off for the process. It's available on the app's
       deployment target (macOS 26; the function needs 14). A non-zero status is `_exit(1)`
       (fail closed), so the service never listens.
     - A per-request check that `CGImageSourceGetType` equals core's sniffed kind comes on
       top. It isn't the barrier, since both sniff the same magic bytes.
   - **The caps are checked again as ImageIO reads them:** `CGImageSourceCopyPropertiesAtIndex(0)`
     width and height against core's caps (8192 a side, 40 MP), before any decode. Core and
     ImageIO can disagree on which size a file has: a WebP canvas vs its frame, a GIF screen
     vs its frame, a JPEG with several SOF markers.
   - **Frame 0 only, at reduced size:** `CGImageSourceCreateThumbnailAtIndex(0,
     MaxPixelSize: 720, CreateThumbnailFromImageAlways, CreateThumbnailWithTransform,
     ShouldCacheImmediately)`.
     - `Always` means an EXIF thumbnail chosen by the sender is never used in place of the
       image.
     - JPEG decodes at reduced scale. PNG, GIF and WebP decode in full and then scale, which
       is why the 40 MP cap is the real memory bound.
   - **The protocol carries plain values only:**
     - request `(bytes: Data, kind: Int)`;
     - reply `(code: Int, width: Int, height: Int, rgba: Data)`, with no `NSError` and no
       object graphs;
     - the interface's allowed classes are set explicitly;
     - the reply block is answered once, and a second call is ignored.
     The service draws into sRGB, 8 bits, `premultipliedLast | byteOrder32Big`.
2. **The app treats the reply as untrusted** (the equivalent of GTK's `frame_fits`), in this
   order:
   - `code == 0`;
   - `0 < width ≤ 720` and `0 < height ≤ 720`, checked before any arithmetic. The values
     came over XPC and may be negative;
   - the longer side is at most core's longer side, and the shorter at most the shorter,
     since EXIF rotation may swap them and a thumbnail only ever shrinks;
   - `rgba.count == width × 4 × height`, with `multipliedReportingOverflow`.

   Then the app builds the `CGImage` with **its own** colour space (sRGB), bitmap info and
   bits per pixel, never any taken from the reply, over a `CGDataProvider` that owns a copy
   of the bytes. Anything that fails a check means no preview.
3. **Timeouts and cancellation:**
   - The app's timeout is 10 s from the request, after which it invalidates the connection.
     The service's own watchdog (8 s) has already ended the process by then.
   - A row that goes away invalidates its request's connection and `cancelTransfer`s its
     fetch.
4. **The attachment row, as GTK's:**
   - A preview is offered only for a declared `image/png`, `image/jpeg`, `image/gif` or
     `image/webp`, and core's sniff still decides.
   - Files of 4 MiB or less (core's `FileInfo.size`) preview by themselves, unless the
     connection is expensive (`NWPathMonitor`: `isExpensive` or `isConstrained`) and the
     file isn't cached.
   - Anything up to `previewMaxBytes()` (16 MiB, core's constant) gets "Show preview".
     Core refuses anything bigger before it fetches.
   - The thumbnail is at most 360 × 240 points, and clicking it runs Open.
   - **One decode at a time**, newest first, and rows that have gone are skipped. There's
     one service instance, so two connections at once would share a process and see each
     other's bytes, and one's exit would kill the other. Fetches (core) can overlap; only
     decodes are serial.
5. **Tests:**
   - The validator (pure Swift):
     - exact sizes pass;
     - each of these is refused: a short buffer, one extra byte, a zero or negative side, a
       side over 720, a size near `Int.max` (overflow), a reply bigger than the header, and
       a rotated reply within the header's sides.
   - **Tests hosted in the app** (a service name only resolves inside its own bundle):
     - two decodes in a row run in **different pids**, and each reply arrives (not only
       the pid change);
     - a service that skips its exit (a test-only request in Debug builds) makes the app
       refuse the reused pid and turn previews off;
     - a fixture that hangs the decoder (a test-only request in Debug builds) is killed
       within the watchdog;
     - a probe build of the service can't open `~/` or `connect()` anywhere;
     - fixtures that decode within the caps: a PNG (8-bit, 16-bit, indexed with tRNS), an
       APNG (first frame), a JPEG (CMYK, EXIF-rotated), a GIF (first frame) and a WebP;
     - fixtures that are refused: a PNG labelled as a JPEG, TIFF, HEIC, PDF and SVG, a
       header/canvas mismatch over the cap, and a 40 MP image (peak memory measured and
       written down).
   - The built Release service's entitlements are exactly the sandbox key (Debug adds
     `get-task-allow`), and it has the hardened-runtime flag and the app's team ID.
   - The queue: one decode in flight, newest first, rows that have gone skipped.

## Not doing

- QLThumbnailGenerator. It runs out of process, but it takes a file URL, which would put
  plaintext image bytes on disk. It also picks its generator from any installed Quick Look
  extension, so we wouldn't choose the decoder.
- Animation, full-size viewing (that's Open), and previews of other kinds.

## Where it fails

- **The service can't launch** (signing, sandbox): no previews. It's logged once, and the
  row stays a plain file row. There's never an in-process fallback.
- **A decoder exploit inside the service:**
  - It lasts for one image. The app never sends a second image to the same pid, and waits
    for it to exit. The watchdog and `RLIMIT_CPU` end a hang.
  - It has no user data and no network, only the base sandbox profile.
  - Its reply can only be pixels, which are validated.
  - It could return wrong pixels for its own image only, never for another attachment.

## Review round 1

Two reviewers (vibe, and a second model standing in while codex is unavailable).

Taken:
- **Process reuse.** One service instance serves every connection, so the service now exits
  after each reply.
- **`invalidate()` doesn't stop a decode.** Taken, with a watchdog in the service.
- **`CGImageSourceSetAllowableTypes`** as the real barrier on formats.
- **"No files" overstated.** It's now "no user data and no network", with the base profile
  stated.
- **Plain values only** on the protocol.
- **ImageIO's own reading of the caps**, checked in the service.
- **Downscaled decoding is JPEG only.** Stated, so the 40 MP cap is kept as the memory bound.
- **Orientation**, compared side by side after sorting.
- **The validator's order**, with overflow checks and negative values.
- **The app picks the pixel format**, and the data provider owns a copy of the bytes.
- **Tests hosted in the app** for pids, the watchdog and the sandbox probe, plus the
  fixtures, and the entitlements check on Release.

Rebutted:
- **An `XPCServiceMemoryLimit` of about 6 MiB.** No such documented key exists, and a real
  decode of a 40 MP image needs far more. The cap, the per-image process and the watchdog
  bound it instead.
- **The app doesn't enforce 4 MiB.** 4 MiB is only when a preview starts by itself. Core
  refuses anything over 16 MiB before it fetches (`file.preview_refused`), which is the
  bound that matters. `previewMaxBytes()` is core's constant, bound in #155.
- **Fuzzing ImageIO.** That's Apple's decoder, and we contain it rather than test it. The
  fixtures cover our own handling.

## Review round 2

Vibe: no objections. The second reviewer accepted the three rebuttals and raised five
points, all taken:
- Exiting was voluntary, so a compromised decoder could skip it. **The app now enforces one
  pid per decode.**
- Two decodes in flight would share the one process. **Now one decode at a time.**
- Exiting straight after the reply could drop it. **The service now exits on invalidation,
  with a backstop.**
- **The watchdog is armed before any ImageIO call**, on its own queue, plus `RLIMIT_CPU`.
- **`CGImageSourceSetAllowableTypes`'s status is checked**, and the service fails closed.

All points taken, none disputed: closed.
