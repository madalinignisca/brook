"""End-to-end call test: real stack (api + Janus) and two real Chrome participants.

What it proves (the question "what would be true if calls were broken?"):
signaling finishing is not enough: each browser must DECODE VIDEO FRAMES that the
other one sent, through Janus, and the counters must keep rising. Then one side
leaves, and the other must see `left` and a renegotiation that removes the video.

Needs a running stack with the media profile and BROOK_DEV_HARNESS=true, e.g.
    docker compose -p brook-dev --env-file <env> --profile media up -d --build
Run:  uv run --with playwright --with httpx python e2e/call_e2e.py http://127.0.0.1:18080
Chrome gets fake camera/mic devices, so no hardware is needed.
"""

from __future__ import annotations

import asyncio
import secrets
import sys
import time

import httpx
from playwright.async_api import Page, async_playwright

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18080"
CHROME = "/usr/bin/google-chrome"


def setup() -> tuple[str, dict[str, str]]:
    """Fresh users + a channel with both as members. Returns (channel_id, passwords)."""
    tag = secrets.token_hex(3)
    pw = {f"alice{tag}": secrets.token_urlsafe(16), f"bob{tag}": secrets.token_urlsafe(16)}
    a, b = list(pw)
    with httpx.Client(base_url=BASE, timeout=10) as c:
        admin_hdr: dict[str, str] = {}
        # The stack may already have an admin (from earlier runs): then the first
        # registration needs one. This script only runs on throwaway dev stacks.
        r = c.post(
            "/api/v1/auth/register", json={"handle": a, "display_name": "Alice", "password": pw[a]}
        )
        if r.status_code == 403:
            raise SystemExit("stack already has users: run on a fresh dev stack (make reset)")
        r.raise_for_status()
        tok = c.post("/api/v1/auth/login", json={"handle": a, "password": pw[a]}).json()[
            "access_token"
        ]
        admin_hdr = {"Authorization": f"Bearer {tok}"}
        c.post(
            "/api/v1/auth/register",
            headers=admin_hdr,
            json={"handle": b, "display_name": "Bob", "password": pw[b]},
        ).raise_for_status()
        ch = c.post(
            "/api/v1/channels", headers=admin_hdr, json={"kind": "channel", "name": f"e2e-{tag}"}
        ).json()
        c.post(
            f"/api/v1/channels/{ch['id']}/members", headers=admin_hdr, json={"handle": b}
        ).raise_for_status()
    return ch["id"], pw


async def wait_for(page: Page, js: str, what: str, limit_s: float = 20) -> object:
    end = time.monotonic() + limit_s
    while time.monotonic() < end:
        v = await page.evaluate(js)
        if v:
            return v
        errs = await page.evaluate("window.brook && window.brook.errors")
        if errs:
            raise AssertionError(f"{what}: page errors {errs}")
        await asyncio.sleep(0.25)
    raise AssertionError(f"timed out waiting for {what}")


async def diagnose(page: Page, who: str) -> str:
    state = await page.evaluate("window.brook.pcState()")
    await asyncio.sleep(12)  # past the 10 s request timeout: pending replies resolve
    log = await page.evaluate("document.getElementById('log').textContent")
    return f"\n--- {who} pc state: {state}\n--- {who} harness log:\n{log}"


async def media_flowing(page: Page, who: str) -> dict[str, int]:
    """Two stats samples 2 s apart: bytes AND decoded frames must both increase."""
    try:
        return await _media_flowing(page, who)
    except AssertionError as exc:
        raise AssertionError(str(exc) + await diagnose(page, who)) from None


