"""Channels, DMs, memberships, and messages.

Sending is REST-only (`POST /channels/{id}/messages`) — the single send path; the
server persists then fans out `message.new` over the WebSocket hub. History is
paginated by the time-sortable message id (`before=`/`after=`).
"""

from __future__ import annotations

import re
import time
import uuid
from datetime import datetime
from typing import Annotated, Any

from fastapi import APIRouter, Depends, HTTPException, Query, status
from fastapi.encoders import jsonable_encoder
from sqlalchemy import and_, case, func, or_, select
from sqlalchemy.ext.asyncio import AsyncSession
from sqlalchemy.orm import aliased

from ..db import get_session
from ..deps import get_current_user
from ..hub import Hub, get_hub
from ..models import Channel, Membership, Message, Reaction, User, utcnow
from ..schemas import (
    ChannelCreate,
    ChannelOut,
    ChannelPatch,
    MemberAdd,
    MessageCreate,
    MessageEdit,
    MessageOut,
    ReactionSummary,
    ReactionToggle,
    ReadIn,
    ReplyExcerpt,
    UserSummary,
)

# Quoted-reply previews are truncated to this many characters.
_REPLY_EXCERPT_LEN = 140

# An @mention token: @ at a boundary, then a handle (the registration charset, not
# ending in punctuation) or @channel / @here.
_MENTION_RE = re.compile(r"(?<![\w@.-])@([A-Za-z0-9_](?:[A-Za-z0-9_.-]*[A-Za-z0-9_])?)")

# Server-side typing throttle: ignore a (user, channel)'s typing signal if it
# fired within this window, so a client can't bypass its debounce and spam fan-out.
_TYPING_THROTTLE_SECONDS = 2.0
_typing_last: dict[tuple[uuid.UUID, uuid.UUID], float] = {}


def _typing_allowed(user_id: uuid.UUID, channel_id: uuid.UUID) -> bool:
    now = time.monotonic()
    key = (user_id, channel_id)
    last = _typing_last.get(key)
    if last is not None and now - last < _TYPING_THROTTLE_SECONDS:
        return False
    _typing_last[key] = now
    # Opportunistic cleanup so the map can't grow without bound.
    if len(_typing_last) > 10_000:
        cutoff = now - 60.0
        for stale in [k for k, t in _typing_last.items() if t < cutoff]:
            del _typing_last[stale]
    return True


router = APIRouter(prefix="/channels", tags=["channels"])

CurrentUser = Annotated[User, Depends(get_current_user)]
Session = Annotated[AsyncSession, Depends(get_session)]
HubDep = Annotated[Hub, Depends(get_hub)]


def _forbidden(message: str) -> HTTPException:
    return HTTPException(
        status_code=status.HTTP_403_FORBIDDEN,
        detail={"code": "authz.forbidden", "message": message},
    )


def _not_found() -> HTTPException:
    return HTTPException(
        status_code=status.HTTP_404_NOT_FOUND,
        detail={"code": "not_found", "message": "Channel not found"},
    )


def _validation(message: str) -> HTTPException:
    return HTTPException(
        status_code=status.HTTP_422_UNPROCESSABLE_ENTITY,
        detail={"code": "validation.error", "message": message},
    )


async def _members(session: AsyncSession, channel_id: uuid.UUID) -> list[User]:
    """The users belonging to a channel."""
    stmt = (
        select(User)
        .join(Membership, Membership.user_id == User.id)
        .where(Membership.channel_id == channel_id)
        .order_by(User.handle)
    )
    return list((await session.scalars(stmt)).all())


def _channel_out(channel: Channel, members: list[User], unread_count: int = 0) -> ChannelOut:
    return ChannelOut(
        id=channel.id,
        kind=channel.kind,
        name=channel.name,
        topic=channel.topic,
        created_by=channel.created_by,
        created_at=channel.created_at,
        members=[UserSummary.model_validate(m) for m in members],
        unread_count=unread_count,
        public=channel.public,
        archived=channel.archived_at is not None,
    )


