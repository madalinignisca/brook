//! Files sent with a message: picked in the composer, copied by core into its encrypted
//! outbox, then uploaded (core's `send_queued_with_files`).
//!
//! - Files wait under the message box as chips until Send; each can be removed.
//! - On Send, core copies each file before the call returns (`Preparing`): its chip shows
//!   the copy, and the chip's stop button cancels the whole send (nothing is queued).
//! - Once queued, the pending bubble shows each file's upload, a Cancel for the message's
//!   sending (Retry resumes it), and the server's refusal of a file, if any.
//!
//! Progress arrives on `transfer_events` by transfer id; [`Progress`] routes it to whichever
//! bar currently shows that id.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{
    BrookClient, OutgoingFile, PendingFile, TransferId, TransferState, MAX_FILES_PER_MESSAGE,
    MAX_FILE_BYTES,
};
use gtk::{gio, glib};

/// A file waiting in the composer.
#[derive(Clone)]
pub struct Staged {
    pub path: PathBuf,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    /// Its progress and cancel id, made before Send so the copy can be followed.
    pub transfer_id: TransferId,
}

impl Staged {
    pub fn outgoing(&self) -> OutgoingFile {
        OutgoingFile {
            path: self.path.clone(),
            filename: self.name.clone(),
            content_type: self.content_type.clone(),
            transfer_id: Some(self.transfer_id),
        }
    }
}

/// Why a picked file can't be added (checked again by core, which has the final word).
pub fn refusal(already: usize, name: &str, size: u64) -> Option<String> {
    if already >= MAX_FILES_PER_MESSAGE {
        return Some(format!(
            "A message can carry up to {MAX_FILES_PER_MESSAGE} files."
        ));
    }
    if size == 0 {
        return Some(format!("{name} is empty."));
    }
    if size > MAX_FILE_BYTES {
        return Some(format!(
            "{name} is larger than {}.",
            glib::format_size(MAX_FILE_BYTES)
        ));
    }
    None
}

/// A send with files that core refused (its local codes), briefly.
pub fn send_error_text(code: &str) -> Option<&'static str> {
    Some(match code {
        "outbox.too_many_files" => "Too many files for one message.",
        "outbox.file_too_large" => "A file is too large to send.",
        "outbox.empty_file" => "An empty file can't be sent.",
        "outbox.empty_message" => "Write something or add a file.",
        "outbox.file_unreadable" => "A file couldn't be read. Is it still there?",
        "outbox.store" => "Couldn't prepare the files. Is the disk full?",
        "local.unavailable" => {
            "Sending files needs this device's storage, which is off (no keyring)."
        }
        _ => return None,
    })
}

/// A queued file's problem, from the code that failed its message.
pub fn file_error_text(code: &str) -> &'static str {
    match code {
        "file.too_large" => "Too large for the server",
        "file.quota_exceeded" => "Over your storage quota",
        "file.bad_content_type" => "The server refused its type",
        "outbox.duplicate_file" => "The same file is in this message twice",
        "outbox.snapshot_damaged" => "The saved copy is damaged: delete and send it again",
        _ => "Not uploaded",
    }
}

/// One file's progress widgets.
#[derive(Clone)]
struct Bar {
    bar: gtk::ProgressBar,
    status: gtk::Label,
}

/// Transfer id -> the bar showing it, fed by one `transfer_events` listener.
#[derive(Clone, Default)]
pub struct Progress {
    bars: Rc<RefCell<HashMap<TransferId, Bar>>>,
}

impl Progress {
    /// Route the client's transfer events to the registered bars, for the chat's life.
    pub fn listen(&self, client: &Arc<BrookClient>) {
        let mut events = client.transfer_events();
        let bars = Rc::downgrade(&self.bars);
        glib::spawn_future_local(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(RecvError::Lagged(_)) => continue, // the next event catches up
                    Err(RecvError::Closed) => break,
                };
                let Some(bars) = bars.upgrade() else { break };
                let Some(shown) = bars.borrow().get(&event.id).cloned() else {
                    continue;
                };
                if event.total > 0 {
                    shown
                        .bar
                        .set_fraction((event.done as f64 / event.total as f64).min(1.0));
                }
                shown.bar.set_visible(true);
                let status = match &event.state {
                    TransferState::Preparing => "Preparing…".to_string(),
                    TransferState::Running => "Uploading…".to_string(),
                    TransferState::Retrying { after_secs } if *after_secs >= 60 => {
                        format!("Waiting to retry ({} min)", after_secs.div_ceil(60))
                    }
                    TransferState::Retrying { .. } => "Waiting to retry".to_string(),
                    _ => String::new(),
                };
                shown.status.set_label(&status);
                shown.status.set_visible(!status.is_empty());
            }
        });
    }

    fn show(&self, id: TransferId, bar: &gtk::ProgressBar, status: &gtk::Label) {
        self.bars.borrow_mut().insert(
            id,
            Bar {
                bar: bar.clone(),
                status: status.clone(),
            },
        );
    }

    pub fn forget(&self, ids: impl IntoIterator<Item = TransferId>) {
        let mut bars = self.bars.borrow_mut();
        for id in ids {
            bars.remove(&id);
        }
    }
}

