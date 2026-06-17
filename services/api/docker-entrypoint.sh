#!/bin/sh
# Container entrypoint: bring the schema to head, then serve.
#
# Running `alembic upgrade head` on every start is idempotent (a no-op when
# already current) and makes the deployed path identical to the upgrade
# rehearsal — deploy a new image, it migrates, it serves. Single-node only
# (Phase 0); multiple replicas would need a separate migration job to avoid
# racing. Schema creation is owned by migrations here, so BROOK_AUTO_CREATE_SCHEMA
# must stay false in deployed environments.
set -e

echo "[entrypoint] applying database migrations (alembic upgrade head)..."
alembic upgrade head

echo "[entrypoint] starting API server..."
exec uvicorn app.main:app --host 0.0.0.0 --port 8000 "$@"
