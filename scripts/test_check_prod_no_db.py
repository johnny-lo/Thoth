#!/usr/bin/env python3
"""Tests for scripts/check_prod_no_db.py. Run: python3 scripts/test_check_prod_no_db.py

Only temporary directories are used, and the script is always run as a
subprocess through its command line. Every red case is a real violation the
check has to catch; the last test is the candidate proof: the real src/ of
this repository is green, and one call put back outside src/db/ turns it red.
"""
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOOL = HERE / "check_prod_no_db.py"

CALL = '    let _ = conn.execute("DELETE FROM events", []);\n'
CLEAN = "fn a() {\n    let x = 1;\n}\n"
TESTS_MOD = "#[cfg(test)]\nmod tests {\n    fn t() {\n" + CALL + "    }\n}\n"


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
        (self.root / "src").mkdir(parents=True)

    def write(self, rel, text, newline="\n"):
        p = self.root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(text.replace("\n", newline).encode("utf-8"))
        return p

    def check(self, *args, cwd=None):
        return cli("--root", str(self.root), *args, cwd=cwd)


class GreenTests(TmpCase):
    def test_a_clean_tree_is_green(self):
        self.write("src/main.rs", CLEAN)
        self.write("src/commands/x.rs", CLEAN)
        code, out, err = self.check()
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "ok: scanned 2 file(s) under src/ outside src/db/, 0 production database call(s)\n")

    def test_the_same_call_inside_the_inline_test_module_is_green(self):
        self.write("src/a.rs", CLEAN + "\n" + TESTS_MOD)
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))
        self.assertTrue(out.startswith("ok"), out)

    def test_a_pub_module_and_a_stacked_attribute_also_open_the_test_region(self):
        self.write("src/a.rs", "#[cfg(test)]\n#[allow(dead_code)]\n\npub(crate) mod tests {\n" + CALL + "}\n")
        self.write("src/b.rs", "#[cfg(test)]\npub mod checks {\n" + CALL + "}\n")
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))

    def test_a_file_named_tests_rs_is_all_test(self):
        self.write("src/runner/tests.rs", "fn t() {\n" + CALL + "}\n")
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))

    def test_a_file_under_a_tests_directory_is_all_test(self):
        self.write("src/thing/tests/case.rs", "fn t() {\n" + CALL + "}\n")
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))

    def test_a_call_under_src_db_is_green_and_not_counted_as_scanned(self):
        self.write("src/db/state.rs", "fn q() {\n" + CALL + "}\n")
        self.write("src/main.rs", CLEAN)
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))
        self.assertIn("scanned 1 file(s)", out)

    def test_a_comment_line_with_a_call_is_green(self):
        self.write("src/a.rs", "// conn.execute(\"x\", [])\n    // conn.prepare(\"y\")\nfn a() {}\n")
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))

    def test_a_production_call_after_the_test_module_is_not_counted(self):
        self.write("src/a.rs", CLEAN + TESTS_MOD + "\nfn after() {\n" + CALL + "}\n")
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))

    def test_crlf_and_non_ascii_text_count_the_same(self):
        body = "// 註解\nfn 測試() {\n    let 連線 = conn.query_row(\"état\", [], |r| r.get(0));\n}\n"
        self.write("src/中文_lf.rs", body, newline="\n")
        self.write("src/中文_crlf.rs", body, newline="\r\n")
        code, out, err = self.check("--print-counts")
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "src/中文_crlf.rs 1\nsrc/中文_lf.rs 1\n")


