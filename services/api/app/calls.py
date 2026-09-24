"""Call signaling: PROTOCOL.md §3 on top of Janus VideoRoom.

Model (contract §3.1): one call per channel, authorized by membership. Each
participant has one Janus session with two VideoRoom handles:
- publish: joined as ``publisher``; the client offers, we ``configure``, Janus answers.
- subscribe: joined as ``subscriber`` to every *other* published feed; Janus offers,
  the client answers (``start``); renegotiated with ``update`` on roster changes.

Subscription design: we do not react to Janus's own roster notifications. After
any change (join, publish, leave) we recompute each participant's *desired* feed
set and reconcile it against what their subscribe handle has. That keeps a single
source of truth (``Call.participants``). A reconcile requested while an offer is
still unanswered is deferred and replayed once the answer arrives, so each
subscribe PC has at most one offer in flight (contract: no glare).
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import secrets
import uuid
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any

from sqlalchemy import select

from .config import get_settings
from .db import get_sessionmaker
from .hub import get_hub
from .janus import JanusClient, JanusError
from .models import Channel, Membership, User
from .routers.ws import (
    Connection,
    connect_hooks,
    disconnect_hooks,
    envelope,
    error_frame,
    handles,
)

log = logging.getLogger(__name__)

MAX_PARTICIPANTS = 8  # contract §3.6
VIDEO_BITRATE_BPS = 1_500_000
RESUME_GRACE_S = 30.0  # contract §3.5


class CallError(Exception):
    """A protocol-level refusal, sent to the caller as an ``error`` frame."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


@dataclass(eq=False)
class Participant:
    participant_id: str
    user_id: uuid.UUID
    display_name: str
    call: Call
    conn: Connection | None
    sid: int = 0  # Janus session (both handles live in it)
    pub_hid: int = 0
    feed_id: int = 0  # Janus publisher id
    private_id: int = 0
    publishing: list[dict[str, str]] = field(default_factory=list)
    # True once Janus reports this publish PC up ("webrtcup"). Janus refuses to
    # subscribe anyone to a feed before that ("No such feed"), so a participant only
    # counts as a subscribable publisher from then on; "hangup" clears it.
    media_up: bool = False
    # Contract §3.4: false until published; then set from what the publish offer
    # sends, and changed only by call.media afterwards.
    audio: bool = False
    video: bool = False
    # Proves "this device is that participant" on call.resume (contract §3.5). A
    # user may be in one call from several devices, so user_id alone would let one
    # device take over another's participant. Rotated on every resume.
    resume_token: str = field(default_factory=lambda: secrets.token_urlsafe(24))
    # The token the client last *used* also stays valid (one step of lookback):
    # if the call.joined carrying the new token is lost to a second drop, the
    # client still holds only the old one and must not lose the call over it.
    prev_resume_token: str | None = None
    sub_hid: int | None = None
    sub_feeds: set[int] = field(default_factory=set)
    sub_version: int = 0
    sub_pending: dict[str, Any] | None = None  # the unanswered offer frame, if any
    sub_dirty: bool = False
    grace: asyncio.Task[None] | None = None
    lock: asyncio.Lock = field(default_factory=asyncio.Lock)

    def view(self) -> dict[str, Any]:
        return {
            "participant_id": self.participant_id,
            "user_id": str(self.user_id),
            "display_name": self.display_name,
            "audio": self.audio,
            "video": self.video,
            "publishing": self.publishing,
        }

    async def send(self, frame: dict[str, Any]) -> None:
        if self.conn is not None:
            await self.conn.send(frame)


@dataclass(eq=False)
class Call:
    call_id: str
    channel_id: uuid.UUID
    room: int
    admin_sid: int
    admin_hid: int
    participants: dict[str, Participant] = field(default_factory=dict)


