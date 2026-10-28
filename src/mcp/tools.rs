use serde_json::{json, Map, Value};

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    String,
    Integer,
    Object,
    Role,
    Bool,
    StrArray,
}

#[derive(Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
}

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub params: &'static [Param],
    pub command: &'static [&'static str],
}

const S: Kind = Kind::String;
const I: Kind = Kind::Integer;
const O: Kind = Kind::Object;
const R: Kind = Kind::Role;
const B: Kind = Kind::Bool;
const A: Kind = Kind::StrArray;
const REPO: Param = Param {
    name: "repo",
    kind: S,
    required: false,
};

macro_rules! p {
    ($n:literal,$k:expr,$r:expr) => {
        Param {
            name: $n,
            kind: $k,
            required: $r,
        }
    };
}

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "thoth_init",
        description: "Register and initialize a repository.",
        read_only: false,
        params: &[REPO],
        command: &["init"],
    },
    Tool {
        name: "thoth_status",
        description: "Show repository or task status.",
        read_only: true,
        params: &[p!("task", S, false), REPO],
        command: &["status"],
    },
    Tool {
        name: "thoth_profile_usage",
        description: "Show recorded profile quota usage.",
        read_only: true,
        params: &[p!("id", S, false)],
        command: &["profile", "usage"],
    },
    Tool {
        name: "thoth_models",
        description: "List known models without refreshing external data.",
        read_only: true,
        params: &[],
        command: &["models"],
    },
    Tool {
        name: "thoth_recommend",
        description: "List every profile's quota facts for an implementer or reviewer run (eligible or not, and why) and the models it can run; the three external tables filtered to the organizations with an account here (Artificial Analysis indexes, Agent Arena ranks, AI Stupid Level's recent degradation); Thoth's own usage per account, model, effort and task type (average tokens and quota points, how many runs are left); and with task, the task's attempts and reject history. It does not choose a profile, model or effort for you.",
        read_only: true,
        params: &[p!("role", R, true), p!("task", S, false), REPO],
        command: &["recommend"],
    },
    Tool {
        name: "thoth_task_create",
        description: "Create a task from a structured specification. base is where the task starts (a commit or a branch); without it, the HEAD of the worktree at repo (or the server's default repo).",
        read_only: false,
        params: &[p!("title", S, true), p!("spec", O, true), p!("base", S, false), REPO],
        command: &["task", "create"],
    },
    Tool {
        name: "thoth_task_update_spec",
        description: "Replace a task specification.",
        read_only: false,
        params: &[p!("task", S, true), p!("spec", O, true), REPO],
        command: &["task", "update-spec"],
    },
    Tool {
        name: "thoth_task_close",
        description: "Close a task with a reason.",
        read_only: false,
        params: &[p!("task", S, true), p!("reason", S, true), REPO],
        command: &["task", "close"],
    },
    Tool {
        name: "thoth_dispatch",
        description: "Start an implementation run and immediately return its run id. Give model and effort on the first dispatch; a reroute in the same attempt keeps the previous run's. effort can be left out only for a model without an effort setting (haiku) or an agy model whose name ends in its level (gemini-3.8-flash-high: that level is recorded and no flag is sent); any other agy model takes --effort low|medium|high|xhigh|max.",
        read_only: false,
        params: &[
            p!("task", S, true),
            p!("profile", S, true),
            p!("model", S, false),
            p!("timeout", S, false),
            p!("allowed_tools", S, false),
            p!("allow_attempt", I, false),
            p!("effort", S, false),
            p!("reason", S, false),
            p!("skip_spec_review", S, false),
            REPO,
        ],
        command: &["dispatch"],
    },
    Tool {
        name: "thoth_wait",
        description: "Wait for a task or run for a bounded number of seconds.",
        read_only: true,
        params: &[
            p!("task", S, false),
            p!("run", S, false),
            p!("timeout_seconds", I, false),
            REPO,
        ],
        command: &["wait"],
    },
    Tool {
        name: "thoth_events",
        description: "List events after a sequence number.",
        read_only: true,
        params: &[p!("since", I, false), p!("task", S, false), REPO],
        command: &["events"],
    },
    Tool {
        name: "thoth_evidence_list",
        description: "List evidence captured for a task.",
        read_only: true,
        params: &[p!("task", S, true), REPO],
        command: &["evidence", "list"],
    },
    Tool {
        name: "thoth_evidence_show",
        description: "Read one evidence item with an optional byte limit.",
        read_only: false,
        params: &[
            p!("code", S, true),
            p!("task", S, true),
            p!("bytes", I, false),
            REPO,
        ],
        command: &["evidence", "show"],
    },
    Tool {
        name: "thoth_review_packet",
        description: "Build a Markdown review packet and return its path.",
        read_only: false,
        params: &[p!("task", S, true), REPO],
        command: &["review", "packet"],
    },
    Tool {
        name: "thoth_review_record",
        description: "Record a structured human or agent review.",
        read_only: false,
        params: &[p!("task", S, true), p!("review", O, true), REPO],
        command: &["review", "record"],
    },
    Tool {
        name: "thoth_review_start",
        description: "Start a review run and immediately return its run id. Give model and effort on the first review of an attempt; starting it again keeps the previous run's. effort can be left out only for a model without an effort setting (haiku) or an agy model whose name ends in its level (gemini-3.8-flash-high); any other agy model takes --effort low|medium|high|xhigh|max.",
        read_only: false,
        params: &[
            p!("task", S, true),
            p!("profile", S, true),
            p!("model", S, false),
            p!("timeout", S, false),
            p!("effort", S, false),
            p!("reason", S, false),
            REPO,
        ],
        command: &["review", "start"],
    },
    Tool {
        name: "thoth_review_show",
        description: "Show the recorded review for a task.",
        read_only: true,
        params: &[p!("task", S, true), REPO],
        command: &["review", "show"],
    },
    Tool {
        name: "thoth_spec_review_record",
        description: "Record a spec review made by a subagent or a human before the task is dispatched: APPROVE or REVISE of the task's current spec, each finding bound to an acceptance criterion with a file:line location. An important or high task needs an APPROVE of its current spec before its first dispatch.",
        read_only: false,
        params: &[p!("task", S, true), p!("review", O, true), REPO],
        command: &["spec", "review", "record"],
    },
    Tool {
        name: "thoth_spec_review_show",
        description: "Show the task's newest spec review and whether its current spec is approved for the first dispatch.",
        read_only: true,
        params: &[p!("task", S, true), REPO],
        command: &["spec", "review", "show"],
    },
    Tool {
        name: "thoth_spec_review_start",
        description: "Start a read-only spec reviewer run against the task's current spec, checked out at the HEAD of the worktree at repo when that contains the task's base (otherwise at the base), and immediately return its run id; the verdict lands as a spec_review event (source reviewer_run) that the dispatch gate reads. Use a profile of the provider that did not write the spec. Give model and effort every time (effort can be left out only for a model without an effort setting, haiku, or an agy model whose name ends in its level, gemini-3.8-flash-high; any other agy model takes --effort low|medium|high|xhigh|max). Block on it with thoth_wait (run), then read thoth_spec_review_show.",
        read_only: false,
        params: &[
            p!("task", S, true),
            p!("profile", S, true),
            p!("model", S, false),
            p!("timeout", S, false),
            p!("effort", S, false),
            p!("reason", S, false),
            p!("edit", B, false),
            REPO,
        ],
        command: &["spec", "review", "start"],
    },
    Tool {
        name: "thoth_spec_review_confirm",
        description: "Start the author's check of the edits the newest edit-mode spec review (thoth_spec_review_start with edit) proposed, and immediately return its run id: the author judges only those edits, ok or revise each; an APPROVE replaces the task's spec with the proposed one and lands as a spec_review APPROVE of the new spec, a REVISE only records. Needs thoth_spec_review_show's pending_edits true and a task that update-spec allows. Use the profile and model that wrote the spec. Give model and effort every time. Block on it with thoth_wait (run), then read thoth_spec_review_show.",
        read_only: false,
        params: &[
            p!("task", S, true),
            p!("profile", S, true),
            p!("model", S, false),
            p!("timeout", S, false),
            p!("effort", S, false),
            p!("reason", S, false),
            REPO,
        ],
        command: &["spec", "review", "confirm"],
    },
    Tool {
        name: "thoth_resume",
        description: "Resume a stopped run.",
        read_only: false,
        params: &[p!("run", S, true), REPO],
        command: &["resume"],
    },
    Tool {
        name: "thoth_cancel",
        description: "Cancel a run; cancellation is terminal for its task.",
        read_only: false,
        params: &[p!("run", S, true), REPO],
        command: &["cancel"],
    },
    Tool {
        name: "thoth_verify_start",
        description: "Start re-running this repository's approved checks on a run, in the background. It runs the approved checks, not the commands the worker declared. Returns a job id immediately; get the result with thoth_job_status.",
        read_only: false,
        params: &[
            p!("run", S, true),
            p!("at_base", B, false),
            p!("dry_run", B, false),
            p!("timeout", S, false),
            REPO,
        ],
        command: &["verify"],
    },
    Tool {
        name: "thoth_job_status",
        description: "Report a background job started by thoth_verify_start: working, completed, failed or unknown, with the verify result once it has finished.",
        read_only: true,
        params: &[p!("job_id", S, true), p!("wait_seconds", I, false)],
        command: &[],
    },
    Tool {
        name: "thoth_checks_list",
        description: "List the repository's checks with their status: approved, stale or pending.",
        read_only: true,
        params: &[REPO],
        command: &["checks", "list"],
    },
    Tool {
        name: "thoth_checks_add",
        description: "Propose a check, or replace one. The check stays pending until it is approved with thoth_checks_approve, or with `thoth checks approve` in a terminal.",
        read_only: false,
        params: &[
            p!("id", S, true),
            p!("cmd", S, true),
            p!("shell", S, false),
            p!("cwd", S, false),
            p!("exe_pin", S, false),
            p!("inputs", A, false),
            REPO,
        ],
        command: &["checks", "add"],
    },
    Tool {
        name: "thoth_checks_approve",
        description: "Approve every pending check and every check whose files changed, reading the main work tree, or with run the files of that settled run's worktree; with run it also approves the protected test files that run changed, which review start and a PASS review record wait for. reason is required and is recorded with the approval for the reviewer.",
        read_only: false,
        params: &[p!("reason", S, true), p!("run", S, false), REPO],
        command: &["checks", "approve"],
    },
    Tool {
        name: "thoth_checks_remove",
        description: "Delete a check from the repository's catalogue. Removal takes effect at once and needs no approval: later verify jobs no longer run it, and the next verify result names a removed approved check under missing_since_last_verify.",
        read_only: false,
        params: &[p!("id", S, true), REPO],
        command: &["checks", "remove"],
    },
    Tool {
        name: "thoth_config_list",
        description: "Read-only: every setting with its effective value and its source (an environment variable, config.json, or the default), what is kept elsewhere (profile and check counts), and whether the two API keys are set, never their values. Changing a setting is a human's step in a terminal (thoth config set, thoth config unset); this server cannot.",
        read_only: true,
        params: &[REPO],
        command: &["config", "list"],
    },
    // Before thoth_guide, which tests/guide.rs pins as the last tool.
    Tool {
        name: "thoth_doctor",
        description: "Read-only health check: state directory, the three CLIs, profile logins, this repository's init and approved checks, config.json, and the installed Claude Code plugin's version.",
        read_only: true,
        params: &[p!("shell", S, false), REPO],
        command: &["doctor"],
    },
    Tool {
        name: "thoth_spec_lint",
        description: "Read-only: check a spec (pass it as spec, or the absolute path of a spec file as file) with the fixed rules task create, task update-spec and spec review start apply first. An error makes those three refuse the spec; a warning does not.",
        read_only: true,
        params: &[p!("spec", O, false), p!("file", S, false), REPO],
        command: &["spec", "lint"],
    },
    Tool {
        name: "thoth_task_accept",
        description: "Take the delivery of an in_review docs, research or review task with a reason: the task becomes accepted, with no review recorded and nothing counted as PASS or REJECT.",
        read_only: false,
        params: &[p!("task", S, true), p!("reason", S, true), REPO],
        command: &["task", "accept"],
    },
    Tool {
        name: "thoth_task_result",
        description: "Read-only: the newest result.json a task's worker delivered (or that of run): its summary in full and its claims.",
        read_only: true,
        params: &[p!("task", S, true), p!("run", S, false), REPO],
        command: &["task", "result"],
    },
    Tool {
        name: "thoth_guide",
        description: "Return the commander guide: the full dispatch and review workflow.",
        read_only: true,
        params: &[],
        command: &["guide"],
    },
];

