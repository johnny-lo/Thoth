/** One open task, as `thoth status --json` lists it under `tasks[]`. */
export type ThothTask = {
  task_id: string
  title: string
  status: string
  risk: string
  /** How many runs the task has had. */
  runs: number
  any_heartbeat_stale: boolean
  updated_at: string
  /** The root of the task's repository; empty when thoth did not say. */
  repo: string
}

/** One quota window of a profile, as `thoth profile usage --json` reports it. */
export type ThothWindow = {
  /** `five_hour`, `seven_day`, or another name the harness used. */
  name: string
  /** 0-100. */
  used_percent: number
  /** RFC 3339 UTC, or null when unknown. */
  resets_at: string | null
}

/** One registered profile, as `thoth profile usage --json` lists it under `profiles[]`. */
export type ThothProfile = {
  profile_id: string
  harness: string
  /** `ok`, `unknown` or `exhausted`. */
  quota_state: string
  windows: ThothWindow[]
  /** The bracket text thoth worked out: the tier, else the profile's --label; null when neither. */
  plan_label?: string | null
}

/** Why the panel cannot show tasks: no repo registration, no executable, or another failure. */
export type ThothProblem = 'not_initialised' | 'not_found' | 'failed'

/** One entry of `thoth events --since N --json`'s `events[]`. */
export type ThothEvent = {
  seq: number
  type: string
  detail: Record<string, unknown>
}

/** One check of `thoth checks list --json` that waits for the commander's approval. */
export type ThothCheck = {
  check_id: string
  status: 'pending' | 'stale'
}

/** The decision table's one-line summary, from `thoth models --json`'s `decision_table.summary`. */
export type ThothTable = {
  runs: number
  machines: number
  choicesRecent: number
  choicesRecentWithoutReason: number
  recentDays: number
}

/** What one refresh read from thoth for the current repository. */
export type ThothSnapshot = {
  tasks: ThothTask[]
  /** True when the session is not in a git repository: `tasks` are every registered repository's. */
  outsideRepo: boolean
  /** The checks waiting for approval; empty when none, when `checks list` failed, or outside a repository. */
  checks: ThothCheck[]
  /** The decision table's one-line summary; null when `thoth models --json` failed or had none. */
  table: ThothTable | null
  profiles: ThothProfile[]
  /** True when `thoth profile usage --json` failed; `profiles` is then empty. */
  usageFailed: boolean
  /** When the refresh finished, ms since the epoch. */
  fetchedAt: number
  problem: ThothProblem | null
  /** The first line of thoth's stderr when `problem` is `failed`, else empty. */
  problemText: string
}

/** One thing a worker did, as `thoth transcript RUN --json` lists it under `steps[]`. */
export type ThothStep = {
  /** The line's first byte in the stream file. */
  at: number
  kind: 'start' | 'message' | 'tool' | 'error' | 'note' | 'result'
  nested: boolean
  text: string
  tool: string | null
  action: 'read' | 'write' | 'edit' | 'run' | 'search' | 'other' | null
  target: string | null
  exit_code: number | null
  is_error: boolean | null
}

/** The worker of the task being watched: its newest run, and the steps read so far. */
export type ThothWorker = {
  task_id: string
  run_id: string
  role: string
  profile: string
  model: string
  status: string
  heartbeat_stale: boolean
  worktree: string
  harness: string | null
  exists: boolean
  /** Where the next `thoth transcript` read starts. */
  offset: number
  steps: ThothStep[]
  /** How many earlier steps were dropped to keep the list short. */
  dropped: number
  problem: 'no_run' | 'status_failed' | 'transcript_failed' | null
  fetchedAt: number
}

declare module 'claude-code' {
  interface PluginState {
    thoth: {
      snapshot: ThothSnapshot | null
      project: string
      cursor: number | null
      titles: Record<string, string>
      /** `repo` is where the worker's commands run; empty: the project. */
      watching: { task_id: string; title: string; repo: string } | null
      worker: ThothWorker | null
    }
  }
}
