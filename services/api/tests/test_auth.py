"""Local auth tests: bootstrap, authz, login, refresh rotation."""

from __future__ import annotations

import httpx
import pytest
from sqlalchemy import func, select
from sqlalchemy.ext.asyncio import AsyncSession

from app import db
from app.models import User

API = "/api/v1/auth"


async def _register(client: httpx.AsyncClient, handle: str, **kw: str) -> httpx.Response:
    # admin_password is ignored by the bootstrap and required for every later account.
    body = {
        "handle": handle,
        "display_name": handle.title(),
        "password": "supersecret",
        "admin_password": "supersecret",
    }
    return await client.post(f"{API}/register", json=body, **kw)


async def test_first_user_bootstraps_as_admin(client: httpx.AsyncClient) -> None:
    resp = await _register(client, "alice")
    assert resp.status_code == 201
    assert resp.json()["global_role"] == "admin"


async def test_second_register_requires_admin(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")  # first = admin
    # anonymous second registration is forbidden
    forbidden = await _register(client, "bob")
    assert forbidden.status_code == 403
    assert forbidden.json()["error"]["code"] == "authz.forbidden"

    # admin can create a member
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    access = login.json()["access_token"]
    created = await _register(client, "bob", headers={"Authorization": f"Bearer {access}"})
    assert created.status_code == 201
    assert created.json()["global_role"] == "member"


async def test_login_me_and_bad_password(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    assert login.status_code == 200
    access = login.json()["access_token"]

    me = await client.get(f"{API}/me", headers={"Authorization": f"Bearer {access}"})
    assert me.status_code == 200
    assert me.json()["handle"] == "alice"

    bad = await client.post(f"{API}/login", json={"handle": "alice", "password": "nope"})
    assert bad.status_code == 401


async def test_refresh_rotates_and_revokes_old(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    old_refresh = login.json()["refresh_token"]

    rotated = await client.post(f"{API}/refresh", json={"refresh_token": old_refresh})
    assert rotated.status_code == 200
    assert rotated.json()["refresh_token"] != old_refresh

    # once the new one has been used, the old one is dead (reuse: test_refresh_reuse.py)
    newer = await client.post(
        f"{API}/refresh", json={"refresh_token": rotated.json()["refresh_token"]}
    )
    assert newer.status_code == 200
    reused = await client.post(f"{API}/refresh", json={"refresh_token": old_refresh})
    assert reused.status_code == 401


async def test_me_requires_auth(client: httpx.AsyncClient) -> None:
    # A *missing* token is 401 "not authenticated", not a 403 authorization failure.
    resp = await client.get(f"{API}/me")
    assert resp.status_code == 401
    assert resp.headers["WWW-Authenticate"] == "Bearer"
    assert resp.json()["error"]["code"] == "auth.unauthorized"


async def test_duplicate_handle_conflicts(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")  # admin
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    access = login.json()["access_token"]
    dup = await _register(client, "alice", headers={"Authorization": f"Bearer {access}"})
    assert dup.status_code == 409
    assert dup.json()["error"]["code"] == "conflict"


async def test_invalid_bearer_token_rejected(client: httpx.AsyncClient) -> None:
    # An *invalid* token (vs. a missing one) is auth.invalid_token, also 401.
    resp = await client.get(f"{API}/me", headers={"Authorization": "Bearer not-a-real-token"})
    assert resp.status_code == 401
    assert resp.json()["error"]["code"] == "auth.invalid_token"


async def test_login_unknown_handle_is_401(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    resp = await client.post(f"{API}/login", json={"handle": "ghost", "password": "whatever-long"})
    assert resp.status_code == 401


async def test_error_uses_brook_envelope_with_code(client: httpx.AsyncClient) -> None:
    # Brook's wire contract (PROTOCOL.md §5): {"error": {code, message}}, NOT
    # FastAPI's default {"detail": ...}. Clients parse this shape.
    await _register(client, "alice")
    resp = await client.post(f"{API}/login", json={"handle": "alice", "password": "nope-long"})
    assert resp.status_code == 401
    body = resp.json()
    assert "detail" not in body
    assert body["error"]["code"] == "auth.invalid_credentials"
    assert body["error"]["message"]


async def test_validation_error_uses_envelope(client: httpx.AsyncClient) -> None:
    # A malformed body (password too short) → 422 in the same envelope.
    resp = await client.post(
        f"{API}/register", json={"handle": "x", "display_name": "X", "password": "short"}
    )
    assert resp.status_code == 422
    body = resp.json()
    assert "detail" not in body
    assert body["error"]["code"] == "validation.error"
    assert body["error"]["details"]["errors"]


async def test_logout_revokes_refresh_token(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    refresh_token = login.json()["refresh_token"]

    out = await client.post(f"{API}/logout", json={"refresh_token": refresh_token})
    assert out.status_code == 204

    reused = await client.post(f"{API}/refresh", json={"refresh_token": refresh_token})
    assert reused.status_code == 401


def _b64(raw: bytes) -> str:
    import base64

    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


async def test_an_undecodable_token_is_refused_never_a_server_error(
    client: httpx.AsyncClient,
) -> None:
    # The JWS header is parsed before the signature is checked, so it is attacker-controlled.
    # Nested past Python's recursion limit it escaped pyjwt <= 2.13 as a raw RecursionError,
    # which none of our handlers catches (a 500). pyjwt 2.15 raises DecodeError: a 401.
    # The depth that triggers it depends on the Python version: about 10k levels (a 27 KB
    # token, small enough for a header or a WebSocket frame) on 3.12, which production and CI
    # run, and about 100k on 3.14. 200k fails on both, so the test does not depend on it
    # (the WebSocket tests in test_ws_commands.py use smaller payloads and only discriminate
    # on 3.12 and 3.13).
    depth = 200_000
    header = _b64(b"[" * depth + b"]" * depth)
    token = f"{header}.{_b64(b'{}')}.{_b64(b'sig')}"
    resp = await client.get("/api/v1/channels", headers={"Authorization": f"Bearer {token}"})
    assert resp.status_code == 401


async def _admin_headers(client: httpx.AsyncClient) -> dict[str, str]:
    await _register(client, "alice")  # first = admin, password "supersecret"
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    return {"Authorization": f"Bearer {login.json()['access_token']}"}


async def _count_users(handle: str) -> int:
    async with db.get_sessionmaker()() as s:
        n = await s.scalar(select(func.count()).select_from(User).where(User.handle == handle))
    return int(n or 0)


async def test_register_needs_the_admin_password(client: httpx.AsyncClient) -> None:
    """A stolen admin access token alone must not mint accounts (encryption spec §7.6):
    without the admin's password nothing is created."""
    admin = await _admin_headers(client)
    body = {"handle": "mallory", "display_name": "M", "password": "mallory-pass"}

    missing = await client.post(f"{API}/register", json=body, headers=admin)
    assert missing.status_code == 422
    assert missing.json()["error"]["code"] == "validation.error"

    wrong = await client.post(
        f"{API}/register", json={**body, "admin_password": "not-it-at-all"}, headers=admin
    )
    assert wrong.status_code == 403
    assert wrong.json()["error"]["code"] == "auth.invalid_credentials"
    assert await _count_users("mallory") == 0

    ok = await client.post(
        f"{API}/register", json={**body, "admin_password": "supersecret"}, headers=admin
    )
    assert ok.status_code == 201


async def test_register_checks_role_then_password_then_handle(client: httpx.AsyncClient) -> None:
    admin = await _admin_headers(client)
    assert (await _register(client, "bob", headers=admin)).status_code == 201
    login = await client.post(f"{API}/login", json={"handle": "bob", "password": "supersecret"})
    member = {"Authorization": f"Bearer {login.json()['access_token']}"}

    # A member is refused on role before their password is looked at.
    bad = {"handle": "carol", "display_name": "C", "password": "supersecret"}
    r = await client.post(
        f"{API}/register", json={**bad, "admin_password": "supersecret"}, headers=member
    )
    assert r.status_code == 403 and r.json()["error"]["code"] == "authz.forbidden"

    # A taken handle with a wrong admin password is a 403, not a 409: handles of
    # existing accounts are not probed by someone who cannot prove they are the admin.
    taken = {**bad, "handle": "bob", "admin_password": "wrong-password"}
    r = await client.post(f"{API}/register", json=taken, headers=admin)
    assert r.status_code == 403 and r.json()["error"]["code"] == "auth.invalid_credentials"
    # ...and with the right one it is the conflict.
    r = await client.post(
        f"{API}/register", json={**taken, "admin_password": "supersecret"}, headers=admin
    )
    assert r.status_code == 409


async def test_register_with_a_refused_token_is_401_not_403(client: httpx.AsyncClient) -> None:
    """Clients refresh on 401 only: an expired token must not read as "not allowed"."""
    await _register(client, "alice")
    r = await _register(client, "bob", headers={"Authorization": "Bearer not-a-real-token"})
    assert r.status_code == 401
    assert r.json()["error"]["code"] == "auth.invalid_token"
    assert await _count_users("bob") == 0


async def test_bootstrap_ignores_a_stale_token(client: httpx.AsyncClient) -> None:
    """A client that kept a token from a wiped server must still be able to set up the
    first account: there is nobody to refresh against."""
    r = await _register(client, "alice", headers={"Authorization": "Bearer stale-token"})
    assert r.status_code == 201
    assert r.json()["global_role"] == "admin"


async def test_register_display_name_limit_is_one_rule(client: httpx.AsyncClient) -> None:
    admin = await _admin_headers(client)
    base = {"password": "supersecret", "admin_password": "supersecret"}
    ok = await client.post(
        f"{API}/register", json={**base, "handle": "n64", "display_name": "n" * 64}, headers=admin
    )
    assert ok.status_code == 201
    long_ = await client.post(
        f"{API}/register", json={**base, "handle": "n65", "display_name": "n" * 65}, headers=admin
    )
    assert long_.status_code == 422
    assert long_.json()["error"]["code"] == "validation.error"


async def test_register_losing_a_handle_race_is_409(
    client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Two registers of one handle both pass the existence check; the unique column
    picks one. The other must be a 409, not a 500. The rival row is committed by a
    second session right before this request's own commit, i.e. after its check."""
    admin = await _admin_headers(client)
    real_commit = AsyncSession.commit
    rival_done = False

    async def commit_after_rival(self: AsyncSession) -> None:
        nonlocal rival_done
        if not rival_done:
            rival_done = True
            async with db.get_sessionmaker()() as other:
                other.add(User(handle="bob", display_name="Rival", password_hash="x"))
                await other.commit()
        await real_commit(self)

    monkeypatch.setattr(AsyncSession, "commit", commit_after_rival)
    r = await _register(client, "bob", headers=admin)
    assert r.status_code == 409
    assert r.json()["error"]["code"] == "conflict"
    assert await _count_users("bob") == 1