class CallManager:
    def __init__(self) -> None:
        self._janus: JanusClient | None = None
        self.by_channel: dict[uuid.UUID, Call] = {}
        self.by_id: dict[str, Call] = {}
        self._channel_locks: dict[uuid.UUID, asyncio.Lock] = {}
        self._tasks: set[asyncio.Task[None]] = set()

    # ---- plumbing -------------------------------------------------------------

    def janus(self) -> JanusClient:
        if self._janus is None:
            s = get_settings()
            if not s.janus_url:
                raise CallError("sfu_unavailable", "no SFU configured")
            self._janus = JanusClient(s.janus_url, s.janus_api_secret)
            self._janus.on_disconnect = self._on_janus_lost
        return self._janus

    def _spawn(self, coro: Any) -> None:
        """Run work in the background. Used for everything that happens *after* a
        command's reply, so a failure there is logged instead of turning into a
        second reply to the same command (contract §3.2: exactly one reply)."""

        async def guarded() -> None:
            try:
                await coro
            except Exception:
                log.exception("background call task failed")

        # Keep a reference: an unreferenced task can be garbage-collected mid-flight.
        task: asyncio.Task[None] = asyncio.create_task(guarded())
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)

    def _lock(self, channel_id: uuid.UUID) -> asyncio.Lock:
        return self._channel_locks.setdefault(channel_id, asyncio.Lock())

    def _participant_of(self, conn: Connection, call_id: Any) -> Participant:
        call = self.by_id.get(call_id) if isinstance(call_id, str) else None
        if call is not None:
            for p in call.participants.values():
                if p.conn is conn:
                    return p
        raise CallError("not_in_call", "not in that call")

    async def _members(self, channel_id: uuid.UUID) -> set[uuid.UUID]:
        async with get_sessionmaker()() as session:
            rows = await session.scalars(
                select(Membership.user_id).where(Membership.channel_id == channel_id)
            )
            return set(rows)

    async def _announce_channel_call(self, channel_id: uuid.UUID) -> None:
        call = self.by_channel.get(channel_id)
        data = {
            "channel_id": str(channel_id),
            "call_id": call.call_id if call else None,
            "participant_count": len(call.participants) if call else 0,
        }
        await get_hub().send_to_users(
            list(await self._members(channel_id)), envelope("channel.call", data)
        )

    async def _broadcast(self, call: Call, frame: dict[str, Any], skip: Participant) -> None:
        for p in list(call.participants.values()):
            if p is not skip:
                await p.send(frame)

    # ---- join / leave ---------------------------------------------------------

    async def join(self, conn: Connection, channel_id: uuid.UUID) -> tuple[Call, Participant]:
        # Membership is checked here, once per join, by design. Losing membership
        # later must end the call explicitly: channel delete calls end_channel(); any
        # future member-removal or user-deactivation path must call end_for_user().
        async with get_sessionmaker()() as session:
            member = await session.get(Membership, (channel_id, conn.user_id))
            user = await session.get(User, conn.user_id)
            channel = await session.get(Channel, channel_id)
        if member is None or user is None or channel is None:
            raise CallError("not_member", "not a member of that channel")
        if channel.archived_at is not None:
            # Same rule as messages: an archived channel is read-only.
            raise CallError("bad_state", "channel is archived")
        janus = self.janus()
        async with self._lock(channel_id):
            call = self.by_channel.get(channel_id)
            if call is not None:
                # One participant per socket per call: several *devices* of one user
                # may join (contract §3.1), but one socket joining repeatedly could
                # fill the call, and its extra participants would be unreachable
                # (commands resolve the participant by socket).
                if any(o.conn is conn for o in call.participants.values()):
                    raise CallError("bad_state", "this connection is already in the call")
                if len(call.participants) >= MAX_PARTICIPANTS:
                    raise CallError("call_full", f"calls are limited to {MAX_PARTICIPANTS}")
            else:
                call = await self._create_call(janus, channel_id)
            p = Participant(
                participant_id=str(uuid.uuid4()),
                user_id=conn.user_id,
                display_name=user.display_name,
                call=call,
                conn=conn,
            )
            try:
                p.sid = await janus.create_session()
                p.pub_hid = await janus.attach(p.sid, self._pub_listener(p))
                joined = await janus.message(
                    p.sid,
                    p.pub_hid,
                    {
                        "request": "join",
                        "ptype": "publisher",
                        "room": call.room,
                        "display": p.participant_id,
                    },
                )
                data = joined["plugindata"]["data"]
                p.feed_id, p.private_id = int(data["id"]), int(data["private_id"])
            except (JanusError, KeyError, ValueError) as exc:
                log.warning("SFU refused a join in %s: %s", channel_id, exc)
                if p.pub_hid:
                    janus.detach_listener(p.pub_hid)  # else the closure pins p and conn
                if p.sid:
                    await janus.destroy_session(p.sid)
                if not call.participants:
                    await self._destroy_call(call)
                raise CallError("sfu_unavailable", "the SFU refused the join") from exc
            call.participants[p.participant_id] = p
        return call, p

    async def _create_call(self, janus: JanusClient, channel_id: uuid.UUID) -> Call:
        sid = 0
        try:
            sid = await janus.create_session()
            hid = await janus.attach(sid)
            room = secrets.randbelow(2**52) + 1  # random: room ids must not be guessable
            await janus.message(
                sid,
                hid,
                {
                    "request": "create",
                    "room": room,
                    "is_private": True,
                    "publishers": MAX_PARTICIPANTS,
                    "bitrate": VIDEO_BITRATE_BPS,
                    "bitrate_cap": True,
                    "videocodec": "h264,vp8",
                    "h264_profile": "42e01f",
                    "audiocodec": "opus",
                    "notify_joining": False,
                },
            )
        except (JanusError, KeyError) as exc:
            # create_session() starts a keepalive: without this, a failed room create
            # leaked a Janus session kept alive forever by nobody's keepalive task.
            if sid:
                await janus.destroy_session(sid)
            log.warning("SFU refused a room for %s: %s", channel_id, exc)
            raise CallError("sfu_unavailable", "the SFU refused the room") from exc
        call = Call(
            call_id=str(uuid.uuid4()),
            channel_id=channel_id,
            room=room,
            admin_sid=sid,
            admin_hid=hid,
        )
        self.by_channel[channel_id] = call
        self.by_id[call.call_id] = call
        return call

    async def _destroy_call(self, call: Call) -> None:
        self.by_channel.pop(call.channel_id, None)
        self.by_id.pop(call.call_id, None)
        if self._janus is not None:
            with contextlib.suppress(JanusError):
                await self._janus.message(
                    call.admin_sid, call.admin_hid, {"request": "destroy", "room": call.room}
                )
            await self._janus.destroy_session(call.admin_sid)

    async def remove(self, p: Participant, only_if_detached: bool = False) -> None:
        """Remove a participant. ``only_if_detached`` (grace expiry) aborts if the
        participant was resumed while this removal waited for the channel lock:
        otherwise a client could get a successful call.joined and then be torn
        down silently (the "left" broadcast skips the participant itself)."""
        call = p.call
        async with self._lock(call.channel_id):
            if only_if_detached and p.conn is not None:
                return
            if call.participants.pop(p.participant_id, None) is None:
                return
            if p.grace is not None:
                p.grace.cancel()
            # Under p.lock: never destroy the session under an in-flight reconcile or
            # subscribe answer that is still waiting for Janus on it.
            async with p.lock:
                if self._janus is not None:
                    self._janus.detach_listener(p.pub_hid)
                    if p.sub_hid is not None:
                        self._janus.detach_listener(p.sub_hid)
                    await self._janus.destroy_session(p.sid)  # drops both handles
            left = envelope(
                "call.participant",
                {"call_id": call.call_id, "event": "left", "participant": p.view()},
            )
            await self._broadcast(call, left, skip=p)
            if not call.participants:
                await self._destroy_call(call)
        await self._announce_channel_call(call.channel_id)
        self._reconcile_all(call)

    # ---- publish --------------------------------------------------------------

    async def publish(self, p: Participant, sdp: str) -> str:
        try:
            msg = await self.janus().message(
                p.sid,
                p.pub_hid,
                {"request": "configure", "audio": True, "video": True},
                jsep={"type": "offer", "sdp": sdp},
            )
            answer = msg["jsep"]["sdp"]
        except (JanusError, KeyError) as exc:
            log.warning("SFU rejected a publish offer from %s: %s", p.participant_id, exc)
            raise CallError("invalid", "the SFU rejected the offer") from exc
        p.publishing = _publishing_from_sdp(sdp)
        p.audio = any(x["kind"] == "audio" for x in p.publishing)
        p.video = any(x["kind"] == "video" for x in p.publishing)
        await self._broadcast(
            p.call,
            envelope(
                "call.participant",
                {"call_id": p.call.call_id, "event": "updated", "participant": p.view()},
            ),
            skip=p,
        )
        self._reconcile_all(p.call)
        return str(answer)

    # ---- subscribe ------------------------------------------------------------

    def _reconcile_all(self, call: Call) -> None:
        for p in list(call.participants.values()):
            self._spawn(self.reconcile(p))

    async def reconcile(self, p: Participant) -> None:
        """Bring p's subscribe PC to "every other participant whose media is up"."""
        async with p.lock:
            if p.participant_id not in p.call.participants:
                return
            if p.sub_pending is not None:
                p.sub_dirty = True  # replayed after the client answers
                return
            desired = {
                o.feed_id
                for o in p.call.participants.values()
                if o is not p and o.publishing and o.media_up
            }
            if desired == p.sub_feeds:
                return
            janus = self.janus()
            if p.sub_hid is None:
                hid = await janus.attach(p.sid, self._sub_listener(p))
                try:
                    msg = await janus.message(
                        p.sid,
                        hid,
                        {
                            "request": "join",
                            "ptype": "subscriber",
                            "room": p.call.room,
                            "private_id": p.private_id,
                            "streams": [{"feed": f} for f in sorted(desired)],
                        },
                    )
                except JanusError:
                    # Never keep a handle whose join failed: a later `update` on it
                    # fails too, wedging this participant's subscriptions for good.
                    log.exception("subscriber join failed for %s", p.participant_id)
                    await janus.detach(p.sid, hid)
                    return
                p.sub_hid = hid
            else:
                body: dict[str, Any] = {"request": "update"}
                if add := desired - p.sub_feeds:
                    body["subscribe"] = [{"feed": f} for f in sorted(add)]
                if drop := p.sub_feeds - desired:
                    body["unsubscribe"] = [{"feed": f} for f in sorted(drop)]
                try:
                    msg = await janus.message(p.sid, p.sub_hid, body)
                except JanusError:
                    log.exception("subscriber update failed for %s", p.participant_id)
                    return
            p.sub_feeds = desired
            if "jsep" in msg:
                await self._offer(p, msg)

    async def _offer(self, p: Participant, msg: dict[str, Any]) -> None:
        """Forward a Janus subscribe offer to the client, with the mid mapping."""
        p.sub_version += 1
        by_feed = {o.feed_id: o for o in p.call.participants.values()}
        streams = []
        for s in msg.get("plugindata", {}).get("data", {}).get("streams", []):
            owner = by_feed.get(s.get("feed_id"))
            if (
                owner is None
                or not s.get("active", True)
                or s.get("type") not in ("audio", "video")
            ):
                continue
            streams.append(
                {
                    "mid": str(s["mid"]),
                    "participant_id": owner.participant_id,
                    "kind": s["type"],
                    "source": "mic" if s["type"] == "audio" else "camera",
                }
            )
        frame = envelope(
            "call.subscribe.offer",
            {
                "call_id": p.call.call_id,
                "sdp": msg["jsep"]["sdp"],
                "version": p.sub_version,
                "streams": streams,
            },
        )
        p.sub_pending = frame
        await p.send(frame)

    async def subscribe_answer(self, p: Participant, version: Any, sdp: str) -> None:
        async with p.lock:
            if p.sub_pending is None or version != p.sub_version:
                raise CallError("stale", "not the latest subscribe offer")
            if p.sub_hid is None:
                raise CallError("bad_state", "no subscribe offer outstanding")
            try:
                res = await self.janus().message(
                    p.sid, p.sub_hid, {"request": "start"}, jsep={"type": "answer", "sdp": sdp}
                )
                log.debug(
                    "subscribe start v%s for %s -> %s",
                    version,
                    p.participant_id,
                    res.get("plugindata", {}).get("data"),
                )
            except JanusError as exc:
                # Don't leave sub_pending set: every later reconcile would defer to
                # it and this participant would never get another subscribe offer.
                # Start over with a fresh subscriber handle and a fresh offer.
                if p.sub_hid is not None:
                    await self.janus().detach(p.sid, p.sub_hid)
                p.sub_hid, p.sub_feeds, p.sub_pending, p.sub_dirty = None, set(), None, False
                self._spawn(self.reconcile(p))
                log.warning("SFU rejected a subscribe answer from %s: %s", p.participant_id, exc)
                raise CallError("invalid", "the SFU rejected the answer") from exc
            p.sub_pending = None
            replay, p.sub_dirty = p.sub_dirty, False
        if replay:
            self._spawn(self.reconcile(p))

    # ---- Janus → client -------------------------------------------------------

    def _pub_listener(self, p: Participant) -> Any:
        async def on_event(msg: dict[str, Any]) -> None:
            kind = msg.get("janus")
            if kind == "trickle":
                await p.send(
                    envelope(
                        "call.ice",
                        {
                            "call_id": p.call.call_id,
                            "pc": "publish",
                            "candidate": _candidate(msg.get("candidate")),
                        },
                    )
                )
            elif kind in ("webrtcup", "hangup"):
                # The publish PC came up (or went away): only now can others
                # subscribe to it (or must stop). Re-plan everyone's subscriptions.
                p.media_up = kind == "webrtcup"
                self._reconcile_all(p.call)

        return on_event

    def _sub_listener(self, p: Participant) -> Any:
        async def on_event(msg: dict[str, Any]) -> None:
            if msg.get("janus") == "trickle":
                await p.send(
                    envelope(
                        "call.ice",
                        {
                            "call_id": p.call.call_id,
                            "pc": "subscribe",
                            "candidate": _candidate(msg.get("candidate")),
                        },
                    )
                )
            elif "jsep" in msg and msg["jsep"].get("type") == "offer":
                # Janus renegotiated on its own (e.g. a feed vanished). Forward it
                # through the same versioned path so the client sees one sequence.
                async with p.lock:
                    await self._offer(p, msg)

        return on_event

    async def end_for_user(self, channel_id: uuid.UUID, user_id: uuid.UUID, reason: str) -> None:
        """End a user's participation (e.g. removed from the channel): call.ended to
        them, removal for everyone else. Their later commands get not_in_call."""
        call = self.by_channel.get(channel_id)
        if call is None:
            return
        for p in [p for p in call.participants.values() if p.user_id == user_id]:
            await p.send(envelope("call.ended", {"call_id": call.call_id, "reason": reason}))
            await self.remove(p)

    async def end_channel(self, channel_id: uuid.UUID, reason: str) -> None:
        """End everyone's participation (e.g. the channel was deleted)."""
        call = self.by_channel.get(channel_id)
        if call is None:
            return
        for p in list(call.participants.values()):
            await p.send(envelope("call.ended", {"call_id": call.call_id, "reason": reason}))
            await self.remove(p)

    async def snapshot(self, conn: Connection) -> None:
        """After auth.ok: one channel.call per active call in the user's channels,
        so a client that just (re)connected sees calls already in progress."""
        if not self.by_channel:
            return
        async with get_sessionmaker()() as session:
            mine = set(
                await session.scalars(
                    select(Membership.channel_id).where(Membership.user_id == conn.user_id)
                )
            )
        for channel_id, call in list(self.by_channel.items()):
            if channel_id in mine:
                data = {
                    "channel_id": str(channel_id),
                    "call_id": call.call_id,
                    "participant_count": len(call.participants),
                }
                await conn.send(envelope("channel.call", data))

    async def _on_janus_lost(self) -> None:
        """Janus restarted or the socket dropped: every session is gone."""
        calls = list(self.by_id.values())
        self.by_id.clear()
        self.by_channel.clear()
        for call in calls:
            ended = envelope("call.ended", {"call_id": call.call_id, "reason": "sfu_restart"})
            for p in call.participants.values():
                if p.grace is not None:
                    p.grace.cancel()
                await p.send(ended)
            await self._announce_channel_call(call.channel_id)

    # ---- connection lifecycle ----------------------------------------------

    async def on_disconnect(self, conn: Connection) -> None:
        for call in list(self.by_id.values()):
            for p in list(call.participants.values()):
                if p.conn is conn:
                    p.conn = None
                    p.grace = asyncio.create_task(self._grace_expiry(p))

    async def _grace_expiry(self, p: Participant) -> None:
        await asyncio.sleep(RESUME_GRACE_S)
        p.grace = None
        await self.remove(p, only_if_detached=True)

    async def resume(
        self, conn: Connection, call_id: Any, participant_id: Any, token: Any
    ) -> Participant:
        call = self.by_id.get(call_id) if isinstance(call_id, str) else None
        if call is None or not isinstance(participant_id, str):
            raise CallError("not_in_call", "no such participant to resume")
        # Under the channel lock, so it cannot interleave with a grace-expiry
        # removal: either the removal ran first (the participant is gone and this
        # is not_in_call) or this runs first (and the removal sees p.conn set).
        async with self._lock(call.channel_id):
            p = call.participants.get(participant_id)
            # Same user AND the secret handed to this participant only. Every
            # mismatch is the same not_in_call, so a guess learns nothing.
            if (
                p is None
                or p.user_id != conn.user_id
                or not isinstance(token, str)
                or not _token_ok(token, p.resume_token, p.prev_resume_token)
            ):
                raise CallError("not_in_call", "no such participant to resume")
            if p.grace is not None:
                p.grace.cancel()
                p.grace = None
            displaced, p.conn = p.conn, conn
            p.prev_resume_token = token  # the one this client holds, even if our reply is lost
            p.resume_token = secrets.token_urlsafe(24)
        if displaced is not None and displaced is not conn:
            # A still-open older socket (a half-open TCP session, or a duplicate
            # device) no longer owns the participant: tell it, so it shows the
            # right state instead of believing it is still in the call.
            ended = envelope("call.ended", {"call_id": call.call_id, "reason": "replaced"})
            self._spawn(displaced.send(ended))
        return p


