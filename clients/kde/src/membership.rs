//! Membership rules shared by the KDE UI: who may remove or offer ownership to whom, the
//! profile lengths, which messages mention you, and why an action was refused. The server
//! decides every one of these; the UI only offers what it would allow. They match the GTK
//! client's (`clients/gnome/src/chat.rs`) and the Mac's.

use brook_core::{Error, Message};

/// Whether a viewer is offered Remove on a member (#183): never themselves (that's Leave);
/// a global admin removes anyone; a channel owner removes members but not other owners.
pub fn may_remove(
    admin: bool,
    my_role: Option<&str>,
    their_role: Option<&str>,
    is_me: bool,
) -> bool {
    if is_me {
        return false;
    }
    admin || (my_role == Some("owner") && their_role != Some("owner"))
}

/// Whether a viewer is offered "Make owner…" (or Withdraw, with an offer pending) on a
/// member (#190): an owner or a global admin, beside someone else who isn't an owner yet.
pub fn may_offer(
    admin: bool,
    my_role: Option<&str>,
    their_role: Option<&str>,
    is_me: bool,
) -> bool {
    !is_me && their_role != Some("owner") && (admin || my_role == Some("owner"))
}

/// Whether a profile edit is within the server's lengths (#183): a display name of 1 to 64
/// characters and a status line of at most 100, both trimmed.
pub fn profile_fits(name: &str, status: &str) -> bool {
    let n = name.trim().chars().count();
    (1..=64).contains(&n) && status.trim().chars().count() <= 100
}

/// What a profile edit sends: only the fields that changed (omitted ones stay).
pub fn profile_changes(
    (old_name, old_status): (&str, &str),
    (name, status): (&str, &str),
) -> (Option<String>, Option<String>) {
    (
        (name.trim() != old_name.trim()).then(|| name.to_string()),
        (status.trim() != old_status.trim()).then(|| status.to_string()),
    )
}

/// Whether a message calls for your attention: someone else's, not deleted, naming you or
/// everyone. Mentions are stored with messages (#195), so history carries them too.
pub fn mentions_me(message: &Message, me: &str) -> bool {
    !me.is_empty()
        && message.author_id != me
        && !message.is_deleted()
        && (message.mention_everyone || message.mentions.iter().any(|m| m == me))
}

/// Why leaving, removing, an ownership action or a profile change was refused, briefly.
pub fn error_text(err: &Error) -> String {
    match err {
        Error::Api { code, .. } => match code.as_str() {
            "channel.last_owner" => "The last owner can't leave. Delete the channel instead.",
            "channel.dm" => "A direct message can't be left.",
            "authz.forbidden" => "Only an admin or the channel's owner can do that.",
            "not_found" => "That member isn't in this channel any more.",
            "offer.not_found" => "That offer was already answered or withdrawn.",
            "channel.already_owner" => "They're already an owner.",
            "channel.not_member" => "They aren't a member of this channel.",
            "profile.invalid" => {
                "That name or status can't be used (it's empty, too long, or has invisible characters)."
            }
            _ => "That didn't work. Try again.",
        }
        .into(),
        Error::NotAuthenticated => "You were signed out.".into(),
        _ => "Couldn't reach the server.".into(),
    }
}

/// An error that means the action already happened: leaving a channel you're no longer in,
/// or answering or withdrawing an offer that's gone. Nothing to report.
pub fn already_so(err: &Error, action: Action) -> bool {
    let Error::Api { code, .. } = err else {
        return false;
    };
    match action {
        Action::Leave => code == "not_found",
        Action::Answer | Action::Withdraw => code == "offer.not_found",
        Action::Offer => code == "channel.already_owner",
        Action::Remove | Action::Profile => false,
    }
}

/// A membership action, for `already_so` and the failure's heading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Leave,
    Remove,
    Offer,
    Withdraw,
    Answer,
    Profile,
}

