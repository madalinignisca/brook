# Brook — working rules for agents

Brook is a small, self-hosted team chat with native clients on a shared Rust core. Read
[README.md](README.md) for the goals and non-goals, and the doc in `docs/` for the area you
touch ([ARCHITECTURE](docs/ARCHITECTURE.md), [PROTOCOL](docs/PROTOCOL.md),
[QUALITY](docs/QUALITY.md)). Most component directories have their own README.

## 1. A new feature goes spec → plan → implementation

1. **Spec**: the problem, who it is for, what is out of scope, open questions. File:
   `docs/superpowers/specs/YYYY-MM-DD-<name>-spec.md` (or `-design.md`).
2. **Plan**: the files to touch, the order, the risks, and how each step is tested. Same
   folder, `...-plan.md`.
3. **Implementation**, only after the plan is approved.

Each stage is reviewed (section 2), the findings fixed, and then the owner approves it. Do not
start the next stage before that. A small fix needs no spec or plan; say so in the PR.

**Who does what.** The main agent talks to the owner, decides, and hands each job to a
subagent on the newest model of its tier. It does not write the work itself.

| Job | Subagent model |
|---|---|
| Spec and plan | Opus |
| Review of every stage | Opus (section 2) |
| Code and its tests | Sonnet |
| Docs, after the code works | Haiku |
| Commit messages | Haiku |

Haiku sees only the diff, so hand it the *why* in the prompt; it writes the *what*. A design
decision that comes up while implementing goes back to the main agent and the owner, not to
the implementing subagent. "Newest" means the newest model of that tier: when a new one ships,
change the model pinned in the agent's definition.

## 2. Opus reviews every stage, as a subagent

A newest-Opus subagent, never the one that wrote the work, reviews the spec, then the plan,
then the implementation, using the
`brook-reviewer` subagent in a pinned worktree (a branch switch must not change files under
it). For a spec or plan, hand it the file and the question "does this solve the stated
problem?"; for code, the PR and its evidence. It attacks the evidence, not the style. Fix what
it finds and have it check again. A change to authentication or authorization also goes to
`auth-reviewer`, in addition to Opus. Reviewers end with a `VERDICT:` line.

## 3. Run the tests at every relevant step

Run the tests of what you changed after each step, not once at the end. A bug fix ships with a
test that fails without it: break the code, watch the test go red, restore it. A test that
skips itself at runtime is a bug. The PR says what you did not test, and why.

Before pushing, merge `origin/main` into your branch and run what CI runs:

| Part | Run |
|---|---|
| `services/api` (from that directory, after `uv sync --locked --extra dev`) | `uv run ruff check . && uv run ruff format --check . && uv run mypy app && uv run coverage run -m pytest && uv run coverage report && uv run bandit -q -r app tests -s B101,B105,B106 && uv run pip-audit --skip-editable` |
| Rust: `core`, `clients/gst-media`, `clients/gnome`, `bindings/apple` (from the repo root) | `cargo fmt --all -- --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked` |
| macOS | `clients/macos/build.sh test` |

CI also runs `cargo-deny` and the API tests against Postgres. `pre-commit install` runs the
cheap checks on every commit. [QUALITY](docs/QUALITY.md) has the full standard.

Fix the code; never loosen the check. Do not edit lint, type, test or CI settings, and do not
add `noqa`, `nolint`, `# type: ignore` or skip markers, to make a check pass. If a rule is
wrong for the code, stop and ask the owner.

## 4. Keep it simple

The owner wants the smallest thing that works. No options, layers, abstractions or
dependencies for a future nobody asked for; a few plain lines beat a framework. Before 1.0.0,
do not build for old clients or old data. The production database is kept through migrations.

## 5. Comment for the junior who inherits this

Write comments so a junior developer who has never seen the code can learn from it. Say *why*:
why it is this way, what breaks otherwise, what was tried and dropped, which rule or doc it
comes from. Do not repeat what the line says, and stay on the code next to the comment.

## 6. Keep the written record current

Once the code works, a Haiku subagent updates the docs. A change that alters behavior updates,
in the same PR: the spec and plan it came from,
[PROTOCOL](docs/PROTOCOL.md) if the wire contract moves, the
[user guide](docs/user-guide.md) and [admin guide](docs/admin-guide.md), and any ADR it
affects. A real design decision gets a new ADR in `docs/adr/` (`0001-title.md`, then
`0002-...`). Also fix or delete any agent memory note the change makes wrong; those live
outside the repo. A doc that no longer matches the code is a bug.

## 7. Commits and pull requests

- **Subject**: one short line, prefixed by the area (e.g. `api:`, `core:`, `gnome:`, `mac:`,
  `ci:`, `docs:`).
- **Body**: 3 to 4 sentences. What changed, why, and anything a reviewer could miss. A Haiku
  subagent writes both, from the diff and the *why* you give it.
- One self-contained change per commit; commit as you go.
- Work starts from an issue, and the PR links it (`Closes #N`). The PR states what would be
  true if the change were broken and what you ran to show it is not. Green CI alone is not that.
- Only the owner merges into `main`, once CI is green and the reviews are in. Merging `main`
  into the branch afterwards resets the approval.

## 8. Safety

No secrets, tokens or real passwords in the repo, commits, PRs or logs. In a workflow, pin
actions by commit SHA and pass event data to scripts only through `env:`. Ask before touching
production. Do not put agent or session names in the repo or on GitHub.
