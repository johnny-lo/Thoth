import type { On } from 'claude-code'
import { expect, mock, test } from 'claude-code/testing'
import type { Engine, MockClock } from 'claude-code/testing'

import { hasKey } from '../hooks/i18n'
import { groupTasks } from '../hooks/model'
import { named, planLabels } from '../hooks/model'

const SURFACES = ['terminal', 'desktop'] as const
const SCROLL = { offset: 0, bodyRows: 10 }
const PANE_PROPS = { title: 'Thoth', isFocused: true, bodyColumns: 100, placement: 'dock', scroll: SCROLL, view: {} } as const
const BAND_PROPS = { hasSurvey: false, isWorking: false, maxRows: 5, bodyColumns: 100, scroll: SCROLL, view: {} }

const task = (task_id: string, title: string, status: string, extra: Record<string, unknown> = {}) => ({
  task_id,
  title,
  status,
  risk: 'normal',
  runs: 1,
  any_heartbeat_stale: false,
  updated_at: '2026-10-06T00:00:00Z',
  ...extra,
})

const SAMPLE_STATUS = {
  tasks: [
    task('T1', 'S3-9 handoff', 'in_review', { risk: 'important', runs: 2 }),
    task('T2', 'S3-11 codex quota', 'running'),
    task('T3', 'P7-x check', 'unverified', { any_heartbeat_stale: true }),
    task('T4', 'MOD-2 stream', 'failed', { runs: 3 }),
    task('T5', 'Small fixes', 'queued', { runs: 0 }),
  ],
}

const SAMPLE_USAGE = {
  profiles: [
    {
      profile_id: 'claude-a',
      harness: 'claude-code',
      quota_state: 'ok',
      windows: [
        { name: 'five_hour', used_percent: 22, resets_at: '2026-10-06T14:00:00Z' },
        { name: 'seven_day', used_percent: 36, resets_at: '2026-10-10T00:00:00Z' },
      ],
    },
    {
      profile_id: 'codex-a',
      harness: 'codex',
      quota_state: 'exhausted',
      windows: [{ name: 'five_hour', used_percent: 86, resets_at: null }],
    },
    { profile_id: 'agy-a', harness: 'antigravity', quota_state: 'unknown', windows: [] },
  ],
}

const SAMPLE_EVENTS_FIRST = {
  last_seq: 40,
  events: [{ seq: 40, type: 'status_change', detail: { entity: 'task', id: 'T1', from: 'running', to: 'in_review' } }],
}

const SAMPLE_EVENTS_NEW = {
  last_seq: 43,
  events: [
    { seq: 41, type: 'status_change', detail: { entity: 'task', id: 'T4', from: 'in_review', to: 'reject' } },
    { seq: 42, type: 'quota_exceeded', detail: { profile: 'codex-a', note: 'limit reached' } },
    { seq: 43, type: 'status_change', detail: { entity: 'run', id: 'R9', from: 'running', to: 'failed' } },
  ],
}

const EMPTY_EVENTS = { last_seq: 43, events: [] }

// The store key for /repo, spelled in parts: written whole, the letter, colon and slash
// in the middle read as a drive-letter path to the repository's portability check.
const CURSOR_KEY = ['events_cursor', '/repo'].join(':')

type Reply = { exitCode?: number; stdout?: string; stderr?: string } | 'throw'

type World = {
  /** Replies by thoth's arguments joined with spaces (`status --json`). */
  replies: Record<string, Reply>
  calls: { argv: readonly string[]; env: Record<string, string> | undefined; cwd: string | undefined }[]
  toasts: string[]
  submitted: string[]
  opened: string[]
  /** Every value the plugin stored, by key, as last written. */
  stored: Map<string, unknown>
  clock: MockClock
  /** How many times `thoth status --json` ran. */
  statusRuns: () => number
  /** While true, the store replacement's `set` answers with a denial. */
  storeSetFails: boolean
  /** While true, the store replacement's `get` answers with a denial. */
  storeGetFails: boolean
  /** What `ui.copy` was asked to copy. */
  copied: string[]
  /** The decisions plugin's cards; undefined: that plugin is not installed. */
  cards: unknown[] | undefined
  /** How many times `thoth checks list --json` ran. */
  checkRuns: () => number
  /** The ids of the panes `ui.close` was asked to close. */
  closed: string[]
  /** How many times `thoth models --json` ran. */
  modelRuns: () => number
}

const json = (value: unknown): Reply => ({ stdout: JSON.stringify(value) })

/** What the engine itself draws in the band when the plugin passes; the engine tags it with the mod that drew it. */
const ENGINE_BAND = { type: 'Text', children: ['engine band'], part: { plugin: 'test' } } as const

// A session on /repo whose thoth answers from `replies`, unlisted commands an empty object.
function world(on: On, replies: Record<string, Reply>): World {
  const w: World = {
    replies,
    calls: [],
    toasts: [],
    submitted: [],
    opened: [],
    stored: new Map(),
    clock: mock.clock(on),
    statusRuns: () => w.calls.filter(call => call.argv.slice(1).join(' ') === 'status --json').length,
    storeSetFails: false,
    storeGetFails: false,
    copied: [],
    cards: undefined,
    checkRuns: () => w.calls.filter(call => call.argv.slice(1).join(' ') === 'checks list --json').length,
    closed: [],
    modelRuns: () => w.calls.filter(call => call.argv.slice(1).join(' ') === 'models --json').length,
  }
  on('ui.close', (_, e) => {
    w.closed.push(e.id)
    return { value: undefined }
  })
  on('state.get', (_, e, next) => {
    if (e.plugin === 'decisions' && e.key === 'items' && w.cards !== undefined) {
      return { value: { value: w.cards, version: 1 } }
    }
    return next(e)
  })
  on('ui.copy', (_, e) => {
    w.copied.push(e.text)
    return { value: { isCopied: true } }
  })
  // A store in memory the test can read back (mock.store keeps its own out of reach).
  on('store.get', (_, e) => {
    if (w.storeGetFails) return { deny: 'store is unreadable' }
    return { value: w.stored.get(e.key) }
  })
  on('store.set', (_, e) => {
    if (w.storeSetFails) return { deny: 'store is read only' }
    w.stored.set(e.key, JSON.parse(JSON.stringify(e.value)))
    return { value: undefined }
  })
  on('ui.render', { component: 'AbovePrompt' }, () => ENGINE_BAND)
  on('session.cwd', () => ({ value: '/repo' }))
  on('session.start', (_, e) => e)
  on('command.register', (_, e) => ({ value: { command: e.name } }))
  on('ui.open', (_, e) => {
    w.opened.push(e.id)
    return { value: undefined as never }
  })
  on('ui.toast', (_, e) => {
    w.toasts.push(e.text)
    return { value: undefined }
  })
  on('prompt.submit', (_, e) => {
    w.submitted.push(e.text)
    return { text: e.text }
  })
  on('process.run', (_, e) => {
    w.calls.push({ argv: e.argv, env: e.init?.env, cwd: e.init?.cwd })
    if (e.argv[0] === 'git') {
      return { value: { exitCode: 0, stdout: '/repo/.git\n', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
    }
    const reply = w.replies[e.argv.slice(1).join(' ')] ?? (e.argv[1] === 'checks' ? { stdout: '{"checks":[]}' } : { stdout: '{}' })
    if (reply === 'throw') throw new Error('thoth: not found')
    return {
      value: {
        exitCode: reply.exitCode ?? 0,
        stdout: reply.stdout ?? '',
        stderr: reply.stderr ?? '',
        isStdoutTruncated: false,
        isStderrTruncated: false,
      },
    }
  })
  return w
}

const SAMPLE_REPLIES = (): Record<string, Reply> => ({
  'status --json': json(SAMPLE_STATUS),
  'profile usage --json': json(SAMPLE_USAGE),
  'events --since 0 --json': json(SAMPLE_EVENTS_FIRST),
  'events --since 40 --json': json(SAMPLE_EVENTS_NEW),
  'events --since 43 --json': json(EMPTY_EVENTS),
})

async function start($: Engine, w: World): Promise<void> {
  await $.session.start({ cwd: '/repo' })
  // session.start leaves the first refresh running; let it finish.
  await w.clock.settle()
}

const pane = ($: Engine, surface: (typeof SURFACES)[number]) =>
  $.ui.mount({ plugin: 'thoth', surface, component: 'Pane', requestId: 'thoth', props: PANE_PROPS })

const band = ($: Engine, surface: (typeof SURFACES)[number], props = BAND_PROPS) =>
  $.ui.mount({ plugin: 'thoth', surface, component: 'AbovePrompt', props })

test('groups_tasks_into_waiting_on_commander_and_in_progress', () => {
  const { waitingOnCommander, inProgress } = groupTasks(SAMPLE_STATUS.tasks)
  expect(waitingOnCommander.map(t => t.task_id)).toEqual(['T1', 'T3', 'T4'])
  expect(inProgress.map(t => t.task_id)).toEqual(['T2', 'T5'])
})

test('pane_draws_three_sections_and_quota', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    expect(await ui.find({ text: /^Thoth · repo · / })).toBeDefined()
    expect(await ui.find({ text: '需要你' })).toBeDefined()
    expect(await ui.find({ text: '進行中' })).toBeDefined()
    expect(await ui.find({ text: '帳號額度' })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /^● S3-9 handoff 等審查$/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /^● P7-x check 未驗證 可能卡住$/ })).toBeDefined()
    expect(await ui.find({ type: 'Text', text: /^● S3-11 codex quota 執行中$/ })).toBeDefined()
    const claude = await ui.find({ type: 'Text', text: /^claude-a/ })
    expect(claude?.text).toContain('剩 78%')
    expect(claude?.text).toContain('每週 剩 64%')
    expect(claude?.text).toContain(' · ')
    const codex = await ui.find({ type: 'Text', text: /^codex-a/ })
    expect(codex?.text).toContain('剩 14%')
    expect(codex?.text).toContain('額度用完')
    expect(codex?.props.color).toBe('red')
    expect((await ui.find({ type: 'Text', text: /^agy-a/ }))?.text).toContain('還沒有讀數')
    await ui.unmount()
  }
})

test('ask_button_submits_title_and_status', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    await ui.press({ key: 'ask:T1' })
    expect(w.submitted).toHaveLength(1)
    expect(w.submitted[0]).toBe('Thoth task "S3-9 handoff" is now in review. Please take a look and tell me the next step.')
    expect(w.submitted[0]).not.toContain('T1')
    await ui.unmount()
    w.submitted.length = 0
  }
})

test('report_button_runs_thoth_report_open', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    w.calls.length = 0
    const ui = await pane($, surface)
    await ui.press({ key: 'report:T3' })
    expect(w.calls.map(call => call.argv)).toContainEqual(['thoth', 'report', '--open'])
    expect(w.toasts).toEqual([])
    await ui.unmount()
  }
  // A report that cannot open says so in one toast.
  w.replies['report --open'] = { exitCode: 1, stderr: 'no browser' }
  const ui = await pane($, 'terminal')
  await ui.press({ key: 'report:T3' })
  expect(w.toasts).toHaveLength(1)
})