async def _media_flowing(page: Page, who: str) -> dict[str, int]:
    s1 = await page.evaluate("window.brook.stats()")
    await asyncio.sleep(2)
    s2 = await page.evaluate("window.brook.stats()")
    # Only growth is required: the first sample may legitimately still be 0.
    assert s2["inboundVideoBytes"] > s1["inboundVideoBytes"] and s2["inboundVideoBytes"] > 0, (
        f"{who}: no inbound video {s1}->{s2}"
    )
    assert s2["framesDecoded"] > s1["framesDecoded"], f"{who}: no frames decoded {s1}->{s2}"
    return s2


async def main() -> None:
    channel, pw = setup()
    (a, pa), (b, pb) = pw.items()
    async with async_playwright() as p:
        browser = await p.chromium.launch(
            executable_path=CHROME,
            args=["--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream"],
        )
        pages = []
        for handle, password in ((a, pa), (b, pb)):
            page = await (await browser.new_context()).new_page()
            page.on(
                "console",
                lambda m, h=handle: m.type == "error" and print(f"[{h}] console:", m.text),
            )
            await page.goto(
                f"{BASE}/dev/call?handle={handle}&password={password}&channel={channel}&autojoin=1"
            )
            pages.append(page)
        alice, bob = pages

        for page, who in ((alice, "alice"), (bob, "bob")):
            await wait_for(page, "window.brook.state === 'published'", f"{who} published")
        for page, who in ((alice, "alice"), (bob, "bob")):
            await wait_for(
                page,
                "Object.values(window.brook.remote).some(r => r.kind === 'video')",
                f"{who} receives a remote video track",
            )
        sa = await media_flowing(alice, "alice")
        sb = await media_flowing(bob, "bob")
        fa, fb = sa["framesDecoded"], sb["framesDecoded"]
        print(f"PASS media both ways: alice decoded {fa} frames, bob {fb}")

        # The track alice sees must be attributed to bob's participant, via the
        # per-receiver mid mapping in call.subscribe.offer (contract §3.4).
        owner = await alice.evaluate(
            "Object.values(window.brook.remote).find(r => r.kind==='video').participant_id"
        )
        names = await alice.evaluate(
            "Object.values(window.brook.participants).map(p => [p.participant_id, p.display_name])"
        )
        assert [n for pid, n in names if pid == owner] == ["Bob"], (
            f"mid mapping wrong: {owner} {names}"
        )
        print("PASS remote video on alice's side is attributed to Bob")

        # Screen share (PROTOCOL.md §3.3 `tracks`): alice adds a third m-line labelled
        # "screen" on her SAME publish PC. Janus does not add it to bob's existing
        # subscription by itself; the server must, and bob must DECODE it.
        await alice.evaluate("window.brook.shareScreen(true)")
        screen_mid = await wait_for(
            bob,
            "(Object.entries(window.brook.remote)"
            ".find(([m, r]) => r.source === 'screen') || [])[0]",
            "bob receives alice's screen stream",
        )
        f1 = (await bob.evaluate("window.brook.framesByMid()")).get(screen_mid, 0)
        await asyncio.sleep(2)
        f2 = (await bob.evaluate("window.brook.framesByMid()")).get(screen_mid, 0)
        assert f2 > f1, f"screen stream not decoding on mid {screen_mid}: {f1}->{f2}"
        print(f"PASS screen share: bob decodes alice's screen on mid {screen_mid} ({f2} frames)")
        await alice.evaluate("window.brook.stopScreen()")
        await wait_for(
            bob,
            "!Object.values(window.brook.remote).some(r => r.source === 'screen')",
            "bob's screen stream goes away when alice stops sharing",
        )
        print("PASS stop sharing: screen stream renegotiated away")

        await bob.evaluate("window.brook.leave()")
        await wait_for(
            alice, "Object.keys(window.brook.participants).length === 0", "alice sees bob leave"
        )
        await wait_for(
            alice,
            "!Object.values(window.brook.remote).some(r => r.kind === 'video')",
            "alice's renegotiation drops bob's video",
        )
        print("PASS leave: participant left + renegotiated away")
        await browser.close()


if __name__ == "__main__":
    asyncio.run(main())
