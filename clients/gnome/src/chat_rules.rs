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
#[derive(Debug, Default, Clone, Copy)]
pub struct Offered {
    pub files: bool,
    pub text: bool,
    pub image: bool,
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
}