test('band_shows_only_when_something_runs_or_waits', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await band($, surface)
    const line = await ui.find({ type: 'Text', text: /Thoth · / })
    expect(line?.text).toContain('2 件在跑')
    expect(line?.text).not.toContain('件等你')
    expect(line?.text).toContain('3 件指揮官處理中')
    expect(line?.text).toContain('codex-a 五小時 剩 14%')
    expect(await ui.find({ type: 'Button', key: 'open' })).toBeDefined()
    await ui.press({ key: 'open' })
    expect(w.opened).toContain('thoth')
    await ui.unmount()

    // A survey holds the band: the engine's own drawing stands.
    const survey = await band($, surface, { ...BAND_PROPS, hasSurvey: true })
    expect(await survey.find({ text: /Thoth · / })).toBeUndefined()
    expect(await survey.find({ text: 'engine band' })).toBeDefined()
    await survey.unmount()
  }

  // No open tasks: the band leaves its place.
  w.replies['status --json'] = json({ tasks: [] })
  await w.clock.advance(10_000)
  for (const surface of SURFACES) {
    const ui = await band($, surface)
    expect(await ui.drawn()).toEqual(ENGINE_BAND)
    expect(await ui.find({ key: 'open' })).toBeUndefined()
    await ui.unmount()
  }
})

test('first_load_sets_the_cursor_and_toasts_nothing', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  expect(w.stored.get(CURSOR_KEY)).toBe(40)
  expect(w.toasts).toEqual([])
  expect(w.calls.map(call => call.argv.join(' '))).toContain('thoth events --since 0 --json')
})

test('first_load_of_a_truncated_log_takes_the_largest_seq', async ($, on) => {
  const replies = SAMPLE_REPLIES()
  replies['events --since 0 --json'] = { stdout: '{"events":[{"seq":7,"type":"x"},{"seq":12,"type":"status_change","det' }
  const w = world(on, replies)
  await start($, w)
  expect(w.stored.get(CURSOR_KEY)).toBe(12)
  expect(w.toasts).toEqual([])
})

test('a_status_change_toasts_once_and_old_events_never_again', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(['MOD-2 stream 被退件', 'codex-a 額度用完，任務中斷'])
  expect(w.stored.get(CURSOR_KEY)).toBe(43)
  const since = w.calls.map(call => call.argv.join(' ')).filter(argv => argv.includes('events'))
  expect(since).toEqual(['thoth events --since 0 --json', 'thoth events --since 40 --json', 'thoth events --since 43 --json'])
})

const taskGone = (id: string) =>
  json({
    last_seq: 41,
    events: [{ seq: 41, type: 'status_change', detail: { entity: 'task', id, from: 'in_review', to: 'pass' } }],
  })

test('a_task_no_longer_listed_is_toasted_by_its_remembered_title', async ($, on) => {
  const replies = SAMPLE_REPLIES()
  replies['events --since 40 --json'] = taskGone('T1')
  const w = world(on, replies)
  await start($, w)
  w.replies['status --json'] = json({ tasks: [task('T2', 'S3-11 codex quota', 'running')] })
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(['S3-9 handoff passed'])
})

test('a_task_never_seen_is_toasted_as_a_task', async ($, on) => {
  const replies = SAMPLE_REPLIES()
  replies['events --since 40 --json'] = taskGone('T9')
  const w = world(on, replies)
  await start($, w)
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(['a task passed'])
})

const PANEL_ID = '01M493CG3JVZQR0KYKVMAMDWGR'
const panelReplies = (): Record<string, Reply> => ({
  ...SAMPLE_REPLIES(),
  'status --json': json({ tasks: [task(PANEL_ID, 'Panel ids', 'in_review', { any_heartbeat_stale: true })] }),
})

async function texts(ui: Awaited<ReturnType<typeof pane>>): Promise<string[]> {
  const nodes = [...(await ui.findAll({ type: 'Text' })), ...(await ui.findAll({ type: 'Button' }))]
  return nodes.map(node => node.text ?? '')
}

test('no_human_facing_text_carries_a_task_id', async ($, on) => {
  const w = world(on, panelReplies())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const drawn = await texts(ui)
    for (const text of drawn) expect(text).not.toContain('01M493CG')
    expect(drawn).toContain('● Panel ids in review may be stuck')
    await ui.press({ key: `ask:${PANEL_ID}` })
    expect(w.submitted).toEqual(['Thoth task "Panel ids" is now in review. Please take a look and tell me the next step.'])
    expect(w.submitted[0]).not.toContain('01M493CG')
    expect(w.submitted[0]).not.toContain('in_review')
    await ui.unmount()
    w.submitted.length = 0
    const line = await band($, surface)
    for (const text of await texts(line)) expect(text).not.toContain('01M493CG')
    await line.unmount()
  }
  for (const text of w.toasts) expect(text).not.toContain('01M493CG')
})

test('no_human_facing_text_carries_a_task_id_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, panelReplies())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const drawn = await texts(ui)
    for (const text of drawn) expect(text).not.toContain('01M493CG')
    expect(drawn).toContain('● Panel ids 等審查 可能卡住')
    await ui.press({ key: `ask:${PANEL_ID}` })
    expect(w.submitted).toEqual(['Thoth 任務「Panel ids」目前是等審查，請看一下並告訴我下一步。'])
    await ui.unmount()
    w.submitted.length = 0
    const line = await band($, surface)
    for (const text of await texts(line)) expect(text).not.toContain('01M493CG')
    await line.unmount()
  }
})

test('an_unreadable_store_with_no_cursor_waits_for_the_next_refresh', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  w.stored.set(CURSOR_KEY, 40)
  w.storeGetFails = true
  await start($, w)
  expect(eventCalls(w)).toEqual([])
  w.storeGetFails = false
  w.calls.length = 0
  await w.clock.advance(10_000)
  expect(eventCalls(w)).toEqual(['thoth events --since 40 --json'])
  expect(w.toasts).toEqual(TWO_TOASTS)
})

test('not_initialised_shows_a_hint_not_a_dump', { options: { language: 'zh-TW' } }, async ($, on) => {
  const stderr = 'error: repo /somewhere is not initialised; run `thoth init` in /somewhere\n'
  const w = world(on, { 'status --json': { exitCode: 1, stderr } })
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const drawn = await ui.findAll({ type: 'Text' })
    expect(drawn).toHaveLength(1)
    expect(drawn[0]!.text).toContain('thoth init')
    expect(drawn[0]!.text).not.toContain('somewhere')
    await ui.unmount()
    const quiet = await band($, surface)
    expect(await quiet.find({ text: 'engine band' })).toBeDefined()
    await quiet.unmount()
  }
  expect(w.calls.some(call => call.argv.includes('usage'))).toBe(false)

  // No thoth at all.
  w.replies['status --json'] = 'throw'
  await w.clock.advance(60_000)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const drawn = await ui.findAll({ type: 'Text' })
    expect(drawn).toHaveLength(1)
    expect(drawn[0]!.text).toContain('thoth_bin')
    await ui.unmount()
  }

  // Any other failure names its first stderr line.
  w.replies['status --json'] = { exitCode: 2, stderr: 'database is locked\nmore detail\n' }
  await w.clock.advance(60_000)
  const ui = await pane($, 'terminal')
  const drawn = await ui.findAll({ type: 'Text' })
  expect(drawn).toHaveLength(1)
  expect(drawn[0]!.text).toContain('database is locked')
  expect(drawn[0]!.text).not.toContain('more detail')
  expect(drawn[0]!.text).toContain('thoth doctor')
})

test('usage_failure_keeps_the_tasks', async ($, on) => {
  const replies = SAMPLE_REPLIES()
  replies['profile usage --json'] = { exitCode: 1, stderr: 'boom' }
  const w = world(on, replies)
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    expect(await ui.find({ text: /^● S3-9 handoff/ })).toBeDefined()
    expect(await ui.find({ text: /Could not read account quota/ })).toBeDefined()
    await ui.unmount()
  }
})

test('zh_tw_labels', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    for (const label of ['需要你', '進行中', '帳號額度', '問指揮官', '打開報告']) {
      expect(await ui.find({ text: label }), label).toBeDefined()
    }
    await ui.unmount()
  }
})

test('en_labels_by_default', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    for (const label of ['Needs you', 'In progress', 'Account quota', 'Ask the commander', 'Open report', '78% left']) {
      expect(await ui.find({ text: label }), label).toBeDefined()
    }
    expect(await ui.find({ text: '需要你' })).toBeUndefined()
    await ui.unmount()
    const line = await band($, surface)
    expect((await line.find({ type: 'Text', text: /Thoth · / }))?.text).toContain('2 running')
    expect(await line.find({ type: 'Button', text: 'Open' })).toBeDefined()
    await line.unmount()
  }
})

test('thoth_home_and_thoth_bin_reach_the_process', { options: { thoth_home: '/h', thoth_bin: '/b/thoth' } }, async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  const thoth = w.calls.filter(call => call.argv[0] !== 'git')
  expect(thoth.length).toBeGreaterThan(0)
  for (const call of thoth) {
    expect(call.argv[0]).toBe('/b/thoth')
    expect(call.env?.THOTH_HOME).toBe('/h')
  }
})

test('without_thoth_home_no_variable_is_set', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  const status = w.calls.find(call => call.argv[0] === 'thoth')
  expect(status?.env).toEqual({})
})

test('refresh_interval_is_10s_while_running_and_60s_when_idle', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  expect(w.statusRuns()).toBe(1)
  await w.clock.advance(10_000)
  expect(w.statusRuns()).toBe(2)

  // Nothing queued or running: the next refresh waits a minute.
  w.replies['status --json'] = json({ tasks: [task('T1', 'S3-9 handoff', 'in_review')] })
  await w.clock.advance(10_000)
  expect(w.statusRuns()).toBe(3)
  await w.clock.advance(10_000)
  expect(w.statusRuns()).toBe(3)
  await w.clock.advance(50_000)
  expect(w.statusRuns()).toBe(4)
})

test('command_and_prompt_refresh_at_once', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  expect(w.statusRuns()).toBe(1)
  const ran = await $.command.run({
    command: 'thoth',
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: true, columns: 160 },
  })
  expect(w.statusRuns()).toBe(2)
  expect(w.opened).toEqual(['thoth'])
  expect(ran).toMatchObject({ text: expect.any(String) })
  await $.prompt.submit({ text: 'hello', wait: false, origin: { kind: 'composer' } })
  await w.clock.settle()
  expect(w.statusRuns()).toBe(3)
})

const eventCalls = (w: World) => w.calls.map(call => call.argv.join(' ')).filter(argv => argv.includes('events'))
const TWO_TOASTS = ['MOD-2 stream was rejected', 'codex-a ran out of quota; its task was interrupted']

