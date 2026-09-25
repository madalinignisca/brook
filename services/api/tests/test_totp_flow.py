"""TOTP over HTTP (spec 2026-09-25-totp-server-design.md §7): login step, pending
token, replay across endpoints, recovery codes, decrypt failure, budgets, admin
reset, host CLI, startup canary."""

from __future__ import annotations

import uuid
from collections.abc import Iterator
from urllib.parse import parse_qs, urlparse

import httpx
import pytest
from fastapi.testclient import TestClient
from sqlalchemy import select, update
from starlette.websockets import WebSocketDisconnect

from app import db, ratelimit, secretbox
from app import totp as totp_core
from app.models import AuthEvent, Totp
from app.routers import totp as totp_router

AUTH = "/api/v1/auth"
TOTP = "/api/v1/auth/totp"
PW = "supersecret"
T0 = 1_800_000_000.0  # a fixed wall clock for code steps


class Clock:
    def __init__(self) -> None:
        self.t = T0

    def __call__(self) -> float:
        return self.t

    def next_step(self) -> None:
        self.t += totp_core.PERIOD_S


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> Iterator[Clock]:
    c = Clock()
    monkeypatch.setattr(totp_router, "_now", c)
    yield c


def _h(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


def code(secret: str, clock: Clock) -> str:
    return totp_core.code_at(secret, totp_core.step_for(clock()))


async def _login(
    client: httpx.AsyncClient, handle: str = "alice", **extra: object
) -> httpx.Response:
    body = {"handle": handle, "password": PW, "supports_totp": True, **extra}
    return await client.post(f"{AUTH}/login", json=body)


async def _setup(client: httpx.AsyncClient) -> dict[str, str]:
    """alice (admin) signed in; returns her pair."""
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    assert (await client.post(f"{AUTH}/register", json=body)).status_code == 201
    return dict((await _login(client)).json())


async def _enable(
    client: httpx.AsyncClient, pair: dict[str, str], clock: Clock
) -> tuple[str, dict]:
    """Enrol + activate; returns (secret, activation response). Advances the clock one
    step so the next code is fresh (the activation code is spent: replay guard)."""
    r = await client.post(f"{TOTP}/enroll", json={"password": PW}, headers=_h(pair["access_token"]))
    assert r.status_code == 200, r.text
    secret = parse_qs(urlparse(r.json()["otpauth_uri"]).query)["secret"][0]
    a = await client.post(
        f"{TOTP}/activate", json={"code": code(secret, clock)}, headers=_h(pair["access_token"])
    )
    assert a.status_code == 200, a.text
    clock.next_step()
    return secret, a.json()


async def _pending(client: httpx.AsyncClient) -> str:
    r = await _login(client)
    assert r.status_code == 200 and r.json()["totp_required"] is True
    return str(r.json()["totp_token"])


async def _events(user_handle: str = "alice") -> list[tuple[str, uuid.UUID | None, str]]:
    from app.models import User

    async with db.get_sessionmaker()() as s:
        uid = await s.scalar(select(User.id).where(User.handle == user_handle))
        rows = (await s.scalars(select(AuthEvent).where(AuthEvent.user_id == uid))).all()
        return [(e.kind, e.actor_id, e.via) for e in rows]


# ---------------------------------------------------------------- login


async def test_full_totp_login(client: httpx.AsyncClient, clock: Clock) -> None:
    pair = await _setup(client)
    secret, activated = await _enable(client, pair, clock)
    assert len(activated["recovery_codes"]) == 10
    me = (await client.get(f"{AUTH}/me", headers=_h(activated["access_token"]))).json()
    assert me["totp_enabled"] is True and me["recovery_codes_left"] == 10

    token = await _pending(client)
    r = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert r.status_code == 200
    assert (await client.get(f"{AUTH}/me", headers=_h(r.json()["access_token"]))).status_code == 200


async def test_old_client_gets_a_clear_refusal(client: httpx.AsyncClient, clock: Clock) -> None:
    pair = await _setup(client)
    await _enable(client, pair, clock)
    old = await client.post(f"{AUTH}/login", json={"handle": "alice", "password": PW})
    assert old.status_code == 403
    assert old.json()["error"]["code"] == "auth.totp_client_required"


async def test_replay_is_refused_across_endpoints(client: httpx.AsyncClient, clock: Clock) -> None:
    pair = await _setup(client)
    r = await client.post(f"{TOTP}/enroll", json={"password": PW}, headers=_h(pair["access_token"]))
    secret = parse_qs(urlparse(r.json()["otpauth_uri"]).query)["secret"][0]
    used = code(secret, clock)
    assert (
        await client.post(f"{TOTP}/activate", json={"code": used}, headers=_h(pair["access_token"]))
    ).status_code == 200
    # The same code, same window, at login: refused. The next window's code works.
    token = await _pending(client)
    again = await client.post(TOTP, json={"totp_token": token, "code": used})
    assert again.status_code == 403 and again.json()["error"]["code"] == "auth.invalid_code"
    clock.next_step()
    ok = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert ok.status_code == 200


async def test_pending_token_is_single_use(client: httpx.AsyncClient, clock: Clock) -> None:
    pair = await _setup(client)
    secret, _ = await _enable(client, pair, clock)
    token = await _pending(client)
    assert (
        await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    ).status_code == 200
    clock.next_step()
    reuse = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert reuse.status_code == 403 and reuse.json()["error"]["code"] == "auth.totp_expired"


async def test_pending_token_is_refused_everywhere_else(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    pair = await _setup(client)
    await _enable(client, pair, clock)
    token = await _pending(client)
    assert (await client.get(f"{AUTH}/me", headers=_h(token))).status_code == 401
    change = await client.post(
        f"{AUTH}/password",
        json={"current_password": PW, "new_password": "brand-new-pass"},
        headers=_h(token),
    )
    assert change.status_code == 401
    # And the mirror: an access token is not a pending token.
    mirror = await client.post(TOTP, json={"totp_token": pair["access_token"], "code": "000000"})
    assert mirror.status_code == 403 and mirror.json()["error"]["code"] == "auth.totp_expired"


def test_pending_token_is_refused_on_the_websocket(sync_client: TestClient) -> None:
    from app.config import get_settings
    from app.security import create_totp_pending_token

    http = sync_client
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    assert http.post(f"{AUTH}/register", json=body).status_code == 201
    pair = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    uid = uuid.UUID(http.get(f"{AUTH}/me", headers=_h(pair["access_token"])).json()["id"])
    token, _ = create_totp_pending_token(get_settings(), uid)
    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": token}})
        with pytest.raises(WebSocketDisconnect) as closed:
            ws.receive_json()
        assert closed.value.reason == "auth_failed"


async def test_pending_token_dies_on_a_password_change(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    pair = await _setup(client)
    secret, activated = await _enable(client, pair, clock)
    token = await _pending(client)
    # Changed with the box UNCHECKED: nothing else is revoked, but the token was
    # minted by proving the old password, so it dies anyway.
    r = await client.post(
        f"{AUTH}/password",
        json={
            "current_password": PW,
            "new_password": "brand-new-pass",
            "sign_out_other_devices": False,
        },
        headers=_h(activated["access_token"]),
    )
    assert r.status_code == 200
    dead = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert dead.status_code == 403 and dead.json()["error"]["code"] == "auth.totp_expired"


async def test_activation_signs_out_other_sessions(client: httpx.AsyncClient, clock: Clock) -> None:
    pair = await _setup(client)
    other = dict((await _login(client)).json())  # a second device, before enrolment
    _secret, activated = await _enable(client, pair, clock)
    stale = await client.post(f"{AUTH}/refresh", json={"refresh_token": other["refresh_token"]})
    assert stale.status_code == 401
    assert (await client.get(f"{AUTH}/me", headers=_h(other["access_token"]))).status_code == 401
    assert (
        await client.get(f"{AUTH}/me", headers=_h(activated["access_token"]))
    ).status_code == 200


# ---------------------------------------------------------------- recovery codes


async def test_recovery_codes_are_single_use_and_counted(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    pair = await _setup(client)
    _secret, activated = await _enable(client, pair, clock)
    first = activated["recovery_codes"][0]
    token = await _pending(client)
    r = await client.post(TOTP, json={"totp_token": token, "recovery_code": first.lower()})
    assert r.status_code == 200 and r.json()["recovery_codes_left"] == 9
    token = await _pending(client)
    again = await client.post(TOTP, json={"totp_token": token, "recovery_code": first})
    assert again.status_code == 403 and again.json()["error"]["code"] == "auth.invalid_code"
    assert ("recovery_code_used", None, "api") in await _events()


async def test_regenerate_replaces_every_code(client: httpx.AsyncClient, clock: Clock) -> None:
    pair = await _setup(client)
    secret, activated = await _enable(client, pair, clock)
    access = activated["access_token"]
    r = await client.post(
        f"{TOTP}/recovery-codes",
        json={"password": PW, "code": code(secret, clock)},
        headers=_h(access),
    )
    assert r.status_code == 200 and len(r.json()["recovery_codes"]) == 10
    token = await _pending(client)
    old = await client.post(
        TOTP, json={"totp_token": token, "recovery_code": activated["recovery_codes"][1]}
    )
    assert old.status_code == 403
    new = await client.post(
        TOTP, json={"totp_token": token, "recovery_code": r.json()["recovery_codes"][0]}
    )
    assert new.status_code == 200


async def test_disable_needs_password_and_second_factor(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    pair = await _setup(client)
    secret, activated = await _enable(client, pair, clock)
    access = activated["access_token"]
    wrong_pw = await client.post(
        f"{TOTP}/disable",
        json={"password": "nope-nope", "code": code(secret, clock)},
        headers=_h(access),
    )
    assert (
        wrong_pw.status_code == 403
        and wrong_pw.json()["error"]["code"] == "auth.invalid_credentials"
    )
    ok = await client.post(
        f"{TOTP}/disable", json={"password": PW, "code": code(secret, clock)}, headers=_h(access)
    )
    assert ok.status_code == 204
    plain = await _login(client)
    assert "access_token" in plain.json()  # no TOTP step any more
    me = (await client.get(f"{AUTH}/me", headers=_h(access))).json()
    assert me["totp_enabled"] is False and me["recovery_codes_left"] is None


# ---------------------------------------------------------------- enrolment


async def test_enrolment_rules(client: httpx.AsyncClient, clock: Clock) -> None:
    pair = await _setup(client)
    access = pair["access_token"]
    r = await client.post(f"{TOTP}/enroll", json={"password": PW}, headers=_h(access))
    assert r.json()["expires_in"] == 600
    secret = parse_qs(urlparse(r.json()["otpauth_uri"]).query)["secret"][0]
    # An expired enrolment is refused with its own code.
    async with db.get_sessionmaker()() as s:
        await s.execute(update(Totp).values(pending_expires_at=Totp.created_at))
        await s.commit()
    expired = await client.post(
        f"{TOTP}/activate", json={"code": code(secret, clock)}, headers=_h(access)
    )
    assert (
        expired.status_code == 409
        and expired.json()["error"]["code"] == "auth.totp_enrollment_expired"
    )
    # Re-enrol and activate; enrolling again is then refused (no silent re-enrolment).
    _secret, activated = await _enable(client, pair, clock)
    again = await client.post(
        f"{TOTP}/enroll", json={"password": PW}, headers=_h(activated["access_token"])
    )
    assert again.status_code == 409 and again.json()["error"]["code"] == "conflict"


# ---------------------------------------------------------------- decrypt failure


async def test_decrypt_failure_fails_closed_and_is_not_a_guess(
    client: httpx.AsyncClient, clock: Clock, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A roomy per-IP burst, so every 429 below can only come from the decrypt budget.
    from app import config

    monkeypatch.setenv("BROOK_RATELIMIT_BURST", "100")
    config.get_settings.cache_clear()
    ratelimit.get_limiter.cache_clear()
    pair = await _setup(client)
    secret, _ = await _enable(client, pair, clock)
    async with db.get_sessionmaker()() as s:  # corrupt the stored ciphertext
        row = await s.scalar(select(Totp))
        assert row is not None
        tampered = row.secret[:-4] + ("AAAA" if not row.secret.endswith("AAAA") else "BBBB")
        await s.execute(update(Totp).values(secret=tampered))
        await s.commit()
    before = sum(secretbox.decrypt_failures.values())
    token = await _pending(client)
    for _ in range(5):
        r = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
        # Byte-identical to a wrong code: never 200, never 500, never "not enrolled".
        assert r.status_code == 403 and r.json()["error"]["code"] == "auth.invalid_code"
    assert sum(secretbox.decrypt_failures.values()) == before + 5
    # Its own budget: the 6th is refused before decrypting.
    capped = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert capped.status_code == 429
    assert sum(secretbox.decrypt_failures.values()) == before + 5
    # Not limiter failures: no backoff or go-away for this IP after 5 of them
    # (consume=False: ask about escalation only, not the per-request bucket).
    assert ratelimit.get_limiter().check("127.0.0.1", consume=False) is None


# ---------------------------------------------------------------- budgets


async def test_code_budget_paces_spraying_and_records_it(
    client: httpx.AsyncClient, clock: Clock, monkeypatch: pytest.MonkeyPatch
) -> None:
    pair = await _setup(client)
    secret, _ = await _enable(client, pair, clock)
    # The setup logins made this IP trusted for alice (exempt, by design); model an
    # attacker's IP instead: a limiter that has never seen a completed login.
    ratelimit.get_limiter.cache_clear()
    lim = ratelimit.get_limiter()
    for _ in range(lim.config.code_budget - 1):
        lim.code_failure("alice")  # an attacker elsewhere already spent 9
    token = await _pending(client)
    crossing = await client.post(TOTP, json={"totp_token": token, "code": "000000"})
    assert crossing.status_code == 403
    assert ("totp_guessing", None, "api") in await _events()

    def no_hmac(*_a: object, **_k: object) -> None:
        raise AssertionError("HMAC ran for a paced handle")

    monkeypatch.setattr(totp_core, "match_step", no_hmac)
    paced = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert paced.status_code == 429
    assert int(paced.headers["retry-after"]) >= 800


async def test_code_budget_resets_and_trusted_ips_are_exempt(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    pair = await _setup(client)
    secret, activated = await _enable(client, pair, clock)
    lim = ratelimit.get_limiter()
    # A completed TOTP login makes this IP trusted for alice.
    token = await _pending(client)
    assert (
        await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    ).status_code == 200
    clock.next_step()
    for _ in range(lim.config.code_budget + 5):
        lim.code_failure("alice")
    assert lim.code_check("alice", "127.0.0.1") is None  # trusted: not paced
    assert lim.code_check("alice", "203.0.113.9") is not None  # elsewhere: paced
    # A password change clears the budget for everyone.
    r = await client.post(
        f"{AUTH}/password",
        json={
            "current_password": PW,
            "new_password": "brand-new-pass",
            "sign_out_other_devices": False,
        },
        headers=_h(activated["access_token"]),
    )
    assert r.status_code == 200
    assert lim.code_check("alice", "203.0.113.9") is None


async def test_password_stage_alone_earns_no_trust(client: httpx.AsyncClient, clock: Clock) -> None:
    """A correct password for a TOTP user is half a login: it must not mark the IP
    trusted, or a password-only attacker would exempt themselves from the budget."""
    pair = await _setup(client)
    await _enable(client, pair, clock)
    lim = ratelimit.get_limiter()
    ratelimit.get_limiter.cache_clear()  # forget the trust the setup logins earned
    lim = ratelimit.get_limiter()
    await _pending(client)
    for _ in range(lim.config.code_budget):
        lim.code_failure("alice")
    assert lim.code_check("alice", "127.0.0.1") is not None


# ---------------------------------------------------------------- admin reset + CLI


async def test_admin_totp_reset(client: httpx.AsyncClient, clock: Clock) -> None:
    admin = await _setup(client)
    body = {"handle": "bob", "display_name": "Bob", "password": PW}
    await client.post(f"{AUTH}/register", json=body, headers=_h(admin["access_token"]))
    bob = dict((await _login(client, "bob")).json())
    _secret, bob_on = await _enable(client, bob, clock)
    bob_id = (await client.get(f"{AUTH}/me", headers=_h(bob_on["access_token"]))).json()["id"]
    admin_id = (await client.get(f"{AUTH}/me", headers=_h(admin["access_token"]))).json()["id"]
    url = f"/api/v1/users/{bob_id}/totp/reset"

    wrong = await client.post(
        url, json={"admin_password": "nope-nope"}, headers=_h(admin["access_token"])
    )
    assert wrong.status_code == 403 and wrong.json()["error"]["code"] == "auth.invalid_credentials"
    self_reset = await client.post(
        f"/api/v1/users/{admin_id}/totp/reset",
        json={"admin_password": PW},
        headers=_h(admin["access_token"]),
    )
    assert self_reset.status_code == 400
    ok = await client.post(url, json={"admin_password": PW}, headers=_h(admin["access_token"]))
    assert ok.status_code == 204
    assert (await client.get(f"{AUTH}/me", headers=_h(bob_on["access_token"]))).status_code == 401
    assert "access_token" in (await _login(client, "bob")).json()  # plain login again
    assert ("totp_reset", uuid.UUID(admin_id), "api") in await _events("bob")


async def test_admin_cannot_reset_another_admins_totp(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    from app.models import User

    admin = await _setup(client)
    body = {"handle": "carol", "display_name": "Carol", "password": PW}
    await client.post(f"{AUTH}/register", json=body, headers=_h(admin["access_token"]))
    async with db.get_sessionmaker()() as s:
        await s.execute(update(User).where(User.handle == "carol").values(global_role="admin"))
        await s.commit()
        carol_id = await s.scalar(select(User.id).where(User.handle == "carol"))
    r = await client.post(
        f"/api/v1/users/{carol_id}/totp/reset",
        json={"admin_password": PW},
        headers=_h(admin["access_token"]),
    )
    assert r.status_code == 403 and r.json()["error"]["code"] == "authz.forbidden"


async def test_host_cli_resets_an_admin(client: httpx.AsyncClient, clock: Clock) -> None:
    from app.cli import totp_reset

    pair = await _setup(client)
    await _enable(client, pair, clock)
    assert await totp_reset("alice") == 0
    assert "access_token" in (await _login(client)).json()
    assert ("totp_reset", None, "host_cli") in await _events()
    assert await totp_reset("nobody") == 1


# ---------------------------------------------------------------- startup


async def test_startup_canary_refuses_an_undecryptable_secret(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    from app.main import secret_canary

    pair = await _setup(client)
    await _enable(client, pair, clock)
    await secret_canary()  # a healthy ring: fine
    async with db.get_sessionmaker()() as s:
        row = await s.scalar(select(Totp))
        assert row is not None
        await s.execute(update(Totp).values(secret=row.secret[:-4] + "AAAA"))
        await s.commit()
    with pytest.raises(RuntimeError, match="secret canary failed"):
        await secret_canary()


def test_app_refuses_to_start_without_a_keyring(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    from app import config
    from app.main import create_app

    monkeypatch.delenv("BROOK_SECRET_KEYS", raising=False)
    monkeypatch.setenv("BROOK_JWT_SIGNING_KEY", "test-signing-key-at-least-32-bytes-long!")
    config.get_settings.cache_clear()
    secretbox.get_secret_box.cache_clear()
    try:
        with pytest.raises(RuntimeError), TestClient(create_app()):
            pass
    finally:
        config.get_settings.cache_clear()
        secretbox.get_secret_box.cache_clear()


async def test_a_signed_non_pending_token_is_refused_by_type(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    """Only the type check can refuse this one: validly signed, has a jti (so the
    required-claims check passes), for a real TOTP user, but type 'access'."""
    import jwt as pyjwt

    from app.config import get_settings
    from app.models import User

    pair = await _setup(client)
    await _enable(client, pair, clock)
    async with db.get_sessionmaker()() as s:
        uid = await s.scalar(select(User.id).where(User.handle == "alice"))
    settings = get_settings()
    forged = pyjwt.encode(
        {
            "sub": str(uid),
            "type": "access",
            "jti": "x",
            "iat": 1,
            "iat_ms": 4_102_444_800_000,
            "exp": 4_102_444_800,
        },
        settings.jwt_signing_key,
        algorithm=settings.jwt_algorithm,
    )
    r = await client.post(TOTP, json={"totp_token": forged, "code": "000000"})
    assert r.status_code == 403 and r.json()["error"]["code"] == "auth.totp_expired"


async def test_disable_accepts_a_recovery_code_in_the_code_field(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    """Spec §2.3: disable/recovery-codes take {password, code}, where code may be a
    recovery code (the app has one field)."""
    pair = await _setup(client)
    _secret, activated = await _enable(client, pair, clock)
    r = await client.post(
        f"{TOTP}/disable",
        json={"password": PW, "code": activated["recovery_codes"][0]},
        headers=_h(activated["access_token"]),
    )
    assert r.status_code == 204


# ---------------------------------------------------------------- auth review of #52


@pytest.mark.parametrize("route", ["disable", "recovery-codes"])
async def test_management_refuses_a_wrong_code(
    client: httpx.AsyncClient, clock: Clock, route: str
) -> None:
    """B1: the second factor on disable/regenerate is real, not just the password."""
    pair = await _setup(client)
    secret, activated = await _enable(client, pair, clock)
    access = activated["access_token"]
    r = await client.post(
        f"{TOTP}/{route}", json={"password": PW, "code": "000000"}, headers=_h(access)
    )
    assert r.status_code == 403 and r.json()["error"]["code"] == "auth.invalid_code"
    # Nothing changed: TOTP is still on, and the original recovery codes still work.
    me = (await client.get(f"{AUTH}/me", headers=_h(access))).json()
    assert me["totp_enabled"] is True and me["recovery_codes_left"] == 10
    token = await _pending(client)
    ok = await client.post(
        TOTP, json={"totp_token": token, "recovery_code": activated["recovery_codes"][0]}
    )
    assert ok.status_code == 200


async def test_a_code_used_at_login_is_refused_at_disable(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    pair = await _setup(client)
    secret, _ = await _enable(client, pair, clock)
    token = await _pending(client)
    used = code(secret, clock)
    signed_in = await client.post(TOTP, json={"totp_token": token, "code": used})
    assert signed_in.status_code == 200
    r = await client.post(
        f"{TOTP}/disable",
        json={"password": PW, "code": used},
        headers=_h(signed_in.json()["access_token"]),
    )
    assert r.status_code == 403 and r.json()["error"]["code"] == "auth.invalid_code"


async def test_pending_token_dies_on_sign_out_everywhere_alone(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    """Any future sign-out-everywhere that doesn't change the password must still kill
    the pending token (only session_revoked covers that)."""
    from datetime import UTC, datetime, timedelta

    import jwt as pyjwt

    from app.models import User

    pair = await _setup(client)
    secret, _ = await _enable(client, pair, clock)
    token = await _pending(client)
    # One millisecond after the token's own issue time, not "now": a cutoff in the
    # same millisecond correctly spares the token (strict <), which made this test
    # flaky when login and the update ran within one ms.
    iat_ms = pyjwt.decode(token, options={"verify_signature": False})["iat_ms"]
    cutoff = datetime.fromtimestamp(iat_ms / 1000, tz=UTC) + timedelta(milliseconds=1)
    async with db.get_sessionmaker()() as s:
        await s.execute(
            update(User).where(User.handle == "alice").values(sessions_valid_after=cutoff)
        )
        await s.commit()
    r = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert r.status_code == 403 and r.json()["error"]["code"] == "auth.totp_expired"


async def test_pending_token_dies_when_the_user_is_disabled(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    from app.models import User

    pair = await _setup(client)
    secret, _ = await _enable(client, pair, clock)
    token = await _pending(client)
    async with db.get_sessionmaker()() as s:
        await s.execute(update(User).where(User.handle == "alice").values(status="disabled"))
        await s.commit()
    r = await client.post(TOTP, json={"totp_token": token, "code": code(secret, clock)})
    assert r.status_code == 403 and r.json()["error"]["code"] == "auth.totp_expired"


async def test_a_recovery_code_only_works_for_its_owner(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    admin = await _setup(client)
    _s1, alice_on = await _enable(client, admin, clock)
    body = {"handle": "bob", "display_name": "Bob", "password": PW}
    await client.post(f"{AUTH}/register", json=body, headers=_h(alice_on["access_token"]))
    bob = dict((await _login(client, "bob")).json())
    await _enable(client, bob, clock)
    bob_token = (await _login(client, "bob")).json()["totp_token"]
    r = await client.post(
        TOTP, json={"totp_token": bob_token, "recovery_code": alice_on["recovery_codes"][0]}
    )
    assert r.status_code == 403 and r.json()["error"]["code"] == "auth.invalid_code"


async def test_a_wrong_recovery_code_counts_toward_the_budget(
    client: httpx.AsyncClient, clock: Clock
) -> None:
    pair = await _setup(client)
    await _enable(client, pair, clock)
    ratelimit.get_limiter.cache_clear()
    lim = ratelimit.get_limiter()
    for _ in range(lim.config.code_budget - 1):
        lim.code_failure("alice")
    token = await _pending(client)
    r = await client.post(
        TOTP, json={"totp_token": token, "recovery_code": "0000-0000-0000-0000-0000"}
    )
    assert r.status_code == 403
    assert ("totp_guessing", None, "api") in await _events()


async def test_a_password_recheck_earns_no_trust(client: httpx.AsyncClient, clock: Clock) -> None:
    """Trust = a completed login (spec §6). Re-entering the password for enroll /
    disable / regenerate must not exempt the IP from the code budget."""
    pair = await _setup(client)
    ratelimit.get_limiter.cache_clear()  # forget the trust the setup login earned
    lim = ratelimit.get_limiter()
    r = await client.post(f"{TOTP}/enroll", json={"password": PW}, headers=_h(pair["access_token"]))
    assert r.status_code == 200  # a successful password re-check
    for _ in range(lim.config.code_budget):
        lim.code_failure("alice")
    assert lim.code_check("alice", "127.0.0.1") is not None