async def _unread_counts(session: AsyncSession, user_id: uuid.UUID) -> dict[uuid.UUID, int]:
    """Unread count per channel for a user, in a single query (avoids N+1).

    Unread = non-deleted messages newer than the membership's read marker.
    """
    msg = aliased(Message)
    stmt = (
        select(Membership.channel_id, func.count(msg.id))
        .select_from(Membership)
        .outerjoin(
            msg,
            and_(
                msg.channel_id == Membership.channel_id,
                msg.deleted_at.is_(None),
                or_(
                    Membership.last_read_message_id.is_(None),
                    msg.id > Membership.last_read_message_id,
                ),
            ),
        )
        .where(Membership.user_id == user_id)
        .group_by(Membership.channel_id)
    )
    return {channel_id: count for channel_id, count in (await session.execute(stmt)).all()}


async def _latest_message_id(session: AsyncSession, channel_id: uuid.UUID) -> uuid.UUID | None:
    """The newest non-deleted message id in a channel, or None."""
    latest: uuid.UUID | None = await session.scalar(
        select(Message.id)
        .where(Message.channel_id == channel_id, Message.deleted_at.is_(None))
        .order_by(Message.id.desc())
        .limit(1)
    )
    return latest


async def _membership(
    session: AsyncSession, channel_id: uuid.UUID, user_id: uuid.UUID
) -> Membership | None:
    return await session.get(Membership, (channel_id, user_id))


async def _require_member(session: AsyncSession, channel_id: uuid.UUID, user: User) -> Channel:
    """Return the channel, or 404 if it doesn't exist or the user isn't a member.

    404 (not 403) for non-members so channel existence isn't leaked.
    """
    channel = await session.get(Channel, channel_id)
    if channel is None or (await _membership(session, channel_id, user.id)) is None:
        raise _not_found()
    return channel


async def _require_channel_admin(
    session: AsyncSession, channel_id: uuid.UUID, user: User
) -> Channel:
    """The channel, if it exists, is a real channel (not a DM), and the caller is a
    global admin or its owner. 404 if missing, 422 for a DM, 403 otherwise."""
    channel = await session.get(Channel, channel_id)
    if channel is None:
        raise _not_found()
    if channel.kind == "dm":
        raise _validation("DMs cannot be renamed, archived, or deleted")
    caller = await _membership(session, channel_id, user.id)
    if user.global_role != "admin" and (caller is None or caller.role != "owner"):
        raise _forbidden("Only an admin or the channel owner can manage this channel")
    return channel


@router.get("", response_model=list[ChannelOut])
async def list_channels(user: CurrentUser, session: Session) -> list[ChannelOut]:
    """Channels and DMs the caller belongs to, each with the caller's unread count."""
    channels = list(
        (
            await session.scalars(
                select(Channel)
                .join(Membership, Membership.channel_id == Channel.id)
                .where(Membership.user_id == user.id)
                .order_by(Channel.created_at)
            )
        ).all()
    )
    unread = await _unread_counts(session, user.id)
    return [_channel_out(c, await _members(session, c.id), unread.get(c.id, 0)) for c in channels]


@router.get("/public", response_model=list[ChannelOut])
async def list_public_channels(user: CurrentUser, session: Session) -> list[ChannelOut]:
    """Public, non-archived channels the caller hasn't joined yet (to self-join)."""
    joined = select(Membership.channel_id).where(Membership.user_id == user.id)
    channels = list(
        (
            await session.scalars(
                select(Channel)
                .where(
                    Channel.kind == "channel",
                    Channel.public.is_(True),
                    Channel.archived_at.is_(None),
                    Channel.id.not_in(joined),
                )
                .order_by(Channel.name)
            )
        ).all()
    )
    return [_channel_out(c, await _members(session, c.id)) for c in channels]


