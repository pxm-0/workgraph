export const meta = {
  name: 'run',
  description: 'Split a coding task into a graph of units, run each on the cheapest model proven reliable in this repo, loop on real checks with model escalation, then learn weighted rules and model stats',
  whenToUse: 'A coding task in the current git repo that should run end to end, leaving only the final diff for you to approve',
  phases: [
    { title: 'Recall', detail: 'load rules and model stats for this repo', model: 'haiku' },
    { title: 'Plan', detail: 'split into units, find the check commands', model: 'opus' },
    { title: 'Check', detail: 'run the check commands; exit codes decide', model: 'haiku' },
    { title: 'Build', detail: 'one worker per unit on its allocated model' },
    { title: 'Fix', detail: 'only the failing unit goes back, one model tier up per round' },
    { title: 'Learn', detail: 'score rules, update model stats, save memory' },
  ],
}

const MEMORY = '.claude/work-memory.json'
const TIERS = ['haiku', 'sonnet', 'opus'] // cheapest first
const MAX_UNITS = 5
const MAX_FIX_ROUNDS = 3
const MIN_OBS = 3 // tries before a tier is trusted or ruled out
const TARGET = 0.75 // first-pass rate a tier needs to keep a job
const WINDOW = 20 // evidence cap, so old results fade as new ones arrive
const RULES_PER_PROMPT = 8
const MIN_WEIGHT = 0.4 // rules below this stay in memory but out of prompts
const PRUNE_BELOW = 0.3 // ...and are deleted once they have 4+ observations
const MAX_RULES = 30
const HELD = 0.25 // credit when a rule was in play and its failure stayed away
const RECURRED = 1 // penalty when its failure came back anyway

const task = String((typeof args === 'string' ? args : args && args.task) || '').trim()
if (!task) throw new Error('work: no task given. Pass args: {task: "what to do"}')

const str = { type: 'string' }
const strs = { type: 'array', items: str }
const SCOPE = { type: 'string', enum: ['planner', 'worker', 'all'] }
const LOAD = { type: 'object', properties: { exists: { type: 'boolean' }, content: str }, required: ['exists'] }
const PLAN = {
  type: 'object',
  properties: {
    branch: str,
    note: str,
    checks: strs,
    units: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: str, goal: str, files: strs, dependsOn: strs,
          size: { type: 'string', enum: ['small', 'large'] },
          risky: { type: 'boolean' }, riskReason: str,
        },
        required: ['id', 'goal', 'files', 'dependsOn', 'size', 'risky'],
      },
    },
  },
  required: ['branch', 'checks', 'units'],
}
const WORK = { type: 'object', properties: { summary: str, blocked: str }, required: ['summary'] }
const CHECK = {
  type: 'object',
  properties: {
    results: {
      type: 'array',
      items: { type: 'object', properties: { exitCode: { type: 'integer' }, outputTail: str }, required: ['exitCode', 'outputTail'] },
    },
    changedFiles: strs,
  },
  required: ['results', 'changedFiles'],
}
const LEARN = {
  type: 'object',
  properties: { rules: { type: 'array', items: { type: 'object', properties: { sig: str, text: str, scope: SCOPE }, required: ['sig', 'text', 'scope'] } } },
  required: ['rules'],
}

const r2 = x => Math.round(x * 100) / 100
const weight = r => r.a / (r.a + r.b)
const tally = s => {
  const pass = s ? s.pass : 0, fail = s ? s.fail : 0
  return { n: pass + fail, rate: (pass + 1) / (pass + fail + 2) }
}
const capPair = (x, y) => (x + y > WINDOW ? [r2((x * WINDOW) / (x + y)), r2((y * WINDOW) / (x + y))] : [r2(x), r2(y)])
const statKey = u => `worker:${u.size === 'large' ? 'large' : 'small'}`
const isRule = r => r && typeof r.id === 'string' && typeof r.text === 'string' && typeof r.sig === 'string' &&
  ['planner', 'worker', 'all'].includes(r.scope) && Number.isFinite(r.a) && r.a > 0 && Number.isFinite(r.b) && r.b > 0

// A failure's identity across runs: the command plus its first error line with paths and numbers stripped.
function signature(f) {
  const lines = f.outputTail.split('\n').filter(l => l.trim())
  const line = lines.find(l => /error|fail|expected|assert|exception|cannot|undefined|not found/i.test(l)) || lines.pop() || ''
  return `${f.cmd} | ${line.replace(/\S*[/\\]\S*/g, 'PATH').replace(/\d+/g, 'N').replace(/\s+/g, ' ').trim().slice(0, 120)}`
}

