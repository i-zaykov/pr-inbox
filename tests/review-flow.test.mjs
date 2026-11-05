import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import vm from 'node:vm'

const html = readFileSync(new URL('../public/index.html', import.meta.url), 'utf8')
const fn = name => html.match(new RegExp(`(?:async )?function ${name}\\([^]*?\\n\\}`))[0]
const declaration = name => html.match(new RegExp(`^const ${name} = .+$`, 'm'))[0]
const verbs = html.match(/const VERBS = \{[^]*?\n\}/)[0]
const actions = html.match(/const ACTIONS = \{[^]*?\n\}/)[0]
const pr = { repo: 'owner/repo', name: 'repo', number: 7, url: 'https://github.com/owner/repo/pull/7',
  state: 'OPEN', title: 'A linked PR', author: 'someone', lane: 'waiting', writable: true, approvable: true }
const run = () => ({ id: 'review-1', kind: 'approve', title: 'Review linked PR', prUrl: pr.url,
  status: 'running', mode: 'plan', cwd: '/tmp/review', startedAt: Date.now(), updatedAt: 1,
  events: [{ kind: 'text', text: 'Checking the final diff', at: 1 }], eventCount: 1 })

function setup() {
  const S = { data: { mine: [], others: [] }, linkedPrs: { 'owner/repo#7': pr }, detail: 'owner/repo#7',
    runs: [], runsOpen: {}, busy: {}, drafts: {}, fullLog: {}, logOpen: {}, replyOpen: {}, replyDrafts: {}, openRun: null }
  const requests = []
  const rendered = { html: '' }
  let remoteRuns = []
  const context = vm.createContext({
    S, ADDRESS: '', review: () => ({ state: 'none' }), esc: String, short: p => `${p.name}#${p.number}`,
    keyOf: p => `${p.repo}#${p.number}`, active: r => ['running', 'queued'].includes(r.status),
    KIND: { approve: ['Reviewing, then publishing', 'Thermo & approve'] }, RUN_DOT: {},
    dur: () => '1s', ago: () => 'just now', tilde: String, inline: String, firstLine: String,
    icon: () => '', toast() {}, setTimeout() {},
    document: { getElementById: () => ({ scrollIntoView() {} }) },
    openDetail: k => { S.detail = k },
    renderBoard: () => { rendered.html = context.detailRunCards(pr) },
    renderRuns: () => { rendered.html = context.detailRunCards(pr) },
    api: async (path, body) => {
      requests.push({ path, body })
      if (path === '/api/runs') { const r = run(); remoteRuns = [r]; return r }
      if (path === '/api/runs/review-1') return { events: [{ kind: 'tool', text: 'Full review evidence', at: 0 }, ...remoteRuns[0].events] }
      if (path.endsWith('/stop')) { remoteRuns[0].stopRequested = true; return {} }
      throw Error(`Unexpected request: ${path}`)
    },
    loadRuns: async () => { S.runs = remoteRuns; context.renderBoard() },
  })
  const functions = ['allPrs', 'detailRunCards', 'runCard', 'showRun', 'announce', 'startEach', 'startRun', 'runVerb', 'loadFullLog']
  vm.runInContext([
    verbs, declaration('can'), declaration('prOf'), declaration('runsOf'), declaration('lastSaid'), declaration('runName'),
    html.match(/const cardSig = [^]*?\n\n/)[0], ...functions.map(fn), actions,
    'globalThis.controls = ACTIONS; globalThis.allowed = can; globalThis.ownerOf = prOf',
  ].join('\n'), context)
  return { S, context, rendered, requests }
}

test('approval availability follows the server verdict even for assigned Waiting PRs', () => {
  const { context } = setup()
  assert.equal(context.allowed('approve', { ...pr, yourReview: 'APPROVED', approvable: false }, false), false)
  assert.equal(context.allowed('approve', { ...pr, yourReview: 'APPROVED', reviewRequested: true }, false), true)
  assert.equal(context.allowed('approve', { ...pr, state: 'MERGED' }, false), false)
  assert.equal(context.allowed('approve', { ...pr, isCrossRepo: true, writable: false }, false), true, 'a read-only review can inspect a fork')
})

