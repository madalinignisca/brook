// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::chat::ChannelMember;
use crate::sidebar::*;

fn member(id: &str, handle: &str, display_name: &str) -> ChannelMember {
    ChannelMember {
        id: id.into(),
        handle: handle.into(),
        display_name: display_name.into(),
        role: None,
    }
}

fn dm_members() -> Vec<ChannelMember> {
    vec![member("me", "me", "Me"), member("b", "bob", "Bob R")]
}

#[test]
fn channel_label_is_hash_name() {
    for show in [false, true] {
        assert_eq!(
            conversation_label("channel", Some("general"), &dm_members(), "me", show),
            "#general"
        );
    }
}

#[test]
fn dm_label_follows_the_preference() {
    let m = dm_members();
    assert_eq!(conversation_label("dm", None, &m, "me", false), "Bob R");
    assert_eq!(conversation_label("dm", None, &m, "me", true), "@bob");
    assert_eq!(person_label("Bob R", "bob", true), "@bob");
}

#[test]
fn blank_name_falls_back_to_at_handle() {
    for show in [false, true] {
        assert_eq!(person_label("", "bob", show).as_str(), "@bob");
    }
    assert_eq!(person_label("  \t", "bob", false), "@bob");
    assert_eq!(person_label("  Bob R ", "bob", false), "Bob R", "trimmed");
}

#[test]
fn an_empty_name_counts_as_none() {
    assert_eq!(
        conversation_label("dm", Some(""), &dm_members(), "me", false),
        "Bob R"
    );
}

#[test]
fn an_unknown_me_joins_the_members_instead_of_picking_one() {
    assert_eq!(
        conversation_label("dm", None, &dm_members(), "", false),
        "Me, Bob R"
    );
    assert_eq!(
        conversation_label("dm", None, &dm_members(), "", true),
        "@me, @bob"
    );
}

#[test]
fn only_a_dm_picks_the_other_and_your_own_dm_reads_you() {
    let three = vec![
        member("me", "me", "Me"),
        member("b", "bob", "Bob"),
        member("c", "cy", "Cy"),
    ];
    assert_eq!(
        conversation_label("private", None, &three, "me", false),
        "Me, Bob, Cy"
    );
    assert_eq!(
        conversation_label("dm", None, &[member("me", "me", "Me")], "me", false),
        "Me"
    );
}

#[test]
fn no_members_still_reads_direct_message() {
    assert_eq!(
        conversation_label("dm", None, &[], "me", false),
        "Direct message"
    );
}

fn entry(id: &str, kind: &str, last: Option<&str>, opened: Option<i64>, key: &str) -> SidebarEntry {
    SidebarEntry {
        id: id.into(),
        kind: kind.into(),
        last_message_id: last.map(str::to_string),
        opened,
        sort_key: key.into(),
    }
}

#[test]
fn channels_come_before_dms() {
    let order = sidebar_order(&[
        entry("dm", "dm", Some("9"), None, "a"),
        entry("ch", "channel", Some("1"), None, "z"),
        entry("pr", "private", None, None, "z"),
    ]);
    assert_eq!(order, ["ch", "pr", "dm"]);
}

#[test]
fn newest_message_first_and_none_last() {
    let order = sidebar_order(&[
        entry("none", "channel", None, Some(99), "a"),
        entry("old", "channel", Some("1"), None, "b"),
        entry("new", "channel", Some("2"), None, "c"),
    ]);
    assert_eq!(order, ["new", "old", "none"]);
}

#[test]
fn a_tie_goes_to_the_opened_rank_then_the_name_key_then_the_id() {
    let order = sidebar_order(&[
        entry("a", "channel", Some("1"), None, "a"),
        entry("b", "channel", Some("1"), Some(1), "b"),
        entry("c", "channel", Some("1"), Some(2), "c"),
    ]);
    assert_eq!(order, ["c", "b", "a"], "opened later first, unopened last");

    let order = sidebar_order(&[
        entry("1", "channel", Some("1"), Some(1), "beta"),
        entry("2", "channel", Some("1"), Some(1), "alpha"),
    ]);
    assert_eq!(order, ["2", "1"], "name key, A to Z, before the id");

    let order = sidebar_order(&[
        entry("2", "channel", None, None, "same"),
        entry("1", "channel", None, None, "same"),
    ]);
    assert_eq!(order, ["1", "2"], "id last");
}

#[test]
fn activity_moves_only_when_strictly_newer() {
    assert!(activity_moves(None, "a"));
    assert!(activity_moves(Some("a"), "b"));
    assert!(!activity_moves(Some("b"), "b"));
    assert!(!activity_moves(Some("c"), "b"));
}

#[test]
fn the_sort_key_is_the_lowercase_label_and_ignores_the_preference() {
    let m = dm_members();
    // "Bob R" shown, "@bob" with usernames on: the key must be the former, lowercased, and
    // never the latter, so toggling the preference cannot reorder the list.
    assert_eq!(sort_key("dm", None, &m, "me"), "bob r");
    assert_eq!(sort_key("channel", Some("General"), &m, "me"), "#general");
}
