// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The About Brook dialog.
//!
//! Brook is AGPL-3.0-or-later, so the people who run this client are told who holds the
//! copyright and under which license they got it, in the place GNOME users look for it.
//! Offering the *server* source (AGPL section 13) is a separate piece of work (#300) and is
//! deliberately not linked here.

use adw::prelude::*;

/// Shown as the copyright line. The year and holder match the SPDX headers in the sources.
const COPYRIGHT: &str = "© 2026 Madalin Ignisca and Brook contributors";

/// The client's own source. Not the server's: that link belongs to #300.
const WEBSITE: &str = "https://github.com/madalinignisca/brook";

/// Builds the dialog. Version comes from Cargo so it cannot drift from the binary.
pub fn dialog() -> adw::AboutDialog {
    adw::AboutDialog::builder()
        .application_name("Brook")
        .application_icon("dev.brook.Brook")
        .version(env!("CARGO_PKG_VERSION"))
        .copyright(COPYRIGHT)
        // `Agpl30` is "AGPL 3.0 or later" in GTK; `Agpl30Only` is the strict variant.
        .license_type(gtk::License::Agpl30)
        .website(WEBSITE)
        .build()
}

/// Opens the dialog over `parent`.
pub fn show(parent: &impl IsA<gtk::Widget>) {
    dialog().present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    // GTK needs a display to build widgets, which CI does not have, so this checks the
    // text the dialog is built from.
    #[test]
    fn names_the_holder_and_points_at_the_client_source() {
        assert!(COPYRIGHT.contains("Madalin Ignisca and Brook contributors"));
        assert!(COPYRIGHT.contains("2026"));
        assert_eq!(WEBSITE, "https://github.com/madalinignisca/brook");
    }
}
