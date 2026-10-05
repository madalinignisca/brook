# Plan: admin adds users (#265)

Each step builds and tests alone. The server PR need not land first: the current server ignores the extra field.

1. **Core TestServer route** (`core/src/test_support.rs`): `POST /api/v1/auth/register` through `authed(...)`, with modes
   Created, Conflict, Forbidden, WrongAdmin, Echo422, RateLimited, BadBody (201 with `{}`), Gated.
2. **Core `create_user`** (`core/src/account.rs`, beside `list_users`): epoch snapshot, `ctx().send(epoch, OnExpired::SingleFlight, ..)`,
   `account_error` with the `validation` message replaced, `UnexpectedResponse` on an unparseable 201. Tests in `account_tests.rs`
   (sends four fields with bearer and returns the summary; conflict; forbidden; wrong admin password; 422 never echoes; rate limited;
   refreshes once on 401; signed-out sends nothing; unparseable 201) and `log_secrecy_tests.rs` (never logs either password).
3. **Binding** (`bindings/apple/src/client.rs`): `create_user(..) -> FfiUserSummary`; test that the arguments reach the right JSON keys;
   in the same step add `createUser` to `FakeRealtime` in `BrookTests/CallModelTests.swift` (the regenerated protocol would stop the test target compiling).
4. **Swift model** (`Account/AccountModels.swift`): `createUser` on `AccountClient`; `NewUserPolicy`, `PasswordGenerator`, `AddUserModel`
   (`problem`, `generate()`, `submit()` with a busy guard and `lastNoAnswerHandle`, `clear()`); `FakeAccount` records the call; tests in `AddUserModelTests.swift`.
5. **View and menu** (`AccountViews.swift`, `SignedInView.swift`): `AddUserSheet`, `.onDisappear { model.clear() }`, `Add User…` inside the admin block,
   visibility as a static predicate with a test. By hand on the owner's Mac: items 1, 12, 13 and autofill behaviour.

If it stops halfway: after 1-2 nothing visible changes (core has an unused public method, GTK can use it); after 3 the Swift side can call it but no UI does;
after 4 the model is tested with no menu entry. Nothing deletes or migrates data; rollback is a revert.

Mutants that must die: swap password/admin_password in the body; drop bearer or use the wrong refresh mode; Ok on non-2xx; 409 as validation; 422 body in the
message; retry on 403; skip the epoch check; remove the busy guard; leave a secret out of `clear()`; not clearing after success; swap the forbidden and
invalid-credentials texts; `<` for `<=` on each length bound; skip the no-answer handle match; `generate()` filling only Password; the menu predicate true for member.
