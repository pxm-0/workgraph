// Control-flow test for workflows/run.js, run against fake agents.
// Run: node --test tests/   (override the script path with WORK_WORKFLOW=...)
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const path = process.env.WORK_WORKFLOW || new URL('../workflows/run.js', import.meta.url)
const body = readFileSync(path, 'utf8').replace(/^export const meta/m, 'const meta')
const AsyncFunction = (async () => {}).constructor
const script = new AsyncFunction('agent', 'parallel', 'pipeline', 'phase', 'log', 'args', 'budget', body)

const CLI = '/opt/haruspex/scripts/haruspex.py'
const WG = '/opt/workgraph/wg/target/release/wg'
const unit = (id, extra = {}) => ({ id, goal: `do ${id}`, files: [`src/${id}.js`], dependsOn: [], criteria: [`AC-${id}`], size: 'small', risky: false, ...extra })
const B_FAIL = 'FAIL src/b.js:3 expected 302, got 200'
const B_SIG = 'npm test | FAIL PATH expected N, got N'
const PASS = { passed: true, criteria: [], findings: [] }
const NO_CHALLENGE = { added: [], assign: [], escalations: [] }

// The check relay: return what wg would print for the command line in the prompt.
const unq = s => s.replace(/'\\''/g, "'")
function wgOutput(prompt, checker, label, n, changed) {
  const nonce = unq(prompt.match(/--nonce '((?:[^']|'\\'')*)'/)[1])
  const cmds = [...prompt.matchAll(/--cmd '((?:[^']|'\\'')*)'/g)].map(m => unq(m[1]))
  const results = cmds.map(cmd => {
    const out = checker(label, cmd, n)
    return out ? { cmd, exitCode: 1, outputTail: out } : { cmd, exitCode: 0, outputTail: 'ok' }
  })
  return JSON.stringify({ nonce, results, changedFiles: changed[label] || [] })
}

// checker(label, cmd, n) returns failure output (exit 1) or 0 (pass) for the n-th run of a check label.
// memory: null = no file, string = raw file text, object = parsed file. review(n) answers review#n.
// relay(label, honest) lets a test make the check relay lie; by default it returns wg's output.
async function run({
  units, checks = ['npm test'], checker = () => 0, changed = {}, task = 'add a thing', memory = null,
  innocent = () => false, escalations = [], challenge = NO_CHALLENGE, review = () => PASS, extraArgs = {}, dirty = [],
  relay = (label, honest) => honest,
}) {
  const calls = [], opts = {}, prompts = {}, seen = {}
  const recall = memory === null ? { exists: false, dirty } : { exists: true, dirty, content: typeof memory === 'string' ? memory : JSON.stringify(memory) }
  const agent = async (prompt, o) => {
    const { label } = o
    calls.push(label)
    opts[label] = o
    prompts[label] = prompt
    if (label === 'read') return { branch: 'work/test', taskId: 'T-001', initialized: true, risk: 'medium', checks, criteria: [], units, escalations }
    if (label === 'challenge') return challenge
    if (label.startsWith('review#')) return review(Number(label.slice(7)))
    if (label.startsWith('check:')) {
      const base = label.replace(/!retry$/, '')
      const n = (seen[base] = (seen[base] || 0) + 1)
      return { raw: relay(label, wgOutput(prompt, checker, base, n, changed)) }
    }
    if (label.startsWith('work:')) return { summary: 'done' }
    if (label.startsWith('fix:')) return innocent(label) ? { summary: '', blocked: 'not caused by my unit' } : { summary: 'patched it' }
    if (label === 'learn') {
      const items = JSON.parse(prompt.split('EVIDENCE:\n')[1])
      return {
        rules: items.filter(it => it.sig).map(it => ({ sig: it.sig, text: `rule for ${it.sig}`, scope: 'worker' })),
        proposals: items.some(it => it.reviewFindings) ? [{ category: 'automated-check', proposal: 'lint rule', evidence: 'RF-1' }] : [],
      }
    }
    throw new Error(`unexpected agent ${label}`)
  }
  const parallel = thunks => Promise.all(thunks.map(t => t().catch(() => null)))
  const args = { task, haruspexCli: CLI, wg: WG, runId: 'run-abc123', memory: recall, ...extraArgs }
  const result = await script(agent, parallel, null, () => {}, () => {}, args, { total: null })
  const model = label => opts[label] && opts[label].model
  return { result, calls, opts, prompts, saved: result.memoryToSave, model }
}