@router.get("/search", response_model=list[MessageOut])
async def search_messages(
    user: CurrentUser,
    session: Session,
    q: Annotated[str, Query(min_length=1, max_length=128)],
    limit: Annotated[int, Query(ge=1, le=100)] = 50,
) -> list[MessageOut]:
    """Full-text-ish search of message bodies across the caller's channels.

    Phase 1b uses case-insensitive substring match (ILIKE); Postgres FTS is the P2
    upgrade. Results are newest-first and scoped to channels the caller belongs to.
    """
    # Escape LIKE wildcards in the user's term so they're matched literally.
    term = q.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
    stmt = (
        select(Message, User)
        .outerjoin(User, User.id == Message.author_id)
        .join(
            Membership,
            and_(
                Membership.channel_id == Message.channel_id,
                Membership.user_id == user.id,
            ),
        )
        .where(
            Message.deleted_at.is_(None),
            Message.body.ilike(f"%{term}%", escape="\\"),
        )
        .order_by(Message.id.desc())
        .limit(limit)
    )
    rows = (await session.execute(stmt)).all()
    return [_message_out(m, author) for m, author in rows]


@router.post("", response_model=ChannelOut, status_code=status.HTTP_201_CREATED)
async def create_channel(
    body: ChannelCreate, user: CurrentUser, session: Session, hub: HubDep
) -> ChannelOut:
    """Create a channel (admin only) or open/find a 1:1 DM (any member)."""
    if body.kind == "channel":
        if user.global_role != "admin":
            raise _forbidden("Admin role required to create channels")
        if not body.name:
            raise _validation("A channel name is required")
        channel = Channel(
            kind="channel",
            name=body.name,
            topic=body.topic,
            created_by=user.id,
            public=body.public,
        )
        session.add(channel)
        await session.flush()
        session.add(Membership(channel_id=channel.id, user_id=user.id, role="owner"))
        await session.commit()
        await session.refresh(channel)
        return _channel_out(channel, await _members(session, channel.id))

    # kind == "dm"
    if not body.member:
        raise _validation("A member handle is required for a DM")
    other = await session.scalar(select(User).where(User.handle == body.member))
    if other is None or other.id == user.id:
        raise _validation("Unknown or invalid DM recipient")

    # TODO(Phase 1b): find-or-create races under concurrent opens can create
    # duplicate DMs for a pair. Make it canonical with a per-pair unique key (e.g.
    # a sorted-user-id column with a unique constraint) + conflict retry.
    existing = await session.scalar(
        select(Channel)
        .join(Membership, Membership.channel_id == Channel.id)
        .where(Channel.kind == "dm", Membership.user_id == user.id)
        .where(Channel.id.in_(select(Membership.channel_id).where(Membership.user_id == other.id)))
    )
    if existing is not None:
        return _channel_out(existing, await _members(session, existing.id))

    channel = Channel(kind="dm", created_by=user.id)
    session.add(channel)
    await session.flush()
    session.add_all(
        [
            Membership(channel_id=channel.id, user_id=user.id, role="member"),
            Membership(channel_id=channel.id, user_id=other.id, role="member"),
        ]
    )
    await session.commit()
    await session.refresh(channel)
    # Notify the other member so their client shows the new DM live.
    await _emit_channel_update(hub, session, channel)
    return _channel_out(channel, await _members(session, channel.id))


@router.post("/{channel_id}/members", status_code=status.HTTP_204_NO_CONTENT)
async def add_member(
    channel_id: uuid.UUID, body: MemberAdd, user: CurrentUser, session: Session, hub: HubDep
) -> None:
    """Add a member to a channel. Caller must be a global admin or the channel owner.

    Note: a global admin may add members to a channel they have not joined, so this
    does NOT require the caller to be a member (unlike read/post).
    """
    channel = await session.get(Channel, channel_id)
    if channel is None:
        raise _not_found()
    if channel.kind == "dm":
        raise _validation("Cannot add members to a DM")
    caller = await _membership(session, channel_id, user.id)
    if user.global_role != "admin" and (caller is None or caller.role != "owner"):
        raise _forbidden("Only an admin or the channel owner can add members")

    target = await session.scalar(select(User).where(User.handle == body.handle))
    if target is None:
        raise _validation("Unknown user")
    if await _membership(session, channel_id, target.id) is None:
        # Start the new member at the channel's latest message, so they aren't
        # flooded with all prior history counted as unread.
        last_read = await _latest_message_id(session, channel_id)
        session.add(
            Membership(
                channel_id=channel_id,
                user_id=target.id,
                role="member",
                last_read_message_id=last_read,
            )
        )
        await session.commit()
        # Fan out so the new member (and existing ones) refresh their channel list live.
        await _emit_channel_update(hub, session, channel)


