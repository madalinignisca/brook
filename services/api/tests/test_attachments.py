"""Attachments over the api (attachments spec §9): local files, streaming cap, atomic
commit, attach rules, authorisation, download headers, limits, sweep, delete."""

from __future__ import annotations

import asyncio
import hashlib
import os
import uuid
from datetime import timedelta
from pathlib import Path

import httpx
import pytest
from sqlalchemy import select, update

from app import db
from app import files as storage
from app.models import File, utcnow

AUTH = "/api/v1/auth"
PW = "supersecret"


def _h(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


async def _login(client: httpx.AsyncClient, handle: str) -> dict[str, str]:
    r = await client.post(f"{AUTH}/login", json={"handle": handle, "password": PW})
    return _h(r.json()["access_token"])


async def _setup(client: httpx.AsyncClient) -> tuple[dict[str, str], dict[str, str], str]:
    """alice (admin) and bob in one channel."""
    await client.post(
        f"{AUTH}/register", json={"handle": "alice", "display_name": "A", "password": PW}
    )
    ha = await _login(client, "alice")
    await client.post(
        f"{AUTH}/register", json={"handle": "bob", "display_name": "B", "password": PW}, headers=ha
    )
    hb = await _login(client, "bob")
    ch = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "g"}, headers=ha)
    ).json()
    await client.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": "bob"}, headers=ha)
    return ha, hb, ch["id"]


async def _create(
    client: httpx.AsyncClient,
    h: dict[str, str],
    ch: str,
    data: bytes,
    name: str = "a.txt",
    **extra: object,
) -> httpx.Response:
    body = {"filename": name, "size": len(data), "content_type": "text/plain", **extra}
    return await client.post(f"/api/v1/channels/{ch}/files", json=body, headers=h)


async def _upload(
    client: httpx.AsyncClient, h: dict[str, str], ch: str, data: bytes, **kw: object
) -> dict:
    created = await _create(client, h, ch, data, **kw)
    assert created.status_code == 201, created.text
    put = await client.put(created.json()["upload_url"], content=data, headers=h)
    assert put.status_code == 200, put.text
    return dict(put.json())


def _dir_files() -> list[Path]:
    return [p for p in storage.root().glob("*/*") if p.is_file()]


# ---------------------------------------------------------------- upload


