"""Auth rate limiting: the limiter with an injected clock, and flows through the app."""

from __future__ import annotations

import logging

import httpx
import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from app import ratelimit, security
from app.ratelimit import AuthLimiter, BoundedLRU, LimitConfig


class FakeClock:
    """Injected time: tests move it explicitly (never real time)."""

    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


def limiter(**overrides: float) -> tuple[AuthLimiter, FakeClock]:
    clock = FakeClock()
    return AuthLimiter(LimitConfig(**overrides), clock=clock), clock  # type: ignore[arg-type]


# ---- unit: token bucket -----------------------------------------------------------


def test_bucket_allows_a_burst_then_paces() -> None:
    lim, clock = limiter(bucket_capacity=3, refill_per_s=1.0)
    assert [lim.check("ip") for _ in range(3)] == [None, None, None]
    assert lim.check("ip") == 1  # empty: one token takes 1 s
    clock.advance(1.0)
    assert lim.check("ip") is None


def test_bucket_is_per_ip() -> None:
    lim, _ = limiter(bucket_capacity=1, refill_per_s=0.1)
    assert lim.check("a") is None
    assert lim.check("a") is not None
    assert lim.check("b") is None


def test_ws_checks_do_not_consume_tokens() -> None:
    lim, _ = limiter(bucket_capacity=1, refill_per_s=0.01)
    for _ in range(20):
        assert lim.check("ip", consume=False) is None
    assert lim.check("ip") is None  # the one token is still there


# ---- unit: failure escalation -----------------------------------------------------


def test_backoff_grows_exponentially_after_n_failures() -> None:
    lim, clock = limiter(backoff_after=3, bucket_capacity=1000)
    for _ in range(3):
        lim.failure("ip")
    assert lim.check("ip") == 1  # 2**0 s after the 3rd failure
    clock.advance(1.0)
    assert lim.check("ip") is None
    lim.failure("ip")  # 4th
    assert lim.check("ip") == 2
    for _ in range(3):
        lim.failure("ip")  # 7th
    assert lim.check("ip") == 16


def test_backoff_is_capped() -> None:
    lim, _ = limiter(
        backoff_after=1, max_backoff_s=60, bucket_capacity=1000, go_away_per_hour=10**6
    )
    for _ in range(40):
        lim.failure("ip")
    assert lim.check("ip") == 60


def test_success_resets_the_streak() -> None:
    lim, _ = limiter(backoff_after=2, bucket_capacity=1000)
    lim.failure("ip")
    lim.failure("ip")
    assert lim.check("ip") is not None
    lim.success("ip")
    assert lim.check("ip") is None


def test_go_away_tier_answers_before_anything_else() -> None:
    lim, clock = limiter(go_away_per_hour=5, backoff_after=10**6, bucket_capacity=1000)
    for _ in range(6):
        lim.failure("ip")
        clock.advance(1.0)
    wait = lim.check("ip")
    assert wait is not None and wait > 3000  # until the oldest failure leaves the hour
    lim.success("ip")  # a success doesn't wipe the hourly count
    assert lim.check("ip") is not None
    clock.advance(3600)
    assert lim.check("ip") is None


def test_escalation_logs_once_per_tier(caplog: pytest.LogCaptureFixture) -> None:
    lim, _ = limiter(backoff_after=1, go_away_per_hour=3, bucket_capacity=1000)
    caplog.set_level(logging.WARNING, logger="app.ratelimit")
    lim.failure("10.0.0.9")
    lim.check("10.0.0.9")
    lim.check("10.0.0.9")
    for _ in range(3):
        lim.failure("10.0.0.9")
    lim.check("10.0.0.9")
    lim.check("10.0.0.9")
    lines = [r.getMessage() for r in caplog.records]
    assert lines == [
        "auth throttle: 10.0.0.9 escalated to tier 1",
        "auth throttle: 10.0.0.9 escalated to tier 2",
    ]


# ---- unit: per-handle -------------------------------------------------------------


def test_handle_is_slowed_across_ips_but_capped() -> None:
    lim, clock = limiter(handle_slow_after=3, handle_max_delay_s=10, bucket_capacity=1000)
    for i in range(3):
        lim.failure(f"attacker-{i}", "gabriel")
    wait = lim.check("new-ip", "gabriel")
    assert wait == 1
    for i in range(3, 20):
        lim.failure(f"attacker-{i}", "gabriel")
    assert lim.check("new-ip", "gabriel") == 10  # capped: slowed, never locked
    clock.advance(10)
    assert lim.check("new-ip", "gabriel") is None


def test_trusted_ip_is_never_slowed_by_attacks_on_its_handle() -> None:
    lim, _ = limiter(handle_slow_after=1, bucket_capacity=1000)
    lim.success("home", "gabriel")
    for i in range(50):
        lim.failure(f"attacker-{i}", "gabriel")
    assert lim.check("home", "gabriel") is None
    assert lim.check("elsewhere", "gabriel") is not None


