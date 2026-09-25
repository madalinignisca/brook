"""In-process rate limiting and failure escalation for the auth endpoints.

One uvicorn worker on one node, so no Redis: everything lives in bounded LRU maps
(``max_keys`` each), so a flood of spoofed keys can't exhaust memory; the oldest
entries are simply forgotten.

Per client IP (``request.client.host``: uvicorn trusts ``X-Forwarded-For`` only from
127.0.0.1, where Caddy sits in the native deploy; behind the docker-compose Caddy
container every request appears to come from that container's IP instead):

- a **token bucket** paces attempts (login, register, refresh, WS ``auth``);
- **failure escalation**: after ``backoff_after`` consecutive failures the IP waits an
  exponentially growing time (capped at ``max_backoff_s``) and gets 429 +
  ``Retry-After``;
- the **go-away tier**: more than ``go_away_per_hour`` failures within the last hour
  answers 429 immediately, *before* any Argon2 work (the CPU is the point).

Per handle (failures from many IPs): the handle is slowed down, capped at
``handle_max_delay_s``, and **never locked**: an IP that logged in to that handle
successfully within ``trust_ttl_s`` is exempt, so the owner's usual devices keep
working while an attacker's IPs are slowed, and anyone else waits at most the cap.
Note: a polling attacker can keep the handle's slot occupied for non-trusted IPs
(each waits up to the cap). That is the price of never locking; keep the cap short,
because raising it turns the slowdown into a lock-out for new devices.

IPv6 clients are keyed per /64 (one subscriber's prefix), so rotating addresses
within it can't reset the budget; IPv4 per address.

No tarpitting: callers are told to come back (``Retry-After``), never held open.
Escalations are logged once per tier change with the IP (a fail2ban hook).

Reusable: other credential endpoints (password change, TOTP) call
:func:`enforce` before verifying and :meth:`AuthLimiter.failure` /
:meth:`AuthLimiter.success` after.
"""

from __future__ import annotations

import ipaddress
import logging
import math
import time
from collections import OrderedDict, deque
from collections.abc import Callable
from dataclasses import dataclass, field
from functools import lru_cache
from typing import Generic, TypeVar

from fastapi import HTTPException, status

from .config import get_settings

log = logging.getLogger(__name__)

Clock = Callable[[], float]
K = TypeVar("K")
V = TypeVar("V")

HOUR = 3600.0


class BoundedLRU(Generic[K, V]):
    """A dict that forgets its least recently used entries beyond ``max_keys``."""

    def __init__(self, max_keys: int, factory: Callable[[], V]) -> None:
        self._max = max_keys
        self._factory = factory
        self._data: OrderedDict[K, V] = OrderedDict()

    def get(self, key: K) -> V:
        """The entry for ``key`` (created if missing), marked most recently used."""
        value = self._data.get(key)
        if value is None:
            value = self._factory()
            self._data[key] = value
            while len(self._data) > self._max:
                self._data.popitem(last=False)
        else:
            self._data.move_to_end(key)
        return value

    def peek(self, key: K) -> V | None:
        """The entry for ``key`` without creating or touching it."""
        return self._data.get(key)

    def __len__(self) -> int:
        return len(self._data)


@dataclass
class _IpState:
    tokens: float = math.inf  # set to capacity on first use
    refilled_at: float = 0.0
    consecutive: int = 0  # failures since the last success
    last_failure: float = 0.0
    recent: deque[float] = field(default_factory=deque)  # bounded by the limiter
    tier: int = 0  # 0 = normal, 1 = backoff, 2 = go-away (for logging tier changes)


@dataclass
class _HandleState:
    failures: deque[float] = field(default_factory=lambda: deque(maxlen=512))
    last_failure: float = 0.0


@dataclass(frozen=True)
class LimitConfig:
    """Tunables (see :class:`app.config.Settings` for the env knobs)."""

    max_keys: int = 10_000
    bucket_capacity: float = 10.0  # burst of attempts per IP
    refill_per_s: float = 10.0 / 60.0  # sustained: 10 attempts / minute
    backoff_after: int = 5  # consecutive failures before backoff starts
    max_backoff_s: float = HOUR
    go_away_per_hour: int = 50
    handle_window_s: float = 15 * 60.0
    handle_slow_after: int = 10  # failures on one handle (any IPs) within the window
    handle_max_delay_s: float = 60.0
    trusted_ips_per_handle: int = 8
    trust_ttl_s: float = 30 * 24 * HOUR
    # TOTP code budget (spec 2026-09-25-totp §6): after code_budget wrong codes for a
    # handle within code_window_s, each further attempt waits code_spacing_s after the
    # last failure. <= 96 guesses/day at these values, never a lock.
    code_budget: int = 10
    code_window_s: float = 24 * HOUR
    code_spacing_s: float = 15 * 60.0
    # Decrypt failures of a user's TOTP secret: an operator fault, not a guess, so
    # not a limiter failure, but bounded so it can't amplify logs or CPU (§5).
    decrypt_per_hour: int = 5


