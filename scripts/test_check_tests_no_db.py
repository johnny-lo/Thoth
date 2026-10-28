#!/usr/bin/env python3
"""Tests for scripts/check_tests_no_db.py. Run: python scripts/test_check_tests_no_db.py

Only temporary directories are used. These are the deliberately wrong
examples the check has to catch (decisions 2026-09-29): every red case here
is a real violation that a reviewer would otherwise have to find by hand.
"""
import io
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
TOOL = HERE / "check_tests_no_db.py"
sys.path.insert(0, str(HERE))
import check_tests_no_db  # noqa: E402  (the module under test, for in-process runs)

CLEAN = "mod common;\n\n#[test]\nfn a() {\n    assert_eq!(1, 1);\n}\n"
ONE_HIT = "use rusqlite::params;\n\n#[test]\nfn a() {}\n"
TWO_HITS = (
    "fn open() {\n"
    "    let c = rusqlite::Connection::open(\"x\");\n"
    "    let p = home.join(\"state.db\");\n"
    "}\n"
)


def cli(*args, cwd=None):
    r = subprocess.run(
        [sys.executable, "-I", str(TOOL), *args],
        capture_output=True,
        cwd=cwd,
        env={**os.environ, "PYTHONUTF8": "1"},
    )
    return r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace")


def snapshot(root):
    """Every file under root with its bytes, so a run can be proven read-only."""
    return {
        p.relative_to(root).as_posix(): p.read_bytes()
        for p in sorted(root.rglob("*"))
        if p.is_file()
    }


class TmpCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name) / "repo"
        (self.root / "tests").mkdir(parents=True)
        (self.root / "scripts").mkdir()

    def write(self, rel, text, newline="\n"):
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(text.replace("\n", newline).encode("utf-8"))
        return p

    def allow(self, text):
        return self.write("scripts/tests_db_allowlist.txt", text)

    def three_files(self):
        self.write("tests/clean.rs", CLEAN)
        self.write("tests/one.rs", ONE_HIT)
        self.write("tests/common/mod.rs", TWO_HITS)

    def check(self, *args, cwd=None):
        return cli("--root", str(self.root), *args, cwd=cwd)


