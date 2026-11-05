import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import vm from 'node:vm'

const html = readFileSync(new URL('../public/index.html', import.meta.url), 'utf8')
const selection = html.match(/\/\/ ---------- linked PRs ----------([\s\S]*?)\/\/ ---------- run cards ----------/)[1]
const allPrs = html.match(/function allPrs\(\) \{[\s\S]*?\n\}/)[0]
const findPr = html.match(/^const findPr = .+$/m)[0]
const matches = html.match(/function matches\(pr, query\) \{[\s\S]*?\n\}/)[0]

function setup(api, mine = []) {
  const S = { data: { mine, others: [] }, linkedPrs: {}, linkedLogin: '', detail: 'old/repo#1', tabs: ['old/repo#1'], comments: { old: 'An unfinished comment' }, drafts: { old: 'An unfinished request' }, query: '', show: 'all', lane: 'all', group: 'repo' }
  const errors = []
  const reveals = []
  const search = { value: '' }
  const row = { style: {}, scrollIntoView: options => reveals.push({ key: S.detail, ...options, margin: row.style.scrollMarginTop }) }
  const context = vm.createContext({
    S, api, keyOf: pr => `${pr.repo}#${pr.number}`, CSS: { escape: value => value },
    $: selector => selector === '#search' ? search : { offsetHeight: 120 },
    document: { querySelector: () => row }, handled: pr => pr.handled,
    renderBoard() { context.revealSelectedRow() }, renderHeader() {}, renderDetail() {}, loadDetail() {},
    toast: (...args) => errors.push(args),
  })
  vm.runInContext(`${allPrs}\n${findPr}\n${matches}\n${selection}`, context)
  return { S, errors, reveals, search, ...vm.runInContext('({ openLinkedPr, openDetail, findPr, allPrs, settleDetailReveal, revealSelectedRow })', context) }
}
const pr = n => ({ repo: 'owner/repo', number: n, url: `https://github.com/owner/repo/pull/${n}`, lane: 'ready', title: `PR ${n}`, labels: [] })

test('opens an inbox PR without fetching or discarding drafts and existing tabs', async () => {
  const app = setup(() => { throw Error('unexpected request') }, [pr(7)])
  await app.openLinkedPr(pr(7).url)
  assert.equal(app.S.detail, 'owner/repo#7')
  assert.deepEqual([...app.S.tabs], ['old/repo#1', 'owner/repo#7'])
  assert.equal(app.S.comments.old, 'An unfinished comment')
  assert.equal(app.S.drafts.old, 'An unfinished request')
  assert.equal(app.errors.length, 0)
})

test('fetches an external PR on cold load and keeps it out of inbox bulk actions', async () => {
  let requested
  const app = setup(async path => { requested = path; return { login: 'me', pr: pr(8) } })
  app.S.data = null
  await app.openLinkedPr(pr(8).url)
  assert.equal(requested, `/api/pr-summary?url=${encodeURIComponent(pr(8).url)}`)
  assert.equal(app.S.detail, 'owner/repo#8')
  assert.equal(app.findPr('owner/repo#8').number, 8)
  assert.equal(app.allPrs().length, 0)
  assert.equal(app.S.linkedLogin, 'me')
})

test('an inaccessible PR reports an error and preserves the current selection', async () => {
  const app = setup(async () => { throw Error('GitHub denied access') })
  await app.openLinkedPr(pr(9).url)
  assert.equal(app.S.detail, 'old/repo#1')
  assert.match(app.errors[0][0], /GitHub denied access/)
  assert.equal(app.errors[0][1], true)
})

test('a slow earlier response cannot replace the most recently opened PR', async () => {
  const pending = []
  const app = setup(() => new Promise(resolve => pending.push(resolve)))
  const earlier = app.openLinkedPr(pr(10).url)
  const later = app.openLinkedPr(pr(11).url)
  pending[1]({ login: 'me', pr: pr(11) })
  await later
  pending[0]({ login: 'me', pr: pr(10) })
  await earlier
  assert.equal(app.S.detail, 'owner/repo#11')
  assert.equal(app.findPr('owner/repo#10'), undefined)
})

test('reopening an external PR fetches current state and keeps a single tab', async () => {
  let fetches = 0
  const app = setup(async () => ({ login: 'me', pr: { ...pr(12), state: ++fetches === 1 ? 'OPEN' : 'MERGED' } }))
  await app.openLinkedPr(pr(12).url)
  await app.openLinkedPr(pr(12).url)
  assert.equal(app.findPr('owner/repo#12').state, 'MERGED')
  assert.equal(app.S.tabs.filter(key => key === 'owner/repo#12').length, 1)
})

