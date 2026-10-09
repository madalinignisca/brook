# Android: app skeleton that signs in and lists channels (#273): plan

Spec: [2026-10-08-android-skeleton-spec.md](2026-10-08-android-skeleton-spec.md) (merged in #309;
every decision is in its section 3). Status: after review round 2 (Opus LGTM), with the owner's
decisions of 2026-10-09 recorded in section 5; for the owner's approval.

**Changed after peer review (#315, Apple/core agent's non-blocking notes):**
- the macOS/Linux/iOS side of `store.rs` is described as a behaviour-preserving refactor;
- `std::io::Write` joins the keyed-only gating list (only `write_atomically` uses it);
- the TOTP-challenge rendering test is dropped: the Swift challenge has no fields, so that test
  could never fail on a leak;
- client A gets `enablePersistence` before `login` in the TOTP test;
- step 6 notes that iOS is unaffected (grep-checked);
- PR 2 says the main agent tells the Apple/core agent when it merges.

**Changed after review round 2 (Opus LGTM with nits, Codex one finding):**
- the round-1 note now reads "step 6 (Rust and Swift) is one commit" (Codex and Opus);
- step 1's check uses `grep -i android`;
- section 0's step numbers are fixed (Gradle wiring is step 10, the JDK is needed before step 10,
  `dataExtractionRules` come in step 14);
- `testATotpRequiredResultNeverRendersItsChallenge` is simply added: `RedactionTests` has none
  today;
- step 10's break-and-watch no longer quotes a compiler message: any compile failure on the clash
  will do;
- section 5 records the owner's four decisions as decided, including the JDK: a local, checksummed
  JDK 21 under `~/.local`, found by Gradle through `~/.gradle/gradle.properties`.

**Changed after review round 1 (Opus and Codex, both changes requested):**
- **Kotlin generator:**
  - step 9 now builds the arm64 `.so` it reads;
  - `KotlinBindings` declares `uniffi.toml` and the generator source as inputs;
  - the renames' break-and-watch moved to step 10, where the Kotlin is actually compiled.
- **The request to pass `bindings/apple/uniffi.toml` as `--config` was not taken, with evidence.**
  In UniFFI 0.32.2, `--config` expects a *global* file (`[defaults]`, `[crates.<name>]`). A flat
  per-crate file passed there is ignored with an "old-style --config" warning (source:
  `uniffi_bindgen-0.32.2/src/global_config.rs`). The crate's own `uniffi.toml` is found through
  `cargo metadata` (`cargo_metadata.rs`, `{crate_root}/uniffi.toml`), which is what applied the
  renames in the section 0 build.
- **CI:**
  - every action is named with its SHA pin; four come from `release-gtk.yml`, and two new ones
    were looked up with `gh api`;
  - step 3 installs `libssl-dev` as `rust.yml` does;
  - the APK upload is dropped.