@router.patch("/{channel_id}", response_model=ChannelOut)
async def update_channel(
    channel_id: uuid.UUID, body: ChannelPatch, user: CurrentUser, session: Session, hub: HubDep
) -> ChannelOut:
    """Rename / retopic / archive a channel (admin or owner). Fans `channel.update`."""
    channel = await _require_channel_admin(session, channel_id, user)
    if body.name is not None:
        if not body.name.strip():
            raise _validation("A channel name cannot be empty")
        channel.name = body.name
    if body.topic is not None:
        channel.topic = body.topic
    if body.archived is not None:
        channel.archived_at = utcnow() if body.archived else None
    await session.commit()
    await session.refresh(channel)

    members = await _members(session, channel_id)
    await _emit_channel_update(hub, session, channel)
    return _channel_out(channel, members)


@router.delete("/{channel_id}", status_code=status.HTTP_204_NO_CONTENT)
async def delete_channel(
    channel_id: uuid.UUID, user: CurrentUser, session: Session, hub: HubDep
) -> None:
    """Delete a channel and its history (admin or owner). Fans `channel.delete`."""
    channel = await _require_channel_admin(session, channel_id, user)
    member_ids = [m.id for m in await _members(session, channel_id)]
    await session.delete(channel)  # cascades to memberships, messages, reactions
    await session.commit()
    await hub.send_to_users(member_ids, _envelope("channel.delete", {"id": str(channel_id)}))


@router.post("/{channel_id}/join", response_model=ChannelOut)
async def join_channel(
    channel_id: uuid.UUID, user: CurrentUser, session: Session, hub: HubDep
) -> ChannelOut:
    """Self-join a public, non-archived channel. Fans `channel.update`."""
    channel = await session.get(Channel, channel_id)
    if (
        channel is None
        or channel.kind != "channel"
        or not channel.public
        or channel.archived_at is not None
    ):
        raise _not_found()
    if await _membership(session, channel_id, user.id) is None:
        last_read = await _latest_message_id(session, channel_id)
        session.add(
            Membership(
                channel_id=channel_id,
                user_id=user.id,
                role="member",
                last_read_message_id=last_read,
            )
        )
        await session.commit()
        await _emit_channel_update(hub, session, channel)
    return _channel_out(channel, await _members(session, channel_id))


async def _emit_channel_update(hub: Hub, session: AsyncSession, channel: Channel) -> None:
    """Broadcast a `channel.update` to a channel's members (membership/metadata changed)."""
    members = await _members(session, channel.id)
    out = _channel_out(channel, members)
    member_ids = [m.id for m in members]
    await hub.send_to_users(member_ids, _envelope("channel.update", jsonable_encoder(out)))


@router.get("/{channel_id}/messages", response_model=list[MessageOut])
async def history(
    channel_id: uuid.UUID,
    user: CurrentUser,
    session: Session,
    before: Annotated[uuid.UUID | None, Query()] = None,
    after: Annotated[uuid.UUID | None, Query()] = None,
    limit: Annotated[int, Query(ge=1, le=100)] = 50,
) -> list[MessageOut]:
    """Channel history, oldest→newest. `before=<id>` back-paginates; `after=<id>`
    forward-syncs missed messages on reconnect."""
    await _require_member(session, channel_id, user)

    # outerjoin (not join): author_id has no FK (bots / deleted users), so an
    # inner join would silently drop messages whose author row is absent.
    stmt = (
        select(Message, User)
        .outerjoin(User, User.id == Message.author_id)
        .where(Message.channel_id == channel_id, Message.deleted_at.is_(None))
    )
    if after is not None:
        stmt = stmt.where(Message.id > after).order_by(Message.id.asc()).limit(limit)
    else:
        if before is not None:
            stmt = stmt.where(Message.id < before)
        # fetch the newest `limit`, then present oldest→newest
        stmt = stmt.order_by(Message.id.desc()).limit(limit)

    rows = list((await session.execute(stmt)).all())
    if after is None:
        rows.reverse()
    messages = [m for m, _ in rows]
    excerpts = await _reply_excerpts(session, messages)
    reactions = await _reactions_for(session, [m.id for m in messages], user.id)
    # Mentions are resolved only on the live send (they drive notifications); not
    # recomputed per history read (that would mis-resolve against today's membership).
    return [
        _message_out(m, author, excerpts.get(m.reply_to_id), reactions.get(m.id))
        for m, author in rows
    ]


