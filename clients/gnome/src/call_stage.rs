//! Which tile fills the call window (#268). A shared screen is what everyone looks at and is
//! unreadable in a grid cell, so a remote one takes the stage on its own; the user can put any
//! tile on the stage by clicking it, and clicking the pinned stage releases it. Everything else
//! waits in a strip along the bottom. No stage means the equal grid. The same rule as the Mac's
//! `CallStage`; plain functions, so a client that ignores a screen share or a pin fails a test.

/// What the rule needs to know of a tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileRef {
    pub id: String,
    /// A remote participant's shared screen.
    pub screen: bool,
}

/// The tile on the stage (none: the equal grid) and the rest, in the order given.
#[derive(Debug, PartialEq, Eq)]
pub struct Split {
    pub stage: Option<String>,
    pub strip: Vec<String>,
}

/// `pinned`: the tile the user chose. A pin that is no longer in the call (the person left, the
/// screen share ended) is ignored, so the layout falls back by itself: the first screen if there
/// is one, else the grid.
pub fn split(tiles: &[TileRef], pinned: Option<&str>) -> Split {
    let chosen = tiles
        .iter()
        .find(|t| Some(t.id.as_str()) == pinned)
        .or_else(|| tiles.iter().find(|t| t.screen));
    match chosen {
        None => Split {
            stage: None,
            strip: tiles.iter().map(|t| t.id.clone()).collect(),
        },
        Some(chosen) => Split {
            stage: Some(chosen.id.clone()),
            strip: tiles
                .iter()
                .filter(|t| t.id != chosen.id)
                .map(|t| t.id.clone())
                .collect(),
        },
    }
}

/// A click on a tile: on the stage and pinned it releases the pin (back to the automatic
/// choice); anywhere else it pins that tile. (Clicking the stage a screen share holds by itself
/// pins it, which keeps it there if the share is replaced by another.)
pub fn toggled(pinned: Option<&str>, clicked: &str, on_stage: Option<&str>) -> Option<String> {
    if on_stage == Some(clicked) && pinned == Some(clicked) {
        None
    } else {
        Some(clicked.to_string())
    }
}

/// The pin after a tile left: a pin for someone who is no longer in the call is dropped for good,
/// so that when the id is used again (a participant joins and takes the same stream) the new tile
/// is not put on the stage by a pin meant for the one who left.
pub fn pruned(pinned: Option<&str>, tiles: &[TileRef]) -> Option<String> {
    pinned
        .filter(|p| tiles.iter().any(|t| t.id == *p))
        .map(str::to_string)
}

/// Where a new tile goes in the order: a remote screen after the screens already there (so the
/// first share stays on the stage and a second one waits in the strip), anything else at the end.
pub fn insert_at(tiles: &[TileRef], screen: bool) -> usize {
    if !screen {
        return tiles.len();
    }
    tiles.iter().rposition(|t| t.screen).map_or(0, |i| i + 1)
}

