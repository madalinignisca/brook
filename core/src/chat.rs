//! Chat domain types: channels, members, and messages (mirror the api's
//! `ChannelOut` / `MessageOut`).

use serde::{Deserialize, Serialize};

/// A member of a channel (lightweight user reference).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChannelMember {
    /// User id.
    pub id: String,
    /// Unique handle.
    pub handle: String,
    /// Display name.
    pub display_name: String,
}

/// A channel or 1:1 DM the user belongs to.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Channel {
    /// Stable channel id.
    pub id: String,
    /// `"dm"` or `"channel"`.
    pub kind: String,
    /// Channel name (DMs have none).
    pub name: Option<String>,
    /// Channel topic.
    pub topic: Option<String>,
    /// Current members.
    pub members: Vec<ChannelMember>,
    /// Unread messages for the current user (server-computed).
    #[serde(default)]
    pub unread_count: i64,
    /// Whether the channel is public (browsable + self-joinable).
    #[serde(default)]
    pub public: bool,
    /// Whether the channel is archived (read-only).
    #[serde(default)]
    pub archived: bool,
}

impl Channel {
    /// Whether this is a 1:1 DM.
    pub fn is_dm(&self) -> bool {
        self.kind == "dm"
    }

    /// A human label for the channel: the channel name, or — for a DM — the
    /// other member's display name (falling back to `self_handle`'s peer).
    pub fn title(&self, self_user_id: &str) -> String {
        if let Some(name) = self.name.as_ref().filter(|n| !n.is_empty()) {
            return name.clone();
        }
        if self.is_dm() {
            if let Some(other) = self.members.iter().find(|m| m.id != self_user_id) {
                return other.display_name.clone();
            }
        }
        // Fallback: a DM with yourself, or an unnamed channel.
        self.members
            .iter()
            .map(|m| m.display_name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A message in a channel.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Message {
    /// UUIDv7 id (time-sortable).
    pub id: String,
    /// The channel this belongs to.
    pub channel_id: String,
    /// Author user id (empty on a tombstone core made from a live delete).
    #[serde(default)]
    pub author_id: String,
    /// Author handle (absent for bots / deleted users).
    pub author_handle: Option<String>,
    /// Author display name (absent for bots / deleted users).
    pub author_display_name: Option<String>,
    /// Message text.
    pub body: String,
    /// ISO-8601 creation timestamp (empty on a tombstone core made from a live delete).
    #[serde(default)]
    pub created_at: String,
    /// ISO-8601 last-edit timestamp, if the message was edited.
    #[serde(default)]
    pub edited_at: Option<String>,
    /// The id of the message this one replies to (quote-reply), if any.
    #[serde(default)]
    pub reply_to_id: Option<String>,
    /// A compact preview of the quoted message, if this is a reply.
    #[serde(default)]
    pub reply_to: Option<ReplyExcerpt>,
    /// Emoji reaction tallies on this message (with the caller's `me` flag).
    #[serde(default)]
    pub reactions: Vec<ReactionSummary>,
    /// Specific user ids mentioned (resolved server-side on send).
    #[serde(default)]
    pub mentions: Vec<String>,
    /// True when `@channel`/`@here` mentioned everyone in the channel.
    #[serde(default)]
    pub mention_everyone: bool,
    /// Attached files (none on a tombstone). Save under `filename`; show `original_name`.
    #[serde(default)]
    pub attachments: Vec<crate::FileInfo>,
    /// The sender's outbox id, echoed by the server: a UI drops its pending bubble once the
    /// message with this `client_id` is in the cache.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Set on a tombstone (the body is then empty).
    #[serde(default)]
    pub deleted_at: Option<String>,
    /// A tombstone core made from a live delete, before the server's own arrives.
    #[serde(default)]
    pub deleted: bool,
}

impl Message {
    /// Deleted: show a tombstone, not the (empty) body.
    pub fn is_deleted(&self) -> bool {
        self.deleted || self.deleted_at.is_some()
    }
}

/// An emoji's reaction tally on a message, plus whether the caller reacted.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReactionSummary {
    /// The emoji.
    pub emoji: String,
    /// How many users reacted with it.
    pub count: i64,
    /// Whether the current user is one of them.
    pub me: bool,
}

/// A compact preview of a quoted message (for rendering quote-replies).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReplyExcerpt {
    /// The quoted message's id.
    pub id: String,
    /// Quoted author handle (absent for bots / deleted users).
    pub author_handle: Option<String>,
    /// Quoted author display name.
    pub author_display_name: Option<String>,
    /// Quoted body (truncated server-side).
    pub body: String,
}
