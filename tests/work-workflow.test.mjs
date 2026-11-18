// Control-flow test for workflows/run.js, run against fake agents.
// Run: node --test tests/   (override the script path with WORK_WORKFLOW=...)
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const path = process.env.WORK_WORKFLOW || new URL('../workflows/run.js', import.meta.url)
const body = readFileSync(path, 'utf8').replace(/^export const meta/m, 'const meta')
const AsyncFunction = (async () => {}).constructor
const script = new AsyncFunction('agent', 'parallel', 'pipeline', 'phase', 'log', 'args', 'budget', body)

const unit = (id, extra = {}) => ({ id, goal: `do ${id}`, files: [`src/${id}.js`], dependsOn: [], size: 'small', risky: false, ...extra })
const B_FAIL = 'FAIL src/b.js:3 expected 302, got 200'
const B_SIG = 'npm test | FAIL PATH expected N, got N'

// checker(label, cmd, n) returns failure output (exit 1) or 0 (pass) for the n-th run of a check label.
// memory: null = no file, string = raw file text, object = parsed file.
async function run({ units, checks = ['npm test'], checker = () => 0, changed = {}, task = 'add a thing', memory = null, innocent = () => false }) {
  const calls = [], opts = {}, prompts = {}, seen = {}
  let saved = null
  const agent = async (prompt, o) => {
    const { label } = o
    calls.push(label)
    opts[label] = o
    prompts[label] = prompt
    if (label === 'memory:load') return memory === null ? { exists: false } : { exists: true, content: typeof memory === 'string' ? memory : JSON.stringify(memory) }
    if (label === 'memory:save') { saved = JSON.parse(prompt.split('<<<\n')[1].split('\n>>>')[0]); return 'saved' }
    if (label === 'plan') return { branch: 'work/test', checks, units }
    if (label.startsWith('check:')) {
      const n = (seen[label] = (seen[label] || 0) + 1)
      const cmds = [...prompt.matchAll(/^- (.+)$/gm)].map(m => m[1])
      return {
        results: cmds.map(cmd => {
          const out = checker(label, cmd, n)
          return out ? { exitCode: 1, outputTail: out } : { exitCode: 0, outputTail: 'ok' }
        }),
        changedFiles: changed[label] || [],
      }
    }
    if (label.startsWith('work:')) return { summary: 'done' }
    if (label.startsWith('fix:')) return innocent(label) ? { summary: '', blocked: 'not caused by my unit' } : { summary: 'patched it' }
    if (label === 'learn') {
      const items = JSON.parse(prompt.split('EVIDENCE:\n')[1])
      return { rules: items.map(it => ({ sig: it.sig, text: `rule for ${it.sig}`, scope: 'worker' })) }
    }
    throw new Error(`unexpected agent ${label}`)
  }
  const parallel = thunks => Promise.all(thunks.map(t => t().catch(() => null)))
  const result = await script(agent, parallel, null, () => {}, () => {}, { task }, { total: null })
  const model = label => opts[label] && opts[label].model
  return { result, calls, opts, prompts, saved, model }
}

test('graph: parallel layer, only the blamed unit is fixed, the dependent unit waits, memory is saved', async () => {
  const { result, calls, saved, model } = await run({
    units: [unit('a'), unit('b'), unit('c', { dependsOn: ['a', 'b'] })],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
    changed: { 'check:baseline': ['notes.txt'], 'check:L2': ['notes.txt', 'src/a.js', 'src/b.js', 'src/c.js', 'stray.txt'] },
  })
  assert.deepEqual(calls, ['memory:load', 'plan', 'check:baseline', 'work:a', 'work:b', 'check:L1', 'fix:b#1', 'check:L1', 'work:c', 'check:L2', 'learn', 'memory:save'])
  assert.equal(result.status, 'ready_for_review')
  assert.deepEqual(result.done, ['a', 'b', 'c'])
  assert.deepEqual(result.outOfScope, ['stray.txt'])
  assert.equal(saved.runs, 1)
  assert.deepEqual(saved.checks, ['npm test'])
})

