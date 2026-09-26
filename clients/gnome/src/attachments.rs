//! Attachments on messages (#66): shown with their display name, saved on request.
//!
//! The server's rules (attachments spec §4, §5) shape it:
//! - `original_name` is display text only; `filename` (sanitised ASCII) is the only name
//!   ever used on disk;
//! - the declared `content_type` is untrusted: it only picks an icon here;
//! - a file is saved where the user chooses (the save dialog opens in Downloads, a name
//!   clash gets " (1)"), and it is **never opened automatically**.
//!
//! Bytes go through core's transfer layer straight into the chosen file (`FileSink`: no
//! temporary sibling, which a Flatpak's document portal wouldn't allow); a failed or
//! cancelled save leaves nothing behind.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{BrookClient, FileInfo, FileSink, TransferId, TransferState};
use gtk::{gio, glib};
use tokio::runtime::Handle;

/// An icon for the declared type. Only cosmetic: the type is the uploader's claim.
pub fn icon_for(content_type: &str) -> &'static str {
    let kind = content_type.split('/').next().unwrap_or("");
    match kind {
        "image" => "image-x-generic-symbolic",
        "video" => "video-x-generic-symbolic",
        "audio" => "audio-x-generic-symbolic",
        "text" => "text-x-generic-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

/// Split `name` into stem and extension at the first dot after the start, so a clash on
/// `report.tar.gz` becomes `report (1).tar.gz`, not `report.tar (1).gz`.
fn split_name(name: &str) -> (&str, &str) {
    match name.char_indices().skip(1).find(|(_, c)| *c == '.') {
        Some((i, _)) => name.split_at(i),
        None => (name, ""),
    }
}

/// `name` in `dir`, or `name (1)`, `name (2)`, … if taken: never overwrite silently.
pub fn unique_name(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = split_name(name);
    (1..10_000)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|candidate| !dir.join(candidate).exists())
        .unwrap_or_else(|| name.to_string())
}

/// Running inside a Flatpak sandbox (the document portal grants single files).
fn is_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Where the bytes are written first: the chosen file itself, or, when it already exists
/// and a sibling can be created (not under Flatpak), a hidden part file beside it that is
/// renamed over it after the checksum matched.
pub fn write_target(chosen: &Path, flatpak: bool) -> PathBuf {
    if flatpak || !chosen.exists() {
        return chosen.to_path_buf();
    }
    let name = chosen
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    chosen.with_file_name(format!(".{name}.brook-part"))
}

/// Where the save dialog opens: the user's Downloads, else home.
fn downloads_dir() -> PathBuf {
    glib::user_special_dir(glib::UserDirectory::Downloads).unwrap_or_else(glib::home_dir)
}

