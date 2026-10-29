/// `eprintln!` through the masker (RD-4): every line thoth writes to stderr
/// leaves masked. Defined before the modules so that all of them see it.
macro_rules! masked_eprintln {
    ($($arg:tt)*) => {
        $crate::output::eprintln_masked(&format!($($arg)*))
    };
}

mod adapter;
mod blobs;
mod brief;
mod checks;
mod cli;
mod commands;
mod config;
mod db;
mod evidence;
mod guide;
mod handoff;
mod ids;
mod ingest;
mod lang;
mod login_name;
mod mcp;
mod model;
mod model_list;
mod output;
mod plan_label;
mod plugin_check;
mod process;
mod protected;
mod records;
mod redact;
mod result_json;
mod review_fix_edits;
mod review_json;
mod run_quota;
mod runner;
mod spec;
mod spec_edits;
mod spec_review_json;
mod spec_review_prompt;
mod user_shell;
mod worktree;

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use clap::Parser;

/// RD-4: a parse error quotes the arguments as typed, so it is masked like
/// any other stderr line. The search runs on clap's plain text (on a terminal
/// clap puts a colour code right before a quoted argument, which would hide
/// a key format's start). With nothing to mask, or for --help and --version
/// (stdout, exit 0), clap prints as it always did.
fn parse_failed(e: clap::Error) -> ! {
    if !e.use_stderr() {
        e.exit()
    }
    let plain = e.render().to_string();
    let (masked, counts) = redact::Masker::current().mask_text(&plain);
    if counts.is_empty() {
        e.exit()
    }
    output::eprint_masked(&masked);
    std::process::exit(e.exit_code())
}

fn main() {
    let parsed = match cli::Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(e) => parse_failed(e),
    };
    // Read the --json flag before `run` consumes `parsed`, so the top-level
    // error handler can honor it even when the error happens before `run`
    // reaches the point where it would otherwise know.
    let json = match &parsed.command {
        cli::Command::Config { action } => match action {
            cli::ConfigAction::List { json } => *json,
            cli::ConfigAction::Get { json, .. } => *json,
            cli::ConfigAction::Set { json, .. } => *json,
            cli::ConfigAction::Unset { json, .. } => *json,
            cli::ConfigAction::SetSecret { json, .. } => *json,
            cli::ConfigAction::UnsetSecret { json, .. } => *json,
        },
        cli::Command::Records { action } => match action {
            cli::RecordsAction::Export { json } => *json,
        },
        cli::Command::Init { json } => *json,
        cli::Command::Mcp { .. } => false,
        cli::Command::Guide { json } => *json,
        cli::Command::Doctor { json, .. } => *json,
        cli::Command::Setup { .. } => false,
        cli::Command::Profile { action } => match action {
            cli::ProfileAction::Add { json, .. } => *json,
            cli::ProfileAction::List { json } => *json,
            cli::ProfileAction::Check { json, .. } => *json,
            cli::ProfileAction::Login { json, .. } => *json,
            cli::ProfileAction::SetConfigDir { json, .. } => *json,
            cli::ProfileAction::ForgetModel { json, .. } => *json,
            cli::ProfileAction::SetShared { json, .. } => *json,
            cli::ProfileAction::Usage { json, .. } => *json,
        },
        cli::Command::Recommend { json, .. } => *json,
        cli::Command::Models { json, .. } => *json,
        cli::Command::Report { json, .. } => *json,
        cli::Command::Retro { json, .. } => *json,
        cli::Command::Task { action } => match action {
            cli::TaskAction::Create { json, .. } => *json,
            cli::TaskAction::Show { json, .. } => *json,
            cli::TaskAction::UpdateSpec { json, .. } => *json,
            cli::TaskAction::Close { json, .. } => *json,
            cli::TaskAction::Accept { json, .. } => *json,
            cli::TaskAction::Result { json, .. } => *json,
        },
        cli::Command::RunWorker { .. } => false,
        cli::Command::Hook { .. } => false,
        cli::Command::Dispatch { json, .. } => *json,
        cli::Command::Status { json, .. } => *json,
        cli::Command::Events { json, .. } => *json,
        cli::Command::Wait { json, .. } => *json,
        cli::Command::Cancel { json, .. } => *json,
        cli::Command::Resume { json, .. } => *json,
        cli::Command::Submit { json, .. } => *json,
        cli::Command::Evidence { action } => match action {
            cli::EvidenceAction::List { json, .. } => *json,
            cli::EvidenceAction::Show { json, .. } => *json,
        },
        cli::Command::Transcript { json, .. } => *json,
        cli::Command::Review { action } => match action {
            cli::ReviewAction::Start { json, .. } => *json,
            cli::ReviewAction::Confirm { json, .. } => *json,
            cli::ReviewAction::Show { json, .. } => *json,
            cli::ReviewAction::Record { json, .. } => *json,
            cli::ReviewAction::Packet { json, .. } => *json,
        },
        cli::Command::Spec { action } => match action {
            cli::SpecAction::Review { action } => match action {
                cli::SpecReviewAction::Start { json, .. } => *json,
                cli::SpecReviewAction::Confirm { json, .. } => *json,
                cli::SpecReviewAction::Record { json, .. } => *json,
                cli::SpecReviewAction::Show { json, .. } => *json,
            },
            cli::SpecAction::Lint { json, .. } => *json,
        },
        cli::Command::Verify { json, .. } => *json,
        cli::Command::Decision { action } => match action {
            cli::DecisionAction::Add { json, .. } => *json,
        },
        cli::Command::Checks { action } => match action {
            cli::ChecksAction::Add { json, .. } => *json,
            cli::ChecksAction::Approve { json, .. } => *json,
            cli::ChecksAction::List { json } => *json,
            cli::ChecksAction::Remove { json, .. } => *json,
            cli::ChecksAction::Protect { json, .. } => *json,
            cli::ChecksAction::Unprotect { json, .. } => *json,
        },
        cli::Command::Worktrees { action } => match action {
            cli::WorktreesAction::Prune { json } => *json,
        },
    };
    let code = match run(parsed) {
        Ok(code) => code,
        Err(e) => {
            if json {
                if let Some(stuck) = e.downcast_ref::<commands::dispatch::StuckDispatchError>() {
                    let mut doc = serde_json::json!({
                        "ok": false,
                        "error": stuck.message,
                        "stuck": {"kind": stuck.reason.kind(), "value": stuck.reason.value()}
                    });
                    redact::Masker::current().mask_value(&mut doc);
                    masked_eprintln!("{doc}");
                } else {
                    output::emit_err(true, &format!("{e:#}"));
                }
            } else {
                output::emit_err(false, &format!("{e:#}"));
            }
            1
        }
    };
    std::process::exit(code);
}

