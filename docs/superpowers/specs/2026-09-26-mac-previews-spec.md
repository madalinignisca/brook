# Inline image previews on the Mac (#66): spec

Status: spec, closed after review round 2; amended after the spike (see the end), and the amendment closed after its round 2. Review dial: **Heavy**. Decoding bytes a stranger chose
is the attack surface here, as for GTK (`2026-09-26-image-previews.md`).

The UI waits on local data, and so on the provisioning profile (#79): `previewFile` reads
only from the encrypted cache. The decoder service and its tests don't wait on anything.

## The rule

No image decoder runs in the app's process. `NSImage`, `CGImageSource` and anything else
built on ImageIO decode in-process, so the app never hands attachment bytes to them. It
only builds a `CGImage` from raw RGBA that it has checked itself, in a pixel format it
chose itself.

## Done means

1. **A broker service and one worker process per image.**
   - **The broker** is an XPC service (`BrookImageDecoder.xpc` in `Contents/XPCServices`,
     signed by the same team, with the hardened runtime).
     - It is sandboxed, with no entitlements beyond the sandbox key: no user data and no
       network. It still has its own container, the base profile's reads of the system,
       and its Mach lookups.
     - It is long-lived and **never decodes or parses image bytes**. It hands them to a
       worker and reads the worker's fixed-format reply.
   - **The worker** is `BrookImageWorker`, an executable in the broker's bundle. The broker
     spawns one per image through `posix_spawn`, in its own process group.
     - Its entitlements are exactly `app-sandbox` and `inherit`, so it runs in the broker's
       entitlement-free sandbox.
     - The build re-signs it with exactly those two keys, in every configuration. A child
       with any other key beside `inherit` is killed at launch, and Xcode adds testing
       exceptions to what it builds for tests.
     - A decoder exploit lives in that one worker, for that one image. The next image
       goes to a new process.
     - The spawn uses `POSIX_SPAWN_CLOEXEC_DEFAULT`. Only fds 0, 1 and 2 pass through
       file actions (stderr is `/dev/null`), so a worker never inherits another image's
       pipes. The broker closes its copies of the child's ends right after the spawn.
   - **The worker, in order:**
     1. It sets `RLIMIT_NPROC` to 0 (no `fork` or `posix_spawn`, so an exploit can't leave
        a process behind) and `RLIMIT_CPU` to 10 s. For both, the hard limit equals the
        soft one, so they can't be raised again. `RLIMIT_CPU` is only a second bound: past
        it, XNU sends `SIGXCPU`, which a compromised worker could catch. The broker's
        `SIGKILL` at the deadline is the real bound.
     2. It calls `CGImageSourceSetAllowableTypes` with the PNG, JPEG, GIF and WebP UTIs. A
        non-zero status is `_exit(1)`.
     3. It reads `kind` (1 byte) and then the image from stdin until EOF. More than
        16 MiB is `_exit(1)`, never truncated.
     4. It decodes:
        - `CGImageSourceGetType` must equal the kind's UTI;
        - `CopyPropertiesAtIndex(0)`: width and height within 8192 a side and 40 MP;
        - `CreateThumbnailAtIndex(0, MaxPixelSize: 720, FromImageAlways, WithTransform,
          ShouldCacheImmediately)`;
        - drawn into sRGB, 8-bit, `premultipliedLast | byteOrder32Big`, with
          `bytesPerRow = w × 4`.
     5. It writes a frame to stdout: `code u32 | width u32 | height u32 | length u64`, all
        little-endian, then `length` bytes. Then it exits.
     - `FromImageAlways` means an EXIF thumbnail chosen by the sender is never shown in the
       image's place.
     - JPEG decodes at reduced scale. PNG, GIF and WebP decode in full and then scale,
       which is why the 40 MP cap is the real memory bound.
   - **The broker, per image:**
     - It maps the request's `kind` to the allowed set (the four kinds, plus the test kinds
       in Debug builds only) before it becomes a byte. Anything else is refused.
     - It spawns the worker. The 8 s deadline starts at the spawn and covers writing too.
     - It writes the input from its own thread, with `SIGPIPE` off for the pipe
       (`F_SETNOSIGPIPE`), and closes stdin after the last byte. A worker that exits early
       costs a write error, never the broker.
     - It reads the 20-byte header, then refuses the frame unless all of these hold:
       - `0 < width ≤ 720` and `0 < height ≤ 720`;
       - `length == width × height × 4`;
       - it reads exactly `length` bytes, and EOF follows;
       - the worker exits with status 0.
     - At the deadline, or on a bad frame, it sends `kill(pid, SIGKILL)` to the worker
       itself, then `killpg` for good measure. The worker may have moved to another group
       by `setpgid`, which needs no fork. The pid can't be reused before the broker reaps
       it.
     - It always reaps with `waitpid(pid)`, never `-1`, so it never collects the other
       worker's status.
     - It accepts connections only from Brook.app: a code-signing requirement on its
       listener (the app's identifier, plus the team ID in Release). A worker can't use it
       as a client.
     - At start, it empties its own temp directory.
     - It passes the frame's values on to the app as plain values. The app's own checks
       come on top.
   - **The protocol between app and broker carries plain values only:**
     - the request is `(bytes: Data, kind: Int)`;
     - the reply is `(code: Int, width: Int, height: Int, rgba: Data)`;
     - there's no `NSError`, and the interface's allowed classes are set explicitly;
     - the reply block is answered once.
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
   - The broker's 8 s deadline kills the worker, and `RLIMIT_CPU` is a second bound inside
     it.
   - The app's timeout is 10 s from the request, after which it invalidates the
     connection. The broker kills that request's worker when its connection goes.
   - A row that goes away invalidates its request and `cancelTransfer`s its fetch.
4. **The attachment row, as GTK's:**
   - A preview is offered only for a declared `image/png`, `image/jpeg`, `image/gif` or
     `image/webp`, and core's sniff still decides.
   - Files of 4 MiB or less (core's `FileInfo.size`) preview by themselves, unless the
     connection is expensive (`NWPathMonitor`: `isExpensive` or `isConstrained`) and the
     file isn't cached.
   - Anything up to `previewMaxBytes()` (16 MiB, core's constant) gets "Show preview".
     Core refuses anything bigger before it fetches.
   - The thumbnail is at most 360 × 240 points, and clicking it runs Open.
   - At most 2 decodes in flight, newest first, and rows that have gone are skipped. Each
     is its own worker process.
5. **Tests:**
   - The validator (pure Swift):
     - exact sizes pass, and so does a reply rotated by EXIF (sides swapped) within the
       header's sides;
     - each of these is refused: a short buffer, one extra byte, a zero or negative side, a
       side over 720, a size near `Int.max` (overflow), and a reply bigger than the header.
   - The broker's frame reader (pure Swift, over a pipe): a short header, `length` over the
     cap, fewer bytes than `length`, and bytes after it are each refused.
   - **Tests hosted in the app, through the real broker and workers** (the worker is signed
     with exactly its own keys even in test builds, so this is the shipped sandbox):
     - two images decode in two different worker pids under one broker pid;
     - a worker that hangs (a test-only kind in Debug builds) is killed at the deadline,
       and nothing of it survives (its process group is empty);
     - a probe worker (Debug only):
       - can't `connect()`, can't write under `~/`, and can't `fork()` or `posix_spawn()`;
       - has only fds 0 to 2 open;
       - reports its `RLIMIT_CPU` and `RLIMIT_NPROC` with hard equal to soft;
       - can't connect to the broker's service;
     - fixtures that decode within the caps: a PNG (8-bit, 16-bit, indexed with tRNS), an
       APNG (first frame), a JPEG (CMYK, EXIF-rotated), a GIF (first frame) and a WebP;
     - fixtures that are refused: a PNG labelled as a JPEG, TIFF, HEIC, PDF and SVG, a
       header/canvas mismatch over the cap, and a 40 MP image (peak memory measured and
       written down).
   - The built Release bundle:
     - the broker's entitlements are exactly the sandbox key;
     - the worker's are exactly `app-sandbox` and `inherit`;
     - both have the hardened-runtime flag and the app's team ID;
     - the Debug test kinds are compiled out (`#if DEBUG`), and a Release test asks for one
       and is refused;
     - `codesign --verify --deep --strict` passes.
   - The queue: 2 in flight, newest first, rows that have gone skipped.

## Not doing

- QLThumbnailGenerator. It runs out of process, but it takes a file URL, which would put
  plaintext image bytes on disk. It also picks its generator from any installed Quick Look
  extension, so we wouldn't choose the decoder.
- Animation, full-size viewing (that's Open), and previews of other kinds.

## Where it fails

- **The broker can't launch, or a worker can't spawn** (signing, sandbox): no previews.
  It's logged once, and the row stays a plain file row. There's never an in-process
  fallback.
- **A decoder exploit inside a worker:**
  - It lasts for one image, and can't fork.
  - It has no user data and no network.
  - It's killed at the deadline.
  - Its output can only be a frame, checked by the broker and again by the app. It could
    return wrong pixels for its own image only.
  - It shares a sandbox with the broker and the other worker, so it can signal them (a
    denial of service). It also shares their container, temp directory and preferences
    domain, and could leave something there for later workers. The broker empties its
    temp directory at start, but a planted preference value would stay.
  - Its real escape surface is the set of system services the base profile allows it to
    reach: GPU, IOSurface, hardware decoders. That's Apple's to harden, and the per-image
    process keeps each attempt to one image.
- **The broker itself** parses only the fixed 20-byte header, and reads a length it has
  capped.

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

## Amendment after the spike (plan step 0)

The spike ran on this machine, from tests hosted in the sandboxed app.
- **One service process per image fails in practice.** The process exit was seen in under
  1 ms, but launchd then held the relaunch for 10.1 s, its throttle for a job that exits
  soon after it starts. That would mean one preview every 10 s.
- **Replaced by a long-lived broker that spawns a worker per image.** Measured: two images
  in two different worker pids, in 0.1 s for both. The worker's `connect()` to a public
  address fails with `EPERM`, so the inherited sandbox holds.
- **Found along the way:** Xcode adds testing exceptions to sandboxed binaries it builds for
  tests, including read access to all of `/`. A worker with any key beside `inherit` is
  killed at launch. So the build re-signs the worker with exactly its two keys, and the
  deep signature verifies on clean and incremental builds.

The app no longer tracks pids or waits on exits, since the broker owns the workers'
lifetimes. That also answers the pid-recycling note from #157's review. The rest of the
spec is unchanged.

### Amendment review, round 1

Two reviewers (vibe, and a second model standing in while codex is unavailable).

Taken:
- Only fds 0 to 2 reach a worker (`POSIX_SPAWN_CLOEXEC_DEFAULT`).
- The kill goes to the pid itself, since `setpgid` can leave the group without a fork.
  Reaping is by pid.
- No `SIGPIPE`, the write happens on the broker's own thread, and the deadline starts at
  the spawn.
- Hard limits equal the soft ones.
- The broker accepts only Brook.app.
- The shared state is stated, and the temp directory is emptied at start.
- `kind` is checked before it becomes a byte.
- The broker checks the frame fully, including the worker's exit status.
- The Release checks cover `--deep --strict` and compiled-out test kinds.
- The probe reports the limits. That replaces waiting out 10 s of CPU.

Rebutted:
- **`RLIMIT_AS`.** macOS doesn't enforce `RLIMIT_AS` or `RLIMIT_DATA`. The memory bound is
  the 40 MP cap, checked before any pixels are decoded, plus the kill at the deadline.

### Amendment review, round 2

Vibe: no objections. The second reviewer accepted the rebuttal and confirmed nine of the
ten fixes. It found one overstatement: a CPU overrun can be caught (`SIGXCPU`). That's now
worded as a second bound, with the broker's kill as the real one. Confirmed on the way:
`F_SETNOSIGPIPE` exists for pipes, `setConnectionCodeSigningRequirement` needs macOS 13 (the
deployment target is 26), and an identifier-only requirement matches ad-hoc Debug builds.
Closed.
