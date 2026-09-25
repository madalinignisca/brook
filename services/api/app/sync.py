"""Change sequence for /sync (sync spec 2026-09-25 §2): automatic stamping.

Every flush that inserts or changes a message, channel, membership or user stamps
those rows with the transaction's ``seq``, taken once per transaction from the single
``sync_counter`` row. That row's lock is held until commit, so change-writing
transactions commit one at a time and seq order is commit order: a /sync that
returns cursor N can never later miss a change numbered below N. (A Postgres
sequence hands out numbers at nextval time, not commit time, and would.)

It is a flush hook, not a call in each route, so a new write path can't forget it.
Reactions and file deletions re-stamp their message; ended memberships (including a
deleted channel's, which the database cascade removes unseen) leave tombstones.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from sqlalchemy import event, insert, inspect, select, update
from sqlalchemy.orm import Session

from .models import Channel, File, Membership, Message, Reaction, SyncCounter, SyncTombstone, User

log = logging.getLogger(__name__)

_STAMPED = (Message, Channel, Membership, User)
_KEY = "brook_sync_seq"
_TX = "brook_sync_seq_tx"  # the (sub)transaction that took it
_HINT = "brook_sync_hint"  # (channel ids, user ids) whose visible rows changed


def _take_seq(session: Session) -> int:
    """This transaction's seq: one counter bump per transaction, lock held to commit."""
    cached = session.info.get(_KEY)
    if cached is not None:
        return int(cached)
    seq = session.execute(
        update(SyncCounter)
        .where(SyncCounter.id == 1)
        .values(seq=SyncCounter.seq + 1)
        .returning(SyncCounter.seq)
    ).scalar()
    if seq is None:  # a fresh database without the migration's row (tests, create_all)
        session.execute(insert(SyncCounter).values(id=1, seq=2, floor=0))
        seq = 2
    session.info[_KEY] = seq
    session.info[_TX] = session.get_nested_transaction() or session.get_transaction()
    return int(seq)


_PROFILE = ("handle", "display_name", "status")


def _profile_changed(user: User) -> bool:
    """Only what other members can see. A password or session change (password_hash,
    sessions_valid_after, ...) must not move users.seq, or co-members could infer it."""
    state = inspect(user)
    return any(state.attrs[name].history.has_changes() for name in _PROFILE)


@event.listens_for(Session, "before_flush")
def _stamp(session: Session, _ctx: Any, _instances: Any) -> None:
    changed = [
        obj
        for obj in (*session.new, *session.dirty)
        if isinstance(obj, _STAMPED)
        and (obj in session.new or session.is_modified(obj))
        and (not isinstance(obj, User) or obj in session.new or _profile_changed(obj))
    ]
    reactions = [o for o in (*session.new, *session.deleted) if isinstance(o, Reaction)]
    dead_files = [o for o in session.deleted if isinstance(o, File) and o.message_id is not None]
    ended = [o for o in session.deleted if isinstance(o, Membership)]
    dead_channels = [o for o in session.deleted if isinstance(o, Channel)]
    if not (changed or reactions or dead_files or ended or dead_channels):
        return
    seq = _take_seq(session)
    # sync.hint only for changes with no live event of their own. Messages, reactions
    # and channel edits already have one; hinting those too would just make every
    # client run /sync after every message.
    #   channels: membership changed (joined, left): hint every member;
    #   users:    that user only (their read marker, moved on another device);
    #   sharers:  a profile change: everyone who shares a channel with them.
    channels, users, sharers = session.info.setdefault(_HINT, (set(), set(), set()))
    for obj in changed:
        if isinstance(obj, Membership):
            if obj in session.new:
                channels.add(obj.channel_id)
                users.add(obj.user_id)
            else:
                users.add(obj.user_id)  # last_read moved: only their other devices care
        elif isinstance(obj, User) and obj not in session.new:
            sharers.add(obj.id)
    channels.update(m.channel_id for m in ended)
    users.update(m.user_id for m in ended)  # the removed member hears it too
    for obj in changed:
        obj.seq = seq
        if isinstance(obj, Membership) and obj in session.new:
            obj.joined_seq = seq
    # The member list is part of a channel's state: joining or leaving re-stamps it, so
    # channel.update events (and /sync) carry a seq that orders them.
    member_changes = {m.channel_id for m in (*session.new, *ended) if isinstance(m, Membership)}
    if member_changes:
        session.execute(update(Channel).where(Channel.id.in_(member_changes)).values(seq=seq))
    touched = {r.message_id for r in reactions} | {f.message_id for f in dead_files}
    if touched:
        session.execute(update(Message).where(Message.id.in_(touched)).values(seq=seq))
    for m in ended:
        session.add(SyncTombstone(channel_id=m.channel_id, user_id=m.user_id, seq=seq))
    for ch in dead_channels:
        members = (
            session.execute(select(Membership.user_id).where(Membership.channel_id == ch.id))
            .scalars()
            .all()
        )
        for user_id in members:
            session.add(SyncTombstone(channel_id=ch.id, user_id=user_id, seq=seq))
            # channel.delete already tells them live; no hint needed


