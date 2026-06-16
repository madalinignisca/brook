# Android client — Kotlin + Jetpack Compose (Material 3)

Native Android app over the shared Rust [`core`](../../core).

## Stack
- **Jetpack Compose** with **Material 3**, Kotlin.
- `core` via **UniFFI**-generated Kotlin bindings (JNI under the hood).
- Media: platform WebRTC + **MediaCodec** (HW H.264). See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Native UX commitments (Material 3)
- Dynamic color / Material You, predictive back, themed app icon.
- Storage Access Framework for the file picker; system notifications; foreground service for active calls.

## Packaging
Play Store (AAB).