manager = CallManager()


def _token_ok(given: str, current: str, previous: str | None) -> bool:
    """Constant-time match against the current or the last-used resume token.

    Both comparisons always run, so timing does not reveal which one matched.
    """
    if not given.isascii():
        return False  # compare_digest raises on non-ASCII str; our tokens are ASCII
    ok_current = secrets.compare_digest(given, current)
    ok_previous = secrets.compare_digest(given, previous) if previous else False
    return ok_current or ok_previous


def _publishing_from_sdp(sdp: str) -> list[dict[str, str]]:
    """What a publish offer sends: one entry per non-inactive audio/video m-line."""
    out: list[dict[str, str]] = []
    kind: str | None = None
    direction_ok = True

    def flush() -> None:
        if kind in ("audio", "video") and direction_ok:
            out.append({"kind": kind, "source": "mic" if kind == "audio" else "camera"})

    for line in sdp.splitlines():
        if line.startswith("m="):
            flush()
            kind, direction_ok = line[2:].split(" ", 1)[0], True
        elif line.strip() in ("a=inactive", "a=recvonly"):
            direction_ok = False
    flush()
    return out


def _candidate(c: Any) -> dict[str, Any] | None:
    if not isinstance(c, dict) or c.get("completed"):
        return None
    return {k: c.get(k) for k in ("candidate", "sdpMid", "sdpMLineIndex")}


