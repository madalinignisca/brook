"""No write path bypasses sync stamping (cache-core plan, "Where this fails", C2).

app/sync.py stamps rows in a flush hook, which sees ORM objects only. A Core
``update(Message)`` / ``delete(Membership)`` / ``insert(...)`` goes straight to SQL,
unseen: the change gets no seq, and every offline cache misses it until the row
happens to change again. Nothing at runtime would notice, so this test reads the
source and fails on any such statement against a table the hook watches.

If it fails on your change, write through the ORM (load the rows, set attributes,
``session.delete(obj)``) so the hook sees them. Add to ALLOWED only with the reason
the change can't matter to a client's cache.
"""

from __future__ import annotations

import ast
import re
from pathlib import Path

APP = Path(__file__).resolve().parent.parent / "app"

# What app/sync.py watches: the stamped tables, plus the two whose changes re-stamp
# their message (a reaction; a deleted file that was attached).
WATCHED = {"Message", "Channel", "Membership", "User", "Reaction", "File"}
BULK = {"update", "delete", "insert"}
# The same writes as raw SQL, e.g. text("UPDATE messages ..."): the hook can't see those either.
RAW = re.compile(
    r"\b(?:UPDATE|DELETE\s+FROM|INSERT\s+INTO)\s+(messages|channels|memberships|users|reactions|files)\b",
    re.IGNORECASE,
)

# (file relative to app/, enclosing function, table) -> why it's safe.
ALLOWED = {
    # The hook itself stamps with Core statements: it is the stamping.
    ("sync.py", "_stamp", "Channel"): "the hook re-stamping channels",
    ("sync.py", "_stamp", "Message"): "the hook re-stamping messages",
    # pending -> committed on an upload: no message has the file yet.
    ("routers/files.py", "_stream_and_commit", "File"): "file not attached yet",
    # Unattached files only (pending, or committed but never attached).
    ("sweep.py", "sweep_once", "File"): "only files no message holds",
    # Deleting a message deletes its files in the same flush; the message's own
    # tombstone is stamped, and that's what clients sync.
    ("routers/channels.py", "delete_message", "File"): "the message itself is stamped",
}


def _bulk_writes() -> set[tuple[str, str, str]]:
    found: set[tuple[str, str, str]] = set()
    for path in APP.rglob("*.py"):
        rel = path.relative_to(APP).as_posix()
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for fn in ast.walk(tree):
            if not isinstance(fn, ast.FunctionDef | ast.AsyncFunctionDef):
                continue
            for node in ast.walk(fn):
                if isinstance(node, ast.Constant) and isinstance(node.value, str):
                    for table in RAW.findall(node.value):
                        found.add((rel, fn.name, f"sql:{table.lower()}"))
                if not (isinstance(node, ast.Call) and node.args):
                    continue
                # update(...) and sa.update(...) / sqlalchemy.update(...) alike
                name = (
                    node.func.id
                    if isinstance(node.func, ast.Name)
                    else node.func.attr
                    if isinstance(node.func, ast.Attribute)
                    else None
                )
                table = node.args[0]
                if name in BULK and isinstance(table, ast.Name) and table.id in WATCHED:
                    found.add((rel, fn.name, table.id))
    return found


def test_no_bulk_write_bypasses_sync_stamping() -> None:
    unexpected = _bulk_writes() - ALLOWED.keys()
    assert not unexpected, (
        "Core bulk writes skip app/sync.py's flush hook, so clients never sync these "
        f"changes; write through the ORM instead: {sorted(unexpected)}"
    )


def test_the_scan_sees_the_known_sites() -> None:
    # Guards the scanner itself: if it stopped finding anything (a refactor of the
    # AST walk, a moved app/), the test above would pass vacuously.
    assert ALLOWED.keys() <= _bulk_writes()