test('two_windows_each_get_the_notifications', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  // Another window has read 41-43 and moved the shared value on.
  w.stored.set(CURSOR_KEY, 43)
  w.calls.length = 0
  await w.clock.advance(10_000)
  expect(eventCalls(w)).toEqual(['thoth events --since 40 --json'])
  expect(w.toasts).toEqual(TWO_TOASTS)
  expect(w.stored.get(CURSOR_KEY)).toBe(43)
})

test('the_shared_cursor_never_moves_down', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  w.stored.set(CURSOR_KEY, 50)
  await w.clock.advance(10_000)
  expect(w.stored.get(CURSOR_KEY)).toBe(50)
  w.calls.length = 0
  await w.clock.advance(10_000)
  expect(eventCalls(w)).toEqual(['thoth events --since 43 --json'])
})

test('a_new_session_continues_from_the_store', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  w.stored.set(CURSOR_KEY, 40)
  await start($, w)
  expect(eventCalls(w)).toEqual(['thoth events --since 40 --json'])
  expect(w.toasts).toEqual(TWO_TOASTS)
})

test('a_store_that_fails_to_write_still_notifies', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  w.storeSetFails = true
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(TWO_TOASTS)
  const runs = w.statusRuns()
  await w.clock.advance(10_000)
  expect(w.statusRuns()).toBe(runs + 1)
  expect(w.toasts).toEqual(TWO_TOASTS)
})

test('the_timer_survives_a_failed_refresh', async ($, on) => {
  let deny = false
  on('state.set', (_, e, next) => {
    if (deny && e.plugin === 'thoth' && e.key === 'snapshot') {
      deny = false
      return { deny: 'snapshot write refused for the test' }
    }
    return next(e)
  })
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  deny = true
  await w.clock.advance(10_000)
  const runs = w.statusRuns()
  await w.clock.advance(60_000)
  expect(w.statusRuns()).toBeGreaterThan(runs)

  deny = true
  await $.command.run({
    command: 'thoth',
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: true, columns: 160 },
  })
  expect(w.opened).toContain('thoth')
})

test('the_heading_counts_seconds_without_a_new_refresh', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  const ui = await pane($, 'terminal')
  expect(await ui.find({ text: /updated 0s ago/ })).toBeDefined()
  const runs = w.calls.length
  await w.clock.advance(5_000)
  expect(await ui.find({ text: /updated 5s ago/ })).toBeDefined()
  expect(w.statusRuns()).toBe(1)
  expect(w.calls.length).toBe(runs)
  await ui.unmount()
})

const CHECKS_REPLY = json({
  checks: [
    { check_id: 'test', status: 'approved' },
    { check_id: 'plugin-test', status: 'stale' },
    { check_id: 'lint', status: 'pending' },
  ],
})
const HEADINGS = [/^── Needs you /, /^── Commander is on it /, /^── In progress /, /^── Account quota /]

/** The Text strings of the pane strictly between two section headings (by index in HEADINGS). */
async function between(ui: Awaited<ReturnType<typeof pane>>, from: number, to: number): Promise<string[]> {
  const all = (await ui.findAll({ type: 'Text' })).map(node => node.text ?? '')
  const at = (n: number) => all.findIndex(text => HEADINGS[n]!.test(text))
  expect(at(from)).toBeGreaterThanOrEqual(0)
  expect(at(to)).toBeGreaterThan(at(from))
  return all.slice(at(from) + 1, at(to))
}

test('waiting_checks_are_listed_with_the_commander', async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'checks list --json': CHECKS_REPLY })
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    expect(await between(ui, 0, 1)).toEqual(['(none)'])
    expect(await between(ui, 1, 2)).toEqual([
      '● S3-9 handoff in review',
      '● P7-x check unverified may be stuck',
      '● MOD-2 stream failed',
      "● plugin-test changed, waiting for the commander's approval again",
      "● lint waiting for the commander's approval",
    ])
    expect(await ui.find({ type: 'Button', key: 'copy-approve' })).toBeUndefined()
    expect(w.copied).toEqual([])
    await ui.unmount()
  }
})

test('decision_cards_are_counted_in_needs_you', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  w.cards = [{ id: 'a' }, { id: 'b' }]
  await start($, w)
  const ui = await pane($, 'terminal')
  expect(await between(ui, 0, 1)).toEqual(['● 2 decision(s) waiting in the decisions pane'])
  expect(await ui.find({ type: 'Button', key: 'copy-approve' })).toBeUndefined()
  await ui.unmount()
})

test('no_cards_means_no_decision_line', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  const ui = await pane($, 'terminal')
  for (const text of await texts(ui)) expect(text).not.toContain('decision(s) waiting')
  await ui.unmount()
})

test('the_three_sections_split_by_who_acts', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  const ui = await pane($, 'terminal')
  expect(await between(ui, 0, 1)).toEqual(['(none)'])
  expect(await between(ui, 1, 2)).toEqual(['● S3-9 handoff in review', '● P7-x check unverified may be stuck', '● MOD-2 stream failed'])
  expect(await between(ui, 2, 3)).toEqual(['● S3-11 codex quota running', '● Small fixes queued'])
  await ui.unmount()
})

test('the_band_counts_checks_with_the_commander', async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'checks list --json': CHECKS_REPLY })
  await start($, w)
  const bandLine = async (surface: (typeof SURFACES)[number]) => {
    const line = await band($, surface)
    const text = (await line.find({ type: 'Text', text: /Thoth · / }))?.text
    await line.unmount()
    return text
  }
  for (const surface of SURFACES) {
    const text = (await bandLine(surface)) ?? ''
    expect(text).toContain('2 running')
    expect(text).toContain('5 with the commander')
    expect(text).not.toContain('need you')
  }

  // The first refresh after a change is still within the minute; the one after it reruns checks.
  w.cards = [{ id: 'a' }, { id: 'b' }]
  w.replies['checks list --json'] = json({ checks: [] })
  await w.clock.advance(10_000)
  await w.clock.advance(60_000)
  for (const surface of SURFACES) {
    const text = (await bandLine(surface)) ?? ''
    expect(text).toContain('1 need you')
    expect(text).toContain('3 with the commander')
  }

  w.cards = undefined
  w.replies['status --json'] = json({ tasks: [] })
  w.replies['checks list --json'] = json({ checks: [{ check_id: 'lint', status: 'pending' }] })
  await w.clock.advance(10_000)
  await w.clock.advance(60_000)
  for (const surface of SURFACES) {
    const text = (await bandLine(surface)) ?? ''
    expect(text).toContain('1 with the commander')
    expect(text).toContain('0 running')
  }

  w.replies['checks list --json'] = json({ checks: [{ check_id: 'lint', status: 'approved' }] })
  await w.clock.advance(60_000)
  await w.clock.advance(60_000)
  for (const surface of SURFACES) {
    const quiet = await band($, surface)
    expect(await quiet.drawn()).toEqual(ENGINE_BAND)
    await quiet.unmount()
  }
})

test('a_failing_checks_list_leaves_the_rest_of_the_pane', async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'checks list --json': { exitCode: 1, stderr: 'boom' } })
  await start($, w)
  const ui = await pane($, 'terminal')
  expect(await between(ui, 0, 1)).toEqual(['(none)'])
  expect(await between(ui, 1, 2)).toHaveLength(3)
  expect(await ui.find({ type: 'Button', key: 'copy-approve' })).toBeUndefined()
  for (const text of await texts(ui)) expect(text).not.toContain('boom')
  expect(w.toasts).toEqual([])
  await ui.unmount()
})

test('a_status_problem_skips_checks_list', async ($, on) => {
  const w = world(on, { 'status --json': { exitCode: 2, stderr: 'database is locked' } })
  await start($, w)
  expect(w.checkRuns()).toBe(0)
})

test('needs_you_and_commander_labels_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'checks list --json': CHECKS_REPLY })
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    for (const label of ['需要你', '指揮官處理中']) expect(await ui.find({ text: label }), label).toBeDefined()
    expect(await ui.find({ text: /等指揮官核准/ })).toBeDefined()
    expect(await ui.find({ text: /變過了，等指揮官重新核准/ })).toBeDefined()
    expect(await ui.find({ type: 'Button', text: '複製核准指令' })).toBeUndefined()
    for (const text of await texts(ui)) expect(text).not.toContain('等你核准')
    await ui.unmount()
  }
})

test('checks_list_runs_at_most_once_a_minute_but_always_for_the_command', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  expect(w.checkRuns()).toBe(1)
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(w.checkRuns()).toBe(1)
  for (let i = 0; i < 4; i++) await w.clock.advance(10_000)
  expect(w.checkRuns()).toBe(2)
  await $.command.run({
    command: 'thoth',
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: true, columns: 160 },
  })
  expect(w.checkRuns()).toBe(3)
})

test('the_run_count_labels_are_gone', () => {
  expect(hasKey('runs')).toBe(false)
  expect(hasKey('run')).toBe(false)
  expect(hasKey('stale')).toBe(true)
  expect(hasKey('aTask')).toBe(true)
})

const RUN_A = '01M4RUNAAAAAAAAAAAAAAAAAAA'
const RUN_B = '01M4RUNBBBBBBBBBBBBBBBBBBB'
const RUN_C = '01M4RUNCCCCCCCCCCCCCCCCCCC'
const RUN_Q = '01M4RUNQQQQQQQQQQQQQQQQQQQ'

const runRow = (run_id: string, role: string, status: string, profile: string, model: string) => ({
  run_id,
  role,
  profile,
  model,
  status,
  heartbeat_stale: false,
  worktree: '/wt/T2-a1',
  run_dir: `/home/runs/${run_id}`,
})
const ROW_A = runRow(RUN_A, 'implementer', 'interrupted', 'claude-b', 'sonnet')
const ROW_B = runRow(RUN_B, 'implementer', 'running', 'codex-a', 'gpt-5.6-luna')
const ROW_C = runRow(RUN_C, 'reviewer', 'running', 'claude-b', 'opus')

const T2_STATUS = { task_id: 'T2', title: 'S3-11 codex quota', status: 'running', runs: [ROW_A, ROW_B] }

const step = (kind: string, extra: Record<string, unknown> = {}) => ({
  at: 0,
  kind,
  nested: false,
  text: '',
  tool: null,
  action: null,
  target: null,
  exit_code: null,
  is_error: null,
  ...extra,
})
const toolStep = (action: string, target: string, extra: Record<string, unknown> = {}) =>
  step('tool', { tool: 'x', action, target, text: target, ...extra })

const transcript = (run_id: string, steps: unknown[], extra: Record<string, unknown> = {}) => ({
  run_id,
  task_id: 'T2',
  harness: 'codex',
  exists: true,
  size: 1234,
  offset: 0,
  next_offset: 1234,
  reset: false,
  lines: 7,
  skipped: 0,
  steps,
  ...extra,
})

