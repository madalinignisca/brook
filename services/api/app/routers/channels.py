"""Channels, DMs, memberships, and messages.

Sending is REST-only (`POST /channels/{id}/messages`) — the single send path; the
server persists then fans out `message.new` over the WebSocket hub. History is
paginated by the time-sortable message id (`before=`/`after=`).
"""

from __future__ import annotations

import uuid
from datetime import datetime
from typing import Annotated, Any

from fastapi import APIRouter, Depends, HTTPException, Query, status
from fastapi.encoders import jsonable_encoder
from sqlalchemy import and_, func, or_, select
from sqlalchemy.ext.asyncio import AsyncSession
from sqlalchemy.orm import aliased

from ..db import get_session
from ..deps import get_current_user
from ..hub import Hub, get_hub
from ..models import Channel, Membership, Message, User, utcnow
from ..schemas import (
    ChannelCreate,
    ChannelOut,
    MemberAdd,
    MessageCreate,
    MessageOut,
    ReadIn,
    UserSummary,
)

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
        channel = Channel(kind="channel", name=body.name, topic=body.topic, created_by=user.id)
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
    return [_message_out(m, author) for m, author in rows]


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
    await _require_member(session, channel_id, user)

    message = Message(channel_id=channel_id, author_type="user", author_id=user.id, body=body.body)
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

    out = _message_out(message, user)
    member_ids = [m.id for m in await _members(session, channel_id)]
    await hub.send_to_users(member_ids, _envelope("message.new", jsonable_encoder(out)))
    return out


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


def _message_out(message: Message, author: User | None) -> MessageOut:
    return MessageOut(
        id=message.id,
        channel_id=message.channel_id,
        author_id=message.author_id,
        author_handle=author.handle if author else None,
        author_display_name=author.display_name if author else None,
        body=message.body,
        created_at=message.created_at,
        edited_at=message.edited_at,
    )


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