// Kahn layering: units in one layer never depend on each other, so they run in parallel.
function layersOf(units) {
  const ids = new Set(units.map(u => u.id))
  const pending = new Map(units.map(u => [u.id, u]))
  const placed = new Set()
  const layers = []
  while (pending.size) {
    const ready = [...pending.values()].filter(u => u.dependsOn.every(d => placed.has(d) || !ids.has(d)))
    if (!ready.length) {
      log(`dependency cycle among ${[...pending.keys()].join(', ')}; running them one at a time`)
      for (const u of pending.values()) layers.push([u])
      break
    }
    layers.push(ready)
    ready.forEach(u => { pending.delete(u.id); placed.add(u.id) })
  }
  return layers
}

// shortcut: blames units whose paths appear in the failure output, else the whole layer; add a triage agent if blame is often wrong.
function blame(failed, units) {
  const text = failed.map(f => f.outputTail).join('\n')
  const hit = units.filter(u => u.files.some(f => text.includes(f) || text.includes(f.split('/').pop())))
  return hit.length ? hit : units
}

// The checker only relays exit codes; it never judges. A command it fails to report counts as failed.
async function runChecks(cmds, label) {
  const r = await agent(
    (cmds.length
      ? `Run these shell commands from the repo root, one at a time, in this order, exactly as written, each with a 10-minute timeout:\n${cmds.map(c => `- ${c}`).join('\n')}\n` +
        'Report one result per command, in the same order: its exit code and the last 40 lines of its output. '
      : 'There are no check commands; report results: []. ') +
      'Do not edit, create or delete any file, and do not try to fix anything. ' +
      'Finally run `git status --porcelain` and report every changed or untracked path, repo-relative, path only.',
    { label, phase: 'Check', schema: CHECK, model: 'haiku', effort: 'low' })
  if (!r) throw new Error(`${label}: the checker agent died, so nothing can be verified. Edits so far are uncommitted in the working tree.`)
  const results = cmds.map((cmd, i) => ({ cmd, ...(r.results[i] || { exitCode: -1, outputTail: 'checker did not report this command' }) }))
  return { failed: results.filter(x => x.exitCode !== 0), changedFiles: r.changedFiles }
}

// ---- Recall: the sandbox has no file access, so a cheap agent reads the repo's memory file.
phase('Recall')
const loaded = await agent(
  `If ${MEMORY} exists, return exists=true and its exact contents in "content". If it does not exist, return exists=false. Do not create or change any file.`,
  { label: 'memory:load', phase: 'Recall', schema: LOAD, model: 'haiku', effort: 'low' })
let memory = { version: 1, runs: 0, nextId: 1, checks: [], rules: [], models: {} }
let canSave = true
if (!loaded) {
  canSave = false
  log(`could not read ${MEMORY}; running without memory and not saving, so nothing gets overwritten`)
} else if (loaded.exists) {
  try {
    const m = JSON.parse(loaded.content)
    const rules = Array.isArray(m.rules) ? m.rules.filter(isRule) : []
    if (Array.isArray(m.rules) && rules.length < m.rules.length) log(`ignored ${m.rules.length - rules.length} malformed rule(s) in ${MEMORY}`)
    memory = {
      version: 1,
      runs: Number.isFinite(m.runs) ? m.runs : 0,
      nextId: Number.isFinite(m.nextId) ? m.nextId : rules.length + 1,
      checks: Array.isArray(m.checks) ? m.checks.filter(c => typeof c === 'string') : [],
      rules,
      models: m.models && typeof m.models === 'object' ? m.models : {},
    }
  } catch (e) {
    canSave = false
    log(`${MEMORY} is not valid JSON (${e.message}); left untouched, this run will not learn`)
  }
}

// ---- Weighting: only the strongest rules for a role reach its prompt, and we remember which ones did.
const inPlay = new Set()
function ruleBlock(role) {
  const rs = memory.rules
    .filter(r => (r.scope === role || r.scope === 'all') && weight(r) >= MIN_WEIGHT)
    .sort((x, y) => weight(y) - weight(x))
    .slice(0, RULES_PER_PROMPT)
  rs.forEach(r => inPlay.add(r.id))
  return rs.length ? `\nRules learned in this repo, strongest first. Follow them:\n${rs.map(r => `- ${r.text}`).join('\n')}\n` : ''
}

