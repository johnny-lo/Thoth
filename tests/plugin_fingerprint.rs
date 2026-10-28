use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const PLUGIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugins/thoth");
const RECORD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugins/thoth-fingerprint.txt");

fn files(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, base, out);
        } else {
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
}

/// sha256 over every regular file: relative path, `\n`, content with `\r\n`
/// turned into `\n`, `\0`; files in byte order of the relative path.
fn fingerprint(root: &Path) -> String {
    let mut list = Vec::new();
    files(root, root, &mut list);
    list.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut h = Sha256::new();
    for (rel, path) in list {
        h.update(rel.as_bytes());
        h.update(b"\n");
        let bytes = fs::read(path).unwrap();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                i += 1;
                continue;
            }
            h.update([bytes[i]]);
            i += 1;
        }
        h.update(b"\0");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn records() -> Vec<(String, String)> {
    fs::read_to_string(RECORD)
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (v, h) = l.split_once(' ').expect("VERSION HEX");
            (v.to_owned(), h.to_owned())
        })
        .collect()
}

fn version_numbers(v: &str) -> [u64; 3] {
    let p = v
        .split('.')
        .map(|x| x.parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(p.len(), 3, "{v}");
    [p[0], p[1], p[2]]
}

#[test]
fn plugin_content_changes_only_with_a_version_bump() {
    let computed = fingerprint(Path::new(PLUGIN));
    let (version, hex) = records().pop().expect("a record line");
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(Path::new(PLUGIN).join(".claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["version"], version.as_str());
    assert_eq!(env!("CARGO_PKG_VERSION"), version);
    assert_eq!(
        hex, computed,
        "computed {computed}, recorded version {version} hex {hex}: plugins/thoth changed: bump version in Cargo.toml and plugins/thoth/.claude-plugin/plugin.json (and tests/cli_smoke.rs), then append \"NEWVERSION {computed}\" to plugins/thoth-fingerprint.txt"
    );
}

#[test]
fn fingerprint_versions_are_unique_and_increase() {
    let recs = records();
    assert!(!recs.is_empty());
    let mut prev: Option<[u64; 3]> = None;
    for (v, _) in recs {
        let n = version_numbers(&v);
        if let Some(p) = prev {
            assert!(n > p, "{v} is not greater than the line before it");
        }
        prev = Some(n);
    }
}

fn copy_tree(from: &Path, to: &Path, crlf: bool) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let dest = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &dest, crlf);
        } else if crlf {
            let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
            fs::write(&dest, text.replace("\r\n", "\n").replace('\n', "\r\n")).unwrap();
        } else {
            fs::copy(&path, &dest).unwrap();
        }
    }
}

#[test]
fn fingerprint_ignores_crlf_but_sees_content_and_new_files() {
    let computed = fingerprint(Path::new(PLUGIN));
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("plugin");
    copy_tree(Path::new(PLUGIN), &copy, true);
    assert_eq!(fingerprint(&copy), computed);

    let skill = copy.join("skills/thoth-dispatch/SKILL.md");
    let original = fs::read(&skill).unwrap();
    let mut changed = original.clone();
    changed.push(b'x');
    fs::write(&skill, changed).unwrap();
    assert_ne!(fingerprint(&copy), computed);
    fs::write(&skill, original).unwrap();
    assert_eq!(fingerprint(&copy), computed);

    fs::write(copy.join("extra.txt"), "x").unwrap();
    assert_ne!(fingerprint(&copy), computed);

    let ab = dir.path().join("ab");
    let ba = dir.path().join("ba");
    fs::create_dir_all(&ab).unwrap();
    fs::create_dir_all(&ba).unwrap();
    fs::write(ab.join("a.txt"), "1").unwrap();
    fs::write(ab.join("b.txt"), "2").unwrap();
    fs::write(ba.join("b.txt"), "2").unwrap();
    fs::write(ba.join("a.txt"), "1").unwrap();
    assert_eq!(fingerprint(&ab), fingerprint(&ba));
}