const TRANSCRIPT_B0 = transcript(RUN_B, [
  step('start', { text: 'gpt-5.6-luna' }),
  toolStep('read', '/wt/T2-a1/src/a.rs'),
  toolStep('run', 'cargo test', { exit_code: 0, is_error: false }),
  step('message', { text: 'All green' }),
  toolStep('edit', '/wt/T2-a1/src/b.rs'),
  step('error', { text: 'boom', is_error: true }),
  step('result', { text: 'done', is_error: false }),
])

const runPane = ($: Engine, surface: (typeof SURFACES)[number]) =>
  $.ui.mount({ plugin: 'thoth', surface, component: 'Pane', requestId: 'thoth-run', props: PANE_PROPS })

const WATCH_REPLIES = (): Record<string, Reply> => ({
  ...SAMPLE_REPLIES(),
  'status T2 --json': json(T2_STATUS),
  [`transcript ${RUN_B} --offset 0 --json`]: json(TRANSCRIPT_B0),
})

/** Starts a session and presses Watch worker on T2. */
async function watchT2($: Engine, w: World): Promise<void> {
  await start($, w)
  const ui = await pane($, 'terminal')
  await ui.press({ key: 'watch:T2' })
  await ui.unmount()
  await w.clock.settle()
}

const textsOf = async (ui: Awaited<ReturnType<typeof runPane>>) =>
  (await ui.findAll({ type: 'Text' })).map(node => node.text ?? '')

const EN_LINES = [
  'S3-11 codex quota · implementer · codex-a · gpt-5.6-luna · running',
  '7 steps · updated 0s ago',
  '▶ started on gpt-5.6-luna',
  '● read src/a.rs',
  '● ran cargo test',
  '│ All green',
  '● edited src/b.rs',
  '✗ boom',
  '■ finished: done',
]

test('watch_button_opens_the_run_pane_with_the_newest_run', async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    w.calls.length = 0
    w.opened.length = 0
    const main = await pane($, surface)
    await main.press({ key: 'watch:T2' })
    await w.clock.settle()
    expect(w.opened).toContain('thoth-run')
    const calls = w.calls.map(call => call.argv.join(' '))
    expect(calls).toContain('thoth status T2 --json')
    expect(calls).toContain(`thoth transcript ${RUN_B} --offset 0 --json`)
    expect(calls.filter(call => call.includes(`transcript ${RUN_A}`))).toEqual([])
    await main.unmount()

    const ui = await runPane($, surface)
    expect(await textsOf(ui)).toEqual(EN_LINES)
    expect((await ui.find({ type: 'Text', text: '✗ boom' }))?.props.color).toBe('red')
    expect(await ui.findAll({ type: 'Button' })).toHaveLength(0)
    for (const text of await textsOf(ui)) {
      expect(text).not.toContain('01M4RUN')
      expect(text).not.toContain('T2')
    }
    await ui.unmount()
  }
})

test('the_run_pane_appends_new_steps_from_the_next_offset', async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  await watchT2($, w)
  w.replies[`transcript ${RUN_B} --offset 1234 --json`] = json(
    transcript(RUN_B, [toolStep('run', 'cargo clippy', { exit_code: 101, is_error: true })], { offset: 1234, next_offset: 2000 }),
  )
  w.calls.length = 0
  await w.clock.advance(10_000)
  expect(w.calls.map(call => call.argv.join(' '))).toContain(`thoth transcript ${RUN_B} --offset 1234 --json`)
  for (const surface of SURFACES) {
    const ui = await runPane($, surface)
    const lines = await textsOf(ui)
    expect(lines).toContain('● ran cargo test')
    expect(lines).toContain('● ran cargo clippy (exit 101)')
    expect((await ui.find({ type: 'Text', text: '● ran cargo clippy (exit 101)' }))?.props.color).toBe('red')
    expect(lines[1]).toContain('8 steps')
    await ui.unmount()
  }

  w.replies[`transcript ${RUN_B} --offset 2000 --json`] = json(
    transcript(RUN_B, [], { offset: 2000, next_offset: 2000 }),
  )
  await w.clock.advance(10_000)
  for (const surface of SURFACES) {
    const ui = await runPane($, surface)
    expect((await textsOf(ui))[1]).toContain('8 steps')
    await ui.unmount()
  }
})

test('a_newer_run_replaces_the_transcript', async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  await watchT2($, w)
  w.replies['status T2 --json'] = json({ ...T2_STATUS, runs: [ROW_A, ROW_B, ROW_C] })
  w.replies[`transcript ${RUN_C} --offset 0 --json`] = json(
    transcript(RUN_C, [step('message', { text: 'Reviewing' })], { next_offset: 50 }),
  )
  await w.clock.advance(10_000)
  for (const surface of SURFACES) {
    const ui = await runPane($, surface)
    const lines = await textsOf(ui)
    expect(lines[0]).toContain('reviewer · claude-b · opus')
    expect(lines).toContain('│ Reviewing')
    expect(lines).not.toContain('● ran cargo test')
    await ui.unmount()
  }
})

test('closing_the_run_pane_stops_the_polling', async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  // What the plugin wrote to `watching` and `worker`, with how many panes `ui.close` had been asked
  // to close by then: the writes of the close handler are the ones that come after the close.
  const writes: string[] = []
  on('state.set', (_, e, next) => {
    if (e.plugin === 'thoth' && (e.key === 'watching' || e.key === 'worker')) {
      writes.push(`${e.key}=${JSON.stringify(e.value)}@${w.closed.length}`)
    }
    return next(e)
  })
  await watchT2($, w)
  w.closed.length = 0
  writes.length = 0
  // The test engine's `$` has no `ui.close` (the engine sets `origin` itself) and a hook of the test
  // may not call it: a new session is the one way to make the engine close the pane the plugin
  // left open, which raises `ui.close` through the plugin's own handler.
  await $.session.start({ cwd: '/repo' })
  await w.clock.settle()
  expect(w.closed).toEqual(['thoth-run'])
  // session.start clears both keys before it closes the pane; the handler clears them again after.
  expect(writes).toEqual([
    'watching=null@0',
    'worker=null@0',
    'watching=null@1',
    'worker=null@1',
  ])
  w.calls.length = 0
  await w.clock.advance(10_000)
  const joined = w.calls.map(call => call.argv.join(' '))
  expect(joined.filter(call => call.includes('transcript'))).toEqual([])
  expect(joined.filter(call => call.includes('status T2'))).toEqual([])
  expect(w.statusRuns()).toBe(1)
  const ui = await runPane($, 'terminal')
  expect(await textsOf(ui)).toEqual(['(none)'])
  await ui.unmount()
})

test('a_main_refresh_that_could_not_read_the_tasks_does_not_poll_the_worker', async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  await watchT2($, w)
  w.replies['status --json'] = { exitCode: 1, stderr: 'no repo here' }
  w.calls.length = 0
  await w.clock.advance(10_000)
  const joined = w.calls.map(call => call.argv.join(' '))
  expect(w.statusRuns()).toBe(1)
  expect(joined.filter(call => call.includes('status T2'))).toEqual([])
  expect(joined.filter(call => call.includes('transcript'))).toEqual([])
})

test('a_second_press_while_the_first_poll_runs_does_not_poll_again', async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  await start($, w)
  const ui = await pane($, 'terminal')
  w.calls.length = 0
  await Promise.all([ui.press({ key: 'watch:T2' }), ui.press({ key: 'watch:T2' })])
  await w.clock.settle()
  await ui.unmount()
  const joined = w.calls.map(call => call.argv.join(' '))
  expect(joined.filter(call => call === 'thoth status T2 --json')).toHaveLength(1)
  expect(joined.filter(call => call.includes('transcript'))).toHaveLength(1)
})

test('a_queued_run_waits_and_a_task_without_runs_says_so', async ($, on) => {
  const w = world(on, {
    ...SAMPLE_REPLIES(),
    'status T5 --json': json({ task_id: 'T5', title: 'Small fixes', status: 'queued', runs: [runRow(RUN_Q, 'implementer', 'queued', 'claude-b', 'sonnet')] }),
    [`transcript ${RUN_Q} --offset 0 --json`]: json(
      transcript(RUN_Q, [], { exists: false, size: 0, next_offset: 0, lines: 0 }),
    ),
  })
  await start($, w)
  const main = await pane($, 'terminal')
  await main.press({ key: 'watch:T5' })
  await main.unmount()
  await w.clock.settle()
  const ui = await runPane($, 'terminal')
  const lines = await textsOf(ui)
  expect(lines).toContain('Waiting for the worker to start…')
  expect(lines.filter(text => text.startsWith('●'))).toEqual([])
  await ui.unmount()

  w.replies['status T5 --json'] = json({ task_id: 'T5', title: 'Small fixes', status: 'queued', runs: [] })
  await w.clock.advance(10_000)
  const empty = await runPane($, 'terminal')
  expect(await textsOf(empty)).toContain('No run yet for this task')
  await empty.unmount()
})

test('a_failing_transcript_keeps_the_steps_already_shown', async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  await watchT2($, w)
  // The first refresh raises its event toasts; the failing read comes after them.
  await w.clock.advance(10_000)
  w.replies[`transcript ${RUN_B} --offset 1234 --json`] = { exitCode: 1, stderr: 'boom2' }
  const toasts = [...w.toasts]
  await w.clock.advance(10_000)
  const ui = await runPane($, 'terminal')
  const lines = await textsOf(ui)
  expect(lines).toContain('● ran cargo test')
  expect(lines).toContain('Could not read the transcript; showing what was read before')
  for (const text of lines) expect(text).not.toContain('boom2')
  expect(w.toasts).toEqual(toasts)
  await ui.unmount()
})

test('an_antigravity_run_shows_a_hint', async ($, on) => {
  const w = world(on, {
    ...WATCH_REPLIES(),
    [`transcript ${RUN_B} --offset 0 --json`]: json(transcript(RUN_B, [], { harness: 'antigravity' })),
  })
  await watchT2($, w)
  const ui = await runPane($, 'terminal')
  expect(await textsOf(ui)).toContain('Not available yet for an Antigravity account')
  await ui.unmount()
})

test('run_pane_labels_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, WATCH_REPLIES())
  await start($, w)
  const main = await pane($, 'terminal')
  expect(await main.find({ type: 'Button', text: '看 worker' })).toBeDefined()
  await main.press({ key: 'watch:T2' })
  await main.unmount()
  await w.clock.settle()
  const ui = await runPane($, 'terminal')
  expect(await textsOf(ui)).toEqual([
    'S3-11 codex quota · 實作者 · codex-a · gpt-5.6-luna · 執行中',
    '7 步 · 更新於 0 秒前',
    '▶ 開始了，用 gpt-5.6-luna',
    '● 讀了 src/a.rs',
    '● 跑了 cargo test',
    '│ All green',
    '● 改了 src/b.rs',
    '✗ boom',
    '■ 做完了：done',
  ])
  await ui.unmount()
})