def test_other_handles_are_unaffected() -> None:
    lim, _ = limiter(handle_slow_after=1, bucket_capacity=1000)
    for i in range(10):
        lim.failure(f"a{i}", "gabriel")
    assert lim.check("x", "someone-else") is None


# ---- unit: bounded memory ---------------------------------------------------------


def test_lru_forgets_the_oldest_beyond_max_keys() -> None:
    lru: BoundedLRU[str, list[int]] = BoundedLRU(3, list)
    for k in "abcd":
        lru.get(k)
    assert len(lru) == 3
    assert lru.peek("a") is None
    lru.get("b")  # touch: now most recent
    lru.get("e")
    assert lru.peek("b") is not None and lru.peek("c") is None


def test_a_flood_of_spoofed_ips_stays_bounded() -> None:
    lim, _ = limiter(max_keys=100)
    for i in range(10_000):
        lim.failure(f"10.{i // 65536}.{i // 256 % 256}.{i % 256}", f"h{i}")
    assert len(lim._ips) == 100
    assert len(lim._handles) == 100


# ---- flows through the app ---------------------------------------------------------


async def _register(client: httpx.AsyncClient) -> None:
    r = await client.post(
        "/api/v1/auth/register",
        json={"handle": "alice", "display_name": "Alice", "password": "correct-horse-battery"},
    )
    assert r.status_code == 201


async def _login(client: httpx.AsyncClient, handle: str, password: str) -> httpx.Response:
    return await client.post("/api/v1/auth/login", json={"handle": handle, "password": password})


async def test_repeated_wrong_passwords_get_429_with_retry_after(
    client: httpx.AsyncClient,
) -> None:
    await _register(client)
    codes = [(await _login(client, "alice", "wrong-password-x")).status_code for _ in range(6)]
    assert codes[:5] == [401] * 5
    assert codes[5] == 429
    r = await _login(client, "alice", "wrong-password-x")
    assert r.status_code == 429
    assert r.json()["error"]["code"] == "auth.rate_limited"
    assert int(r.headers["Retry-After"]) >= 1


async def test_429_does_not_reveal_whether_the_handle_exists(client: httpx.AsyncClient) -> None:
    await _register(client)
    known = [(await _login(client, "alice", "nope-nope-nope")) for _ in range(7)]
    ratelimit.get_limiter.cache_clear()
    unknown = [(await _login(client, "nobody", "nope-nope-nope")) for _ in range(7)]
    assert [r.status_code for r in known] == [r.status_code for r in unknown]
    assert known[-1].json() == unknown[-1].json()
    assert known[0].json() == unknown[0].json()  # the 401 bodies match too


