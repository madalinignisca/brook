//! UniFFI bindings of `brook-core` for the Apple clients.
//!
//! `brook-core` stays idiomatic Rust (it is used directly by the GNOME client); every
//! FFI concern — the owned Tokio runtime, the listener callback, FFI-safe types — lives
//! here. See `docs/superpowers/specs/2026-09-24-apple-ffi-bridge-design.md`.

uniffi::setup_scaffolding!();