// ---- Allocation: cheapest tier proven on this kind of unit; each run probes one unproven cheaper tier once.
const probed = new Set()
function pickTier(key) {
  const st = t => tally((memory.models[key] || {})[t])
  const proven = TIERS.find(t => st(t).n >= MIN_OBS && st(t).rate >= TARGET)
  const ceiling = TIERS.indexOf(proven || 'opus') // opus is never probed, only escalated to
  const probe = TIERS.slice(0, ceiling).find(t => st(t).n < MIN_OBS && !probed.has(`${key}:${t}`))
  if (probe) { probed.add(`${key}:${probe}`); return probe }
  if (proven) return proven
  return st('sonnet').n >= MIN_OBS ? 'opus' : 'sonnet' // nothing proven: sonnet, unless it already proved too weak
}
const fixTier = (u, round) => TIERS[Math.min(TIERS.indexOf(u.tier) + round, TIERS.length - 1)]

const planPrompt = `Task: ${task}
${ruleBlock('planner')}${memory.checks.length ? `\nCheck commands that worked here before: ${memory.checks.join(' | ')}. Reuse them if they still fit.\n` : ''}
You are the planner of a build graph. Workers do the edits; you only read and plan.
1. Read the code this task touches.
2. If the current branch is the repo's default branch, create and switch to a new branch named work/<short-slug>. Never commit, stash, reset or discard anything; uncommitted changes stay as they are. Return the branch name.
3. Split the task into the fewest units that make sense, at most ${MAX_UNITS}. A unit is one coherent change and lists every repo-relative file it may create or edit, including its tests. Units that touch the same file must be one unit or depend on each other. Add a dependsOn only when a unit truly needs another unit's output: independent units run in parallel.
4. Give each unit a size: "small" for a contained edit to one or two files, "large" for new logic, several files or tricky behavior. Size picks the model that builds it.
5. checks: the non-interactive shell commands that prove this change works here (targeted tests, typecheck, lint, build), taken from the repo's own scripts, CI config or dev stack. Each must exit 0 on success. Return [] if the repo has none.
6. Set risky=true with a riskReason for any unit that is hard to undo or reaches outside the repo: schema migrations, deleting data or files, deploys, payments, auth or permission changes, secrets, CI config, force pushes. Risky units are not run; the user does them.
If the task is unclear or needs no change, return units: [] and say why in note.`

const workPrompt = u => `Overall task: ${task}

You are one worker in a build graph. Your unit (${u.id}): ${u.goal}
Edit only these files: ${u.files.join(', ')}
Other workers are editing other files at the same time, so touch nothing else. If the unit can't be done within these files, change nothing and explain why in "blocked".
${ruleBlock('worker')}Add or update tests for the behavior you change. Do not commit and do not run the full check suite; a separate checker does that.`

const fixPrompt = (u, failed, round) => `Overall task: ${task}

Your unit (${u.id}): ${u.goal}
Files you may edit: ${u.files.join(', ')}
After the build, these check commands failed (fix round ${round} of ${MAX_FIX_ROUNDS}):

${failed.map(f => `$ ${f.cmd}   (exit ${f.exitCode})\n${f.outputTail.slice(-3000)}`).join('\n\n')}
${ruleBlock('worker')}
Find the root cause and fix it within your files. Never weaken, skip or delete a test or check to make it pass. If your unit did not cause the failure, change nothing and say so in "blocked".`

const learnPrompt = items => `Each item below is a check failure that a fixer resolved in this run. Write one rule per item: a single imperative sentence a future planner or worker in this repo can follow so this failure does not happen again. Be specific to this repo: name the module, convention or command. If "existingRule" is set, that rule was already known and the failure happened anyway, so rewrite it to be the rule that would have prevented this. Set scope to "planner" if the rule is about how to split or size work, "worker" if it is about how to write the code, "all" if both. Copy each item's sig exactly. Generic advice like "write tests" is not a rule.

EVIDENCE:
${JSON.stringify(items, null, 2)}`

// ---- Plan
phase('Plan')
const plan = await agent(planPrompt, { label: 'plan', phase: 'Plan', schema: PLAN, model: 'opus', effort: 'high' })
if (!plan) throw new Error('work: the planner died; nothing was changed')
if (!plan.units.length) return { status: 'nothing_to_do', branch: plan.branch, note: plan.note || 'the planner returned no units' }
const units = plan.units