class RedTests(TmpCase):
    def test_one_production_call_is_red_with_path_line_and_text(self):
        self.write("src/records.rs", CLEAN + "\nfn q() {\n" + CALL + "}\n")
        code, out, err = self.check()
        self.assertEqual(code, 1, (out, err))
        self.assertIn("src/records.rs: 1 production database call(s) outside src/db/ (allowed 0)", out)
        self.assertIn('  src/records.rs:6: let _ = conn.execute("DELETE FROM events", []);', out)
        self.assertEqual(out.splitlines()[-1], "FAIL: 1 call(s) in 1 file(s)")

    def test_every_call_method_of_the_pattern_is_a_hit(self):
        for method in ("execute", "execute_batch", "prepare", "prepare_cached", "query_row"):
            with self.subTest(method=method):
                self.write("src/m.rs", f"fn q() {{ conn.{method}(\"x\"); }}\n")
                code, out, _ = self.check()
                self.assertEqual(code, 1, out)
        self.write("src/m.rs", "fn q() { conn.query_map(\"x\"); conn.executor(1); }\n")
        self.assertEqual(self.check()[0], 0)

    def test_a_string_or_trailing_comment_counts_and_one_line_counts_once(self):
        self.write("src/a.rs", 'let s = "conn.execute(";\nlet n = 1; // x.prepare(\nconn.execute(a); conn.prepare(b);\n')
        code, out, _ = self.check("--print-counts")
        self.assertEqual(code, 0)
        self.assertEqual(out, "src/a.rs 3\n")

    def test_cfg_test_over_a_const_or_a_mod_declaration_does_not_hide_a_later_call(self):
        self.write("src/c.rs", "#[cfg(test)]\nconst SEAM: u32 = 1;\n\nfn late() {\n" + CALL + "}\n")
        self.write("src/d.rs", "#[cfg(test)]\nmod tests;\n\nfn late() {\n" + CALL + "}\n")
        self.write("src/e.rs", "#[cfg(test)]\nuse std::fmt;\n\nfn late() {\n" + CALL + "}\n")
        code, out, _ = self.check("--print-counts")
        self.assertEqual(code, 0)
        self.assertEqual(out, "src/c.rs 1\nsrc/d.rs 1\nsrc/e.rs 1\n")

    def test_a_cfg_test_quoted_in_a_doc_comment_does_not_start_the_region(self):
        self.write("src/p.rs", "/// the `#[cfg(test)]` module\n/// #[cfg(test)]\nmod x {\nfn late() {\n" + CALL + "}\n}\n")
        code, out, _ = self.check("--print-counts")
        self.assertEqual(out, "src/p.rs 1\n")

    def test_a_production_call_above_a_test_module_is_red_and_one_after_it_is_not(self):
        self.write("src/a.rs", "fn before() {\n" + CALL + "}\n" + TESTS_MOD + "fn after() {\n" + CALL + "}\n")
        code, out, _ = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn("src/a.rs: 1 production database call(s)", out)
        self.assertIn("src/a.rs:2:", out)
        self.assertNotIn("src/a.rs:9:", out)

    def test_six_hits_show_only_five_lines(self):
        self.write("src/many.rs", "".join(CALL for _ in range(6)))
        code, out, _ = self.check()
        self.assertEqual(code, 1)
        self.assertIn("src/many.rs: 6 production database call(s) outside src/db/ (allowed 0)", out)
        shown = [line for line in out.splitlines() if line.startswith("  src/many.rs:")]
        self.assertEqual(len(shown), 5)
        self.assertEqual(out.splitlines()[-1], "FAIL: 6 call(s) in 1 file(s)")

    def test_two_files_are_both_named_and_totalled(self):
        self.write("src/a.rs", CALL)
        self.write("src/sub/b.rs", CALL + CALL)
        code, out, _ = self.check()
        self.assertEqual(code, 1)
        self.assertIn("src/a.rs: 1 production", out)
        self.assertIn("src/sub/b.rs: 2 production", out)
        self.assertEqual(out.splitlines()[-1], "FAIL: 3 call(s) in 2 file(s)")


class PrintCountsTests(TmpCase):
    def test_print_counts_lists_files_with_hits_sorted_and_exits_zero(self):
        self.write("src/z.rs", CALL + CALL)
        self.write("src/a.rs", CALL)
        self.write("src/clean.rs", CLEAN)
        self.write("src/db/state.rs", CALL)
        self.write("src/b.rs", CLEAN + TESTS_MOD)
        code, out, err = self.check("--print-counts")
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "src/a.rs 1\nsrc/z.rs 2\n")

    def test_print_counts_on_a_clean_tree_prints_nothing(self):
        self.write("src/a.rs", CLEAN)
        code, out, err = self.check("--print-counts")
        self.assertEqual((code, out, err), (0, "", ""))