test('graph: Reading, challenge, parallel layer, blamed-unit fix, dependent unit, review, memory', async () => {
  const { result, calls } = await run({
    units: [unit('a'), unit('b'), unit('c', { dependsOn: ['a', 'b'] })],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
    changed: {
      'check:baseline': ['notes.txt', '.haruspex/state.json'],
      'check:L2': ['notes.txt', '.haruspex/tasks/T-001.json', 'src/a.js', 'src/b.js', 'src/c.js', 'stray.txt'],
    },
  })
  assert.deepEqual(calls, [
    'read', 'challenge', 'check:baseline',
    'work:a', 'work:b', 'check:L1', 'fix:b#1', 'check:L1', 'work:c', 'check:L2',
    'review#1', 'learn',
  ])
  assert.equal(result.status, 'ready_for_review')
  assert.equal(result.reviewed, true)
  assert.deepEqual(result.done, ['a', 'b', 'c'])
  assert.deepEqual(result.outOfScope, ['stray.txt'], 'Haruspex records never count as out of scope')
  assert.deepEqual(result.haruspex, { taskId: 'T-001', initialized: true, risk: 'medium', challengeAdded: [] })
})

test('the user\'s uncommitted work and the Haruspex init files are never counted as the run\'s scope slips', async () => {
  const { result } = await run({
    units: [unit('a')],
    dirty: ['wip.md'],
    changed: { 'check:L1': ['wip.md', 'AGENTS.md', 'CLAUDE.md', '.haruspex/project.json', 'src/a.js', 'surprise.js'] },
  })
  assert.deepEqual(result.userChangesBefore, ['wip.md'])
  assert.deepEqual(result.outOfScope, ['surprise.js'], 'AGENTS.md and CLAUDE.md count as harness files because init ran')
})

test('a plan that edits files holding the user\'s uncommitted work escalates before any edit', async () => {
  const { result, calls, prompts } = await run({ units: [unit('a'), unit('b')], dirty: ['src/b.js', 'notes.md'] })
  assert.deepEqual(calls, ['read', 'challenge'])
  assert.equal(result.status, 'needs_decision')
  assert.deepEqual(result.escalations.map(e => e.id), ['DIRTY'])
  assert.match(result.escalations[0].question, /src\/b\.js/)
  assert.deepEqual(result.userChangesBefore, ['src/b.js', 'notes.md'])
  assert.match(prompts.read, /uncommitted work\. Plan no edits to them unless the task truly needs it: src\/b\.js, notes\.md/)
})

test('a rerun keeps the first run\'s view of the user\'s changes and the init, and honours DIRTY', async () => {
  const { result, calls } = await run({
    units: [unit('a'), unit('b')],
    dirty: ['src/b.js', 'notes.md', 'AGENTS.md', 'CLAUDE.md', '.haruspex/state.json'], // dirty now because of the first run
    changed: { 'check:L1': ['src/a.js', 'src/b.js', 'AGENTS.md', 'CLAUDE.md', '.haruspex/state.json'] },
    extraArgs: { taskId: 'T-001', initialized: true, userChangesBefore: ['src/b.js', 'notes.md'], decisions: [{ id: 'DIRTY', answer: 'Edit on top' }] },
  })
  assert.ok(calls.includes('work:b'), 'the user chose to let the run edit on top')
  assert.deepEqual(result.userChangesBefore, ['notes.md'], 'src/b.js now goes into the commit with the run')
  assert.deepEqual(result.outOfScope, [])
  assert.equal(result.haruspex.initialized, true)
})