def _require(frame: dict[str, Any], *keys: str) -> dict[str, Any]:
    data = frame.get("data")
    if not isinstance(data, dict) or any(k not in data for k in keys):
        raise CallError("invalid", f"data must contain {', '.join(keys)}")
    return data


CmdFn = Callable[[Connection, dict[str, Any], str | None], Awaitable[None]]


def _cmd(type_: str) -> Callable[[CmdFn], CmdFn]:
    """Register a call command; map CallError to exactly one ``error`` reply."""

    def deco(fn: CmdFn) -> CmdFn:
        @handles(type_)
        async def wrapper(conn: Connection, frame: dict[str, Any]) -> None:
            re = frame.get("id") if isinstance(frame.get("id"), str) else None
            try:
                await fn(conn, frame, re)
            except CallError as exc:
                await conn.send(error_frame(re, exc.code, exc.message))

        return fn

    return deco


@_cmd("call.join")
async def _join(conn: Connection, frame: dict[str, Any], re: str | None) -> None:
    data = _require(frame, "channel_id")
    try:
        channel_id = uuid.UUID(str(data["channel_id"]))
    except ValueError as exc:
        raise CallError("invalid", "channel_id is not a UUID") from exc
    call, p = await manager.join(conn, channel_id)
    others = [o.view() for o in call.participants.values() if o is not p]
    await conn.send(
        envelope(
            "call.joined",
            {
                "call_id": call.call_id,
                "channel_id": str(channel_id),
                "self": {"participant_id": p.participant_id, "resume_token": p.resume_token},
                "participants": others,
            },
            re=re,
        )
    )
    # After the reply: nothing below may produce a second reply (see _spawn).
    joined = envelope(
        "call.participant",
        {"call_id": call.call_id, "event": "joined", "participant": p.view()},
    )
    manager._spawn(manager._broadcast(call, joined, skip=p))
    manager._spawn(manager._announce_channel_call(channel_id))
    manager._reconcile_all(call)


