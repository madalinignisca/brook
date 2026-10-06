//! Small chat behaviours shared with the Mac (#291): what Ctrl+V does with the clipboard, the
//! name a pasted image gets, when the jump-to-latest button shows, and which pasted files are
//! left over. Plain functions, so a change that breaks them fails a test.

use std::path::{Path, PathBuf};

/// What Ctrl+V in the message box does.
#[derive(Debug, PartialEq, Eq)]
pub enum Paste {
    /// Copied files: stage them, as if dropped.
    Files,
    /// A picture and nothing else: stage it as a new PNG file.
    Image,
    /// Text, or nothing of ours: the box's own paste.
    Text,
}

/// The clipboard as Ctrl+V sees it: whether it holds a file list, text, or an image.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Offered {
    pub files: bool,
    pub text: bool,
    pub image: bool,
}

/// What the clipboard offers, from GDK's view of it: the MIME types, and whether GDK can turn it
/// into a file list (it can from `text/uri-list`), a picture, or a string.
pub fn offered_from(mimes: &[String], file_list: bool, texture: bool, string: bool) -> Offered {
    Offered {
        files: file_list,
        text: string || mimes.iter().any(|m| m.starts_with("text/plain")),
        image: texture || mimes.iter().any(|m| m.starts_with("image/")),
    }
}

/// The Mac's rule (`PasteImport.decide`): nothing is staged where files can't be attached;
/// copied files decide before text (a file manager also offers their names as text); text
/// beside a picture is text (a cell copied from a spreadsheet); a picture alone is an image.
pub fn paste(offered: Offered, can_attach: bool) -> Paste {
    if !can_attach {
        return Paste::Text;
    }
    if offered.files {
        return Paste::Files;
    }
    if offered.text {
        return Paste::Text;
    }
    if offered.image {
        Paste::Image
    } else {
        Paste::Text
    }
}

/// "Pasted image 2026-10-06 19.40.12.png": no colons (some systems can't name a file with them).
pub fn pasted_name(year: i32, month: u32, day: u32, h: u32, m: u32, s: u32) -> String {
    format!("Pasted image {year:04}-{month:02}-{day:02} {h:02}.{m:02}.{s:02}.png")
}

/// How far up from the newest message the view must be before the jump-to-latest button shows.
pub const JUMP_AFTER: f64 = 80.0;

/// Whether the jump-to-latest button shows: scrolled up more than [`JUMP_AFTER`] from the bottom.
pub fn shows_jump(value: f64, page_size: f64, upper: f64) -> bool {
    upper - (value + page_size) > JUMP_AFTER
}

/// The pasted files under `root` that no staged file uses any more (unstaged, sent, refused):
/// they are deleted.
pub fn leftovers(root: &Path, existing: &[PathBuf], staged: &[PathBuf]) -> Vec<PathBuf> {
    existing
        .iter()
        .filter(|p| p.starts_with(root) && !staged.contains(p))
        .cloned()
        .collect()
}

