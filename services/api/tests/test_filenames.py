"""Attachment filename rules (attachments spec §4, owner decision)."""

from __future__ import annotations

import pytest

from app.filenames import original_name, safe_filename


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        # Transliteration to ASCII.
        ("Ștefan–raport.pdf", "Stefan-raport.pdf"),
        ("naïve résumé.odt", "naive resume.odt"),
        ("Привет мир.docx", "Privet mir.docx"),
        ("日本語.txt", "RiBenYu.txt"),
        # Paths can't climb or smuggle a directory.
        ("../../etc/passwd", "passwd"),
        ("C:\\Windows\\system32\\evil.dll", "evil.dll"),
        ("/tmp/x/", "file"),  # noqa: S108 - a filename under test, not a temp path
        # Invisible and direction characters: no disguised extensions.
        ("invoice\u202efdp.exe", "invoicefdp.exe"),
        ("a\u200bb\u2066c.txt", "abc.txt"),
        ("line\nbreak\t.txt", "linebreak.txt"),
        # Windows-reserved characters and edges.
        ('what?<is>:this"|*.txt', "what__is__this___.txt"),
        ("  ..hidden. ", "hidden"),
        ("many   spaces.txt", "many spaces.txt"),
        # Windows device names, with and without an extension.
        ("CON", "_CON"),
        ("nul.txt", "_nul.txt"),
        ("com1.log", "_com1.log"),
        ("console.txt", "console.txt"),  # only the exact device names
        # Nothing left.
        ("", "file"),
        ("...", "file"),
        ("\u202e", "file"),
    ],
)
def test_safe_filename(raw: str, expected: str) -> None:
    assert safe_filename(raw) == expected


def test_long_names_are_capped_keeping_the_extension() -> None:
    name = safe_filename("a" * 300 + ".pdf")
    assert len(name.encode()) <= 255 and name.endswith(".pdf")
    cjk = safe_filename("文" * 200 + ".jpg")  # transliteration makes it longer
    assert len(cjk.encode()) <= 255 and cjk.endswith(".jpg")


def test_result_is_always_plain_ascii() -> None:
    for raw in ["Ωmega.png", "emoji 🎉.gif", "Ärger\u202e.txt", "x\x00y.bin"]:
        out = safe_filename(raw)
        assert out.isascii() and out.isprintable() and "/" not in out and "\\" not in out


def test_original_name_keeps_unicode_but_not_invisibles() -> None:
    assert original_name("Ștefan–raport.pdf") == "Ștefan–raport.pdf"
    assert original_name("invoice\u202efdp.exe") == "invoicefdp.exe"
    assert original_name("\x00\u200b") == "file"


def test_long_tar_gz_keeps_both_extensions() -> None:
    """Owner decision: extensions win, the name shrinks."""
    name = safe_filename("x" * 300 + ".tar.gz")
    assert len(name.encode()) == 255 and name.endswith("x.tar.gz")
    three = safe_filename("y" * 300 + ".min.js.map")
    assert len(three.encode()) <= 255 and three.endswith("y.min.js.map")
    # A long "extension" isn't one: it counts as stem and gets shortened.
    odd = safe_filename("z" * 300 + ".averyveryverylongpart")
    assert len(odd.encode()) <= 255


@pytest.mark.parametrize("raw", ["LPT1 .txt", "con .txt", "Nul  .log"])
def test_device_names_with_spaces_before_the_dot(raw: str) -> None:
    assert safe_filename(raw).startswith("_")


def test_extension_only_names_keep_the_extension() -> None:
    assert safe_filename("‮.exe") == "file.exe"