pub fn definition(t: &Tool) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for p in t.params {
        let schema = match p.kind {
            Kind::String => json!({"type":"string"}),
            Kind::Integer => json!({"type":"integer"}),
            Kind::Object => json!({"type":"object"}),
            Kind::Role => json!({"type":"string","enum":["implementer","reviewer"]}),
            Kind::Bool => json!({"type":"boolean"}),
            Kind::StrArray => json!({"type":"array","items":{"type":"string"}}),
        };
        properties.insert(p.name.to_string(), schema);
        if p.required {
            required.push(Value::String(p.name.to_string()));
        }
    }
    json!({"name":t.name,"description":t.description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":t.read_only}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn table_and_schema_agree_and_list_is_small() {
        let (mut booleans, mut arrays) = (0, 0);
        for t in TOOLS {
            let d = definition(t);
            let props = d["inputSchema"]["properties"].as_object().unwrap();
            for p in t.params {
                match p.kind {
                    Kind::Bool => {
                        assert_eq!(props[p.name], json!({"type":"boolean"}));
                        booleans += 1;
                    }
                    Kind::StrArray => {
                        assert_eq!(
                            props[p.name],
                            json!({"type":"array","items":{"type":"string"}})
                        );
                        arrays += 1;
                    }
                    Kind::String => assert_eq!(props[p.name], json!({"type":"string"})),
                    Kind::Integer => assert_eq!(props[p.name], json!({"type":"integer"})),
                    Kind::Object => assert_eq!(props[p.name], json!({"type":"object"})),
                    Kind::Role => assert_eq!(props[p.name]["type"], "string"),
                }
            }
            assert_eq!(
                props
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                t.params.iter().map(|p| p.name).collect()
            );
            assert_eq!(
                d["inputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect::<std::collections::BTreeSet<_>>(),
                t.params
                    .iter()
                    .filter(|p| p.required)
                    .map(|p| p.name)
                    .collect()
            );
        }
        // thoth_verify_start has two booleans and thoth_spec_review_start one
        // (SR-3 `edit`), thoth_checks_add one string array
        assert_eq!((booleans, arrays), (3, 1));
        assert!(
            serde_json::to_vec(&json!({"tools":TOOLS.iter().map(definition).collect::<Vec<_>>() }))
                .unwrap()
                .len()
                <= 20_000
        );
    }
}
