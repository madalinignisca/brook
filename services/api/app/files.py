"""Attachment bytes on the local filesystem (attachments spec §2, owner decision).

Layout: ``<files_dir>/<aa>/<file_id>``, where ``aa`` is the id's first two hex digits,
so no directory holds more than ~1/256 of the files. In-progress uploads are
``<id>.<random>.part`` beside it, one per PUT (O_EXCL), so two overlapping uploads of
one file can never interleave their bytes; only the one that commits is renamed into
place (os.replace, atomic on one filesystem).

No object storage: "for a humble human with a little server on budget, local
filesystem is a bless". Durability is the volume's (replicated, with scheduled volume
backups); this module only has to be correct and never fill the disk.
"""

from __future__ import annotations

import hashlib
import os
import secrets
import shutil
import time
import uuid
from pathlib import Path

from .config import get_settings

PART_SUFFIX = ".part"


def root() -> Path:
    return Path(get_settings().files_dir)


def final_path(file_id: uuid.UUID) -> Path:
    return root() / file_id.hex[:2] / str(file_id)


def new_part_path(file_id: uuid.UUID) -> Path:
    return root() / file_id.hex[:2] / f"{file_id}.{secrets.token_hex(8)}{PART_SUFFIX}"


def free_bytes() -> int:
    """Free space on the files filesystem, for unprivileged writers."""
    path = root()
    path.mkdir(parents=True, exist_ok=True)
    return shutil.disk_usage(path).free


class PartWriter:
    """One upload's part file: exclusive create, streaming sha256, fsync, and a
    guaranteed cleanup unless :meth:`commit_to` moved it into place."""

    def __init__(self, file_id: uuid.UUID) -> None:
        self.path = new_part_path(file_id)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        # O_EXCL: this PUT's own file; never shared with another upload.
        self._fd = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o640)
        self.size = 0
        self._hash = hashlib.sha256()
        self._done = False

    def write(self, chunk: bytes) -> None:
        self.size += len(chunk)
        self._hash.update(chunk)
        view = memoryview(chunk)
        while view:
            written = os.write(self._fd, view)
            view = view[written:]

    @property
    def sha256(self) -> str:
        return self._hash.hexdigest()

    def finish(self) -> None:
        """Flush to disk before the rename makes the file visible."""
        os.fsync(self._fd)
        os.close(self._fd)
        self._fd = -1

    def commit_to(self, final: Path) -> None:
        os.replace(self.path, final)
        self._done = True

    def discard(self) -> None:
        if self._fd >= 0:
            os.close(self._fd)
            self._fd = -1
        if not self._done:
            self.path.unlink(missing_ok=True)
            self._done = True


def remove(file_id: uuid.UUID) -> None:
    """Remove a file's bytes and any of its part files (best effort)."""
    final = final_path(file_id)
    final.unlink(missing_ok=True)
    for part in final.parent.glob(f"{file_id}.*{PART_SUFFIX}"):
        part.unlink(missing_ok=True)


def stale_entries(older_than_s: float) -> list[Path]:
    """Every file under the root last modified more than ``older_than_s`` ago: the
    sweep's candidates (orphans, abandoned parts). A part is only ever removed when it
    is this old, so the sweep never deletes one mid-upload."""
    base = root()
    if not base.exists():
        return []
    cutoff = time.time() - older_than_s
    return [p for p in base.glob("*/*") if p.is_file() and p.stat().st_mtime < cutoff]