test('opening and reopening a PR reveals its row below the sticky header', async () => {
  const app = setup(() => { throw Error('unexpected request') }, [pr(7)])
  await app.openLinkedPr(pr(7).url)
  await app.openLinkedPr(pr(7).url)
  assert.deepEqual(app.reveals.map(r => r.key), ['owner/repo#7', 'owner/repo#7'])
  assert.equal(app.reveals[0].margin, '136px')
  app.revealSelectedRow()
  assert.equal(app.reveals.length, 2, 'background redraws must not pull the user back to the row')
})

test('removes only filters that hide the requested inbox PR', async () => {
  const app = setup(() => { throw Error('unexpected request') }, [{ ...pr(7), handled: true }])
  Object.assign(app.S, { query: 'repo:another', show: 'needs', lane: 'fix' })
  app.search.value = app.S.query
  await app.openLinkedPr(pr(7).url)
  assert.equal(app.S.query, '')
  assert.equal(app.search.value, '')
  assert.equal(app.S.show, 'all')
  assert.equal(app.S.lane, 'all')
  assert.equal(app.S.group, 'repo')

  Object.assign(app.S, { query: '#7', show: 'all', lane: 'ready' })
  app.search.value = '#7'
  await app.openLinkedPr(pr(7).url)
  assert.equal(app.S.query, '#7')
  assert.equal(app.search.value, '#7')
  assert.equal(app.S.lane, 'ready')
})

test('a cold link is revealed again when its row moves into the loaded inbox', async () => {
  const app = setup(async () => ({ login: 'me', pr: pr(8) }))
  app.S.data = null
  app.S.query = 'repo:another'
  await app.openLinkedPr(pr(8).url)
  assert.equal(app.S.revealAfterLoad, 'owner/repo#8')
  assert.equal(app.S.query, 'repo:another', 'a standalone linked row does not need filters cleared')
  app.S.data = { mine: [pr(8)], others: [] }
  app.settleDetailReveal()
  app.revealSelectedRow()
  assert.deepEqual(app.reveals.map(r => r.key), ['owner/repo#8', 'owner/repo#8'])
  assert.equal(app.S.query, '')
  assert.equal(app.S.revealAfterLoad, null)
})

test('a failed cold inbox load retains the reveal until recovery', async () => {
  const app = setup(async () => ({ login: 'me', pr: pr(8) }))
  app.S.data = null
  app.S.error = 'GitHub temporarily unavailable'
  app.S.query = 'repo:another'
  await app.openLinkedPr(pr(8).url)
  app.settleDetailReveal()
  assert.equal(app.S.revealAfterLoad, 'owner/repo#8')
  app.S.data = { mine: [pr(8)], others: [] }
  app.S.error = null
  app.settleDetailReveal()
  app.revealSelectedRow()
  assert.equal(app.S.query, '')
  assert.deepEqual(app.reveals.map(r => r.key), ['owner/repo#8', 'owner/repo#8'])
  assert.equal(app.S.revealAfterLoad, null)
})

test('a pending cold reveal never scrolls back to an earlier selection', async () => {
  const app = setup(async () => ({ login: 'me', pr: pr(8) }))
  app.S.data = null
  await app.openLinkedPr(pr(8).url)
  app.S.detail = null
  app.S.data = { mine: [pr(8)], others: [] }
  app.settleDetailReveal()
  app.revealSelectedRow()
  assert.equal(app.reveals.length, 1)
  assert.equal(app.S.revealAfterLoad, null)
})

test('a linked-only PR gets a selected left-hand row without a bulk checkbox', () => {
  const linked = { ...pr(8), failing: [], additions: 1, deletions: 0, files: 1, reviewers: [], author: 'me', name: 'repo' }
  const S = { data: null, detail: 'owner/repo#8', linkedPrs: { 'owner/repo#8': linked }, picked: new Set() }
  const row = html.match(/function row\(pr, lane, byRepo = false, linked = false\) \{[\s\S]*?\n\}/)[0]
  const openedPrRow = html.match(/function openedPrRow\(\) \{[\s\S]*?\n\}/)[0]
  const context = vm.createContext({
    S, LANE: { ready: { label: 'Ready', dot: 'green', hint: 'Ready to merge' } },
    keyOf: pr => `${pr.repo}#${pr.number}`, esc: String, doneFor: () => null, idle: () => true,
    httpsOnly: url => url, actsHtml: () => '', plural: () => '', ago: () => '', age: () => '',
    blockers: () => '', short: pr => `repo#${pr.number}`, pickable: () => true,
    tags: () => '', strip: () => '',
  })
  vm.runInContext(`${allPrs}\n${findPr}\n${row}\n${openedPrRow}`, context)
  const rendered = context.openedPrRow()
  assert.match(rendered, /class="pr selected"/)
  assert.match(rendered, /aria-current="true"/)
  assert.match(rendered, /data-key="owner\/repo#8"/)
  assert.doesNotMatch(rendered, /data-act="pick"/)
  S.data = { mine: [linked], others: [] }
  assert.equal(context.openedPrRow(), '', 'use the real inbox row once its snapshot arrives')
})