/// Hands back a state-changing command's own result unchanged after
/// rewriting report.html (only if it already exists; errors ignored, nothing
/// printed). Every such command has committed its writes by the time it
/// returns, so the page never sees a half-applied change.
fn refreshing(paths: &config::Paths, result: anyhow::Result<i32>) -> anyhow::Result<i32> {
    commands::report::refresh_if_present(paths);
    result
}

fn run(parsed: cli::Cli) -> anyhow::Result<i32> {
    match parsed.command {
        cli::Command::Config { action } => {
            let paths = config::Paths::from_env()?;
            match action {
                cli::ConfigAction::List { json } => {
                    let cwd = std::env::current_dir()?;
                    commands::settings::list(&paths, &cwd, json)
                }
                cli::ConfigAction::Get { key, json } => commands::settings::get(&paths, &key, json),
                cli::ConfigAction::Set { key, value, json } => commands::settings::set(&paths, &key, &value, json),
                cli::ConfigAction::Unset { key, json } => commands::settings::unset(&paths, &key, json),
                cli::ConfigAction::SetSecret { key, value, json } => {
                    commands::settings::set_secret(&paths, &key, &value, json)
                }
                cli::ConfigAction::UnsetSecret { key, json } => {
                    commands::settings::unset_secret(&paths, &key, json)
                }
            }
        }
        cli::Command::Records { action } => {
            let paths = config::Paths::from_env()?;
            match action {
                cli::RecordsAction::Export { json } => records::export_command(&paths, json),
            }
        }
        cli::Command::Init { json } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            commands::init::run(&paths, &cwd, json)
        }
        cli::Command::Mcp { default_repo, max_wait, max_result_bytes, plugin_root } => {
            mcp::run(mcp::Options { default_repo, max_wait, max_result_bytes, plugin_root })
        }
        cli::Command::Guide { json } => guide::run(json),
        // No `Paths::from_env()`: doctor reports the old-directory case that
        // it would refuse (see `config::probe_state_dir`).
        cli::Command::Doctor { shell, json } => {
            let cwd = std::env::current_dir()?;
            commands::doctor::run(&cwd, shell.as_deref(), json)
        }
        // Same as doctor: setup stops on the old-directory case itself.
        cli::Command::Setup { shell } => {
            let cwd = std::env::current_dir()?;
            commands::setup::run(&cwd, shell.as_deref())
        }
        cli::Command::Profile { action } => match action {
            cli::ProfileAction::Add {
                id,
                provider,
                harness,
                config_dir,
                label,
                json,
            } => {
                let paths = config::Paths::from_env()?;
                commands::profile::add(
                    &paths,
                    commands::profile::AddArgs {
                        id: &id,
                        provider: &provider,
                        harness: &harness,
                        config_dir: &config_dir,
                        label: label.as_deref(),
                        json,
                    },
                )
            }
            cli::ProfileAction::List { json } => {
                let paths = config::Paths::from_env()?;
                commands::profile::list(&paths, json)
            }
            cli::ProfileAction::Check { id, json } => {
                let paths = config::Paths::from_env()?;
                commands::profile::check(&paths, &id, json)
            }
            cli::ProfileAction::Login {
                id,
                harness,
                provider,
                label,
                json,
            } => {
                let paths = config::Paths::from_env()?;
                commands::profile_login::run(
                    &paths,
                    commands::profile_login::LoginArgs {
                        id: id.as_deref(),
                        harness: harness.as_deref(),
                        provider: provider.as_deref(),
                        label: label.as_deref(),
                        json,
                    },
                )
            }
            cli::ProfileAction::SetConfigDir { id, dir, json } => {
                let paths = config::Paths::from_env()?;
                commands::profile::set_config_dir(&paths, &id, &dir, json)
            }
            cli::ProfileAction::ForgetModel { id, model, json } => {
                let paths = config::Paths::from_env()?;
                commands::profile::forget_model(&paths, &id, &model, json)
            }
            cli::ProfileAction::SetShared { id, value, json } => {
                let paths = config::Paths::from_env()?;
                commands::profile::set_shared(&paths, &id, &value, json)
            }
            cli::ProfileAction::Usage { id, refresh, json } => {
                let paths = config::Paths::from_env()?;
                commands::usage::profile_usage(&paths, id.as_deref(), refresh, json)
            }
        },
        cli::Command::Recommend { role, task, model, tables, json } => {
            let paths = config::Paths::from_env()?;
            // RJ-2: recommend reads the score caches, so like `report` it may
            // refresh them once a day.
            commands::models::with_auto_refresh(|| {
                commands::usage::recommend(&paths, &role, task.as_deref(), model.as_deref(), tables, json)
            })
        }
        cli::Command::Models { refresh, json } => {
            let paths = config::Paths::from_env()?;
            commands::models::run(&paths, refresh, json)
        }
        cli::Command::Report { out, open, section, json } => {
            let paths = config::Paths::from_env()?;
            commands::models::with_auto_refresh(|| {
                commands::report::run(&paths, out.as_deref(), open, section.as_deref(), json)
            })
        }
        cli::Command::Retro { days, json } => {
            let paths = config::Paths::from_env()?;
            commands::retro::run(&paths, days, json)
        }
        cli::Command::Task { action } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match action {
                cli::TaskAction::Create { file, title, base, json } => refreshing(
                    &paths,
                    commands::task::create(&paths, &cwd, &file, title.as_deref(), base.as_deref(), json),
                ),
                cli::TaskAction::Show { task, json } => {
                    commands::task::show(&paths, &cwd, &task, json)
                }
                cli::TaskAction::UpdateSpec { task, file, json } => refreshing(
                    &paths,
                    commands::task::update_spec(&paths, &cwd, &task, &file, json),
                ),
                cli::TaskAction::Close { task, reason, json } => refreshing(
                    &paths,
                    commands::task::close(&paths, &cwd, &task, &reason, json),
                ),
                cli::TaskAction::Accept { task, reason, json } => refreshing(
                    &paths,
                    commands::task::accept(&paths, &cwd, &task, &reason, json),
                ),
                cli::TaskAction::Result { task, run, json } => {
                    commands::task::result(&paths, &cwd, &task, run.as_deref(), json)
                }
            }
        }
        cli::Command::RunWorker { run, supervised } => {
            // Linux: a runner outside its own systemd unit re-runs itself
            // inside one and exits with that run's code (see
            // `process::ensure_supervised`). Windows: always `None`.
            let args = ["run-worker".to_string(), run.clone()];
            match process::ensure_supervised(&args, supervised)? {
                Some(code) => Ok(code),
                None => runner::run_worker(&run),
            }
        }
        // No `Paths::from_env()`, no database, no output: see the module doc.
        // `--agy-pre` prints its one decision line and nothing else.
        cli::Command::Hook { agy_pre: true } => Ok(adapter::antigravity::hook::run_agy_pre()),
        cli::Command::Hook { .. } => Ok(commands::hook::run()),
        cli::Command::Dispatch {
            task,
            profile,
            model,
            timeout,
            allowed_tools,
            allow_attempt,
            effort,
            reason,
            skip_spec_review,
            json,
        } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            let result = commands::dispatch::run(
                &paths,
                &cwd,
                commands::dispatch::DispatchArgs {
                    task: &task,
                    profile: &profile,
                    model: model.as_deref(),
                    timeout_seconds: timeout,
                    allowed_tools: &allowed_tools,
                    allow_attempt,
                    effort: effort.as_deref(),
                    reason: reason.as_deref(),
                    skip_spec_review: skip_spec_review.as_deref(),
                    json,
                },
            );
            refreshing(&paths, result)
        }
        cli::Command::Status { task, json } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            commands::status::run(&paths, &cwd, task.as_deref(), json)
        }
        cli::Command::Events { since, task, json } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            commands::events::run(&paths, &cwd, since, task.as_deref(), json)
        }
        cli::Command::Wait {
            task,
            run,
            timeout,
            json,
        } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match (task, run) {
                // clap's `required_unless_present` / `conflicts_with` pair makes
                // exactly one of these reachable; the fallthrough is for the
                // reader, not for clap.
                (_, Some(run)) => commands::wait::for_run(&paths, &cwd, &run, timeout, json),
                (Some(task), None) => commands::wait::run(&paths, &cwd, &task, timeout, json),
                (None, None) => anyhow::bail!("wait needs either a task id or --run <run_id>"),
            }
        }
        cli::Command::Cancel { run, json } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            refreshing(&paths, commands::cancel::run(&paths, &cwd, &run, json))
        }
        cli::Command::Resume { run, json } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            refreshing(&paths, commands::resume::run(&paths, &cwd, &run, json))
        }
        cli::Command::Submit { file, run, json } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            refreshing(&paths, commands::submit::run(&paths, &cwd, &file, &run, json))
        }
        cli::Command::Evidence { action } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match action {
                cli::EvidenceAction::List { task, json } => {
                    commands::evidence::list(&paths, &cwd, &task, json)
                }
                cli::EvidenceAction::Show {
                    code,
                    task,
                    bytes,
                    json,
                } => commands::evidence::show(
                    &paths,
                    &cwd,
                    &code,
                    task.as_deref(),
                    bytes,
                    json,
                ),
            }
        }
        cli::Command::Transcript { run, offset, json } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            commands::transcript::run(&paths, &cwd, &run, offset, json)
        }
        cli::Command::Review { action } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match action {
                cli::ReviewAction::Start {
                    task,
                    profile,
                    model,
                    timeout,
                    effort,
                    reason,
                    edit,
                    json,
                } => {
                    let result = commands::review::start(
                        &paths,
                        &cwd,
                        commands::review::StartArgs {
                            task: &task,
                            profile: &profile,
                            model: model.as_deref(),
                            timeout_seconds: timeout,
                            effort: effort.as_deref(),
                            reason: reason.as_deref(),
                            edit,
                            json,
                        },
                    );
                    refreshing(&paths, result)
                }
                cli::ReviewAction::Confirm {
                    task,
                    profile,
                    model,
                    timeout,
                    effort,
                    reason,
                    json,
                } => {
                    let result = commands::review::confirm(
                        &paths,
                        &cwd,
                        commands::review::ConfirmArgs {
                            task: &task,
                            profile: &profile,
                            model: &model,
                            timeout_seconds: timeout,
                            effort: effort.as_deref(),
                            reason: reason.as_deref(),
                            json,
                        },
                    );
                    refreshing(&paths, result)
                }
                cli::ReviewAction::Show { task, full, json } => {
                    commands::review::show(&paths, &cwd, &task, full, json)
                }
                cli::ReviewAction::Record { task, file, json } => {
                    let result = commands::review::record(
                        &paths,
                        &cwd,
                        commands::review::RecordArgs {
                            task: &task,
                            file: &file,
                            json,
                        },
                    );
                    refreshing(&paths, result)
                }
                cli::ReviewAction::Packet { task, out, json } => {
                    commands::review_packet::run(&paths, &cwd, &task, out.as_deref(), json)
                }
            }
        }
        cli::Command::Spec { action } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match action {
                cli::SpecAction::Review { action } => match action {
                    cli::SpecReviewAction::Start {
                        task,
                        profile,
                        model,
                        timeout,
                        effort,
                        reason,
                        edit,
                        json,
                    } => {
                        let result = commands::spec_review::start(
                            &paths,
                            &cwd,
                            commands::spec_review::StartArgs {
                                task: &task,
                                profile: &profile,
                                model: model.as_deref(),
                                timeout_seconds: timeout,
                                effort: effort.as_deref(),
                                reason: reason.as_deref(),
                                edit,
                                json,
                            },
                        );
                        refreshing(&paths, result)
                    }
                    cli::SpecReviewAction::Confirm {
                        task,
                        profile,
                        model,
                        timeout,
                        effort,
                        reason,
                        json,
                    } => {
                        let result = commands::spec_review::confirm(
                            &paths,
                            &cwd,
                            commands::spec_review::ConfirmArgs {
                                task: &task,
                                profile: &profile,
                                model: model.as_deref(),
                                timeout_seconds: timeout,
                                effort: effort.as_deref(),
                                reason: reason.as_deref(),
                                json,
                            },
                        );
                        refreshing(&paths, result)
                    }
                    cli::SpecReviewAction::Record { task, file, json } => {
                        let result = commands::spec_review::record(
                            &paths,
                            &cwd,
                            commands::spec_review::RecordArgs {
                                task: &task,
                                file: &file,
                                json,
                            },
                        );
                        refreshing(&paths, result)
                    }
                    cli::SpecReviewAction::Show { task, proposed_out, json } => {
                        commands::spec_review::show(&paths, &cwd, &task, proposed_out.as_deref(), json)
                    }
                },
                cli::SpecAction::Lint { file, json } => {
                    commands::spec_lint::run(&paths, &cwd, &file, json)
                }
            }
        }
        cli::Command::Verify {
            run,
            dry_run,
            timeout,
            at_base,
            json,
        } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            commands::verify::run(&paths, &cwd, &run, dry_run, timeout, at_base, json)
        }
        cli::Command::Decision { action } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match action {
                cli::DecisionAction::Add { text, file, json } => {
                    commands::decision::add(&paths, &cwd, &text, file.as_deref(), json)
                }
            }
        }
        cli::Command::Checks { action } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match action {
                cli::ChecksAction::Add {
                    id,
                    cmd,
                    shell,
                    cwd: check_cwd,
                    exe_pin,
                    input,
                    json,
                } => commands::checks::add(
                    &paths,
                    &cwd,
                    commands::checks::AddArgs {
                        id: &id,
                        cmd: &cmd,
                        shell: &shell,
                        cwd: &check_cwd,
                        inputs: &input,
                        exe_pin: &exe_pin,
                        json,
                    },
                ),
                cli::ChecksAction::Approve { run, reason, json } => {
                    commands::checks::approve(&paths, &cwd, run.as_deref(), reason.as_deref(), json)
                }
                cli::ChecksAction::List { json } => commands::checks::list(&paths, &cwd, json),
                cli::ChecksAction::Remove { id, json } => {
                    commands::checks::remove(&paths, &cwd, &id, json)
                }
                cli::ChecksAction::Protect { pattern, json } => {
                    commands::checks::protect(&paths, &cwd, &pattern, json)
                }
                cli::ChecksAction::Unprotect { pattern, json } => {
                    commands::checks::unprotect(&paths, &cwd, &pattern, json)
                }
            }
        }
        cli::Command::Worktrees { action } => {
            let paths = config::Paths::from_env()?;
            let cwd = std::env::current_dir()?;
            match action {
                cli::WorktreesAction::Prune { json } => commands::worktrees::prune(&paths, &cwd, json),
            }
        }
    }
}
