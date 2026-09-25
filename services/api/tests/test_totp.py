"""TOTP core (app/totp.py): RFC 6238 vectors, skew, replay, recovery codes."""

from __future__ import annotations

import base64
import re
from urllib.parse import parse_qs, unquote, urlparse

import pytest

from app import totp

# RFC 6238 appendix B: the SHA-1 seed is ASCII "12345678901234567890".
RFC_SECRET = base64.b32encode(b"12345678901234567890").decode().rstrip("=")


@pytest.mark.parametrize(
    ("unix_time", "expected8"),
    [
        (59, "94287082"),
        (1111111109, "07081804"),
        (1111111111, "14050471"),
        (1234567890, "89005924"),
        (2000000000, "69279037"),
        (20000000000, "65353130"),
    ],
)
def test_rfc6238_sha1_vectors(unix_time: int, expected8: str) -> None:
    step = totp.step_for(unix_time)
    assert totp.code_at(RFC_SECRET, step, digits=8) == expected8
    # The 6-digit code apps show is the same value mod 10^6.
    assert totp.code_at(RFC_SECRET, step) == expected8[-6:]


def test_skew_window_is_one_step_each_way() -> None:
    now = 1_700_000_000.0
    center = totp.step_for(now)
    for delta in (-1, 0, 1):
        code = totp.code_at(RFC_SECRET, center + delta)
        assert totp.match_step(RFC_SECRET, code, now, None) == center + delta
    for delta in (-2, 2):
        code = totp.code_at(RFC_SECRET, center + delta)
        assert totp.match_step(RFC_SECRET, code, now, None) is None


def test_replay_is_refused_within_the_window() -> None:
    now = 1_700_000_000.0
    code = totp.code_at(RFC_SECRET, totp.step_for(now))
    step = totp.match_step(RFC_SECRET, code, now, None)
    assert step is not None
    assert totp.match_step(RFC_SECRET, code, now, last_used_step=step) is None
    # An older step is refused too; the next one is fine.
    earlier = totp.code_at(RFC_SECRET, step - 1)
    assert totp.match_step(RFC_SECRET, earlier, now, last_used_step=step) is None
    later = totp.code_at(RFC_SECRET, step + 1)
    assert totp.match_step(RFC_SECRET, later, now, last_used_step=step) == step + 1


@pytest.mark.parametrize("bad", ["", "12345", "1234567", "12a456", "١٢٣٤٥٦", "123 45"])
def test_malformed_codes_never_match(bad: str) -> None:
    assert totp.match_step(RFC_SECRET, bad, 1_700_000_000.0, None) is None


def test_new_secret_is_160_bits_base32() -> None:
    s = totp.new_secret()
    assert re.fullmatch(r"[A-Z2-7]{32}", s)
    assert len(base64.b32decode(s)) == 20
    assert totp.new_secret() != s


def test_otpauth_uri_encodes_label_and_values() -> None:
    uri = totp.otpauth_uri("JBSWY3DPEHPK3PXP", issuer="Brook & Co", account="a.b_c")
    parsed = urlparse(uri)
    assert parsed.scheme == "otpauth" and parsed.netloc == "totp"
    assert unquote(parsed.path[1:]) == "Brook & Co:a.b_c"
    assert "&" not in parsed.path and ":" not in parsed.path[1:]  # encoded, not raw
    q = parse_qs(parsed.query)
    assert q["issuer"] == ["Brook & Co"]
    assert q["secret"] == ["JBSWY3DPEHPK3PXP"]
    assert (q["algorithm"], q["digits"], q["period"]) == (["SHA1"], ["6"], ["30"])


# ---------------------------------------------------------------- recovery codes


def test_recovery_codes_shape_and_uniqueness() -> None:
    codes = totp.new_recovery_codes()
    assert len(codes) == 10
    assert len({c.lookup for c in codes}) == 10
    for c in codes:
        assert re.fullmatch(r"[0-9A-HJKMNP-TV-Z]{4}(-[0-9A-HJKMNP-TV-Z]{4}){4}", c.display)
        assert c.display.startswith(c.lookup + "-")


def test_recovery_codes_are_argon2id_not_sha256() -> None:
    """The structural guard of encryption spec §7.1: a stored recovery code must be an
    Argon2id hash. A fast hash of a human-typed code falls to a GPU from a leaked dump,
    which would let an attacker skip the encrypted TOTP secret entirely."""
    code = totp.new_recovery_codes(1)[0]
    assert code.hash.startswith("$argon2id$")
    assert not re.fullmatch(r"[0-9a-f]{64}", code.hash)
    lookup, secret = totp.parse_recovery_code(code.display) or ("", "")
    assert lookup == code.lookup
    assert totp.verify_recovery_secret(code.hash, secret)
    assert not totp.verify_recovery_secret(code.hash, "0" * 16)


def test_recovery_code_input_is_forgiving_but_strict() -> None:
    code = totp.new_recovery_codes(1)[0]
    lookup, secret = totp.parse_recovery_code(code.display) or ("", "")
    sloppy = f" {code.display.lower().replace('-', ' ')} "
    assert totp.parse_recovery_code(sloppy) == (lookup, secret)
    assert totp.parse_recovery_code(code.display[:-1]) is None  # too short
    assert totp.parse_recovery_code(code.display + "0") is None  # too long
    assert totp.parse_recovery_code("UUUU-" + code.display[5:]) is None  # U isn't in the set
    # Look-alikes a person might type are mapped, not refused.
    assert totp.parse_recovery_code("O" * 20) == ("0000", "0" * 16)


def test_unknown_lookup_still_spends_an_argon2(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[str] = []
    monkeypatch.setattr(totp, "dummy_verify", lambda s: calls.append(s))
    assert totp.verify_recovery_secret(None, "X" * 16) is False
    assert calls == ["X" * 16]
