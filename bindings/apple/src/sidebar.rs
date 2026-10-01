//! The sidebar's labels and order (`brook_core::sidebar`), for the Mac to call.

use crate::offline::FfiMember;

/// One conversation as the sidebar order sees it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiSidebarEntry {
    pub id: String,
    /// `"dm"` or anything else (a channel).
    pub kind: String,
    pub last_message_id: Option<String>,
    /// When this device last opened it: higher is later.
    pub opened: Option<i64>,
    /// The lowercase label with usernames off: the preference never changes the order.
    pub sort_key: String,
}

impl From<FfiSidebarEntry> for brook_core::SidebarEntry {
    fn from(e: FfiSidebarEntry) -> Self {
        Self {
            id: e.id,
            kind: e.kind,
            last_message_id: e.last_message_id,
            opened: e.opened,
            sort_key: e.sort_key,
        }
    }
}

#[uniffi::export]
pub fn person_label(display_name: String, handle: String, show_usernames: bool) -> String {
    brook_core::person_label(&display_name, &handle, show_usernames)
}

#[uniffi::export]
pub fn conversation_label(
    kind: String,
    name: Option<String>,
    members: Vec<FfiMember>,
    me: String,
    show_usernames: bool,
) -> String {
    let members: Vec<brook_core::ChannelMember> = members
        .into_iter()
        .map(|m| brook_core::ChannelMember {
            id: m.id,
            handle: m.handle,
            display_name: m.display_name,
            role: m.role,
        })
        .collect();
    brook_core::conversation_label(&kind, name.as_deref(), &members, &me, show_usernames)
}

/// The conversation ids in sidebar order.
#[uniffi::export]
pub fn sidebar_order(entries: Vec<FfiSidebarEntry>) -> Vec<String> {
    let entries: Vec<brook_core::SidebarEntry> = entries.into_iter().map(Into::into).collect();
    brook_core::sidebar_order(&entries)
}

#[uniffi::export]
pub fn activity_moves(current: Option<String>, message_id: String) -> bool {
    brook_core::activity_moves(current.as_deref(), &message_id)
}
