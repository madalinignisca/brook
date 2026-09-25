# Brook API — production image (reproducible, non-root)
FROM python:3.12-slim

ENV PYTHONUNBUFFERED=1 \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    PATH="/app/.venv/bin:$PATH"

# uv for locked, reproducible installs
RUN pip install --no-cache-dir uv

WORKDIR /app

# Install dependencies from the lockfile first (cached layer); no dev deps.
COPY pyproject.toml uv.lock ./
RUN uv sync --locked --no-dev --no-install-project

# App code + migrations, then install the project itself against the locked env.
COPY app ./app
COPY alembic ./alembic
COPY alembic.ini docker-entrypoint.sh ./
RUN chmod +x docker-entrypoint.sh && uv sync --locked --no-dev

# Run as an unprivileged user.
# /data/files: the attachments volume's mount point. A new named volume copies this
# directory's ownership, so appuser can write to it (a root-owned volume would not).
RUN useradd --create-home --uid 10001 appuser \
    && mkdir -p /data/files \
    && chown -R appuser:appuser /app /data/files
USER appuser

EXPOSE 8000

# Liveness check without extra packages (no curl in slim).
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD python -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://localhost:8000/health').status==200 else 1)"

# Behind Caddy (TLS). The entrypoint runs `alembic upgrade head`, then serves
# (binds all interfaces inside the container network).
CMD ["./docker-entrypoint.sh"]