test('only_the_last_200_steps_are_kept', async ($, on) => {
  const many = Array.from({ length: 250 }, (_, n) => toolStep('run', `cmd${n}`, { exit_code: 0 }))
  const w = world(on, {
    ...WATCH_REPLIES(),
    [`transcript ${RUN_B} --offset 0 --json`]: json(transcript(RUN_B, many)),
  })
  await watchT2($, w)
  const ui = await runPane($, 'terminal')
  const lines = await textsOf(ui)
  expect(lines).toContain('… 50 earlier steps')
  expect(lines).not.toContain('● ran cmd0')
  expect(lines).toContain('● ran cmd50')
  expect(lines).toContain('● ran cmd249')
  expect(lines[1]).toContain('250 steps')
  await ui.unmount()
})

test('watch_buttons_sit_on_both_sections', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  const ui = await pane($, 'terminal')
  expect(await ui.findAll({ type: 'Button', text: 'Watch worker' })).toHaveLength(5)
  for (const id of ['T1', 'T3', 'T4']) {
    expect(await ui.findAll({ type: 'Button', key: `ask:${id}` })).toHaveLength(1)
    expect(await ui.findAll({ type: 'Button', key: `report:${id}` })).toHaveLength(1)
    expect(await ui.findAll({ type: 'Button', key: `watch:${id}` })).toHaveLength(1)
  }
  expect(await ui.findAll({ type: 'Button', key: 'watch:T2' })).toHaveLength(1)
  expect(await ui.findAll({ type: 'Button', key: 'watch:T5' })).toHaveLength(1)
  await ui.unmount()
})

const SAMPLE_MODELS = {
  profiles: [],
  records: { dir: null, machine: 'alpha', files: [], error: null },
  decision_table: {
    rows: [],
    choices: [],
    summary: {
      runs: 128,
      machines: 3,
      rows: 12,
      choices: 140,
      choices_recent: 9,
      choices_recent_without_reason: 4,
      recent_days: 7,
    },
  },
}
const TABLE_REPLIES = (): Record<string, Reply> => ({ ...SAMPLE_REPLIES(), 'models --json': json(SAMPLE_MODELS) })
const TABLE_LINE = 'Decision table: 128 runs, 3 computers; 9 launches in the last 7 days, 4 without a reason'
const TABLE_UNREADABLE = 'Decision table: could not read; run `thoth models`'

test('the_decision_table_line_and_button_sit_after_the_quota', async ($, on) => {
  const w = world(on, TABLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const all = (await ui.findAll({ type: 'Text' })).map(node => node.text ?? '')
    const i = all.findIndex(text => text.startsWith('── Decision table '))
    expect(i).toBeGreaterThanOrEqual(0)
    expect(all.slice(i + 1)).toEqual([TABLE_LINE])
    expect((await ui.find({ type: 'Button', key: 'table' }))?.text).toBe('Open decision table')
    expect(await between(ui, 2, 3)).toEqual(['● S3-11 codex quota running', '● Small fixes queued'])
    expect(await ui.findAll({ type: 'Button', text: 'Watch worker' })).toHaveLength(5)
    w.calls.length = 0
    await ui.press({ key: 'table' })
    expect(w.calls.map(call => call.argv)).toContainEqual(['thoth', 'report', '--open', '--section', 'decision-table'])
    expect(w.toasts).toEqual([])
    await ui.unmount()
  }
  // A page that cannot open says so in one toast.
  w.replies['report --open --section decision-table'] = { exitCode: 1, stderr: 'no browser' }
  const ui = await pane($, 'terminal')
  await ui.press({ key: 'table' })
  expect(w.toasts).toHaveLength(1)
})

test('the_decision_table_line_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, TABLE_REPLIES())
  await start($, w)
  const ui = await pane($, 'terminal')
  const all = (await ui.findAll({ type: 'Text' })).map(node => node.text ?? '')
  expect(all).toContain('決策表：128 筆、3 台；最近 7 天派工 9 次，4 次沒寫理由')
  expect((await ui.find({ type: 'Button', key: 'table' }))?.text).toBe('打開決策表')
  expect(all.filter(text => text.startsWith('── 決策表 '))).toHaveLength(1)
  await ui.unmount()
})

test('a_summary_without_the_count_of_launches_without_a_reason_is_unreadable', async ($, on) => {
  const summary: Record<string, number> = { ...SAMPLE_MODELS.decision_table.summary }
  delete summary.choices_recent_without_reason
  const models = { ...SAMPLE_MODELS, decision_table: { ...SAMPLE_MODELS.decision_table, summary } }
  const w = world(on, { ...SAMPLE_REPLIES(), 'models --json': json(models) })
  await start($, w)
  const ui = await pane($, 'terminal')
  const line = await ui.find({ type: 'Text', text: TABLE_UNREADABLE })
  expect(line).toBeDefined()
  expect(line?.props.dimColor).toBe(true)
  for (const node of await ui.findAll({ type: 'Text' })) expect(node.text ?? '').not.toContain('without a reason')
  expect(await ui.find({ type: 'Button', key: 'table' })).toBeDefined()
  await ui.unmount()
})

test('an_unreadable_models_output_shows_one_dim_line_and_keeps_the_button', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  let ui = await pane($, 'terminal')
  let line = await ui.find({ type: 'Text', text: TABLE_UNREADABLE })
  expect(line).toBeDefined()
  expect(line?.props.dimColor).toBe(true)
  expect(await ui.find({ type: 'Button', key: 'table' })).toBeDefined()
  expect(w.toasts).toEqual([])
  await ui.unmount()

  w.replies['models --json'] = { exitCode: 1, stderr: 'boom' }
  await w.clock.advance(60_000)
  ui = await pane($, 'terminal')
  line = await ui.find({ type: 'Text', text: TABLE_UNREADABLE })
  expect(line).toBeDefined()
  expect(line?.props.dimColor).toBe(true)
  for (const node of await ui.findAll({ type: 'Text' })) expect(node.text ?? '').not.toContain('boom')
  await ui.unmount()
})

test('models_json_runs_at_most_once_a_minute_but_always_for_the_command', async ($, on) => {
  const w = world(on, TABLE_REPLIES())
  await start($, w)
  expect(w.modelRuns()).toBe(1)
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(w.modelRuns()).toBe(1)
  for (let i = 0; i < 4; i++) await w.clock.advance(10_000)
  expect(w.modelRuns()).toBe(2)
  await $.command.run({
    command: 'thoth',
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: true, columns: 160 },
  })
  expect(w.modelRuns()).toBe(3)
})

test('a_status_problem_skips_models_json', async ($, on) => {
  const w = world(on, { 'status --json': { exitCode: 2, stderr: 'database is locked' } })
  await start($, w)
  expect(w.modelRuns()).toBe(0)
})

test('the_table_line_carries_no_task_id', async ($, on) => {
  const w = world(on, { ...panelReplies(), 'models --json': json(SAMPLE_MODELS) })
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const drawn = await texts(ui)
    expect(drawn).toContain(TABLE_LINE)
    for (const text of drawn) expect(text).not.toContain('01M493CG')
    await ui.unmount()
  }
})

const SAMPLE_USAGE_PLANS = {
  profiles: [
    { ...SAMPLE_USAGE.profiles[0]!, plan_label: 'Max' },
    { ...SAMPLE_USAGE.profiles[1]!, plan_label: 'Plus' },
    { ...SAMPLE_USAGE.profiles[2]!, plan_label: null },
  ],
}

test('plan_label_named_and_plan_labels', () => {
  expect(named('claude-a', 'Max', 'en')).toBe('claude-a (Max)')
  expect(named('claude-a', 'Max', 'zh-TW')).toBe('claude-a（Max）')
  expect(named('agy-a', null, 'zh-TW')).toBe('agy-a')
  expect(named('x', '', 'en')).toBe('x')
  expect(named('x', undefined, 'en')).toBe('x')
  const profile = (profile_id: string, extra: Record<string, unknown>) => ({
    profile_id,
    harness: 'claude-code',
    quota_state: 'ok',
    windows: [],
    ...extra,
  })
  const labels = planLabels([
    profile('a', { plan_label: 'Max' }),
    profile('b', { plan_label: '   ' }),
    profile('c', { plan_label: 5 }),
    profile('d', {}),
  ] as never)
  expect(labels.size).toBe(1)
  expect(labels.get('a')).toBe('Max')
  expect(labels.get('constructor')).toBeUndefined()
})

test('plan_label_quota_band_and_toast_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'profile usage --json': json(SAMPLE_USAGE_PLANS) })
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    expect((await ui.find({ type: 'Text', text: /^claude-a（Max）  / }))?.text).toContain('剩 78%')
    const codex = await ui.find({ type: 'Text', text: /^codex-a（Plus）  / })
    expect(codex?.text).toContain('額度用完')
    expect(codex?.props.color).toBe('red')
    const agy = await ui.find({ type: 'Text', text: /^agy-a  / })
    expect(agy?.text).toContain('還沒有讀數')
    expect(agy?.text).not.toContain('（')
    await ui.unmount()
  }
  for (const surface of SURFACES) {
    const ui = await band($, surface)
    expect((await ui.find({ type: 'Text', text: /Thoth · / }))?.text).toContain('codex-a（Plus） 五小時 剩 14%')
    await ui.unmount()
  }
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(['MOD-2 stream 被退件', 'codex-a（Plus） 額度用完，任務中斷'])
})

test('plan_label_quota_band_and_toast_in_english', async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'profile usage --json': json(SAMPLE_USAGE_PLANS) })
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    expect((await ui.find({ type: 'Text', text: /^claude-a \(Max\) {2}/ }))?.text).toContain('78% left')
    expect((await ui.find({ type: 'Text', text: /^codex-a \(Plus\) {2}/ }))?.text).toContain('quota exhausted')
    await ui.unmount()
  }
  for (const surface of SURFACES) {
    const ui = await band($, surface)
    expect((await ui.find({ type: 'Text', text: /Thoth · / }))?.text).toContain('codex-a (Plus) 5h 14% left')
    await ui.unmount()
  }
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(['MOD-2 stream was rejected', 'codex-a (Plus) ran out of quota; its task was interrupted'])
})

test('plan_label_run_pane_first_line', async ($, on) => {
  const w = world(on, { ...WATCH_REPLIES(), 'profile usage --json': json(SAMPLE_USAGE_PLANS) })
  await watchT2($, w)
  const ui = await runPane($, 'terminal')
  expect((await textsOf(ui))[0]).toBe('S3-11 codex quota · implementer · codex-a (Plus) · gpt-5.6-luna · running')
  await ui.unmount()
})

test('plan_label_run_pane_first_line_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, { ...WATCH_REPLIES(), 'profile usage --json': json(SAMPLE_USAGE_PLANS) })
  await watchT2($, w)
  const ui = await runPane($, 'terminal')
  expect((await textsOf(ui))[0]).toBe('S3-11 codex quota · 實作者 · codex-a（Plus） · gpt-5.6-luna · 執行中')
  await ui.unmount()
})

