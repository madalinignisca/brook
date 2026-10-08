# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Application settings, loaded from environment (prefix ``BROOK_``)."""

from __future__ import annotations

import ipaddress
from functools import lru_cache
from urllib.parse import urlsplit

from pydantic import SecretStr, field_validator
from pydantic_settings import BaseSettings, SettingsConfigDict

_DEV_JWT_KEY = "dev-insecure-change-me"  # noqa: S105 - sentinel for the guard, not a secret

# Where this server's source lives (AGPL-3.0 section 13: whoever runs a modified server must
# offer its users the source of that version). Unmodified servers point at upstream.
DEFAULT_SOURCE_URL = "https://github.com/madalinignisca/brook"
# The same number is SOURCE_URL_MAX_BYTES in core/src/server_info.rs, so a value this server
# accepts is never refused by a client. Change both together.
SOURCE_URL_MAX_BYTES = 2048


class Settings(BaseSettings):
    """Runtime configuration. Override via env vars or a local ``.env``."""

    model_config = SettingsConfigDict(env_prefix="BROOK_", env_file=".env", extra="ignore")

    # Storage / DB are opaque to the app (see docs/SECURITY.md §4a). Default is a
    # local SQLite file for dev; production sets a Postgres URL via env.
    database_url: str = "sqlite+aiosqlite:///./brook_dev.db"

    # Phase 0 creates tables on startup. Production with Alembic migrations sets
    # this to false so startup never runs ``create_all`` (avoids migration drift).
    auto_create_schema: bool = True

    # Auth / sessions (lifetimes per docs/SECURITY.md §7).
    jwt_signing_key: str = _DEV_JWT_KEY  # nosec B105 - dev default; guarded at startup
    jwt_algorithm: str = "HS256"
    access_ttl_seconds: int = 15 * 60

    # TOTP: the name authenticator apps show next to the account (otpauth issuer).
    totp_issuer: str = "Brook"

    # Attachments (spec 2026-09-25-attachments): plain files on the local disk.
    files_dir: str = "./data/files"
    files_max_bytes: int = 100 * 1024 * 1024
    files_quota_bytes: int = 5 * 1024 * 1024 * 1024
    # Refuse uploads that would leave less than this free: the disk is shared (on the
    # production host, with the git server), and attachments must never fill it.
    files_min_free_bytes: int = 5 * 1024 * 1024 * 1024
    refresh_ttl_seconds: int = 7 * 24 * 3600
    # A rotated refresh token presented again within this window is accepted once, if
    # its successor was never used: the client crashed, or lost our reply to a dropped
    # connection and then stayed offline, before saving the new token. Later, or with
    # the successor used, it's theft (_rotate). 24 h: owner decision, 2026-09-25.
    refresh_reuse_grace_seconds: int = 24 * 3600

    # Calls (Phase 4). Unset URL = no SFU configured: call commands answer
    # `sfu_unavailable`, everything else works.
    janus_url: str | None = None
    janus_api_secret: str = ""

    # Serves the dev-only call harness at /dev/call (app/static/call_harness.html).
    # Off by default: it is a test tool, not a product client.
    dev_harness: bool = False

    # Encryption of stored secrets (TOTP, bot secrets): a keyring of AES-256 keys,
    # `1:<base64url 32 bytes>,2:<...>`, and the explicit primary (encrypting) key id.
    # Required at startup (spec 2026-09-22 §5.7). SecretStr: never printed.
    secret_keys: SecretStr | None = None
    secret_primary_key_id: int | None = None

    # Auth rate limiting (app/ratelimit.py). In-process, per worker; defaults suit a
    # single node. Keys are client IPs (X-Forwarded-For is trusted only from 127.0.0.1).
    ratelimit_max_keys: int = 10_000
    ratelimit_burst: float = 10.0
    ratelimit_per_minute: float = 10.0
    ratelimit_backoff_after: int = 5
    ratelimit_go_away_per_hour: int = 50

    # AGPL section 13 (spec 2026-10-08-agpl-source-url): an operator running a modified
    # server sets this to where that modified source is. Served on the public GET /health.
    # An empty value counts as unset (see _unset_source_url_is_default).
    source_url: str = DEFAULT_SOURCE_URL

    # Must be explicitly enabled to run with a weak/default JWT key (local dev only).
    allow_insecure_auth: bool = False

    @field_validator("secret_primary_key_id", mode="before")
    @classmethod
    def _unset_primary_is_none(cls, value: object) -> object:
        # Compose passes an unset variable as "" (`${VAR:-}`): treat that as not set, so
        # the keyring guard refuses it with its own message instead of an int parse error.
        return None if value == "" else value

    @field_validator("source_url", mode="before")
    @classmethod
    def _unset_source_url_is_default(cls, value: object) -> object:
        # Same reason as above: compose passes an unset `${BROOK_SOURCE_URL:-}` as "", which
        # must mean "use the upstream default", not "refuse to start".
        return DEFAULT_SOURCE_URL if value == "" else value

    def assert_secure(self) -> None:
        """Refuse to start with a forgeable JWT key, an unusable secret keyring or a
        malformed source URL. Only the JWT key and keyring checks can be waived
        (allow_insecure_auth, local dev only)."""
        self._assert_source_url()
        self._assert_secret_keyring()
        weak = self.jwt_signing_key == _DEV_JWT_KEY or len(self.jwt_signing_key) < 32
        if weak and not self.allow_insecure_auth:
            raise RuntimeError(
                "BROOK_JWT_SIGNING_KEY must be a strong (>=32 char) non-default value. "
                "For local dev set BROOK_ALLOW_INSECURE_AUTH=1."
            )

    def _assert_source_url(self) -> None:
        """The source URL must be an absolute http(s) URL with a host and no userinfo, at most
        SOURCE_URL_MAX_BYTES, that a client will show unchanged.

        Refusing at startup (instead of falling back to upstream) catches an operator's typo
        at the moment they make it, rather than silently serving a link they did not mean.
        allow_insecure_auth does not excuse this: that hatch is about the JWT key and
        keyring only. The message never repeats the value: userinfo may hold a password.
        """
        if not _source_url_is_valid(self.source_url):
            raise RuntimeError(
                "BROOK_SOURCE_URL must be an absolute http(s) URL with a host, no user or "
                f"password, at most {SOURCE_URL_MAX_BYTES} bytes. "
                "Unset it to use the upstream repository."
            )

    def _assert_secret_keyring(self) -> None:
        """The keyring must parse, every key be 32 bytes, ids be unique, and the
        primary be in the ring. Only the dev escape hatch may omit it, and then a
        fixed dev key is used: never plaintext, never TOTP disabled."""
        from .secretbox import KeyringError, SecretBox, parse_keyring

        if self.secret_keys is None:
            if self.allow_insecure_auth:
                return
            raise RuntimeError(
                "BROOK_SECRET_KEYS and BROOK_SECRET_PRIMARY_KEY_ID are required "
                "(`make init` generates them). For local dev set BROOK_ALLOW_INSECURE_AUTH=1."
            )
        try:
            keys = parse_keyring(self.secret_keys.get_secret_value())
            if self.secret_primary_key_id is None:
                raise KeyringError("BROOK_SECRET_PRIMARY_KEY_ID is not set")
            SecretBox(keys, self.secret_primary_key_id)
        except KeyringError as exc:
            # The message names ids and lengths, never key material.
            raise RuntimeError(f"secret keyring rejected: {exc}") from None


