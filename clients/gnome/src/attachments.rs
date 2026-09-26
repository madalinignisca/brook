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

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{BrookClient, FileCacheState, FileInfo, FileSink, TransferId, TransferState};
use gtk::{gdk, gio, glib};
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
    let open = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text("Open")
        .css_classes(["flat"])
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
    let keep = gtk::ToggleButton::builder()
        .icon_name("folder-download-symbolic")
        .tooltip_text("Keep available offline")
        .css_classes(["flat"])
        .build();
    for w in [
        icon.upcast_ref::<gtk::Widget>(),
        name.upcast_ref(),
        size.upcast_ref(),
        status.upcast_ref(),
        progress.upcast_ref(),
        open.upcast_ref(),
        save.upcast_ref(),
        keep.upcast_ref(),
        cancel.upcast_ref(),
    ] {
        row.append(w);
    }

    // Not uploaded yet (or no checksum to verify against): nothing to save.
    let Some(sha256) = file.sha256.clone().filter(|_| file.status == "committed") else {
        save.set_sensitive(false);
        save.set_tooltip_text(Some("Still uploading"));
        open.set_sensitive(false);
        keep.set_sensitive(false);
        return row.upcast();
    };

    // The preview part (at the end) needs these after Save's handler has taken its own.
    let (p_client, p_runtime, p_file) = (client.clone(), runtime.clone(), file.clone());

    // "Keep available offline": the row follows the cache's state for this file (pinned,
    // fetching, gone), refreshed on `CacheEvent::Files`.
    let refresh: Rc<dyn Fn()> = Rc::new({
        let (client, runtime, file_id) = (client.clone(), runtime.clone(), file.id.clone());
        let (keep, status, progress) = (keep.downgrade(), status.downgrade(), progress.downgrade());
        let following = Rc::new(std::cell::Cell::new(None::<TransferId>));
        // Reads are numbered: only the newest one started applies, whenever it finishes.
        let reads = Rc::new(std::cell::Cell::new(0u64));
        move || {
            let mine = reads.get() + 1;
            reads.set(mine);
            let reads = reads.clone();
            let (client, file_id) = (client.clone(), file_id.clone());
            let state = runtime.spawn({
                let (client, file_id) = (client.clone(), file_id.clone());
                async move { client.file_state(&file_id).await }
            });
            let (keep, status, progress, following) = (
                keep.clone(),
                status.clone(),
                progress.clone(),
                following.clone(),
            );
            glib::spawn_future_local(async move {
                let Ok(Ok(state)) = state.await else { return };
                if reads.get() != mine {
                    return; // a newer read is on its way
                }
                let (Some(keep), Some(status), Some(progress)) =
                    (keep.upgrade(), status.upgrade(), progress.upgrade())
                else {
                    return;
                };
                match keep_view(&state) {
                    KeepView::Off => {
                        set_active_quietly(&keep, false);
                        if status.text() == "Available offline"
                            || status.text() == "Downloading for offline"
                        {
                            status.set_visible(false);
                        }
                    }
                    KeepView::Fetching(id) => {
                        set_active_quietly(&keep, true);
                        status.set_text("Downloading for offline");
                        status.set_visible(true);
                        if following.replace(id) != id {
                            if let Some(id) = id {
                                progress.set_visible(true);
                                follow_progress(&client, id, &progress);
                            }
                        }
                    }
                    KeepView::Kept => {
                        set_active_quietly(&keep, true);
                        progress.set_visible(false);
                        status.set_text("Available offline");
                        status.set_visible(true);
                    }
                }
            });
        }
    });
    register_row(&file.id, &refresh);
    refresh();
    keep.connect_toggled({
        let (client, runtime, file_id, status) = (
            client.clone(),
            runtime.clone(),
            file.id.clone(),
            status.clone(),
        );
        // The registry holds `refresh` weakly: this handler keeps it alive exactly as long
        // as the row (it only captures widgets weakly, so there's no cycle).
        let refresh = refresh.clone();
        move |button| {
            let _alive = &refresh;
            if button.widget_name() == QUIET {
                return; // set from the cache's state, not by the user
            }
            let pin = button.is_active();
            // No second toggle until this one's call returns: a quick on/off can't land out
            // of order.
            button.set_sensitive(false);
            let (client, file_id) = (client.clone(), file_id.clone());
            let done = runtime.spawn(async move {
                if pin {
                    client.pin_file(&file_id).await
                } else {
                    client.unpin_file(&file_id).await
                }
            });
            let (button, status, refresh) = (button.clone(), status.clone(), refresh.clone());
            glib::spawn_future_local(async move {
                let result = done.await;
                button.set_sensitive(true);
                if let Ok(Err(err)) = result {
                    status.set_text(&keep_error_text(&err));
                    status.set_visible(true);
                    // The cache's word, not a guess: a newer state (a Files event meanwhile)
                    // wins over simply setting the toggle back.
                    refresh();
                }
            });
        }
    });

    // Open and Save each run under their own id; Cancel stops whichever is running.
    let transfer = TransferId::new();
    let running = std::rc::Rc::new(std::cell::Cell::new(transfer));
    cancel.connect_clicked({
        let (client, running) = (client.clone(), running.clone());
        move |_| client.cancel_transfer(running.get())
    });
    open.connect_clicked({
        let (client, runtime, file_id) = (client.clone(), runtime.clone(), file.id.clone());
        let (save, cancel, progress, status, running) = (
            save.clone(),
            cancel.clone(),
            progress.clone(),
            status.clone(),
            running.clone(),
        );
        move |button| {
            let id = TransferId::new();
            running.set(id);
            button.set_sensitive(false);
            cancel.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_visible(true);
            status.set_visible(false);
            follow_progress(&client, id, &progress);
            // Downloaded into this device's encrypted cache (it opens offline next time),
            // then a private copy is handed to the system's app for its type.
            let opened = runtime.spawn({
                let (client, file_id) = (client.clone(), file_id.clone());
                async move { client.open_file(id, &file_id).await }
            });
            let window = button.root().and_downcast::<gtk::Window>();
            let (button, save, cancel, progress, status) = (
                button.clone(),
                save.clone(),
                cancel.clone(),
                progress.clone(),
                status.clone(),
            );
            glib::spawn_future_local(async move {
                let result = opened
                    .await
                    .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
                cancel.set_visible(false);
                progress.set_visible(false);
                button.set_sensitive(true);
                match result {
                    Ok(path) => {
                        // Core's copy, opened exactly as returned; it's only ever cleared at
                        // sign-out or close, but check before handing it over.
                        if !path.exists() {
                            status.set_text("Not available yet. Try again in a moment");
                            status.set_visible(true);
                            return;
                        }
                        let status = status.clone();
                        gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
                            window.as_ref(),
                            gio::Cancellable::NONE,
                            move |launched| {
                                if let Err(err) = launched {
                                    // Dismissing the app chooser isn't a failure.
                                    if !err.matches(gtk::DialogError::Dismissed)
                                        && !err.matches(gtk::DialogError::Cancelled)
                                    {
                                        status.set_text("No app opens this. Save it instead");
                                        status.set_visible(true);
                                    }
                                }
                            },
                        );
                    }
                    Err(err) => {
                        status.set_text(&open_error_text(&err));
                        status.set_visible(true);
                        if is_gone(&err) {
                            button.set_visible(false);
                            save.set_visible(false);
                        }
                    }
                }
            });
        }
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
        let running = running.clone();
        dialog.save(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
            let Some(path) = chosen.ok().and_then(|f| f.path()) else {
                return; // cancelled
            };
            save.set_visible(false);
            cancel.set_visible(true);
            progress.set_fraction(0.0);
            progress.set_visible(true);
            status.set_visible(false);
            running.set(transfer);
            follow_progress(&client, transfer, &progress);

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
                    // From this device's cache when it's there (works offline), else from
                    // the server as before. Either way into `target`, verified.
                    let cached = match client.save_cached_file(&file.id, &target).await {
                        Ok(saved) => saved,
                        // No local data, or not a file the cache knows: download it.
                        Err(brook_core::Error::Api { code, .. })
                            if code == "local.unavailable" || code == "file.unknown" =>
                        {
                            false
                        }
                        Err(err) => return Err(err),
                    };
                    if !cached {
                        let mut sink = FileSink::create(&target).await.map_err(io)?;
                        client
                            .download_file(transfer, &file.id, &sha256, file.size, &mut sink)
                            .await?;
                    }
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
    // An inline preview under a declared image (previews spec §1, §3).
    if !previewable_type(&p_file.content_type) || p_file.size > brook_core::PREVIEW_MAX_BYTES {
        return row.upcast();
    }
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    outer.append(&row);
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .halign(gtk::Align::Start)
        .margin_top(4)
        .visible(false)
        .tooltip_text("Open")
        .css_classes(["card"])
        .build();
    let show = gtk::Button::builder()
        .label("Show preview")
        .halign(gtk::Align::Start)
        .css_classes(["flat", "caption"])
        .visible(false)
        .build();
    outer.append(&picture);
    outer.append(&show);
    // Clicking the preview is Open.
    let click = gtk::GestureClick::new();
    click.connect_released({
        let open = open.downgrade();
        move |_, _, _, _| {
            if let Some(open) = open.upgrade() {
                open.emit_clicked();
            }
        }
    });
    picture.add_controller(click);
    let start: Rc<dyn Fn()> = Rc::new({
        let (client, runtime, file_id) = (p_client.clone(), p_runtime.clone(), p_file.id.clone());
        let (picture, show) = (picture.downgrade(), show.downgrade());
        move || {
            if let Some(show) = show.upgrade() {
                show.set_visible(false);
            }
            let alive = picture.clone();
            let (client, runtime, file_id, picture) = (
                client.clone(),
                runtime.clone(),
                file_id.clone(),
                picture.clone(),
            );
            crate::preview::QUEUE.with(|q| {
                q.push(crate::preview::Job {
                    alive: Box::new(move || alive.upgrade().is_some()),
                    run: Box::new(move |done| {
                        let decoded = runtime.spawn(async move {
                            let bytes = client
                                .preview_file(TransferId::new(), &file_id)
                                .await
                                .ok()?
                                .bytes;
                            crate::preview::decode(bytes).await
                        });
                        glib::spawn_future_local(async move {
                            if let (Ok(Some(px)), Some(picture)) =
                                (decoded.await, picture.upgrade())
                            {
                                show_pixels(&picture, px);
                            }
                            done();
                        });
                    }),
                })
            });
        }
    });
    show.connect_clicked({
        let start = start.clone();
        move |_| start()
    });
    // Small ones by themselves, unless the connection is metered and it isn't cached yet.
    if p_file.size <= crate::preview::AUTO_MAX_BYTES {
        let metered = gio::NetworkMonitor::default().is_network_metered();
        if !metered {
            start();
        } else {
            let cached = p_runtime.spawn({
                let (client, file_id) = (p_client.clone(), p_file.id.clone());
                async move { client.file_state(&file_id).await }
            });
            let (start, show) = (start.clone(), show.downgrade());
            glib::spawn_future_local(async move {
                match cached.await {
                    Ok(Ok(
                        FileCacheState::Cached | FileCacheState::Pinned { cached: true, .. },
                    )) => start(),
                    _ => {
                        if let Some(show) = show.upgrade() {
                            show.set_visible(true);
                        }
                    }
                }
            });
        }
    } else {
        show.set_visible(true);
    }
    outer.upcast()
}

/// A declared image type a preview may be tried for (the sniff in core still decides).
pub fn previewable_type(content_type: &str) -> bool {
    matches!(
        content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Put decoded pixels on the picture, scaled down to at most 360 x 240, aspect kept.
fn show_pixels(picture: &gtk::Picture, px: crate::preview::Pixels) {
    let texture = gdk::MemoryTexture::new(
        px.width as i32,
        px.height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(px.rgba),
        px.stride,
    );
    let scale = (360.0 / px.width as f64)
        .min(240.0 / px.height as f64)
        .min(1.0);
    picture.set_size_request(
        ((px.width as f64 * scale).round() as i32).max(1),
        ((px.height as f64 * scale).round() as i32).max(1),
    );
    picture.set_paintable(Some(&texture));
    picture.set_visible(true);
}

/// How the "keep offline" part of a row shows a cache state.
#[derive(Debug, PartialEq, Eq)]
enum KeepView {
    Off,
    /// Pinned, not complete yet: the background download's id, if it's running.
    Fetching(Option<TransferId>),
    Kept,
}

fn keep_view(state: &FileCacheState) -> KeepView {
    match state {
        FileCacheState::Pinned { cached: true, .. } => KeepView::Kept,
        FileCacheState::Pinned { transfer, .. } => KeepView::Fetching(*transfer),
        _ => KeepView::Off,
    }
}

fn keep_error_text(err: &brook_core::Error) -> String {
    match err {
        brook_core::Error::Api { code, .. } if code == "local.unavailable" => {
            "Keeping files offline needs this device's storage".into()
        }
        brook_core::Error::Api { code, .. } if code == "file.unknown" => {
            "Not available yet. Try again in a moment".into()
        }
        other => save_error_text(other),
    }
}

/// A toggle set from state, not a click: its handler checks this name and does nothing.
const QUIET: &str = "brook-quiet";

fn set_active_quietly(button: &gtk::ToggleButton, active: bool) {
    if button.is_active() == active {
        return;
    }
    let name = button.widget_name();
    button.set_widget_name(QUIET);
    button.set_active(active);
    button.set_widget_name(&name);
}

/// A row's "re-read your state" callback, held weakly (the row owns it).
type RowRefresh = Weak<dyn Fn()>;

thread_local! {
    /// The attachment rows on screen by file id, for `CacheEvent::Files`.
    static ROWS: RefCell<HashMap<String, Vec<RowRefresh>>> = RefCell::default();
}

fn register_row(file_id: &str, refresh: &Rc<dyn Fn()>) {
    ROWS.with(|rows| {
        let mut rows = rows.borrow_mut();
        let entry = rows.entry(file_id.to_string()).or_default();
        entry.retain(|w| w.strong_count() > 0);
        entry.push(Rc::downgrade(refresh));
    });
}

/// The cache says these files changed: rows showing them re-read their state.
pub fn refresh_rows(file_ids: &[String]) {
    let live: Vec<Rc<dyn Fn()>> = ROWS.with(|rows| {
        let mut rows = rows.borrow_mut();
        rows.retain(|_, v| {
            v.retain(|w| w.strong_count() > 0);
            !v.is_empty()
        });
        file_ids
            .iter()
            .filter_map(|id| rows.get(id))
            .flatten()
            .filter_map(Weak::upgrade)
            .collect()
    });
    for refresh in live {
        refresh();
    }
}

/// Follow a transfer's progress on `bar` until it ends (on the GTK loop).
fn follow_progress(client: &BrookClient, id: TransferId, bar: &gtk::ProgressBar) {
    let mut events = client.transfer_events();
    let bar = bar.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            if event.id != id {
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
}

fn is_gone(err: &brook_core::Error) -> bool {
    matches!(err, brook_core::Error::Api { code, .. } if code == "file.gone")
}

/// A failed Open, briefly.
pub fn open_error_text(err: &brook_core::Error) -> String {
    match err {
        brook_core::Error::Api { code, .. } => match code.as_str() {
            "file.open_refused" => "Can't be opened from Brook. Save it instead".into(),
            "local.unavailable" => "Open needs this device's storage. Save it instead".into(),
            "file.unknown" => "Not available yet. Try again in a moment".into(),
            other => save_error_text_code(other),
        },
        _ => save_error_text(err),
    }
}

fn save_error_text_code(code: &str) -> String {
    save_error_text(&brook_core::Error::Api {
        code: code.into(),
        message: String::new(),
    })
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
    fn the_keep_offline_view_follows_the_cache() {
        let id = TransferId::new();
        let pinned = |cached, transfer| FileCacheState::Pinned {
            cached,
            done: 0,
            size: 1,
            transfer,
        };
        assert_eq!(keep_view(&FileCacheState::Cached), KeepView::Off);
        assert_eq!(keep_view(&FileCacheState::NotCached), KeepView::Off);
        assert_eq!(keep_view(&pinned(true, None)), KeepView::Kept);
        assert_eq!(
            keep_view(&pinned(false, Some(id))),
            KeepView::Fetching(Some(id))
        );
        assert_eq!(keep_view(&pinned(false, None)), KeepView::Fetching(None));
    }

    #[test]
    fn open_errors_say_what_to_do() {
        let api = |code: &str| brook_core::Error::Api {
            code: code.into(),
            message: String::new(),
        };
        assert!(open_error_text(&api("file.open_refused")).contains("Save it instead"));
        assert!(open_error_text(&api("local.unavailable")).contains("Save it instead"));
        assert_eq!(open_error_text(&api("file.gone")), "No longer available");
        assert_eq!(open_error_text(&api("transfer.cancelled")), "Cancelled");
        assert!(is_gone(&api("file.gone")));
    }

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