test('starting a linked PR review opens its progress, result, full log and Stop in the panel', async () => {
  const { S, context, rendered, requests } = setup()
  await context.startRun('approve', pr)
  assert.equal(S.openRun, 'review-1')
  assert.equal(context.ownerOf(S.runs[0]), pr)
  assert.match(rendered.html, /Checking the final diff/)
  assert.match(rendered.html, /data-act="run-stop" data-run="review-1"/)
  assert.match(rendered.html, /data-act="run-log"/)
  assert.equal(context.allPrs().length, 0, 'linked PRs must remain outside bulk actions')

  context.controls['run-log']({ run: 'review-1' })
  await new Promise(resolve => setImmediate(resolve))
  assert.match(rendered.html, /Full review evidence/)
  await context.controls['run-stop']({ run: 'review-1' })
  assert.ok(requests.some(request => request.path === '/api/runs/review-1/stop'))
  assert.match(rendered.html, /Stopping…/)

  Object.assign(S.runs[0], { status: 'done', endedAt: Date.now(), updatedAt: 2 })
  S.runs[0].events.push({ kind: 'result', text: 'Approval published on reviewed commit', at: 2 })
  context.renderRuns()
  assert.match(rendered.html, /Approval published on reviewed commit/)
  assert.doesNotMatch(rendered.html, /data-act="run-stop"/)
})

test('a linked run reopens its PR and gives its cards back to the inbox without duplicates', async () => {
  const { S, context, rendered } = setup()
  await context.startRun('approve', pr)
  S.detail = null
  context.showRun('review-1')
  assert.equal(S.detail, 'owner/repo#7')
  assert.equal((rendered.html.match(/id="run-review-1"/g) || []).length, 1)
  S.data.others = [pr]
  assert.equal(context.detailRunCards(pr), '', 'an inbox run must only have its board card')
  assert.equal(context.allPrs().length, 1)
})

test('own Thermo actions disclose changes and approval says PR Inbox publishes the review', () => {
  const { context } = setup()
  const labels = vm.runInContext('({ thermo: VERBS.thermo.menu, approve: VERBS.approve.confirm.body })', context)
  assert.match(labels.thermo, /review, fix & push/i)
  assert.match(labels.approve(pr), /PR Inbox publishes the review/)
  assert.match(labels.approve(pr), /incomplete review publishes nothing/)
})

test('final review cards offer only supported recovery actions after their snapshot is removed', () => {
  const { S, context } = setup()
  const r = { ...run(), sessionStarted: true }
  S.openRun = r.id
  S.replyOpen[r.id] = true
  S.replyDrafts[r.id] = 'A draft left from the repair session'
  for (const stage of ['preparing', 'reviewing', 'publishing', 'complete']) {
    for (const status of stage === 'complete' ? ['done'] : ['failed', 'interrupted', 'stopped']) {
      const card = context.runCard({ ...r, status, reviewJob: { stage } })
      assert.doesNotMatch(card, /data-act="(?:reply-open|reply-send|run-terminal)"/)
      assert.doesNotMatch(card, /A draft left from the repair session/)
      if (status === 'failed') assert.match(card, /data-act="run-again"/)
      else if (status !== 'done') assert.match(card, /data-act="run-retry"/)
      else assert.doesNotMatch(card, /data-act="(?:run-again|run-retry)"/)
      assert.match(card, /data-act="run-log"/)
      assert.match(card, /data-act="run-dismiss"/)
    }
  }
})

test('repair and legacy sessions retain Reply, Retry and Terminal controls', () => {
  const { S, context } = setup()
  const r = { ...run(), status: 'failed', sessionStarted: true }
  S.openRun = r.id
  S.replyOpen[r.id] = true
  for (const reviewJob of [null, { stage: 'repair' }]) {
    const card = context.runCard({ ...r, reviewJob })
    for (const action of ['reply-open', 'reply-send', 'run-again', 'run-terminal']) {
      assert.ok(card.includes(`data-act="${action}"`), `${action} stays available`)
    }
  }
})
