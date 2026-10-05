//! Paging back through a channel's history (#248): which older page to ask for, and when the
//! start of the channel has been reached. The Mac's `loadOlder` follows the same rules: one
//! page at a time, the cache first (loading it when it can't vouch for the page), the network
//! when there is no local data, and the start is an empty page that was vouched for.

/// Per-channel paging state. `generation` changes with every channel switch, so a page that
/// arrives after the switch is dropped instead of drawn into the wrong channel.
#[derive(Debug, Default)]
pub struct Pager {
    generation: u64,
    /// The newest page is on screen: older ones can be asked for.
    ready: bool,
    loading: bool,
    at_start: bool,
}

impl Pager {
    /// A different channel was opened: forget everything about the last one.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.ready = false;
        self.loading = false;
        self.at_start = false;
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
    pub fn begin(&mut self, oldest: Option<String>) -> Option<(u64, String)> {
        if !self.ready || self.loading || self.at_start {
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
        if empty && vouched {
            self.at_start = true;
        }
        true
    }

    /// A page failed to come (offline, say): paging may be tried again.
    pub fn failed(&mut self, generation: u64) {
        if generation == self.generation {
            self.loading = false;
        }
    }
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
        assert_eq!(p.begin(Some("m5".into())), None);
        let g = p.generation();
        p.ready(g);
        assert_eq!(p.begin(Some("m5".into())), Some((g, "m5".into())));
    }

    #[test]
    fn nothing_is_asked_with_no_message_on_screen() {
        let mut p = ready_pager();
        assert_eq!(p.begin(None), None);
        assert!(
            p.begin(Some("m5".into())).is_some(),
            "an empty ask doesn't block"
        );
    }

    #[test]
    fn one_page_at_a_time() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into())).unwrap();
        assert_eq!(p.begin(Some("m5".into())), None, "still loading");
        assert!(p.done(g, false, true));
        assert!(p.begin(Some("m1".into())).is_some());
    }

    #[test]
    fn an_empty_vouched_page_is_the_start_of_the_channel() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into())).unwrap();
        p.done(g, true, true);
        assert_eq!(p.begin(Some("m5".into())), None);
    }

    #[test]
    fn an_empty_page_nobody_vouched_for_is_not_the_start() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into())).unwrap();
        p.done(g, true, false);
        assert!(p.begin(Some("m5".into())).is_some());
    }

    #[test]
    fn a_full_page_is_never_the_start() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into())).unwrap();
        p.done(g, false, true);
        assert!(p.begin(Some("m1".into())).is_some());
    }

    #[test]
    fn a_failed_page_can_be_tried_again() {
        let mut p = ready_pager();
        let (g, _) = p.begin(Some("m5".into())).unwrap();
        p.failed(g);
        assert!(p.begin(Some("m5".into())).is_some());
    }

    #[test]
    fn a_page_for_a_channel_we_left_is_dropped() {
        let mut p = ready_pager();
        let (old, _) = p.begin(Some("m5".into())).unwrap();
        p.reset();
        let g = p.generation();
        p.ready(g);
        assert!(!p.done(old, true, true), "not drawn");
        // ...and it neither ended the new channel's history nor freed a page that is not ours.
        assert!(p.begin(Some("n9".into())).is_some());
        p.failed(old);
        assert_eq!(
            p.begin(Some("n9".into())),
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
        assert_eq!(p.begin(Some("m5".into())), None);
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
}
