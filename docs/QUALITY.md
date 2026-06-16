# Quality, testing & CI standards

> Principle: **each sub-project uses the best-in-class, idiomatic toolchain for its stack** — no lowest-common-denominator. Quality is enforced, not aspirational.

## Enforcement (three layers)
1. **`pre-commit`** — fast local hooks (format, lint, secret scan) before every commit.
2. **GitHub Actions** — **path-filtered** per sub-project, so a Rust change doesn't run Python CI and vice-versa.
3. **Branch protection** on `main` — green required checks + review before merge.

Repo-wide: **`.editorconfig`**, markdownlint, **gitleaks** (secret scanning), Conventional-Commits-style messages (encouraged).

## Per sub-project toolchains

### `services/api` — Python / FastAPI
- **Ruff** — linter **and** formatter (one tool; replaces flake8/black/isort).
- **mypy** — static typing, `strict` mode.
- **pytest** (+ `pytest-asyncio`, `httpx.AsyncClient`) — unit + API tests; **coverage gate** (start ≥ 80%, ratchet up).
- **bandit** (code security) + **pip-audit** (dependency CVEs).
- Run: `ruff check . && ruff format --check . && mypy . && pytest`.

### `core/`, `clients/gnome/` — Rust
- **rustfmt** (format) + **clippy** with `-D warnings` (lint as errors).
- **`cargo test`** / **nextest**; coverage via `cargo llvm-cov`.
- **cargo-deny** — license + security-advisory + duplicate-dep checks.
- Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.

### `clients/kde/` — Qt 6 + Kirigami via CXX-Qt
- Rust side: rustfmt + clippy + cargo test (as above).
- QML: `qmllint` / `qmlformat`; **clang-format** if any hand-written C++ glue.
- QML unit tests where logic warrants.

### Future clients (reference)
- **macOS/iOS** (Swift): `swiftformat` + `swiftlint`, XCTest.
- **Windows** (C#): `dotnet format` + Roslyn analyzers, xUnit.
- **Android** (Kotlin): `ktlint`/`detekt`, JUnit + Compose UI tests.

## Testing philosophy
- **Test behavior, not implementation.** API tests hit real endpoints (via `httpx`/test client) against an ephemeral DB.
- **Every bug fix ships with a regression test.**
- **CI must be green to merge.** No skipping; a quarantined/flaky test is tracked and fixed, not ignored.
- Coverage is a floor that **ratchets upward**, never a vanity number — focus on meaningful paths (auth, authz, money/data integrity, protocol edges).

## Definition of done (per change)
Formatted ✓ · lint clean ✓ · types clean ✓ · tests added/updated & green ✓ · security checks pass ✓ · docs updated if behavior/contract changed ✓.
