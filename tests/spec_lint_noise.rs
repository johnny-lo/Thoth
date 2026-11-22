//! SMALL-17 (b): fewer false `missing_path` and `two_dot_diff` warnings from
//! `thoth spec lint`, in sentences taken from earlier specs, and the
//! sentences whose warnings stay. Driven only through `spec lint FILE --json`
//! in a temporary repository; no runner is started.

mod common;

use common::Fixture;
use serde_json::{json, Value};

/// An initialised repository whose main work tree holds the files the
/// sentences below name, and an empty `src/db/` folder kept by one file.
fn noise_fixture() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let repo = f.repo_path();
    for path in [
        "src/bin/fake-claude.rs",
        "tests/spec_review.rs",
        "tests/mcp.rs",
        "src/commands/review.rs",
        "tests/fixtures/antigravity/fc1-a.jsonl",
        "docs/phase1/x.md",
        "plan/log.jsonl",
        "src/runner/mod.rs",
        "src/db/.keep",
        "plugins/p.txt",
        "scripts/s.py",
    ] {
        let full = path.split('/').fold(repo.clone(), |acc, part| acc.join(part));
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, "x\n").unwrap();
    }
    f
}

/// The findings of `rule` in the warnings of `spec lint --json` for a spec
/// whose one acceptance criterion is `criterion`.
fn warnings(f: &Fixture, criterion: &str, rule: &str) -> Vec<Value> {
    let file = f.home_path().join("noise-spec.json");
    let spec = json!({"goal": "lint noise", "acceptance_criteria": [criterion]});
    std::fs::write(&file, spec.to_string()).unwrap();
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").output().unwrap();
    let report: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .unwrap_or_else(|e| panic!("spec lint printed no JSON ({e}): {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(out.status.code(), Some(0), "{report}");
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["rule"] == rule)
        .cloned()
        .collect()
}

/// The paths the `missing_path` warnings name, in order.
fn missing_paths(f: &Fixture, criterion: &str) -> Vec<String> {
    warnings(f, criterion, "missing_path")
        .iter()
        .map(|w| {
            let message = w["message"].as_str().unwrap();
            message
                .split_once(" is not in the main work tree")
                .unwrap_or_else(|| panic!("unexpected message: {message}"))
                .0
                .to_string()
        })
        .collect()
}

#[test]
fn globs_nouns_and_lists_of_new_paths_give_no_missing_path() {
    let f = noise_fixture();
    for criterion in [
        "擴充 src/bin/fake-*.rs 只能加新的 FAKE_ 開關，既有開關與輸出不變。",
        "只改 tests/spec_review*.rs、tests/mcp*.rs。",
        "src/commands/review*.rs 不動。",
        "以 ``in `tests/fixtures/antigravity/fc1-*` `` 結尾的那一句。",
        "文件在 docs/phase* 裡。",
        "tasks：T-1 code/high、T-2 code/normal、T-3 docs/low（status pass，risk normal）。",
        "寫進 plan/log 的那一筆。",
        "追蹤 README.md 與 `docs/User Guide.md`。",
        "new test files src/s17_first_one.rs and src/s17_second_one.rs.",
        "新檔 src/s17_alpha.rs、src/s17_beta.rs、src/s17_gamma.rs。",
        "create src/db/s17_pipes.rs, scripts/s17_check.py and scripts/s17_test_check.py.",
    ] {
        assert_eq!(missing_paths(&f, criterion), Vec::<String>::new(), "{criterion}");
    }
}

#[test]
fn stale_paths_still_give_missing_path() {
    let f = noise_fixture();
    for (criterion, named) in [
        ("src/runner.rs 的 run_worker_inner 不動。", vec!["src/runner.rs"]),
        ("src/s17_nothing_like_this*.rs 都要改。", vec!["src/s17_nothing_like_this*.rs"]),
        ("src/db/migrations/ 底下的檔。", vec!["src/db/migrations/"]),
        ("見 `plugins/s17_nothing`。", vec!["plugins/s17_nothing"]),
        (
            "c1（base，含 `src/s17_pre.rs` 與 `src/s17_other.rs`）。",
            vec!["src/s17_pre.rs", "src/s17_other.rs"],
        ),
        ("新檔 src/s17_delta.rs。src/s17_gone_later.rs 要改。", vec!["src/s17_gone_later.rs"]),
        ("the new function lives in src/s17_old_place.rs today.", vec!["src/s17_old_place.rs"]),
    ] {
        assert_eq!(missing_paths(&f, criterion), named, "{criterion}");
    }
}

#[test]
fn diff_inside_a_word_and_rust_ranges_give_no_two_dot_diff() {
    let f = noise_fixture();
    for criterion in [
        "`record_diff_attributed(.., from=c2, to=c3, Some(&Attribution{task_base: c1, integration_tip: None}))` 不變。",
        "a different range 0..10 is fine.",
    ] {
        assert_eq!(warnings(&f, criterion, "two_dot_diff"), Vec::<Value>::new(), "{criterion}");
    }
    for (criterion, first) in [
        ("run `git diff master..HEAD` now.", "master..HEAD"),
        ("evidence::record_diff 改用 `git diff --name-status --no-renames <from>..<to>`。", "<from>..<to>"),
        (
            "用 `git diff --name-status --no-renames <run.base_commit>..<candidate>` 算。",
            "<run.base_commit>..<candidate>",
        ),
    ] {
        let found = warnings(&f, criterion, "two_dot_diff");
        assert_eq!(found.len(), 1, "{criterion}: {found:?}");
        let message = found[0]["message"].as_str().unwrap();
        assert!(message.contains(&format!("(first: {first};")), "{criterion}: {message}");
    }
}