test('allocation: fixed models for fixed jobs; workers probe haiku once, sonnet once, then default; fixes go one tier up', async () => {
  const { result, model, opts } = await run({
    units: [unit('a'), unit('b'), unit('c', { dependsOn: ['a', 'b'] })],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
  })
  assert.deepEqual([model('read'), opts.read.effort], ['opus', 'high'])
  assert.deepEqual([model('challenge'), opts.challenge.effort], ['sonnet', 'high'])
  assert.equal(model('review#1'), 'opus')
  assert.equal(model('check:baseline'), 'haiku')
  assert.equal(model('work:a'), 'haiku')
  assert.equal(model('work:b'), 'sonnet')
  assert.equal(model('fix:b#1'), 'opus')
  assert.equal(model('work:c'), 'sonnet')
  assert.equal(model('learn'), 'sonnet')
  assert.deepEqual(result.models.map(m => m.tier), ['haiku', 'sonnet', 'sonnet'])
})

test('Matt Pocock skills: workers test-first, fixers diagnose, reviewer uses code-review, learn uses retro', async () => {
  const { prompts } = await run({
    units: [unit('b')],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
  })
  assert.match(prompts['work:b'], /mattpocock-skills:tdd/)
  assert.match(prompts['work:b'], /AC-b/)
  assert.match(prompts['work:b'], /\.haruspex\/tasks\/T-001\.json/)
  assert.match(prompts['fix:b#1'], /mattpocock-skills:diagnosing-bugs/)
  assert.match(prompts['review#1'], /mattpocock-skills:code-review/)
  assert.match(prompts['review#1'], /independent_review/)
  assert.match(prompts.learn, /mattpocock-skills:retro/)
  assert.ok(prompts.read.includes(`python3 ${CLI} init .`), 'the Reading inits Haruspex with the given CLI')
  assert.ok(prompts.read.includes(`python3 ${CLI} gate ready-to-build .`))
})

test('a critical escalation stops the run before any edit, after the challenge pass', async () => {
  const esc = { id: 'FM-002', severity: 'critical', question: 'Which timezone do reports use?', recommendation: 'UTC' }
  const { result, calls, saved } = await run({
    units: [unit('a')],
    escalations: [esc],
    challenge: { added: [{ id: 'ASM-003', summary: 'retry safety' }], assign: [], escalations: [{ ...esc, id: 'ASM-003', severity: 'high' }] },
  })
  assert.deepEqual(calls, ['read', 'challenge'])
  assert.equal(result.status, 'needs_decision')
  assert.deepEqual(result.escalations.map(e => e.id), ['FM-002', 'ASM-003'])
  assert.equal(result.taskId, 'T-001')
  assert.equal(saved, undefined, 'nothing happened, so nothing is learned or saved')
})

test('checks are run by wg: the relay prompt is one quoted wg command with a per-call nonce', async () => {
  const { prompts, opts } = await run({ units: [unit('a')], checks: ["npm test -- --grep 'it''s'"] })
  const p = prompts['check:L1']
  assert.ok(p.includes(`'${WG}' check --nonce 'run-abc123:check:L1:1' --cmd 'npm test -- --grep '\\''it'\\'''\\''s'\\'''`), p)
  assert.deepEqual([opts['check:L1'].model, opts['check:L1'].effort], ['haiku', 'low'])
})

test('a relay that paraphrases gets one retry; the honest second answer is used', async () => {
  const { result, calls } = await run({
    units: [unit('a')],
    relay: (label, honest) => (label === 'check:L1' ? 'All tests passed!' : honest),
  })
  assert.ok(calls.includes('check:L1!retry'))
  assert.equal(result.status, 'ready_for_review')
})