/// A row for one attachment: icon, name, size, and Save.
pub fn attachment_row(file: &FileInfo, client: Arc<BrookClient>, runtime: Handle) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(4)
        .css_classes(["card"])
        .build();
    let icon = gtk::Image::builder()
        .icon_name(icon_for(&file.content_type))
        .margin_start(8)
        .build();
    let name = gtk::Label::builder()
        .label(&file.original_name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .tooltip_text(format!("Saved as {}", file.filename))
        .build();
    let size = gtk::Label::builder()
        .label(glib::format_size(file.size).as_str())
        .css_classes(["caption", "dim-label"])
        .build();
    let status = gtk::Label::builder()
        .css_classes(["caption", "dim-label"])
        .visible(false)
        .build();
    let progress = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .width_request(80)
        .visible(false)
        .build();
    let save = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Save…")
        .css_classes(["flat"])
        .build();
    let cancel = gtk::Button::builder()
        .icon_name("process-stop-symbolic")
        .tooltip_text("Cancel")
        .css_classes(["flat"])
        .visible(false)
        .build();
    for w in [
        icon.upcast_ref::<gtk::Widget>(),
        name.upcast_ref(),
        size.upcast_ref(),
        status.upcast_ref(),
        progress.upcast_ref(),
        save.upcast_ref(),
        cancel.upcast_ref(),
    ] {
        row.append(w);
    }

    // Not uploaded yet (or no checksum to verify against): nothing to save.
    let Some(sha256) = file.sha256.clone().filter(|_| file.status == "committed") else {
        save.set_sensitive(false);
        save.set_tooltip_text(Some("Still uploading"));
        return row.upcast();
    };

    let transfer = TransferId::new();
    cancel.connect_clicked({
        let client = client.clone();
        move |_| client.cancel_transfer(transfer)
    });
    let file = file.clone();
    save.connect_clicked(move |button| {
        let dir = downloads_dir();
        let dialog = gtk::FileDialog::builder()
            .title("Save Attachment")
            .initial_folder(&gio::File::for_path(&dir))
            .initial_name(unique_name(&dir, &file.filename))
            .build();
        let window = button.root().and_downcast::<gtk::Window>();
        let (client, runtime, sha256, file) = (
            client.clone(),
            runtime.clone(),
            sha256.clone(),
            file.clone(),
        );
        let (save, cancel, progress, status) = (
            button.clone(),
            cancel.clone(),
            progress.clone(),
            status.clone(),
        );
        dialog.save(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
            let Some(path) = chosen.ok().and_then(|f| f.path()) else {
                return; // cancelled
            };
            save.set_visible(false);
            cancel.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_visible(true);
            status.set_visible(false);

            // Progress: the transfer's events, on the GTK loop.
            let mut events = client.transfer_events();
            let bar = progress.downgrade();
            glib::spawn_future_local(async move {
                while let Ok(event) = events.recv().await {
                    if event.id != transfer {
                        continue;
                    }
                    let Some(bar) = bar.upgrade() else { break };
                    if event.total > 0 {
                        bar.set_fraction(event.done as f64 / event.total as f64);
                    }
                    if !matches!(
                        event.state,
                        TransferState::Running | TransferState::Retrying { .. }
                    ) {
                        break;
                    }
                }
            });

            // Under Flatpak, replacing an existing file writes into it directly.
            let replaced_in_place = is_flatpak() && path.exists();
            let download = runtime.spawn({
                let client = client.clone();
                async move {
                    // Replacing a file natively: download beside it and swap it in only
                    // once verified, so a failed save keeps the old copy (#105). Under
                    // Flatpak the portal grants just the chosen file: write it directly.
                    // A symlink is followed: the file it points to is what gets replaced.
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    let target = write_target(&path, is_flatpak());
                    let io = |err: std::io::Error| brook_core::Error::Api {
                        code: "transfer.io".into(),
                        message: format!("{:?}", err.kind()),
                    };
                    let mut sink = FileSink::create(&target).await.map_err(io)?;
                    client
                        .download_file(transfer, &file.id, &sha256, file.size, &mut sink)
                        .await?;
                    if target != path {
                        // Keep the replaced file's permissions, then swap it in and sync
                        // the directory so the rename itself survives a crash.
                        if let Ok(meta) = tokio::fs::metadata(&path).await {
                            let _ = tokio::fs::set_permissions(&target, meta.permissions()).await;
                        }
                        tokio::fs::rename(&target, &path).await.map_err(io)?;
                        if let Some(dir) = path.parent() {
                            if let Ok(d) = tokio::fs::File::open(dir).await {
                                let _ = d.sync_all().await;
                            }
                        }
                    }
                    Ok(())
                }
            });
            glib::spawn_future_local(async move {
                let result = download
                    .await
                    .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
                cancel.set_visible(false);
                progress.set_visible(false);
                status.set_visible(true);
                match result {
                    // Saved, and nothing more: never opened automatically.
                    Ok(()) => status.set_text("Saved"),
                    Err(err) => {
                        let mut text = save_error_text(&err);
                        if replaced_in_place {
                            // Under Flatpak a replace writes into the file itself: say so.
                            text.push_str(". The previous file was removed");
                        }
                        status.set_text(&text);
                        save.set_visible(true);
                    }
                }
            });
        });
    });
    row.upcast()
}

/// A failed save, briefly (the row's own label).
pub fn save_error_text(err: &brook_core::Error) -> String {
    match err {
        brook_core::Error::Api { code, .. } => match code.as_str() {
            "transfer.cancelled" => "Cancelled".into(),
            "transfer.integrity" => "Damaged in transfer, not saved".into(),
            "transfer.io" => "Couldn't write the file".into(),
            "file.gone" | "not_found" => "No longer available".into(),
            _ => "Couldn't save".into(),
        },
        brook_core::Error::NotAuthenticated => "Signed out".into(),
        _ => "Couldn't reach the server".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clash_gets_a_number_before_the_whole_extension() {
        let dir = tempfile_dir();
        assert_eq!(unique_name(&dir, "report.tar.gz"), "report.tar.gz");
        std::fs::write(dir.join("report.tar.gz"), b"").unwrap();
        assert_eq!(unique_name(&dir, "report.tar.gz"), "report (1).tar.gz");
        std::fs::write(dir.join("report (1).tar.gz"), b"").unwrap();
        assert_eq!(unique_name(&dir, "report.tar.gz"), "report (2).tar.gz");
        std::fs::write(dir.join("README"), b"").unwrap();
        assert_eq!(unique_name(&dir, "README"), "README (1)");
        std::fs::write(dir.join(".profile"), b"").unwrap();
        assert_eq!(unique_name(&dir, ".profile"), ".profile (1)");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacing_a_file_writes_beside_it_except_under_flatpak() {
        let dir = tempfile_dir();
        let chosen = dir.join("report.pdf");
        assert_eq!(
            write_target(&chosen, false),
            chosen,
            "a new file is written directly"
        );
        std::fs::write(&chosen, b"old").unwrap();
        assert_eq!(
            write_target(&chosen, false),
            dir.join(".report.pdf.brook-part")
        );
        assert_eq!(
            write_target(&chosen, true),
            chosen,
            "the portal grants only this file"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_declared_type_only_picks_an_icon() {
        assert_eq!(icon_for("image/png"), "image-x-generic-symbolic");
        assert_eq!(
            icon_for("application/x-msdownload"),
            "text-x-generic-symbolic"
        );
        assert_eq!(icon_for(""), "text-x-generic-symbolic");
    }

    #[test]
    fn save_errors_are_short_and_honest() {
        let api = |code: &str| brook_core::Error::Api {
            code: code.into(),
            message: String::new(),
        };
        assert!(save_error_text(&api("transfer.integrity")).contains("not saved"));
        assert_eq!(save_error_text(&api("transfer.cancelled")), "Cancelled");
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brook-att-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
