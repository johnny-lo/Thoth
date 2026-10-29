import { atom, read, update } from 'claude-code'
import type { EngineInterface, PluginOptions, Register } from 'claude-code'

import type { ThothCheck, ThothConfirm, ThothLogin, ThothSnapshot, ThothTable, ThothTask, ThothWorker } from '../types'
import { langOf, t } from './i18n'
import type { Lang } from './i18n'
import {
  accountOf,
  actionArgs,
  bandText,
  checkLine,
  CLOSABLE,
  commanderCount,
  confirmStillFits,
  confirmText,
  cursorKey,
  errorText,
  fit,
  groupTasks,
  loginNames,
  newestImplementerRun,
  newestRun,
  nextDelay,
  needsYouCount,
  parseChecks,
  parseEvents,
  parseLogins,
  parseOutsideRepo,
  parseProfiles,
  parseRuns,
  parseTable,
  parseTranscript,
  parseTasks,
  planLabels,
  problemHint,
  projectRoot,
  quotaText,
  rememberTitles,
  repoName,
  roleLabel,
  statusLabel,
  statusProblem,
  STEPS_KEPT,
  stepLine,
  stepProps,
  tableLine,
  taskLineIn,
  taskName,
  textWidth,
  toastsFor,
} from './model'
import type { Ran } from './model'

// Every value comes from the thoth CLI's --json output. The panel writes only after a second press:
// cancel a live run, close a task, or dispatch it again exactly as its last run.
const PANE = 'thoth'
const RUN_PANE = 'thoth-run'
const TITLE = 'Thoth'
const THOTH_TIMEOUT_MS = 20_000
const WRITE_TIMEOUT_MS = 60_000
const snapshot = atom({ plugin: 'thoth', key: 'snapshot' } as const, null)
const project = atom({ plugin: 'thoth', key: 'project' } as const, '')
// The task whose worker the run pane shows, and what was last read of that worker.
const watching = atom({ plugin: 'thoth', key: 'watching' } as const, null)
const worker = atom({ plugin: 'thoth', key: 'worker' } as const, null)
// The Cancel, Close task or Dispatch again waiting for its second press.
const confirming = atom({ plugin: 'thoth', key: 'confirming' } as const, null)

// A Text prop set that marks a line red, or leaves it as it is.
const red = (isRed: boolean) => (isRed ? { color: 'red' } : {})

type Options = { language: Lang; thothBin: string; thothHome: string }

function optionsOf(raw: PluginOptions): Options {
  const text = (value: unknown) => (typeof value === 'string' ? value.trim() : '')
  return { language: langOf(raw.language), thothBin: text(raw.thoth_bin), thothHome: text(raw.thoth_home) }
}

// Worktrees of one repo share thoth's registration: run in the git common dir's root, else the cwd.
async function projectOf($: EngineInterface): Promise<string> {
  const cwd = await $.session.cwd()
  try {
    const ran = await $.process.run(['git', 'rev-parse', '--path-format=absolute', '--git-common-dir'], { cwd })
    const root = projectRoot(ran.stdout)
    if (ran.exitCode === 0 && root) return root
  } catch {}
  return cwd.replace(/\\/g, '/')
}

/** Runs thoth in `cwd`, else the repository's root; null when it could not start or timed out. */
async function thoth($: EngineInterface, options: Options, args: string[], cwd = '', timeoutMs = THOTH_TIMEOUT_MS): Promise<Ran> {
  const env: Record<string, string> = options.thothHome ? { THOTH_HOME: options.thothHome } : {}
  try {
    return await $.process.run([options.thothBin || 'thoth', ...args], {
      cwd: cwd || (await read($, project)),
      env,
      timeoutMs,
    })
  } catch {
    return null
  }
}

// Per load: a reload drops the timer and starts the cycle over from session.start.
let timer: { cancel: () => void } | undefined
let isRefreshing = false
// The one pending redraw that moves the heading's seconds on.
let tick: { cancel: () => void } | undefined

// `checks list` hashes every check's program and inputs: run it at most once a minute, `/thoth` aside.
const CHECKS_EVERY_MS = 60_000
let checksAt: number | null = null
let checksKnown: ThothCheck[] = []

