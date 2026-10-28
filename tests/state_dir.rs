use assert_cmd::Command;

/// `thoth profile list` with both state variables cleared and both home
/// variables pointed at `home`.
fn thoth(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    c.env_remove("THOTH_HOME");
    c.env_remove("AGENTCTL_HOME");
    c.env("USERPROFILE", home);
    c.env("HOME", home);
    c.args(["profile", "list"]);
    c
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = walk(dir);
    v.sort();
    v
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        out.push(p.display().to_string());
        if p.is_dir() {
            out.extend(walk(&p));
        }
    }
    out
}

fn command_line_is_clean(line: &str) -> bool {
    !line.trim_start().contains('(')
}

#[test]
fn the_old_variable_alone_is_refused_and_both_names_are_in_the_message() {
    let home = tempfile::tempdir().unwrap();
    let out = thoth(home.path())
        .env("AGENTCTL_HOME", home.path().join("old"))
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(err.contains("AGENTCTL_HOME") && err.contains("THOTH_HOME"), "{err}");
    assert!(!home.path().join("old").exists());
}

#[test]
fn an_old_directory_without_a_new_one_is_refused_and_nothing_is_touched() {
    let home = tempfile::tempdir().unwrap();
    let old = home.path().join(".agentctl");
    std::fs::create_dir_all(old.join("profiles")).unwrap();
    std::fs::write(old.join("global.db"), b"x").unwrap();
    let before = listing(home.path());

    let out = thoth(home.path()).assert().failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let new = home.path().join(".thoth");
    for want in [
        old.display().to_string(),
        new.display().to_string(),
        "does not move the old directory".to_string(),
        "does not read it".to_string(),
        "thoth profile list".to_string(),
        "thoth profile set-config-dir".to_string(),
        "empty".to_string(),
        "THOTH_HOME".to_string(),
        "absolute paths".to_string(),
        "blob_path".to_string(),
        "gitdir".to_string(),
        "recommended".to_string(),
        "old `agentctl`".to_string(),
        "git worktree repair".to_string(),
    ] {
        assert!(err.contains(&want), "stderr lacks `{want}`:\n{err}");
    }
    #[cfg(windows)]
    let (present, absent) = (
        ["setx ", "Move-Item ", "New-Item "],
        ["export ", "mv ", "ln -s "],
    );
    #[cfg(not(windows))]
    let (present, absent) = (
        ["export ", "mv ", "ln -s "],
        ["setx ", "Move-Item ", "New-Item "],
    );
    for command in present {
        assert!(err.lines().any(|line| line.trim_start().starts_with(command)), "stderr lacks `{command}`:\n{err}");
    }
    for command in absent {
        assert!(!err.lines().any(|line| line.trim_start().starts_with(command)), "stderr has `{command}`:\n{err}");
    }
    for line in err.lines().map(str::trim_start).filter(|line| {
        ["setx ", "export ", "Move-Item ", "mv ", "New-Item ", "ln -s "]
            .iter()
            .any(|prefix| line.starts_with(prefix))
    }) {
        assert!(command_line_is_clean(line), "command line has explanatory suffix: {line}");
    }
    assert!(!command_line_is_clean("setx THOTH_HOME \"X\"   (then open a new window)"));
    assert_eq!(listing(home.path()), before, "the refusal must not create or change anything");
}

#[test]
fn the_new_directory_wins_even_when_the_old_one_is_still_there() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".agentctl")).unwrap();
    std::fs::create_dir(home.path().join(".thoth")).unwrap();
    thoth(home.path()).assert().success();
    assert!(home.path().join(".thoth").join("global.db").exists());
    assert!(!home.path().join(".agentctl").join("global.db").exists());
}

#[test]
fn neither_directory_is_a_normal_fresh_start() {
    let home = tempfile::tempdir().unwrap();
    thoth(home.path()).assert().success();
    assert!(home.path().join(".thoth").join("global.db").exists());
    assert!(!home.path().join(".agentctl").exists());
}