@router.post(
    "/{channel_id}/messages",
    response_model=MessageOut,
    status_code=status.HTTP_201_CREATED,
)
async def send_message(
    channel_id: uuid.UUID,
    body: MessageCreate,
    user: CurrentUser,
    session: Session,
    hub: HubDep,
) -> MessageOut:
    """Persist a message then fan it out to the channel's members over the WS."""
    channel = await _require_member(session, channel_id, user)
    if channel.archived_at is not None:
        raise _forbidden("This channel is archived")

    # Quote-reply: the target must be a live message in this same channel.
    reply: ReplyExcerpt | None = None
    if body.reply_to_id is not None:
        quoted = await _get_message(session, channel_id, body.reply_to_id)
        reply = _excerpt(quoted, await session.get(User, quoted.author_id))

    message = Message(
        channel_id=channel_id,
        author_type="user",
        author_id=user.id,
        body=body.body,
        reply_to_id=body.reply_to_id,
    )
    session.add(message)
    await session.flush()
    # Sending implicitly reads the channel up to your own message — but only ever
    # advance the marker (don't rewind past a newer message read concurrently).
    membership = await _membership(session, channel_id, user.id)
    if membership is not None and (
        membership.last_read_message_id is None or message.id > membership.last_read_message_id
    ):
        membership.last_read_message_id = message.id
    await session.commit()
    await session.refresh(message)

    members = await _members(session, channel_id)
    mentions, everyone = _mentions_in(message.body, members)
    out = _message_out(message, user, reply, mentions=mentions, mention_everyone=everyone)
    member_ids = [m.id for m in members]
    await hub.send_to_users(member_ids, _envelope("message.new", jsonable_encoder(out)))
    return out


async def _get_message(
    session: AsyncSession, channel_id: uuid.UUID, message_id: uuid.UUID
) -> Message:
    """A live (non-deleted) message in the channel, or 404."""
    message = await session.get(Message, message_id)
    if message is None or message.channel_id != channel_id or message.deleted_at is not None:
        raise _not_found()
    return message


@router.patch("/{channel_id}/messages/{message_id}", response_model=MessageOut)
async def edit_message(
    channel_id: uuid.UUID,
    message_id: uuid.UUID,
    body: MessageEdit,
    user: CurrentUser,
    session: Session,
    hub: HubDep,
) -> MessageOut:
    """Edit a message's body (author only), then fan out `message.update`."""
    await _require_member(session, channel_id, user)
    message = await _get_message(session, channel_id, message_id)
    if message.author_id != user.id:
        raise _forbidden("Only the author can edit a message")

    message.body = body.body
    message.edited_at = utcnow()
    await session.commit()
    await session.refresh(message)

    reply: ReplyExcerpt | None = None
    if message.reply_to_id is not None:
        quoted = await session.get(Message, message.reply_to_id)
        if quoted is not None:
            reply = _excerpt(quoted, await session.get(User, quoted.author_id))
    reactions = (await _reactions_for(session, [message_id], user.id)).get(message_id, [])

    # Edits don't re-resolve/re-notify mentions (mentions fire on the original send).
    out = _message_out(message, user, reply, reactions)
    member_ids = [m.id for m in await _members(session, channel_id)]
    await hub.send_to_users(member_ids, _envelope("message.update", jsonable_encoder(out)))
    return out