async function readChecks($: EngineInterface, options: Options, force: boolean): Promise<ThothCheck[]> {
  const now = await $.clock.now()
  if (!force && checksAt !== null && now - checksAt < CHECKS_EVERY_MS) return checksKnown
  checksAt = now
  checksKnown = parseChecks(await thoth($, options, ['checks', 'list', '--json'])) ?? []
  return checksKnown
}

// `models --json` opens every repository's database for the decision table: at most once a minute too.
const TABLE_EVERY_MS = 60_000
let tableAt: number | null = null
let tableKnown: ThothTable | null = null

async function readTable($: EngineInterface, options: Options, force: boolean): Promise<ThothTable | null> {
  const now = await $.clock.now()
  if (!force && tableAt !== null && now - tableAt < TABLE_EVERY_MS) return tableKnown
  tableAt = now
  tableKnown = parseTable(await thoth($, options, ['models', '--json']))
  return tableKnown
}

// `profile list --json` names each account's login: at most once a minute too; a failure is no names.
const LOGINS_EVERY_MS = 60_000
let loginsAt: number | null = null
let loginsKnown: ThothLogin[] = []

async function readLogins($: EngineInterface, options: Options, force: boolean): Promise<ThothLogin[]> {
  const now = await $.clock.now()
  if (!force && loginsAt !== null && now - loginsAt < LOGINS_EVERY_MS) return loginsKnown
  loginsAt = now
  loginsKnown = parseLogins(await thoth($, options, ['profile', 'list', '--json'])) ?? []
  return loginsKnown
}

// The decisions plugin's cards, read only; none when it is not installed or unreadable.
async function cardCount($: EngineInterface): Promise<number> {
  try {
    const read = (await $.state.get({ plugin: 'decisions', key: 'items' } as never)) as { value?: unknown }
    return Array.isArray(read.value) ? read.value.length : 0
  } catch {
    return 0
  }
}

async function readSnapshot($: EngineInterface, options: Options, force: boolean): Promise<ThothSnapshot> {
  const fetched = { tasks: [], outsideRepo: false, checks: [], table: null, profiles: [], usageFailed: false, logins: [], problemText: '' }
  const status = await thoth($, options, ['status', '--json'])
  const problem = statusProblem(status)
  if (problem !== null || status === null) {
    return { ...fetched, fetchedAt: await $.clock.now(), problem: problem?.problem ?? 'not_found', problemText: problem?.text ?? '' }
  }
  const outsideRepo = parseOutsideRepo(status.stdout)
  const profiles = parseProfiles(await thoth($, options, ['profile', 'usage', '--json']))
  // Checks belong to one repository: outside one there are none to list.
  const checks = outsideRepo ? [] : await readChecks($, options, force)
  const table = await readTable($, options, force)
  const logins = await readLogins($, options, force)
  return {
    ...fetched,
    tasks: parseTasks(status.stdout),
    outsideRepo,
    checks,
    table,
    profiles: profiles ?? [],
    usageFailed: profiles === null,
    logins,
    fetchedAt: await $.clock.now(),
    problem: null,
  }
}

// This session's own place in the event log; the store carries it between sessions.
const cursor = atom({ plugin: 'thoth', key: 'cursor' } as const, null)
// The title of every task this session has read, by task_id: a task that leaves the list is still named.
const titles = atom({ plugin: 'thoth', key: 'titles' } as const, {})

// The shared store value only moves up; a store that fails costs nothing else.
async function raiseStored($: EngineInterface, key: string, seq: number): Promise<void> {
  try {
    const stored = await $.store.get(key)
    if (typeof stored !== 'number' || seq > stored) await $.store.set(key, seq)
  } catch {}
}