def transaction_seq(session: Session) -> int:
    """The seq this transaction stamped (after a flush), for events of deleted rows
    that can't be read back after commit. 0 if nothing was stamped."""
    return int(session.info.get(_KEY) or 0)


@event.listens_for(Session, "after_commit")
def _hint_after_commit(session: Session) -> None:
    """After a commit that stamped changes, tell each affected user "run /sync now" (WS
    ``sync.hint``). Most changes also have their own live event, but some don't (added
    to a channel, a member left, a profile change), and a connected client would only
    learn of those on its next reconnect. Fire-and-forget: a commit never waits on it."""
    seq = session.info.get(_KEY)
    hint = session.info.pop(_HINT, None)
    if seq is None or hint is None or not any(hint) or not HINTS_ENABLED:
        return
    try:
        loop = asyncio.get_running_loop()
    except RuntimeError:
        return  # no event loop (the host CLI): nobody is connected to this process
    task = loop.create_task(_send_hints(int(seq), set(hint[0]), set(hint[1]), set(hint[2])))
    _pending.add(task)  # keep a reference until it finishes
    task.add_done_callback(_pending.discard)


_pending: set[asyncio.Task[None]] = set()
# Tests turn hints off by default (tests/conftest.py): a hint from setup can land ahead of
# the frame a test is waiting for. The hint tests turn it back on.
HINTS_ENABLED = True


async def _send_hints(seq: int, channels: set[Any], users: set[Any], sharers: set[Any]) -> None:
    from .db import get_sessionmaker  # local: db imports this module
    from .hub import get_hub

    try:
        async with get_sessionmaker()() as session:
            recipients = set(users)
            if channels:
                recipients |= set(
                    (
                        await session.scalars(
                            select(Membership.user_id).where(Membership.channel_id.in_(channels))
                        )
                    ).all()
                )
            if sharers:
                # A profile change reaches everyone who shares a channel with its owner.
                shared = select(Membership.channel_id).where(Membership.user_id.in_(sharers))
                recipients |= set(
                    (
                        await session.scalars(
                            select(Membership.user_id).where(Membership.channel_id.in_(shared))
                        )
                    ).all()
                )
        if recipients:
            await get_hub().send_to_users(
                list(recipients), {"type": "sync.hint", "data": {"seq": seq}}
            )
    except Exception:  # an optimisation: /sync on reconnect still catches everything up
        log.exception("sync hint failed")


@event.listens_for(Session, "after_transaction_end")
def _forget(session: Session, transaction: Any) -> None:
    if transaction.parent is None:  # the outermost transaction ended: next one bumps again
        session.info.pop(_KEY, None)
        session.info.pop(_TX, None)
        session.info.pop(_HINT, None)


@event.listens_for(Session, "after_soft_rollback")
def _forget_on_rollback(session: Session, previous_transaction: Any) -> None:
    """A rollback of the savepoint that took the seq (or of anything above it) undid the
    counter bump and released its row lock. Reusing the cached number afterwards would
    stamp later writes without the lock, breaking commit order: take a fresh one."""
    taken_in = session.info.get(_TX)
    tx = taken_in
    while tx is not None:
        if tx is previous_transaction:
            session.info.pop(_KEY, None)
            session.info.pop(_TX, None)
            session.info.pop(_HINT, None)
            return
        tx = tx.parent
