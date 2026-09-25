"""Local auth tests: bootstrap, authz, login, refresh rotation."""

from __future__ import annotations

import httpx

API = "/api/v1/auth"


async def _register(client: httpx.AsyncClient, handle: str, **kw: str) -> httpx.Response:
    body = {"handle": handle, "display_name": handle.title(), "password": "supersecret"}
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