// Toasts each announced event once per session: the cursor is kept before the toasts go up.
async function announce($: EngineInterface, options: Options, current: ThothSnapshot): Promise<void> {
  const key = cursorKey(await read($, project))
  let seen = await read($, cursor)
  if (seen === null) {
    let stored: unknown = null
    try {
      stored = await $.store.get(key)
    } catch {
      // An unreadable store is not an empty one: leave the cursor unset and ask again next time.
      return
    }
    if (typeof stored === 'number') {
      seen = stored
      await update($, cursor, () => stored as number)
    }
  }
  if (seen === null) {
    // First load anywhere: start from what is there now, announcing none of it.
    const first = parseEvents(await thoth($, options, ['events', '--since', '0', '--json']))
    if (first === null) return
    await update($, cursor, () => first.lastSeq)
    await raiseStored($, key, first.lastSeq)
    return
  }
  const fresh = parseEvents(await thoth($, options, ['events', '--since', String(seen), '--json']))
  if (fresh === null) return
  const toasts = toastsFor(
    fresh.events,
    seen,
    current.tasks,
    await read($, titles),
    options.language,
    planLabels(current.profiles),
    loginNames(current.logins),
  )
  if (fresh.lastSeq > seen) {
    await update($, cursor, () => fresh.lastSeq)
    await raiseStored($, key, fresh.lastSeq)
  }
  for (const text of toasts) $.ui.toast(text)
}

function emptyWorker(task_id: string, fetchedAt: number): ThothWorker {
  return {
    task_id,
    run_id: '',
    role: '',
    profile: '',
    model: '',
    status: '',
    heartbeat_stale: false,
    worktree: '',
    harness: null,
    exists: false,
    offset: 0,
    steps: [],
    dropped: 0,
    problem: null,
    fetchedAt,
  }
}

// One pass over the watched task's newest run; a pass asked for while one runs returns at once.
let isPolling = false

async function pollOnce($: EngineInterface, options: Options): Promise<void> {
  const watched = await read($, watching)
  if (watched === null) return
  const previous = await read($, worker)
  const before = previous !== null && previous.task_id === watched.task_id ? previous : null
  const now = await $.clock.now()
  const store = async (next: ThothWorker) => {
    // The person may have closed the pane or watched another task while thoth answered.
    const still = await read($, watching)
    if (still === null || still.task_id !== watched.task_id) return
    await update($, worker, () => next)
  }
  const runs = parseRuns(await thoth($, options, ['status', watched.task_id, '--json'], watched.repo))
  if (runs === null) {
    await store({ ...(before ?? emptyWorker(watched.task_id, now)), problem: 'status_failed', fetchedAt: now })
    return
  }
  const newest = newestRun(runs)
  if (newest === null) {
    await store({ ...(before ?? emptyWorker(watched.task_id, now)), problem: 'no_run', fetchedAt: now })
    return
  }
  const same = before !== null && before.run_id === newest.run_id
  const held = same ? before : { ...emptyWorker(watched.task_id, now), run_id: newest.run_id }
  const offset = same ? before.offset : 0
  const head = {
    ...held,
    role: newest.role,
    profile: newest.profile,
    model: newest.model,
    status: newest.status,
    heartbeat_stale: newest.heartbeat_stale,
    worktree: newest.worktree,
    fetchedAt: await $.clock.now(),
  }
  const read2 = parseTranscript(
    await thoth($, options, ['transcript', newest.run_id, '--offset', String(offset), '--json'], watched.repo),
  )
  if (read2 === null) {
    await store({ ...head, problem: 'transcript_failed' })
    return
  }
  const kept = read2.reset ? [] : head.steps
  const all = [...kept, ...read2.steps]
  const cut = Math.max(0, all.length - STEPS_KEPT)
  await store({
    ...head,
    harness: read2.harness,
    exists: read2.exists,
    offset: read2.nextOffset,
    steps: all.slice(cut),
    dropped: (read2.reset ? 0 : head.dropped) + cut,
    problem: null,
  })
}

async function pollWorker($: EngineInterface, options: Options): Promise<void> {
  if (isPolling) return
  isPolling = true
  try {
    await pollOnce($, options)
  } catch {
    // The run pane is a side view: its failure never reaches the main refresh.
  } finally {
    isPolling = false
  }
}

