"""The API must never answer with a redirect.

A 307/308 preserves method and body, so a redirect on a login route re-sends the
password to whatever URL the Location header names. Behind a TLS-terminating proxy
(Caddy) uvicorn sees plain http, so FastAPI's default trailing-slash redirect built
``Location: http://...`` for an ``https://`` request: the server itself issued an
https->http downgrade carrying credentials. A JSON API has no use for redirects: a
wrong path should be a 404, never a hop.

Clients are hardened separately (core refuses to follow redirects), but that only
protects our own clients; anything else that follows redirects would still leak.
"""

from __future__ import annotations

import httpx

from app.main import create_app


def _route_variants() -> list[tuple[str, str]]:
    """Every documented (method, path), plus its trailing-slash twin.

    Read from the OpenAPI schema rather than ``app.routes``: FastAPI >= 0.137 mounts
    included routers lazily (``_IncludedRouter``), so walking ``app.routes`` finds only
    the docs pages and this test would pass while checking nothing.
    """
    out: list[tuple[str, str]] = []
    for path, ops in create_app().openapi()["paths"].items():
        for method in sorted(ops):
            out.append((method.upper(), path))
            out.append((method.upper(), path + "/"))
    return out


def test_route_list_is_not_empty() -> None:
    # Guards the test below: an empty enumeration would make it pass vacuously.
    assert len(_route_variants()) >= 10


async def test_no_api_route_answers_with_a_redirect(client: httpx.AsyncClient) -> None:
    offenders = []
    for method, path in _route_variants():
        body = {"handle": "x", "password": "y"} if method == "POST" else None
        r = await client.request(method, path, json=body, follow_redirects=False)
        if 300 <= r.status_code < 400:
            offenders.append(f"{method} {path} -> {r.status_code} {r.headers.get('location')}")
    assert offenders == [], "API issued redirects:\n" + "\n".join(offenders)
