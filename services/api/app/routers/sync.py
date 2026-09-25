"""``GET /sync``: what changed since a cursor (sync spec 2026-09-25 §3).

The cursor is the last change sequence the client has (opaque to it). Every row
stamped by app/sync.py after the cursor, in channels the caller belongs to, comes
back with its ``seq``; the client keeps the highest per row. Pages never split a seq:
a transaction's rows arrive together.

Rules the cache relies on:
- ``since=0`` (and after 410 sync.reset) is **state only**: channels, memberships and
  users, no messages, and a cursor at the current seq. History is paged with before=.
- a channel new to the caller (their own membership row is in the page) arrives with
  its full member list and profiles regardless of seq, but its **history is not
  included**: the client back-fills with before=. Don't "fix" that by adding history.
- the current seq is read FIRST and every query is bounded by it, so a page never
  holds a change newer than the cursor it returns.
"""

from __future__ import annotations

import uuid
from collections.abc import Sequence
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException, Query
from pydantic import BaseModel
from sqlalchemy import ColumnElement, Row, select, union
from sqlalchemy.ext.asyncio import AsyncSession
from sqlalchemy.orm import InstrumentedAttribute

from ..db import get_session
from ..deps import get_current_user
from ..models import Channel, Membership, Message, SyncCounter, SyncTombstone, User
from ..schemas import ChannelOut, MessageOut
from .channels import (
    _attachments_for,
    _channel_out,
    _message_out,
    _reactions_for,
    _reply_excerpts,
)

router = APIRouter(tags=["sync"])

MAX_PAGE = 500


class SyncMembership(BaseModel):
    channel_id: uuid.UUID
    user_id: uuid.UUID
    role: str
    seq: int
    last_read_message_id: uuid.UUID | None = None  # the caller's own rows only


class SyncUser(BaseModel):
    id: uuid.UUID
    handle: str
    display_name: str
    status: str
    seq: int


class SyncRemoval(BaseModel):
    channel_id: uuid.UUID
    seq: int


class SyncLeft(BaseModel):
    channel_id: uuid.UUID
    user_id: uuid.UUID
    seq: int


class SyncOut(BaseModel):
    channels: list[ChannelOut]
    removed_channels: list[SyncRemoval]
    memberships: list[SyncMembership]
    left_members: list[SyncLeft]
    users: list[SyncUser]
    messages: list[MessageOut]
    next: str
    more: bool


def _reset() -> HTTPException:
    return HTTPException(
        status_code=410,
        detail={
            "code": "sync.reset",
            "message": "Cursor unknown here; wipe the cache and sync from 0",
        },
    )


