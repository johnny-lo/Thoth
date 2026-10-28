//! The language of human-facing output (setup scope U9): `THOTH_LANG=zh-TW`
//! for Traditional Chinese, English otherwise. Every string a person reads
//! lives in `MESSAGES`; `--json` output never depends on the language.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    ZhTw,
}

impl Lang {
    /// The `THOTH_LANG` environment variable, then `language` in config.json,
    /// then English. `zh-tw` or `zh_tw` in any letter case, surrounding blanks
    /// ignored; anything else (unset, empty, plain `zh`) is English.
    pub fn from_env() -> Lang {
        Lang::parse(Some(&crate::commands::settings::effective("language")))
    }

    fn parse(v: Option<&str>) -> Lang {
        match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("zh-tw") | Some("zh_tw") => Lang::ZhTw,
            _ => Lang::En,
        }
    }
}

/// (key, English, Traditional Chinese). `{0}`, `{1}`, ... are filled by
/// `fill`. Shell commands are arguments, never part of a template.
const MESSAGES: &[(&str, &str, &str)] = &[
    // doctor: item labels
    ("doctor.label.state_dir", "Thoth state directory", "Thoth 狀態目錄"),
    ("doctor.label.cli_claude", "Claude Code CLI (claude)", "Claude Code 指令（claude）"),
    ("doctor.label.cli_codex", "Codex CLI (codex)", "Codex 指令（codex）"),
    ("doctor.label.cli_agy", "Antigravity CLI (agy)", "Antigravity 指令（agy）"),
    ("doctor.label.profiles", "Profiles", "帳號"),
    ("doctor.label.profile", "Profile {0}", "帳號 {0}"),
    ("doctor.label.repo", "This repository", "這個 repo"),
    ("doctor.label.checks", "Checks", "檢查"),
    ("doctor.label.config", "Config file", "設定檔"),
    ("doctor.label.plugin", "Claude Code plugin", "Claude Code 插件"),
    // doctor: state directory
    ("doctor.state_dir.ok_env", "{0} (from THOTH_HOME)", "{0}（由 THOTH_HOME 指定）"),
    ("doctor.state_dir.ok_default", "{0} (default location)", "{0}（預設位置）"),
    (
        "doctor.state_dir.missing_env",
        "{0} (from THOTH_HOME) does not exist yet; it is created the first time a command needs it",
        "{0}（由 THOTH_HOME 指定）還不存在；第一次有指令用到時會建立",
    ),
    (
        "doctor.state_dir.missing_default",
        "{0} (default location) does not exist yet; it is created the first time a command needs it",
        "{0}（預設位置）還不存在；第一次有指令用到時會建立",
    ),
    (
        "doctor.state_dir.old_dir_only",
        "found the old directory {0} but not {1} (default location); other thoth commands refuse to run until this is settled",
        "找到舊的目錄 {0}，但沒有 {1}（預設位置）；這件事處理好之前，其他 thoth 指令都會拒絕執行",
    ),
    (
        "doctor.state_dir.agentctl_var_only",
        "AGENTCTL_HOME is set ({0}) but THOTH_HOME is not, so the state directory would be {1} (default location); other thoth commands refuse to run until THOTH_HOME is set",
        "設了 AGENTCTL_HOME（{0}）但沒有設 THOTH_HOME，狀態目錄會是 {1}（預設位置）；設好 THOTH_HOME 之前，其他 thoth 指令都會拒絕執行",
    ),
    (
        "doctor.fix.persist_old_dir",
        "Keep using the old directory (other ways: docs/phase1/usage.md, Coming from agentctl): {0}",
        "繼續用舊的目錄（其他做法見 docs/phase1/usage.md 的 Coming from agentctl）：{0}",
    ),
    ("doctor.fix.persist_var", "Set THOTH_HOME to the same directory: {0}", "把 THOTH_HOME 設成同一個目錄：{0}"),
    ("doctor.fix.new_window", "Then open a new window.", "然後開一個新視窗。"),
    ("doctor.fix.shell_profile", "Put it in your shell profile.", "把它寫進 shell profile。"),
    // doctor: CLIs
    ("doctor.cli.ok", "{0}", "{0}"),
    ("doctor.cli.not_run", "could not run {0}: {1}", "無法執行 {0}：{1}"),
    ("doctor.cli.exit", "{0} --version exited with {1}: {2}", "{0} --version 結束碼 {1}：{2}"),
    ("doctor.cli.unused", "(no profile uses it)", "（目前沒有帳號用它）"),
    ("doctor.fix.install_claude", "Install it: {0}", "安裝：{0}"),
    ("doctor.fix.install_codex", "Install it: {0}", "安裝：{0}"),
    ("doctor.fix.install_agy", "Install it from {0}", "從 {0} 安裝"),
    ("doctor.fix.install_agy_linux", "or: {0}", "或：{0}"),
    ("doctor.fix.rerun", "Then rerun thoth doctor.", "裝好後重跑 thoth doctor。"),
    // doctor: profiles
    ("doctor.profiles.no_db", "no profiles registered (no global.db yet)", "還沒有登記任何帳號（還沒有 global.db）"),
    ("doctor.profiles.none", "no profiles registered", "還沒有登記任何帳號"),
    ("doctor.profiles.ok", "{0} registered", "已登記 {0} 個"),
    ("doctor.profiles.error", "cannot read profiles: {0}", "讀不到帳號：{0}"),
    ("doctor.fix.profile_add", "Log in to one; it is registered for you: {0}", "登入一個，會自動登記：{0}"),
    (
        "doctor.fix.profile_docs",
        "See One-time setup in docs/phase1/usage.md.",
        "做法見 docs/phase1/usage.md 的 One-time setup。",
    ),
    ("doctor.profile.ok", "logged in", "已登入"),
    ("doctor.profile.ok_with", "logged in ({0})", "已登入（{0}）"),
    ("doctor.profile.logged_out", "not logged in", "沒有登入"),
    ("doctor.profile.error", "cannot check the login: {0}", "無法確認登入狀態：{0}"),
    ("doctor.fix.login", "Log in: {0}", "登入：{0}"),
    // doctor: repository
    ("doctor.repo.ok", "{0} is initialized", "{0} 已經 init"),
    ("doctor.repo.not_init", "{0} is not initialized", "{0} 還沒有 init"),
    ("doctor.repo.not_git", "{0} is not inside a git repository", "{0} 不在 git repo 裡"),
    ("doctor.repo.error", "cannot identify the repository: {0}", "無法辨識這個 repo：{0}"),
    ("doctor.fix.init", "Initialize it: {0}", "初始化：{0}"),
    // doctor: checks
    ("doctor.checks.no_repo", "not in a git repository", "不在 git repo 裡"),
    ("doctor.checks.no_repo_id", "the repository could not be identified", "無法辨識這個 repo"),
    ("doctor.checks.not_init", "the repository is not initialized", "這個 repo 還沒有 init"),
    ("doctor.checks.error", "cannot read the checks: {0}", "讀不到檢查：{0}"),
    ("doctor.checks.counts", "{0} checks: {1} approved, {2} pending", "共 {0} 條檢查：{1} 條已核准，{2} 條待核准"),
    ("doctor.checks.missing", "{0}: program '{1}' not found on PATH", "{0}：PATH 上找不到程式 '{1}'"),
    ("doctor.checks.stale", "{0}: changed {1}", "{0}：有變動 {1}"),
    ("doctor.checks.stale_unknown", "{0}: fingerprint differs", "{0}：指紋不同"),
    ("doctor.checks.unreadable", "{0}: cannot fingerprint: {1}", "{0}：無法計算指紋：{1}"),
    ("doctor.checks.path_only", "{0}: program pinned by path only", "{0}：程式只綁路徑、不綁內容"),
    (
        "doctor.fix.checks_path",
        "Put the folder that contains {0} on PATH, then rerun thoth doctor: {1}",
        "把含有 {0} 的資料夾放進 PATH，再重跑 thoth doctor：{1}",
    ),
    (
        "doctor.fix.checks_stale",
        "Look at what changed (listed above), then approve again in a terminal: {0}",
        "看過上面列出的變動後，在終端機重新核准：{0}",
    ),
    ("doctor.fix.checks_pending", "Approve the pending checks in a terminal: {0}", "在終端機核准待核准的檢查：{0}"),
    ("doctor.fix.checks_none", "Add a check, for example: {0}", "加一條檢查，例如：{0}"),
    // doctor: config.json
    (
        "doctor.config.none",
        "{0} does not exist; every setting is at its default or comes from its environment variable",
        "{0} 不存在；每個設定都是預設值或來自環境變數",
    ),
    (
        "doctor.config.ok",
        "{0}: {1} known setting(s) stored; thoth config list shows every effective value and its source",
        "{0}：存了 {1} 個認得的設定；thoth config list 會列出每個生效的值與來源",
    ),
    (
        "doctor.config.broken",
        "{0} cannot be used ({1}); every setting stored in it is ignored",
        "{0} 讀不懂（{1}）；存在裡面的設定全部不算",
    ),
    ("doctor.config.not_object", "not a JSON object", "不是 JSON 物件"),
    ("doctor.config.unknown_keys", "{0} has unknown keys: {1}", "{0} 有不認得的鍵：{1}"),
    ("doctor.fix.config_broken", "Fix or delete this file: {0}", "修好或刪掉這個檔：{0}"),
    (
        "doctor.fix.config_unknown",
        "Remove these keys from {0} (thoth ignores them): {1}",
        "從 {0} 移除這些鍵（thoth 會忽略它們）：{1}",
    ),
    (
        "doctor.config.records_dir_missing",
        "{0}: records-dir {1} is not a directory; runs are recorded only on this computer until it exists",
        "{0}：records-dir {1} 不是資料夾；在它存在之前，run 只記在這台電腦",
    ),
    (
        "doctor.fix.records_dir",
        "Create the folder (clone the records repository there), or change it: {0}",
        "建立這個資料夾（把紀錄的 repo clone 到那裡），或改設定：{0}",
    ),
    // doctor: secrets.json (the two score API keys)
    (
        "doctor.config.secrets_broken",
        "{0}; {1} cannot be used ({2}); the keys stored in it are ignored",
        "{0}；{1} 讀不懂（{2}）；存在裡面的金鑰全部不算",
    ),
    ("doctor.config.secrets_unknown", "{0}; {1} has unknown keys: {2}", "{0}；{1} 有不認得的鍵：{2}"),
    ("doctor.config.secrets_exposed", "{0}; {1} is readable by other users", "{0}；{1} 其他使用者也讀得到"),
    ("doctor.fix.secrets_private", "Make it readable by you only: {0}", "改成只有你讀得到：{0}"),
    // doctor: plugin
    ("doctor.plugin.no_claude", "cannot run claude, see cli_claude", "無法執行 claude，見 cli_claude 那一項"),
    ("doctor.plugin.failed", "claude plugin list --json exited with {0}: {1}", "claude plugin list --json 結束碼 {0}：{1}"),
    (
        "doctor.plugin.bad_output",
        "claude plugin list --json did not print a JSON array: {0}",
        "claude plugin list --json 印出的不是 JSON 陣列：{0}",
    ),
    ("doctor.plugin.missing", "the thoth plugin is not installed", "沒有裝 thoth 插件"),
    ("doctor.plugin.disabled", "{0} is installed but not enabled", "{0} 有裝但沒有啟用"),
    ("doctor.plugin.ok", "{0} {1} is enabled", "{0} {1} 已啟用"),
    (
        "doctor.plugin.version",
        "{0} is version {1}, this thoth is {2}",
        "{0} 的版本是 {1}，這個 thoth 是 {2}",
    ),
    ("doctor.fix.plugin_install", "Install it: {0}", "安裝：{0}"),
    ("doctor.fix.plugin_enable", "Enable it: {0}", "啟用：{0}"),
    ("doctor.fix.plugin_update", "Update it: {0}", "更新：{0}"),
    // doctor: summary
    ("doctor.summary.ok", "Nothing needs fixing.", "沒有需要修的項目。"),
    ("doctor.summary.fail", "{0} item(s) need fixing.", "有 {0} 項要修。"),
    // profile login
    (
        "login.open.claude",
        "Opening claude for {0} (config dir {1}). Log in there (type /login to switch accounts), then type /exit to come back here.",
        "正在為 {0} 打開 claude（設定目錄 {1}）。在裡面登入（要換帳號就打 /login），完成後打 /exit 回到這裡。",
    ),
    (
        "login.open.codex",
        "Running codex login for {0} (CODEX_HOME {1}). It opens a browser and comes back by itself.",
        "正在為 {0} 執行 codex login（CODEX_HOME {1}）。它會打開瀏覽器，完成後自己回來。",
    ),
    (
        "login.open.agy",
        "Opening agy for {0}. Log in there, then press Ctrl+C twice (or Ctrl+D on an empty prompt) to come back here.",
        "正在為 {0} 打開 agy。在裡面登入，完成後按兩次 Ctrl+C（或在空白的輸入列按 Ctrl+D）回到這裡。",
    ),
    (
        "login.ok.new",
        "{0}: logged in ({1}), registered with config dir {2}.",
        "{0}：已登入（{1}），已登記，設定目錄 {2}。",
    ),
    (
        "login.ok.again",
        "{0}: logged in ({1}); the profile was already registered.",
        "{0}：已登入（{1}）；帳號原本就已登記。",
    ),
    (
        "login.not_logged_in.new",
        "{0} is not logged in; nothing was registered. Run the same command again: {1}",
        "{0} 沒有登入；沒有登記任何東西。再執行一次同樣的指令：{1}",
    ),
    (
        "login.not_logged_in.again",
        "{0} is still not logged in; the profile is unchanged. Run the same command again: {1}",
        "{0} 仍然沒有登入；帳號紀錄沒有變。再執行一次同樣的指令：{1}",
    ),
    (
        "login.refuse.no_tty",
        "thoth profile login needs a real terminal (stdin or stdout is not one). Open a terminal window and run: {0}",
        "thoth profile login 需要真的終端機（stdin 或 stdout 不是終端機）。請開一個終端機視窗執行：{0}",
    ),
    (
        "login.refuse.in_run",
        "thoth profile login refuses to run with THOTH_RUN_ID set: logging in is the human's job, not a worker's.",
        "設了 THOTH_RUN_ID 時 thoth profile login 拒絕執行：登入是人做的事，不是 worker 的。",
    ),
    (
        "login.refuse.need_harness",
        "{0} is not a registered profile yet; say which CLI it is for: --harness claude-code, codex or antigravity.",
        "{0} 還不是登記過的帳號；請用 --harness 說明是哪一家：claude-code、codex 或 antigravity。",
    ),
    (
        "login.refuse.need_harness_noid",
        "Say which CLI to log in to: --harness claude-code, codex or antigravity (the profile id is chosen for you).",
        "請用 --harness 說明要登入哪一家：claude-code、codex 或 antigravity（帳號名稱會自動取）。",
    ),
    (
        "login.refuse.harness_mismatch",
        "{0} is already registered with harness {1}; leave out --harness, or pick another id.",
        "{0} 已經以 harness {1} 登記；請拿掉 --harness，或換一個名稱。",
    ),
    (
        "login.refuse.provider_mismatch",
        "{0} is already registered with provider {1}; leave out --provider.",
        "{0} 已經以 provider {1} 登記；請拿掉 --provider。",
    ),
    (
        "login.refuse.agy_exists",
        "This machine's agy account is already registered as {0} (one agy account per machine user). Log in again with: {1}",
        "這台機器的 agy 帳號已經登記成 {0}（每個機器使用者只能有一個 agy 帳號）。要重新登入請執行：{1}",
    ),
    (
        "login.refuse.bad_id",
        "'{0}' cannot be a profile id made by login: use 1-64 letters, digits, '-', '_' or '.', not starting with '.'",
        "'{0}' 不能當 login 建立的帳號名稱：請用 1–64 個英數字、'-'、'_' 或 '.'，且不以 '.' 開頭",
    ),
    (
        "login.refuse.no_free_id",
        "every id from {0}-a to {0}-z is taken; give an id yourself.",
        "{0}-a 到 {0}-z 都用掉了；請自己給一個名稱。",
    ),
    (
        "login.refuse.id_case",
        "'{0}' differs from the registered profile {1} only in letter case; on Windows they would share one directory. Use {1}, or pick a clearly different id.",
        "'{0}' 和已登記的帳號 {1} 只差大小寫；在 Windows 上會共用同一個目錄。請用 {1}，或換一個明顯不同的名稱。",
    ),
    // setup
    (
        "setup.refuse.in_run",
        "thoth setup refuses to run with THOTH_RUN_ID set: setting up is the human's job, not a worker's.",
        "設了 THOTH_RUN_ID 時 thoth setup 拒絕執行：設定是人做的事，不是 worker 的。",
    ),
    (
        "setup.refuse.no_tty",
        "thoth setup needs a real terminal (stdin or stdout is not one; Claude Code's ! and pipes are not). Open a terminal window and run: {0}",
        "thoth setup 需要真的終端機（stdin 或 stdout 不是終端機；Claude Code 的 ! 與管線都不是）。請在終端機打 {0}",
    ),
    (
        "setup.note.tty_skipped",
        "thoth setup: terminal check skipped (THOTH_LOGIN_SKIP_TTY_CHECK=1)",
        "thoth setup：略過終端機檢查（THOTH_LOGIN_SKIP_TTY_CHECK=1）",
    ),
    (
        "setup.stop.state_dir",
        "Settle the state directory first, then run thoth setup again.",
        "先把狀態目錄處理好，再跑一次 thoth setup。",
    ),
    ("setup.fix.rerun", "Then run thoth setup again.", "裝好後再跑一次 thoth setup。"),
    ("setup.ask.login", "Log in to {0} now?", "現在登入 {0}？"),
    ("setup.ask.retry", "Try again?", "再試一次？"),
    (
        "setup.ask.add_account",
        "Add an account for {0} now? (the login screen opens here)",
        "要加一個 {0} 帳號嗎？（登入畫面會在這裡打開）",
    ),
    ("setup.ask.init", "Register this repository with thoth now?", "現在登記？"),
    (
        "setup.checks.skip_missing",
        "{0}: program '{1}' is not on PATH; skipped (put it on PATH and run thoth setup again)",
        "{0}：PATH 上找不到程式 '{1}'；先跳過（放進 PATH 後再跑一次 thoth setup）",
    ),
    (
        "setup.ask.checks_add",
        "Add these {0} check(s) to this repository (pending until approved)?",
        "加入這 {0} 條檢查？（核准前是待核准）",
    ),
    (
        "setup.ask.checks_approve",
        "Approve these {0} check(s) now? Pressing Y here is your approval.",
        "現在核准這 {0} 條檢查？在這裡按 Y 就是你的核准。",
    ),
    ("setup.live_quota.on", "live quota: on", "即時查額度：開"),
    ("setup.live_quota.off", "live quota: off", "即時查額度：關"),
    (
        "setup.ask.live_quota",
        "Turn it on? Then thoth profile usage --refresh asks agy, codex and claude for their own quota numbers, and recommend asks agy and codex before ranking when a reading is older than 10 minutes; it uses no model quota and reads no credentials.",
        "打開？之後 thoth profile usage --refresh 會請 agy、codex 與 claude 自己回報額度；recommend 排名前，agy 與 codex 的讀數超過 10 分鐘也會自動查。不花模型額度、不讀憑證。",
    ),
    ("setup.ask.plugin_install", "Install the Thoth plugin into Claude Code now?", "現在安裝 Thoth 插件？"),
    ("setup.ask.plugin_enable", "Enable it now?", "現在啟用？"),
    ("setup.ask.plugin_update", "Update it now (claude plugin update)?", "現在更新（claude plugin update）？"),
    ("setup.plugin.failed", "{0} failed ({1}); run the following yourself:", "{0} 失敗（{1}）；請自己執行下面的指令："),
    (
        "setup.plugin.version_left",
        "{0} is still {1} after the update; this thoth is {2}. Update thoth or the plugin's source so they match.",
        "更新後 {0} 仍是 {1}，這個 thoth 是 {2}。請更新 thoth 或插件的來源，讓兩者一致。",
    ),
    (
        "setup.summary.done",
        "All set. Next: in Claude Code say \"use Thoth to dispatch\", or type /thoth. Another account: thoth profile login --harness claude-code|codex|antigravity",
        "全部完成。下一步：在 Claude Code 裡說「用 Thoth 派工」，或打 /thoth。要再加一個帳號：thoth profile login --harness claude-code|codex|antigravity",
    ),
    (
        "setup.summary.left",
        "{0} item(s) still need attention (see above); run thoth setup again after fixing them.",
        "還有 {0} 項要處理（見上面）；處理好後再跑一次 thoth setup。",
    ),
    (
        "setup.summary.restart",
        "Restart Claude Code (or type /reload-plugins) to load the plugin change.",
        "重開 Claude Code（或打 /reload-plugins），插件的變更才會生效。",
    ),
    (
        "setup.fix.checks_path",
        "Put the folder that contains {0} on PATH, then run thoth setup again: {1}",
        "把含有 {0} 的資料夾放進 PATH，再跑一次 thoth setup：{1}",
    ),
    (
        "setup.checks.changed_since_listed",
        "{0} changed after it was listed; not approved. Run thoth setup again.",
        "{0} 在列出之後有變動；沒有核准。請再跑一次 thoth setup。",
    ),
    ("setup.checks.path_pin_refused", "{0} was not approved: {1}", "{0} 沒有核准：{1}"),
    ("setup.yn", "[Y/n]", "[Y/n]"),
    ("setup.ny", "[y/N]", "[y/N]"),
];

