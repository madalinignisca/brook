"""Secret encryption keyring (spec 2026-09-22 §5): format, binding, rotation,
fail-closed decryption and the startup guard."""

from __future__ import annotations

import base64
import os
import uuid

import pytest
from pydantic import SecretStr

from app import secretbox
from app.config import Settings
from app.secretbox import DecryptError, KeyringError, Purpose, SecretBox, parse_keyring

ROW = uuid.UUID("00000000-0000-4000-8000-000000000001")
OTHER_ROW = uuid.UUID("00000000-0000-4000-8000-000000000002")


def key() -> bytes:
    return os.urandom(32)


def b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def box(keys: dict[int, bytes] | None = None, primary: int = 1) -> SecretBox:
    return SecretBox(keys or {1: key()}, primary)


# ---- registry ----------------------------------------------------------------------


def test_the_purpose_registry_is_exactly_this() -> None:
    """Adding a purpose is a reviewed change (spec §3): this list must be edited."""
    assert {p.name: p.value for p in Purpose} == {
        "TOTP_SECRET": ("totp", "secret"),
        "BOT_OUTBOUND_SECRET": ("bots", "outbound_secret_enc"),
    }


def test_free_form_purposes_are_refused() -> None:
    b = box()
    with pytest.raises(TypeError):
        b.encrypt("x", purpose=("totp", "secret"), row_pk=ROW)  # type: ignore[arg-type]
    stored = b.encrypt("x", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    with pytest.raises(TypeError):
        b.decrypt(stored, purpose=("totp", "secret"), row_pk=ROW)  # type: ignore[arg-type]


def test_aad_golden_vector() -> None:
    """Every part of the field identity is in the AAD (the swap tests change several
    parts at once, so dropping one alone would not fail them)."""
    assert secretbox._aad(1, Purpose.TOTP_SECRET, ROW) == (
        b"brook.v1|1|totp|secret|00000000-0000-4000-8000-000000000001"
    )
    assert secretbox._aad(42, Purpose.BOT_OUTBOUND_SECRET, OTHER_ROW) == (
        b"brook.v1|42|bots|outbound_secret_enc|00000000-0000-4000-8000-000000000002"
    )


# ---- format and round trip ---------------------------------------------------------


def test_round_trip_and_format() -> None:
    b = box({7: key()}, 7)
    stored = b.encrypt("JBSWY3DPEHPK3PXP", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    version, key_id, nonce, ct = stored.split(".")
    assert (version, key_id) == ("v1", "7")
    assert "=" not in stored  # unpadded base64url
    assert len(base64.urlsafe_b64decode(nonce + "==")) == 12
    assert b.decrypt(stored, purpose=Purpose.TOTP_SECRET, row_pk=ROW) == "JBSWY3DPEHPK3PXP"


def test_nonces_are_fresh() -> None:
    b = box()
    a = b.encrypt("same", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    c = b.encrypt("same", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    assert a != c


def test_plaintext_is_bounded() -> None:
    with pytest.raises(ValueError):
        box().encrypt("x" * 5000, purpose=Purpose.TOTP_SECRET, row_pk=ROW)


# ---- AAD binding: fail closed on the wrong field or row ------------------------------


def test_ciphertext_is_bound_to_its_row() -> None:
    b = box()
    stored = b.encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    with pytest.raises(DecryptError) as err:
        b.decrypt(stored, purpose=Purpose.TOTP_SECRET, row_pk=OTHER_ROW)
    assert err.value.reason == "invalid_tag"


def test_ciphertext_is_bound_to_its_column() -> None:
    b = box()
    stored = b.encrypt("s", purpose=Purpose.BOT_OUTBOUND_SECRET, row_pk=ROW)
    with pytest.raises(DecryptError) as err:
        b.decrypt(stored, purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    assert err.value.reason == "invalid_tag"


def test_the_key_id_is_authenticated() -> None:
    """Relabelling a ciphertext with another key id in the ring fails closed."""
    k = key()
    b = SecretBox({1: k, 2: k}, 1)  # same material, different ids
    stored = b.encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    relabelled = stored.replace("v1.1.", "v1.2.", 1)
    with pytest.raises(DecryptError) as err:
        b.decrypt(relabelled, purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    assert err.value.reason == "invalid_tag"


# ---- every failure is a DecryptError with a reason -----------------------------------


@pytest.mark.parametrize(
    ("mutate", "reason"),
    [
        (lambda s: "v2" + s[2:], "unknown_version"),
        (lambda s: s.replace("v1.1.", "v1.9.", 1), "unknown_key_id"),
        (lambda s: s[:-4] + ("AAAA" if not s.endswith("AAAA") else "BBBB"), "invalid_tag"),
        (lambda s: "garbage", "malformed"),
        (lambda s: s + ".extra", "malformed"),
        (lambda s: "v1.x." + s.split(".", 2)[2], "malformed"),
        (lambda s: "v1.1.AAAA." + s.split(".")[3], "malformed"),  # 3-byte nonce
        (lambda s: s.replace(s.split(".")[3], "not+base64/"), "malformed"),
        (lambda s: "v1.1." + "A" * 20000, "malformed"),
        # Non-ASCII digits: isdigit() alone accepts them; int() then raises or reads "1".
        (lambda s: "v1.\u00b2." + s.split(".", 2)[2], "malformed"),  # superscript two
        (lambda s: "v1.\uff11." + s.split(".", 2)[2], "malformed"),  # fullwidth one
        # A huge ASCII id: int() would raise its own ValueError (4300-digit limit).
        (lambda s: "v1." + "1" * 5000 + "." + s.split(".", 2)[2], "malformed"),
        (lambda s: "v1.12345678901." + s.split(".", 2)[2], "malformed"),  # 11 digits
        (lambda s: "v1.01." + s.split(".", 2)[2], "malformed"),  # non-canonical 1
        (lambda s: "v1.." + s.split(".", 2)[2], "malformed"),  # empty id
    ],
)
def test_decrypt_failures_fail_closed_with_a_reason(mutate, reason) -> None:  # type: ignore[no-untyped-def]
    b = box()
    stored = b.encrypt("secret", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    before = secretbox.decrypt_failures[reason]
    with pytest.raises(DecryptError) as err:
        b.decrypt(mutate(stored), purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    assert err.value.reason == reason
    assert secretbox.decrypt_failures[reason] == before + 1  # counted for the operator


def test_an_oversized_well_formed_value_is_refused_by_length() -> None:
    """Well-formed apart from its length, so only the length bound can refuse it
    (without the bound it would reach AES-GCM and fail as invalid_tag)."""
    b = box()
    _, key_id, nonce, _ = b.encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW).split(".")
    stored = f"v1.{key_id}.{nonce}." + "A" * secretbox.MAX_STORED_CHARS
    assert len(stored.split(".")) == 4
    with pytest.raises(DecryptError) as err:
        b.decrypt(stored, purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    assert err.value.reason == "malformed"


def test_needs_rewrap_never_raises_on_a_bad_id() -> None:
    b = box()
    tail = b.encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW).split(".", 2)[2]
    for bad in ("1" * 5000, "01", "\u00b2", ""):
        assert b.needs_rewrap(f"v1.{bad}.{tail}") is False


def test_decrypt_error_never_carries_material() -> None:
    b = box()
    stored = b.encrypt("TOPSECRET", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    with pytest.raises(DecryptError) as err:
        b.decrypt(stored, purpose=Purpose.TOTP_SECRET, row_pk=OTHER_ROW)
    assert "TOPSECRET" not in str(err.value) and stored not in str(err.value)


# ---- rotation ----------------------------------------------------------------------


def test_rotation_is_migration_free() -> None:
    k1, k2 = key(), key()
    old = SecretBox({1: k1}, 1).encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    rotated = SecretBox({1: k1, 2: k2}, 2)
    assert rotated.decrypt(old, purpose=Purpose.TOTP_SECRET, row_pk=ROW) == "s"
    assert rotated.needs_rewrap(old)
    new = rotated.encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    assert new.startswith("v1.2.") and not rotated.needs_rewrap(new)


def test_dropping_a_key_too_early_is_unknown_key_id() -> None:
    k1, k2 = key(), key()
    old = SecretBox({1: k1}, 1).encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    with pytest.raises(DecryptError) as err:
        SecretBox({2: k2}, 2).decrypt(old, purpose=Purpose.TOTP_SECRET, row_pk=ROW)
    assert err.value.reason == "unknown_key_id"


# ---- keyring parsing ---------------------------------------------------------------


def test_parse_keyring() -> None:
    k1, k2 = key(), key()
    assert parse_keyring(f"1:{b64(k1)},2:{b64(k2)}") == {1: k1, 2: k2}


@pytest.mark.parametrize(
    "spec",
    [
        "",
        "1",
        "x:AAAA",
        f"1:{b64(os.urandom(16))}",  # wrong length
        f"1:{b64(os.urandom(32))},1:{b64(os.urandom(32))}",  # duplicate id
        f" 1:{b64(os.urandom(32))}",  # whitespace
        f"1:{b64(os.urandom(32))}\n",  # trailing newline
        "1:not+base64/==",
        f"\u00b2:{b64(os.urandom(32))}",  # superscript two: int() raises ValueError
        f"\uff11:{b64(os.urandom(32))}",  # fullwidth one: int() would read it as 1
        f"{'1' * 5000}:{b64(os.urandom(32))}",  # int() would raise its own ValueError
        f"12345678901:{b64(os.urandom(32))}",  # 11 digits
        f"01:{b64(os.urandom(32))}",  # non-canonical 1
        f"0:{b64(os.urandom(32))}",  # reserved for the dev key
        f":{b64(os.urandom(32))}",  # empty id
    ],
)
def test_parse_keyring_is_strict(spec: str) -> None:
    with pytest.raises(KeyringError):
        parse_keyring(spec)


def test_primary_must_be_in_the_ring() -> None:
    with pytest.raises(KeyringError):
        SecretBox({1: key()}, 2)


# ---- startup guard (§5.7) ----------------------------------------------------------

_JWT = "x" * 40


def settings(**kw: object) -> Settings:
    return Settings(jwt_signing_key=_JWT, **kw)  # type: ignore[arg-type]


def test_startup_refuses_a_missing_ring() -> None:
    with pytest.raises(RuntimeError, match="BROOK_SECRET_KEYS"):
        settings(secret_keys=None).assert_secure()


def test_startup_refuses_a_primary_outside_the_ring() -> None:
    s = settings(secret_keys=SecretStr(f"1:{b64(key())}"), secret_primary_key_id=2)
    with pytest.raises(RuntimeError, match="primary key id 2"):
        s.assert_secure()


def test_startup_refuses_a_missing_primary() -> None:
    s = settings(secret_keys=SecretStr(f"1:{b64(key())}"), secret_primary_key_id=None)
    with pytest.raises(RuntimeError):
        s.assert_secure()


def test_startup_errors_never_show_key_material() -> None:
    material = b64(os.urandom(16))  # wrong length
    s = settings(secret_keys=SecretStr(f"1:{material}"), secret_primary_key_id=1)
    with pytest.raises(RuntimeError) as err:
        s.assert_secure()
    assert material not in str(err.value)


def test_the_insecure_flag_never_excuses_a_malformed_ring() -> None:
    """The dev hatch only covers an *absent* ring; a present but broken one still
    refuses to boot."""
    s = settings(
        secret_keys=SecretStr("1:short"), secret_primary_key_id=1, allow_insecure_auth=True
    )
    with pytest.raises(RuntimeError, match="secret keyring rejected"):
        s.assert_secure()
    s = settings(secret_keys=SecretStr(""), secret_primary_key_id=1, allow_insecure_auth=True)
    with pytest.raises(RuntimeError, match="secret keyring rejected"):
        s.assert_secure()


def test_an_empty_primary_from_compose_is_refused_by_the_guard(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Compose passes an unset `${VAR:-}` as "": that is "not set" (the guard's own
    message), never a guessed primary and never a pydantic parse error."""
    monkeypatch.setenv("BROOK_SECRET_KEYS", f"1:{b64(key())}")
    monkeypatch.setenv("BROOK_SECRET_PRIMARY_KEY_ID", "")
    s = Settings(jwt_signing_key=_JWT)
    assert s.secret_primary_key_id is None
    with pytest.raises(RuntimeError, match="PRIMARY_KEY_ID is not set"):
        s.assert_secure()


def test_a_valid_ring_boots() -> None:
    settings(secret_keys=SecretStr(f"1:{b64(key())}"), secret_primary_key_id=1).assert_secure()


def test_the_ring_is_never_printed() -> None:
    material = b64(key())
    s = settings(secret_keys=SecretStr(f"1:{material}"), secret_primary_key_id=1)
    assert material not in repr(s) and material not in str(s.model_dump())


def test_dev_escape_hatch_uses_a_fixed_key_never_plaintext(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    s = settings(secret_keys=None, allow_insecure_auth=True)
    s.assert_secure()  # allowed
    monkeypatch.setattr("app.config.get_settings", lambda: s)
    secretbox.get_secret_box.cache_clear()
    try:
        b = secretbox.get_secret_box()
        stored = b.encrypt("s", purpose=Purpose.TOTP_SECRET, row_pk=ROW)
        assert stored.startswith("v1.0.") and stored != "s"
        assert b.decrypt(stored, purpose=Purpose.TOTP_SECRET, row_pk=ROW) == "s"
    finally:
        secretbox.get_secret_box.cache_clear()


def test_get_secret_box_never_guesses_the_primary(monkeypatch: pytest.MonkeyPatch) -> None:
    s = settings(
        secret_keys=SecretStr(f"1:{b64(key())},2:{b64(key())}"), secret_primary_key_id=None
    )
    monkeypatch.setattr("app.config.get_settings", lambda: s)
    secretbox.get_secret_box.cache_clear()
    try:
        with pytest.raises(KeyringError):
            secretbox.get_secret_box()
    finally:
        secretbox.get_secret_box.cache_clear()
