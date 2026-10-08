# Brook — working rules for agents

Brook is a small, self-hosted team chat with native clients on a shared Rust core. Read
[README.md](README.md) for the goals and non-goals, and the file in `docs/` that covers the
area you touch ([ARCHITECTURE](docs/ARCHITECTURE.md), [PROTOCOL](docs/PROTOCOL.md),
[QUALITY](docs/QUALITY.md)). Each component directory has its own README. These rules come
from the owner and apply to every change.

## 1. A new feature goes spec → plan → implementation

1. **Spec**: what problem, who it is for, what is out of scope, open questions. Put it in
   `docs/superpowers/specs/YYYY-MM-DD-<name>-spec.md` (or `-design.md`).
2. **Plan**: the files to touch, the order, the risks, and how each step is tested. Same folder,
   `...-plan.md`.
3. **Implementation**: only after the plan is agreed.

A small fix does not need a spec or plan; say so in the PR instead. When the owner has not
approved a spec or plan, stop and ask. Do not start coding.

## 2. Opus reviews every stage, as a subagent

The newest Opus available (today Opus 5.5) reviews the spec, then the plan, then the
implementation. Use the `brook-reviewer` agent, and give it a pinned worktree so a branch
switch cannot change files under it mid-review. Point it at the evidence, not the style. Fix
what it finds and have it check again before moving on. A change to authentication or
authorization also goes to `auth-reviewer`. Every PR also gets a codex review and an LGTM from
the owner of each area it touches; reviewers end with a `VERDICT:` line. Do not change the
codex settings.

## 3. Run the tests at every relevant step

Run the tests of what you changed after each step, not once at the end. Run the linter against
`origin/main`, as CI does. A bug fix ships with a test that fails without the fix: change the
code, watch the test go red, restore it. A test that skips itself at runtime is a bug. Say in
the PR what you did not test, and why.

| Part | Run (from its directory) |
|---|---|
| `services/api` | `uv run ruff check . && uv run ruff format --check . && uv run mypy app && uv run coverage run -m pytest && uv run coverage report` |
| `core`, `clients/gnome`, `bindings/apple` | `cargo fmt --all -- --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked` |

Apple and KDE clients have their own steps in their README. [QUALITY](docs/QUALITY.md) is the
full standard.

Fix the code; never loosen the check. Do not edit lint, type, test or CI settings, and do not
add `noqa`, `nolint`, `# type: ignore` or skip markers, to make a check pass. If a rule is
wrong for the code, stop and ask the owner.

## 4. Keep it simple

The owner wants the smallest thing that works. Do not add options, layers, abstractions or
dependencies for a future that has not been asked for. Prefer a few plain lines over a
framework. Do not build for old clients or old data before 1.0.0; the production database is kept through migrations.

## 5. Comment for the junior who inherits this

Write comments so a junior developer who has never seen the code can learn from it. Say *why*:
why it is this way, what breaks otherwise, what was tried and dropped, and which rule or
document it comes from. Do not repeat what the line already says. Keep each comment focused on
the code next to it; a long comment that wanders loses the reader. If a reason lives only in a
chat, it is lost.

## 6. Keep the written record current

Every change that alters behavior updates, in the same PR: the spec and plan it came from,
[PROTOCOL](docs/PROTOCOL.md) when the wire contract moves, the user and admin guides, any ADR
it affects (a real design decision gets a new one in `docs/adr/NNNN-title.md`), and the agent memory notes it makes wrong
(fix or delete them). A doc that no longer matches the code is a bug.

## 7. Commits and pull requests

- **Subject**: one short line saying what changed, prefixed by the area (`api:`, `core:`,
  `gnome:`, `mac:`, `docs:`, `ci:`).
- **Body**: 3 to 4 sentences. What changed, why, and anything a reviewer could miss.
- One self-contained change per commit; commit as you go.
- A PR opens from an issue, links it (`Closes #N`), and states what would be true if the
  change were broken and what you ran to show it is not. Green CI alone is not that.
- The owner merges, or the author after CI is green, the LGTMs are in and `main` is merged in.
  Never force-push `main`.

## 8. Safety

- Ask before touching production. Take a backup first; never loosen a security check to get
  past a problem.
- No secrets, tokens or real passwords in the repo, commits, PRs or logs.
- Do not put agent or session names in the repo or on GitHub.