test('a relay that keeps inventing results stops the run instead of acting on them', async () => {
  const forged = honest => JSON.stringify({ ...JSON.parse(honest), nonce: 'guessed' })
  await assert.rejects(
    run({ units: [unit('a')], relay: (label, honest) => (label.startsWith('check:L1') ? forged(honest) : honest) }),
    /check:L1: no verified check result/,
  )
  const missing = honest => JSON.stringify({ ...JSON.parse(honest), results: [] })
  await assert.rejects(
    run({ units: [unit('a')], relay: (label, honest) => (label.startsWith('check:baseline') ? missing(honest) : honest) }),
    /check:baseline: no verified check result/,
  )
})

test('a rerun carries the user\'s decisions and the task id into the Reading', async () => {
  const { prompts } = await run({
    units: [unit('a')],
    extraArgs: { taskId: 'T-001', decisions: [{ id: 'FM-002', answer: 'Reports use UTC' }, { id: 'bad' }] },
  })
  assert.ok(prompts.read.includes('- FM-002: Reports use UTC'))
  assert.ok(prompts.read.includes(`task activate T-001 .`))
  assert.ok(!prompts.read.includes('- bad:'), 'a decision without an answer is dropped')
})

test('criteria the challenge adds are merged into the unit that owns them', async () => {
  const { prompts } = await run({
    units: [unit('a'), unit('b')],
    challenge: { added: [{ id: 'AC-009', kind: 'criterion', summary: 'empty input' }], assign: [{ criterionId: 'AC-009', unitId: 'a' }, { criterionId: 'AC-010', unitId: 'zz' }], escalations: [] },
  })
  assert.match(prompts['work:a'], /AC-a, AC-009/)
  assert.doesNotMatch(prompts['work:b'], /AC-009/)
})

test('review rejection: one fix round on the unit the finding names, recheck, re-review, then proposals', async () => {
  const { result, calls, model } = await run({
    units: [unit('a'), unit('b')],
    review: n => (n === 1
      ? { passed: false, criteria: [{ id: 'AC-b', status: 'failed' }], findings: [{ id: 'RF-1', severity: 'high', file: 'src/b.js', summary: 'no error path' }, { id: 'RF-2', severity: 'low', file: 'src/a.js', summary: 'naming' }] }
      : PASS),
  })
  assert.deepEqual(calls.slice(-5), ['review#1', 'fix:b#r1', 'check:review1', 'review#2', 'learn'])
  assert.equal(model('fix:b#r1'), 'opus', 'b was built on sonnet, so its review fix runs one tier up')
  assert.equal(result.status, 'ready_for_review')
  assert.equal(result.reviewFixRounds, 1)
  assert.deepEqual(result.proposals, [{ category: 'automated-check', proposal: 'lint rule', evidence: 'RF-1' }])
})

test('a review that still rejects after its fix round leaves the run stuck', async () => {
  const reject = { passed: false, criteria: [{ id: 'AC-a', status: 'failed' }], findings: [] }
  const { result, calls } = await run({ units: [unit('a')], review: () => reject })
  assert.ok(calls.includes('review#2'))
  assert.ok(!calls.includes('review#3'))
  assert.equal(result.status, 'stuck')
  assert.match(result.stuck[0].why, /independent review/)
  assert.equal(result.reviewed, false)
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
  assert.equal(model('work:a'), 'sonnet')
  assert.equal(model('work:b'), 'haiku')
  assert.deepEqual(saved.models['worker:small'].sonnet, { pass: 19.05, fail: 0.95 })
  assert.deepEqual(saved.models['worker:large'], { haiku: { pass: 1, fail: 0 } })
})

test('a new rule is born from a fixed failure, keyed by its signature', async () => {
  const { result, saved } = await run({
    units: [unit('b')],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? B_FAIL : 0),
  })
  assert.deepEqual(result.rules.added, ['r1'])
  assert.deepEqual(saved.rules, [{ id: 'r1', text: `rule for ${B_SIG}`, scope: 'worker', sig: B_SIG, a: 2, b: 1 }])
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
  assert.ok(prompts.read.includes('Split migrations from code') && prompts.read.includes('Keep handlers pure'))
  assert.ok(!prompts.read.includes('Auth redirects'), 'worker rules stay out of the Reading prompt')
  assert.ok(prompts.read.includes('Check commands that worked here before: npm test'))

  const byId = Object.fromEntries(saved.rules.map(r => [r.id, r]))
  assert.equal(byId.r1.b, 2)
  assert.equal(byId.r1.text, `rule for ${B_SIG}`)
  assert.equal(byId.r2.a, 1.25)
  assert.equal(byId.r4.a, 2.25)
  assert.equal(byId.r3, undefined)
  assert.deepEqual(result.rules.revised, ['r1'])
  assert.deepEqual(result.rules.pruned, ['r3'])
})