async function refresh($: EngineInterface, options: Options, force = false): Promise<void> {
  if (isRefreshing) return
  isRefreshing = true
  let drawn: ThothSnapshot | null = null
  try {
    const fresh = await readSnapshot($, options, force)
    drawn = fresh
    await update($, snapshot, () => fresh)
    try {
      await dropUnfitConfirm($, fresh)
    } catch {}
    if (fresh.problem === null) {
      try {
        await update($, titles, known => rememberTitles(known, fresh.tasks))
      } catch {}
    }
    if (fresh.problem === null) {
      // The event cursor is per repository: outside one there is no log to announce from.
      if (!fresh.outsideRepo) await announce($, options, fresh)
      await pollWorker($, options)
    }
  } catch {
    // A failed step must not stop the cycle: the next refresh is still scheduled below.
  } finally {
    isRefreshing = false
  }
  timer?.cancel()
  timer = $.clock.after(nextDelay(drawn), () => {
    timer = undefined
    void refresh($, options).catch(() => {})
  })
}

/** Drops a waiting action once the task left its section or its live run changed. */
async function dropUnfitConfirm($: EngineInterface, current: ThothSnapshot): Promise<void> {
  const waiting = await read($, confirming)
  if (waiting !== null && !confirmStillFits(waiting, current)) await update($, confirming, () => null)
}

/** Where a task's commands run: its own repository outside one, else the project. */
const repoOf = (task: ThothTask, outsideRepo: boolean) => (outsideRepo ? task.repo : '')

function waitingAction(kind: ThothConfirm['kind'], task: ThothTask, outsideRepo: boolean): ThothConfirm {
  return {
    kind,
    task_id: task.task_id,
    title: task.title,
    repo: repoOf(task, outsideRepo),
    run_id: '',
    role: '',
    profile: '',
    model: '',
    effort: null,
    blocked: null,
  }
}

/** First press of Cancel: nothing runs; the live run is the one a confirm stops. */
async function askCancel($: EngineInterface, task: ThothTask, outsideRepo: boolean): Promise<void> {
  const live = task.live_run
  if (!live) return
  const next = { ...waitingAction('cancel', task, outsideRepo), run_id: live.run_id, role: live.role, profile: live.profile, model: live.model }
  await update($, confirming, () => next)
}

/** First press of Close task: nothing runs. */
async function askClose($: EngineInterface, task: ThothTask, outsideRepo: boolean): Promise<void> {
  const next = waitingAction('close', task, outsideRepo)
  await update($, confirming, () => next)
}

/** First press of Dispatch again: reads the task's last implementer run, writes nothing. */
async function askAgain($: EngineInterface, options: Options, task: ThothTask, outsideRepo: boolean): Promise<void> {
  const lang = options.language
  const title = taskName(task.title, lang)
  const ran = await thoth($, options, ['status', task.task_id, '--json'], repoOf(task, outsideRepo))
  if (ran === null) {
    $.ui.toast(t(lang, 'actionTimedOut'))
    return
  }
  const runs = parseRuns(ran)
  if (runs === null) {
    $.ui.toast(t(lang, 'actionFailed', { title, text: errorText(ran) }))
    return
  }
  const last = newestImplementerRun(runs)
  if (last === null) {
    $.ui.toast(t(lang, 'noEarlierRun', { title }))
    return
  }
  // The same account is out of quota or logged out: say what to do, offer no confirm.
  const stuck = last.interrupted_reason === 'quota' || last.interrupted_reason === 'auth'
  const next = {
    ...waitingAction('again', task, outsideRepo),
    run_id: last.run_id,
    profile: last.profile,
    model: last.model,
    effort: last.effort_requested,
    blocked: stuck ? (last.next_step ?? '') : null,
  }
  await update($, confirming, () => next)
}

// The tasks whose confirmed action is running: another confirm meanwhile runs nothing.
const acting = new Set<string>()