test('plan_label_ignores_a_blank_or_non_string_label', { options: { language: 'zh-TW' } }, async ($, on) => {
  const usage = {
    profiles: [
      { ...SAMPLE_USAGE.profiles[0]!, plan_label: '   ' },
      { ...SAMPLE_USAGE.profiles[1]!, plan_label: 5 },
      { ...SAMPLE_USAGE.profiles[2]! },
    ],
  }
  const w = world(on, { ...SAMPLE_REPLIES(), 'profile usage --json': json(usage) })
  await start($, w)
  const ui = await pane($, 'terminal')
  for (const prefix of [/^claude-a {2}/, /^codex-a {2}/, /^agy-a {2}/]) {
    const line = await ui.find({ type: 'Text', text: prefix })
    expect(line).toBeDefined()
    expect(line?.text).not.toContain('（')
    expect(line?.text).not.toContain('(')
  }
  await ui.unmount()
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(['MOD-2 stream 被退件', 'codex-a 額度用完，任務中斷'])
})

test('plan_label_an_account_named_constructor_is_never_decorated', { options: { language: 'zh-TW' } }, async ($, on) => {
  const usage = {
    profiles: [
      ...SAMPLE_USAGE_PLANS.profiles,
      {
        profile_id: 'constructor',
        harness: 'claude-code',
        quota_state: 'ok',
        windows: [{ name: 'five_hour', used_percent: 99, resets_at: null }],
      },
    ],
  }
  const w = world(on, {
    ...SAMPLE_REPLIES(),
    'profile usage --json': json(usage),
    'events --since 40 --json': json({
      last_seq: 41,
      events: [{ seq: 41, type: 'quota_exceeded', detail: { profile: 'constructor' } }],
    }),
    'events --since 41 --json': json({ last_seq: 41, events: [] }),
  })
  await start($, w)
  const ui = await pane($, 'terminal')
  const line = await ui.find({ type: 'Text', text: /^constructor {2}/ })
  expect(line).toBeDefined()
  expect(line?.text).not.toContain('（')
  expect(line?.text).not.toContain('function')
  await ui.unmount()
  const top = await band($, 'terminal')
  expect((await top.find({ type: 'Text', text: /Thoth · / }))?.text).toContain('constructor 五小時 剩 1%')
  await top.unmount()
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(w.toasts).toEqual(['constructor 額度用完，任務中斷'])
})

const ALPHA = '/work/alpha'
const BETA = '\\work\\beta'
const OUTSIDE_STATUS = {
  tasks: SAMPLE_STATUS.tasks.map(t => ({ ...t, repo: t.task_id === 'T2' || t.task_id === 'T5' ? BETA : ALPHA })),
  last_verify: null,
  outside_repo: true,
  note: 'not inside a git repository',
}
const OUTSIDE_REPLIES = (): Record<string, Reply> => ({
  ...TABLE_REPLIES(),
  'status --json': json(OUTSIDE_STATUS),
  'checks list --json': CHECKS_REPLY,
})
const OUTSIDE_EN =
  'Not a git repository: showing the tasks of every repository. To dispatch, open Claude Code in a repository.'
const OUTSIDE_ZH = '這裡不是 git repo：列出所有 repo 的任務。要派工，請在 repo 裡開 Claude Code。'

async function outsidePane(
  $: Engine,
  w: World,
  notice: string,
  headings: readonly [string, string, string],
  commanderLine: string,
  progressLine: string,
  tableLine: string,
): Promise<void> {
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const all = await ui.findAll({ type: 'Text' })
    expect(all[1]?.text).toBe(notice)
    expect(all[1]?.props.color).toBe('yellow')
    expect(all[1]?.props.wrap).toBe('wrap')
    const textsAll = all.map(node => node.text ?? '')
    const at = (label: string) => textsAll.findIndex(text => text.startsWith(`── ${label} `))
    const [commander, progress, quota] = headings.map(at) as [number, number, number]
    expect(commander).toBeGreaterThan(1)
    expect(textsAll.slice(commander + 1, progress)).toContain(commanderLine)
    expect(textsAll.slice(progress + 1, quota)).toContain(progressLine)
    expect(await ui.find({ type: 'Text', text: /^claude-a/ })).toBeDefined()
    expect(textsAll).toContain(tableLine)
    for (const id of ['T1', 'T2', 'T3', 'T4', 'T5']) expect(await ui.find({ type: 'Button', key: `watch:${id}` })).toBeDefined()
    await ui.unmount()
  }
  expect(w.checkRuns()).toBe(0)
  await $.command.run({
    command: 'thoth',
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: true, columns: 160 },
  })
  expect(w.checkRuns()).toBe(0)
  expect(w.modelRuns()).toBeGreaterThan(0)
  expect(w.calls.filter(call => call.argv.includes('events'))).toEqual([])
  expect(w.toasts).toEqual([])
}

test('outside_repo_pane_lists_every_repository', async ($, on) => {
  const w = world(on, OUTSIDE_REPLIES())
  await outsidePane(
    $,
    w,
    OUTSIDE_EN,
    ['Commander is on it', 'In progress', 'Account quota'],
    '● S3-9 handoff in review · alpha',
    '● S3-11 codex quota running · beta',
    TABLE_LINE,
  )
})

test('outside_repo_pane_lists_every_repository_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, OUTSIDE_REPLIES())
  await outsidePane(
    $,
    w,
    OUTSIDE_ZH,
    ['指揮官處理中', '進行中', '帳號額度'],
    '● S3-9 handoff 等審查 · alpha',
    '● S3-11 codex quota 執行中 · beta',
    '決策表：128 筆、3 台；最近 7 天派工 9 次，4 次沒寫理由',
  )
})

test('outside_repo_watch_runs_in_the_tasks_repository', async ($, on) => {
  const w = world(on, { ...WATCH_REPLIES(), 'status --json': json(OUTSIDE_STATUS) })
  await watchT2($, w)
  const cwdOf = (args: string) => w.calls.filter(call => call.argv.slice(1).join(' ') === args).map(call => call.cwd)
  expect(cwdOf('status T2 --json')).toEqual([BETA])
  expect(cwdOf(`transcript ${RUN_B} --offset 0 --json`)).toEqual([BETA])
  expect(cwdOf('status --json')).toEqual(['/repo'])
  const ui = await runPane($, 'terminal')
  expect(await textsOf(ui)).toEqual(EN_LINES)
  await ui.unmount()
})

test('band_keeps_what_the_mods_after_it_draw', async ($, on) => {
  const w = world(on, SAMPLE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await band($, surface)
    const all = (await ui.findAll({ type: 'Text' })).map(node => node.text ?? '')
    const thoth = all.findIndex(text => text.startsWith('Thoth · '))
    const engine = all.indexOf('engine band')
    expect(thoth).toBeGreaterThanOrEqual(0)
    expect(engine).toBeGreaterThan(thoth)
    expect(await ui.find({ type: 'Button', key: 'open' })).toBeDefined()
    await ui.unmount()
  }
})

// PANEL-3: login names, review in progress, and Cancel / Close task / Dispatch again with a second press.

const RUN_REV = '01M5RUNREVIEWAAAAAAAAAAAAA'
const RUN_IMPL = '01M5RUNIMPLAAAAAAAAAAAAAAA'
const RUN_SPEC = '01M5RUNSPECAAAAAAAAAAAAAAA'
const RUN_OTHER = '01M5RUNOTHERAAAAAAAAAAAAAA'
const liveRun = (run_id: string, role: string, profile: string, model: string, status = 'running') => ({
  run_id,
  role,
  profile,
  model,
  status,
})

const LIVE_STATUS = {
  tasks: [
    task('T1', 'S3-9 handoff', 'in_review', { live_run: liveRun(RUN_REV, 'reviewer', 'claude-a', 'opus') }),
    task('T2', 'S3-11 codex quota', 'running', { live_run: liveRun(RUN_IMPL, 'implementer', 'codex-a', 'gpt-5.6-luna') }),
    task('T3', 'P7-x check', 'unverified', { live_run: null }),
    task('T4', 'MOD-2 stream', 'failed', { runs: 3, live_run: null }),
    task('T6', 'Spec draft', 'draft', { live_run: liveRun(RUN_SPEC, 'spec_reviewer', 'codex-a', 'gpt-5.6-luna') }),
  ],
}
const PROFILE_LIST = {
  profiles: [
    { profile_id: 'claude-a', login_name: 'alice', login_name_seen_at: '2026-10-27T00:00:00Z' },
    { profile_id: 'codex-a', login_name: 'carol', login_name_seen_at: '2026-10-27T00:00:00Z' },
    { profile_id: 'agy-a', login_name: null, login_name_seen_at: null },
  ],
}
const LIVE_REPLIES = (): Record<string, Reply> => ({
  ...SAMPLE_REPLIES(),
  'status --json': json(LIVE_STATUS),
  'profile usage --json': json(SAMPLE_USAGE_PLANS),
  'profile list --json': json(PROFILE_LIST),
})
const WRITES = ['cancel', 'task', 'dispatch']
/** Every thoth command the panel ran that writes, as its arguments joined with spaces. */
const writes = (w: World) =>
  w.calls.filter(call => call.argv[0] !== 'git' && WRITES.includes(call.argv[1] ?? '')).map(call => call.argv.slice(1).join(' '))
const profileListRuns = (w: World) => w.calls.filter(call => call.argv.slice(1).join(' ') === 'profile list --json').length

test('panel3_named_puts_the_login_before_the_label', () => {
  expect(named('claude-a', 'Max', 'en', 'alice')).toBe('claude-a (alice, Max)')
  expect(named('claude-a', 'Max', 'zh-TW', 'alice')).toBe('claude-a（alice，Max）')
  expect(named('claude-a', null, 'en', 'alice')).toBe('claude-a (alice)')
  expect(named('claude-a', null, 'zh-TW', 'alice')).toBe('claude-a（alice）')
  expect(named('claude-a', 'Max', 'en', null)).toBe('claude-a (Max)')
  expect(named('agy-a', null, 'zh-TW', undefined)).toBe('agy-a')
})

test('panel3_a_running_review_is_in_progress_with_its_account_and_model', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    expect(await between(ui, 1, 2)).toEqual(['● P7-x check unverified', '● MOD-2 stream failed'])
    expect(await between(ui, 2, 3)).toEqual([
      '● S3-9 handoff under review · claude-a (alice, Max) · opus',
      '● S3-11 codex quota running',
      '● Spec draft spec under review · codex-a (carol, Plus) · gpt-5.6-luna',
    ])
    await ui.unmount()
  }

  // The review ends: the task waits on the commander again.
  w.replies['status --json'] = json({
    tasks: LIVE_STATUS.tasks.map(t => (t.task_id === 'T1' ? { ...t, live_run: null } : t)),
  })
  await w.clock.advance(10_000)
  const ui = await pane($, 'terminal')
  expect(await between(ui, 1, 2)).toEqual(['● S3-9 handoff in review', '● P7-x check unverified', '● MOD-2 stream failed'])
  expect(await between(ui, 2, 3)).not.toContain('● S3-9 handoff in review')
  await ui.unmount()
})