@_cmd("call.publish")
async def _publish(conn: Connection, frame: dict[str, Any], re: str | None) -> None:
    data = _require(frame, "call_id", "sdp")
    p = manager._participant_of(conn, data["call_id"])
    answer = await manager.publish(p, str(data["sdp"]))
    await conn.send(
        envelope("call.publish.answer", {"call_id": p.call.call_id, "sdp": answer}, re=re)
    )


@_cmd("call.subscribe.answer")
async def _sub_answer(conn: Connection, frame: dict[str, Any], re: str | None) -> None:
    data = _require(frame, "call_id", "version", "sdp")
    p = manager._participant_of(conn, data["call_id"])
    await manager.subscribe_answer(p, data["version"], str(data["sdp"]))
    await conn.send(envelope("call.ok", {}, re=re))


@_cmd("call.ice")
async def _ice(conn: Connection, frame: dict[str, Any], re: str | None) -> None:
    data = _require(frame, "call_id", "pc", "candidate")
    p = manager._participant_of(conn, data["call_id"])
    hid = p.pub_hid if data["pc"] == "publish" else p.sub_hid
    if hid is None or data["pc"] not in ("publish", "subscribe"):
        return  # no reply for call.ice (contract §3.3); nothing to relay to
    cand = data["candidate"]
    try:
        await manager.janus().trickle(p.sid, hid, cand if isinstance(cand, dict) else None)
    except JanusError as exc:
        # call.ice has no reply (contract §3.3), so this log is the only trace.
        log.warning("trickle to %s handle failed for %s: %s", data["pc"], p.participant_id, exc)