/// The text of `key` in `lang`; the key itself when the table lacks it.
pub fn text(lang: Lang, key: &str) -> &'static str {
    match MESSAGES.iter().find(|(k, _, _)| *k == key) {
        Some((_, en, zh)) => match lang {
            Lang::En => en,
            Lang::ZhTw => zh,
        },
        None => leak_key(key),
    }
}

/// A missing key is a programming slip, not a reason to panic in front of the
/// user; the table's own test keeps it from happening.
fn leak_key(key: &str) -> &'static str {
    Box::leak(key.to_string().into_boxed_str())
}

/// `template` with `{0}`, `{1}`, ... replaced by `args` in one pass, so an
/// argument that itself contains `{1}` is left alone.
pub fn fill(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let hit = after.find('}').and_then(|close| {
            after[..close]
                .parse::<usize>()
                .ok()
                .and_then(|n| args.get(n))
                .map(|a| (close, *a))
        });
        match hit {
            Some((close, a)) => {
                out.push_str(a);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_keys_are_unique_and_both_languages_are_filled() {
        let mut seen = std::collections::HashSet::new();
        for (k, en, zh) in MESSAGES {
            assert!(seen.insert(*k), "duplicate key {k}");
            assert!(!en.is_empty() && !zh.is_empty(), "{k}");
        }
    }

    #[test]
    fn lang_from_thoth_lang_values() {
        for (v, want) in [
            ("zh-TW", Lang::ZhTw),
            ("ZH-tw", Lang::ZhTw),
            ("zh_TW", Lang::ZhTw),
            (" zh-tw ", Lang::ZhTw),
            ("en", Lang::En),
            ("xx", Lang::En),
            ("zh", Lang::En),
            ("", Lang::En),
        ] {
            assert_eq!(Lang::parse(Some(v)), want, "{v:?}");
        }
        assert_eq!(Lang::parse(None), Lang::En);
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("THOTH_LANG");
        let prev_home = std::env::var_os("THOTH_HOME");
        let empty = tempfile::tempdir().unwrap();
        std::env::set_var("THOTH_HOME", empty.path());
        std::env::set_var("THOTH_LANG", "zh-TW");
        assert_eq!(Lang::from_env(), Lang::ZhTw);
        std::env::remove_var("THOTH_LANG");
        assert_eq!(Lang::from_env(), Lang::En);
        if let Some(v) = prev {
            std::env::set_var("THOTH_LANG", v);
        }
        match prev_home {
            Some(v) => std::env::set_var("THOTH_HOME", v),
            None => std::env::remove_var("THOTH_HOME"),
        }
    }

    #[test]
    fn lang_text_and_fill() {
        assert_eq!(text(Lang::ZhTw, "doctor.label.state_dir"), "Thoth 狀態目錄");
        assert_eq!(text(Lang::En, "doctor.label.repo"), "This repository");
        assert_eq!(text(Lang::En, "no.such.key"), "no.such.key");
        assert_eq!(fill("{0} and {1}", &["a", "{1}"]), "a and {1}");
        assert_eq!(fill("{x} {9} {", &["a"]), "{x} {9} {");
    }
}