/** Second press: runs the waiting action once, says how it went, and refreshes. */
async function runConfirmed($: EngineInterface, options: Options, taskId: string): Promise<void> {
  const lang = options.language
  const waiting = await read($, confirming)
  if (waiting === null || waiting.task_id !== taskId || waiting.blocked !== null || acting.has(taskId)) return
  acting.add(taskId)
  try {
    await update($, confirming, () => null)
    if (!confirmStillFits(waiting, await read($, snapshot))) return
    const ran = await thoth($, options, actionArgs(waiting), waiting.repo, WRITE_TIMEOUT_MS)
    const title = taskName(waiting.title, lang)
    const done = waiting.kind === 'cancel' ? 'done.cancel' : waiting.kind === 'close' ? 'done.close' : 'done.again'
    if (ran === null) $.ui.toast(t(lang, 'actionTimedOut'))
    else if (ran.exitCode === 0) $.ui.toast(t(lang, done, { title }))
    else $.ui.toast(t(lang, 'actionFailed', { title, text: errorText(ran) }))
  } finally {
    acting.delete(taskId)
  }
  await refresh($, options)
}

async function openReport($: EngineInterface, options: Options, section?: string): Promise<void> {
  const ran = await thoth($, options, section ? ['report', '--open', '--section', section] : ['report', '--open'])
  if (ran === null || ran.exitCode !== 0) $.ui.toast(t(options.language, 'reportFailed'))
}

