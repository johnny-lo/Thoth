//! SMALL-13: shell scripts keep LF line endings in a clone made with
//! `core.autocrlf=true` (a Windows clone), because `sh` cannot read CRLF. The
//! rule is in this repository's `.gitattributes`; the test copies that file and
//! the repository's `.sh` files into a temporary repository and clones it the
//! way a Windows machine would.

mod common;

use std::path::Path;

const NO_CONVERSION: [&str; 6] = ["-c", "core.autocrlf=false", "-c", "user.name=thoth test", "-c", "user.email=test@example.com"];

fn git_with(dir: &Path, prefix: &[&str], args: &[&str]) -> String {
    let all: Vec<&str> = prefix.iter().chain(args.iter()).copied().collect();
    common::git(dir, &all)
}

#[test]
fn shell_scripts_check_out_with_lf_even_with_autocrlf() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    // `.gitattributes` is left out of the public tree (scripts/public_tree.exclude);
    // a checkout of that tree has no rule to test.
    if !here.join(".gitattributes").is_file() {
        eprintln!("no .gitattributes in this tree (the public tree leaves it out): nothing to check");
        return;
    }
    let listed = common::git(here, &["ls-files", "*.sh"]);
    let scripts: Vec<&str> = listed.lines().filter(|l| !l.is_empty()).collect();
    assert!(!scripts.is_empty(), "git ls-files lists no .sh file: the check would pass empty");

    let source = tempfile::tempdir().unwrap();
    let s = source.path();
    common::git(s, &["init", "--initial-branch=main"]);
    std::fs::copy(here.join(".gitattributes"), s.join(".gitattributes")).unwrap();
    for rel in &scripts {
        let to = s.join(rel);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(here.join(rel), &to).unwrap();
    }
    std::fs::write(s.join("notes.txt"), "one\ntwo\n").unwrap();
    git_with(s, &NO_CONVERSION, &["add", "-A"]);
    git_with(s, &NO_CONVERSION, &["commit", "-m", "scripts and notes"]);

    let parent = tempfile::tempdir().unwrap();
    common::git(parent.path(), &["-c", "core.autocrlf=true", "clone", s.to_str().unwrap(), "clone"]);
    let clone = parent.path().join("clone");

    // The conversion is active: a plain text file comes out with CRLF. Without
    // this the .sh assertion below would pass for any reason.
    assert_eq!(std::fs::read(clone.join("notes.txt")).unwrap(), b"one\r\ntwo\r\n", "autocrlf=true did not convert notes.txt");
    for rel in &scripts {
        let got = std::fs::read(clone.join(rel)).unwrap();
        assert!(!got.contains(&b'\r'), "{rel} has CR in the clone");
        assert_eq!(got, std::fs::read(here.join(rel)).unwrap(), "{rel} differs from the source in the clone");
    }
}
