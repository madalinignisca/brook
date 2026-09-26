# Inline image previews on the Mac: plan

Spec: `2026-09-26-mac-previews-spec.md` (closed, including its amendment after the spike).
Heavy. Two PRs: the decoder now, and the attachment row after #79.

## Done already: the spike (step 0)

These run from the sandboxed app, and they're kept as the build's foundation:
- the targets, and the worker's re-sign step;
- a new worker pid per image, at 0.1 s for two;
- the worker's `connect()` gets `EPERM`;
- the deep signature verifies on clean and incremental builds.

## PR 1: broker, worker, client, validator

1. **`Shared/ImageDecoding.swift`** (compiled into the app and the broker):
   - the `@objc` protocol `decode(_ bytes: Data, kind: Int, reply: (Int, Int, Int, Data) ->
     Void)`;
   - `enum ImageKindCode: UInt8 { png = 1, jpeg, gif, webp }`, and in `#if DEBUG` only,
     `hang = 200` and `probe = 201`;
   - `ReplyCode`: 0 ok, 1 refused (type), 2 over the caps, 3 no image, 4 draw, 5 worker
     failed, 6 timeout, 7 bad frame;
   - the frame layout constants: a 20-byte header, 720 as the largest side.
2. **`Shared/Frame.swift`** (in project.yml, added to the broker's, the worker's and the
   tests' `sources`): the frame's encode, and `readFrame(fd:deadline:)`, which reads the
   20-byte header and checks it: sides greater than 0 and at most 720, and
   `length == w × h × 4` with overflow-checked arithmetic. It then reads exactly `length`
   bytes and requires EOF after them. It's tested over real pipes, not only on a parsed
   header.
3. **The worker, `BrookImageWorker/main.swift`:**
   - hard and soft `RLIMIT_NPROC` = 0 and `RLIMIT_CPU` = 10. Each `setrlimit` is checked,
     and any failure is `_exit(1)`;
   - `CGImageSourceSetAllowableTypes`, or `_exit(1)`;
   - `kind` from 1 byte, then the bytes up to EOF, refused over 16 MiB;
   - the decode (spec §1, step 4), then the frame to stdout;
   - in `#if DEBUG`: `hang` spins forever, and `probe` returns a frame whose 4 × N bytes
     encode a result line. The line covers `connect`, a write under `~`, `fork`,
     `posix_spawn`, which fds are open, the limits, and whether it can look up the broker's
     service.
4. **The broker, `BrookImageDecoder/main.swift` and `Worker.swift`:**
   - At start, it empties `NSTemporaryDirectory()`.
   - The listener has `setConnectionCodeSigningRequirement`:
     - `identifier "dev.brook.Brook"`;
     - when the broker's own signature has a team (read at runtime with
       `SecCodeCopySigningInformation`, `kSecCodeInfoTeamIdentifier`),
       `and anchor apple generic and certificate leaf[subject.OU] = "<TEAM>"`;
     - Release with no team fails closed (the broker exits).
     `Signing.xcconfig` applies to all three targets, so the broker and the worker are
     signed like the app.
   - `decode` returns at once. The work runs on a concurrent queue, and the reply block is
     called later, exactly once (NSXPC delivers on the connection's serial queue, so a
     blocking `decode` would put the second request's deadline behind the first).
     - The interface's allowed classes are set explicitly, and the reply carries no
       `NSError`.
     - It maps `kind` to `ImageKindCode`, or replies 1. It spawns the worker with
     `posix_spawn` (`POSIX_SPAWN_CLOEXEC_DEFAULT`, `POSIX_SPAWN_SETPGROUP`, file actions
     for fds 0, 1 and 2, with 2 as `/dev/null`). It closes its copies of the child's pipe ends
     right after the spawn: a kept write end means EOF never comes, and every request would
     run to the deadline. It writes on its own queue with
     `F_SETNOSIGPIPE`, and reads the header, then the body, with a deadline 8 s from the
     spawn, using `poll` on the read fd. On a timeout or a bad frame it sends
     `kill(pid, 9)` and then `killpg`, and always reaps with `wait4(pid, …)` (by pid, with
     rusage). Status 0 is required only when the broker didn't kill the worker.
     - When a request's connection goes, the broker kills that request's worker.
   - Two requests may run at once (one worker each). Each has its own pipes and its own
     pid.
5. **The app's client, `Brook/Previews/ImageDecoder.swift`:** an actor.
   - **One `NSXPCConnection` per request**, so a timeout invalidates only its own request
     (and the broker kills only that worker).
   - At most 2 requests in flight. The rest queue, newest first, and one whose row has gone
     is dropped.
   - A 10 s timeout per request. Replies go through `PreviewValidator`.
   - When the connection can't be made, previews are off and it's logged once.
6. **`Brook/Previews/PreviewValidator.swift`:** pure. It builds the `CGImage` in the app's
   own format, from a copied `CFData`.
7. **Tests:**
   - `FrameTests` (pure): each header refusal, and exact bodies.
   - `PreviewValidatorTests` (pure): each case in spec §5.
   - `ImageDecoderServiceTests` (hosted in the app, through the **raw XPC proxy**, not the
     `ImageDecoder` actor, since the probe's frame isn't an image the validator would pass):
     - distinct worker pids under one broker (the probe reports `getpid` and `getppid`);
     - `hang` gets code 6 in 7 to 9.5 s. In Debug only, the reply's rgba carries the
       `wait4` result and whether `killpg(pgid, 0)` gave `ESRCH`. The real proof is the
       reap by pid plus `NPROC = 0`, which means no children;
     - two `hang` requests at once both end in under 9.5 s: the requests run concurrently;
     - the probe's results;
     - the fixtures, generated in the test (ImageIO encoding in the test process only)
       plus committed WebP, SVG and PDF files;
     - 40 MP (a compressible image, since core refuses anything over 16 MiB): the peak
       resident size is taken from `wait4`'s `ru_maxrss` (in bytes on macOS) and written
       down.
   - The client's queue (2 in flight, newest first, gone rows dropped): a model test in
     this PR, where the queue is built.
   - The release check, in `build.sh release`:
     - both targets' entitlements are exact;
     - the runtime flag and the team are set;
     - `--deep --strict` passes;
     - the Release broker and worker contain no `hang` or `probe` symbols or strings. It's
       a static check, since a hardened Release app can't host tests.
8. The spike's `ping` and `spawnWorker` and their test are replaced **in step 1**, so no
   step leaves a test that doesn't build.

## PR 2 (after #79): the row

`AttachmentRow` gains the thumbnail, as spec §4, with a model test for the row and the
queue.

## Where this fails

- **`posix_spawn` with `CLOEXEC_DEFAULT` from inside the sandbox.** The spike used
  `Process`, so step 4 checks this before anything else. `Process` can't give
  `SETPGROUP` or `CLOEXEC_DEFAULT`, so if `posix_spawn` is refused, we **stop and amend the
  spec**. There's no quiet fallback.
- **The code-signing requirement refuses the Debug test host.** The tests run inside
  Brook.app, so the identifier matches. If the requirement API refuses ad-hoc builds,
  it's `identifier` only in Debug, and the release check covers Release.
- **The throttle again:** the broker is long-lived, so no relaunch. The pid test measures
  it.

## If it stops halfway

Nothing here reaches the UI until PR 2. Each step leaves the app as it is today, with the
broker simply never called.

## Plan review, round 1

Two reviewers (vibe, and a second model standing in while codex is unavailable).

Taken, all nine from the second reviewer and two from vibe:
- Concurrency: NSXPC's serial delivery. `decode` returns at once, and the app uses a
  connection per request.
- The team read at runtime, `anchor apple generic`, and `Signing.xcconfig` on all targets.
- The spec items the plan missed: kill on connection loss, closing the child's pipe ends,
  exact length then EOF, a reply sent once, and frame tests over pipes.
- The probe tested through the raw proxy.
- The Debug hang reply carries the reap result.
- `ru_maxrss` in bytes, and `wait4` for the reap.
- The spike replaced in step 1.
- A stop in place of a quiet `Process` fallback.
- `setrlimit` checked.

Rebutted (vibe):
- **A larger 16 MiB cap.** Core refuses anything over 16 MiB before it fetches. The 40 MP
  fixture is compressible.
- **`RLIMIT_NPROC` = 1 in case ImageIO forks.** It doesn't fork, and the fixture tests would
  fail if it did.
- **`RLIMIT_CPU` against the 8 s deadline.** The CPU limit is only a second bound. The 8 s
  wall-clock kill is the real one, whatever the core count.

## Deviations in the implementation (for the reviewers)

- **The code-signing requirement applies only to team-signed builds.** Under test, the ad-hoc
  Debug host app carries injected code, so its dynamic signature doesn't validate, and even
  an identifier-only requirement refused it (measured: every request failed with the
  requirement on, and passed with it off). A team-signed build has the full requirement, and
  a Release broker without a team exits. In Debug the probe still shows that a worker can't
  reach the broker (`brokerClient=no`): the lookup itself fails from inside the worker.
- **Debug reports carry text.** Failure frames are all zeros in Release. In Debug only, the
  `report` code (the probe) and the broker's timeout reply carry text for the tests.
- **The Release checks** are in `check-decoder.sh`, which `build.sh` runs after every plain
  build (Release adds the hook, runtime and team checks). It was tested three ways:
  - a plain Debug build passes;
  - an ad-hoc Release build passes everything except the team, which needs the owner's
    identity;
  - a Debug build checked as Release is caught ("a Debug test hook is in
    BrookImageDecoder").

## Measured

- Two images, two worker pids under one broker pid.
- A hung worker ends at 8.1 s, and two hung workers together at 8.0 s.
- A 40 MP PNG (8000 × 5000, flat colour, 536 KB) becomes a 720 × 450 thumbnail with a peak
  resident size of **28 MB**, against 12 MB for a 2-pixel image. ImageIO's PNG thumbnailing
  streams rather than decoding the whole image; content that doesn't compress will cost
  more, within the 40 MP cap. Measured with `/usr/bin/time -l` on an ad-hoc copy of the
  worker, outside the sandbox. The decode code is the same.

## Implementation review, round 1

Two reviewers (vibe, and a second model standing in while codex is unavailable).

Taken:
- **A worker that replies and then lingers is bounded.** The broker waits for its exit
  without reaping (`waitid` with `WNOWAIT`), and kills it at the deadline even after a good
  frame, which then counts as a failure. Tested with a Debug `linger` kind.
- **Cancel races.** The session refuses new runs once it's cancelled. A run cancelled
  while its worker is being spawned kills the worker the moment its pid is known.
- **No kill after exit.** The pid is cleared once `waitid` sees the exit, before the reap,
  so a kill never reaches a reused pid.
- **`wait4`'s return is checked.** Anything other than the child's pid is a failure.
- **An `EINTR` on the final EOF read is retried.**
- **The "group is empty" assertion couldn't fail.** The probe now reports whether it leads
  its own process group.
- **The unknown-kind test sends a valid PNG,** so a kind that wrapped to `png` would show
  as a decode.
- **Previews go off only after three unanswered requests in a row.** One can be a broker
  killed under memory pressure.
- **The timeout task is cancelled** when a reply comes.
- **At most 2 workers per broker,** whatever the clients ask.
- **Workers get an empty environment.**
- **Release re-signs the worker with a secure timestamp** (notarization).
- **Small fixes:** the temp directory is joined as a URL, and the spawn setup's return
  values are checked.

Rebutted (vibe):
- **File actions leaked on a `pipe` failure.** They're created after both pipes.
- **The child's pipe ends leaked on a spawn failure.** They're closed unconditionally, right
  after the spawn.
- **A `poll` error counted as ready.** The `read` that follows then fails, and the frame
  is refused as malformed. That's the right outcome.

Noted: the fake-client stubs in `CallModelTests.swift` are the separate #158 fix, merged
into this branch so it builds. The 40 MP peak was measured outside the sandbox (see
"Measured").