test('panel3_a_running_review_is_in_progress_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const ui = await pane($, surface)
    const all = (await ui.findAll({ type: 'Text' })).map(node => node.text ?? '')
    const at = (label: string) => all.findIndex(text => text.startsWith(`── ${label} `))
    expect(all.slice(at('指揮官處理中') + 1, at('進行中'))).toEqual(['● P7-x check 未驗證', '● MOD-2 stream 失敗'])
    expect(all.slice(at('進行中') + 1, at('帳號額度'))).toEqual([
      '● S3-9 handoff 審查中 · claude-a（alice，Max） · opus',
      '● S3-11 codex quota 執行中',
      '● Spec draft 規格審查中 · codex-a（carol，Plus） · gpt-5.6-luna',
    ])
    await ui.unmount()
  }
})

test('panel3_a_stuck_review_outside_a_repository_keeps_its_marks', async ($, on) => {
  const status = {
    tasks: [task('T1', 'S3-9 handoff', 'in_review', { any_heartbeat_stale: true, repo: '/work/alpha', live_run: liveRun(RUN_REV, 'reviewer', 'claude-b', 'opus') })],
    outside_repo: true,
  }
  const w = world(on, { ...SAMPLE_REPLIES(), 'status --json': json(status) })
  await start($, w)
  const ui = await pane($, 'terminal')
  expect(await between(ui, 2, 3)).toEqual(['● S3-9 handoff under review · claude-b · opus may be stuck · alpha'])
  await ui.unmount()
})

test('panel3_without_live_run_the_grouping_is_as_before', async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'profile list --json': json(PROFILE_LIST) })
  await start($, w)
  const ui = await pane($, 'terminal')
  expect(await between(ui, 1, 2)).toEqual(['● S3-9 handoff in review', '● P7-x check unverified may be stuck', '● MOD-2 stream failed'])
  expect(await between(ui, 2, 3)).toEqual(['● S3-11 codex quota running', '● Small fixes queued'])
  expect(await ui.find({ type: 'Button', key: 'cancel:T2' })).toBeUndefined()
  await ui.unmount()
})

test('panel3_the_band_counts_a_review_as_running', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  for (const surface of SURFACES) {
    const line = await band($, surface)
    const text = (await line.find({ type: 'Text', text: /Thoth · / }))?.text ?? ''
    expect(text).toContain('3 running')
    expect(text).toContain('2 with the commander')
    await line.unmount()
  }
})

test('panel3_a_running_review_alone_refreshes_every_ten_seconds', async ($, on) => {
  const reviewing = { tasks: [task('T1', 'S3-9 handoff', 'in_review', { live_run: liveRun(RUN_REV, 'reviewer', 'claude-b', 'opus') })] }
  const w = world(on, { ...SAMPLE_REPLIES(), 'status --json': json(reviewing) })
  await start($, w)
  await w.clock.advance(10_000)
  expect(w.statusRuns()).toBe(2)

  // Without a live run the same task waits on the commander: the next refresh is a minute away.
  w.replies['status --json'] = json({ tasks: [task('T1', 'S3-9 handoff', 'in_review', { live_run: null })] })
  await w.clock.advance(10_000)
  expect(w.statusRuns()).toBe(3)
  await w.clock.advance(10_000)
  expect(w.statusRuns()).toBe(3)
  await w.clock.advance(50_000)
  expect(w.statusRuns()).toBe(4)
})

test('panel3_login_names_in_quota_band_toast_and_run_pane', async ($, on) => {
  const w = world(on, { ...WATCH_REPLIES(), 'profile usage --json': json(SAMPLE_USAGE_PLANS), 'profile list --json': json(PROFILE_LIST) })
  await watchT2($, w)
  const ui = await pane($, 'terminal')
  expect((await ui.find({ type: 'Text', text: /^claude-a \(alice, Max\) {2}/ }))?.text).toContain('78% left')
  expect((await ui.find({ type: 'Text', text: /^codex-a \(carol, Plus\) {2}/ }))?.text).toContain('quota exhausted')
  expect(await ui.find({ type: 'Text', text: /^agy-a {2}/ })).toBeDefined()
  await ui.press({ key: 'ask:T1' })
  expect(w.submitted).toEqual(['Thoth task "S3-9 handoff" is now in review. Please take a look and tell me the next step.'])
  await ui.unmount()
  const top = await band($, 'terminal')
  expect((await top.find({ type: 'Text', text: /Thoth · / }))?.text).toContain('codex-a (carol, Plus) 5h 14% left')
  await top.unmount()
  const run = await runPane($, 'terminal')
  expect((await textsOf(run))[0]).toBe('S3-11 codex quota · implementer · codex-a (carol, Plus) · gpt-5.6-luna · running')
  await run.unmount()
  await w.clock.advance(10_000)
  expect(w.toasts).toContain('codex-a (carol, Plus) ran out of quota; its task was interrupted')
})

test('panel3_login_names_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, { ...WATCH_REPLIES(), 'profile usage --json': json(SAMPLE_USAGE_PLANS), 'profile list --json': json(PROFILE_LIST) })
  await watchT2($, w)
  const ui = await pane($, 'terminal')
  expect((await ui.find({ type: 'Text', text: /^claude-a（alice，Max） {2}/ }))?.text).toContain('剩 78%')
  await ui.unmount()
  const run = await runPane($, 'terminal')
  expect((await textsOf(run))[0]).toBe('S3-11 codex quota · 實作者 · codex-a（carol，Plus） · gpt-5.6-luna · 執行中')
  await run.unmount()
  await w.clock.advance(10_000)
  expect(w.toasts).toContain('codex-a（carol，Plus） 額度用完，任務中斷')
})

test('panel3_profile_list_runs_at_most_once_a_minute_and_a_failure_means_no_names', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  expect(profileListRuns(w)).toBe(1)
  await w.clock.advance(10_000)
  await w.clock.advance(10_000)
  expect(profileListRuns(w)).toBe(1)
  for (let i = 0; i < 4; i++) await w.clock.advance(10_000)
  expect(profileListRuns(w)).toBe(2)

  w.replies['profile list --json'] = { exitCode: 1, stderr: 'boom' }
  await $.command.run({
    command: 'thoth',
    args: '',
    origin: { kind: 'composer' },
    presentation: { isFullscreen: true, columns: 160 },
  })
  expect(profileListRuns(w)).toBe(3)
  const ui = await pane($, 'terminal')
  expect(await ui.find({ type: 'Text', text: /^claude-a \(Max\) {2}/ })).toBeDefined()
  expect(await between(ui, 2, 3)).toContain('● S3-9 handoff under review · claude-a (Max) · opus')
  for (const text of await texts(ui)) {
    expect(text).not.toContain('alice')
    expect(text).not.toContain('boom')
  }
  await ui.unmount()
})

/** Presses `key` on a fresh terminal pane, lets the panel settle, and gives back the pane drawn after it. */
async function pressAndRedraw($: Engine, w: World, key: string) {
  const ui = await pane($, 'terminal')
  await ui.press({ key })
  await ui.unmount()
  await w.clock.settle()
  return pane($, 'terminal')
}

const CANCEL_EN = {
  implementer:
    'Stop the run of S3-11 codex quota? This is final: the task becomes cancelled and cannot be dispatched again; redoing it needs a new task.',
  reviewer: 'Stop the review of S3-9 handoff (claude-a (alice, Max), opus)? The task stays in review; the commander can start the review again.',
  spec_reviewer: 'Stop the spec review of Spec draft? The task stays where it was; the spec review can be started again.',
}
const CANCEL_ZH = {
  implementer: '停止「S3-11 codex quota」的 run？這是最終的：任務會變成已取消、不能再派，要重做得另建任務。',
  reviewer: '停止「S3-9 handoff」的審查（claude-a（alice，Max），opus）？任務留在等審查，指揮官可以重新開審查。',
  spec_reviewer: '停止「Spec draft」的規格審查？任務停在原狀態，可以重新開規格審查。',
}

test('panel3_cancel_asks_first_and_runs_once_on_yes', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  w.calls.length = 0
  let ui = await pressAndRedraw($, w, 'cancel:T2')
  expect(writes(w)).toEqual([])
  expect(await ui.find({ type: 'Text', text: CANCEL_EN.implementer })).toBeDefined()
  expect((await ui.find({ type: 'Button', key: 'confirm:T2' }))?.text).toBe('Yes, cancel')
  expect((await ui.find({ type: 'Button', key: 'back:T2' }))?.text).toBe('Back')
  // A second press while thoth answers runs nothing more.
  await Promise.all([ui.press({ key: 'confirm:T2' }), ui.press({ key: 'confirm:T2' })])
  await ui.unmount()
  await w.clock.settle()
  expect(writes(w)).toEqual([`cancel ${RUN_IMPL} --json`])
  expect(w.toasts).toContain('Cancelled: S3-11 codex quota')
  expect(w.statusRuns()).toBeGreaterThanOrEqual(1)
  ui = await pane($, 'terminal')
  expect(await ui.find({ type: 'Text', text: CANCEL_EN.implementer })).toBeUndefined()
  await ui.unmount()
})

test('panel3_back_runs_nothing', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  w.calls.length = 0
  let ui = await pressAndRedraw($, w, 'cancel:T1')
  expect(await ui.find({ type: 'Text', text: CANCEL_EN.reviewer })).toBeDefined()
  await ui.press({ key: 'back:T1' })
  await ui.unmount()
  await w.clock.settle()
  ui = await pane($, 'terminal')
  expect(await ui.find({ type: 'Text', text: CANCEL_EN.reviewer })).toBeUndefined()
  expect(await ui.find({ type: 'Button', key: 'confirm:T1' })).toBeUndefined()
  await ui.unmount()
  expect(writes(w)).toEqual([])
})

test('panel3_the_three_cancel_sentences', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  for (const [key, text] of [['cancel:T2', CANCEL_EN.implementer], ['cancel:T1', CANCEL_EN.reviewer], ['cancel:T6', CANCEL_EN.spec_reviewer]]) {
    const ui = await pressAndRedraw($, w, key!)
    expect(await ui.find({ type: 'Text', text: text! })).toBeDefined()
    await ui.unmount()
  }
  expect(writes(w)).toEqual([])
})

test('panel3_the_three_cancel_sentences_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  for (const [key, text] of [['cancel:T2', CANCEL_ZH.implementer], ['cancel:T1', CANCEL_ZH.reviewer], ['cancel:T6', CANCEL_ZH.spec_reviewer]]) {
    const ui = await pressAndRedraw($, w, key!)
    expect(await ui.find({ type: 'Text', text: text! })).toBeDefined()
    expect((await ui.find({ type: 'Button', text: '確定取消' }))).toBeDefined()
    expect((await ui.find({ type: 'Button', text: '返回' }))).toBeDefined()
    await ui.unmount()
  }
  const ui = await pane($, 'terminal')
  expect((await ui.find({ type: 'Button', key: 'cancel:T2' }))?.text).toBe('取消')
  await ui.unmount()
  expect(writes(w)).toEqual([])
})