phase('Check')
const base = await runChecks(plan.checks, 'check:baseline')
const leftOut = base.failed.map(f => f.cmd)
const gate = plan.checks.filter(c => !leftOut.includes(c))
const preexisting = new Set(base.changedFiles)
if (leftOut.length) log(`already failing before any change, left out of the gate: ${leftOut.join(' | ')}`)
if (!gate.length) log('no passing automated check here: units run once and the result is unverified')

// ---- Build, check, fix: layer by layer
const done = [], skipped = [], stuck = [], fixes = [], allocation = [], outcomes = []
const fixedSigs = new Set()
const blocked = new Set()
let changed = base.changedFiles
let stopped = false

for (const [i, layer] of layersOf(units).entries()) {
  const L = `L${i + 1}`
  const runnable = []
  for (const u of layer) {
    const dep = u.dependsOn.find(d => blocked.has(d))
    const why = stopped ? 'run stopped after an earlier layer failed its checks'
      : u.risky ? `risky, yours to do: ${u.riskReason || 'hard to undo'}`
      : dep ? `depends on ${dep}, which did not finish` : null
    if (why) { blocked.add(u.id); skipped.push({ id: u.id, goal: u.goal, why }) } else runnable.push(u)
  }
  if (!runnable.length) continue

  const assigned = runnable.map(u => ({ ...u, tier: pickTier(statKey(u)) }))
  assigned.forEach(u => allocation.push({ id: u.id, size: u.size, tier: u.tier }))
  phase('Build')
  const built = await parallel(assigned.map(u => () =>
    agent(workPrompt(u), { label: `work:${u.id}`, phase: 'Build', schema: WORK, model: u.tier })))
  const ok = []
  assigned.forEach((u, j) => {
    const w = built[j]
    if (w && !w.blocked) ok.push(u)
    else { blocked.add(u.id); stuck.push({ id: u.id, goal: u.goal, why: w ? `blocked: ${w.blocked}` : 'worker died' }) } // a planning problem, not model evidence
  })
  if (!ok.length) continue

  phase('Check')
  let check = await runChecks(gate, `check:${L}`)
  const needsFix = new Set()
  const layerSigs = []
  let round = 0
  while (check.failed.length && round < MAX_FIX_ROUNDS) {
    round++
    const culprits = blame(check.failed, ok)
    log(`${L}: ${check.failed.map(f => f.cmd).join(' | ')} failed; fix ${round}/${MAX_FIX_ROUNDS} for ${culprits.map(u => `${u.id} on ${fixTier(u, round)}`).join(', ')}`)
    phase('Fix')
    const fixed = await parallel(culprits.map(u => () =>
      agent(fixPrompt(u, check.failed, round), {
        label: `fix:${u.id}#${round}`, phase: 'Fix', schema: WORK, model: fixTier(u, round),
        ...(round === MAX_FIX_ROUNDS ? { effort: 'high' } : {}),
      })))
    culprits.forEach((u, k) => { if (!(fixed[k] && fixed[k].blocked)) needsFix.add(u.id) }) // a fixer that proves innocence clears its unit
    const failed = check.failed.map(f => ({ cmd: f.cmd, exitCode: f.exitCode, sig: signature(f), outputTail: f.outputTail.slice(-1000) }))
    layerSigs.push(...failed.map(f => f.sig))
    fixes.push({
      layer: L, round, units: culprits.map(u => u.id), failed,
      fixSummaries: fixed.map(f => (f ? (f.blocked ? `blocked: ${f.blocked}` : f.summary) : 'fixer died')),
    })
    phase('Check')
    check = await runChecks(gate, `check:${L}`)
  }
  changed = check.changedFiles
  ok.forEach(u => outcomes.push({ key: statKey(u), tier: u.tier, pass: !needsFix.has(u.id) }))
  if (check.failed.length) {
    const why = `checks still failing after ${MAX_FIX_ROUNDS} fix rounds: ${check.failed.map(f => f.cmd).join(' | ')}`
    ok.forEach(u => { blocked.add(u.id); stuck.push({ id: u.id, goal: u.goal, why }) })
    stopped = true
  } else {
    layerSigs.forEach(s => fixedSigs.add(s))
    ok.forEach(u => done.push(u.id))
  }
}