test('allocation on a fresh repo: probe haiku once, probe sonnet once, then default to sonnet; fixes go one tier up', async () => {
  const { result, model } = await run({
    units: [unit('a'), unit('b'), unit('c', { dependsOn: ['a', 'b'] })],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
  })
  assert.equal(model('plan'), 'opus')
  assert.equal(model('check:baseline'), 'haiku')
  assert.equal(model('work:a'), 'haiku')
  assert.equal(model('work:b'), 'sonnet')
  assert.equal(model('fix:b#1'), 'opus')
  assert.equal(model('work:c'), 'sonnet')
  assert.equal(model('learn'), 'sonnet')
  assert.equal(model('memory:save'), 'haiku')
  assert.deepEqual(result.models, [{ id: 'a', size: 'small', tier: 'haiku' }, { id: 'b', size: 'small', tier: 'sonnet' }, { id: 'c', size: 'small', tier: 'sonnet' }])
})

test('model stats: a fixed unit counts against its tier, a clean one for it', async () => {
  const { saved } = await run({
    units: [unit('a'), unit('b'), unit('c', { dependsOn: ['a', 'b'] })],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
  })
  assert.deepEqual(saved.models, { 'worker:small': { haiku: { pass: 1, fail: 0 }, sonnet: { pass: 1, fail: 1 } } })
})

test('allocation uses learned stats per size, and evidence is capped at 20', async () => {
  const { model, saved } = await run({
    units: [unit('a'), unit('b', { size: 'large' })],
    memory: { version: 1, runs: 9, nextId: 1, checks: [], rules: [], models: { 'worker:small': { haiku: { pass: 1, fail: 4 }, sonnet: { pass: 19, fail: 1 } } } },
  })
  assert.equal(model('work:a'), 'sonnet') // haiku ruled out (2/7), sonnet proven (20/22)
  assert.equal(model('work:b'), 'haiku') // nothing known about large units yet: probe the cheapest
  assert.deepEqual(saved.models['worker:small'].sonnet, { pass: 19.05, fail: 0.95 }) // 20 + 1 observations scaled back to 20
  assert.deepEqual(saved.models['worker:large'], { haiku: { pass: 1, fail: 0 } })
})

test('a new rule is born from a fixed failure, keyed by its signature', async () => {
  const { result, saved } = await run({
    units: [unit('b')],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
  })
  assert.deepEqual(result.rules.added, ['r1'])
  assert.deepEqual(saved.rules, [{ id: 'r1', text: `rule for ${B_SIG}`, scope: 'worker', sig: B_SIG, a: 2, b: 1 }])
  assert.equal(saved.nextId, 2)
})

test('weighting: strongest rules reach the right prompts; in-play rules are scored; weak ones are pruned', async () => {
  const memory = {
    version: 1, runs: 4, nextId: 5, checks: ['npm test'], models: {},
    rules: [
      { id: 'r1', text: 'Auth redirects return 302', scope: 'worker', sig: B_SIG, a: 3, b: 1 },
      { id: 'r2', text: 'Keep handlers pure', scope: 'all', sig: 'npm test | other', a: 1, b: 1 },
      { id: 'r3', text: 'Old bad rule', scope: 'worker', sig: 'npm test | stale', a: 1, b: 4 },
      { id: 'r4', text: 'Split migrations from code', scope: 'planner', sig: 'npm test | split', a: 2, b: 1 },
    ],
  }
  const { result, saved, prompts } = await run({
    units: [unit('b')],
    memory,
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
  })
  const w = prompts['work:b']
  assert.ok(w.indexOf('Auth redirects return 302') < w.indexOf('Keep handlers pure'), 'strongest first')
  assert.ok(!w.includes('Old bad rule'), 'below 0.4 stays out of prompts')
  assert.ok(!w.includes('Split migrations'), 'planner rules stay out of worker prompts')
  assert.ok(prompts.plan.includes('Split migrations from code') && prompts.plan.includes('Keep handlers pure'))
  assert.ok(!prompts.plan.includes('Auth redirects'), 'worker rules stay out of the planner prompt')
  assert.ok(prompts.plan.includes('Check commands that worked here before: npm test'))

  const byId = Object.fromEntries(saved.rules.map(r => [r.id, r]))
  assert.equal(byId.r1.b, 2, 'its failure came back: +1 against')
  assert.equal(byId.r1.text, `rule for ${B_SIG}`, 'and it is rewritten from the new evidence')
  assert.equal(byId.r2.a, 1.25, 'in play, its failure stayed away: +0.25')
  assert.equal(byId.r4.a, 2.25)
  assert.equal(byId.r3, undefined, 'weight 0.2 after 5 observations: pruned')
  assert.deepEqual(result.rules.revised, ['r1'])
  assert.deepEqual(result.rules.pruned, ['r3'])
  assert.deepEqual(result.rules.added, [])
})