@_cmd("call.media")
async def _media(conn: Connection, frame: dict[str, Any], re: str | None) -> None:
    data = _require(frame, "call_id", "audio", "video")
    p = manager._participant_of(conn, data["call_id"])
    p.audio, p.video = bool(data["audio"]), bool(data["video"])
    await conn.send(envelope("call.ok", {}, re=re))
    updated = envelope(
        "call.participant",
        {"call_id": p.call.call_id, "event": "updated", "participant": p.view()},
    )
    manager._spawn(manager._broadcast(p.call, updated, skip=p))


@_cmd("call.leave")
async def _leave(conn: Connection, frame: dict[str, Any], re: str | None) -> None:
    data = _require(frame, "call_id")
    p = manager._participant_of(conn, data["call_id"])
    # Act first, reply second: the removal must not depend on the reply being
    # deliverable (the client may already be gone).
    manager._spawn(manager.remove(p))
    await conn.send(envelope("call.ok", {}, re=re))


@_cmd("call.resume")
async def _resume(conn: Connection, frame: dict[str, Any], re: str | None) -> None:
    data = _require(frame, "call_id", "participant_id", "resume_token")
    p = await manager.resume(conn, data["call_id"], data["participant_id"], data["resume_token"])
    others = [o.view() for o in p.call.participants.values() if o is not p]
    await conn.send(
        envelope(
            "call.joined",
            {
                "call_id": p.call.call_id,
                "channel_id": str(p.call.channel_id),
                "self": {"participant_id": p.participant_id, "resume_token": p.resume_token},
                "participants": others,
            },
            re=re,
        )
    )
    if p.sub_pending is not None:
        manager._spawn(conn.send(p.sub_pending))


disconnect_hooks.append(manager.on_disconnect)
connect_hooks.append(manager.snapshot)