/// How tall a strip tile is: about a fifth of the window, kept between a usable minimum and a
/// sensible maximum.
pub fn strip_height(window_height: i32) -> i32 {
    (window_height / 5).clamp(72, 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(id: &str, screen: bool) -> TileRef {
        TileRef {
            id: id.into(),
            screen,
        }
    }

    fn ids(split: &Split) -> (Option<&str>, Vec<&str>) {
        (
            split.stage.as_deref(),
            split.strip.iter().map(String::as_str).collect(),
        )
    }

    #[test]
    fn with_no_screen_and_no_pin_it_is_the_equal_grid() {
        let s = split(&[tile("a", false), tile("b", false)], None);
        assert_eq!(ids(&s), (None, vec!["a", "b"]));
    }

    #[test]
    fn a_shared_screen_takes_the_stage_by_itself() {
        let tiles = [tile("cam", false), tile("scr", true), tile("me", false)];
        let s = split(&tiles, None);
        assert_eq!(ids(&s), (Some("scr"), vec!["cam", "me"]));
    }

    #[test]
    fn a_pin_beats_the_screen() {
        let tiles = [tile("scr", true), tile("cam", false), tile("me", false)];
        let s = split(&tiles, Some("cam"));
        assert_eq!(ids(&s), (Some("cam"), vec!["scr", "me"]));
    }

    #[test]
    fn a_pin_for_someone_who_left_falls_back_to_the_screen_or_the_grid() {
        let with_screen = [tile("scr", true), tile("cam", false)];
        assert_eq!(
            ids(&split(&with_screen, Some("gone"))),
            (Some("scr"), vec!["cam"])
        );
        let none = [tile("cam", false), tile("me", false)];
        assert_eq!(ids(&split(&none, Some("gone"))), (None, vec!["cam", "me"]));
    }

    #[test]
    fn a_screen_share_that_ended_leaves_the_grid_again() {
        let before = [tile("scr", true), tile("cam", false)];
        assert_eq!(split(&before, None).stage.as_deref(), Some("scr"));
        let after = [tile("cam", false)];
        assert_eq!(ids(&split(&after, None)), (None, vec!["cam"]));
        // ...and a pin on the ended share is stale too.
        assert_eq!(split(&after, Some("scr")).stage, None);
    }

    #[test]
    fn the_first_of_two_screens_is_on_the_stage_and_the_other_waits() {
        let tiles = [tile("s1", true), tile("s2", true), tile("cam", false)];
        assert_eq!(ids(&split(&tiles, None)), (Some("s1"), vec!["s2", "cam"]));
        assert_eq!(
            ids(&split(&tiles, Some("s2"))),
            (Some("s2"), vec!["s1", "cam"])
        );
    }

    #[test]
    fn an_empty_call_has_nothing_on_the_stage() {
        assert_eq!(ids(&split(&[], None)), (None, vec![]));
        assert_eq!(ids(&split(&[], Some("x"))), (None, vec![]));
    }

    #[test]
    fn a_click_pins_a_tile_and_a_click_on_the_pinned_stage_releases_it() {
        // A camera in the strip: pinned.
        assert_eq!(toggled(None, "cam", Some("scr")).as_deref(), Some("cam"));
        // Now on the stage and pinned: released.
        assert_eq!(toggled(Some("cam"), "cam", Some("cam")), None);
        // Another tile while one is pinned: moves the pin.
        assert_eq!(
            toggled(Some("cam"), "me", Some("cam")).as_deref(),
            Some("me")
        );
    }

    #[test]
    fn a_click_on_the_automatic_stage_pins_it_instead_of_releasing_nothing() {
        // The screen is on the stage by itself (no pin): the click pins it.
        assert_eq!(toggled(None, "scr", Some("scr")).as_deref(), Some("scr"));
        // With no stage (the grid) a click puts that tile on the stage.
        assert_eq!(toggled(None, "a", None).as_deref(), Some("a"));
    }

    #[test]
    fn the_strip_is_about_a_fifth_of_the_window() {
        assert_eq!(strip_height(640), 128);
        assert_eq!(strip_height(300), 72, "never below a usable size");
        assert_eq!(strip_height(2000), 200, "never huge");
    }

    #[test]
    fn a_pin_for_someone_who_left_is_dropped_for_good() {
        let tiles = [tile("scr", true), tile("cam", false)];
        assert_eq!(pruned(Some("cam"), &tiles).as_deref(), Some("cam"));
        assert_eq!(pruned(Some("gone"), &tiles), None);
        assert_eq!(pruned(None, &tiles), None);
        // The id comes back as someone else's stream: nothing is pinned any more.
        let later = [tile("scr", true), tile("cam", false), tile("gone", false)];
        assert_eq!(
            split(&later, pruned(Some("gone"), &tiles).as_deref())
                .stage
                .as_deref(),
            Some("scr")
        );
    }

    #[test]
    fn a_new_screen_goes_after_the_screens_and_a_camera_at_the_end() {
        let tiles = [tile("s1", true), tile("s2", true), tile("cam", false)];
        assert_eq!(insert_at(&tiles, true), 2, "the first share stays first");
        assert_eq!(insert_at(&tiles, false), 3);
        assert_eq!(insert_at(&[tile("cam", false)], true), 0);
        assert_eq!(insert_at(&[], true), 0);
        assert_eq!(insert_at(&[], false), 0);
    }
}
