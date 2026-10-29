import type {
  ThothCheck,
  ThothConfirm,
  ThothEvent,
  ThothLiveRun,
  ThothLogin,
  ThothProblem,
  ThothProfile,
  ThothSnapshot,
  ThothStep,
  ThothTable,
  ThothTask,
  ThothWindow,
} from '../types'
import { hasKey, t } from './i18n'
import type { Lang } from './i18n'

/** Statuses that wait on the commander. */
export const WAITING_ON_COMMANDER = ['in_review', 'unverified', 'interrupted', 'failed', 'reject']
/** Statuses with a worker queued or at work. */
export const IN_PROGRESS = ['queued', 'running']
/** Statuses of the commander's tasks the panel can close or dispatch again. */
export const CLOSABLE = ['failed', 'interrupted', 'unverified', 'reject']
/** Task statuses a `status_change` event is announced for. */
export const TOAST_STATUSES = ['in_review', 'pass', 'reject', 'unverified', 'failed', 'interrupted']

export const FAST_MS = 10_000
export const IDLE_MS = 60_000

/** What `$.process.run` answered, or `null` when it rejected (no executable, timed out). */
export type Ran = { exitCode: number; stdout: string; stderr: string; isStdoutTruncated: boolean } | null

export function cursorKey(project: string): string {
  return `events_cursor:${project}`
}

/** The git common dir as the repository's root: forward slashes, no trailing `/.git`. */
export function projectRoot(gitCommonDir: string): string {
  return gitCommonDir.trim().replace(/\\/g, '/').replace(/\/\.git\/?$/, '')
}

/** The last part of a path; `/` and `\` both separate. */
export function repoName(project: string): string {
  return project.split(/[\\/]/).filter(Boolean).pop() ?? ''
}

/** Whether a run of the task is queued or running, whatever the task's status (a review, a spec review). */
export function hasLiveRun(task: ThothTask): boolean {
  return task.live_run !== null && task.live_run !== undefined
}

/** A task with a live run is in progress; only a waiting status without one is the commander's. */
export function groupTasks(tasks: readonly ThothTask[]): { waitingOnCommander: ThothTask[]; inProgress: ThothTask[] } {
  return {
    waitingOnCommander: tasks.filter(task => WAITING_ON_COMMANDER.includes(task.status) && !hasLiveRun(task)),
    inProgress: tasks.filter(task => IN_PROGRESS.includes(task.status) || hasLiveRun(task)),
  }
}

export function parseJson(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return undefined
  }
}