class InputErrorTests(TmpCase):
    def test_a_missing_src_directory_is_an_error_even_for_print_counts(self):
        shutil.rmtree(self.root / "src")
        for args in ((), ("--print-counts",)):
            code, out, err = self.check(*args)
            self.assertEqual(code, 1)
            self.assertEqual(out, "")
            self.assertTrue(err.startswith("error:"), err)
            self.assertIn("no src/ directory", err)

    def test_a_non_utf8_file_is_an_error(self):
        self.write("src/a.rs", CLEAN)
        (self.root / "src" / "bad.rs").write_bytes(b"fn a() {}\n\xff\xfe\n")
        for args in ((), ("--print-counts",)):
            code, out, err = self.check(*args)
            self.assertEqual(code, 1)
            self.assertFalse(out.startswith("ok"), out)
            self.assertTrue(err.startswith("error:"), err)
            self.assertIn("src/bad.rs", err)
            self.assertIn("UTF-8", err)

    @unittest.skipIf(os.name == "nt" or (hasattr(os, "geteuid") and os.geteuid() == 0),
                     "needs POSIX permissions and a non-root user")
    def test_an_unreadable_directory_is_an_error_not_a_partial_scan(self):
        self.write("src/a.rs", CLEAN)
        self.write("src/locked/hidden.rs", CALL)
        locked = self.root / "src" / "locked"
        locked.chmod(0)
        self.addCleanup(locked.chmod, stat.S_IRWXU)
        for args in ((), ("--print-counts",)):
            code, out, err = self.check(*args)
            self.assertEqual(code, 1, (out, err))
            self.assertFalse(out.startswith("ok"), out)
            self.assertTrue(err.startswith("error:"), err)
            self.assertIn("src/locked", err)
            self.assertIn("cannot list", err)

    @unittest.skipIf(os.name == "nt" or (hasattr(os, "geteuid") and os.geteuid() == 0),
                     "needs POSIX permissions and a non-root user")
    def test_an_unreadable_file_is_an_error(self):
        self.write("src/a.rs", CLEAN)
        locked = self.write("src/locked.rs", CALL)
        locked.chmod(0)
        self.addCleanup(locked.chmod, stat.S_IRUSR | stat.S_IWUSR)
        code, out, err = self.check()
        self.assertEqual(code, 1, (out, err))
        self.assertTrue(err.startswith("error:"), err)
        self.assertIn("src/locked.rs", err)

    def test_a_run_changes_nothing_on_disk(self):
        self.write("src/a.rs", CALL)
        self.write("src/b.rs", CLEAN + TESTS_MOD)
        self.write("src/db/x.rs", CALL)
        before = snapshot(self.root)
        self.assertEqual(self.check()[0], 1)
        self.assertEqual(self.check("--print-counts")[0], 0)
        self.assertEqual(snapshot(self.root), before)

    def test_root_defaults_to_the_parent_of_scripts(self):
        # a copy of the script placed in <tmp>/scripts scans <tmp>/src
        (self.root / "scripts").mkdir()
        shutil.copy(TOOL, self.root / "scripts" / "check_prod_no_db.py")
        self.write("src/a.rs", CALL)
        r = subprocess.run(
            [sys.executable, "-I", str(self.root / "scripts" / "check_prod_no_db.py")],
            capture_output=True,
            cwd=self.root.parent,
        )
        self.assertEqual(r.returncode, 1, r.stderr)
        self.assertIn("src/a.rs: 1 production", r.stdout.decode("utf-8"))


class CandidateProofTests(TmpCase):
    def test_the_real_src_is_green_and_one_call_put_back_turns_it_red(self):
        real_src = HERE.parent / "src"
        self.assertTrue(real_src.is_dir(), real_src)
        shutil.copytree(real_src, self.root / "src", dirs_exist_ok=True)
        code, out, err = self.check()
        self.assertEqual(code, 0, (out, err))
        self.assertTrue(out.startswith("ok:"), out)
        records = self.root / "src" / "records.rs"
        lines = records.read_text(encoding="utf-8").splitlines(keepends=True)
        at = next(i for i, line in enumerate(lines) if line.strip() == "#[cfg(test)]")
        lines.insert(at, CALL)
        records.write_text("".join(lines), encoding="utf-8")
        code, out, err = self.check()
        self.assertEqual(code, 1, (out, err))
        self.assertIn("src/records.rs", out)
        self.assertIn('let _ = conn.execute("DELETE FROM events", []);', out)
        self.assertEqual(out.splitlines()[-1], "FAIL: 1 call(s) in 1 file(s)")


if __name__ == "__main__":
    unittest.main()
