//! Paging back through a channel's history (#248): which older page to ask for, and when the
//! start of the channel has been reached. The Mac's `loadOlder` follows the same rules: one
//! page at a time, the cache first (loading it when it can't vouch for the page), the network
//! when there is no local data, and the start is an empty page that was vouched for.

use std::time::{Duration, Instant};

/// After a page fails (offline, say), no new attempt for this long: scrolling at the top fires
/// edge signals on every tick, and each attempt is a cache read and a request.
const RETRY_AFTER: Duration = Duration::from_secs(3);

/// Per-channel paging state. `generation` changes with every channel switch, so a page that
/// arrives after the switch is dropped instead of drawn into the wrong channel.
#[derive(Debug, Default)]
pub struct Pager {
    generation: u64,
    /// The newest page is on screen: older ones can be asked for.
    ready: bool,
    loading: bool,
    at_start: bool,
    /// An older page was drawn: the user may be reading history, so live messages must not
    /// pull the view away from it.
    paged_back: bool,
    /// No new attempt before this (after a failed page).
    retry_at: Option<Instant>,
}

impl Pager {
    /// A different channel was opened: forget everything about the last one.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.ready = false;
        self.loading = false;
        self.at_start = false;
        self.paged_back = false;
        self.retry_at = None;
    }

    /// The newest page of the open channel is drawn (offline too): paging may begin.
    pub fn ready(&mut self, generation: u64) {
        if generation == self.generation {
            self.ready = true;
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Start paging: the message to page before and the generation to answer with, or `None`
    /// while the newest page isn't drawn, a page is already coming, the start was reached, or
    /// there is nothing on screen to page before.
    pub fn begin(&mut self, oldest: Option<String>, now: Instant) -> Option<(u64, String)> {
        if !self.ready || self.loading || self.at_start || self.retry_at.is_some_and(|t| now < t) {
            return None;
        }
        let oldest = oldest?;
        self.loading = true;
        Some((self.generation, oldest))
    }

    /// A page arrived: `empty` if it had no messages, `vouched` if its source can prove that
    /// nothing is missing (the network always can; the cache only when it says it is complete).
    /// Returns whether the page belongs to the open channel and should be drawn.
    pub fn done(&mut self, generation: u64, empty: bool, vouched: bool) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.retry_at = None;
        if empty && vouched {
            self.at_start = true;
        }
        if !empty {
            self.paged_back = true;
        }
        true
    }

    /// A page failed to come (offline, say): paging may be tried again.
    pub fn failed(&mut self, generation: u64, now: Instant) {
        if generation == self.generation {
            self.loading = false;
            self.retry_at = Some(now + RETRY_AFTER);
        }
    }

    /// Whether an older page has been drawn for the open channel.
    pub fn paged_back(&self) -> bool {
        self.paged_back
    }
}

/// The messages of a page that aren't on screen yet (a page can overlap rows already drawn).
pub fn fresh<T>(page: &[T], id: impl Fn(&T) -> &str, shown: impl Fn(&str) -> bool) -> Vec<&T> {
    page.iter().filter(|m| !shown(id(m))).collect()
}

/// Whether the view is at (or within a few rows of) the bottom of the list.
pub fn near_bottom(value: f64, page_size: f64, upper: f64) -> bool {
    value + page_size >= upper - 50.0
}

/// Whether a new row should scroll the list to the bottom: always for the user's own message
/// and before any older page was drawn; afterwards only when they are already at the bottom,
/// so a live message doesn't pull them away from the history they are reading.
pub fn follows_bottom(own: bool, paged_back: bool, at_bottom: bool) -> bool {
    own || !paged_back || at_bottom
}

/// The oldest message id among those on screen (ids are UUIDv7 in lowercase, so string order is
/// time order), which is where the next older page starts.
pub fn oldest<'a>(shown: impl Iterator<Item = &'a String>) -> Option<String> {
    shown.min().cloned()
}

