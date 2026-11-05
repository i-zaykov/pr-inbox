import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import vm from 'node:vm'

const html = readFileSync(new URL('../public/index.html', import.meta.url), 'utf8')
const fn = name => html.match(new RegExp(`(?:async )?function ${name}\\([^]*?\\n\\}`))[0]
const declaration = name => html.match(new RegExp(`^const ${name} = .+$`, 'm'))[0]

const pr = { url: 'https://github.com/o/r/pull/7', lane: 'waiting', askedAt: '2026-11-05T10:00:00Z' }
const NOW = Date.parse('2026-11-05T12:00:00Z')
const status = (extra = {}) => ({ settings: { enabled: true, sessionLimit: 70, weeklyLimit: 90 }, waiting: {}, message: '', ...extra })

function setup({ auto = status(), skips = {}, runs = [] } = {}) {
  const S = { auto, runs }
  const context = vm.createContext({ S, LANE: { waiting: { theirs: true } }, recall: () => JSON.stringify(skips), active: r => ['running', 'queued'].includes(r.status) })
  vm.runInContext([
    fn('clock'), fn('autoLine'),
    declaration('skippedAt'), declaration('skipped'), declaration('runFor'), declaration('autoNext'),
    'globalThis.page = { clock, autoLine, autoNext }',
  ].join('\n'), context)
  return context.page
}

test('a reset today shows its time, a later one its weekday too', () => {
  const { clock } = setup()
  assert.match(clock(NOW + 3600000, NOW), /^\d\d:\d\d$/)
  assert.match(clock(NOW + 3 * 86400000, NOW), /^\S+ \d\d:\d\d$/)
})

test('the notice says on or off, the usage against its limits, and what happened', () => {
  const { autoLine, clock } = setup()
  assert.match(autoLine(status({ settings: { enabled: false, sessionLimit: 70, weeklyLimit: 90 } }), NOW), /^Off\./)
  const resets = NOW + 2 * 3600000
  const usage = { session: 42.4, sessionResetsAt: resets, weekly: 59, fetchedAt: NOW - 60000 }
  assert.equal(autoLine(status({ usage, message: 'Started Thermo & approve on r#7.' }), NOW),
    `On · Session 42% of 70%, resets ${clock(resets, NOW)} · weekly 59% of 90% · Started Thermo & approve on r#7.`)
  assert.match(autoLine(status({ usage: { ...usage, fetchedAt: NOW - 3600000 } }), NOW), / · read at \d\d:\d\d$/)
  assert.equal(autoLine(status({ usage, usageError: 'Claude refused the sign-in token (HTTP 401)', message: '1 PR waits: Usage unknown, so nothing starts.' }), NOW),
    'On · 1 PR waits: Usage unknown, so nothing starts.', 'a failed read hides the old figures')
  assert.equal(autoLine(status({ message: 'No new PR waits on your review.' }), NOW), 'On · No new PR waits on your review.')
})

test('a row shows why the automatic run waits, unless it is off, skipped or already running', () => {
  const waiting = { [pr.url]: 'Checks are still running' }
  assert.equal(setup({ auto: status({ waiting }) }).autoNext(pr), 'Checks are still running')
  assert.equal(setup({ auto: status({ waiting: {} }) }).autoNext(pr), null)
  assert.equal(setup({ auto: null }).autoNext(pr), null)
  assert.equal(setup({ auto: status({ waiting, settings: { enabled: false } }) }).autoNext(pr), null)
  assert.equal(setup({ auto: status({ waiting }), skips: { [pr.url]: '2026-11-05T11:00:00.000Z' } }).autoNext(pr), null, 'skipped after the request')
  assert.equal(setup({ auto: status({ waiting }), skips: { [pr.url]: '2026-11-05T09:00:00.000Z' } }).autoNext(pr), 'Checks are still running', 'asked again since the skip')
  assert.equal(setup({ auto: status({ waiting }), runs: [{ prUrl: pr.url, status: 'running' }] }).autoNext(pr), null)
})