/// The common file line: icon, name, size, status, progress; the caller adds buttons.
fn file_line(
    content_type: &str,
    name: &str,
    size: u64,
) -> (gtk::Box, gtk::ProgressBar, gtk::Label) {
    let line = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(2)
        .css_classes(["card"])
        .build();
    let icon = gtk::Image::builder()
        .icon_name(crate::attachments::icon_for(content_type))
        .margin_start(8)
        .build();
    let label = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    let size = gtk::Label::builder()
        .label(glib::format_size(size).as_str())
        .css_classes(["caption", "dim-label"])
        .build();
    let status = gtk::Label::builder()
        .css_classes(["caption", "dim-label"])
        .visible(false)
        .build();
    let bar = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .width_request(80)
        .visible(false)
        .build();
    line.append(&icon);
    line.append(&label);
    line.append(&size);
    line.append(&status);
    line.append(&bar);
    (line, bar, status)
}

/// A composer chip for a staged file. `on_stop` removes it before Send, and cancels the
/// send while its copy runs.
pub fn staged_chip(file: &Staged, progress: &Progress, on_stop: impl Fn() + 'static) -> gtk::Box {
    let (line, bar, status) = file_line(&file.content_type, &file.name, file.size);
    progress.show(file.transfer_id, &bar, &status);
    let stop = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Remove")
        .css_classes(["flat"])
        .build();
    stop.connect_clicked(move |_| on_stop());
    line.append(&stop);
    line
}

/// A queued message's file, in its pending bubble.
pub fn pending_file_line(file: &PendingFile, progress: &Progress) -> gtk::Box {
    // The pending record has no declared type: the name's guess only picks the icon.
    let (content_type, _) = gio::content_type_guess(Some(file.filename.as_str()), &[]);
    let mime = gio::content_type_get_mime_type(&content_type)
        .map(|m| m.to_string())
        .unwrap_or_default();
    let (line, bar, status) = file_line(&mime, &file.filename, file.size);
    if let Some(code) = &file.error {
        status.set_label(file_error_text(code));
        status.remove_css_class("dim-label");
        status.add_css_class("error");
        status.set_visible(true);
    } else if file.uploaded {
        status.set_label("Uploaded");
        status.set_visible(true);
    } else {
        progress.show(file.transfer_id, &bar, &status);
    }
    line
}

/// Ask for files to add to the message (several at once).
pub fn pick(parent: Option<&gtk::Window>, on_picked: impl Fn(Vec<gio::File>) + 'static) {
    let dialog = gtk::FileDialog::builder()
        .title("Add files")
        .modal(true)
        .build();
    dialog.open_multiple(parent, gio::Cancellable::NONE, move |picked| {
        let Ok(list) = picked else { return }; // dismissed
        let files = (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gio::File>())
            .collect();
        on_picked(files);
    });
}

/// What the composer needs to stage a picked file (off the GTK loop's critical path: a
/// portal path answers asynchronously).
pub async fn describe(file: &gio::File) -> Option<Staged> {
    let path = file.path()?;
    let info = file
        .query_info_future(
            "standard::display-name,standard::size,standard::content-type",
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT,
        )
        .await
        .ok()?;
    let content_type = info
        .content_type()
        .and_then(|t| gio::content_type_get_mime_type(&t))
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".into());
    Some(Staged {
        path,
        name: info.display_name().to_string(),
        content_type,
        size: info.size().max(0) as u64,
        transfer_id: TransferId::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_checked_when_a_file_is_added() {
        assert!(refusal(0, "a.txt", 1).is_none());
        assert!(refusal(0, "a.txt", 0).unwrap().contains("empty"));
        assert!(refusal(0, "big.iso", MAX_FILE_BYTES + 1)
            .unwrap()
            .contains("larger"));
        assert!(refusal(MAX_FILES_PER_MESSAGE, "a.txt", 1)
            .unwrap()
            .contains("up to"));
        assert!(refusal(0, "edge.bin", MAX_FILE_BYTES).is_none());
    }

    #[test]
    fn core_refusals_read_as_sentences() {
        assert!(send_error_text("outbox.file_unreadable").is_some());
        assert!(send_error_text("local.unavailable")
            .unwrap()
            .contains("storage"));
        assert!(send_error_text("transfer.cancelled").is_none()); // the user's own choice
        assert_eq!(
            file_error_text("file.quota_exceeded"),
            "Over your storage quota"
        );
        assert_eq!(file_error_text("something.new"), "Not uploaded");
    }
}