/// Where the scroll position goes after older rows were put above: the same message stays under
/// the same pixel, so the page doesn't jump. `before` is the adjustment's (upper, value) taken
/// before the rows were inserted; `upper` is its upper bound after layout.
pub fn anchored_value(before: (f64, f64), upper: f64) -> f64 {
    before.1 + (upper - before.0).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        thread_local! { static T: Instant = Instant::now(); }
        T.with(|t| *t)
    }

    fn ready_pager() -> Pager {
        let mut p = Pager::default();
        p.reset();
        let g = p.generation();
        p.ready(g);
        p
    }

    #[test]
    fn nothing_is_asked_before_the_newest_page_is_drawn() {
        let mut p = Pager::default();
        p.reset();
        assert_eq!(p.begin(Some("m5".into()), t0()), None);
        let g = p.generation();
        p.ready(g);
        assert_eq!(p.begin(Some("m5".into()), t0()), Some((g, "m5".into())));
    }

    #[test]
    fn nothing_is_asked_with_no_message_on_screen() {
        let mut p = ready_pager();
        assert_eq!(p.begin(None, t0()), None);
        assert!(
            p.begin(Some("m5".into()), t0()).is_some(),
            "an empty ask doesn't block"
        );
    }

    #[test]
    fn one_page_at_a_time() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into()), t0()).unwrap();
        assert_eq!(p.begin(Some("m5".into()), t0()), None, "still loading");
        assert!(p.done(g, false, true));
        assert!(p.begin(Some("m1".into()), t0()).is_some());
    }

    #[test]
    fn an_empty_vouched_page_is_the_start_of_the_channel() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into()), t0()).unwrap();
        p.done(g, true, true);
        assert_eq!(p.begin(Some("m5".into()), t0()), None);
    }

    #[test]
    fn an_empty_page_nobody_vouched_for_is_not_the_start() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into()), t0()).unwrap();
        p.done(g, true, false);
        assert!(p.begin(Some("m5".into()), t0()).is_some());
    }

    #[test]
    fn a_full_page_is_never_the_start() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into()), t0()).unwrap();
        p.done(g, false, true);
        assert!(p.begin(Some("m1".into()), t0()).is_some());
    }

    #[test]
    fn a_failed_page_can_be_tried_again() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into()), t0()).unwrap();
        p.failed(g, t0());
        assert_eq!(
            p.begin(Some("m5".into()), t0() + Duration::from_secs(1)),
            None,
            "not at once: every scroll tick at the top would retry"
        );
        assert!(p
            .begin(Some("m5".into()), t0() + Duration::from_secs(4))
            .is_some());
    }

    #[test]
    fn a_page_for_a_channel_we_left_is_dropped() {
        let mut p = ready_pager();
        let (old, _) = p.begin(Some("m5".into()), t0()).unwrap();
        p.reset();
        let g = p.generation();
        p.ready(g);
        assert!(!p.done(old, true, true), "not drawn");
        // ...and it neither ended the new channel's history nor freed a page that is not ours.
        assert!(p.begin(Some("n9".into()), t0()).is_some());
        p.failed(old, t0());
        assert_eq!(
            p.begin(Some("n9".into()), t0()),
            None,
            "the new channel's page is still coming"
        );
    }

    #[test]
    fn a_late_ready_for_a_channel_we_left_changes_nothing() {
        let mut p = Pager::default();
        p.reset();
        let old = p.generation();
        p.reset();
        p.ready(old);
        assert_eq!(p.begin(Some("m5".into()), t0()), None);
    }

    #[test]
    fn the_oldest_shown_is_where_the_next_page_starts() {
        let shown: Vec<String> = ["0190a-3", "0190a-1", "0190a-2"].map(String::from).to_vec();
        assert_eq!(oldest(shown.iter()), Some("0190a-1".into()));
        assert_eq!(oldest(Vec::<String>::new().iter()), None);
    }

    #[test]
    fn the_view_stays_on_the_same_message_after_rows_are_added_above() {
        // 1000 px of rows, viewing from 0; 600 px arrive above: the view moves down by 600.
        assert_eq!(anchored_value((1000.0, 0.0), 1600.0), 600.0);
        assert_eq!(anchored_value((1000.0, 40.0), 1600.0), 640.0);
        // Nothing grew (or the layout shrank): never move up.
        assert_eq!(anchored_value((1000.0, 40.0), 1000.0), 40.0);
        assert_eq!(anchored_value((1000.0, 40.0), 900.0), 40.0);
    }

    #[test]
    fn only_rows_not_yet_shown_are_drawn() {
        let page = ["a", "b", "c"];
        let shown = |id: &str| id == "b";
        let fresh = fresh(&page, |m| m, shown);
        assert_eq!(fresh, vec![&"a", &"c"]);
        // A page of nothing new draws nothing, so nothing needs anchoring.
        assert!(super::fresh(&page, |m| m, |_| true).is_empty());
    }

    #[test]
    fn live_messages_leave_a_reader_of_history_alone() {
        // Before any older page: as before, new rows scroll to the bottom.
        assert!(follows_bottom(false, false, false));
        // Reading history: a live message doesn't pull the view away...
        assert!(!follows_bottom(false, true, false));
        // ...unless they are at the bottom already, or it is their own message.
        assert!(follows_bottom(false, true, true));
        assert!(follows_bottom(true, true, false));
    }

    #[test]
    fn the_bottom_has_a_little_slack() {
        assert!(near_bottom(950.0, 100.0, 1000.0));
        assert!(near_bottom(860.0, 100.0, 1000.0), "40 px short is still the bottom");
        assert!(!near_bottom(840.0, 100.0, 1000.0), "60 px short is not");
        assert!(!near_bottom(500.0, 100.0, 1000.0));
    }

    #[test]
    fn a_page_drawn_marks_the_reader_as_in_history() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into()), t0()).unwrap();
        p.done(g, true, true);
        assert!(!p.paged_back(), "an empty page draws nothing");
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into()), t0()).unwrap();
        p.done(g, false, true);
        assert!(p.paged_back());
        p.reset();
        assert!(!p.paged_back());
    }
}
