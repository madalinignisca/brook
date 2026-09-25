"""Host-side admin commands: ``uv run python -m app.cli <command> ...`` on the server.

For what the HTTP API deliberately can't do: an admin who lost both their phone and
their recovery codes can't be reset over HTTP (admins never reset admins, and not
themselves), so the operator does it here, with shell access as the authority.
"""

from __future__ import annotations

import argparse
import asyncio
import sys

from sqlalchemy import select

from .db import get_sessionmaker
from .events import record_event
from .models import User
from .routers.auth import lock_user, sign_out_everywhere
from .routers.totp import remove_totp


async def totp_reset(handle: str) -> int:
    """Remove ``handle``'s TOTP and recovery codes and sign them out everywhere.

    Open WebSockets of that user are held by the api process, not this one: they are
    refused at their next re-auth or reconnect (the cutoff is in the database) and
    otherwise end when their access token expires (<= 15 min)."""
    async with get_sessionmaker()() as session:
        user_id = await session.scalar(select(User.id).where(User.handle == handle))
        if user_id is None:
            print(f"no such user: {handle}", file=sys.stderr)
            return 1
        user = await lock_user(session, user_id)
        assert user is not None  # noqa: S101 - just selected, under our lock now
        await remove_totp(session, user.id)
        record_event(session, user.id, "totp_reset", via="host_cli")
        await sign_out_everywhere(session, user)
        await session.commit()
    print(f"TOTP removed for {handle}; all their sessions are signed out.")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m app.cli")
    sub = parser.add_subparsers(dest="command", required=True)
    reset = sub.add_parser("totp-reset", help="remove a user's TOTP (lost device and codes)")
    reset.add_argument("handle")
    args = parser.parse_args(argv)
    if args.command == "totp-reset":
        return asyncio.run(totp_reset(args.handle))
    return 2


if __name__ == "__main__":
    sys.exit(main())