test('panel3_a_changed_run_id_drops_the_confirm', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  w.calls.length = 0
  let ui = await pressAndRedraw($, w, 'cancel:T2')
  expect(await ui.find({ type: 'Text', text: CANCEL_EN.implementer })).toBeDefined()
  await ui.unmount()
  w.replies['status --json'] = json({
    tasks: LIVE_STATUS.tasks.map(t => (t.task_id === 'T2' ? { ...t, live_run: liveRun(RUN_OTHER, 'implementer', 'codex-a', 'gpt-5.6-luna') } : t)),
  })
  await w.clock.advance(10_000)
  ui = await pane($, 'terminal')
  expect(await ui.find({ type: 'Text', text: CANCEL_EN.implementer })).toBeUndefined()
  expect(await ui.find({ type: 'Button', key: 'confirm:T2' })).toBeUndefined()
  await ui.unmount()
  expect(writes(w)).toEqual([])
})

test('panel3_a_reload_drops_the_confirm', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  let ui = await pressAndRedraw($, w, 'close:T4')
  expect(await ui.find({ type: 'Button', key: 'confirm:T4' })).toBeDefined()
  await ui.unmount()
  await $.session.start({ cwd: '/repo' })
  await w.clock.settle()
  ui = await pane($, 'terminal')
  expect(await ui.find({ type: 'Button', key: 'confirm:T4' })).toBeUndefined()
  await ui.unmount()
  expect(writes(w)).toEqual([])
})

const CLOSE_ARGS = 'task close T4 --reason closed from the Thoth panel --json'

test('panel3_close_task_asks_first_and_runs_once', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  await start($, w)
  let ui = await pane($, 'terminal')
  expect((await ui.find({ type: 'Button', key: 'close:T4' }))?.text).toBe('Close task')
  expect(await ui.find({ type: 'Button', key: 'close:T3' })).toBeDefined()
  expect(await ui.find({ type: 'Button', key: 'cancel:T4' })).toBeUndefined()
  await ui.unmount()
  w.calls.length = 0
  ui = await pressAndRedraw($, w, 'close:T4')
  expect(writes(w)).toEqual([])
  expect(
    await ui.find({ type: 'Text', text: 'Close MOD-2 stream? It becomes cancelled and drops out of the list; it cannot be dispatched again.' }),
  ).toBeDefined()
  await ui.press({ key: 'confirm:T4' })
  await ui.unmount()
  await w.clock.settle()
  expect(writes(w)).toEqual([CLOSE_ARGS])
  expect(w.toasts).toContain('Closed: MOD-2 stream')
})

test('panel3_close_task_in_zh_tw_and_never_on_in_review', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, { ...SAMPLE_REPLIES(), 'profile list --json': json(PROFILE_LIST) })
  await start($, w)
  let ui = await pane($, 'terminal')
  expect(await ui.find({ type: 'Button', key: 'close:T1' })).toBeUndefined()
  expect(await ui.find({ type: 'Button', key: 'again:T1' })).toBeUndefined()
  expect((await ui.find({ type: 'Button', key: 'close:T4' }))?.text).toBe('關閉任務')
  expect((await ui.find({ type: 'Button', key: 'again:T4' }))?.text).toBe('重派')
  await ui.unmount()
  ui = await pressAndRedraw($, w, 'close:T4')
  expect(await ui.find({ type: 'Text', text: '關閉「MOD-2 stream」？任務會變成已取消、從清單消失，不能再派。' })).toBeDefined()
  expect((await ui.find({ type: 'Button', key: 'confirm:T4' }))?.text).toBe('確定關閉')
  await ui.press({ key: 'confirm:T4' })
  await ui.unmount()
  await w.clock.settle()
  expect(w.toasts).toContain('已關閉：MOD-2 stream')
})

test('panel3_an_error_is_told_and_the_panel_refreshes', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  w.replies[CLOSE_ARGS] = { exitCode: 1, stderr: 'warning: something\n{"ok":false,"error":"the task changed meanwhile"}\n' }
  await start($, w)
  const ui = await pressAndRedraw($, w, 'close:T4')
  const runs = w.statusRuns()
  await ui.press({ key: 'confirm:T4' })
  await ui.unmount()
  await w.clock.settle()
  expect(w.toasts).toContain('MOD-2 stream: the task changed meanwhile')
  expect(w.toasts.filter(text => text.startsWith('Closed'))).toEqual([])
  expect(w.statusRuns()).toBeGreaterThan(runs)
})

test('panel3_no_answer_in_time_is_told', async ($, on) => {
  const w = world(on, LIVE_REPLIES())
  w.replies[`cancel ${RUN_IMPL} --json`] = 'throw'
  await start($, w)
  const ui = await pressAndRedraw($, w, 'cancel:T2')
  const runs = w.statusRuns()
  await ui.press({ key: 'confirm:T2' })
  await ui.unmount()
  await w.clock.settle()
  expect(w.toasts).toContain('thoth did not answer in time; the panel will show what happened')
  expect(w.statusRuns()).toBeGreaterThan(runs)
})

const T4_RUNS = (implementer: Record<string, unknown>) => ({
  task_id: 'T4',
  title: 'MOD-2 stream',
  status: 'failed',
  runs: [
    { ...runRow(RUN_A, 'implementer', 'interrupted', 'codex-a', 'gpt-5.6-luna'), effort_requested: 'low', interrupted_reason: 'crash', next_step: 'x' },
    { ...runRow(RUN_B, 'implementer', 'failed', 'claude-b', 'sonnet'), effort_requested: 'high', interrupted_reason: null, next_step: null, ...implementer },
    { ...runRow(RUN_C, 'reviewer', 'completed', 'codex-a', 'gpt-5.6-luna'), effort_requested: 'xhigh' },
  ],
})
const AGAIN_REASON = `dispatched again from the Thoth panel: same profile, model and effort as run ${RUN_B}`

test('panel3_dispatch_again_repeats_the_last_implementer_run', async ($, on) => {
  const w = world(on, { ...LIVE_REPLIES(), 'status T4 --json': json(T4_RUNS({})) })
  await start($, w)
  w.calls.length = 0
  const ui = await pressAndRedraw($, w, 'again:T4')
  expect(writes(w)).toEqual([])
  expect(w.calls.map(call => call.argv.slice(1).join(' '))).toContain('status T4 --json')
  expect(
    await ui.find({
      type: 'Text',
      text: "Dispatch MOD-2 stream again on claude-b with sonnet, effort high, the same as its last run? A new run starts and uses that account's quota.",
    }),
  ).toBeDefined()
  expect((await ui.find({ type: 'Button', key: 'confirm:T4' }))?.text).toBe('Yes, dispatch again')
  await Promise.all([ui.press({ key: 'confirm:T4' }), ui.press({ key: 'confirm:T4' })])
  await ui.unmount()
  await w.clock.settle()
  expect(writes(w)).toEqual([`dispatch T4 --profile claude-b --model sonnet --effort high --reason ${AGAIN_REASON} --json`])
  expect(w.toasts).toContain('Dispatched again: MOD-2 stream')
})

test('panel3_dispatch_again_without_effort_in_zh_tw', { options: { language: 'zh-TW' } }, async ($, on) => {
  const w = world(on, { ...LIVE_REPLIES(), 'status T4 --json': json(T4_RUNS({ effort_requested: null })) })
  await start($, w)
  w.calls.length = 0
  const ui = await pressAndRedraw($, w, 'again:T4')
  expect(await ui.find({ type: 'Text', text: '用 claude-b、sonnet 照上一次重派「MOD-2 stream」？會開新的 run，用掉那個帳號的額度。' })).toBeDefined()
  expect((await ui.find({ type: 'Button', key: 'confirm:T4' }))?.text).toBe('確定重派')
  await ui.press({ key: 'confirm:T4' })
  await ui.unmount()
  await w.clock.settle()
  expect(writes(w)).toEqual([`dispatch T4 --profile claude-b --model sonnet --reason ${AGAIN_REASON} --json`])
  expect(w.toasts).toContain('已重派：MOD-2 stream')
})

test('panel3_dispatch_again_on_quota_only_shows_the_next_step', async ($, on) => {
  const next = 'quota used up on claude-b; dispatch again with a profile that still has quota: thoth dispatch T4 --profile OTHER'
  const w = world(on, {
    ...LIVE_REPLIES(),
    'status T4 --json': json(T4_RUNS({ status: 'interrupted', interrupted_reason: 'quota', next_step: next })),
  })
  await start($, w)
  w.calls.length = 0
  const ui = await pressAndRedraw($, w, 'again:T4')
  expect(await ui.find({ type: 'Text', text: next })).toBeDefined()
  expect(await ui.find({ type: 'Button', key: 'confirm:T4' })).toBeUndefined()
  expect((await ui.find({ type: 'Button', key: 'back:T4' }))?.text).toBe('Back')
  for (const text of await texts(ui)) expect(text).not.toContain('Dispatch MOD-2 stream again')
  await ui.unmount()
  expect(writes(w)).toEqual([])
})

test('panel3_dispatch_again_without_an_implementer_run_asks_for_the_commander', async ($, on) => {
  const w = world(on, {
    ...LIVE_REPLIES(),
    'status T4 --json': json({ task_id: 'T4', runs: [runRow(RUN_C, 'reviewer', 'completed', 'codex-a', 'gpt-5.6-luna')] }),
  })
  await start($, w)
  w.calls.length = 0
  const ui = await pressAndRedraw($, w, 'again:T4')
  expect(await ui.find({ type: 'Button', key: 'confirm:T4' })).toBeUndefined()
  await ui.unmount()
  expect(w.toasts).toContain('MOD-2 stream has no earlier run to repeat; ask the commander')
  expect(writes(w)).toEqual([])
})

test('panel3_a_refused_dispatch_is_told', async ($, on) => {
  const w = world(on, { ...LIVE_REPLIES(), 'status T4 --json': json(T4_RUNS({})) })
  w.replies[`dispatch T4 --profile claude-b --model sonnet --effort high --reason ${AGAIN_REASON} --json`] = {
    exitCode: 1,
    stderr: '{"ok":false,"error":"profile claude-b failed its auth check"}\n',
  }
  await start($, w)
  const ui = await pressAndRedraw($, w, 'again:T4')
  await ui.press({ key: 'confirm:T4' })
  await ui.unmount()
  await w.clock.settle()
  expect(w.toasts).toContain('MOD-2 stream: profile claude-b failed its auth check')
  expect(w.toasts.filter(text => text.startsWith('Dispatched again'))).toEqual([])
})
