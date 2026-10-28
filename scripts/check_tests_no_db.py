#!/usr/bin/env python3
"""tests-no-db check: tests must not open the database (TESTS-1).

Scans every ``.rs`` file under ``tests/`` for lines that mention rusqlite,
``Connection::open``, ``state.db`` or ``global.db``, and compares the count per
file with ``scripts/tests_db_allowlist.txt``. The allowlist records what is
allowed *today*; a file not in it must have no such line, a listed file may
not grow, and a listed file that shrank must have its number lowered. The
script only reads: it never edits a file, never touches git, never uses the
network, and needs only the standard library.

    python scripts/check_tests_no_db.py                 # exit 0: ok / 1: FAIL
    python scripts/check_tests_no_db.py --print-counts  # the current counts,
                                                        # in allowlist format

Counting rule (one hit per line at most): strip the line; a line starting
with ``//`` is a comment and never counts; otherwise it counts when it contains
any of the tokens as a plain, case-sensitive substring. A path string that
mentions ``state.db`` or an assertion that quotes one of the tokens counts too:
the rule is about the text, not about whether a connection is really opened.

A scan that cannot finish is never green: a directory under ``tests/`` that
cannot be listed, a file that cannot be read or is not UTF-8, or a missing
``tests/`` directory is exit 1 with the path and the reason (also under
``--print-counts``).

Allowlist lines are ``path count``. The path is normalised before it is
stored, compared with the scan or checked for duplicates (``\\`` to ``/``,
``.`` segments and doubled slashes dropped), so ``tests/common/./mod.rs`` is
``tests/common/mod.rs``; the count is ASCII digits only. A bad line is exit 1
with the allowlist path, the line number and the reason.
"""
import argparse
import os
import re
import sys
from pathlib import Path, PurePosixPath

TOKENS = ("rusqlite", "Connection::open", "state.db", "global.db")
DEFAULT_ALLOWLIST = "scripts/tests_db_allowlist.txt"
TESTS_DIR = "tests"
MAX_LINES_SHOWN = 5
ASCII_COUNT = re.compile(r"[0-9]+")  # not \d or str.isdigit: those accept ² and １


class CheckError(Exception):
    """A problem with the inputs (not a red result): exit 1 with the message."""


def rel_posix(path, root):
    """`path` relative to `root`, with `/` separators, as a string."""
    return path.relative_to(root).as_posix()


def count_hits(text):
    """(count, [(line_number, line_text)]) of the lines that hit a token."""
    hits = []
    for number, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if stripped.startswith("//"):
            continue
        if any(token in stripped for token in TOKENS):
            hits.append((number, stripped))
    return len(hits), hits


def rs_files(root):
    """Every .rs file under root/tests, sorted by relative path.

    `os.walk` with `onerror`, not `Path.rglob`: rglob swallows the OSError
    of a directory it cannot list (Python 3.13), so a partial scan would
    look complete. Here a directory that cannot be listed is a CheckError
    with its path and the reason.
    """
    tests = root / TESTS_DIR

    def cannot_list(error):
        where = getattr(error, "filename", None) or str(tests)
        try:
            shown = rel_posix(Path(where), root)
        except ValueError:
            shown = str(where)
        raise CheckError(f"cannot list {shown}: {error}") from error

    files = []
    for dirpath, dirnames, filenames in os.walk(tests, onerror=cannot_list):
        dirnames.sort()
        for name in filenames:
            if not name.endswith(".rs"):
                continue
            path = Path(dirpath) / name
            if path.is_file():
                files.append(path)
    return sorted(files, key=lambda p: rel_posix(p, root))


def scan(root):
    """{relative posix path: (count, hits)} for every .rs under root/tests."""
    tests = root / TESTS_DIR
    if not tests.is_dir():
        raise CheckError(f"no {TESTS_DIR}/ directory under {root}")
    found = {}
    for path in rs_files(root):
        try:
            data = path.read_bytes()
        except OSError as e:
            raise CheckError(f"cannot read {rel_posix(path, root)}: {e}") from e
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError as e:
            raise CheckError(f"cannot decode {rel_posix(path, root)} as UTF-8: {e}") from e
        found[rel_posix(path, root)] = count_hits(text)
    return found


def listed_path_key(rel, where):
    """The allowlist path `rel` as the key the scan uses (`tests/...`).

    `\\` becomes `/`, `.` segments and doubled slashes are dropped, so
    `tests/common/./mod.rs`, `./tests/common/mod.rs` and `tests\\common\\mod.rs`
    are all `tests/common/mod.rs`; the normalised key is what is stored,
    compared with the scan and checked for duplicates. An absolute path, a
    `..` segment or a path outside tests/ is a CheckError naming `where`
    (allowlist path and line number).
    """
    slashed = rel.replace("\\", "/")
    pure = PurePosixPath(slashed)
    if pure.is_absolute() or (len(slashed) > 1 and slashed[1] == ":"):
        raise CheckError(f"{where}: {rel} is an absolute path; use a path relative to the repo root")
    if ".." in pure.parts or pure.parts[:1] != (TESTS_DIR,) or len(pure.parts) < 2:
        raise CheckError(f"{where}: {rel} is not a path under {TESTS_DIR}/")
    return pure.as_posix()


