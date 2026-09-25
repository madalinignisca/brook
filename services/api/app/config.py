"""Application settings, loaded from environment (prefix ``BROOK_``)."""

from __future__ import annotations

from functools import lru_cache

from pydantic import SecretStr, field_validator
from pydantic_settings import BaseSettings, SettingsConfigDict

_DEV_JWT_KEY = "dev-insecure-change-me"  # noqa: S105 - sentinel for the guard, not a secret


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
    # its successor was never used: the client crashed after our rotation but before
    # saving the new token. Later, or with the successor used, it's theft (_rotate).
    refresh_reuse_grace_seconds: int = 30

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

    # Must be explicitly enabled to run with a weak/default JWT key (local dev only).
    allow_insecure_auth: bool = False

    @field_validator("secret_primary_key_id", mode="before")
    @classmethod
    def _unset_primary_is_none(cls, value: object) -> object:
        # Compose passes an unset variable as "" (`${VAR:-}`): treat that as not set, so
        # the keyring guard refuses it with its own message instead of an int parse error.
        return None if value == "" else value

    def assert_secure(self) -> None:
        """Refuse to start with a forgeable JWT key or an unusable secret keyring,
        unless explicitly allowed (local dev only)."""
        self._assert_secret_keyring()
        weak = self.jwt_signing_key == _DEV_JWT_KEY or len(self.jwt_signing_key) < 32
        if weak and not self.allow_insecure_auth:
            raise RuntimeError(
                "BROOK_JWT_SIGNING_KEY must be a strong (>=32 char) non-default value. "
                "For local dev set BROOK_ALLOW_INSECURE_AUTH=1."
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


@lru_cache
def get_settings() -> Settings:
    """Cached settings accessor (one instance per process)."""
    return Settings()
