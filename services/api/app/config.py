"""Application settings, loaded from environment (prefix ``BROOK_``)."""

from __future__ import annotations

from functools import lru_cache

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
    refresh_ttl_seconds: int = 7 * 24 * 3600

    # Must be explicitly enabled to run with a weak/default JWT key (local dev only).
    allow_insecure_auth: bool = False

    def assert_secure(self) -> None:
        """Refuse to start with a forgeable JWT key unless explicitly allowed."""
        weak = self.jwt_signing_key == _DEV_JWT_KEY or len(self.jwt_signing_key) < 32
        if weak and not self.allow_insecure_auth:
            raise RuntimeError(
                "BROOK_JWT_SIGNING_KEY must be a strong (>=32 char) non-default value. "
                "For local dev set BROOK_ALLOW_INSECURE_AUTH=1."
            )


@lru_cache
def get_settings() -> Settings:
    """Cached settings accessor (one instance per process)."""
    return Settings()
