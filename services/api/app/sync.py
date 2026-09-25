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

from typing import Any

from sqlalchemy import event, insert, select, update
from sqlalchemy.orm import Session

from .models import Channel, File, Membership, Message, Reaction, SyncCounter, SyncTombstone, User

_STAMPED = (Message, Channel, Membership, User)
_KEY = "brook_sync_seq"


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
    return int(seq)


@event.listens_for(Session, "before_flush")
def _stamp(session: Session, _ctx: Any, _instances: Any) -> None:
    changed = [
        obj
        for obj in (*session.new, *session.dirty)
        if isinstance(obj, _STAMPED) and (obj in session.new or session.is_modified(obj))
    ]
    reactions = [o for o in (*session.new, *session.deleted) if isinstance(o, Reaction)]
    dead_files = [o for o in session.deleted if isinstance(o, File) and o.message_id is not None]
    ended = [o for o in session.deleted if isinstance(o, Membership)]
    dead_channels = [o for o in session.deleted if isinstance(o, Channel)]
    if not (changed or reactions or dead_files or ended or dead_channels):
        return
    seq = _take_seq(session)
    for obj in changed:
        obj.seq = seq
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


def transaction_seq(session: Session) -> int:
    """The seq this transaction stamped (after a flush), for events of deleted rows
    that can't be read back after commit. 0 if nothing was stamped."""
    return int(session.info.get(_KEY) or 0)


@event.listens_for(Session, "after_transaction_end")
def _forget(session: Session, transaction: Any) -> None:
    if transaction.parent is None:  # the outermost transaction ended: next one bumps again
        session.info.pop(_KEY, None)