async def test_upload_commits_atomically_with_sha256(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    data = os.urandom(200_000)
    out = await _upload(client, ha, ch, data, name="../../Ștefan.txt")
    assert out["status"] == "committed"
    assert out["sha256"] == hashlib.sha256(data).hexdigest()
    assert out["filename"] == "Stefan.txt" and out["original_name"] == "../../Ștefan.txt"
    # On disk under its UUID only, with no part files left.
    files = _dir_files()
    assert [p.name for p in files] == [out["id"]]
    assert files[0].read_bytes() == data


async def test_more_bytes_than_declared_is_413_and_leaves_nothing(
    client: httpx.AsyncClient,
) -> None:
    ha, _hb, ch = await _setup(client)
    created = (await _create(client, ha, ch, b"x" * 10)).json()
    r = await client.put(created["upload_url"], content=b"x" * 11, headers=ha)
    assert r.status_code == 413
    assert _dir_files() == []


async def test_fewer_bytes_than_declared_is_422(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    created = (await _create(client, ha, ch, b"x" * 10)).json()
    r = await client.put(created["upload_url"], content=b"x" * 9, headers=ha)
    assert r.status_code == 422 and r.json()["error"]["code"] == "file.size_mismatch"
    assert _dir_files() == []


async def test_second_put_gets_already_committed_with_the_file(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    data = b"hello world"
    out = await _upload(client, ha, ch, data)
    again = await client.put(f"/api/v1/files/{out['id']}/content", content=data, headers=ha)
    assert again.status_code == 409
    err = again.json()["error"]
    assert err["code"] == "file.already_committed"
    assert err["details"]["sha256"] == hashlib.sha256(data).hexdigest()


async def test_overlapping_puts_commit_exactly_one_intact_file(client: httpx.AsyncClient) -> None:
    """Two uploads of one pending file racing: one commits with exactly its own bytes,
    the other gets file.already_committed, and no part file is left (spec test 2a)."""
    ha, _hb, ch = await _setup(client)
    a, b = b"A" * 300_000, b"B" * 300_000
    created = (await _create(client, ha, ch, a)).json()
    url = created["upload_url"]
    r1, r2 = await asyncio.gather(
        client.put(url, content=a, headers=ha), client.put(url, content=b, headers=ha)
    )
    codes = sorted([r1.status_code, r2.status_code])
    assert codes == [200, 409]
    winner = r1 if r1.status_code == 200 else r2
    stored = storage.final_path(uuid.UUID(created["file"]["id"])).read_bytes()
    assert stored in (a, b) and hashlib.sha256(stored).hexdigest() == winner.json()["sha256"]
    assert len(_dir_files()) == 1  # the final file only; both parts are gone


async def test_create_is_idempotent_with_client_id(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    cid = str(uuid.uuid4())
    one = await _create(client, ha, ch, b"xyz", client_id=cid)
    two = await _create(client, ha, ch, b"xyz", client_id=cid)
    assert one.status_code == 201 and two.status_code == 200
    assert one.json()["file"]["id"] == two.json()["file"]["id"]


# ---------------------------------------------------------------- attach


async def test_attach_and_see_it_in_history(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    f = await _upload(client, ha, ch, b"pdf-bytes", name="report.pdf")
    msg = await client.post(
        f"/api/v1/channels/{ch}/messages",
        json={"body": "see file", "attachments": [f["id"]]},
        headers=ha,
    )
    assert msg.status_code == 201
    assert [a["id"] for a in msg.json()["attachments"]] == [f["id"]]
    history = (await client.get(f"/api/v1/channels/{ch}/messages", headers=hb)).json()
    assert history[0]["attachments"][0]["filename"] == "report.pdf"


async def test_a_message_may_be_files_only_but_not_empty(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    url = f"/api/v1/channels/{ch}/messages"
    f = await _upload(client, ha, ch, b"photo", name="photo.jpg")
    only_files = await client.post(url, json={"attachments": [f["id"]]}, headers=ha)
    assert only_files.status_code == 201
    assert only_files.json()["body"] == "" and only_files.json()["deleted_at"] is None
    assert [a["id"] for a in only_files.json()["attachments"]] == [f["id"]]
    nothing = await client.post(url, json={"body": ""}, headers=ha)
    assert nothing.status_code == 422
    blank = await client.post(url, json={"body": "  \n "}, headers=ha)  # no files: junk
    assert blank.status_code == 422
    captioned = await client.post(url, json={"body": "hi"}, headers=ha)
    assert captioned.status_code == 201


async def test_attach_rules(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    other = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "o"}, headers=ha)
    ).json()
    mine = await _upload(client, ha, ch, b"1")
    bobs = await _upload(client, hb, ch, b"2")
    elsewhere = await _upload(client, ha, other["id"], b"3")
    pending = (await _create(client, ha, ch, b"4")).json()["file"]
    url = f"/api/v1/channels/{ch}/messages"
    for bad in (bobs["id"], elsewhere["id"], pending["id"], str(uuid.uuid4())):
        r = await client.post(url, json={"body": "x", "attachments": [bad]}, headers=ha)
        assert r.status_code == 422 and r.json()["error"]["code"] == "file.not_attachable", bad
    ok = await client.post(url, json={"body": "x", "attachments": [mine["id"]]}, headers=ha)
    assert ok.status_code == 201
    twice = await client.post(url, json={"body": "y", "attachments": [mine["id"]]}, headers=ha)
    assert twice.status_code == 422  # already attached


# ---------------------------------------------------------------- download


async def test_download_headers_and_range(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    data = bytes(range(256)) * 10
    f = await _upload(client, ha, ch, data, name="page.html")
    async with db.get_sessionmaker()() as s:  # an active type: served as bytes
        await s.execute(update(File).values(content_type="text/html"))
        await s.commit()
    url = f"/api/v1/files/{f['id']}/content"
    r = await client.get(url, headers=hb)
    assert r.status_code == 200 and r.content == data
    assert r.headers["content-type"] == "application/octet-stream"
    assert r.headers["content-disposition"].startswith("attachment")
    assert "page.html" in r.headers["content-disposition"]
    assert r.headers["x-content-type-options"] == "nosniff"
    assert "sandbox" in r.headers["content-security-policy"]
    assert r.headers["etag"] == f'"{f["sha256"]}"'
    part = await client.get(
        url, headers={**hb, "Range": "bytes=100-199", "If-Range": r.headers["etag"]}
    )
    assert part.status_code == 206 and part.content == data[100:200]


async def test_download_authorisation(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    await client.post(
        f"{AUTH}/register", json={"handle": "eve", "display_name": "E", "password": PW}, headers=ha
    )
    he = await _login(client, "eve")
    f = await _upload(client, ha, ch, b"secret")
    assert (await client.get(f"/api/v1/files/{f['id']}/content", headers=he)).status_code == 404
    pending = (await _create(client, ha, ch, b"later")).json()["file"]
    assert (
        await client.get(f"/api/v1/files/{pending['id']}/content", headers=ha)
    ).status_code == 404
    # Eve can't start an upload into a channel she isn't in, nor PUT to alice's file.
    assert (await _create(client, he, ch, b"x")).status_code == 404
    put = await client.put(f"/api/v1/files/{pending['id']}/content", content=b"later", headers=he)
    assert put.status_code == 404


# ---------------------------------------------------------------- limits


async def test_limits(client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch) -> None:
    from app import config

    ha, _hb, ch = await _setup(client)
    body = {"filename": "big.bin", "size": 100 * 1024 * 1024 + 1, "content_type": "x/y"}
    r = await client.post(f"/api/v1/channels/{ch}/files", json=body, headers=ha)
    assert r.status_code == 413 and r.json()["error"]["code"] == "file.too_large"
    monkeypatch.setenv("BROOK_FILES_QUOTA_BYTES", "10")
    monkeypatch.setenv("BROOK_FILES_MIN_FREE_BYTES", "0")
    config.get_settings.cache_clear()
    assert (await _create(client, ha, ch, b"x" * 8)).status_code == 201
    over = await _create(client, ha, ch, b"x" * 3)  # 8 pending + 3 > 10
    assert over.status_code == 413 and over.json()["error"]["code"] == "file.quota_exceeded"
    monkeypatch.setenv("BROOK_FILES_QUOTA_BYTES", str(10**12))
    monkeypatch.setattr(storage, "free_bytes", lambda: 1000)
    monkeypatch.setenv("BROOK_FILES_MIN_FREE_BYTES", "995")
    config.get_settings.cache_clear()
    # 1000 free - 8 pending - 3 new = 989 < 995: the shared disk's floor holds.
    floor = await _create(client, ha, ch, b"x" * 3)
    assert floor.status_code == 507 and floor.json()["error"]["code"] == "file.no_space"
    # "Try later": clients keep the upload and retry after this long.
    assert floor.headers["Retry-After"] == "600"


# ---------------------------------------------------------------- lifecycle


async def test_message_delete_removes_its_files(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    f = await _upload(client, ha, ch, b"bye")
    msg = (
        await client.post(
            f"/api/v1/channels/{ch}/messages",
            json={"body": "x", "attachments": [f["id"]]},
            headers=ha,
        )
    ).json()
    assert (
        await client.delete(f"/api/v1/channels/{ch}/messages/{msg['id']}", headers=ha)
    ).status_code == 204
    assert _dir_files() == []
    assert (await client.get(f"/api/v1/files/{f['id']}/content", headers=ha)).status_code == 404


async def test_sweep(client: httpx.AsyncClient) -> None:
    from app.sweep import sweep_once

    ha, _hb, ch = await _setup(client)
    old_pending = (await _create(client, ha, ch, b"p")).json()["file"]
    unattached = await _upload(client, ha, ch, b"u")
    attached = await _upload(client, ha, ch, b"a")
    await client.post(
        f"/api/v1/channels/{ch}/messages",
        json={"body": "k", "attachments": [attached["id"]]},
        headers=ha,
    )
    fresh_part = storage.new_part_path(uuid.uuid4())
    fresh_part.parent.mkdir(parents=True, exist_ok=True)
    fresh_part.write_bytes(b"in progress")  # must survive: an upload mid-way
    orphan = storage.final_path(uuid.uuid4())
    orphan.parent.mkdir(parents=True, exist_ok=True)
    orphan.write_bytes(b"no row")
    old = (utcnow() - timedelta(hours=2)).timestamp()
    os.utime(orphan, (old, old))
    async with db.get_sessionmaker()() as s:
        await s.execute(
            update(File)
            .where(File.id == uuid.UUID(old_pending["id"]))
            .values(created_at=utcnow() - timedelta(hours=2))
        )
        await s.execute(
            update(File)
            .where(File.id == uuid.UUID(unattached["id"]))
            .values(created_at=utcnow() - timedelta(hours=25))
        )
        await s.commit()
    result = await sweep_once()
    assert result == {"pending": 1, "unattached": 1, "orphans": 1}
    async with db.get_sessionmaker()() as s:
        left = set((await s.scalars(select(File.id))).all())
    assert left == {uuid.UUID(attached["id"])}
    assert fresh_part.exists() and not orphan.exists()
    assert storage.final_path(uuid.UUID(attached["id"])).exists()


async def test_upload_whose_part_was_swept_asks_for_a_retry(
    client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A part removed by the sweep mid-commit (an upload stalled past 1 h): a clean
    409 file.upload_expired and the file stays pending, never a 500."""
    ha, _hb, ch = await _setup(client)
    created = (await _create(client, ha, ch, b"slow")).json()

    def vanished(self: storage.PartWriter, final: Path) -> None:
        self.path.unlink()
        raise FileNotFoundError(self.path)

    monkeypatch.setattr(storage.PartWriter, "commit_to", vanished)
    r = await client.put(created["upload_url"], content=b"slow", headers=ha)
    assert r.status_code == 409 and r.json()["error"]["code"] == "file.upload_expired"
    async with db.get_sessionmaker()() as s:
        row = await s.get(File, uuid.UUID(created["file"]["id"]))
        assert row is not None and row.status == "pending"


# ---------------------------------------------------------------- auth review of #81


async def test_parallel_puts_to_one_file_are_refused_while_one_streams(
    client: httpx.AsyncClient,
) -> None:
    """B1: a second PUT for a file already uploading is refused before it writes a byte,
    so K parallel PUTs can't multiply the disk use of one create."""
    from app.routers import files as files_router

    ha, _hb, ch = await _setup(client)
    created = (await _create(client, ha, ch, b"x" * 10)).json()
    fid = uuid.UUID(created["file"]["id"])
    files_router._in_flight.add(fid)  # an upload of this file is streaming right now
    try:
        r = await client.put(created["upload_url"], content=b"x" * 10, headers=ha)
    finally:
        files_router._in_flight.discard(fid)
    assert r.status_code == 409 and r.json()["error"]["code"] == "file.upload_in_progress"
    assert int(r.headers["retry-after"]) > 0  # transient: the outbox backs off
    assert _dir_files() == []  # it never opened a part file
    ok = await client.put(created["upload_url"], content=b"x" * 10, headers=ha)
    assert ok.status_code == 200 and not files_router._in_flight


async def test_disk_floor_is_rechecked_while_streaming(
    client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    from app import config
    from app.routers import files as files_router

    ha, _hb, ch = await _setup(client)
    created = (await _create(client, ha, ch, b"x" * 20_000)).json()
    monkeypatch.setattr(files_router, "FREE_CHECK_EVERY", 4096)
    free = iter([10**12] + [0] * 100)  # plenty at start, then the disk fills up
    monkeypatch.setattr(storage, "free_bytes", lambda: next(free))
    monkeypatch.setenv("BROOK_FILES_MIN_FREE_BYTES", "1000")
    config.get_settings.cache_clear()

    async def body():  # type: ignore[no-untyped-def]
        for _ in range(5):
            yield b"x" * 4000

    r = await client.put(created["upload_url"], content=body(), headers=ha)
    assert r.status_code == 507 and r.json()["error"]["code"] == "file.no_space"
    assert _dir_files() == []


async def test_file_delete_authorisation(client: httpx.AsyncClient) -> None:
    """B2: uploader or channel owner may delete; another member may not; a non-member
    gets 404."""
    ha, hb, ch = await _setup(client)  # alice created the channel: its owner
    await client.post(
        f"{AUTH}/register", json={"handle": "eve", "display_name": "E", "password": PW}, headers=ha
    )
    he = await _login(client, "eve")
    alices = await _upload(client, ha, ch, b"a")
    bobs = await _upload(client, hb, ch, b"b")
    assert (await client.delete(f"/api/v1/files/{alices['id']}", headers=hb)).status_code == 403
    assert (await client.delete(f"/api/v1/files/{alices['id']}", headers=he)).status_code == 404
    assert (
        await client.delete(f"/api/v1/files/{bobs['id']}", headers=ha)
    ).status_code == 204  # owner
    assert (
        await client.delete(f"/api/v1/files/{alices['id']}", headers=ha)
    ).status_code == 204  # uploader
    assert _dir_files() == []


async def test_no_uploads_into_an_archived_channel(client: httpx.AsyncClient) -> None:
    from app.models import Channel

    ha, _hb, ch = await _setup(client)
    async with db.get_sessionmaker()() as s:
        await s.execute(
            update(Channel).where(Channel.id == uuid.UUID(ch)).values(archived_at=utcnow())
        )
        await s.commit()
    r = await _create(client, ha, ch, b"x")
    assert r.status_code == 403


@pytest.mark.parametrize("bad", ["application/pdf\r\nSet-Cookie: x", "text", "a b/c", "x/y z"])
async def test_content_type_must_be_a_mime_token(client: httpx.AsyncClient, bad: str) -> None:
    ha, _hb, ch = await _setup(client)
    body = {"filename": "a", "size": 1, "content_type": bad}
    r = await client.post(f"/api/v1/channels/{ch}/files", json=body, headers=ha)
    assert r.status_code == 422 and r.json()["error"]["code"] == "file.bad_content_type"


async def test_file_record_reports_the_served_type(client: httpx.AsyncClient) -> None:
    """H2: a client building a blob with FileOut.content_type can't render SVG either."""
    ha, _hb, ch = await _setup(client)
    body = {"filename": "x.svg", "size": 3, "content_type": "Image/SVG+XML; charset=utf-8"}
    created = await client.post(f"/api/v1/channels/{ch}/files", json=body, headers=ha)
    assert created.json()["file"]["content_type"] == "application/octet-stream"
    ok = await client.post(
        f"/api/v1/channels/{ch}/files",
        json={"filename": "p.png", "size": 3, "content_type": "image/png"},
        headers=ha,
    )
    assert ok.json()["file"]["content_type"] == "image/png"


async def test_multi_range_is_refused(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    f = await _upload(client, ha, ch, b"0123456789")
    r = await client.get(
        f"/api/v1/files/{f['id']}/content", headers={**ha, "Range": "bytes=0-0,2-2"}
    )
    assert r.status_code == 416


async def test_a_stalled_upload_is_dropped_and_frees_its_slot(
    client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A half-open upload must not hold its in-flight slot until a proxy timeout."""
    from app.routers import files as files_router

    ha, _hb, ch = await _setup(client)
    created = (await _create(client, ha, ch, b"x" * 20)).json()
    monkeypatch.setattr(files_router, "IDLE_TIMEOUT_S", 0.2)

    async def stalls():  # type: ignore[no-untyped-def]
        yield b"x" * 5
        await asyncio.sleep(5)  # the client goes quiet
        yield b"x" * 15

    r = await client.put(created["upload_url"], content=stalls(), headers=ha)
    assert r.status_code == 408 and r.json()["error"]["code"] == "file.upload_stalled"
    assert not files_router._in_flight and _dir_files() == []
    ok = await client.put(created["upload_url"], content=b"x" * 20, headers=ha)
    assert ok.status_code == 200