@router.delete("/{channel_id}/messages/{message_id}", status_code=status.HTTP_204_NO_CONTENT)
async def delete_message(
    channel_id: uuid.UUID,
    message_id: uuid.UUID,
    user: CurrentUser,
    session: Session,
    hub: HubDep,
) -> None:
    """Soft-delete a message (author or a global admin), then fan out `message.delete`."""
    await _require_member(session, channel_id, user)
    message = await _get_message(session, channel_id, message_id)
    if message.author_id != user.id and user.global_role != "admin":
        raise _forbidden("Only the author or an admin can delete a message")

    message.deleted_at = utcnow()
    await session.commit()

    member_ids = [m.id for m in await _members(session, channel_id)]
    await hub.send_to_users(
        member_ids,
        _envelope("message.delete", {"id": str(message_id), "channel_id": str(channel_id)}),
    )


@router.post("/{channel_id}/typing", status_code=status.HTTP_204_NO_CONTENT)
async def typing(channel_id: uuid.UUID, user: CurrentUser, session: Session, hub: HubDep) -> None:
    """Signal that the caller is typing; fan an ephemeral `typing` event to the
    channel's other members (no persistence)."""
    # Throttle first so a spamming client can't force the member query + fan-out.
    if not _typing_allowed(user.id, channel_id):
        return
    await _require_member(session, channel_id, user)
    others = [m.id for m in await _members(session, channel_id) if m.id != user.id]
    await hub.send_to_users(
        others,
        _envelope(
            "typing",
            {
                "channel_id": str(channel_id),
                "user_id": str(user.id),
                "display_name": user.display_name,
            },
        ),
    )


@router.post(
    "/{channel_id}/messages/{message_id}/reactions",
    response_model=list[ReactionSummary],
)
async def toggle_reaction(
    channel_id: uuid.UUID,
    message_id: uuid.UUID,
    body: ReactionToggle,
    user: CurrentUser,
    session: Session,
    hub: HubDep,
) -> list[ReactionSummary]:
    """Toggle the caller's emoji reaction on a message; fan out the change."""
    await _require_member(session, channel_id, user)
    await _get_message(session, channel_id, message_id)  # 404 if not a live message here

    existing = await session.get(Reaction, (message_id, user.id, body.emoji))
    if existing is None:
        session.add(Reaction(message_id=message_id, user_id=user.id, emoji=body.emoji))
        added = True
    else:
        await session.delete(existing)
        added = False
    await session.commit()

    count = (
        await session.scalar(
            select(func.count())
            .select_from(Reaction)
            .where(Reaction.message_id == message_id, Reaction.emoji == body.emoji)
        )
    ) or 0

    # The `me` flag is per-recipient, so fan an incremental change (who/what/count),
    # not a full summary; each client adjusts its own view.
    member_ids = [m.id for m in await _members(session, channel_id)]
    await hub.send_to_users(
        member_ids,
        _envelope(
            "reaction.update",
            {
                "message_id": str(message_id),
                "channel_id": str(channel_id),
                "emoji": body.emoji,
                "user_id": str(user.id),
                "added": added,
                "count": count,
            },
        ),
    )

    summary = await _reactions_for(session, [message_id], user.id)
    return summary.get(message_id, [])


@router.post("/{channel_id}/read", status_code=status.HTTP_204_NO_CONTENT)
async def mark_read(
    channel_id: uuid.UUID, body: ReadIn, user: CurrentUser, session: Session
) -> None:
    """Advance the caller's read marker (to ``message_id``, or the channel's latest)."""
    membership = await _membership(session, channel_id, user.id)
    if membership is None:
        raise _not_found()

    target = body.message_id
    if target is None:
        target = await _latest_message_id(session, channel_id)
    else:
        # Reject a forged/foreign id — otherwise a client could set a future marker
        # and permanently suppress its own unread count.
        valid = await session.scalar(
            select(Message.id).where(
                Message.id == target,
                Message.channel_id == channel_id,
                Message.deleted_at.is_(None),
            )
        )
        if valid is None:
            raise _validation("Unknown message for this channel")
    if target is None:
        return  # no messages to mark

    # Only ever advance the marker (UUIDv7 ids are time-sortable).
    if membership.last_read_message_id is None or target > membership.last_read_message_id:
        membership.last_read_message_id = target
        await session.commit()


