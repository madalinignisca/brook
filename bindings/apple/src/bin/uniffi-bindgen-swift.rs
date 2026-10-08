// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Swift bindings generator, built from the same pinned UniFFI as the library.

fn main() {
    uniffi::uniffi_bindgen_swift();
}