class AuthLimiter:
    """Pacing + failure escalation for credential checks. All times from ``clock``."""

    def __init__(self, config: LimitConfig | None = None, clock: Clock = time.monotonic) -> None:
        self.config = config or LimitConfig()
        self._clock = clock
        # Enough per-IP history to decide the go-away tier, and no more.
        history = self.config.go_away_per_hour + 1
        self._ips: BoundedLRU[str, _IpState] = BoundedLRU(
            self.config.max_keys, lambda: _IpState(recent=deque(maxlen=history))
        )
        self._handles: BoundedLRU[str, _HandleState] = BoundedLRU(
            self.config.max_keys, _HandleState
        )
        self._trusted: BoundedLRU[str, OrderedDict[str, float]] = BoundedLRU(
            self.config.max_keys, OrderedDict
        )
        self._codes: BoundedLRU[str, deque[float]] = BoundedLRU(self.config.max_keys, deque)
        self._decrypts: BoundedLRU[str, deque[float]] = BoundedLRU(self.config.max_keys, deque)

    # ---- decisions -------------------------------------------------------------

    def check(self, ip: str, handle: str | None = None, *, consume: bool = True) -> int | None:
        """Before verifying a credential: ``None`` to go ahead, else the seconds the
        caller must wait (answer 429 + ``Retry-After``).

        ``consume`` takes one attempt token from the IP's bucket (password and refresh
        checks). The WS ``auth`` frame passes ``False``: a valid access token costs no
        Argon2 and can't be guessed, so reconnecting devices behind one NAT aren't
        paced; failed WS auths still escalate (backoff, go-away)."""
        now = self._clock()
        cfg = self.config
        st = self._ips.get(ip)

        # Go-away: too many failures within the last hour. Checked first, so an abuser
        # costs one dict lookup and no Argon2.
        self._prune(st.recent, now - HOUR)
        if len(st.recent) > cfg.go_away_per_hour:
            self._escalate(ip, st, 2)
            return _ceil(st.recent[0] + HOUR - now)

        # Exponential backoff after consecutive failures.
        if st.consecutive >= cfg.backoff_after:
            # Clamped exponent: 2.0**1100 would overflow into a 500.
            exponent = min(st.consecutive - cfg.backoff_after, 40)
            wait = min(2.0**exponent, cfg.max_backoff_s)
            until = st.last_failure + wait
            if now < until:
                self._escalate(ip, st, 1)
                return _ceil(until - now)

        # Per-handle slowdown (never a lock; trusted IPs exempt).
        if handle is not None and not self._is_trusted(handle, ip):
            hs = self._handles.peek(handle)
            if hs is not None:
                self._prune(hs.failures, now - cfg.handle_window_s)
                over = len(hs.failures) - cfg.handle_slow_after
                if over >= 0:
                    delay = min(2.0 ** min(over, 40), cfg.handle_max_delay_s)
                    until = hs.last_failure + delay
                    if now < until:
                        return _ceil(until - now)

        # Token bucket: pace attempts even when they succeed.
        if not consume:
            return None
        if st.tokens == math.inf:
            st.tokens, st.refilled_at = cfg.bucket_capacity, now
        st.tokens = min(cfg.bucket_capacity, st.tokens + (now - st.refilled_at) * cfg.refill_per_s)
        st.refilled_at = now
        if st.tokens < 1.0:
            return _ceil((1.0 - st.tokens) / cfg.refill_per_s)
        st.tokens -= 1.0
        return None

    def failure(self, ip: str, handle: str | None = None) -> None:
        """A credential check failed (wrong password, bad token, failed WS auth)."""
        now = self._clock()
        st = self._ips.get(ip)
        st.consecutive += 1
        st.last_failure = now
        st.recent.append(now)
        if handle is not None:
            hs = self._handles.get(handle)
            hs.failures.append(now)
            hs.last_failure = now

    def success(self, ip: str, handle: str | None = None) -> None:
        """A credential check succeeded: reset the IP's streak; trust it for ``handle``."""
        st = self._ips.get(ip)
        st.consecutive = 0
        st.tier = 0
        if handle is not None:
            trusted = self._trusted.get(handle)
            trusted[ip] = self._clock()
            trusted.move_to_end(ip)
            while len(trusted) > self.config.trusted_ips_per_handle:
                trusted.popitem(last=False)

    # ---- TOTP code budget (spec 2026-09-25-totp §6) --------------------------------

    def code_check(self, handle: str, ip: str) -> int | None:
        """Before verifying a TOTP or recovery code for ``handle``: seconds to wait, or
        ``None``. IPs that completed a login for this handle recently are exempt, so
        an attacker holding the password can't keep the owner waiting; this budget
        only, the per-IP tiers in :meth:`check` still apply to them."""
        if self._is_trusted(handle, ip):
            return None
        fails = self._codes.peek(handle)
        if not fails:
            return None
        now = self._clock()
        self._prune(fails, now - self.config.code_window_s)
        if len(fails) < self.config.code_budget:
            return None
        until = fails[-1] + self.config.code_spacing_s
        return _ceil(until - now) if now < until else None

    def code_failure(self, handle: str) -> bool:
        """A wrong code for ``handle``. True when this failure crosses the budget
        (the caller records a ``totp_guessing`` event, once per crossing)."""
        now = self._clock()
        fails = self._codes.get(handle)
        self._prune(fails, now - self.config.code_window_s)
        fails.append(now)
        crossed = len(fails) == self.config.code_budget
        if crossed:
            log.warning("totp guessing: handle over its code budget")
        return crossed

    def code_reset(self, handle: str) -> None:
        """Clear ``handle``'s code failures: a correct code, a password change or an
        admin TOTP reset (the owner's remedies must end the waiting)."""
        fails = self._codes.peek(handle)
        if fails is not None:
            fails.clear()

    def decrypt_check(self, handle: str) -> int | None:
        now = self._clock()
        fails = self._decrypts.get(handle)
        self._prune(fails, now - HOUR)
        if len(fails) >= self.config.decrypt_per_hour:
            return _ceil(fails[0] + HOUR - now)
        return None

    def decrypt_failure(self, handle: str) -> None:
        self._decrypts.get(handle).append(self._clock())

    # ---- helpers ---------------------------------------------------------------

    def _is_trusted(self, handle: str, ip: str) -> bool:
        trusted = self._trusted.peek(handle)
        since = trusted.get(ip) if trusted is not None else None
        return since is not None and self._clock() - since <= self.config.trust_ttl_s

    @staticmethod
    def _prune(q: deque[float], cutoff: float) -> None:
        while q and q[0] <= cutoff:
            q.popleft()

    @staticmethod
    def _escalate(ip: str, st: _IpState, tier: int) -> None:
        if tier > st.tier:
            st.tier = tier
            log.warning("auth throttle: %s escalated to tier %d", ip, tier)


