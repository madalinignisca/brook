"""WebSocket realtime test: a message sent by one account is delivered live to
another over the hub. This is the Phase 1 acceptance check (two accounts chat).
"""

from __future__ import annotations

from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from app import config, db
from app.main import create_app

PW = "supersecret"


def _reset(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("BROOK_DATABASE_URL", f"sqlite+aiosqlite:///{tmp_path / 'ws.db'}")
    monkeypatch.setenv("BROOK_JWT_SIGNING_KEY", "test-signing-key-at-least-32-bytes-long!")
    config.get_settings.cache_clear()
    db._engine = None
    db._sessionmaker = None


def test_ws_delivers_message_new(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _reset(tmp_path, monkeypatch)
    app = create_app()

    with TestClient(app) as http:  # entering runs the lifespan → schema created

        def register(handle: str, token: str | None = None) -> None:
            headers = {"Authorization": f"Bearer {token}"} if token else {}
            r = http.post(
                "/api/v1/auth/register",
                json={"handle": handle, "display_name": handle.title(), "password": PW},
                headers=headers,
            )
            assert r.status_code == 201, r.text

        def login(handle: str) -> str:
            r = http.post("/api/v1/auth/login", json={"handle": handle, "password": PW})
            return str(r.json()["access_token"])

        register("alice")  # first user → admin
        alice = login("alice")
        register("bob", token=alice)
        bob = login("bob")

        dm = http.post(
            "/api/v1/channels",
            json={"kind": "dm", "member": "bob"},
            headers={"Authorization": f"Bearer {alice}"},
        ).json()

        with http.websocket_connect("/ws") as ws:
            ws.send_json({"type": "auth", "data": {"access_token": bob}})
            assert ws.receive_json()["type"] == "ready"  # subscribed; no fan-out race

            http.post(
                f"/api/v1/channels/{dm['id']}/messages",
                json={"body": "hi bob"},
                headers={"Authorization": f"Bearer {alice}"},
            )

            event = ws.receive_json()
            assert event["type"] == "message.new"
            assert event["data"]["body"] == "hi bob"
            assert event["data"]["author_handle"] == "alice"
            assert event["data"]["channel_id"] == dm["id"]

    config.get_settings.cache_clear()
    db._engine = None
    db._sessionmaker = None


def test_ws_rejects_missing_auth(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _reset(tmp_path, monkeypatch)
    app = create_app()
    with TestClient(app) as http, http.websocket_connect("/ws") as ws:
        # First frame is not an auth command → server closes the socket.
        ws.send_json({"type": "typing", "data": {}})
        with pytest.raises(Exception):  # noqa: B017 - starlette raises on the close
            ws.receive_json()
    config.get_settings.cache_clear()
    db._engine = None
    db._sessionmaker = None
