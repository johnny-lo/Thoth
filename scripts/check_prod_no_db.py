#!/usr/bin/env python3
"""prod-no-db check: production code must not call the database outside src/db/ (SQL-4).

Scans every ``.rs`` file under ``src/`` (recursively) except the files under
``src/db/``, and counts the production lines that call the database through
rusqlite: a line that, stripped, does not start with ``//`` and matches
``.execute(``, ``.execute_batch(``, ``.prepare(``, ``.prepare_cached(`` or
``.query_row(``. Any hit is red: there is no allowlist, a call that has to
stay is a call that has to move into ``src/db/``. The script only reads: it
never edits a file, never runs git, never uses the network, and needs only
the standard library.

    python3 scripts/check_prod_no_db.py                 # exit 0: ok / 1: FAIL
    python3 scripts/check_prod_no_db.py --print-counts  # `PATH N` per file with
                                                        # hits, exit 0

Counting rule (one hit per line at most; a text rule, so a string or a
trailing comment counts too). Production versus inline tests:

* a file named ``tests.rs``, or with a path part ``tests``, is all test code
  and never counts;
* any other file is production above its test region and test code from there
  to the end of the file. The test region starts at the first line whose
  stripped text is exactly ``#[cfg(test)]`` and whose next line that is neither
  blank nor another ``#[...]`` attribute starts with ``mod``, ``pub mod`` or
  ``pub(crate) mod`` and ends with ``{``. A ``#[cfg(test)]`` over a ``const``,
  a ``use`` or a ``mod NAME;`` does not start it, and neither does one quoted
  in a doc comment; a file with no such module is all production.

A scan that cannot finish is never green: a missing ``src/`` directory, a
directory that cannot be listed, or a file that cannot be read or is not UTF-8
is exit 1 with ``error:`` on stderr naming the path and the reason (also under
``--print-counts``).
"""
import argparse
import os
import re
import sys
from pathlib import Path

CALL = re.compile(r"\.(execute|execute_batch|prepare|prepare_cached|query_row)\(")
SRC_DIR = "src"
DB_DIR = ("src", "db")
MOD_PREFIXES = ("mod ", "pub mod ", "pub(crate) mod ")
MAX_LINES_SHOWN = 5


class CheckError(Exception):
    """A problem with the inputs (not a red result): exit 1 with the message."""


def rel_posix(path, root):
    """`path` relative to `root`, with `/` separators, as a string."""
    return path.relative_to(root).as_posix()


def is_all_test(rel):
    """A file that is test code from its first line: tests.rs or under a tests/ dir."""
    parts = rel.split("/")
    return parts[-1] == "tests.rs" or "tests" in parts[:-1]


def test_region_start(lines):
    """Index of the `#[cfg(test)]` line that opens the inline test module, or None."""
    for i, line in enumerate(lines):
        if line.strip() != "#[cfg(test)]":
            continue
        for follow in lines[i + 1:]:
            text = follow.strip()
            if not text or text.startswith("#["):
                continue
            if text.startswith(MOD_PREFIXES) and text.endswith("{"):
                return i
            break
    return None


def count_hits(rel, text):
    """(count, [(line_number, line_text)]) of the production database calls."""
    if is_all_test(rel):
        return 0, []
    lines = text.splitlines()
    start = test_region_start(lines)
    production = lines if start is None else lines[:start]
    hits = []
    for number, line in enumerate(production, start=1):
        stripped = line.strip()
        if stripped.startswith("//"):
            continue
        if CALL.search(stripped):
            hits.append((number, stripped))
    return len(hits), hits


def rs_files(root):
    """Every .rs file under root/src outside src/db, sorted by relative path.

    `os.walk` with `onerror`, not `Path.rglob`: rglob swallows the OSError
    of a directory it cannot list (Python 3.13), so a partial scan would
    look complete. Here a directory that cannot be listed is a CheckError
    with its path and the reason.
    """
    src = root / SRC_DIR

    def cannot_list(error):
        where = getattr(error, "filename", None) or str(src)
        try:
            shown = rel_posix(Path(where), root)
        except ValueError:
            shown = str(where)
        raise CheckError(f"cannot list {shown}: {error}") from error

    files = []
    for dirpath, dirnames, filenames in os.walk(src, onerror=cannot_list):
        dirnames.sort()
        for name in filenames:
            if not name.endswith(".rs"):
                continue
            path = Path(dirpath) / name
            if path.relative_to(root).parts[: len(DB_DIR)] == DB_DIR:
                continue
            if path.is_file():
                files.append(path)
    return sorted(files, key=lambda p: rel_posix(p, root))


def scan(root):
    """{relative posix path: (count, hits)} for every .rs under root/src outside src/db."""
    if not (root / SRC_DIR).is_dir():
        raise CheckError(f"no {SRC_DIR}/ directory under {root}")
    found = {}
    for path in rs_files(root):
        rel = rel_posix(path, root)
        try:
            data = path.read_bytes()
        except OSError as e:
            raise CheckError(f"cannot read {rel}: {e}") from e
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError as e:
            raise CheckError(f"cannot decode {rel} as UTF-8: {e}") from e
        found[rel] = count_hits(rel, text)
    return found


def print_counts(found, out):
    """`PATH N` for every file with production hits, sorted."""
    for rel in sorted(found):
        count, _ = found[rel]
        if count > 0:
            out.write(f"{rel} {count}\n")


def judge(found, out):
    """Print the verdict; return the exit code."""
    files = 0
    calls = 0
    for rel in sorted(found):
        count, hits = found[rel]
        if count == 0:
            continue
        out.write(f"{rel}: {count} production database call(s) outside src/db/ (allowed 0)\n")
        for number, text in hits[:MAX_LINES_SHOWN]:
            out.write(f"  {rel}:{number}: {text}\n")
        files += 1
        calls += count
    if calls:
        out.write(f"FAIL: {calls} call(s) in {files} file(s)\n")
        return 1
    out.write(f"ok: scanned {len(found)} file(s) under {SRC_DIR}/ outside src/db/, 0 production database call(s)\n")
    return 0


def main(argv=None):
    # UTF-8 on all three standard streams, whatever the console code page
    # (cp950 on a Traditional Chinese Windows); LF on the output streams
    # whatever the platform's text-mode newline.
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
        "--print-counts",
        action="store_true",
        help="print the production database calls per file as `PATH N` (files with hits only) and exit 0",
    )
    args = parser.parse_args(argv)
    root = Path(args.root) if args.root else Path(__file__).resolve().parent.parent
    root = root.resolve()
    try:
        found = scan(root)
    except CheckError as e:
        sys.stderr.write(f"error: {e}\n")
        return 1
    if args.print_counts:
        print_counts(found, sys.stdout)
        return 0
    return judge(found, sys.stdout)


if __name__ == "__main__":
    sys.exit(main())