def listed_count(count_text, rel, where):
    """The allowed count as an int: ASCII digits only.

    `str.isdigit` is not enough: `²` passes it and `int` then raises, and a
    fullwidth `１` or an Arabic-Indic `٣` passes it and `int` accepts it as
    1 or 3. Anything but `[0-9]+` is a CheckError naming `where` (allowlist
    path and line number), the path and the text.
    """
    if not ASCII_COUNT.fullmatch(count_text):
        raise CheckError(
            f"{where}: count for {rel} must be a non-negative integer (ASCII digits only), got {count_text!r}"
        )
    try:
        return int(count_text)
    except ValueError as e:  # only the digit-count limit of int() can get here
        raise CheckError(
            f"{where}: count for {rel} must be a non-negative integer, got {count_text!r}: {e}"
        ) from None


def parse_allowlist(path):
    """{relative posix path: allowed count} from the allowlist file.

    Keys are normalised by `listed_path_key`, so they match the scan's keys
    and two spellings of one file are a duplicate; counts are checked by
    `listed_count`. Every problem is a CheckError with `path:line`.
    """
    try:
        data = path.read_bytes()
    except FileNotFoundError:
        raise CheckError(
            f"allowlist {path} does not exist; create it from `--print-counts`"
        ) from None
    except OSError as e:
        raise CheckError(f"cannot read allowlist {path}: {e}") from e
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as e:
        raise CheckError(f"cannot decode allowlist {path} as UTF-8: {e}") from e
    allowed = {}
    for number, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) != 2:
            raise CheckError(
                f"{path}:{number}: expected `path count`, got {line!r}"
            )
        rel, count_text = parts
        where = f"{path}:{number}"
        count = listed_count(count_text, rel, where)
        key = listed_path_key(rel, where)
        if key in allowed:
            spelled = "" if key == rel else f" (written as {rel} here)"
            raise CheckError(f"{where}: {key} is listed more than once{spelled}")
        allowed[key] = count
    return allowed


def print_counts(found, out):
    """The current counts in allowlist format (files with hits, sorted)."""
    for rel in sorted(found):
        count, _ = found[rel]
        if count > 0:
            out.write(f"{rel} {count}\n")


def judge(found, allowed, out):
    """Compare the scan with the allowlist; print the verdict; return exit code."""
    failures = 0
    for rel in sorted(set(found) | set(allowed)):
        actual = found[rel][0] if rel in found else None
        hits = found[rel][1] if rel in found else []
        wanted = allowed.get(rel)
        if rel not in found:
            out.write(f"{rel}: listed in the allowlist with {wanted} line(s) but the file does not exist; remove the entry\n")
            failures += 1
        elif wanted is None:
            if actual > 0:
                out.write(f"{rel}: {actual} line(s) open or name the database, allowed 0 (not in the allowlist)\n")
                for number, text in hits[:MAX_LINES_SHOWN]:
                    out.write(f"  {rel}:{number}: {text}\n")
                failures += 1
        elif actual > wanted:
            out.write(f"{rel}: {actual} line(s), allowed {wanted}: more than the allowlist permits\n")
            for number, text in hits[:MAX_LINES_SHOWN]:
                out.write(f"  {rel}:{number}: {text}\n")
            failures += 1
        elif actual < wanted:
            out.write(f"{rel}: {actual} line(s), allowed {wanted}: fewer than listed; lower the number in the allowlist to {actual} (it only goes down)\n")
            failures += 1
    if failures:
        out.write(f"FAIL: {failures} file(s)\n")
        return 1
    scanned = len(found)
    permitted = sum(allowed.values())
    out.write(f"ok: scanned {scanned} file(s) under {TESTS_DIR}/, {permitted} database line(s) allowed in {len(allowed)} file(s)\n")
    return 0


def main(argv=None):
    # UTF-8 on all three standard streams, whatever the console code page
    # (cp950 on a Traditional Chinese Windows); LF on the output streams
    # whatever the platform's text-mode newline, so
    # `--print-counts > scripts/tests_db_allowlist.txt` is the same file on
    # Windows and Linux.
    streams = (
        (sys.stdin, {}),
        (sys.stdout, {"newline": "\n"}),
        (sys.stderr, {"newline": "\n"}),
    )
    for stream, extra in streams:
        try:
            stream.reconfigure(encoding="utf-8", errors="replace", **extra)
        except (AttributeError, ValueError):
            pass
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--root",
        default=None,
        help="repository root (default: the parent of the scripts/ directory)",
    )
    parser.add_argument(
        "--allowlist",
        default=DEFAULT_ALLOWLIST,
        help=f"allowlist file (default: {DEFAULT_ALLOWLIST}; a relative path is taken from --root)",
    )
    parser.add_argument(
        "--print-counts",
        action="store_true",
        help="print the current counts in allowlist format and exit 0; the allowlist is not read",
    )
    args = parser.parse_args(argv)
    root = Path(args.root) if args.root else Path(__file__).resolve().parent.parent
    root = root.resolve()
    allowlist = Path(args.allowlist)
    if not allowlist.is_absolute():
        allowlist = root / allowlist
    try:
        found = scan(root)
        if args.print_counts:
            print_counts(found, sys.stdout)
            return 0
        allowed = parse_allowlist(allowlist)
    except CheckError as e:
        sys.stderr.write(f"error: {e}\n")
        return 1
    return judge(found, allowed, sys.stdout)


if __name__ == "__main__":
    sys.exit(main())
