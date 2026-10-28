//! Windows harness executable resolution.
//!
//! `npm i -g @openai/codex` (or claude) leaves a `codex.cmd` shim on PATH and
//! no `codex.exe`, and Rust's `Command::new` only appends `.exe`: it never
//! consults PATHEXT. So the shim is read here, and its JS entry point is
//! launched directly as `node.exe <entry.js> ...`.
//!
//! A `.cmd`/`.bat` is never executed. Running one goes through `cmd.exe`,
//! which expands `%VAR%` and drops newlines in arguments (BatBadBut), and the
//! prompt is an argument. Reading the shim is all this module does: no
//! subprocess, only the file system and environment.

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

const HINT: &str = "set THOTH_CODEX_BIN or THOTH_CLAUDE_BIN to the real executable";
const DP0_PREFIX: &str = "%dp0%\\node_modules\\";

/// Resolves `program` + `args` to what should actually be launched. See
/// `process::resolve_harness_command` for the contract.
pub fn resolve(program: &str, args: &[String]) -> Result<(String, Vec<String>)> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    resolve_in(program, args, &dirs)
}

fn passthrough(program: &str, args: &[String]) -> Result<(String, Vec<String>)> {
    Ok((program.to_string(), args.to_vec()))
}

fn lower_ext(p: &str) -> Option<String> {
    let trimmed = p.trim_end_matches(['.', ' ']);
    Path::new(trimmed).extension().map(|e| e.to_string_lossy().to_lowercase())
}

fn find_in(dirs: &[PathBuf], file: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(file)).find(|p| p.is_file())
}

fn resolve_in(program: &str, args: &[String], dirs: &[PathBuf]) -> Result<(String, Vec<String>)> {
    let has_sep = program.contains('/') || program.contains('\\');
    let ext = lower_ext(program);
    let is_shell = matches!(ext.as_deref(), Some("cmd") | Some("bat"));

    let shim: PathBuf = if has_sep {
        if !is_shell {
            return passthrough(program, args);
        }
        PathBuf::from(program)
    } else if is_shell {
        match find_in(dirs, program) {
            Some(p) => p,
            None => bail!("{program} is a .cmd/.bat shell shim; thoth never runs .cmd/.bat files; {HINT}"),
        }
    } else {
        let exe = if ext.as_deref() == Some("exe") {
            program.to_string()
        } else {
            format!("{program}.exe")
        };
        if find_in(dirs, &exe).is_some() {
            return passthrough(program, args);
        }
        match dirs.iter().find_map(|dir| {
            ["cmd", "bat"].iter().find_map(|e| {
                let candidate = dir.join(format!("{program}.{e}"));
                candidate.is_file().then_some(candidate)
            })
        }) {
            Some(p) => p,
            None => return passthrough(program, args),
        }
    };
    let shim = std::path::absolute(&shim)
        .map_err(|e| anyhow!("cannot make shell shim {shim:?} absolute: {e}; {HINT}"))?;
    expand_shim(&shim, args, dirs).map_err(|e| anyhow!("{program}: {e}"))
}

/// The first double-quoted `%dp0%\node_modules\...` path ending in a JS
/// extension - the npm cmd-shim layout.
fn shim_entry(content: &str) -> Option<&str> {
    content.split('"').skip(1).step_by(2).find(|s| {
        let l = s.to_lowercase();
        l.starts_with(DP0_PREFIX)
            && [".js", ".cjs", ".mjs"].iter().any(|e| l.ends_with(e))
            // another variable in the path means it is not a plain file name
            && !s[DP0_PREFIX.len()..].contains('%')
    })
}