async def test_go_away_tier_skips_argon2(
    client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    await _register(client)
    lim = ratelimit.get_limiter()
    for _ in range(lim.config.go_away_per_hour + 1):
        lim.failure("127.0.0.1")
    calls = {"n": 0}
    real = security.dummy_verify

    def counting(password: str) -> None:
        calls["n"] += 1
        real(password)

    monkeypatch.setattr("app.routers.auth.dummy_verify", counting)
    monkeypatch.setattr("app.routers.auth.verify_password", lambda *_: pytest.fail("argon2 ran"))
    r = await _login(client, "nobody", "whatever-pass")
    assert r.status_code == 429
    assert calls["n"] == 0


async def test_success_after_wrong_passwords_still_logs_in(client: httpx.AsyncClient) -> None:
    await _register(client)
    for _ in range(3):
        assert (await _login(client, "alice", "wrong-password-x")).status_code == 401
    assert (await _login(client, "alice", "correct-horse-battery")).status_code == 200


async def test_bad_refresh_tokens_escalate(client: httpx.AsyncClient) -> None:
    codes = [
        (await client.post("/api/v1/auth/refresh", json={"refresh_token": "junk"})).status_code
        for _ in range(7)
    ]
    assert 429 in codes and codes[0] == 401


def _forged_ws_auth(sync_client: TestClient) -> WebSocketDisconnect:
    """Open /ws, send a forged auth frame, return how the server closed it."""
    with (
        pytest.raises(WebSocketDisconnect) as closed,
        sync_client.websocket_connect("/ws") as ws,
    ):
        ws.send_json({"type": "auth", "data": {"access_token": "forged"}})
        ws.receive_json()
    return closed.value


def test_failed_ws_auth_escalates_to_rate_limited(sync_client: TestClient) -> None:
    for _ in range(5):
        assert _forged_ws_auth(sync_client).reason == "auth_failed"
    closed = _forged_ws_auth(sync_client)
    assert (closed.code, closed.reason) == (1008, "rate_limited")


def test_go_away_works_for_any_configured_threshold() -> None:
    lim, _ = limiter(go_away_per_hour=600, backoff_after=10**6, bucket_capacity=10**6)
    for _ in range(601):
        lim.failure("ip")
    assert lim.check("ip") is not None


# ---- review findings: IPv6 keying, wiring, bounds, hardening ------------------------


def test_ipv6_is_keyed_per_64() -> None:
    a = ratelimit.client_ip("2001:db8:1:2:aaaa::1")
    b = ratelimit.client_ip("2001:db8:1:2:ffff:ffff:ffff:ffff")
    c = ratelimit.client_ip("2001:db8:1:3::1")
    assert a == b == "2001:db8:1:2::/64"
    assert c != a
    assert ratelimit.client_ip("::ffff:192.0.2.7") == "192.0.2.7"  # mapped v4
    assert ratelimit.client_ip("192.0.2.7") == "192.0.2.7"


def test_rotating_ipv6_addresses_share_one_budget() -> None:
    lim, _ = limiter(backoff_after=2, bucket_capacity=1000)
    lim.failure(ratelimit.client_ip("2001:db8::1"))
    lim.failure(ratelimit.client_ip("2001:db8::2"))
    assert lim.check(ratelimit.client_ip("2001:db8::3")) is not None


def test_backoff_exponent_is_clamped_no_overflow() -> None:
    lim, _ = limiter(backoff_after=1, go_away_per_hour=10**6, bucket_capacity=10**6)
    for _ in range(2000):
        lim.failure("ip")
    assert lim.check("ip") == 3600


def test_trust_expires() -> None:
    lim, clock = limiter(handle_slow_after=1, bucket_capacity=1000, trust_ttl_s=100)
    lim.success("home", "gabriel")
    clock.advance(101)
    for i in range(5):
        lim.failure(f"a{i}", "gabriel")
    assert lim.check("home", "gabriel") is not None  # no longer exempt


async def test_success_resets_the_streak_through_login(client: httpx.AsyncClient) -> None:
    """Wiring (A): the owner's typos must not pile up for the life of the process."""
    await _register(client)
    for _ in range(4):
        assert (await _login(client, "alice", "wrong-password-x")).status_code == 401
    assert (await _login(client, "alice", "correct-horse-battery")).status_code == 200
    for _ in range(4):
        assert (await _login(client, "alice", "wrong-password-x")).status_code == 401


async def test_register_from_a_flooded_ip_is_429(client: httpx.AsyncClient) -> None:
    """Wiring (B): register is paced like login."""
    lim = ratelimit.get_limiter()
    for _ in range(lim.config.go_away_per_hour + 1):
        lim.failure("127.0.0.1")
    r = await client.post(
        "/api/v1/auth/register",
        json={"handle": "alice", "display_name": "Alice", "password": "correct-horse-battery"},
    )
    assert r.status_code == 429


async def test_oversized_login_payload_is_422(client: httpx.AsyncClient) -> None:
    r = await _login(client, "x" * 65, "pw")
    assert r.status_code == 422
    r = await _login(client, "alice", "p" * 257)
    assert r.status_code == 422


def _ws_token(sync_client: TestClient) -> str:
    sync_client.post(
        "/api/v1/auth/register",
        json={"handle": "alice", "display_name": "Alice", "password": "correct-horse-battery"},
    )
    r = sync_client.post(
        "/api/v1/auth/login", json={"handle": "alice", "password": "correct-horse-battery"}
    )
    return str(r.json()["access_token"])


def test_ws_reauth_from_an_ip_in_backoff_is_rate_limited(sync_client: TestClient) -> None:
    """Wiring (D): re-auth on an open socket is checked too."""
    token = _ws_token(sync_client)
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": token}})
        assert ws.receive_json()["type"] == "ready"
        lim = ratelimit.get_limiter()
        for _ in range(lim.config.backoff_after):
            lim.failure("testclient")
        ws.send_json({"type": "auth", "data": {"access_token": token}})
        with pytest.raises(WebSocketDisconnect) as closed:
            ws.receive_json()
    assert (closed.value.code, closed.value.reason) == (1008, "rate_limited")


def test_an_expired_ws_token_is_not_a_failure(
    sync_client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Devices waking with stale tokens get token_expired and no backoff."""
    from app import config

    monkeypatch.setattr(config.get_settings(), "access_ttl_seconds", -10)
    token = _ws_token(sync_client)  # issued already expired, correctly signed
    for _ in range(8):
        with (
            pytest.raises(WebSocketDisconnect) as closed,
            sync_client.websocket_connect("/ws") as ws,
        ):
            ws.send_json({"type": "auth", "data": {"access_token": token}})
            ws.receive_json()
        assert closed.value.reason == "token_expired"
    assert ratelimit.get_limiter().check("testclient", consume=False) is None