/// The paste folders (`<pid>-<uuid>`) that belong to Brook processes no longer running: theirs
/// can go at start-up; a running window's stay. A name that isn't one of ours is left alone.
pub fn dead_paste_dirs(names: &[String], alive: impl Fn(u32) -> bool) -> Vec<String> {
    names
        .iter()
        .filter(|name| {
            name.split_once('-')
                .and_then(|(pid, _)| pid.parse::<u32>().ok())
                .is_some_and(|pid| !alive(pid))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(files: bool, text: bool, image: bool) -> Offered {
        Offered { files, text, image }
    }

    #[test]
    fn copied_files_are_staged_even_with_their_names_as_text() {
        assert_eq!(paste(offered(true, true, false), true), Paste::Files);
        assert_eq!(paste(offered(true, false, true), true), Paste::Files);
    }

    #[test]
    fn a_picture_alone_is_staged_and_text_beside_it_wins() {
        assert_eq!(paste(offered(false, false, true), true), Paste::Image);
        assert_eq!(paste(offered(false, true, true), true), Paste::Text);
        assert_eq!(paste(offered(false, true, false), true), Paste::Text);
        assert_eq!(paste(offered(false, false, false), true), Paste::Text);
    }

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn files_copied_in_a_file_manager_are_staged() {
        // Nautilus: a URI list, its own type, and the names as text.
        let o = offered_from(
            &mimes(&[
                "x-special/gnome-copied-files",
                "text/uri-list",
                "text/plain;charset=utf-8",
            ]),
            true,
            false,
            true,
        );
        assert_eq!(paste(o, true), Paste::Files);
    }

    #[test]
    fn a_spreadsheet_cell_pastes_as_text() {
        let o = offered_from(
            &mimes(&["text/plain", "text/html", "image/png"]),
            false,
            true,
            true,
        );
        assert_eq!(paste(o, true), Paste::Text);
    }

    #[test]
    fn a_screenshot_is_staged_as_a_picture() {
        let o = offered_from(&mimes(&["image/png"]), false, true, false);
        assert_eq!(paste(o, true), Paste::Image);
    }

    #[test]
    fn web_links_are_read_as_a_list_then_decided_again_without_files() {
        // GDK offers a file list for any URI list; after reading finds no local file, the paste
        // is decided again with no files: links alone are text, links beside a picture a picture.
        let links = offered_from(&mimes(&["text/uri-list", "text/plain"]), true, false, true);
        assert_eq!(paste(links, true), Paste::Files);
        assert_eq!(
            paste(
                Offered {
                    files: false,
                    ..links
                },
                true
            ),
            Paste::Text
        );
        let with_picture = offered_from(&mimes(&["text/uri-list", "image/png"]), true, true, false);
        assert_eq!(
            paste(
                Offered {
                    files: false,
                    ..with_picture
                },
                true
            ),
            Paste::Image
        );
    }

    #[test]
    fn nothing_is_staged_where_files_cannot_be_attached() {
        assert_eq!(paste(offered(true, false, false), false), Paste::Text);
        assert_eq!(paste(offered(false, false, true), false), Paste::Text);
    }

    #[test]
    fn a_pasted_image_is_named_by_its_time_without_colons() {
        assert_eq!(
            pasted_name(2026, 10, 6, 19, 40, 12),
            "Pasted image 2026-10-06 19.40.12.png"
        );
        assert_eq!(
            pasted_name(2026, 1, 2, 3, 4, 5),
            "Pasted image 2026-01-02 03.04.05.png"
        );
        assert!(!pasted_name(2026, 10, 6, 19, 40, 12).contains(':'));
    }

    #[test]
    fn the_jump_button_shows_only_past_the_threshold() {
        // 1000 px of messages, a 400 px view.
        assert!(!shows_jump(600.0, 400.0, 1000.0), "at the bottom");
        assert!(!shows_jump(520.0, 400.0, 1000.0), "80 px up: not yet");
        assert!(shows_jump(519.0, 400.0, 1000.0), "81 px up");
        assert!(shows_jump(0.0, 400.0, 1000.0), "at the top");
        assert!(!shows_jump(0.0, 400.0, 300.0), "nothing to scroll");
    }

    #[test]
    fn only_pasted_files_no_longer_staged_are_left_over() {
        let root = Path::new("/run/user/1000/brook/pasted");
        let a = root.join("a/Pasted image 1.png");
        let b = root.join("b/Pasted image 2.png");
        let outside = PathBuf::from("/home/me/photo.png");
        let existing = vec![a.clone(), b.clone(), outside.clone()];
        assert_eq!(
            leftovers(root, &existing, std::slice::from_ref(&a)),
            vec![b.clone()]
        );
        assert!(leftovers(root, &existing, &[a, b]).is_empty());
        // Never anything outside the pasted root, staged or not.
        assert!(!leftovers(root, std::slice::from_ref(&outside), &[]).contains(&outside));
    }

    #[test]
    fn only_the_paste_folders_of_dead_runs_go_at_start_up() {
        let names: Vec<String> = ["100-aaa", "200-bbb", "not-ours", "300"]
            .map(String::from)
            .to_vec();
        let alive = |pid: u32| pid == 200;
        assert_eq!(dead_paste_dirs(&names, alive), vec!["100-aaa".to_string()]);
    }
}

/// What GTK itself does on paste, checked against a real display (Broadway or a desktop):
/// `GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 cargo test -p brook-gnome paste_on_a_display --
/// --ignored`. The Mac found its text field swallowed the paste before its own handler saw it;
/// this proves GtkText's paste signal reaches the handler, can be stopped, and what the
/// clipboard offers for a picture alone.
#[cfg(test)]
mod display_tests {
    use gtk::prelude::*;
    use gtk::{gdk, glib};
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    #[ignore = "needs a display"]
    fn paste_on_a_display() {
        gtk::init().expect("a display (GDK_BACKEND=broadway with gtk4-broadwayd running)");
        let entry = gtk::Entry::new();
        let text = entry
            .delegate()
            .and_downcast::<gtk::Text>()
            .expect("a GtkEntry edits through a GtkText");

        // A picture alone on the clipboard: offered as a texture, not as text.
        let png = gdk::MemoryTexture::new(
            1,
            1,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_static(&[255, 0, 0, 255]),
            4,
        );
        let clipboard = entry.clipboard();
        clipboard.set_texture(&png);
        let formats = clipboard.formats();
        assert!(formats.contains_type(gdk::Texture::static_type()));
        assert!(!formats.contains_type(glib::Type::STRING));

        // The paste signal (what Ctrl+V and the menu's Paste emit) reaches a handler that
        // stops it before the text box's own paste runs.
        let seen = Rc::new(Cell::new(0));
        text.connect_paste_clipboard({
            let seen = seen.clone();
            move |t| {
                seen.set(seen.get() + 1);
                t.stop_signal_emission_by_name("paste-clipboard");
            }
        });
        clipboard.set_text("should not be pasted");
        text.emit_paste_clipboard();
        let context = glib::MainContext::default();
        while context.iteration(false) {}
        assert_eq!(seen.get(), 1, "the handler saw the paste");
        assert_eq!(
            entry.text(),
            "",
            "stopping it kept the text box's own paste out"
        );
    }
}
