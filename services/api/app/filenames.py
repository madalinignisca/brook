"""Attachment filenames: the owner's rules (attachments spec §4).

Every file gets two names:
- ``filename``: transliterated to ASCII and sanitised, the name every client saves
  under. It must be acceptable on every OS and device ("we don't need some filename
  to erase a device").
- ``original_name``: the name as typed, with only invisible and control characters
  removed. Display text only ("alt text"), never a filesystem name.

Transliteration uses anyascii (ISC); Unidecode is GPL. Everything else is here.
"""

from __future__ import annotations

import re
import unicodedata

from anyascii import anyascii

MAX_BYTES = 255
FALLBACK = "file"

# Characters no filesystem or shell should ever see: C0/C1 controls and DEL.
_CONTROL = re.compile(r"[\x00-\x1f\x7f-\x9f]")
# Bidi controls (LRE, RLE, PDF, LRO, RLO, LRI, RLI, FSI, PDI, LRM, RLM, ALM) and other
# invisible format characters: "invoice\u202efdp.exe" must not display as "invoiceexe.pdf".
_INVISIBLE = re.compile(r"[\u200b-\u200f\u202a-\u202e\u2060-\u2069\u061c\ufeff]")
# Forbidden in Windows names; / and \ are handled as path separators first.
_RESERVED = re.compile(r'[<>:"|?*]')
_SPACES = re.compile(r"\s+")
# Windows device names, reserved with any extension: "CON", "con.txt", "COM1.log".
# Windows treats "CON .txt" (spaces before the dot) as the device too.
_DEVICE = re.compile(r"^(con|prn|aux|nul|com[1-9]|lpt[1-9])\s*(\..*)?$", re.IGNORECASE)


def _strip_invisible(name: str) -> str:
    return _INVISIBLE.sub("", _CONTROL.sub("", name))


def original_name(raw: str) -> str:
    """The name for display: NFC, invisible/control characters removed, capped."""
    shown = _strip_invisible(unicodedata.normalize("NFC", raw)).strip()
    return shown[:512] or FALLBACK


def safe_filename(raw: str) -> str:
    """The name every client saves under (see the module docstring for the rules)."""
    name = _strip_invisible(unicodedata.normalize("NFC", raw))
    name = anyascii(name)
    name = _strip_invisible(name)  # anyascii output is ASCII, but be sure of controls
    # Last path segment only: "../../etc/passwd" and "C:\\evil\\x" can't climb.
    name = re.split(r"[\\/]", name)[-1]
    name = _RESERVED.sub("_", name)
    name = _SPACES.sub(" ", name)
    only_ext = re.fullmatch(r"\.+([A-Za-z0-9]{1,16})", name.strip(" "))
    if only_ext:  # ".exe" (e.g. what's left of "\u202e.exe"): keep it as file.exe
        return f"{FALLBACK}.{only_ext.group(1)}"
    name = name.strip(" .")
    if not name:
        return FALLBACK
    if _DEVICE.match(name):
        name = "_" + name
    return _cap(name)


def _cap(name: str) -> str:
    """At most MAX_BYTES. Owner decision: extensions win, the name shrinks. The whole
    multi-part extension is kept (``.tar.gz``, ``.min.js.map``: up to 3 short trailing
    parts) and only the stem is shortened."""
    if len(name.encode()) <= MAX_BYTES:
        return name
    parts = name.split(".")
    ext_parts: list[str] = []
    # Walk back over short alphanumeric parts, keeping at least one character of stem.
    while len(parts) > 1 and len(ext_parts) < 3 and re.fullmatch(r"[A-Za-z0-9]{1,8}", parts[-1]):
        ext_parts.insert(0, parts.pop())
    stem = ".".join(parts)
    suffix = "".join(f".{p}" for p in ext_parts)
    room = MAX_BYTES - len(suffix.encode())
    stem = stem.encode()[:room].decode("ascii", "ignore").rstrip(" .") or FALLBACK
    return stem + suffix