@router.get("/sync", response_model=SyncOut)
async def sync(
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    since: Annotated[str, Query(max_length=32)] = "0",
    limit: Annotated[int, Query(ge=1, le=MAX_PAGE)] = MAX_PAGE,
) -> SyncOut:
    if not since.isdigit():
        raise _reset()  # not a cursor we issued
    cursor = int(since)
    counter = await session.get(SyncCounter, 1)
    current = counter.seq if counter is not None else 1
    floor = counter.floor if counter is not None else 0
    if cursor > current or (cursor != 0 and cursor < floor):
        raise _reset()  # a restored or wiped server, or pruned tombstones

    mine = select(Membership.channel_id).where(Membership.user_id == user.id)
    my_channel_ids = set((await session.scalars(mine)).all())

    if cursor == 0:
        return await _state_only(session, user, my_channel_ids, current)

    # The page's upper bound: the `limit`-th distinct seq after the cursor (never
    # splitting one), across everything this caller may see; `current` at most.
    seqs = union(
        select(Channel.seq).where(Channel.id.in_(mine), Channel.seq > cursor),
        select(Membership.seq).where(Membership.channel_id.in_(mine), Membership.seq > cursor),
        select(Message.seq).where(Message.channel_id.in_(mine), Message.seq > cursor),
        select(User.seq).where(
            User.id.in_(select(Membership.user_id).where(Membership.channel_id.in_(mine))),
            User.seq > cursor,
        ),
        select(SyncTombstone.seq).where(
            SyncTombstone.seq > cursor,
            (SyncTombstone.user_id == user.id) | SyncTombstone.channel_id.in_(mine),
        ),
    ).subquery()
    page = list(
        (
            await session.scalars(
                select(seqs.c[0]).where(seqs.c[0] <= current).order_by(seqs.c[0]).limit(limit + 1)
            )
        ).all()
    )
    more = len(page) > limit
    upper = page[limit - 1] if more else current

    def window(col: InstrumentedAttribute[int]) -> ColumnElement[bool]:
        return (col > cursor) & (col <= upper)

    channels = list(
        (
            await session.scalars(select(Channel).where(Channel.id.in_(mine), window(Channel.seq)))
        ).all()
    )
    memberships = list(
        (
            await session.scalars(
                select(Membership).where(Membership.channel_id.in_(mine), window(Membership.seq))
            )
        ).all()
    )
    # A channel new to me: all of its members and the channel itself, whatever their seq.
    new_to_me = {m.channel_id for m in memberships if m.user_id == user.id}
    if new_to_me:
        extra = (
            await session.scalars(select(Membership).where(Membership.channel_id.in_(new_to_me)))
        ).all()
        seen = {(m.channel_id, m.user_id) for m in memberships}
        memberships += [m for m in extra if (m.channel_id, m.user_id) not in seen]
        have = {c.id for c in channels}
        channels += list(
            (await session.scalars(select(Channel).where(Channel.id.in_(new_to_me - have)))).all()
        )
    user_ids_in_page = {m.user_id for m in memberships if m.channel_id in new_to_me}
    users = list(
        (
            await session.scalars(
                select(User).where(
                    User.id.in_(select(Membership.user_id).where(Membership.channel_id.in_(mine))),
                    window(User.seq) | User.id.in_(user_ids_in_page),
                )
            )
        ).all()
    )
    tombs = list(
        (
            await session.scalars(
                select(SyncTombstone).where(
                    window(SyncTombstone.seq),
                    (SyncTombstone.user_id == user.id) | SyncTombstone.channel_id.in_(mine),
                )
            )
        ).all()
    )
    rows = (
        await session.execute(
            select(Message, User)
            .outerjoin(User, User.id == Message.author_id)
            .where(Message.channel_id.in_(mine), window(Message.seq))
            .order_by(Message.seq, Message.id)
        )
    ).all()

    return SyncOut(
        channels=await _channels_out(session, channels),
        removed_channels=[
            SyncRemoval(channel_id=t.channel_id, seq=t.seq)
            for t in tombs
            if t.user_id == user.id and t.channel_id not in my_channel_ids
        ],
        memberships=[_membership_out(m, user.id) for m in memberships],
        left_members=[
            SyncLeft(channel_id=t.channel_id, user_id=t.user_id, seq=t.seq)
            for t in tombs
            if t.user_id != user.id and t.channel_id in my_channel_ids
        ],
        users=[_user_out(u) for u in users],
        messages=await _messages_out(session, rows, user),
        next=str(upper),
        more=more,
    )


async def _state_only(
    session: AsyncSession, user: User, channel_ids: set[uuid.UUID], current: int
) -> SyncOut:
    """since=0: the current state, no messages (history is paged with before=)."""
    channels = list(
        (await session.scalars(select(Channel).where(Channel.id.in_(channel_ids)))).all()
    )
    memberships = list(
        (
            await session.scalars(select(Membership).where(Membership.channel_id.in_(channel_ids)))
        ).all()
    )
    users = list(
        (
            await session.scalars(select(User).where(User.id.in_({m.user_id for m in memberships})))
        ).all()
    )
    return SyncOut(
        channels=await _channels_out(session, channels),
        removed_channels=[],
        memberships=[_membership_out(m, user.id) for m in memberships],
        left_members=[],
        users=[_user_out(u) for u in users],
        messages=[],
        next=str(current),
        more=False,
    )


def _membership_out(m: Membership, me: uuid.UUID) -> SyncMembership:
    return SyncMembership(
        channel_id=m.channel_id,
        user_id=m.user_id,
        role=m.role,
        seq=m.seq,
        # Read state is private: other members' markers never leave the server.
        last_read_message_id=m.last_read_message_id if m.user_id == me else None,
    )


def _user_out(u: User) -> SyncUser:
    return SyncUser(
        id=u.id, handle=u.handle, display_name=u.display_name, status=u.status, seq=u.seq
    )


async def _channels_out(session: AsyncSession, channels: list[Channel]) -> list[ChannelOut]:
    if not channels:
        return []
    ids = [c.id for c in channels]
    pairs = (
        await session.execute(
            select(Membership.channel_id, User)
            .join(User, User.id == Membership.user_id)
            .where(Membership.channel_id.in_(ids))
        )
    ).all()
    members: dict[uuid.UUID, list[User]] = {}
    for channel_id, member in pairs:
        members.setdefault(channel_id, []).append(member)
    return [_channel_out(c, members.get(c.id, [])) for c in channels]


async def _messages_out(
    session: AsyncSession, rows: Sequence[Row[tuple[Message, User]]], user: User
) -> list[MessageOut]:
    if not rows:
        return []
    messages = [m for m, _ in rows]
    excerpts = await _reply_excerpts(session, messages)
    reactions = await _reactions_for(session, [m.id for m in messages], user.id)
    files = await _attachments_for(session, [m.id for m in messages])
    return [
        _message_out(
            m, author, excerpts.get(m.reply_to_id), reactions.get(m.id), attachments=files.get(m.id)
        )
        for m, author in rows
    ]