test('a fixer that proves its unit innocent keeps the failure off that unit\'s model stats', async () => {
  const { saved, calls } = await run({
    units: [unit('a'), unit('b')],
    checker: (label, cmd, n) => (label === 'check:L1' && n === 1 ? 'FAIL some test, no file named' : 0),
    innocent: label => label === 'fix:a#1',
  })
  assert.ok(calls.includes('fix:a#1') && calls.includes('fix:b#1'))
  assert.deepEqual(saved.models['worker:small'], { haiku: { pass: 1, fail: 0 }, sonnet: { pass: 0, fail: 1 } })
})

test('a risky unit and everything depending on it are skipped', async () => {
  const { result, calls } = await run({
    units: [unit('a', { risky: true, riskReason: 'migration' }), unit('b', { dependsOn: ['a'] }), unit('c')],
  })
  assert.deepEqual(calls, ['read', 'challenge', 'check:baseline', 'work:c', 'check:L1', 'review#1'])
  assert.deepEqual(result.skipped.map(s => s.id), ['a', 'b'])
  assert.match(result.skipped[0].why, /risky/)
})

test('a layer that never passes escalates haiku → sonnet → opus → opus at high effort, then stops without review', async () => {
  const { result, calls, opts, saved } = await run({
    units: [unit('a'), unit('c', { dependsOn: ['a'] })],
    checker: label => (label === 'check:L1' ? 'boom in src/a.js' : 0),
  })
  assert.deepEqual(calls, [
    'read', 'challenge', 'check:baseline', 'work:a',
    'check:L1', 'fix:a#1', 'check:L1', 'fix:a#2', 'check:L1', 'fix:a#3', 'check:L1',
    'learn',
  ])
  assert.deepEqual(['work:a', 'fix:a#1', 'fix:a#2', 'fix:a#3'].map(l => opts[l].model), ['haiku', 'sonnet', 'opus', 'opus'])
  assert.equal(opts['fix:a#3'].effort, 'high')
  assert.equal(result.status, 'stuck')
  assert.equal(result.review, null)
  assert.deepEqual(saved.rules, [], 'never fixed, so no rule')
})

test('a command already failing before any change is left out of the gate', async () => {
  const { result } = await run({
    units: [unit('a')],
    checks: ['npm test', 'npm run lint'],
    checker: (label, cmd) => (cmd === 'npm run lint' ? 'lint: 40 problems' : 0),
  })
  assert.deepEqual(result.checks, ['npm test'])
  assert.deepEqual(result.checksAlreadyFailing, ['npm run lint'])
  assert.equal(result.status, 'ready_for_review')
})

test('a corrupt memory file is never overwritten', async () => {
  const { result, calls } = await run({ units: [unit('a')], memory: '{not json' })
  assert.equal(result.memoryToSave, null, 'nothing is handed to wg save, so the file stays as it is')
  assert.ok(calls.length > 0)
})

test('bad input fails fast before any agent runs', async () => {
  await assert.rejects(run({ units: [unit('a')], task: '  ' }), /no task given/)
  await assert.rejects(run({ units: [unit('a')], extraArgs: { haruspexCli: 'haruspex.py' } }), /absolute path to haruspex/)
  await assert.rejects(run({ units: [unit('a')], extraArgs: { wg: 'wg' } }), /absolute path to the wg binary/)
  await assert.rejects(run({ units: [unit('a')], extraArgs: { runId: 'x;rm' } }), /runId/)
})