class GreenTests(TmpCase):
    def test_counts_matching_the_allowlist_pass(self):
        self.three_files()
        self.allow("# comment\n\ntests/one.rs 1\ntests/common/mod.rs 2\n")
        code, out, err = self.check()
        self.assertEqual(code, 0, err)
        self.assertTrue(out.startswith("ok"), out)
        self.assertIn("3 file(s)", out)
        self.assertIn("3 database line(s) allowed", out)

    def test_a_comment_line_does_not_count(self):
        self.write("tests/c.rs", "// rusqlite is not used here\n    // Connection::open neither\nfn a() {}\n")
        self.allow("")
        code, out, _ = self.check()
        self.assertEqual(code, 0, out)
        self.assertTrue(out.startswith("ok"), out)

    def test_one_line_with_several_tokens_counts_once_and_a_path_string_counts(self):
        self.write("tests/m.rs", "let c = rusqlite::Connection::open(dir.join(\"state.db\"));\n")
        self.write("tests/p.rs", "assert!(home.join(\"repos\").join(\"global.db\").is_file());\n")
        self.allow("tests/m.rs 1\ntests/p.rs 1\n")
        code, out, _ = self.check()
        self.assertEqual(code, 0, out)
        code, out, _ = self.check("--print-counts")
        self.assertEqual(code, 0)
        self.assertEqual(out, "tests/m.rs 1\ntests/p.rs 1\n")

    def test_subdirectory_paths_use_forward_slashes(self):
        self.write("tests/common/mod.rs", ONE_HIT)
        self.write("tests/deep/er/x.rs", ONE_HIT)
        code, out, _ = self.check("--print-counts")
        self.assertEqual(code, 0)
        self.assertEqual(out, "tests/common/mod.rs 1\ntests/deep/er/x.rs 1\n")
        self.assertNotIn("\\", out)
        self.allow("tests/common/mod.rs 1\ntests/deep/er/x.rs 1\n")
        self.assertEqual(self.check()[0], 0)

    def test_crlf_and_lf_and_non_ascii_count_the_same(self):
        body = "// 註解\nfn 測試() {\n    let 連線 = rusqlite::Connection::open(\"état.db\");\n    let 路徑 = \"state.db\";\n}\n"
        self.write("tests/中文_lf.rs", body, newline="\n")
        self.write("tests/中文_crlf.rs", body, newline="\r\n")
        code, out, err = self.check("--print-counts")
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "tests/中文_crlf.rs 2\ntests/中文_lf.rs 2\n")
        self.allow("tests/中文_lf.rs 2\ntests/中文_crlf.rs 3\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1)
        self.assertIn("tests/中文_crlf.rs: 2 line(s), allowed 3", out)


class RedTests(TmpCase):
    def test_a_new_file_outside_the_allowlist_is_red_with_line_and_text(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\n")
        self.write("tests/new_one.rs", "fn a() {}\n\nuse rusqlite::params;\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("tests/new_one.rs: 1 line(s)", out)
        self.assertIn("allowed 0", out)
        self.assertIn("tests/new_one.rs:3: use rusqlite::params;", out)
        self.assertTrue(out.rstrip().endswith("FAIL: 1 file(s)"), out)

    def test_a_listed_file_that_grew_is_red_with_both_numbers(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\n")
        self.write("tests/common/mod.rs", TWO_HITS + "fn more() { Connection::open(\"y\"); }\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("tests/common/mod.rs: 3 line(s), allowed 2", out)
        self.assertIn("tests/common/mod.rs:5: fn more() { Connection::open(\"y\"); }", out)
        self.assertIn("FAIL: 1 file(s)", out)

    def test_a_listed_file_that_shrank_is_red_and_asks_to_lower_the_number(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/common/mod.rs 5\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("tests/common/mod.rs: 2 line(s), allowed 5", out)
        self.assertIn("lower the number in the allowlist to 2", out)
        self.assertIn("FAIL: 1 file(s)", out)

    def test_at_most_five_lines_are_shown_for_one_file(self):
        self.write("tests/many.rs", "".join(f"let c{i} = rusqlite::x();\n" for i in range(9)))
        self.allow("")
        code, out, _ = self.check()
        self.assertEqual(code, 1)
        self.assertIn("tests/many.rs: 9 line(s)", out)
        self.assertEqual(out.count("tests/many.rs:"), 6, out)  # the summary line + 5 shown
        self.assertIn("tests/many.rs:5:", out)
        self.assertNotIn("tests/many.rs:6:", out)

    def test_a_listed_file_that_does_not_exist_is_red(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\ntests/gone.rs 1\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("tests/gone.rs", out)
        self.assertIn("does not exist", out)
        self.assertIn("FAIL: 1 file(s)", out)

    def test_several_failures_are_all_listed_and_counted(self):
        self.three_files()
        self.allow("tests/one.rs 0\ntests/gone.rs 1\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("tests/one.rs: 1 line(s), allowed 0", out)
        self.assertIn("tests/common/mod.rs: 2 line(s)", out)
        self.assertIn("tests/gone.rs", out)
        self.assertIn("FAIL: 3 file(s)", out)


class InputErrorTests(TmpCase):
    def test_missing_allowlist_fails_but_print_counts_still_works(self):
        self.three_files()
        code, out, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn("tests_db_allowlist.txt", err)
        self.assertIn("does not exist", err)
        code, out, err = self.check("--print-counts")
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "tests/common/mod.rs 2\ntests/one.rs 1\n")

    def test_print_counts_changes_nothing_on_disk(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\n")
        before = snapshot(self.root)
        self.assertEqual(self.check("--print-counts")[0], 0)
        self.assertEqual(self.check()[0], 0)
        self.assertEqual(snapshot(self.root), before)

    def test_a_malformed_allowlist_line_is_named(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/common/mod.rs two words 2\n")
        code, _, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn("tests_db_allowlist.txt:2", err)
        self.assertIn("expected `path count`", err)

    def test_a_duplicate_path_is_refused(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/one.rs 1\ntests/common/mod.rs 2\n")
        code, _, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn(":2", err)
        self.assertIn("tests/one.rs is listed more than once", err)

    def test_a_negative_or_non_integer_count_is_refused(self):
        self.three_files()
        for bad in ("-1", "1.5", "one"):
            self.allow(f"tests/one.rs {bad}\ntests/common/mod.rs 2\n")
            code, _, err = self.check()
            self.assertEqual(code, 1, bad)
            self.assertIn("tests_db_allowlist.txt:1", err)
            self.assertIn("non-negative integer", err)
            self.assertIn(bad, err)

    def test_an_absolute_or_escaping_path_is_refused(self):
        self.three_files()
        for bad in ("/tmp/tests/one.rs", "C:/x/tests/one.rs", "tests/../src/x.rs", "src/x.rs", "tests"):
            self.allow(f"{bad} 1\n")
            code, _, err = self.check()
            self.assertEqual(code, 1, bad)
            self.assertIn("tests_db_allowlist.txt:1", err)
            self.assertIn(bad, err)
            self.assertTrue("absolute path" in err or "not a path under tests/" in err, err)

    def test_a_missing_tests_directory_is_an_error_even_for_print_counts(self):
        (self.root / "tests").rmdir()
        self.allow("")
        code, _, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn("no tests/ directory", err)
        code, out, err = self.check("--print-counts")
        self.assertEqual(code, 1)
        self.assertEqual(out, "")
        self.assertIn("no tests/ directory", err)

    def test_an_undecodable_test_file_is_an_error(self):
        self.three_files()
        (self.root / "tests" / "bad.rs").write_bytes(b"fn a() {}\n\xff\xfe\n")
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\n")
        code, _, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn("tests/bad.rs", err)
        self.assertIn("UTF-8", err)

    def test_a_relative_allowlist_is_taken_from_root_whatever_the_cwd(self):
        self.three_files()
        self.write("lists/allow.txt", "tests/one.rs 1\ntests/common/mod.rs 2\n")
        here = self.check("--allowlist", "lists/allow.txt", cwd=str(self.root))
        elsewhere = self.check("--allowlist", "lists/allow.txt", cwd=str(self.root.parent))
        self.assertEqual(here[0], 0, here)
        self.assertEqual(elsewhere, here)
        absolute = self.check("--allowlist", str(self.root / "lists" / "allow.txt"), cwd=str(self.root.parent))
        self.assertEqual(absolute, here)
        # the default lives under --root too
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\n")
        self.assertEqual(self.check(cwd=str(self.root.parent))[0], 0)


class ScanErrorTests(TmpCase):
    """Review round 1 (error_path): a directory the scan cannot list is an
    error with the path and the reason, never a green result over a partial
    scan. (`Path.rglob` on Python 3.13 swallows the OSError of a directory
    listing; the check must not.)"""

    def run_in_process(self, *args, denied_dir="fixtures"):
        """Run main() with os.scandir refusing `denied_dir`; (code, out, err)."""
        real_scandir = os.scandir

        def denied(path=".", *a, **k):
            if Path(os.fspath(path)).name == denied_dir:
                raise PermissionError(13, "Permission denied", os.fspath(path))
            return real_scandir(path, *a, **k)

        out, err = io.StringIO(), io.StringIO()
        with mock.patch("os.scandir", denied), redirect_stdout(out), redirect_stderr(err):
            code = check_tests_no_db.main(["--root", str(self.root), *args])
        return code, out.getvalue(), err.getvalue()

    def test_a_directory_that_cannot_be_listed_is_red_not_green(self):
        self.three_files()
        self.write("tests/fixtures/hidden.rs", ONE_HIT)
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\n")
        code, out, err = self.run_in_process()
        self.assertEqual(code, 1, (out, err))
        self.assertFalse(out.startswith("ok"), out)
        self.assertIn("tests/fixtures", err)
        self.assertIn("Permission denied", err)
        self.assertIn("cannot list", err)
        # the same tree lists fine without the refusal
        code, out, err = self.run_in_process(denied_dir="no-such-directory")
        self.assertEqual(code, 1, (out, err))
        self.assertIn("tests/fixtures/hidden.rs: 1 line(s)", out)

    def test_print_counts_fails_too_when_a_directory_cannot_be_listed(self):
        self.three_files()
        self.write("tests/fixtures/hidden.rs", ONE_HIT)
        code, out, err = self.run_in_process("--print-counts")
        self.assertEqual(code, 1, (out, err))
        self.assertEqual(out, "")
        self.assertIn("tests/fixtures", err)
        self.assertIn("Permission denied", err)

    @unittest.skipIf(os.name == "nt" or (hasattr(os, "geteuid") and os.geteuid() == 0),
                     "needs POSIX permissions and a non-root user")
    def test_a_real_unreadable_directory_is_red_through_the_cli(self):
        self.three_files()
        self.write("tests/fixtures/hidden.rs", ONE_HIT)
        self.allow("tests/one.rs 1\ntests/common/mod.rs 2\n")
        locked = self.root / "tests" / "fixtures"
        locked.chmod(0)
        self.addCleanup(locked.chmod, stat.S_IRWXU)
        code, out, err = self.check()
        self.assertEqual(code, 1, (out, err))
        self.assertFalse(out.startswith("ok"), out)
        self.assertIn("tests/fixtures", err)
        self.assertIn("cannot list", err)


class AllowlistInputTests(TmpCase):
    """Review round 2 (error_path): an allowlist path is normalised (`./`
    segments, doubled slashes, `\\` separators) before it is stored, compared
    with the scan and checked for duplicates, so `tests/common/./mod.rs` names
    `tests/common/mod.rs` and is not reported as a missing file next to an
    unlisted one; and a count is a plain ASCII non-negative integer: `²`
    passes str.isdigit() but int() raises on it, and a fullwidth `１` or an
    Arabic-Indic `٣` would be accepted as 1 or 3. Both are refused with the
    allowlist path, the line number and the reason, never a traceback."""

    def test_a_dot_segment_or_backslash_in_a_listed_path_names_the_same_file(self):
        self.three_files()
        for spelling in (
            "tests/common/./mod.rs",
            "./tests/common/mod.rs",
            "tests//common/mod.rs",
            "tests\\common\\mod.rs",
        ):
            self.allow(f"tests/one.rs 1\n{spelling} 2\n")
            code, out, err = self.check()
            self.assertEqual(code, 0, (spelling, out, err))
            self.assertTrue(out.startswith("ok"), (spelling, out))
            self.assertNotIn("does not exist", out)
            self.assertNotIn("not in the allowlist", out)
        # the normalised path is what the messages use, so a wrong count is
        # reported against the file the scan knows
        self.allow("tests/one.rs 1\ntests/common/./mod.rs 5\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("tests/common/mod.rs: 2 line(s), allowed 5", out)
        self.assertNotIn("does not exist", out)
        self.assertIn("FAIL: 1 file(s)", out)

    def test_the_same_file_under_two_spellings_is_a_duplicate(self):
        self.three_files()
        self.allow("tests/one.rs 1\ntests/./one.rs 1\ntests/common/mod.rs 2\n")
        code, out, err = self.check()
        self.assertEqual(code, 1, (out, err))
        self.assertEqual(out, "")
        self.assertIn("tests_db_allowlist.txt:2", err)
        self.assertIn("tests/one.rs is listed more than once", err)
        self.assertIn("tests/./one.rs", err)  # the spelling as written, so the line is easy to find

    def test_a_count_that_is_not_an_ascii_integer_is_refused_without_a_traceback(self):
        self.three_files()
        for bad in ("²", "１", "٣", "1²"):
            self.allow(f"tests/one.rs 1\ntests/common/mod.rs {bad}\n")
            code, out, err = self.check()
            self.assertEqual(code, 1, (bad, out, err))
            self.assertEqual(out, "", bad)
            self.assertIn("tests_db_allowlist.txt:2", err)
            self.assertIn("tests/common/mod.rs", err)
            self.assertIn("non-negative integer", err)
            self.assertIn(bad, err)
            self.assertNotIn("Traceback", err)
        # in-process too: parse_allowlist raises CheckError, not ValueError
        bad_list = self.allow("tests/common/mod.rs ²\n")
        with self.assertRaises(check_tests_no_db.CheckError) as caught:
            check_tests_no_db.parse_allowlist(bad_list)
        self.assertIn(":1:", str(caught.exception))
        self.assertIn("²", str(caught.exception))

    def test_parse_allowlist_returns_the_normalised_keys(self):
        listed = self.allow("tests/./one.rs 1\ntests\\common\\mod.rs 2\n./tests/deep//x.rs 0\n")
        self.assertEqual(
            check_tests_no_db.parse_allowlist(listed),
            {"tests/one.rs": 1, "tests/common/mod.rs": 2, "tests/deep/x.rs": 0},
        )


class StreamEncodingTests(TmpCase):
    """AC3: the entry point puts stdin, stdout and stderr on UTF-8 whatever the
    console code page, so a Traditional Chinese Windows (cp950) reads and
    writes the same bytes as Linux."""

    def test_main_switches_all_three_standard_streams_from_cp950_to_utf8(self):
        self.write("tests/中文.rs", ONE_HIT)
        streams = {
            name: io.TextIOWrapper(io.BytesIO(), encoding="cp950")
            for name in ("stdin", "stdout", "stderr")
        }
        for s in streams.values():
            self.assertEqual(s.encoding, "cp950")
        with mock.patch.multiple(sys, **streams):
            code = check_tests_no_db.main(["--root", str(self.root), "--print-counts"])
            sys.stdout.flush()
            for name, s in streams.items():
                self.assertEqual(s.encoding.lower().replace("-", ""), "utf8", name)
        self.assertEqual(code, 0)
        written = streams["stdout"].buffer.getvalue()
        self.assertEqual(written.decode("utf-8"), "tests/中文.rs 1\n")


if __name__ == "__main__":
    for s in (sys.stdout, sys.stderr):
        s.reconfigure(encoding="utf-8", errors="replace")
    unittest.main()