const planned = units.flatMap(u => u.files)
const inPlan = f => planned.some(p => p === f || (f.endsWith('/') && p.startsWith(f)))
const outOfScope = changed.filter(f => !preexisting.has(f) && f !== MEMORY && !inPlan(f))
if (outOfScope.length) log(`changed outside the plan: ${outOfScope.join(', ')}`)

// ---- Learn
phase('Learn')
// 1. Credit assignment: a rule in play gains a little when its failure stays away and loses a lot when it comes back.
const recurred = new Set(fixes.flatMap(x => x.failed.map(f => f.sig)))
const scored = memory.rules.map(r => {
  if (!inPlay.has(r.id)) return r
  const [a, b] = recurred.has(r.sig) ? capPair(r.a, r.b + RECURRED) : capPair(r.a + HELD, r.b)
  return { ...r, a, b }
})

// 2. Every failure this run fixed becomes a rule, or rewrites the rule that failed to prevent it.
const evidence = [...fixedSigs].map(sig => {
  const x = fixes.find(fx => fx.failed.some(f => f.sig === sig))
  const f = x.failed.find(ff => ff.sig === sig)
  const existing = scored.find(r => r.sig === sig)
  return { sig, cmd: f.cmd, outputTail: f.outputTail, fixSummaries: x.fixSummaries, existingRule: existing ? existing.text : null }
})
let phrased = []
if (evidence.length && canSave) {
  const res = await agent(learnPrompt(evidence), { label: 'learn', phase: 'Learn', schema: LEARN, model: 'sonnet' })
  if (res) phrased = res.rules.filter(p => fixedSigs.has(p.sig) && p.text.trim())
  else log('learn agent died; no new rules this run')
}
let nextId = memory.nextId
const added = [], revised = []
const rewritten = scored.map(r => {
  const p = phrased.find(x => x.sig === r.sig)
  if (!p) return r
  revised.push(r.id)
  return { ...r, text: p.text.trim(), scope: p.scope }
})
const fresh = phrased
  .filter((p, k) => !scored.some(r => r.sig === p.sig) && phrased.findIndex(q => q.sig === p.sig) === k)
  .map(p => { const id = `r${nextId++}`; added.push(id); return { id, text: p.text.trim(), scope: p.scope, sig: p.sig, a: 2, b: 1 } }) // born from a fix that worked

// 3. Prune weak rules that have had a fair trial, keep the strongest MAX_RULES.
const rules = [...rewritten, ...fresh]
  .filter(r => !(weight(r) < PRUNE_BELOW && r.a + r.b >= 4))
  .sort((x, y) => weight(y) - weight(x))
  .slice(0, MAX_RULES)
const pruned = [...rewritten, ...fresh].filter(r => !rules.includes(r)).map(r => r.id)

// 4. Model stats: did the allocated tier get its unit through checks without a fix?
const models = outcomes.reduce((m, o) => {
  const cur = (m[o.key] || {})[o.tier] || { pass: 0, fail: 0 }
  const [pass, fail] = capPair(cur.pass + (o.pass ? 1 : 0), cur.fail + (o.pass ? 0 : 1))
  return { ...m, [o.key]: { ...(m[o.key] || {}), [o.tier]: { pass, fail } } }
}, memory.models)

const nextMemory = { version: 1, runs: memory.runs + 1, nextId, checks: gate.length ? gate : memory.checks, rules, models }
let memorySaved = false
if (canSave) {
  memorySaved = !!(await agent(
    `Replace ${MEMORY} with exactly the JSON between the <<< and >>> lines, creating the .claude folder if needed. Change no other file. Return "saved".\n<<<\n${JSON.stringify(nextMemory, null, 2)}\n>>>`,
    { label: 'memory:save', phase: 'Learn', model: 'haiku', effort: 'low' }))
  if (!memorySaved) log(`memory save failed; ${MEMORY} unchanged`)
}

return {
  status: stuck.length ? 'stuck' : 'ready_for_review',
  branch: plan.branch,
  verified: gate.length > 0,
  checks: gate,
  checksAlreadyFailing: leftOut,
  done,
  skipped,
  stuck,
  fixRounds: fixes.length,
  outOfScope,
  models: allocation,
  rules: {
    added, revised, pruned,
    inPlay: [...inPlay],
    top: rules.slice(0, 5).map(r => ({ id: r.id, weight: r2(weight(r)), scope: r.scope, text: r.text })),
  },
  memorySaved,
  next: 'Nothing is committed. Review the diff; commit and PR are your call.',
}
