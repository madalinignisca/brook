// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Kotlin bindings generator (the Android client), built from the same pinned UniFFI as the
//! library, so the generated Kotlin and the Rust scaffolding always come from one release.
//! The Swift one is `uniffi-bindgen-swift`. Run it inside the workspace: it finds
//! `bindings/apple/uniffi.toml` (the Kotlin renames) through `cargo metadata`.

fn main() {
    uniffi::uniffi_bindgen_main();
}
