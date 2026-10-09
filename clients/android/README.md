# Android client — Kotlin + Jetpack Compose (Material 3)

Native Android app over the shared Rust [`core`](../../core).

## Stack
- **Jetpack Compose** with **Material 3**, Kotlin.
- `core` via **UniFFI**-generated Kotlin bindings (JNI under the hood).
- Media: platform WebRTC + **MediaCodec** (HW H.264). See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Requirements

### Android SDK
- **Minimum Android 13** (API 33), **target Android 14** (API 37).
- Build tools: `build-tools;37.0.0`.
- Platform package: `platforms;android-37.0`.

### Toolchain
- **JDK 21**. Configure Gradle to find it by adding these lines to `~/.gradle/gradle.properties`:
  ```
  org.gradle.java.home=/path/to/jdk-21
  org.gradle.java.installations.paths=/path/to/jdk-21
  ```
- **NDK 30.0.16248370**. AGP downloads it automatically and is used for the Rust cross-compilation.

### Rust targets
- `aarch64-linux-android` (arm64 APK for phones).
- `x86_64-linux-android` (x86_64 APK for emulator).

## Build

```sh
cd clients/android && ./gradlew assembleDebug testDebugUnitTest lintDebug
```

- **`assembleDebug`**: Compiles the app and runs two Gradle custom tasks:
  - **`RustLibs`** cross-builds `brook-ffi` (the UniFFI library) via `with-ndk.sh` for both ABIs (`aarch64-linux-android` and `x86_64-linux-android`), producing two `.so` files.
  - **`KotlinBindings`** runs `uniffi-bindgen` (from the repo root) to generate the Kotlin bindings from the arm64 library.
- **`testDebugUnitTest`**: Runs JUnit unit tests on the JVM.
- **`lintDebug`**: Runs Android Lint; warnings are errors.

### UniFFI Kotlin renames
The file `bindings/apple/uniffi.toml` holds Kotlin-only field and method renames, applied when generating the Kotlin bindings. Why:
- Error classes extend `Throwable`, which already has a `message` property; a field named `message` would hide it and not compile. Renamed to `detail`.
- Every generated object implements `AutoCloseable.close()`; the media engine's own `close()` method clashes. Renamed to `closeEngine`.

## Running on the emulator

```sh
./gradlew installDebugApk  # or adb install build/outputs/apk/debug/...apk
```

- Inside the emulator, `10.0.2.2` reaches the host machine.
- Debug builds allow plain `http` to non-loopback servers (the "Allow insecure http (dev)" switch in Settings). Release builds are https-only.

## Manual testing

### Core emulator
Start the core server from [step 2 of the plan](../../docs/superpowers/specs/2026-10-08-android-skeleton-plan.md):
```sh
# From the repo root
cd core && sqlx database create && cargo run --locked
```

### Device tests
Run the Keystore integration tests on a connected emulator:
```sh
./gradlew connectedDebugAndroidTest
```

These tests verify Keystore encryption and decryption of the session key.

## Storage

- **Session key**: stored in a Keystore-protected key slot (hardware-backed on supported devices) in `no_backup/keyslots`. The session itself is encrypted with this key.
- **Local data**: kept in `no_backup/core` (offline cache and outbox). Plain SQLite on Android, protected by the app sandbox and File-Based Encryption.
- **Backup**: disabled via `allowBackup="false"` and the `data_extraction_rules.xml` policy. Neither the session nor local data are backed up to cloud or transferred to another device (where the Keystore key would not exist). See [ADR 0001](../../docs/adr/0001-plain-sqlite-on-android.md).

## Background & push (required — not optional on mobile)
Android suspends background WebSockets, so always-on WSS is not the delivery path. The app registers an **FCM** token (`POST /devices`) and relies on push to wake-and-sync messages and to present **incoming calls** (high-priority FCM → ConnectionService + foreground service). See [../../docs/PROTOCOL.md](../../docs/PROTOCOL.md) §3a.

## Native UX commitments (Material 3)
- Dynamic color / Material You, predictive back, themed app icon.
- Storage Access Framework for the file picker; system notifications; **foreground service for active calls**.
- **App Links** for OIDC redirect (custom scheme only as fallback).

## Packaging
Play Store (AAB).