# WHATWG forbidden host code points that urlsplit keeps. "%" also covers a percent-encoded
# host, which core's url parser decodes and re-checks. "\" is a path separator to WHATWG
# for http(s): core would show https://good.example\evil/ as https://good.example/evil/,
# a different link from the one configured.
_FORBIDDEN_HOST_CHARS = frozenset("<>^|%\\")


def _source_url_is_valid(value: str) -> bool:
    """True if value is a link every client will accept and show as configured."""
    if len(value.encode("utf-8")) > SOURCE_URL_MAX_BYTES:
        return False
    # Core's url parser does not refuse whitespace/control characters, it rewrites them
    # (strips leading/trailing C0 and spaces, drops tabs and newlines, percent-encodes an
    # inner space), so the link a client shows would differ from what the operator set.
    # Zero-width/format characters are not whitespace to Python and are not covered here.
    if any(c.isspace() or ord(c) < 0x20 or ord(c) == 0x7F for c in value):
        return False
    try:
        parts = urlsplit(value)
        _ = parts.port  # reading it is the check: it raises ValueError when invalid
    except ValueError:
        return False
    host = parts.hostname
    if parts.scheme not in ("http", "https") or not host:
        return False
    # Catches user@, user:pass@ and the empty "@host" (.username reports "" for that).
    if "@" in parts.netloc:
        return False
    if "[" in parts.netloc:  # IPv6 literal: urlsplit already validated the brackets
        return True
    # The rest mirrors what core's WHATWG host parser refuses but urlsplit lets through.
    # It is the known gap, not a full UTS 46 implementation (see the plan's Risks).
    if any(c in _FORBIDDEN_HOST_CHARS for c in host):
        return False
    # WHATWG ignores one trailing empty label, so "1.2.3.4." is an IPv4 address.
    bare = host[:-1] if host.endswith(".") else host
    last = bare.rsplit(".", 1)[-1]
    # ASCII digits only: str.isdigit() is also true for characters WHATWG does not read
    # as numbers. A numeric last label means "this is an IPv4 address", so it must be one.
    if last.isascii() and last.isdigit():
        try:
            ipaddress.IPv4Address(bare)
        except ValueError:
            return False
    for label in bare.split("."):
        if label.startswith("xn--") and not _is_valid_punycode_label(label):
            return False
    return True


def _is_valid_punycode_label(label: str) -> bool:
    # The raw "punycode" codec, not "idna": the idna codec is IDNA 2003 (nameprep) and
    # refuses valid IDNA 2008 hosts that core accepts, such as xn--fa-hia (faß).
    try:
        decoded = label[4:].encode("ascii").decode("punycode")
    except (UnicodeError, ValueError):
        return False
    # All-ASCII output (e.g. "xn--abc-") is not a valid IDN label; control characters
    # (e.g. "xn--a" decodes to U+0080) are refused by core.
    return not decoded.isascii() and not any(
        ord(c) < 0x20 or 0x7F <= ord(c) <= 0x9F for c in decoded
    )


@lru_cache
def get_settings() -> Settings:
    """Cached settings accessor (one instance per process)."""
    return Settings()