def _mentions_in(body: str, members: list[User]) -> tuple[list[uuid.UUID], bool]:
    """Resolve a body's mentions to (specific member ids, everyone?).

    `@handle` is matched case-sensitively (handles are case-sensitive-unique);
    `@channel`/`@here` set the everyone flag rather than listing all member ids.
    """
    tokens = {m.group(1) for m in _MENTION_RE.finditer(body)}
    if not tokens:
        return [], False
    everyone = bool({"channel", "here"} & {t.lower() for t in tokens})
    by_handle = {m.handle: m.id for m in members}
    specific = [by_handle[t] for t in tokens if t in by_handle]
    return specific, everyone


def _message_out(
    message: Message,
    author: User | None,
    reply: ReplyExcerpt | None = None,
    reactions: list[ReactionSummary] | None = None,
    mentions: list[uuid.UUID] | None = None,
    mention_everyone: bool = False,
) -> MessageOut:
    return MessageOut(
        id=message.id,
        channel_id=message.channel_id,
        author_id=message.author_id,
        author_handle=author.handle if author else None,
        author_display_name=author.display_name if author else None,
        body=message.body,
        created_at=message.created_at,
        edited_at=message.edited_at,
        reply_to_id=message.reply_to_id,
        reply_to=reply,
        reactions=reactions or [],
        mentions=mentions or [],
        mention_everyone=mention_everyone,
    )


async def _reactions_for(
    session: AsyncSession, message_ids: list[uuid.UUID], user_id: uuid.UUID
) -> dict[uuid.UUID, list[ReactionSummary]]:
    """Reaction tallies per message (with the caller's `me` flag), in one query."""
    if not message_ids:
        return {}
    rows = (
        await session.execute(
            select(
                Reaction.message_id,
                Reaction.emoji,
                func.count().label("count"),
                func.max(case((Reaction.user_id == user_id, 1), else_=0)).label("me"),
            )
            .where(Reaction.message_id.in_(message_ids))
            .group_by(Reaction.message_id, Reaction.emoji)
            .order_by(Reaction.emoji)
        )
    ).all()
    out: dict[uuid.UUID, list[ReactionSummary]] = {}
    for message_id, emoji, count, me in rows:
        out.setdefault(message_id, []).append(
            ReactionSummary(emoji=emoji, count=count, me=bool(me))
        )
    return out


def _excerpt(message: Message, author: User | None) -> ReplyExcerpt:
    """A compact, truncated preview of a quoted message."""
    body = message.body if message.deleted_at is None else "(deleted)"
    return ReplyExcerpt(
        id=message.id,
        author_handle=author.handle if author else None,
        author_display_name=author.display_name if author else None,
        body=body[:_REPLY_EXCERPT_LEN],
    )


async def _reply_excerpts(
    session: AsyncSession, messages: list[Message]
) -> dict[uuid.UUID, ReplyExcerpt]:
    """Resolve previews of the quoted messages for a batch, in one query (no N+1)."""
    ids = {m.reply_to_id for m in messages if m.reply_to_id is not None}
    if not ids:
        return {}
    rows = (
        await session.execute(
            select(Message, User)
            .outerjoin(User, User.id == Message.author_id)
            .where(Message.id.in_(ids))
        )
    ).all()
    return {msg.id: _excerpt(msg, author) for msg, author in rows}


def _envelope(event_type: str, data: Any) -> dict[str, Any]:
    """A tagged realtime envelope (PROTOCOL.md §2)."""
    return {
        "type": event_type,
        "id": str(uuid.uuid4()),
        "ts": _isoformat(utcnow()),
        "data": data,
    }


def _isoformat(value: datetime) -> str:
    return value.isoformat()