impl Action {
    /// Its name for QML (`action_done`).
    pub fn name(self) -> &'static str {
        match self {
            Action::Leave => "leave",
            Action::Remove => "remove",
            Action::Offer => "offer",
            Action::Withdraw => "withdraw",
            Action::Answer => "answer",
            Action::Profile => "profile",
        }
    }

    /// The heading of the alert when it fails.
    pub fn failed(self) -> &'static str {
        match self {
            Action::Leave => "Couldn't Leave",
            Action::Remove => "Couldn't Remove",
            Action::Offer => "Couldn't Offer",
            Action::Withdraw => "Couldn't Withdraw",
            Action::Answer => "Couldn't Answer",
            Action::Profile => "Couldn't Save Your Profile",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(code: &str) -> Error {
        Error::Api {
            code: code.into(),
            message: String::new(),
        }
    }

    #[test]
    fn remove_follows_the_servers_rules() {
        // An admin removes anyone but themselves.
        assert!(may_remove(true, None, Some("owner"), false));
        assert!(!may_remove(true, None, None, true));
        // An owner removes members, not other owners.
        assert!(may_remove(false, Some("owner"), Some("member"), false));
        assert!(!may_remove(false, Some("owner"), Some("owner"), false));
        // A member removes nobody.
        assert!(!may_remove(false, Some("member"), Some("member"), false));
    }

    #[test]
    fn offers_are_for_owners_and_admins_to_non_owners() {
        assert!(may_offer(false, Some("owner"), Some("member"), false));
        assert!(may_offer(true, Some("member"), Some("member"), false));
        assert!(!may_offer(false, Some("member"), Some("member"), false));
        assert!(!may_offer(true, None, Some("owner"), false));
        assert!(!may_offer(true, Some("owner"), Some("member"), true));
    }

    #[test]
    fn a_profile_fits_the_servers_lengths() {
        assert!(profile_fits("Ana", ""));
        assert!(!profile_fits("  ", ""));
        assert!(profile_fits(&"a".repeat(64), &"s".repeat(100)));
        assert!(!profile_fits(&"a".repeat(65), ""));
        assert!(!profile_fits("Ana", &"s".repeat(101)));
    }

    #[test]
    fn a_profile_edit_sends_only_what_changed() {
        assert_eq!(
            profile_changes(("Ana", "hi"), ("Ana ", "away")),
            (None, Some("away".into()))
        );
        assert_eq!(
            profile_changes(("Ana", ""), ("Ana B", "")),
            (Some("Ana B".into()), None)
        );
        assert_eq!(profile_changes(("Ana", "x"), ("Ana", "x")), (None, None));
    }

    fn message(author: &str, mentions: &[&str], everyone: bool) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": "m1", "channel_id": "c", "author_id": author, "body": "hi",
            "created_at": "2026-09-26T10:00:00Z",
            "mentions": mentions, "mention_everyone": everyone
        }))
        .unwrap()
    }

    #[test]
    fn a_message_mentions_me_by_name_or_everyone_but_never_my_own() {
        assert!(mentions_me(&message("bo", &["me"], false), "me"));
        assert!(mentions_me(&message("bo", &[], true), "me"));
        assert!(!mentions_me(&message("bo", &["someone"], false), "me"));
        assert!(!mentions_me(&message("me", &["me"], true), "me"));
        assert!(!mentions_me(&message("bo", &[], true), ""));
        let mut deleted = message("bo", &["me"], true);
        deleted.deleted_at = Some("2026-09-26T10:01:00Z".into());
        assert!(!mentions_me(&deleted, "me"));
    }

    #[test]
    fn what_already_happened_isnt_reported() {
        assert!(already_so(&api("not_found"), Action::Leave));
        assert!(already_so(&api("offer.not_found"), Action::Answer));
        assert!(already_so(&api("offer.not_found"), Action::Withdraw));
        assert!(already_so(&api("channel.already_owner"), Action::Offer));
        assert!(!already_so(&api("not_found"), Action::Remove));
        assert!(!already_so(&api("channel.last_owner"), Action::Leave));
        assert!(!already_so(&Error::UnexpectedResponse, Action::Leave));
    }

    #[test]
    fn refusals_say_why() {
        assert_eq!(
            error_text(&api("channel.last_owner")),
            "The last owner can't leave. Delete the channel instead."
        );
        assert_eq!(
            error_text(&api("something.new")),
            "That didn't work. Try again."
        );
        assert_eq!(error_text(&Error::NotAuthenticated), "You were signed out.");
    }
}