fn expand_shim(shim: &Path, args: &[String], dirs: &[PathBuf]) -> Result<(String, Vec<String>)> {
    let shim_disp = shim.display();
    let bytes = std::fs::read(shim)
        .map_err(|e| anyhow!("cannot read shell shim {shim_disp}: {e}; {HINT}"))?;
    let content = String::from_utf8_lossy(&bytes);
    let entry = shim_entry(&content).ok_or_else(|| {
        anyhow!(
            "{shim_disp} is not an npm cmd-shim, and thoth never runs .cmd/.bat files \
             (cmd.exe would rewrite the arguments); {HINT}"
        )
    })?;
    let base = match shim.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let js = base.join(&entry[DP0_PREFIX.len() - "node_modules\\".len()..]);
    if !js.is_file() {
        bail!("{shim_disp} points at {}, which does not exist; {HINT}", js.display());
    }
    let beside = base.join("node.exe");
    let node = if beside.is_file() {
        beside
    } else {
        find_in(dirs, "node.exe")
            .ok_or_else(|| anyhow!("{shim_disp} needs node.exe, found neither beside it nor on PATH; {HINT}"))?
    };
    let node = std::path::absolute(node)?;
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(js.to_string_lossy().to_string());
    out.extend(args.iter().cloned());
    Ok((node.to_string_lossy().to_string(), out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIM: &str = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\n\
SETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  \
SET \"_prog=node\"\r\n)\r\n\r\n\"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n";

    /// A dir holding `name` (shim text) and the JS entry it points at.
    fn shim_dir(name: &str, content: &str, with_js: bool) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(name), content).unwrap();
        if with_js {
            let js = d.path().join("node_modules/@openai/codex/bin");
            std::fs::create_dir_all(&js).unwrap();
            std::fs::write(js.join("codex.js"), "").unwrap();
        }
        d
    }

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn expected_js(d: &Path) -> String {
        d.join("node_modules").join("@openai").join("codex").join("bin").join("codex.js")
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn t1_shim_with_node_beside_it() {
        let d = shim_dir("codex.cmd", SHIM, true);
        std::fs::write(d.path().join("node.exe"), "").unwrap();
        let shim = d.path().join("codex.cmd");
        let (prog, args) = resolve_in(shim.to_str().unwrap(), &a(&["login", "status"]), &[]).unwrap();
        assert_eq!(Path::new(&prog), d.path().join("node.exe"));
        assert_eq!(args, vec![expected_js(d.path()), "login".to_string(), "status".to_string()]);
    }

    #[test]
    fn t2_no_node_beside_it_uses_node_from_path() {
        let d = shim_dir("codex.cmd", SHIM, true);
        let nodes = tempfile::tempdir().unwrap();
        std::fs::write(nodes.path().join("node.exe"), "").unwrap();
        let shim = d.path().join("codex.cmd");
        let (prog, args) =
            resolve_in(shim.to_str().unwrap(), &a(&["x"]), &[nodes.path().to_path_buf()]).unwrap();
        assert_eq!(Path::new(&prog), nodes.path().join("node.exe"));
        assert_eq!(args[0], expected_js(d.path()));
        assert_eq!(args[1], "x");

        // and with no node anywhere it is an error, not a guess
        let e = resolve_in(shim.to_str().unwrap(), &[], &[]).unwrap_err().to_string();
        assert!(e.contains("codex.cmd") && e.contains("THOTH_CODEX_BIN"), "{e}");
    }

    #[test]
    fn t3_non_shim_cmd_is_an_error() {
        let d = shim_dir("codex.cmd", "@echo off\r\necho hi > pwned.txt\r\n", false);
        let shim = d.path().join("codex.cmd");
        let e = resolve_in(shim.to_str().unwrap(), &[], &[]).unwrap_err().to_string();
        assert!(e.contains(&shim.display().to_string()), "{e}");
        assert!(e.contains("set THOTH_CODEX_BIN or THOTH_CLAUDE_BIN to the real executable"), "{e}");
    }

    #[test]
    fn t4_bare_name_found_as_cmd_on_path() {
        let d = shim_dir("foo.cmd", SHIM, true);
        std::fs::write(d.path().join("node.exe"), "").unwrap();
        let (prog, args) = resolve_in("foo", &a(&["a"]), &[d.path().to_path_buf()]).unwrap();
        assert_eq!(Path::new(&prog), d.path().join("node.exe"));
        assert_eq!(args, vec![expected_js(d.path()), "a".to_string()]);
    }

    #[test]
    fn t4b_exe_on_path_wins_over_a_cmd() {
        let d = shim_dir("foo.cmd", SHIM, true);
        std::fs::write(d.path().join("foo.exe"), "").unwrap();
        let (prog, args) = resolve_in("foo", &a(&["a"]), &[d.path().to_path_buf()]).unwrap();
        assert_eq!((prog.as_str(), args), ("foo", a(&["a"])));
    }

    #[test]
    fn t4c_bare_name_found_nowhere_is_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let (prog, args) = resolve_in("foo", &a(&["a"]), &[d.path().to_path_buf()]).unwrap();
        assert_eq!((prog.as_str(), args), ("foo", a(&["a"])));
    }

    #[test]
    fn t4d_first_path_directory_wins_across_shell_extensions() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let d = shim_dir("foo.bat", SHIM, true);
        std::fs::copy(d.path().join("foo.bat"), first.path().join("foo.bat")).unwrap();
        std::fs::write(first.path().join("node.exe"), "").unwrap();
        let js = first.path().join("node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&js).unwrap();
        std::fs::write(js.join("codex.js"), "").unwrap();
        std::fs::copy(d.path().join("foo.bat"), second.path().join("foo.cmd")).unwrap();
        let (prog, _) = resolve_in("foo", &[], &[first.path().to_path_buf(), second.path().to_path_buf()]).unwrap();
        assert_eq!(Path::new(&prog), first.path().join("node.exe"));
    }

    #[test]
    fn t4e_missing_bare_cmd_is_rejected() {
        let e = resolve_in("name.cmd", &[], &[]).unwrap_err().to_string();
        assert!(e.contains("name.cmd") && e.contains("THOTH_CODEX_BIN"), "{e}");
    }

    #[test]
    fn t4f_trailing_shell_suffix_is_rejected() {
        let e = resolve_in(r"C:\x\codex.cmd.", &[], &[]).unwrap_err().to_string();
        assert!(e.contains(r"C:\x\codex.cmd.") && e.contains("THOTH_CODEX_BIN"), "{e}");
    }

    #[test]
    fn t4f_trailing_shell_space_is_rejected() {
        let e = resolve_in("codex.cmd ", &[], &[]).unwrap_err().to_string();
        assert!(e.contains("codex.cmd ") && e.contains("THOTH_CODEX_BIN"), "{e}");
    }

    #[test]
    fn t4g_relative_shim_paths_resolve_to_absolute_launch_paths() {
        let shim = tempfile::tempdir_in(".").unwrap();
        std::fs::write(shim.path().join("codex.cmd"), SHIM).unwrap();
        let js = shim.path().join("node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&js).unwrap();
        std::fs::write(js.join("codex.js"), "").unwrap();
        std::fs::write(shim.path().join("node.exe"), "").unwrap();
        let relative_shim = shim.path().strip_prefix(std::env::current_dir().unwrap()).unwrap();
        let (prog, args) = resolve_in(relative_shim.join("codex.cmd").to_str().unwrap(), &[], &[]).unwrap();
        assert!(Path::new(&prog).is_absolute());
        assert!(Path::new(&args[0]).is_absolute());
    }

    #[test]
    fn t5_exe_path_is_untouched() {
        let (prog, args) = resolve_in(r"C:\nowhere\fake-codex.EXE", &a(&["a", "b"]), &[]).unwrap();
        assert_eq!((prog.as_str(), args), (r"C:\nowhere\fake-codex.EXE", a(&["a", "b"])));
    }

    #[test]
    fn t6_bat_is_handled_like_cmd() {
        let d = shim_dir("codex.bat", SHIM, true);
        std::fs::write(d.path().join("node.exe"), "").unwrap();
        let shim = d.path().join("codex.bat");
        let (prog, args) = resolve_in(shim.to_str().unwrap(), &a(&["q"]), &[]).unwrap();
        assert_eq!(Path::new(&prog), d.path().join("node.exe"));
        assert_eq!(args, vec![expected_js(d.path()), "q".to_string()]);
        // and found through PATH when named bare
        let (prog, _) = resolve_in("codex", &[], &[d.path().to_path_buf()]).unwrap();
        assert_eq!(Path::new(&prog), d.path().join("node.exe"));
    }

    #[test]
    fn t7_missing_js_is_an_error() {
        let d = shim_dir("codex.cmd", SHIM, false);
        std::fs::write(d.path().join("node.exe"), "").unwrap();
        let shim = d.path().join("codex.cmd");
        let e = resolve_in(shim.to_str().unwrap(), &[], &[]).unwrap_err().to_string();
        assert!(e.contains(&shim.display().to_string()) && e.contains("THOTH_CODEX_BIN"), "{e}");
    }
}