export const register: Register = (on, raw) => {
  const options = optionsOf(raw)
  const lang = options.language

  on('session.start', async ($, e, next) => {
    const root = await projectOf($)
    await update($, project, () => root)
    checksAt = null
    checksKnown = []
    tableAt = null
    tableKnown = null
    loginsAt = null
    loginsKnown = []
    // The engine keeps the last session's state: nothing is being watched yet, no action waits for a
    // second press, and a pane left over from before a reload would only show an empty list.
    try {
      await update($, confirming, () => null)
      await update($, watching, () => null)
      await update($, worker, () => null)
      await $.ui.close({ id: 'thoth-run' })
    } catch {}
    await $.command.register({ name: 'thoth', description: t(lang, 'commandDescription') })
    const started = await next(e)
    void refresh($, options).catch(() => {})
    return started
  })

  // Closing the run pane stops the polling: refresh reads `watching` and finds nothing.
  on('ui.close', { id: RUN_PANE }, async ($, e, next) => {
    const done = await next(e)
    try {
      await update($, watching, () => null)
      await update($, worker, () => null)
    } catch {}
    return done
  })

  on('command.run', { command: 'thoth' }, async $ => {
    await refresh($, options, true)
    await $.ui.open({ id: PANE, title: TITLE })
    return { text: t(lang, 'commandOpened') }
  })

  // Something may have moved while the person typed; do not hold the prompt for it.
  on('prompt.submit', ($, e, next) => {
    void refresh($, options).catch(() => {})
    return next(e)
  })

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text, Button } = $.ui.resolve(e)
    const columns = Math.max(10, e.props.bodyColumns)
    const current = await read($, snapshot)
    if (current === null) return <Text dimColor>{t(lang, 'loading')}</Text>
    const hint = problemHint(current, lang)
    if (hint !== null) {
      return (
        <Text color="yellow" wrap="wrap">
          {hint}
        </Text>
      )
    }

    const seconds = Math.max(0, Math.round(((await $.clock.now()) - current.fetchedAt) / 1000))
    const heading = `Thoth · ${repoName(await read($, project))} · ${t(lang, 'updated', { n: seconds })}`
    tick?.cancel()
    tick = $.clock.after(1000, () => {
      tick = undefined
      $.ui.invalidate('ui.render')
    })
    const { waitingOnCommander, inProgress } = groupTasks(current.tasks)
    const cards = await cardCount($)
    const rule = (label: string) => fit(`── ${label} ${'─'.repeat(Math.max(0, columns - textWidth(label) - 4))}`, columns)
    const outside = current.outsideRepo
    const account = accountOf(current, lang)
    const line = (task: (typeof current.tasks)[number]) => fit(taskLineIn(task, outside, lang, account), columns)
    // The action waiting for its second press, while it still fits the task as drawn.
    const waiting = await read($, confirming)
    const open = waiting !== null && confirmStillFits(waiting, current) ? waiting : null
    const yes = { cancel: 'yesCancel', close: 'yesClose', again: 'yesAgain' } as const
    const confirmBlock = (task: ThothTask) => {
      if (open === null || open.task_id !== task.task_id) return null
      return (
        <Box key={`confirming:${task.task_id}`} flexDirection="column">
          <Text color="yellow" wrap="wrap">
            {open.blocked ?? confirmText(open, lang, account)}
          </Text>
          <Box flexDirection="row" gap={1}>
            {open.blocked === null && (
              <Button
                key={`confirm:${task.task_id}`}
                label={t(lang, yes[open.kind])}
                onPress={() => runConfirmed($, options, task.task_id)}
              />
            )}
            <Button key={`back:${task.task_id}`} label={t(lang, 'back')} onPress={() => update($, confirming, () => null)} />
          </Box>
        </Box>
      )
    }
    const watch = async (task: { task_id: string; title: string; repo: string }) => {
      // Outside a repository the worker is read in its task's repository; inside, in the project.
      await update($, watching, () => ({ task_id: task.task_id, title: task.title, repo: outside ? task.repo : '' }))
      await update($, worker, () => null)
      await $.ui.open({ id: RUN_PANE, title: fit(taskName(task.title, lang), 30) })
      void pollWorker($, options).catch(() => {})
    }
    const ask = (title: string, status: string) =>
      $.prompt.submit({ text: t(lang, 'askText', { title: taskName(title, lang), status: statusLabel(status, lang) }) })

    return (
      <Box flexDirection="column">
        <Text bold>{fit(heading, columns)}</Text>
        {outside && (
          <Text color="yellow" wrap="wrap">
            {t(lang, 'outsideRepo')}
          </Text>
        )}
        <Text color="yellow">{rule(t(lang, 'needsYou'))}</Text>
        {needsYouCount(cards) === 0 && <Text dimColor>{t(lang, 'none')}</Text>}
        {cards > 0 && <Text>{fit(`● ${t(lang, 'cards', { n: cards })}`, columns)}</Text>}
        <Text color="yellow">{rule(t(lang, 'commander'))}</Text>
        {commanderCount(current) === 0 && <Text dimColor>{t(lang, 'none')}</Text>}
        {waitingOnCommander.map(task => (
          <Box key={`task:${task.task_id}`} flexDirection="column">
            <Text {...red(task.any_heartbeat_stale)}>{line(task)}</Text>
            <Box flexDirection="row" gap={1}>
              <Button
                key={`ask:${task.task_id}`}
                label={t(lang, 'ask')}
                onPress={() => ask(task.title, task.status)}
              />
              <Button key={`report:${task.task_id}`} label={t(lang, 'report')} onPress={() => openReport($, options)} />
              <Button key={`watch:${task.task_id}`} label={t(lang, 'watch')} onPress={() => watch(task)} />
              {CLOSABLE.includes(task.status) && (
                <Button key={`close:${task.task_id}`} label={t(lang, 'closeTask')} onPress={() => askClose($, task, outside)} />
              )}
              {CLOSABLE.includes(task.status) && (
                <Button key={`again:${task.task_id}`} label={t(lang, 'again')} onPress={() => askAgain($, options, task, outside)} />
              )}
            </Box>
            {confirmBlock(task)}
          </Box>
        ))}
        {current.checks.map(check => (
          <Text key={`check:${check.check_id}`}>{fit(checkLine(check, lang), columns)}</Text>
        ))}
        <Text color="cyan">{rule(t(lang, 'inProgress'))}</Text>
        {inProgress.length === 0 && <Text dimColor>{t(lang, 'none')}</Text>}
        {inProgress.map(task => (
          <Box key={`task:${task.task_id}`} flexDirection="column">
            <Text {...red(task.any_heartbeat_stale)}>{line(task)}</Text>
            <Box flexDirection="row" gap={1}>
              <Button key={`watch:${task.task_id}`} label={t(lang, 'watch')} onPress={() => watch(task)} />
              {task.live_run && (
                <Button key={`cancel:${task.task_id}`} label={t(lang, 'cancel')} onPress={() => askCancel($, task, outside)} />
              )}
            </Box>
            {confirmBlock(task)}
          </Box>
        ))}
        <Text color="green">{rule(t(lang, 'quota'))}</Text>
        {current.usageFailed && <Text dimColor>{fit(t(lang, 'usageFailed'), columns)}</Text>}
        {current.profiles.map(profile => (
          <Text key={`quota:${profile.profile_id}`} {...red(profile.quota_state === 'exhausted')}>
            {fit(quotaText(profile, lang, loginNames(current.logins)), columns)}
          </Text>
        ))}
        <Text color="blue">{rule(t(lang, 'table'))}</Text>
        <Text {...(current.table === null ? { dimColor: true } : {})}>{fit(tableLine(current.table, lang), columns)}</Text>
        <Button key="table" label={t(lang, 'openTable')} onPress={() => openReport($, options, 'decision-table')} />
      </Box>
    )
  })

  on('ui.render', { component: 'Pane', requestId: RUN_PANE }, async ($, e) => {
    const { Box, Text } = $.ui.resolve(e)
    const columns = Math.max(10, e.props.bodyColumns)
    const watched = await read($, watching)
    if (watched === null) return <Text dimColor>{t(lang, 'none')}</Text>
    const current = await read($, worker)
    if (current === null) return <Text dimColor>{t(lang, 'loading')}</Text>

    const seconds = Math.max(0, Math.round(((await $.clock.now()) - current.fetchedAt) / 1000))
    tick?.cancel()
    tick = $.clock.after(1000, () => {
      tick = undefined
      $.ui.invalidate('ui.render')
    })
    // The account's login name and label come from the last refresh; the id itself stays as thoth gave it.
    const shown = await read($, snapshot)
    const head = [
      taskName(watched.title, lang),
      roleLabel(current.role, lang),
      accountOf(shown, lang)(current.profile),
      current.model,
      statusLabel(current.status, lang),
    ].join(' · ')
    const count = current.dropped + current.steps.length
    const second = `${t(lang, 'stepsCount', { n: count })} · ${t(lang, 'updated', { n: seconds })}`
    const noSteps = current.steps.length === 0
    return (
      <Box flexDirection="column">
        <Text bold>{fit(current.heartbeat_stale ? `${head} · ${t(lang, 'stale')}` : head, columns)}</Text>
        <Text dimColor>{fit(second, columns)}</Text>
        {current.problem === 'status_failed' ? (
          <Text color="yellow">{fit(t(lang, 'workerStatusFailed'), columns)}</Text>
        ) : current.problem === 'no_run' ? (
          <Text dimColor>{fit(t(lang, 'noRun'), columns)}</Text>
        ) : current.harness === 'antigravity' ? (
          <Text dimColor>{fit(t(lang, 'agyNotYet'), columns)}</Text>
        ) : !current.exists && noSteps ? (
          <Text dimColor>{fit(t(lang, 'waitingWorker'), columns)}</Text>
        ) : null}
        {current.problem === 'transcript_failed' && (
          <Text color="yellow">{fit(t(lang, 'transcriptFailed'), columns)}</Text>
        )}
        {current.dropped > 0 && <Text dimColor>{fit(t(lang, 'earlier', { n: current.dropped }), columns)}</Text>}
        {current.steps.map((step, index) => (
          <Text key={`step:${index}`} {...stepProps(step)}>
            {fit(stepLine(step, current.worktree, lang), columns)}
          </Text>
        ))}
      </Box>
    )
  })

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (e.props.hasSurvey) return next(e)
    const text = bandText(await read($, snapshot), lang, await cardCount($))
    if (text === null) return next(e)
    const { Box, Text, Button } = $.ui.resolve(e)
    const label = t(lang, 'open')
    // The Button draws as `[ label ]`, one cell of gap before it.
    const room = Math.max(0, e.props.bodyColumns - textWidth(label) - 5)
    // A tree returned here replaces what the mods after this one draw, so theirs goes underneath.
    const after = await next(e)
    return (
      <Box flexDirection="column">
        <Box flexDirection="row" gap={1}>
          <Text>{fit(text, room)}</Text>
          <Button key="open" label={label} onPress={() => $.ui.open({ id: PANE, title: TITLE })} />
        </Box>
        {after}
      </Box>
    )
  })
}