def _ceil(seconds: float) -> int:
    return max(1, math.ceil(seconds))


@lru_cache
def get_limiter() -> AuthLimiter:
    """The process-wide limiter (one worker per node)."""
    s = get_settings()
    return AuthLimiter(
        LimitConfig(
            max_keys=s.ratelimit_max_keys,
            bucket_capacity=s.ratelimit_burst,
            refill_per_s=s.ratelimit_per_minute / 60.0,
            backoff_after=s.ratelimit_backoff_after,
            go_away_per_hour=s.ratelimit_go_away_per_hour,
        )
    )


def enforce(limiter: AuthLimiter, ip: str, handle: str | None = None) -> None:
    """Raise 429 + ``Retry-After`` if ``ip`` (and ``handle``) must wait."""
    wait = limiter.check(ip, handle)
    if wait is not None:
        raise HTTPException(
            status_code=status.HTTP_429_TOO_MANY_REQUESTS,
            detail={
                "code": "auth.rate_limited",
                "message": "Too many attempts; try again later",
            },
            headers={"Retry-After": str(wait)},
        )


def client_ip(host: str | None) -> str:
    """The limiter key for a request's peer (``request.client.host``): the address
    for IPv4, the /64 for IPv6 (a subscriber can rotate within its prefix)."""
    if not host:
        return "unknown"
    try:
        addr = ipaddress.ip_address(host)
    except ValueError:
        return host  # e.g. "testclient"
    if isinstance(addr, ipaddress.IPv6Address):
        if addr.ipv4_mapped is not None:  # ::ffff:a.b.c.d is really IPv4
            return str(addr.ipv4_mapped)
        return str(ipaddress.ip_network(f"{addr}/64", strict=False))
    return str(addr)