function firstLine(text: string): string {
  return text.split(/\r?\n/).map(line => line.trim()).find(Boolean) ?? ''
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/** Why `thoth status --json` gave no task list, or null when it did. */
export function statusProblem(ran: Ran): { problem: ThothProblem; text: string } | null {
  if (ran === null) return { problem: 'not_found', text: '' }
  if (ran.exitCode !== 0) {
    if (`${ran.stdout}${ran.stderr}`.includes('is not initialised')) return { problem: 'not_initialised', text: '' }
    return { problem: 'failed', text: firstLine(ran.stderr) }
  }
  const parsed = parseJson(ran.stdout)
  if (!isObject(parsed) || !Array.isArray(parsed.tasks)) return { problem: 'failed', text: firstLine(ran.stderr) }
  return null
}

function asLiveRun(value: unknown): ThothLiveRun | null {
  if (!isObject(value) || typeof value.run_id !== 'string' || !value.run_id) return null
  const text = (field: unknown) => (typeof field === 'string' ? field : '')
  return {
    run_id: value.run_id,
    role: text(value.role),
    profile: text(value.profile),
    model: text(value.model),
    status: text(value.status),
  }
}

function asTask(value: unknown): ThothTask | null {
  if (!isObject(value) || typeof value.task_id !== 'string') return null
  return {
    task_id: value.task_id,
    title: typeof value.title === 'string' ? value.title : '',
    status: typeof value.status === 'string' ? value.status : '',
    risk: typeof value.risk === 'string' ? value.risk : '',
    runs: typeof value.runs === 'number' ? value.runs : 0,
    any_heartbeat_stale: value.any_heartbeat_stale === true,
    updated_at: typeof value.updated_at === 'string' ? value.updated_at : '',
    repo: typeof value.repo === 'string' ? value.repo : '',
    live_run: asLiveRun(value.live_run),
  }
}

export function parseTasks(stdout: string): ThothTask[] {
  const parsed = parseJson(stdout)
  if (!isObject(parsed) || !Array.isArray(parsed.tasks)) return []
  return parsed.tasks.map(asTask).filter((task): task is ThothTask => task !== null)
}

/** Whether `thoth status --json` ran outside a git repository and listed every registered one. */
export function parseOutsideRepo(stdout: string): boolean {
  const parsed = parseJson(stdout)
  return isObject(parsed) && parsed.outside_repo === true
}

function asWindow(value: unknown): ThothWindow | null {
  if (!isObject(value) || typeof value.name !== 'string' || typeof value.used_percent !== 'number') return null
  return {
    name: value.name,
    used_percent: value.used_percent,
    resets_at: typeof value.resets_at === 'string' ? value.resets_at : null,
  }
}

/** The profiles of `thoth profile usage --json`, or null when it failed. */
export function parseProfiles(ran: Ran): ThothProfile[] | null {
  if (ran === null || ran.exitCode !== 0) return null
  const parsed = parseJson(ran.stdout)
  if (!isObject(parsed) || !Array.isArray(parsed.profiles)) return null
  return parsed.profiles.flatMap(value => {
    if (!isObject(value) || typeof value.profile_id !== 'string') return []
    const windows = Array.isArray(value.windows) ? value.windows : []
    return [{
      profile_id: value.profile_id,
      harness: typeof value.harness === 'string' ? value.harness : '',
      quota_state: typeof value.quota_state === 'string' ? value.quota_state : 'unknown',
      windows: windows.map(asWindow).filter((w): w is ThothWindow => w !== null),
      plan_label: typeof value.plan_label === 'string' && value.plan_label.trim() ? value.plan_label.trim() : null,
    }]
  })
}

/**
 * The account as a person reads it: `id (login, Label)`, `id（login，Label）` in Traditional Chinese; either
 * part is left out when there is none, and the id stands alone without both.
 */
export function named(id: string, label: string | null | undefined, lang: Lang, login?: string | null): string {
  const parts = [login, label].filter((part): part is string => typeof part === 'string' && part !== '')
  if (parts.length === 0) return id
  return lang === 'zh-TW' ? `${id}（${parts.join('，')}）` : `${id} (${parts.join(', ')})`
}

/** The login names of `thoth profile list --json`, or null when it failed. */
export function parseLogins(ran: Ran): ThothLogin[] | null {
  if (ran === null || ran.exitCode !== 0) return null
  const parsed = parseJson(ran.stdout)
  if (!isObject(parsed) || !Array.isArray(parsed.profiles)) return null
  return parsed.profiles.flatMap(value =>
    isObject(value) && typeof value.profile_id === 'string' && typeof value.login_name === 'string' && value.login_name.trim()
      ? [{ profile_id: value.profile_id, login_name: value.login_name.trim() }]
      : [],
  )
}

/** Each profile's login name by profile_id; a Map, so an id like `constructor` finds nothing. */
export function loginNames(logins: readonly ThothLogin[] | undefined): ReadonlyMap<string, string> {
  const names = new Map<string, string>()
  for (const login of logins ?? []) names.set(login.profile_id, login.login_name)
  return names
}

/** How the snapshot names an account: its login name and label, as `named` writes them. */
export function accountOf(snapshot: ThothSnapshot | null, lang: Lang): (id: string) => string {
  const labels = snapshot === null ? new Map<string, string>() : planLabels(snapshot.profiles)
  const logins = snapshot === null ? new Map<string, string>() : loginNames(snapshot.logins)
  return id => named(id, labels.get(id), lang, logins.get(id))
}

/** Each profile's non-blank plan_label by profile_id; a Map, so an id like `constructor` finds nothing. */
export function planLabels(profiles: readonly ThothProfile[]): ReadonlyMap<string, string> {
  const labels = new Map<string, string>()
  for (const profile of profiles) {
    const label: unknown = profile.plan_label
    if (typeof label === 'string' && label.trim()) labels.set(profile.profile_id, label.trim())
  }
  return labels
}

/** The checks of `thoth checks list --json` that wait for approval, or null when it failed. */
export function parseChecks(ran: Ran): ThothCheck[] | null {
  if (ran === null || ran.exitCode !== 0) return null
  const parsed = parseJson(ran.stdout)
  if (!isObject(parsed) || !Array.isArray(parsed.checks)) return null
  return parsed.checks.flatMap(value =>
    isObject(value) && typeof value.check_id === 'string' && (value.status === 'pending' || value.status === 'stale')
      ? [{ check_id: value.check_id, status: value.status }]
      : [],
  )
}

/** The decision table's summary of `thoth models --json`, or null when it failed or had none. */
export function parseTable(ran: Ran): ThothTable | null {
  if (ran === null || ran.exitCode !== 0) return null
  const parsed = parseJson(ran.stdout)
  if (!isObject(parsed) || !isObject(parsed.decision_table)) return null
  const summary = parsed.decision_table.summary
  if (!isObject(summary)) return null
  const { runs, machines, choices_recent, choices_recent_without_reason } = summary
  if (
    typeof runs !== 'number' ||
    typeof machines !== 'number' ||
    typeof choices_recent !== 'number' ||
    typeof choices_recent_without_reason !== 'number'
  )
    return null
  return {
    runs,
    machines,
    choicesRecent: choices_recent,
    choicesRecentWithoutReason: choices_recent_without_reason,
    recentDays: typeof summary.recent_days === 'number' ? summary.recent_days : 7,
  }
}

/** The panel's one line about the decision table. */
export function tableLine(table: ThothTable | null, lang: Lang): string {
  if (table === null) return t(lang, 'tableUnavailable')
  return t(lang, 'tableLine', {
    runs: table.runs,
    machines: table.machines,
    n: table.choicesRecent,
    m: table.choicesRecentWithoutReason,
    days: table.recentDays,
  })
}

/** The text of one waiting check's row. */
export function checkLine(check: ThothCheck, lang: Lang): string {
  return `● ${check.check_id} ${t(lang, check.status === 'stale' ? 'checkStale' : 'checkPending')}`
}

/** How many rows the "needs you" section has: one for the decision cards. */
export function needsYouCount(cards: number): number {
  return cards > 0 ? 1 : 0
}

/** How many rows the commander's section has: each task waiting on it, and each check it has to approve. */
export function commanderCount(snapshot: ThothSnapshot): number {
  return groupTasks(snapshot.tasks).waitingOnCommander.length + snapshot.checks.length
}

/** The largest `"seq":N` in the text, for output too long or broken to parse; 0 when none. */
export function maxSeq(text: string): number {
  let max = 0
  for (const match of text.matchAll(/"seq"\s*:\s*(\d+)/g)) max = Math.max(max, Number(match[1]))
  return max
}

/** The events and the cursor to store after them, from `thoth events --since N --json`. */
export function parseEvents(ran: Ran): { events: ThothEvent[]; lastSeq: number } | null {
  if (ran === null || ran.exitCode !== 0) return null
  const parsed = ran.isStdoutTruncated ? undefined : parseJson(ran.stdout)
  if (!isObject(parsed) || typeof parsed.last_seq !== 'number') {
    const seq = maxSeq(ran.stdout)
    return seq > 0 ? { events: [], lastSeq: seq } : null
  }
  const events = Array.isArray(parsed.events) ? parsed.events : []
  return {
    lastSeq: parsed.last_seq,
    events: events.flatMap(value =>
      isObject(value) && typeof value.seq === 'number' && typeof value.type === 'string'
        ? [{ seq: value.seq, type: value.type, detail: isObject(value.detail) ? value.detail : {} }]
        : [],
    ),
  }
}

/** The toast for one event, or null when it is not announced. */
export function toastFor(
  event: ThothEvent,
  tasks: readonly ThothTask[],
  known: Readonly<Record<string, string>>,
  lang: Lang,
  labels: ReadonlyMap<string, string> = new Map(),
  logins: ReadonlyMap<string, string> = new Map(),
): string | null {
  const detail = event.detail
  if (event.type === 'status_change' && detail.entity === 'task') {
    const to = String(detail.to)
    if (!TOAST_STATUSES.includes(to)) return null
    const id = String(detail.id ?? '')
    const listed = tasks.find(task => task.task_id === id)?.title.trim()
    const remembered = Object.hasOwn(known, id) ? known[id]!.trim() : ''
    const key = `toast.${to}`
    return hasKey(key) ? t(lang, key, { name: listed || remembered || t(lang, 'aTask') }) : null
  }
  if (event.type === 'quota_exceeded') {
    const id = String(detail.profile ?? '')
    return t(lang, 'toast.quota_exceeded', { profile: named(id, labels.get(id), lang, logins.get(id)) })
  }
  return null
}

/** The toasts for the events newer than `cursor`, one per announced event, in order. */
export function toastsFor(
  events: readonly ThothEvent[],
  cursor: number,
  tasks: readonly ThothTask[],
  known: Readonly<Record<string, string>>,
  lang: Lang,
  labels: ReadonlyMap<string, string> = new Map(),
  logins: ReadonlyMap<string, string> = new Map(),
): string[] {
  return events
    .filter(event => event.seq > cursor)
    .map(event => toastFor(event, tasks, known, lang, labels, logins))
    .filter((text): text is string => text !== null)
}

export function statusLabel(status: string, lang: Lang): string {
  const key = `status.${status}`
  return hasKey(key) ? t(lang, key) : status
}

export function windowLabel(name: string, lang: Lang): string {
  const key = `window.${name}`
  return hasKey(key) ? t(lang, key) : name
}

export function percentLeft(window: ThothWindow): number {
  return Math.round(100 - window.used_percent)
}

/** `HH:MM` in the local time zone, or empty when the time is unknown or unreadable. */
export function localTime(iso: string | null): string {
  if (!iso) return ''
  const at = new Date(iso)
  if (Number.isNaN(at.getTime())) return ''
  const two = (n: number) => String(n).padStart(2, '0')
  return `${two(at.getHours())}:${two(at.getMinutes())}`
}

/** One profile's quota line: each window's share left and reset time, joined by ` · `. */
export function quotaText(profile: ThothProfile, lang: Lang, logins: ReadonlyMap<string, string> = new Map()): string {
  const parts = profile.windows.map(window => {
    const left = t(lang, 'left', { label: windowLabel(window.name, lang), n: percentLeft(window) })
    const time = localTime(window.resets_at)
    return time ? `${left}  ${t(lang, 'resets', { time })}` : left
  })
  const body = parts.length > 0 ? parts.join(' · ') : t(lang, 'noReading')
  const tail = profile.quota_state === 'exhausted' ? `  ${t(lang, 'exhausted')}` : ''
  return `${named(profile.profile_id, profile.plan_label, lang, logins.get(profile.profile_id))}  ${body}${tail}`
}

/** What a person calls the task: its title, or a plain word when it has none. */
export function taskName(title: string, lang: Lang): string {
  return title.trim() || t(lang, 'aTask')
}

/** The titles known so far, with the non-blank titles of `tasks` laid over them. */
export function rememberTitles(known: Readonly<Record<string, string>>, tasks: readonly ThothTask[]): Record<string, string> {
  const next: Record<string, string> = { ...known }
  for (const task of tasks) if (task.title.trim()) next[task.task_id] = task.title
  return next
}

/**
 * The task's line: its title and status, or, while a review or spec review runs, `under review` with the
 * reviewer's account (as `account` writes it) and model.
 */
export function taskLine(task: ThothTask, lang: Lang, account: (id: string) => string = id => id): string {
  const live = task.live_run
  const review = live && (live.role === 'reviewer' || live.role === 'spec_reviewer') ? live : null
  const state = review
    ? `${t(lang, review.role === 'reviewer' ? 'underReview' : 'specUnderReview')} · ${account(review.profile)} · ${review.model}`
    : statusLabel(task.status, lang)
  const parts = ['●', taskName(task.title, lang), state]
  if (task.any_heartbeat_stale) parts.push(t(lang, 'stale'))
  return parts.join(' ')
}

/** The task's line; outside a repository, the name of the task's repository after it. */
export function taskLineIn(
  task: ThothTask,
  outsideRepo: boolean,
  lang: Lang,
  account: (id: string) => string = id => id,
): string {
  const line = taskLine(task, lang, account)
  return outsideRepo ? `${line} · ${repoName(task.repo)}` : line
}

/** The window with the least left across every profile, or null with no readings. */
export function lowestWindow(profiles: readonly ThothProfile[]): { profile: string; window: ThothWindow } | null {
  let lowest: { profile: string; window: ThothWindow } | null = null
  for (const profile of profiles) {
    for (const window of profile.windows) {
      if (lowest === null || window.used_percent > lowest.window.used_percent) lowest = { profile: profile.profile_id, window }
    }
  }
  return lowest
}

/** The band's text, or null when the band should leave its place to the engine. */
export function bandText(snapshot: ThothSnapshot | null, lang: Lang, cards = 0): string | null {
  if (snapshot === null || snapshot.problem !== null) return null
  const { inProgress } = groupTasks(snapshot.tasks)
  const needsYou = needsYouCount(cards)
  const commander = commanderCount(snapshot)
  if (needsYou === 0 && commander === 0 && inProgress.length === 0) return null
  const parts = ['Thoth', t(lang, 'bandRunning', { n: inProgress.length })]
  if (commander > 0) parts.push(t(lang, 'bandCommander', { n: commander }))
  if (needsYou > 0) parts.push(t(lang, 'bandNeedsYou', { n: needsYou }))
  const lowest = lowestWindow(snapshot.profiles)
  if (lowest) {
    const label = windowLabel(lowest.window.name, lang)
    const name = accountOf(snapshot, lang)(lowest.profile)
    parts.push(`${name} ${t(lang, 'left', { label, n: percentLeft(lowest.window) })}`)
  }
  return parts.join(' · ')
}

/** The pane's single line when thoth could not be read, or null when it could. */
export function problemHint(snapshot: ThothSnapshot, lang: Lang): string | null {
  switch (snapshot.problem) {
    case 'not_initialised':
      return t(lang, 'hintNotInitialised')
    case 'not_found':
      return t(lang, 'hintNotFound')
    case 'failed':
      return t(lang, 'hintFailed', { text: snapshot.problemText })
    default:
      return null
  }
}

/** How long until the next refresh: soon while any task is in progress (a review counts). */
export function nextDelay(snapshot: ThothSnapshot | null): number {
  return snapshot !== null && groupTasks(snapshot.tasks).inProgress.length > 0 ? FAST_MS : IDLE_MS
}

/** Cells a character takes on a terminal: two for wide East Asian characters. */
function cellWidth(char: string): number {
  const code = char.codePointAt(0) ?? 0
  return (code >= 0x1100 && code <= 0x115f) ||
    (code >= 0x2e80 && code <= 0xa4cf) ||
    (code >= 0xac00 && code <= 0xd7a3) ||
    (code >= 0xf900 && code <= 0xfaff) ||
    (code >= 0xfe30 && code <= 0xfe4f) ||
    (code >= 0xff00 && code <= 0xff60) ||
    (code >= 0xffe0 && code <= 0xffe6)
    ? 2
    : 1
}

export function textWidth(text: string): number {
  let width = 0
  for (const char of text) width += cellWidth(char)
  return width
}

/** The text cut to `columns` cells, an ellipsis marking the cut. */
export function fit(text: string, columns: number): string {
  if (textWidth(text) <= columns) return text
  if (columns <= 0) return ''
  let out = ''
  let width = 0
  for (const char of text) {
    const w = cellWidth(char)
    if (width + w > columns - 1) break
    out += char
    width += w
  }
  return `${out}…`
}

/** How many steps of a worker the panel keeps; earlier ones are counted, not shown. */
export const STEPS_KEPT = 200

/** One run of `thoth status TASK --json`'s `runs[]`, as far as the run pane needs it. */
export type ThothRunRow = {
  run_id: string
  role: string
  profile: string
  model: string
  status: string
  heartbeat_stale: boolean
  worktree: string
  effort_requested: string | null
  interrupted_reason: string | null
  next_step: string | null
}

/** The runs of `thoth status TASK --json`, or null when it failed. */
export function parseRuns(ran: Ran): ThothRunRow[] | null {
  if (ran === null || ran.exitCode !== 0) return null
  const parsed = parseJson(ran.stdout)
  if (!isObject(parsed) || !Array.isArray(parsed.runs)) return null
  const text = (value: unknown) => (typeof value === 'string' ? value : '')
  const orNull = (value: unknown) => (typeof value === 'string' && value !== '' ? value : null)
  return parsed.runs.flatMap(value =>
    isObject(value) && typeof value.run_id === 'string'
      ? [{
          run_id: value.run_id,
          role: text(value.role),
          profile: text(value.profile),
          model: text(value.model),
          status: text(value.status),
          heartbeat_stale: value.heartbeat_stale === true,
          worktree: text(value.worktree),
          effort_requested: orNull(value.effort_requested),
          interrupted_reason: orNull(value.interrupted_reason),
          next_step: orNull(value.next_step),
        }]
      : [],
  )
}

/** The newest implementer run, the one Dispatch again repeats; null when the task has none. */
export function newestImplementerRun(runs: readonly ThothRunRow[]): ThothRunRow | null {
  return newestRun(runs.filter(run => run.role === 'implementer'))
}

/** What thoth said went wrong: `error` of the `--json` error line on stderr, else stderr's first line. */
export function errorText(ran: { stdout: string; stderr: string }): string {
  for (const line of ran.stderr.split(/\r?\n/)) {
    const parsed = parseJson(line.trim())
    if (isObject(parsed) && typeof parsed.error === 'string' && parsed.error) return parsed.error
  }
  return firstLine(ran.stderr) || firstLine(ran.stdout)
}

/**
 * Whether a waiting action still fits the task as the snapshot shows it: a Cancel while the same run is live,
 * a Close task or Dispatch again while the task waits on the commander in a status that allows it.
 */
export function confirmStillFits(confirm: ThothConfirm, snapshot: ThothSnapshot | null): boolean {
  if (snapshot === null || snapshot.problem !== null) return false
  const { waitingOnCommander, inProgress } = groupTasks(snapshot.tasks)
  if (confirm.kind === 'cancel') {
    const task = inProgress.find(task => task.task_id === confirm.task_id)
    return task?.live_run?.run_id === confirm.run_id
  }
  const task = waitingOnCommander.find(task => task.task_id === confirm.task_id)
  return task !== undefined && CLOSABLE.includes(task.status)
}

/** The sentence a waiting action asks to confirm. */
export function confirmText(confirm: ThothConfirm, lang: Lang, account: (id: string) => string = id => id): string {
  const title = taskName(confirm.title, lang)
  switch (confirm.kind) {
    case 'cancel':
      if (confirm.role === 'reviewer') {
        return t(lang, 'confirmCancelReview', { title, account: account(confirm.profile), model: confirm.model })
      }
      if (confirm.role === 'spec_reviewer') return t(lang, 'confirmCancelSpecReview', { title })
      return t(lang, 'confirmCancelRun', { title })
    case 'close':
      return t(lang, 'confirmClose', { title })
    case 'again':
      return t(lang, confirm.effort === null ? 'confirmAgainNoEffort' : 'confirmAgain', {
        title,
        account: account(confirm.profile),
        model: confirm.model,
        effort: confirm.effort ?? '',
      })
  }
}

/** The thoth arguments a confirmed action runs. */
export function actionArgs(confirm: ThothConfirm): string[] {
  switch (confirm.kind) {
    case 'cancel':
      return ['cancel', confirm.run_id, '--json']
    case 'close':
      return ['task', 'close', confirm.task_id, '--reason', 'closed from the Thoth panel', '--json']
    case 'again': {
      const effort = confirm.effort === null ? [] : ['--effort', confirm.effort]
      const reason = `dispatched again from the Thoth panel: same profile, model and effort as run ${confirm.run_id}`
      return ['dispatch', confirm.task_id, '--profile', confirm.profile, '--model', confirm.model, ...effort, '--reason', reason, '--json']
    }
  }
}

/** The newest run: ULIDs sort by time, so the largest id. */
export function newestRun(runs: readonly ThothRunRow[]): ThothRunRow | null {
  let newest: ThothRunRow | null = null
  for (const run of runs) if (newest === null || run.run_id > newest.run_id) newest = run
  return newest
}

const STEP_KINDS = ['start', 'message', 'tool', 'error', 'note', 'result']
const STEP_ACTIONS = ['read', 'write', 'edit', 'run', 'search', 'other']

function asStep(value: unknown): ThothStep | null {
  if (!isObject(value) || typeof value.kind !== 'string' || !STEP_KINDS.includes(value.kind)) return null
  const action = typeof value.action === 'string' && STEP_ACTIONS.includes(value.action) ? value.action : null
  return {
    at: typeof value.at === 'number' ? value.at : 0,
    kind: value.kind as ThothStep['kind'],
    nested: value.nested === true,
    text: typeof value.text === 'string' ? value.text : '',
    tool: typeof value.tool === 'string' ? value.tool : null,
    action: action as ThothStep['action'],
    target: typeof value.target === 'string' ? value.target : null,
    exit_code: typeof value.exit_code === 'number' ? value.exit_code : null,
    is_error: typeof value.is_error === 'boolean' ? value.is_error : null,
  }
}

/** What `thoth transcript RUN --offset N --json` returned, or null when it failed. */
export function parseTranscript(
  ran: Ran,
): { exists: boolean; harness: string | null; reset: boolean; nextOffset: number; steps: ThothStep[] } | null {
  if (ran === null || ran.exitCode !== 0) return null
  const parsed = parseJson(ran.stdout)
  if (!isObject(parsed) || !Array.isArray(parsed.steps)) return null
  return {
    exists: parsed.exists === true,
    harness: typeof parsed.harness === 'string' ? parsed.harness : null,
    reset: parsed.reset === true,
    nextOffset: typeof parsed.next_offset === 'number' ? parsed.next_offset : 0,
    steps: parsed.steps.map(asStep).filter((step): step is ThothStep => step !== null),
  }
}

/** `path` without the worktree's directory in front of it; either kind of slash counts. */
export function relativeTo(path: string, worktree: string): string {
  const slashed = path.replace(/\\/g, '/')
  const root = worktree.replace(/\\/g, '/').replace(/\/+$/, '')
  if (root !== '' && slashed.startsWith(`${root}/`)) return slashed.slice(root.length + 1)
  return path
}

/** The label of a run's role: the person's word for it, else thoth's own. */
export function roleLabel(role: string, lang: Lang): string {
  const key = `role.${role}`
  return hasKey(key) ? t(lang, key) : role
}

/** The text of one step's line; the first half of a template when `text` has nothing to say. */
export function stepLine(step: ThothStep, worktree: string, lang: Lang): string {
  const indent = step.nested ? '  ' : ''
  const withText = (key: 'finished' | 'endedWithError') => {
    if (step.text !== '') return t(lang, key, { text: step.text })
    return t(lang, key, { text: '\u0000' }).split('\u0000')[0]!.trimEnd()
  }
  switch (step.kind) {
    case 'start':
      return `${indent}▶ ${step.text === '' ? t(lang, 'startedPlain') : t(lang, 'started', { model: step.text })}`
    case 'message':
      return `${indent}│ ${step.text}`
    case 'note':
      return `${indent}· ${step.text}`
    case 'error':
      return `${indent}✗ ${step.text}`
    case 'result':
      return `${indent}■ ${withText(step.is_error === true ? 'endedWithError' : 'finished')}`
    default: {
      const raw = step.target ?? step.tool ?? step.text
      const action = step.action ?? 'other'
      let body: string
      if (action === 'other') {
        body = t(lang, 'step.other', { tool: step.tool ?? '' })
        if (step.target !== null) body += `: ${step.target}`
      } else {
        const shown = action === 'run' || action === 'search' ? raw : relativeTo(raw, worktree)
        body = t(lang, `step.${action}`, { target: shown })
      }
      const exit = step.exit_code !== null && step.exit_code !== 0 ? ` (exit ${step.exit_code})` : ''
      return `${indent}● ${body}${exit}`
    }
  }
}

/** The Text props of a step's line: dim for talk, red for errors, green for a clean finish. */
export function stepProps(step: ThothStep): Record<string, unknown> {
  switch (step.kind) {
    case 'message':
    case 'note':
      return { dimColor: true }
    case 'error':
      return { color: 'red' }
    case 'result':
      return { color: step.is_error === true ? 'red' : 'green' }
    case 'tool':
      return step.is_error === true || (step.exit_code !== null && step.exit_code !== 0) ? { color: 'red' } : {}
    default:
      return {}
  }
}