- **PR 2:**
  - step 6 (Rust and Swift) is one commit, so the Mac build never breaks;
  - a new server step (`test_activation_signs_out_other_sessions` also checks the activating
    device's own old pair), flagged for the `auth-reviewer`;
  - the test reads the one `session:` slot instead of rebuilding its name.
- **Session model:**
  - the factory returns the class `FfiBrookClient` (which has `close()`), and fakes subclass it
    through `NoHandle`;
  - persistence is always on;
  - the Mac tests to port are listed by name, and the form state lives in `SignInForm`;
  - `testFewRecoveryCodesLeftAreFlagged` is excluded.
- **Step 2:**
  - the emulator check now runs the whole `--lib` and states the expected result. Re-run on
    2026-10-08: the same 12 failures;
  - the `mkfifo` test is gated, with the verified cause (SELinux denies FIFOs to `adb shell` in
    `/data/local/tmp`);
  - the offline test's gating reason was confirmed;
  - the plain-only items carry `cfg(any(target_os = "android", test))`, and test 1 asserts that no
    slot was made.
- **Records:** the main agent's own edits (CLAUDE.md §7's example, CLAUDE.md §3's table) are
  listed; the generator is documented in `clients/android/README.md`, since `bindings/apple` has no
  README; steps 12-14 have Docs lines.
- **Smaller fixes:**
  - step 1's "builds on today's main" was false (verified: today's `main` fails in
    `libsqlite3-sys`'s build script for Android) and is gone;
  - `clients/android/.gitignore`;
  - the rollback names the paired changes;
  - `KeystoreSlot` fsyncs the directory after the rename;
  - the code field uses the authenticator hint `2faAppOTPCode`, not the SMS one (both verified);
  - no "Archived" text;
  - the `FfiSecondFactor.Code` `toString` risk is recorded for the TOTP-enrolment ticket.
- **Owner decisions:**
  - the Compose UI tests are now decision 3;
  - `warningsAsErrors` is marked as this plan's own choice;
  - `targetSdk = 37` is decided (owner decision 1, 2026-10-09).

## 0. What was checked on this machine before planning (2026-10-08)

Everything below was run, not assumed. A throwaway copy of the repo and a throwaway Gradle project
were used outside the worktree and deleted afterwards.

| Claim | How it was checked | Result |
|---|---|---|
| SDK packages | `sdkmanager --list_installed` | `platforms;android-37.0`, `build-tools;37.0.0`, `ndk;30.0.16248370` (r30), `emulator` 37.2.12, `platform-tools` 37.0.1, `system-images;android-33;google_apis;x86_64`, `system-images;android-37.0;google_apis;x86_64` |
| NDK has API 33 compiler wrappers | `ls .../toolchains/llvm/prebuilt/linux-x86_64/bin` | `aarch64-linux-android33-clang`, `x86_64-linux-android33-clang`, `llvm-ar` |
| Rust targets | `rustup target list --installed` | `aarch64-linux-android`, `x86_64-linux-android` (rustc 1.98.1) |
| **Core builds for Android with the target-gated `rusqlite`** | `core/Cargo.toml` changed as in step 2, then `cargo build --locked -p brook-ffi --target x86_64-linux-android` and `--target aarch64-linux-android --release` | Both build, **`Cargo.lock` unchanged** (`--locked` passed). The `.so` needs only `libc`, `libm`, `libdl` (no `libcrypto`), has no SQLCipher strings, bundles SQLite 3.50.2, and its LOAD segments are 16 KB aligned (NDK r30 default). |
| Clippy for Android, all targets | `cargo clippy --locked -p brook-core -p brook-ffi --all-targets --target x86_64-linux-android -- -D warnings` | Clean on today's code. With a prototype of step 2's `store.rs` gating, it lists exactly the keyed-only items left to gate (unused imports, `Kind::slot`, `write_atomically`), which is the point of running it. |
| The plain path is testable on the host | Prototype of step 2's `Protection` enum plus one plain test | `cargo test --locked -p brook-core --lib`: 527 passed; host clippy clean. |
| **Core's tests run on an emulator** | `cargo test -p brook-core --lib --target x86_64-linux-android --no-run`, `adb push`, run on the API 33 image (twice: during planning, and again for round 1) | 514 pass, 12 fail on **today's** store code with step 2's `Cargo.toml`. 11 rely on the key, which proves the keyed code is wrong on Android: SQLite ignores `PRAGMA key`, so it writes plaintext and never detects a missing key. The 12th is `snapshot_tests::only_a_regular_file_is_copied`: `mkfifo` in `/data/local/tmp` is refused by SELinux for the `adb shell` domain (`mkfifo: ... Permission denied`, also by hand), which is a limit of the harness, not of core. |
| Today's `main` does not build for Android | the same `--no-run` build without step 2's `Cargo.toml` change | fails in `libsqlite3-sys`'s build script (SQLCipher) |
| `avdmanager` works with the installed JRE | `avdmanager create avd ...` with no JDK | yes |
| **UniFFI 0.32.2 generates Kotlin from the Android `.so`** | `uniffi_bindgen_main()` bin, `generate --library <arm64 .so> --language kotlin`, run from the repo root | One file, `uniffi/brook_ffi/brook_ffi.kt`, package `uniffi.brook_ffi`, library name `brook_ffi`, uses JNA and kotlinx-coroutines. The crate's `bindings/apple/uniffi.toml` is picked up with no flag, through `cargo metadata`; `--config` is for a different, global file format (see the round 1 note). |
| Generated objects | UniFFI's `ObjectTemplate.kt` and the generated file | Each object is `open class X : Disposable, AutoCloseable, XInterface` with a `constructor(noHandle: NoHandle)` meant for fakes. `close()` is on the class, not on `XInterface`. |
| Autofill hints | Compose `ui-android` 1.12.1 classes; `androidx.autofill` 1.3.0 `HintConstants` | Compose has `ContentType.SmsOtpCode` and a `ContentType(String)` factory. The authenticator-app hint is `"2faAppOTPCode"` (`AUTOFILL_HINT_2FA_APP_OTP`). |
| **The generated Kotlin does not compile as is** | Gradle build of a Compose app that includes it | Two kinds of error: `FfiMediaEngine.close` (async) clashes with `AutoCloseable.close()`; error variants with a field named `message` (`LoginError.Network`, `LoginError.InvalidServerUrl`, `LoginError.Api`, `FfiEngineError.Failed`) clash with `Throwable.message`. A Kotlin-only `[bindings.kotlin.rename]` table in `bindings/apple/uniffi.toml` (step 9) fixes both; with it the build is clean, with no Kotlin warnings. |
| Toolchain versions | Gradle 9.8.1, AGP 9.4.1, Compose compiler plugin 2.4.21, Compose BOM 2026.09.00, `compileSdk = 37`, `buildToolsVersion = "37.0.0"`, `ndkVersion = "30.0.16248370"`, JNA 5.19.1 (`@aar`), kotlinx-coroutines 1.11.0, activity-compose 1.13.0 | `assembleDebug testDebugUnitTest` succeeded. `buildEnvironment` shows Kotlin resolved to 2.4.21. AGP stripped the `.so` files (arm64 16.6 MB -> 10.8 MB). A JVM unit test that implements the generated `FfiKeySlot` and throws `FfiKeySlotException.Exists` passed without the native library. |
| Gradle wiring of cargo and bindgen | Two small task classes and AGP's `addGeneratedSourceDirectory` (step 10) | One `assembleDebug` built both ABIs, generated the bindings, and packaged `lib/arm64-v8a/libbrook_ffi.so` and `lib/x86_64/libbrook_ffi.so` plus JNA's `libjnidispatch.so`. |
| **It runs** | Debug APK on the API 33 emulator | A sync FFI call (`conversationLabel` gave `#general`) and an async one (`FfiBrookClient(...).login` to a closed port gave `LoginException.Network`) both worked. |
| Android Lint | `lintDebug` with `warningsAsErrors = true` | Generated sources not flagged. The only finding was the throwaway manifest's `allowBackup` without `dataExtractionRules`, which step 14 provides anyway. |
| Latest stable versions | Google Maven and Maven Central metadata, `services.gradle.org` | AGP 9.4.1 (9.5 is alpha), Gradle 9.8.1 (released 2026-10-07), Kotlin 2.4.21 (2.5.0 is Beta1), Compose BOM 2026.09.00, JNA 5.19.1, coroutines 1.11.0, androidx.test runner 1.7.0, ext-junit 1.3.0. AGP 9.4: maximum API 37, minimum Gradle 9.6.0, JDK 17+. |

**Not verifiable here, named:**
- **This machine has a Java 21 runtime, not a JDK.** `archlinux-java` lists only `java-21-openjdk`
  and there is no `javac`, so Gradle cannot compile. The checks above used a downloaded, checksummed
  Temurin 21 JDK, since deleted. Decided (owner decision 4): the main agent installs a local,
  checksummed JDK 21 under `~/.local` before step 10. No system package.
- **Android 17 local network protection.** API 37 declares `android.permission.ACCESS_LOCAL_NETWORK`.
  Whether an app targeting 37 needs it to reach a LAN server (the test VM, `10.0.2.2` from the
  emulator) could not be checked: the API 37 image would not stay up headless here (its
  `system_server` kept restarting). See risk 1 and owner decision 1.
- What the GitHub `ubuntu-24.04` runner has preinstalled in its Android SDK. The CI job installs
  the exact packages with `sdkmanager`, so it does not depend on that.
- The instrumented-test setup (`androidTest`, step 11). It is standard, but it was not built here.
- Mozilla's `rust-android-gradle` plugin was not evaluated (see Approach).

## 1. Approach

Three PRs, each one reviewable on its own:

- **PR 1 `core: plain SQLite on Android`.** On `target_os = "android"`, `rusqlite` uses `bundled`
  instead of `bundled-sqlcipher` (same 0.37), and `store.rs` opens databases with no key pragmas
  and no key check. A small `Protection` enum makes the plain path compile on Android and in host
  tests, so CI exercises the same functions on the host. A new CI job runs Android clippy and the
  store tests. For macOS, Linux and iOS, `store.rs` gets a behaviour-preserving refactor: the keyed
  path is today's code split into functions, with the same pragmas, key check and outcomes.
- **PR 2 `ffi: tokens no longer cross the FFI`.** `FfiSession` keeps only `user`. Its own PR, as
  the spec requires, reviewed by Opus, the Apple/core agent and the `auth-reviewer`.
- **PR 3 `android: app skeleton signs in and lists channels`.** A Gradle project in
  `clients/android`. Two plain Gradle tasks run cargo and UniFFI's generator, and a
  Compose/Material 3 app sits on the generated Kotlin.

**How cargo and the generator are wired in.** There is a shell script, `clients/android/with-ndk.sh`,
that sets the NDK compiler, archiver and linker for both Android targets and then runs its
arguments. Two small task classes in `app/build.gradle.kts` call it through Gradle's
`ExecOperations` and hand their output folders to AGP with
`variant.sources.{jniLibs,kotlin}.addGeneratedSourceDirectory`, so AGP orders the tasks itself and
no `preBuild` hook is needed. The same script serves CI's Android clippy. Alternatives:
- `cargo-ndk` would set the same variables. But it is one more tool to install and pin, on every
  machine and in CI, and the bindgen step would still need wiring.
- `rust-android-gradle` is a third-party Gradle plugin that has to follow every AGP major release.
  Not evaluated.
- The script is about fifteen lines, and the NDK path comes from AGP's own
  `androidComponents.sdkComponents.ndkDirectory`, so nobody sets `ANDROID_NDK_HOME`.

**Rust is always built `--release`, for the debug APK too.** Measured: the debug x86_64 `.so` is
210 MB unstripped and 25 MB stripped. The release one is 11 MB stripped, and debug Rust runs slowly.
Nobody debugs Rust on a device in this scope.

**No DI framework, no navigation library, no ViewModel library.**
- **No DI:** the app has one long-lived object, `SessionModel`, which `BrookApp` (the `Application`
  subclass) owns. One process means one model, so it survives rotation and activity recreation
  without a ViewModel. The restore runs once per process, as on the Mac.
- **No navigation library:** there are three screens (Signing in, Sign in with its code step, the
  list), chosen by a `when` on the model's phase. Back is Compose's `BackHandler` on the code step,
  and the platform default on the list (leave the app).
- **Settings:** the platform's `SharedPreferences`, not DataStore.

## 2. Steps

Each step names its implementer. "Main agent" means the main agent itself (CI, per CLAUDE.md §1).
The Rust gate below is CLAUDE.md §3's; run it from the repo root:

```sh
cargo fmt --all -- --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
```

The Android clippy command used below (after step 1 exists):

```sh
clients/android/with-ndk.sh "$HOME/Android/Sdk/ndk/30.0.16248370" \
  cargo clippy --locked -p brook-core -p brook-ffi --all-targets --target x86_64-linux-android -- -D warnings
```

### PR 1: `core: plain SQLite on Android` (closes nothing; refs #273)

**Step 1: the NDK environment script.** *brook-android-implementer.*
- New `clients/android/with-ndk.sh` (SPDX lines, `set -eu`), usage `with-ndk.sh <ndk dir>
  <command...>`. For `aarch64-linux-android` and `x86_64-linux-android` it exports
  `CC_<triple_with_underscores>` and `CARGO_TARGET_<TRIPLE>_LINKER` (both
  `<ndk>/toolchains/llvm/prebuilt/linux-x86_64/bin/<triple>33-clang`) and
  `AR_<triple_with_underscores>` (`.../llvm-ar`), then `exec "$@"`. The comment says why API 33:
  it is the app's minimum, so the `.so` never calls libc functions newer than the oldest supported
  device. It also says why the `CC_`/`AR_` variables are needed: the `cc` crate compiles C for
  `ring` and the bundled SQLite.
- The script refuses a missing `<ndk dir>` with a clear message. It must not fall back to a guessed
  path.
- Test first: there is no unit test for a fifteen-line env script. Its proof is step 2's Android
  build and clippy, which cannot run without it (`cc` finds no compiler for the target). On its
  own, before step 2, an Android build still fails, in `libsqlite3-sys`'s SQLCipher build script
  (verified on today's `main`). That failure is step 2's to fix, not this script's.
- Run: `clients/android/with-ndk.sh /nonexistent true` must exit non-zero with the message.
  `clients/android/with-ndk.sh "$HOME/Android/Sdk/ndk/30.0.16248370" env | grep -i android` must
  show the six variables pointing at existing files.
- Docs made wrong: none yet (the README comes in step 15).

**Step 2: core stores plain SQLite on Android.** *brook-core-implementer.*
- `core/Cargo.toml`: move `rusqlite` out of `[dependencies]` into
  `[target.'cfg(not(target_os = "android"))'.dependencies]` with `features = ["bundled-sqlcipher"]`
  and `[target.'cfg(target_os = "android")'.dependencies]` with `features = ["bundled"]`, both
  `version = "0.37"`. Resolver 2 (already set in the root `Cargo.toml`) keeps the two feature sets
  apart. Rewrite the `rusqlite` comment to say both choices and point to ADR 0001. `Cargo.lock`
  must not change (`--locked`).
- `core/src/store.rs`:
  - Add a crate-private `enum Protection { Keyed, Plain }`. `Keyed` is
    `#[cfg(not(target_os = "android"))]`. `Plain` is `#[cfg(any(target_os = "android", test))]`, so
    host tests can drive it. A private `const PROTECTION` is `Keyed` off Android and `Plain` on
    Android. This `cfg` pair is the only place the platform is chosen.
  - `open`, `rebuild` and `reset` keep their signatures and pass `PROTECTION` on. Add
    `pub(crate) fn open_as(protection, ...)` and `reset_as(protection, ...)` for the tests.
    `open_reserved` and `open_inner` take the `Protection` too.
  - `open_inner` matches on it. `Keyed` is today's body, moved unchanged into `open_keyed`. `Plain`
    calls `open_plain(kind, paths)`, which works like this:
    - if the database exists, it connects without a key: `Ready` if that works, `Damaged`
      otherwise, and nothing is deleted;
    - if not, it connects and creates the schema;
    - it never touches `KeyStore` and never writes the `.check` file;
    - on Android, `store_id` and `keys` are unused (`let _ = (store_id, keys);`, with a comment).
  - Split `connect`. `connect_keyed` keeps the three SQLCipher pragmas. `connect_plain` only opens.
    Both end in a shared `finish_connect` (`temp_store`, `foreign_keys`, the WAL check). Split
    `create` so the schema and `meta` insert (`create_schema`) are shared, and only the keyed path
    writes the check.
  - `reset_as(Plain)` deletes the files (`remove_all`, which already covers `-wal`, `-shm`,
    `-journal` and `.check`) and returns `Ok(true)`. There is no key, so the "keys gone" condition
    that `LocalData::erase_row` waits for is met, and the index row goes.
  - Gate the plain-only items, `open_plain` and `connect_plain`, with
    `#[cfg(any(target_os = "android", test))]` (like `Protection::Plain`), so a host non-test build
    has no dead code. `open_as` and `reset_as` need no gate, because `open` and `reset` call them.
  - Gate every keyed-only item with `#[cfg(not(target_os = "android"))]`: `CHECK_LABEL`,
    `key_check`, `write_atomically`, `Kind::slot`, the `zeroize`, `KeySlotError` and
    `std::io::Write` imports (`Write` is used only by `write_atomically`), and
    `Opened::Locked` with its `Debug` arm. Gating `Locked` is how the spec's "a store never reports
    `Locked` because of a key it does not use" holds by construction: on Android the variant does
    not exist. `Rebuilt::KeyMissing` stays, because `local.rs` constructs it in a comparison; on
    Android it is simply never produced.
  - Rewrite the module doc: on Android the store is plain SQLite in OS-protected storage
    (ADR 0001), with no key and no check file; elsewhere, unchanged.
- Tests to write first, in `core/src/store_tests.rs`, all through `open_as`/`reset_as(Plain, ...)`
  so they run on the host:
  1. `a_plain_store_needs_no_key_and_keeps_its_data`: the store opens with an `InMemoryKeySlot`
     that fails every call (`fail_next` on `load` and `create`), so a plain open that touched the
     slot would come back `Locked` or fail. The test writes rows, closes and reopens
     (`rebuilt == None`, rows kept). It checks that no slot was made (`!slot.contains("cache:s1")`),
     that no `cache.check` exists, and that a bare
     `rusqlite::Connection::open` with **no** `PRAGMA key` reads the rows. That last check is the
     proof that no key pragma was applied: on the host the library is SQLCipher, which encrypts
     whenever a key is set.
  2. `a_plain_reset_deletes_every_file_and_destroys_no_key`: with a slot whose `delete` is set to
     fail, `reset_as(Plain)` still returns `true`. The `.db`, `-wal` and `-shm` files are gone and
     the slot is untouched.
  3. `a_damaged_plain_store_is_kept`: garble page 1, so the open reports `Damaged` and the bytes are
     unchanged.
  4. `a_plain_cache_in_another_format_rebuilds`: the format path is shared code; this proves it
     still runs under `Plain`.
  Break-and-watch: make `open_plain` call `connect_keyed` with a fixed key, and test 1 must go red.
- Existing tests: gate with `#[cfg(not(target_os = "android"))]` every test that relies on a key,
  the `.check` file or ciphertext on disk, with a one-line comment ("keyed stores only; Android
  stores are plain, ADR 0001").
  - **The 11 the emulator run names today** (run twice, the same list):
    - in `store_tests`: `nothing_under_the_store_is_plaintext`,
      `a_store_opens_only_with_its_own_key`,
      `a_copied_store_is_unreadable_once_its_slot_is_destroyed`,
      `a_missing_key_rebuilds_and_says_so`, `a_lost_slot_and_a_lost_check_rebuild`,
      `store_ids_are_random_and_stable` (its plaintext assertion),
      `a_crash_mid_transaction_leaves_only_ciphertext_and_committed_rows`;
    - in `local_tests`: `a_lost_index_key_orphans_every_store`, `a_lost_outbox_is_reported`,
      `orphaned_empty_outboxes_are_not_a_loss`;
    - in `offline_tests`: `client::a_loss_found_while_enabling_is_kept_for_the_app`. Confirmed: it
      overwrites the `index` slot with another key and expects the loss to be reported. A plain
      index has no key to lose.
  - **Also gated, because they cannot compile or hold under `Plain` once step 2 lands:**
    `an_unreadable_key_deletes_nothing` (it names `Opened::Locked`), and the tests that remove,
    read or chmod `*.check` files (`a_lost_check_with_the_right_key_keeps_the_store`,
    `an_unreadable_check_deletes_nothing`, `a_damaged_store_without_its_check_is_kept`, and the
    `.check` mode assertion in `only_the_owner_can_enter_the_store_directory`). For the last one,
    move the `.check` assertion into a keyed-only test of its own and keep the directory-mode
    assertion running everywhere.
  - **`snapshot_tests::only_a_regular_file_is_copied`** is gated `#[cfg(not(target_os =
    "android"))]` with this comment: "the adb-shell test harness cannot make a FIFO: SELinux denies
    `mkfifo` to the shell domain in `/data/local/tmp` (verified 2026-10-08); the host run covers
    this code". It is a limit of the harness, not of core, and the snapshot code is the same on
    every platform. Fixing the test to make its FIFO another way would not help: the denial is on
    FIFO creation itself.
  This is compile-time selection of what applies to the platform, not a runtime skip (CLAUDE.md
  §3). Nothing is gated "just in case": every gate either names a key, a check file or ciphertext,
  or is the FIFO case above.
- Commands:
  - the Rust gate;
  - `cargo test --locked -p brook-core --lib store_tests`;
  - the Android clippy command;
  - the Android build: `clients/android/with-ndk.sh "$HOME/Android/Sdk/ndk/30.0.16248370" cargo build --locked -p brook-ffi --release --target aarch64-linux-android --target x86_64-linux-android`.
- **Emulator check of the real Android branch**, run by hand on this machine, output pasted in the
  PR:

  ```sh
  export ANDROID_AVD_HOME="$HOME/.android/avd" TMPDIR="$HOME/.cache/emu-tmp"; mkdir -p "$TMPDIR"
  echo no | ~/Android/Sdk/cmdline-tools/latest/bin/avdmanager create avd -n brook33 \
    -k "system-images;android-33;google_apis;x86_64"            # once
  ~/Android/Sdk/emulator/emulator -avd brook33 -no-window -no-audio -no-snapshot &
  ~/Android/Sdk/platform-tools/adb wait-for-device
  clients/android/with-ndk.sh "$HOME/Android/Sdk/ndk/30.0.16248370" \
    cargo test --locked -p brook-core --lib --target x86_64-linux-android --no-run
  adb push target/x86_64-linux-android/debug/deps/brook_core-<hash> /data/local/tmp/core-tests
  adb shell 'cd /data/local/tmp && TMPDIR=/data/local/tmp ./core-tests'
  ```

  `TMPDIR` points under `$HOME` because the emulator's temporary files filled this machine's `/tmp`
  quota during planning. Use the API 33 image: the API 37 one did not stay up headless here.
  **Expected:** `test result: ok`, 0 failed, for the whole `--lib`. Before step 2 it is 514 passed
  and 12 failed; after step 2, the gated tests drop out of the count and the four new plain tests
  are in it. Any other failure is a finding to report, not a test to gate.
- Docs made wrong: the ADR and the records in step 4.

**Step 3: CI runs the Android branch of core.** *Main agent.*
- New `.github/workflows/android.yml`, job `core-android`, on `ubuntu-24.04`:
  - triggers: `push` to `main` and `pull_request`, paths `core/**`, `bindings/apple/**`,
    `clients/android/**`, `Cargo.toml`, `Cargo.lock`, `.github/workflows/android.yml`;
  - `permissions: contents: read`; actions pinned by commit SHA (CLAUDE.md §9; `rust.yml` predates
    that rule).
- Its steps:
  1. `actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0` (as in
     `release-gtk.yml`);
  2. `dtolnay/rust-toolchain@6bed0761d98439e5a578e2877258200ad565ba87 # stable branch` (as in
     `release-gtk.yml`), with `components: clippy` and
     `targets: aarch64-linux-android,x86_64-linux-android`;
  3. `Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2` (as in
     `release-gtk.yml`);
  4. `sudo apt-get update && sudo apt-get install -y libssl-dev`, as `rust.yml` installs it. The
     host `cargo test` builds SQLCipher, which on Linux compiles against the system OpenSSL
     headers;
  5. `"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --install "ndk;30.0.16248370"` (licences
     accepted with `yes |`);
  6. the Android clippy command with `"$ANDROID_HOME/ndk/30.0.16248370"`;
  7. `cargo test --locked -p brook-core --lib store_tests` (the plain-path tests, owned by this job
     so they cannot drop out unseen).
- Test first: open the PR with the job and watch it pass. Then push a throwaway commit that removes
  one `#[cfg(not(target_os = "android"))]` from a keyed-only item, watch the job go red, and drop
  the commit. Paste both run links.
- Docs made wrong, **main-agent edits in this PR** (CLAUDE.md is main-agent territory):
  - CLAUDE.md §3's table gains a row for Android core: the Android clippy command and
    `cargo test --locked -p brook-core --lib store_tests`;
  - CLAUDE.md §7's example ("owner decision, not built yet") becomes a statement of fact
    pointing to ADR 0001.

**Step 4: the written record.** *brook-docs-writer.*
- New `docs/adr/0001-plain-sqlite-on-android.md` (the folder is new). It covers:
  - context: SQLCipher needs OpenSSL, which the NDK lacks, and Android's sandbox plus file-based
    encryption;
  - the decision, and that it is gated on `target_os = "android"` only;
  - why desktops keep SQLCipher (no per-app sandbox) and why iOS is decided in #272;
  - the consequences the owner confirmed, from spec section 4: protection is the sandbox plus FBE,
    no wrong-key detection, file keys sit next to their ciphertext, the sign-out wipe is file
    deletion and not a crypto-erase, no `Locked` from an unused key;
  - the requirements: credential-encrypted storage, out of every backup.
- Android notes in `2026-09-25-offline-cache-design.md` §3 and `2026-09-26-keep-offline-spec.md`
  (line 6 and the chunk sealing around lines 105-115).
- `docs/FEATURES.md` line 39 and `docs/ROADMAP.md` line 18 ("encrypted", except on Android), and
  "What lives in `core`" in `docs/CLIENT_PHILOSOPHY.md`.
- `2026-09-25-client-local-data-encryption-design.md` exists only on the `docs/client-local-encryption`
  branch (open PR #46). The main agent has the docs writer add the Android note as a commit on that
  branch; it is not copied to `main`.
- Run: `pre-commit run --files <changed docs>` (markdownlint).

### PR 2: `ffi: tokens no longer cross the FFI` (refs #273)

Reviews: `brook-reviewer`, the Apple/core agent, and the `auth-reviewer` through the server agent
(steps 6 and 7 are the auth points). It merges independently of PR 1. When it merges, the main
agent tells the Apple/core agent, because their iOS steps 5-7 rebase over it. Implementers: core (step 5,
the Rust half of step 6), Apple (the Swift half of step 6), server (step 7), docs (step 8).

**Step 5: the token check moves to core.** *brook-core-implementer.*
- `core/src/client.rs`, test `login_success_resolves_user_and_state`. Use distinct sentinels
  (`"access-sentinel-A"`, `"refresh-sentinel-R"`, as the FFI test did) and assert **both**
  `session.access_token` and `session.refresh_token`. Today it checks only the access token, and
  with `"a"`/`"r"`. This is the check the spec moves out of `brook-ffi`: the tokens arrive intact
  and unswapped.
- Test first: it is the test. Break-and-watch: swap the two fields in core's login mapping and
  watch it go red.
- Run: `cargo test --locked -p brook-core --lib client::tests::login_success_resolves_user_and_state`.
- Docs: none.

**Step 6: `FfiSession` carries only the user. Rust and Swift land in ONE commit.** The Rust change
alone breaks the Mac build (Swift reads the removed fields), so there must be no commit in between.
*brook-core-implementer* writes the Rust part and hands it over uncommitted (or on a scratch branch
that is squashed). *brook-apple-implementer* adds the Swift part on the Mac. The committer makes one
commit of both, and the PR is tested as a whole there.
- **Rust** (core implementer):
  - `bindings/apple/src/types.rs`: `FfiSession` becomes `{ user: FfiUser }` with
    `#[derive(Debug, Clone, uniffi::Record)]`. The hand-written redacting `Debug` goes, since there
    is nothing left to redact. The doc comment says why the tokens are absent: core keeps the
    session itself through `KeySlot`, and Kotlin's generated `toString` cannot be redacted.
    `From<Session>` maps only the user. The record keeps its name and its `user` field, so every
    Swift `.loggedIn(session:)` call site and `session.user` read stays as it is.
  - `bindings/apple/src/client.rs`: rename `login_success_carries_exact_tokens_and_user` to
    `login_success_carries_the_user` and keep only the user assertion. The token check now lives
    in step 5.
  - Run: the Rust gate.
- **Swift** (Apple implementer, on the Mac):
  - `Redaction.swift`: delete the `FfiSession` extension. Keep the `LoginResult` extension: it
    still keeps the challenge out of descriptions, and its `.loggedIn` case now renders the user.
    Update the file's opening comment.
  - `RedactionTests.swift`:
    - `session` is built as `FfiSession(user: ...)`;
    - `testSessionNeverRendersTokens` and `testLoginResultNeverRendersTokens` go, since no tokens
      are left to render;
    - keep `testRedactedRenderingStillIdentifiesTheUser` and the second-factor test;
    - no TOTP-challenge rendering test is added: `FfiTotpChallenge` is an object with no fields in
      Swift, so its rendering has nothing to leak, and a test that cannot fail adds nothing.
  - `LoginIntegrationTests.testLoginReturnsWorkingTokensAndEndsLoggedIn`: rename it to
    `testLoginWorksAgainstTheServerAndEndsLoggedIn`. Replace the token asserts and the raw
    `/auth/me` request with `try await client.me()`, which must return `cfg.handle`. This proves
    the client's own access token works, without exposing it. The suite count stays 2 (`itest.sh`
    enforces it).
  - `SignOutIntegrationTests`: the client's own refresh token is still the one whose revocation is
    tested. Get it without the FFI:
    - give the client an in-test, in-memory `FfiKeySlot` with `enablePersistence(slot:dataDir:)`
      (a temp directory);
    - after login, read the refresh token from the JSON core stored in the slot. Take the one slot
      whose name starts with `session:`, and fail if there is not exactly one, rather than
      rebuilding the name from the origin string. The `{user, refresh_token}` format is in spec
      section 4 and `persist.rs`;
    - then `logout()` and probe `/auth/refresh` with it, as today.
    - Count stays 1.
  - `TotpIntegrationTests.testTwoFactorSignInEndToEnd` lines 178 and 182: device B's raw tokens
    remain. For A, give client A an in-memory slot with `enablePersistence(slot:dataDir:)` **before**
    its `login` (core writes only through a slot that is already set), and read A's refresh token
    from it before activation. Drop only
    the "A's old **access** token" probe: A's access token is never visible now. Step 7 moves that
    exact check into the server's own tests, so no coverage is lost. Count stays 1.
  - `clients/macos/BrookTests/FakeClient.swift:245`: `FfiSession(user: alice)`.
- Test first: the build itself. Any reader of the removed fields, in Rust or Swift, fails to
  compile.
- Run, on the Mac, on the combined tree:
  - the Rust gate;
  - `bindings/apple/build-xcframework.sh` (regenerates the Swift);
  - `bindings/apple/itest.sh` (the full `swift test`, `RedactionTests` included, against the test
    server);
  - `clients/macos/build.sh test`.
  Paste all summaries in the PR.
  - iOS is unaffected and needs no run: neither `clients/ios` nor `SmokeTests.swift` reads
    `FfiSession`'s token fields (checked with grep, 2026-10-09). `FfiSession(user:)` still compiles
    wherever only `user` is read.
- Docs made wrong: `2026-09-24-apple-ffi-bridge-design.md` lines 36-37 and 100-101 (step 8).

**Step 7: the server test covers what the Swift TOTP test gave up.** *brook-server-implementer*
(the server agent's machine). **Flagged for the `auth-reviewer`.**
- `services/api/tests/test_totp_flow.py::test_activation_signs_out_other_sessions` (line 212)
  today checks only the *other* device's pair (`other`) after `_enable`. Add asserts that the
  activating device's own pre-activation pair (`pair["access_token"]` on `GET /auth/me` and
  `pair["refresh_token"]` on `POST /auth/refresh`) both get 401, while
  `activated["access_token"]` still gets 200. That is what the dropped Swift probe checked
  end to end.
- Test first: it is the test. If it fails, the server does not cut off the activating device's old
  pair, which contradicts what the Swift test has asserted until now. Stop and report it to the
  main agent and the owner; do not change the server in this PR.
- Run, from `services/api`: `uv run pytest tests/test_totp_flow.py::test_activation_signs_out_other_sessions`,
  then the full `services/api` row of CLAUDE.md §3.
- Docs: PROTOCOL §1.2 says activation "signs out every **other** session; commit its pair like
  `/auth/password`'s", which leaves the activating device's previous pair unstated. Once the test
  passes, brook-docs-writer adds that the old pair stops working too (step 8).

**Step 8: the bridge design.** *brook-docs-writer.* `2026-09-24-apple-ffi-bridge-design.md` lines
36-37 and 100-101: `FfiSession` no longer carries tokens (why: core persists through `KeySlot`;
Kotlin cannot redact). PROTOCOL §1.2's activation line, if step 7's test passes: the activating
device's previous pair stops working too. Run `pre-commit run --files` on both.

### PR 3: `android: app skeleton signs in and lists channels` (closes #273)

Lands after PR 1 (it needs core to build for Android) and PR 2 (the app must never hold a record
whose `toString` prints tokens). Its steps can be written in parallel and rebased.

**Step 9: Kotlin bindings that compile.** *brook-core-implementer* (the Apple/core agent reviews
this commit).
- `bindings/apple/Cargo.toml`: a second `[[bin]]`, `uniffi-bindgen`, path
  `src/bin/uniffi-bindgen.rs`, `required-features = ["cli"]`, beside `uniffi-bindgen-swift`. The
  new file calls `uniffi::uniffi_bindgen_main()` and says it is the Kotlin generator, from the same
  pinned UniFFI. With `required-features` it stays out of default builds, as the Swift one does.
- New `bindings/apple/uniffi.toml` with only a `[bindings.kotlin.rename]` table:
  `"LoginError.Network.message"`, `"LoginError.InvalidServerUrl.message"`,
  `"LoginError.Api.message"` and `"FfiEngineError.Failed.message"` become `"detail"`, and
  `"FfiMediaEngine.close"` becomes `"closeEngine"`.
  - A comment says why: Kotlin errors extend `Throwable`, whose `message` they would hide, and
    every generated object implements `AutoCloseable.close()`.
  - UniFFI renames only the Kotlin names, never the FFI symbols, and Swift reads no
    `[bindings.kotlin]` key. So nothing changes for Swift or Rust.
  - **How the file is found:** library mode runs `cargo metadata` and reads
    `{crate_root}/uniffi.toml` for each crate (`uniffi_bindgen-0.32.2/src/cargo_metadata.rs`). So
    the generator must run inside the workspace (repo root). Do **not** pass this file as
    `--config`: in 0.32.2 that flag takes a *global* file (`[defaults]`, `[crates.<name>]`) and
    ignores a flat per-crate file with an "old-style --config" warning (`global_config.rs`).
- Test first: grep the generated file for `closeEngine` and `val detail`. Without `uniffi.toml`
  both are absent; with it, both are present. The real proof, that the Kotlin compiles, is step
  10's build, where the break-and-watch of the renames lives.
- Run, from the repo root:
  - the Rust gate;
  - build the arm64 library the generator reads: `clients/android/with-ndk.sh "$HOME/Android/Sdk/ndk/30.0.16248370" cargo build --locked -p brook-ffi --release --target aarch64-linux-android`;
  - `cargo run --locked -p brook-ffi --features cli --bin uniffi-bindgen -- generate --no-format --language kotlin --library target/aarch64-linux-android/release/libbrook_ffi.so --out-dir "$HOME/.cache/brook-kt"`;
  - `grep -c 'closeEngine' "$HOME/.cache/brook-kt/uniffi/brook_ffi/brook_ffi.kt"` must not be 0.
  In the app build, step 10's `RustLibs` task builds that library before `KotlinBindings` reads it.
- Docs: `bindings/apple` has no README, so the generator is documented in
  `clients/android/README.md` (step 15) and in the bin's own doc comment.

**Step 10: the Gradle project builds an APK that calls core.** *brook-android-implementer.*
- Before starting: the local JDK 21 from owner decision 4 must be in place. With no Gradle on the machine, make the
  wrapper once from the official `gradle-9.8.1-bin.zip`, checking it against its published SHA-256
  `dce76f55f8e251a3a1f130eb120f30b3d271de2b76c9b0729d316b5a1b6dc01f`:
  `gradle wrapper --gradle-version 9.8.1 --distribution-type bin --gradle-distribution-sha256-sum <that sum>`.
- Files in `clients/android/`:
  - `settings.gradle.kts`: `google()` and `mavenCentral()` repositories,
    `RepositoriesMode.FAIL_ON_PROJECT_REPOS`, `include(":app")`;
  - `build.gradle.kts`: `com.android.application` 9.4.1 and `org.jetbrains.kotlin.plugin.compose`
    2.4.21, both `apply false`;
  - `gradle.properties`: `org.gradle.jvmargs=-Xmx4g`, `android.useAndroidX=true`;
  - `gradlew`, `gradlew.bat` and `gradle/wrapper/*`, as generated. `gradlew` and the jar are
    Gradle's own files, so they get no SPDX header; say so in the PR.
- `app/build.gradle.kts`:
  - **AGP 9's built-in Kotlin.** Do not apply `org.jetbrains.kotlin.android`. The Compose plugin
    lifts Kotlin to 2.4.21 (verified).
  - `namespace` and `applicationId` = `me.madalin.brook`; `compileSdk = 37`; `targetSdk = 37` (owner decision 1); `minSdk = 33`; `buildToolsVersion = "37.0.0"`;
    `ndkVersion = "30.0.16248370"`, so AGP strips the `.so` with the installed NDK;
    `buildFeatures { compose = true; buildConfig = true }`.
  - `lint { warningsAsErrors = true; abortOnError = true }` and
    `kotlin { jvmToolchain(21); compilerOptions { allWarningsAsErrors.set(true) } }`.
  - Two task classes, `RustLibs` and `KotlinBindings`, the shape verified in section 0:
    - `RustLibs` runs `with-ndk.sh <ndk> cargo build --locked --release -p brook-ffi --target aarch64-linux-android --target x86_64-linux-android`
      from the repo root (`rootProject.projectDir.resolve("../..")`). It copies
      `<target dir>/<triple>/release/libbrook_ffi.so` into `<out>/arm64-v8a/` and `<out>/x86_64/`,
      where `<target dir>` is `$CARGO_TARGET_DIR` or `<repo>/target`. It is never up to date
      (`outputs.upToDateWhen { false }`), because cargo decides what is stale and a no-op cargo
      run takes about a second.
    - `KotlinBindings`:
      - inputs: the `jniLibs` folder as `@InputDirectory`, plus `bindings/apple/uniffi.toml` and
        `bindings/apple/src/bin/uniffi-bindgen.rs` as `@InputFile`s. It reruns when the `.so`, the
        renames or the generator change, and never serves stale Kotlin after a rename edit;
      - it runs step 9's generator on the arm64 `.so`, with the repo root as working directory (so
        `cargo metadata` finds `uniffi.toml`), into its `@OutputDirectory`, after deleting the old
        output. Library mode reads the same metadata from either ABI.
  - The NDK path is `androidComponents.sdkComponents.ndkDirectory`. Both outputs go through
    `androidComponents.onVariants { it.sources.jniLibs?.addGeneratedSourceDirectory(...);
    it.sources.kotlin?.addGeneratedSourceDirectory(...) }`. Comments say why each choice was made
    (junior reader, CLAUDE.md §5).
  - Dependencies:
    - main: Compose BOM 2026.09.00 (`material3`, `ui`), `androidx.activity:activity-compose:1.13.0`,
      `net.java.dev.jna:jna:5.19.1@aar` (what UniFFI's Kotlin calls through; the `@aar` carries
      `libjnidispatch.so` per ABI) and `org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0`
      (UniFFI's async functions are `suspend` functions);
    - tests: `junit:junit:4.13.2`, `net.java.dev.jna:jna:5.19.1` (the plain jar, so JVM tests can
      load the generated classes) and `org.jetbrains.kotlinx:kotlinx-coroutines-test:1.11.0`.
- `app/src/main/AndroidManifest.xml`: `INTERNET`, the application with `android:name=".BrookApp"`,
  and the launcher `MainActivity`. `MainActivity` shows only a Compose `Text` of
  `conversationLabel("channel", "general", emptyList(), "me", false)` for now.
- New `clients/android/.gitignore`: `.gradle/`, `local.properties`, `.kotlin/`. `build/` is
  already ignored by the root file, which this step does not touch.
- Test first: `app/src/test/.../BindingsLoadTest.kt`. It implements the generated `FfiKeySlot` in
  memory and checks that `create` on a taken slot throws `FfiKeySlotException.Exists`. That proves
  the generated API compiles and is usable from JVM tests without the native library. Step 11
  replaces this test.
- **Break-and-watch for step 9's renames** (moved here, the first place the Kotlin is compiled):
  delete the `"LoginError.Api.message"` line from `bindings/apple/uniffi.toml` and run
  `./gradlew assembleDebug`. `KotlinBindings` reruns because its input changed, and the Kotlin
  compile fails on the `message` clash. Restore the line, and it is green again.
- Run: `cd clients/android && ./gradlew assembleDebug testDebugUnitTest lintDebug`. Then on the
  API 33 emulator: `adb install -r app/build/outputs/apk/debug/app-debug.apk` and launch. The
  screen shows `#general`, which proves JNA loaded `libbrook_ffi.so`.
- Docs made wrong: `clients/android/README.md` (step 15).

**Step 11: the Keystore-backed `KeySlot`.** *brook-android-implementer.*
- New `KeystoreSlot.kt`. `class KeystoreSlot(dir: File, keys: SlotKeys) : FfiKeySlot`, plus
  `interface SlotKeys { fun existing(): SecretKey?; fun create(): SecretKey }` and
  `AndroidKeystoreKeys : SlotKeys`.
- `AndroidKeystoreKeys`: alias `brook-keyslots`. An AES-256 key with `PURPOSE_ENCRYPT or
  PURPOSE_DECRYPT`, GCM and no padding. No user authentication, no `setUnlockedDeviceRequired`,
  no StrongBox: core calls slots with no UI and needs an answer within seconds (spec section 4).
- **Files:** one per slot, `dir/<hex of the slot name's UTF-8>`, so slot names such as
  `session:https://host` are safe file names. Each holds `iv (12 bytes) || ciphertext+tag`. The IV
  comes from Keystore (`cipher.iv` after `init(ENCRYPT_MODE, key)`). The **AAD is the slot name**,
  so a file copied to another slot's name fails to decrypt instead of loading the wrong session.
- **Rules**, each with its core-contract reason in a comment:
  - `load`: no file means `null` (the one absent case). A file with no key, or one that fails to
    decrypt, is `FfiKeySlotException.Fatal(status)`, never absent: -10 no key, -11 bad tag, -12
    anything else unexpected. A `KeyStoreException`, `ProviderException` or `IOException` is
    `Unavailable`. `load` never creates a key.
  - `create`: if the file exists, `Exists`. Otherwise encrypt, write a temp file, `fsync`, rename.
  - `replace`: encrypt, write a temp file, `fsync`, then an atomic rename over the old file
    (`Files.move(..., ATOMIC_MOVE, REPLACE_EXISTING)`).
  - After either rename, `fsync` the directory (`FileChannel.open(dir.toPath(), READ).force(true)`),
    as core's `write_atomically` does, so a crash right after cannot lose the rename.
  - Both writes call `keys.existing() ?: keys.create()`: a key is made only when the alias is truly
    absent (`containsAlias` false; a transient fault throws instead). The bytes of other slots
    under a lost key stay `Fatal`, never absent, so the contract holds.
  - `delete`: delete the file; absent is fine; never touches the key.
  - Every method is `@Synchronized`. The app has one process and `BrookApp` holds the only
    instance, so create-only and replace are race-free without file locks; a comment says so.
  - No plaintext, password or token is ever logged.
- **JVM tests to write first**, `KeystoreSlotTest.kt`, with a fake `SlotKeys` (an in-memory key
  from `javax.crypto.KeyGenerator("AES")`, plus a way to "lose" it):
  - absent loads `null`;
  - create then load round-trips, and a second create is `Exists`;
  - replace overwrites, and replace on an absent slot creates it;
  - delete of an absent slot is fine;
  - a slot name with `:` and `/` works;
  - no file under `dir` contains the plaintext sentinel;
  - a file moved to another slot's name loads as `Fatal`, never `null`;
  - after the key is lost, an existing slot loads as `Fatal`, never `null`;
  - after the key is lost, a `replace` makes a new key and the new value loads, while another old
    slot still loads `Fatal`;
  - a truncated file loads as `Fatal`.
  Break-and-watch: make `load` return `null` on a decrypt error, and the two `Fatal` tests go red.
- **Device test**, `app/src/androidTest/.../KeystoreSlotDeviceTest.kt` (deps:
  `androidx.test:runner:1.7.0`, `androidx.test.ext:junit:1.3.0`, and
  `testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"`):
  - a round trip through the real Android Keystore;
  - a value survives a new `KeystoreSlot` instance;
  - deleting the alias makes an existing slot `Fatal`.
  It is run by hand on the API 33 emulator, `./gradlew connectedDebugAndroidTest`, with the result
  pasted in the PR. CI has no emulator (section 4).
- Run: `./gradlew testDebugUnitTest lintDebug`, then the device test.
- Docs: none.

**Step 12: settings and the server address.** *brook-android-implementer.*
- `ServerAddress.kt`: a port of the Mac's `ServerAddress.parse`, using `java.net.URI`. It trims,
  requires `http` or `https` and a host, and refuses user info, a query or a fragment
  (`NotJustAnAddress`); anything else is `Invalid`.
- `Settings.kt`: `class Settings(prefs: SharedPreferences, isDebugBuild: Boolean)`. It holds
  `lastGoodServer: String?` and `allowInsecureHttp`, whose getter returns `false` whenever
  `!isDebugBuild`, whatever is stored. `BrookApp` passes `BuildConfig.DEBUG`.
  - **Why this matters more on Android:** core's sockets are its own, so Android's cleartext
    policy (`usesCleartextTraffic`) never applies to them. Core's `allow_insecure_http` is the only
    gate, and this getter is what keeps release builds `https`-only (plus loopback, which core
    always allows).
- Tests first:
  - `ServerAddressTest`: ports the four Mac `ServerAddressTests`;
  - `SettingsTest`: a stored `true` reads `false` when `isDebugBuild = false`.
  Use a small in-memory `SharedPreferences` fake in the test source set; there is no Robolectric.
- Run: `./gradlew testDebugUnitTest`.
- Docs: none (the README in step 15 mentions the debug-only switch).

**Step 13: the session model (sign in, TOTP, sign out, remote sign-out, restore).**
*brook-android-implementer.*
- New `SessionModel.kt`. It ports the Mac's `SessionStore` minus local data, the second-instance
  lock and the macOS LAN wording. Constructor:
  `SessionModel(settings, slot: FfiKeySlot, dataDir: String, makeClient: (String, Boolean) -> FfiBrookClient, scope: CoroutineScope)`.
  - The factory returns the generated **class** `FfiBrookClient`, not `FfiBrookClientInterface`:
    `close()` comes from `AutoCloseable` on the class (UniFFI 0.32.2 `ObjectTemplate.kt`), and the
    model needs it.
  - Fakes subclass the class through its generated `constructor(noHandle: NoHandle)`, as the fake
    `Subscription` and `FfiTotpChallenge` do and as the Mac's `FakeClient` subclasses its client.
    The class is `open` and its `override` methods are open, so the fake overrides what it uses.
- **Persistence is always on.** Android has no unsigned-build or keychain-group case and no second
  instance. Every client gets `enablePersistence(slot, dataDir)`, where `dataDir` is
  `noBackupFilesDir/brook` (core's sign-out fences). The initial phase is `Restoring` exactly when
  `lastGoodServer` is set.
- `phase: StateFlow<Phase>`, with `Phase` one of: `Restoring`, `SignedOut(error)`, `SigningIn`,
  `NeedsCode(error)`, `SignedIn(user)`. Also `codeBusy` and `signOutWarning`.
- **Form state lives in `SignInForm.kt`**, a port of the Mac's `LoginForm`:
  - it holds `server` (prefilled from `lastGoodServer`), `handle`, `password`, `code` and
    `useRecovery`, and derives `isBusy`, `needsCode`, `error` and `insecureWarning` from the model;
  - `submit()` clears the password only once an attempt was really made; `submitCode()` clears
    the code after each attempt; `back()`;
  - the fields are Compose `mutableStateOf`, so the screens recompose. Whether Compose snapshot
    state is usable in a plain JVM unit test was not checked here; if it is not, use one
    `MutableStateFlow` per field instead and say so in the PR.
- **A new client per attempt**, as on the Mac and as `persist.rs` assumes (the newest client owns
  the slot).
- **Attempt logic ported as is:**
  - the attempt counter;
  - auth events go from the `AuthStateListener` (called on a Rust worker thread) into an unlimited
    `Channel`, numbered under one lock, as the Mac's `DeliveryCount` does;
  - one coroutine per attempt consumes them;
  - the login's completion reads `client.authState()` and settles everything numbered before it;
  - a later `LoggedOut` is a remote sign-out only if `authState()` still says so;
  - `end()` bumps the attempt, cancels the subscription, and calls `close()` on the old client
    once nothing more will call it (after the sign-out task's `logout()` and
    `signOutComplete()`). So the Rust client and its socket go now, not at GC time.
- **Messages:** the Mac's, reworded for a phone. The spec's texts for Unavailable, Offline and the
  sign-out warning ("This phone couldn't forget..."). `auth.rate_limited` gets "Too many attempts.
  Wait a moment and try again." Errors are matched on `LoginException.Api(code, detail)` (step 9's
  rename).
- **TOTP:**
  - the code must be 6 digits (anything else is refused locally); a recovery code is trimmed and
    must not be empty;
  - `auth.invalid_code` stays on the step with the message;
  - `auth.totp_expired` goes back to the password with the note;
  - Back calls `cancelTotp`;
  - `ChallengeSuperseded` changes nothing.
  The `UInt?` (recovery codes left) that `completeTotp` and `completeRecovery` return is ignored:
  there is no UI for it in this scope.
- **Restore:** `restoreAtLaunch()` runs once per process. The five outcomes map exactly as in spec
  section 4.
- The server is saved only after a successful sign-in. The password is passed exactly as typed;
  the handle and server are trimmed.
- **Tests first**, with `runTest`, a `StandardTestDispatcher` scope, `FakeClient`
  (`FfiBrookClient(NoHandle)` subclass), fake `Subscription` and `FfiTotpChallenge` (also
  `NoHandle`), and an in-memory `FfiKeySlot`. The Mac tests are ported by name: drop the `test`
  prefix and lower the first letter.
  - **`SessionModelTest.kt`**, from `SessionStoreTests` (25 of 27):
    - sign-in: `EmptyHandleOrPasswordNeverReachesTheClient`,
      `PasswordIsPassedExactlyWhileHandleAndServerAreTrimmed`,
      `AddressWithCredentialsIsRejectedBeforeAnythingHappens`,
      `SecondSignInWhileSigningInIsIgnored`, `CanRetryAfterAFailedLogin`,
      `ConstructorErrorIsShownAndLoginNeverCalled`, `ErrorMessages`, `ServerIsSavedOnlyAfterSuccess`,
      `FailedLoginDoesNotSaveTheServer`, `FactoryReceivesTheResolvedInsecureFlag`;
    - sign-out: `SignOutEndsTheSessionQuietlyAndSignsOutOfCore`,
      `ARemoteSignOutShowsTheSignInScreenWithTheMessage`, `AFreshClientsInitialLoggedOutIsNotASignOut`,
      `ASignOutBeforeTheLoginResultIsHandledWins`, `ACoalescedSignOutDuringSignInIsCaught`,
      `ALateInitialLoggedOutNeverEndsAGoodSession`, `ARemoteSignOutAfterTheUsersOwnHasNoMessage`,
      `ALoggedOutFromThePreviousClientNeverSignsOutTheNextOne`;
    - TOTP: `ThePasswordAloneLeadsToTheCodeStep`, `TheRightCodeSignsIn`,
      `AWrongCodeStaysOnTheCodeStepAndSaysSo`, `AnExpiredChallengeGoesBackToThePassword`,
      `BackCancelsTheChallenge`, `ASupersededChallengeChangesNothing`,
      `AMalformedCodeNeverReachesTheClient`.
    - **Not ported:** `NetworkErrorOnALanAddressMentionsLocalNetworkPermission` (macOS's
      permission) and `FewRecoveryCodesLeftAreFlagged` (no such UI in scope).
    - **New:** `rateLimitedHasItsOwnMessage`, and `releaseBuildNeverPassesInsecureHttp` (a factory
      spy, with `Settings(isDebugBuild = false)`).
  - **`RestoreTest.kt`**, from `RestoreTests` (13 of 18):
    - `AStoredSessionSignsInAtLaunchOnTheLastServer`, `NothingStoredShowsTheFormQuietly`,
      `OfflineSaysTheSessionIsKept`, `TheRestoreRunsOncePerProcess`,
      `ASecondCallDuringTheRestoreDoesNothing`, `ARemoteSignOutAfterARestoreIsFollowed`,
      `ASignOutThatCouldNotForgetTheStoredCopySaysSo`, `ALateSignOutResultNeverWarnsAfterANewSignIn`,
      `ASignInClearsTheSignOutWarning`, `ACompleteSignOutStaysQuiet`;
    - three adapted, under new names saying what they test now:
      - `ALockedKeychainSaysSo` becomes `anUnreadableStoredSessionSaysSo` (`Unavailable`);
      - `NoPersistenceOrNoServerNeverRestores` becomes `noLastServerNeverRestores` (persistence
        cannot be off);
      - `ASignInStoresTheSessionOnlyWhenPersistenceIsOn` becomes
        `everyClientGetsPersistenceBeforeItSignsIn`.
    - **New:** `aSupersededRestoreShowsTheFormQuietly` (the spec's fifth outcome).
    - **Not ported**, none of which exists on Android: `ASecondInstanceSaysItWontRemember`,
      `NoKeychainGroupInTheSignatureMeansOff`, `AKeychainFaultMeansOffButALockedKeychainDoesNot`,
      `TheLockIsTakenBeforeTheKeychainIsTouched`, `OnlyOneHolderOfTheInstanceLock`.
  - **`SignInFormTest.kt`**, from `LoginFormTests` (5 of 5): `PasswordClearedAfterSuccessfulLogin`,
    `PasswordClearedAfterTheServerRejectsIt`, `PasswordKeptWhenTheFormIsRejectedLocally`,
    `PrefillsTheSavedServer`, `InsecureWarningOnlyWhenOptedIn`.
  Break-and-watch: drop the `authState()` re-check in the remote sign-out path, and
  `aFreshClientsInitialLoggedOutIsNotASignOut` must go red.
- Run: `./gradlew testDebugUnitTest lintDebug`.
- Docs: none of its own; step 15's README describes sign-in, restore and sign-out as built.

**Step 14: the live channel list and the UI.** *brook-android-implementer.*
- New `ChannelListModel.kt`: `ChannelListModel(client, meId, scope)`.
  - `start()`: `subscribeEvents(listener)`, then `startRealtime()` (errors ignored, as on the Mac:
    core's reconnect loop retries), then `load()`.
  - `state: StateFlow<ListState>`, one of `Loading`, `Error`, or `Loaded(channels, dms)`.
  - **Sections:** channels (`kind == "channel"`) and DMs (`kind == "dm"`). Each is sorted by
    `sortKey(kind, name, members, meId)` (core's FFI, usernames off), and each row's text is
    `conversationLabel(..., meId, false)`. Every channel `listChannels` returns is listed, archived
    or not, with no extra marking (the spec asks for none).
  - **Events** are posted to `scope` in arrival order:
    - `Ready` and `Resync` reload;
    - `ChannelUpdate` replaces the row if the channel is listed, and otherwise reloads;
    - `ChannelDelete` removes it;
    - anything else is ignored.
  - `stop()` cancels the subscription.
  - `SessionModel` makes one on `SignedIn` and stops it in `end()`.
- Tests first, `ChannelListModelTest.kt`, with the fake client:
  - two alphabetical sections, and a section with no rows is not shown;
  - an update replaces a row in place;
  - an update for an unlisted channel reloads the list;
  - a delete removes the row;
  - `Ready` and `Resync` reload;
  - a failed load is `Error`, and retrying loads;
  - the events subscription is made **before** `startRealtime` (order recorded by the fake);
  - stopping cancels the subscription.
  Port the matching cases of the Mac's `ChannelEventsTests` by name where they apply. Break-and-
  watch: make an unlisted update a no-op, and its test goes red.
- **UI**: `MainActivity.kt`, `ui/Theme.kt`, `ui/SignInScreen.kt`, `ui/ChannelListScreen.kt`.
  - `enableEdgeToEdge()`; Material 3 with `dynamicLightColorScheme`/`dynamicDarkColorScheme`
    following `isSystemInDarkTheme()`; the screen is a `when (phase)`.
  - **Sign in:** server, handle and password fields. The password uses
    `PasswordVisualTransformation` and `KeyboardType.Password`; the fields carry
    `ContentType.Username` / `ContentType.Password` semantics for autofill. There is a "Sign in"
    button that shows progress and is disabled while busy, and the error under the form.
  - **Debug builds only:** an "Allow insecure http (dev)" switch with the warning text.
  - **The code step:** a number field with `ContentType("2faAppOTPCode")` (Compose's
    `ContentType(String)` factory with the value of `androidx.autofill`'s
    `HintConstants.AUTOFILL_HINT_2FA_APP_OTP`, both verified). That is the hint for an
    authenticator app's code. `SmsOtpCode` would be wrong, since Brook sends no SMS, and no
    `androidx.autofill` dependency is needed for one string; the comment names its origin. In
    recovery mode, a plain text field with no hint. "Use a recovery code instead", and Back, which
    is also the system back through `BackHandler`.
  - **The list:** a top bar with "Brook" and an account menu (display name, "Sign out"); the
    two-section `LazyColumn`; round letter avatars whose color comes from the theme's container
    roles, picked by `id.hashCode()`, which is stable for a `String` across runs. Rows are not
    clickable. "No conversations yet" when empty; the error state has "Try again".
- **The manifest:**
  - `android:allowBackup="false"` and `android:dataExtractionRules="@xml/data_extraction_rules"`,
    excluding every domain from cloud backup and device transfer. The app is not
    `directBootAware`, so `noBackupFilesDir` and `filesDir` are credential-encrypted storage, as the
    spec requires.
  - `android:enableOnBackInvokedCallback="true"` (predictive back); `android:label="Brook"`.
  - An adaptive icon (`mipmap-anydpi/ic_launcher.xml`) with a monochrome layer for themed icons,
    drawn from the GNOME placeholder mark (`clients/gnome/data/icons/dev.brook.Brook.svg`).
- `BrookApp.kt`:
  - builds `Settings`, `KeystoreSlot(File(noBackupFilesDir, "keyslots"), AndroidKeystoreKeys())`
    and `SessionModel` with `MainScope()`;
  - starts `restoreAtLaunch()`.
  The comment says why the model lives here and not in the Activity (rotation, one restore per
  process).
- Test first: the model tests above. There are no Compose UI tests in this step (owner decision 3).
- Run: `./gradlew assembleDebug testDebugUnitTest lintDebug`, then the manual check below.
- Docs: none of its own; step 15's README covers the screens, the dev http switch and the backup
  exclusion.

**Step 15: CI builds the APK, and the README.** Main agent (CI), then brook-docs-writer.
- `android.yml` gains a job `app` (`needs: core-android`), with these steps:
  1. `actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0`;
  2. `actions/setup-java@de7274f081f381c8f8158605e0321c36c376e2e6 # v6.0.1`, with
     `distribution: temurin`, `java-version: 21`, `cache: gradle`. This action is new to the repo;
     the pin is the commit of release `v6.0.1`, looked up with
     `gh api repos/actions/setup-java/git/ref/tags/v6.0.1` on 2026-10-08;
  3. `dtolnay/rust-toolchain@6bed0761d98439e5a578e2877258200ad565ba87 # stable branch`, with
     `targets: aarch64-linux-android,x86_64-linux-android`;
  4. `Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2`;
  5. `sudo apt-get update && sudo apt-get install -y libssl-dev`, because the host build of the
     generator links core, which links SQLCipher, which needs OpenSSL headers on Linux;
  6. `sdkmanager --install "platforms;android-37.0" "build-tools;37.0.0" "ndk;30.0.16248370"`;
  7. `gradle/actions/wrapper-validation@3f5f9adaf7d9fecd50b5935e54106014257a94e6 # v6.4.0`,
     which checks the committed `gradle-wrapper.jar` against Gradle's published checksums, since a
     binary is in the repo. Also new: the `v6.4.0` tag is annotated (`b9bee63e...`), and this SHA
     is the commit it points to (`gh api repos/gradle/actions/git/tags/b9bee63e...`);
  8. `cd clients/android && ./gradlew --no-daemon assembleDebug testDebugUnitTest lintDebug`.
  Pins 1, 3 and 4 are copied from `release-gtk.yml`. No APK artifact is uploaded: the spec asks CI
  to build it, not to publish it.
- Test first: the job goes green on PR 3. Push a throwaway commit that removes one Kotlin rename
  from `uniffi.toml`, watch `app` go red, drop it. Paste both links.
- **Docs, by brook-docs-writer:**
  - `clients/android/README.md`: the JDK 21, the SDK packages, the NDK, the Rust targets, what
    `./gradlew assembleDebug` does (cargo and bindgen included), installing on the emulator
    (`10.0.2.2` reaches the host, with the debug-only http switch) and on a phone, and the manual
    test commands from steps 2 and 11.
  - `docs/QUALITY.md`: the Android line states what runs (see owner decisions 2 and 3).
  - The Kotlin generator is documented in `clients/android/README.md` (what runs it, and that
    `bindings/apple/uniffi.toml` holds the Kotlin-only renames and why). `bindings/apple` has no
    README.
  - **Main-agent edit:** CLAUDE.md §3's table gains an Android row:
    `cd clients/android && ./gradlew assembleDebug testDebugUnitTest lintDebug`.
  - `docs/user-guide.md` line 15 stays as it is: Android is still not distributed (debug APK only).
- After the merge (CLAUDE.md §7): the Android skeleton adds nothing another client lacks, so no
  per-client issues; the main agent says so on the merged PR.

**Manual check for PR 3, results pasted in the PR** (spec "Done means" 1-5). Dev credentials come
from the main agent and never go into the repo or the PR text.
1. From the API 33 emulator, debug build with the http switch on, against the test server on the VM
   through `10.0.2.2` or its LAN address: sign in, and the channels and DMs match what the Mac or
   GNOME client shows for the same account.
2. An account with TOTP: code step, a wrong code, a recovery code, Back.
3. `adb shell am force-stop me.madalin.brook`, then reopen: it lands on the list with no form.
4. Sign out, then force-stop and reopen: the sign-in form, with the server prefilled.
5. With the list open, create, rename and leave a channel from GNOME or the Mac: the list follows
   without a refresh.
6. On a phone with the arm64 APK, against `https://chat.madalin.me`: steps 1, 3 and 4.
7. `adb shell run-as me.madalin.brook ls -R no_backup files shared_prefs`: the session is only in
   `no_backup/keyslots/` as ciphertext, and `grep -r` for the handle in those files finds nothing.

## 3. Migrations and production

- No server or wire change, and no database migration.
- Android has no installed base, so there is no local data to carry over. Before this, core did not
  build for Android at all.
- For macOS, iOS and Linux, `store.rs` is a behaviour-preserving refactor (`Keyed` is today's body,
  split into functions; the existing store tests guard it). Their
  stored session format (`{user, refresh_token}` JSON) is unchanged.
- PR 2 changes the FFI record. The Mac app and its xcframework are built together, so no old binary
  ever reads the new record.
- **Rollback:** revert the PR.
  - PR 1's revert makes Android unbuildable again but changes nothing elsewhere.
  - PR 2's Rust change (`types.rs`) and Swift change (`Redaction.swift`, the tests,
    `FakeClient.swift`) must go together. They are one commit (step 6), so reverting that commit
    restores both.
  - Step 7's server test is independent and stays.
  - PR 3 cannot outlive PR 1: it needs core to build for Android.

## 4. Risks (what the tests would not catch, and how it is checked)

1. **Local network access on Android 17 with `targetSdk = 37`.** Unverified (section 0). If LAN
   addresses need `ACCESS_LOCAL_NETWORK`, sign-in to the test VM or a home server fails with a
   network error. Checked by manual steps 1 and 6 on a phone running Android 17, if one is at hand.
   If it fails, that is a spec change (a permission prompt) for the owner, not something to patch
   in the PR.
2. **The `Protection` selection line itself** (`const PROTECTION` on Android) is not run by host
   tests. Checked by the Android clippy in CI (compiles the gated code) and the emulator run of
   the whole `--lib` in step 2 (runs it).
3. **Keystore behaviour on real devices** (an OEM losing keys after an update or a lock-screen
   change) shows as `Fatal`, so restore says "couldn't be read" and the user signs in again. The
   device test covers only the emulator; manual step 6 covers one phone.
4. **CI has no emulator**, so neither the real Keystore nor the Android-built core tests run in
   CI. Adding an emulator job (KVM, a third-party runner action, minutes per run) is left out on
   purpose. The hand runs are pasted in PRs 1 and 3.
5. **No Compose UI tests** (owner decision 3). QUALITY.md lists them for Android. The screens are
   thin over the models, which are tested; the manual check covers the screens. A Compose test
   harness on the JVM needs Robolectric (a new dependency), and on a device the emulator that CI
   lacks.
6. **Kotlin renames.** A future FFI error with a `message` field, or a method named `close` on an
   exported object, breaks the Kotlin build. CI's `app` job catches it on that PR, which is the
   right place.
7. **The async callback trait `FfiMediaEngine`** compiles in Kotlin but is never exercised. Calls
   are out of scope; the calls ticket must test it.
8. **Release builds are not built or minified.** JNA needs keep rules under R8. This is the Play
   Store ticket's job (a non-goal here).
9. **Tooling freshness.** Gradle 9.8.1 is a day old. AGP 9.4 needs 9.6.0 or later, so 9.6.1 is the
   fallback if 9.8.1 misbehaves; it was not tried.
10. **`FfiSecondFactor.Code` / `.Recovery` print their code in Kotlin's generated `toString`**,
    the same leak that made the tokens leave `FfiSession`. This skeleton never builds one:
    `completeTotp` and `completeRecovery` take a plain `String`. The TOTP-enrolment ticket (which
    uses `totpDisable` and `totpRegenerateRecoveryCodes` with `FfiSecondFactor`) must deal with it
    before shipping, for example with an object instead of an enum. This is recorded here so that
    ticket inherits it.
11. **The `/tmp` quota on this machine.** The emulator and big builds filled it during planning.
    Keep emulator temp files and large target directories under `$HOME`.

## 5. Decisions for the owner

All four were decided by the owner on 2026-10-09; none is open.

1. **`targetSdk = 37`**, matching `compileSdk` and the installed platform.
   - **Accepted cost:** risk 1, Android 17's local network protection, which can only be verified
     on a phone running Android 17.
   - `warningsAsErrors` stays this plan's own choice, not an existing repo rule; with 37,
     `OldTargetApi` never fires.
2. **Android Lint plus Kotlin warnings-as-errors** (`lint { warningsAsErrors = true }`,
   `allWarningsAsErrors`). ktlint/detekt come later, as their own step. The docs writer states
   this on QUALITY.md's Android line (step 15).
3. **No Compose UI tests now.** They are added with the first screen that has real interaction
   logic (opening a conversation). The screens here are thin over `SessionModel`, `SignInForm` and
   `ChannelListModel`, which have JVM tests, and the PR 3 manual check covers them. QUALITY.md's
   Android line says so (step 15).
4. **JDK: a local, checksummed JDK 21 under `~/.local`, used only for Brook builds.** No system
   package; the main agent installs it before step 10.
   - **Install:**
     - Temurin 21 for linux-x64 from Adoptium's API
       (`api.adoptium.net/v3/assets/latest/21/hotspot?architecture=x64&image_type=jdk&os=linux`);
     - check the tarball's SHA-256 against the `checksum` field in the API's answer before
       unpacking;
     - unpack to `~/.local/jdk-21`.
     The same source and check were used for section 0's verification.
   - **How Gradle finds it:** two lines in the user's `~/.gradle/gradle.properties`, outside the
     repo, so no machine path is ever committed:
     - `org.gradle.java.home=/home/<user>/.local/jdk-21` makes the Gradle daemon run on it;
     - `org.gradle.java.installations.paths=/home/<user>/.local/jdk-21` makes the build's
       `jvmToolchain(21)` resolve to it.
     Section 0's verification ran with the equivalent of the second line: the toolchain lookup
     failed without it while only the JRE was present. Setting both does not depend on Gradle
     counting the daemon's JVM as a toolchain.
   - **Why not `JAVA_HOME` in `with-ndk.sh`:** that script runs only inside Gradle's cargo task
     and in CI's clippy, so it cannot choose the JVM Gradle itself starts on. It would also put a
     machine path, or path logic, in the repo.
   - CI is unaffected: `actions/setup-java` provides its own JDK 21.
   - The README (step 15) gives these two lines with a placeholder path.