test('a fixer that proves its unit innocent keeps the failure off that unit\'s model stats', async () => {
  const { saved, calls } = await run({
    units: [unit('a'), unit('b')],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? 'FAIL some test, no file named' : 0),
    innocent: label => label === 'fix:a#1',
  })
  assert.ok(calls.includes('fix:a#1') && calls.includes('fix:b#1'), 'no file named: the whole layer is blamed')
  assert.deepEqual(saved.models['worker:small'], { haiku: { pass: 1, fail: 0 }, sonnet: { pass: 0, fail: 1 } })
})

test('a risky unit and everything depending on it are skipped', async () => {
  const { result, calls } = await run({
    units: [unit('a', { risky: true, riskReason: 'migration' }), unit('b', { dependsOn: ['a'] }), unit('c')],
  })
  assert.deepEqual(calls, ['memory:load', 'plan', 'check:baseline', 'work:c', 'check:L1', 'memory:save'])
  assert.deepEqual(result.skipped.map(s => s.id), ['a', 'b'])
  assert.match(result.skipped[0].why, /risky/)
})

test('a layer that never passes escalates haiku → sonnet → opus → opus at high effort, then stops', async () => {
  const { result, calls, opts, saved } = await run({
    units: [unit('a'), unit('c', { dependsOn: ['a'] })],
    checker: label => (label === 'check:L1' ? 'boom in src/a.js' : 0),
  })
  assert.deepEqual(calls, [
    'memory:load', 'plan', 'check:baseline', 'work:a',
    'check:L1', 'fix:a#1', 'check:L1', 'fix:a#2', 'check:L1', 'fix:a#3', 'check:L1',
    'memory:save',
  ])
  assert.deepEqual(['work:a', 'fix:a#1', 'fix:a#2', 'fix:a#3'].map(l => opts[l].model), ['haiku', 'sonnet', 'opus', 'opus'])
  assert.equal(opts['fix:a#3'].effort, 'high')
  assert.equal(result.status, 'stuck')
  assert.deepEqual(result.skipped.map(s => s.id), ['c'])
  assert.deepEqual(saved.rules, [], 'never fixed, so nothing to learn from')
  assert.deepEqual(saved.models['worker:small'].haiku, { pass: 0, fail: 1 })
})

test('a command already failing before any change is left out of the gate', async () => {
  const { result, calls } = await run({
    units: [unit('a')],
    checks: ['npm test', 'npm run lint'],
    checker: (label, cmd) => (cmd === 'npm run lint' ? 'lint: 40 problems' : 0),
  })
  assert.deepEqual(calls, ['memory:load', 'plan', 'check:baseline', 'work:a', 'check:L1', 'memory:save'])
  assert.deepEqual(result.checks, ['npm test'])
  assert.deepEqual(result.checksAlreadyFailing, ['npm run lint'])
})

test('a corrupt memory file is never overwritten', async () => {
  const { result, calls } = await run({ units: [unit('a')], memory: '{not json' })
  assert.ok(!calls.includes('memory:save'))
  assert.equal(result.memorySaved, false)
  assert.equal(result.status, 'ready_for_review')
})

test('no task fails fast before any agent runs', async () => {
  await assert.rejects(run({ units: [unit('a')], task: '  ' }), /no task given/)
})
