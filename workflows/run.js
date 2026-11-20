export const meta = {
  name: 'run',
  description: 'Run a coding task as a graph under a Haruspex Reading: build units in parallel on the cheapest proven model, test-first, gate on real checks with model escalation, review independently against the criteria, then learn weighted rules',
  whenToUse: 'A coding task in the current git repo that should run end to end, leaving only critical decisions and the final diff to you',
  phases: [
    { title: 'Read', detail: 'Haruspex Reading, mitigations, units mapped to criteria', model: 'opus' },
    { title: 'Challenge', detail: 'adversarial pass on the Reading', model: 'sonnet' },
    { title: 'Check', detail: 'wg runs the check commands; a haiku pipe relays its verified output', model: 'haiku' },
    { title: 'Build', detail: 'one test-first worker per unit on its allocated model' },
    { title: 'Fix', detail: 'only the blamed unit goes back, one model tier up per round' },
    { title: 'Review', detail: 'independent Spec and Standards review against the criteria', model: 'opus' },
    { title: 'Learn', detail: 'score rules, update model stats, propose guardrails' },
  ],
}

const MEMORY = '.claude/work-memory.json'
const TIERS = ['haiku', 'sonnet', 'opus'] // cheapest first
const MAX_UNITS = 5
const MAX_FIX_ROUNDS = 3
const MAX_REVIEW_FIXES = 1 // review → fix → re-review cycles before the run stops
const MIN_OBS = 3 // tries before a tier is trusted or ruled out
const TARGET = 0.75 // first-pass rate a tier needs to keep a job
const WINDOW = 20 // evidence cap, so old results fade as new ones arrive
const RULES_PER_PROMPT = 8
const MIN_WEIGHT = 0.4 // rules below this stay in memory but out of prompts
const PRUNE_BELOW = 0.3 // ...and are deleted once they have 4+ observations
const MAX_RULES = 30
const HELD = 0.25 // credit when a rule was in play and its failure stayed away
const RECURRED = 1 // penalty when its failure came back anyway

const input = typeof args === 'string' ? { task: args } : args || {}
const task = String(input.task || '').trim()
const cli = String(input.haruspexCli || '').trim()
const decisions = Array.isArray(input.decisions) ? input.decisions.filter(d => d && d.id && d.answer) : []
const resumeTask = typeof input.taskId === 'string' && input.taskId.trim() ? input.taskId.trim() : null
if (!task) throw new Error('work: no task given. Pass args: {task: "what to do", haruspexCli: "/abs/path/haruspex.py"}')
if (!cli.startsWith('/')) throw new Error('work: args.haruspexCli must be the absolute path to haruspex.py')
const wg = String(input.wg || '').trim()
const runId = String(input.runId || '').trim()
if (!wg.startsWith('/')) throw new Error('work: args.wg must be the absolute path to the wg binary')
if (!/^[A-Za-z0-9-]{6,}$/.test(runId)) throw new Error('work: args.runId must be 6+ letters, digits or dashes')
const hx = `python3 ${cli}`
const shq = s => `'${String(s).replace(/'/g, `'\\''`)}'`

const str = { type: 'string' }
const strs = { type: 'array', items: str }
const bool = { type: 'boolean' }
const SEVERITY = { type: 'string', enum: ['low', 'medium', 'high', 'critical'] }
const SCOPE = { type: 'string', enum: ['planner', 'worker', 'all'] }
const ESCALATION = {
  type: 'object',
  properties: { id: str, severity: { type: 'string', enum: ['high', 'critical'] }, question: str, recommendation: str, options: strs },
  required: ['id', 'severity', 'question', 'recommendation'],
}
const RAW = { type: 'object', properties: { raw: str }, required: ['raw'] }
const READ = {
  type: 'object',
  properties: {
    branch: str,
    note: str,
    taskId: str,
    initialized: bool,
    risk: SEVERITY,
    checks: strs,
    criteria: { type: 'array', items: { type: 'object', properties: { id: str, text: str }, required: ['id', 'text'] } },
    units: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          id: str, goal: str, files: strs, dependsOn: strs, criteria: strs,
          size: { type: 'string', enum: ['small', 'large'] },
          risky: bool, riskReason: str,
        },
        required: ['id', 'goal', 'files', 'dependsOn', 'criteria', 'size', 'risky'],
      },
    },
    escalations: { type: 'array', items: ESCALATION },
  },
  required: ['branch', 'taskId', 'initialized', 'risk', 'checks', 'criteria', 'units', 'escalations'],
}
const CHALLENGE = {
  type: 'object',
  properties: {
    added: { type: 'array', items: { type: 'object', properties: { id: str, kind: str, summary: str }, required: ['id', 'summary'] } },
    assign: { type: 'array', items: { type: 'object', properties: { criterionId: str, unitId: str }, required: ['criterionId', 'unitId'] } },
    escalations: { type: 'array', items: ESCALATION },
  },
  required: ['added', 'assign', 'escalations'],
}
const WORK = { type: 'object', properties: { summary: str, blocked: str }, required: ['summary'] }
const REVIEW = {
  type: 'object',
  properties: {
    passed: bool,
    criteria: {
      type: 'array',
      items: { type: 'object', properties: { id: str, status: { type: 'string', enum: ['passed', 'failed'] }, evidence: strs }, required: ['id', 'status'] },
    },
    findings: {
      type: 'array',
      items: { type: 'object', properties: { id: str, severity: SEVERITY, file: str, summary: str }, required: ['id', 'severity', 'summary'] },
    },
  },
  required: ['passed', 'criteria', 'findings'],
}
const LEARN = {
  type: 'object',
  properties: {
    rules: { type: 'array', items: { type: 'object', properties: { sig: str, text: str, scope: SCOPE }, required: ['sig', 'text', 'scope'] } },
    proposals: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          category: { type: 'string', enum: ['automated-check', 'coding-standard', 'navigation', 'haruspex-profile'] },
          proposal: str,
          evidence: str,
        },
        required: ['category', 'proposal', 'evidence'],
      },
    },
  },
  required: ['rules', 'proposals'],
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
const SERIOUS = new Set(['high', 'critical'])

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

// Review findings name a file, failed criteria name a unit; anything unplaced goes to every built unit.
function blameReview(review, units) {
  const files = review.findings.filter(f => SERIOUS.has(f.severity) && f.file).map(f => f.file)
  const failedCriteria = review.criteria.filter(c => c.status === 'failed').map(c => c.id)
  const hit = units.filter(u => u.files.some(f => files.includes(f)) || u.criteria.some(c => failedCriteria.includes(c)))
  return hit.length ? hit : units
}

// wg runs the checks; the sandbox can't, so a haiku agent only relays wg's stdout. The output must
// parse, echo this call's nonce and cover every command, or it is rejected: a model can't invent results.
const checkCalls = new Map()
function parseCheck(raw, nonce, cmds) {
  try {
    const v = JSON.parse(String(raw).trim())
    const ok = v && v.nonce === nonce && Array.isArray(v.results) && v.results.length === cmds.length &&
      v.results.every((r, i) => r.cmd === cmds[i] && Number.isInteger(r.exitCode) && typeof r.outputTail === 'string') &&
      Array.isArray(v.changedFiles)
    return ok ? v : null
  } catch (e) {
    return null
  }
}
async function runChecks(cmds, label) {
  const n = (checkCalls.get(label) || 0) + 1
  checkCalls.set(label, n)
  const nonce = `${runId}:${label}:${n}`
  const command = `${shq(wg)} check --nonce ${shq(nonce)}${cmds.map(c => ` --cmd ${shq(c)}`).join('')}`
  const prompt = `Run exactly this one command from the repo root, then return its complete stdout, unchanged, as "raw". Do nothing else: no other commands, no edits, no summary.\n\n${command}`
  for (const attempt of [1, 2]) {
    const r = await agent(prompt, { label: attempt === 1 ? label : `${label}!retry`, phase: 'Check', schema: RAW, model: 'haiku', effort: 'low' })
    const v = r && parseCheck(r.raw, nonce, cmds)
    if (v) return { failed: v.results.filter(x => x.exitCode !== 0), changedFiles: v.changedFiles }
    log(`${label}: the check relay returned ${r ? 'output that is not wg\'s' : 'nothing'}${attempt === 1 ? '; retrying once' : ''}`)
  }
  throw new Error(`${label}: no verified check result, so nothing can be trusted. Edits so far are uncommitted in the working tree.`)
}

// ---- Recall: the skill runs `wg recall` before the workflow and passes its output as args.memory.
const loaded = input.memory && typeof input.memory === 'object' && Array.isArray(input.memory.dirty) ? input.memory : null
// The user's own uncommitted work: never planned over, committed or blamed. A rerun after escalations
// passes the first run's list, since that run's .haruspex/ and init edits are dirty by now.
const userChangesBefore = Array.isArray(input.userChangesBefore) ? input.userChangesBefore : loaded ? loaded.dirty : []
const initializedEarlier = input.initialized === true
let memory = { version: 1, runs: 0, nextId: 1, checks: [], rules: [], models: {} }
let canSave = true
if (!loaded) {
  canSave = false
  log(`no recall output in args.memory; running without memory and not saving, so nothing gets overwritten`)
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

const POLICY = `Auto-proceed policy, set by the user: resolve what you can without them. Validate assumptions by reading the code (status "validated", with the basis). For low or medium impact, choose a reversible default, record it as "accepted_risk" with the resolution, and add a criterion or test that guards it. Mitigate high-severity failure modes inside the plan: a criterion and a test that address them, owned by a unit. Never mark a high or critical item "accepted_risk" yourself; that is the user's call. Escalate only what needs the user: anything critical, or anything high that code and tests cannot mitigate. Each escalation is one concrete question with your recommendation, using the record's id (ASM-…, FM-…, Q-…).`

const readPrompt = `Task: ${task}
${decisions.length ? `\nThe user answered these escalations; record each answer on its item (status "validated" or "answered", or "accepted_risk" with the user's words as the resolution):\n${decisions.map(d => `- ${d.id}: ${d.answer}`).join('\n')}\n` : ''}${userChangesBefore.length ? `\nThese paths hold the user's uncommitted work. Plan no edits to them unless the task truly needs it: ${userChangesBefore.join(', ')}\n` : ''}${ruleBlock('planner')}${memory.checks.length ? `\nCheck commands that worked here before: ${memory.checks.join(' | ')}. Reuse them if they still fit.\n` : ''}
You run the Haruspex Reading for a build graph. The Haruspex CLI is \`${hx}\`; always call it with that absolute path. Workers do the code edits; you only write Haruspex records and the plan.
1. Harness: if .haruspex/ does not exist, run \`${hx} init .\` and report initialized=true. Then make .haruspex/project.json real: project owner and risk, every characteristic true or false (never null), and the check commands from step 6 under "commands".
2. Task: ${resumeTask ? `continue task ${resumeTask} (\`${hx} task activate ${resumeTask} .\`) and apply the answers above` : `create and activate it with \`${hx} task create "<short title>" .\``}. Follow .haruspex/managed/PROTOCOL.md.
3. The Reading, written into .haruspex/tasks/<id>.json and summarised in .haruspex/reading.md: problem (summary, affected actors, current behavior, desired outcome, success metrics), included and excluded scope, testable acceptance criteria (AC-001…), assumptions (ASM-…: basis, impact, blocking, status), failure modes (FM-…: a concrete scenario on the real code path and a severity low|medium|high|critical), a test plan naming the seam each test runs at, the nine core reviews each "complete" with a rationale, and a profile review for every characteristic set true. Set the task risk, and status "ready".
4. ${POLICY}
5. Split the build into the fewest units that make sense, at most ${MAX_UNITS}. Each unit lists every repo-relative file it may create or edit (tests included), dependsOn only when it truly needs another unit's output, size "small" (a contained edit) or "large" (new logic, several files, tricky behavior), the criteria it satisfies, and risky=true with a riskReason if it is hard to undo or reaches outside the repo (migrations, deletes, deploys, payments, auth or permission changes, secrets, CI). Every criterion belongs to exactly one unit. Units that touch the same file must be one unit or depend on each other.
6. checks: the non-interactive commands that prove the work (targeted tests, typecheck, lint, build) from the repo's own scripts, CI or dev stack, each exiting 0 on success. Name them in project.json "commands" and list those names in the task's "required_checks".
7. If the current branch is the repo's default branch, create and switch to work/<short-slug>. Never commit, stash, reset or discard; uncommitted changes stay.
8. Run \`${hx} gate ready-to-build .\`. The only blocker left must be the pending human approval; fix the records until it is.
If the task needs no change, return units: [] and say why in note.`

const challengePrompt = (taskId, units) => `Overall task: ${task}

Haruspex task ${taskId} (.haruspex/tasks/${taskId}.json) holds a Reading another agent just wrote. Assume it is incomplete. Read the code paths it touches and find what it missed: unhappy paths, partial success, retries, concurrency, permissions, data transitions, recovery, rollout and rollback, and external contracts (formats, units, timezones, protocols) that something outside this repo decides.
Add each real gap to the task JSON as a new assumption, failure mode or acceptance criterion with the next free id. Generic worries are not gaps; tie each one to the actual code path.
${POLICY}
Give every criterion you add to one of these units: ${units.map(u => `${u.id} (${u.files.join(', ')})`).join('; ')}.
Do not edit source code. Afterwards \`${hx} gate ready-to-build .\` must still show only the pending approval. Return what you added, the criterion-to-unit assignments, and any escalations.`

const workPrompt = (u, taskId) => `Overall task: ${task}

You are one worker in a build graph. Your unit (${u.id}): ${u.goal}
It must satisfy ${u.criteria.join(', ') || 'its goal'}; the criteria, failure modes and test plan are in .haruspex/tasks/${taskId}.json.
Edit only these files: ${u.files.join(', ')}
Other workers are editing other files at the same time, so touch nothing else, including .haruspex/. If the unit can't be done within these files, change nothing and explain why in "blocked".
Work test-first: call the Skill tool with "mattpocock-skills:tdd" and follow it, except that the seams are already agreed in the task's test plan, so don't stop to ask. If that skill is unavailable: write one failing test at the public seam for one criterion, then the least code that passes it, then the next; never test internals.
${ruleBlock('worker')}Do not commit and do not run the full check suite; a separate checker does that.`

const fixPrompt = (u, problem, round, max) => `Overall task: ${task}

Your unit (${u.id}): ${u.goal}
Files you may edit: ${u.files.join(', ')}
${problem}
This is fix round ${round} of ${max}.
Diagnose before you change code: call the Skill tool with "mattpocock-skills:diagnosing-bugs" and follow it; you already have the feedback loop above, so start at reproduce and minimise. If that skill is unavailable: reproduce with the failing command, minimise, form one hypothesis, confirm it, then fix the root cause.
${ruleBlock('worker')}
Fix it within your files. Never weaken, skip or delete a test or check to make it pass. If your unit did not cause the problem, change nothing and say so in "blocked".`

const checkProblem = failed => `After the build, these check commands failed:\n\n${failed.map(f => `$ ${f.cmd}   (exit ${f.exitCode})\n${f.outputTail.slice(-3000)}`).join('\n\n')}`
const reviewProblem = review => `The independent reviewer rejected the change:\n${[
  ...review.criteria.filter(c => c.status === 'failed').map(c => `- criterion ${c.id} failed`),
  ...review.findings.filter(f => SERIOUS.has(f.severity)).map(f => `- ${f.id} [${f.severity}] ${f.file || ''}: ${f.summary}`),
].join('\n')}`

const reviewPrompt = (taskId, round) => `Overall task: ${task}

You are the independent verifier for Haruspex task ${taskId}. Treat this as a new reading of the result, not a summary of the build; assume the workers' account may be wrong.${round > 1 ? ' This is a re-review after fixes: check the earlier findings again and mark the fixed ones "resolved".' : ''}
Call the Skill tool with "mattpocock-skills:code-review" and use its two axes, with the fixed point being HEAD (\`git diff HEAD\` plus untracked files):
- Spec: does the change satisfy each acceptance criterion and handle each failure mode in .haruspex/tasks/${taskId}.json? Run the check commands and exercise negative and recovery paths with throwaway scripts kept outside the working tree.
- Standards: does it follow this repo's documented standards (CODING_STANDARDS.md, CLAUDE.md, AGENTS.md, linter config)?
Do not edit source files. Do update .haruspex/tasks/${taskId}.json: each criterion "passed" or "failed" with evidence ("check:<command name>" or a test file path), failure modes you confirmed handled to "tested" or "addressed" with evidence, and independent_review = {status: "passed" or "failed", reviewer: "workgraph reviewer (opus)", at: <now, ISO 8601>, findings: [{id: "RF-…", severity, file, summary, status: "open" or "resolved"}], waiver_reason: null}. Pass only when every criterion passes and no high or critical finding is open.
Return passed, the criterion verdicts and the open findings.`

const learnPrompt = items => `You turn this run's evidence into two things.

1. Rules: each fixed failure below with a "sig" becomes one rule, a single imperative sentence a future planner or worker in this repo can follow so it doesn't recur. Name the module, convention or command. If "existingRule" is set, the rule was already known and the failure happened anyway, so rewrite it into the rule that would have prevented this. Scope "planner" for how to split or size work, "worker" for how to write the code, "all" for both. Copy each sig exactly. Generic advice like "write tests" is not a rule.
2. Proposals, as in the "mattpocock-skills:retro" skill: improvements to the environment, not the code. A mechanical mistake (a fixed pattern, a banned API, an import shape, a file-location rule) gets "automated-check": name the lint rule, pre-commit hook or CI job and the repo file it goes in. A judgement call gets "coding-standard". Something the workers struggled to find gets "navigation". A blind spot the Haruspex Reading should have caught gets "haruspex-profile". Only propose what the evidence supports; an empty list is fine. Proposals are shown to the user, never applied.

EVIDENCE:
${JSON.stringify(items, null, 2)}`

// ---- Read and Challenge
phase('Read')
const reading = await agent(readPrompt, { label: 'read', phase: 'Read', schema: READ, model: 'opus', effort: 'high' })
if (!reading) throw new Error('work: the Reading agent died; no source code was changed')
if (!reading.units.length) return { status: 'nothing_to_do', branch: reading.branch, taskId: reading.taskId, note: reading.note || 'the Reading found nothing to change' }
const taskId = reading.taskId

phase('Challenge')
const challenge = await agent(challengePrompt(taskId, reading.units), { label: 'challenge', phase: 'Challenge', schema: CHALLENGE, model: 'sonnet', effort: 'high' })
if (!challenge) log('the challenge agent died; continuing with the Reading as written')
const assign = challenge ? challenge.assign : []
const units = reading.units.map(u => ({
  ...u,
  criteria: [...u.criteria, ...assign.filter(a => a.unitId === u.id && !u.criteria.includes(a.criterionId)).map(a => a.criterionId)],
}))
const unplaced = assign.filter(a => !units.some(u => u.id === a.unitId))
if (unplaced.length) log(`challenge criteria assigned to unknown units, left to the reviewer: ${unplaced.map(a => a.criterionId).join(', ')}`)

const initialized = reading.initialized || initializedEarlier
// A planned edit on top of the user's uncommitted work would mix their changes into this run's commit.
const overlap = units.filter(u => !u.risky).flatMap(u => u.files).filter(f => userChangesBefore.includes(f))
const dirtyEscalation = overlap.length && !decisions.some(d => d.id === 'DIRTY')
  ? [{
      id: 'DIRTY', severity: 'high',
      question: `The plan edits files that hold your uncommitted work (${overlap.join(', ')}). Commit or stash them first, or let the run edit on top so your changes go into its commit?`,
      recommendation: 'Commit or stash them first, then rerun',
      options: ['Commit or stash them first', 'Edit on top; commit my changes with the run'],
    }]
  : []
const escalations = [...reading.escalations, ...(challenge ? challenge.escalations : []), ...dirtyEscalation]
if (escalations.length) {
  return {
    status: 'needs_decision',
    branch: reading.branch,
    taskId,
    initialized,
    risk: reading.risk,
    escalations,
    challengeAdded: challenge ? challenge.added : [],
    userChangesBefore,
    next: 'Nothing was built. Ask the user each escalation, then rerun with {task, haruspexCli, taskId, initialized, userChangesBefore, decisions: [{id, answer}]}.',
  }
}
const committable = decisions.some(d => d.id === 'DIRTY') ? userChangesBefore.filter(f => !overlap.includes(f)) : userChangesBefore

phase('Check')
const base = await runChecks(reading.checks, 'check:baseline')
const leftOut = base.failed.map(f => f.cmd)
const gate = reading.checks.filter(c => !leftOut.includes(c))
const preexisting = new Set(base.changedFiles)
if (leftOut.length) log(`already failing before any change, left out of the gate: ${leftOut.join(' | ')}`)
if (!gate.length) log('no passing automated check here: units run once and the result is unverified')

// ---- Build, check, fix: layer by layer
const done = [], skipped = [], stuck = [], fixes = [], allocation = [], outcomes = []
const fixedSigs = new Set()
const blocked = new Set()
const built = new Map() // id -> unit with its tier, for the review's fix round
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
  const results = await parallel(assigned.map(u => () =>
    agent(workPrompt(u, taskId), { label: `work:${u.id}`, phase: 'Build', schema: WORK, model: u.tier })))
  const ok = []
  assigned.forEach((u, j) => {
    const w = results[j]
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
      agent(fixPrompt(u, checkProblem(check.failed), round, MAX_FIX_ROUNDS), {
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
    ok.forEach(u => { done.push(u.id); built.set(u.id, u) })
  }
}

// ---- Independent review against the Haruspex criteria, with one bounded fix cycle
let review = null
const reviewFixes = []
if (!stopped && built.size) {
  phase('Review')
  review = await agent(reviewPrompt(taskId, 1), { label: 'review#1', phase: 'Review', schema: REVIEW, model: 'opus' })
  if (!review) log('the reviewer died; the change is not independently verified')
  for (let round = 1; review && !review.passed && round <= MAX_REVIEW_FIXES; round++) {
    const culprits = blameReview(review, [...built.values()])
    log(`review rejected the change; fix ${round}/${MAX_REVIEW_FIXES} for ${culprits.map(u => `${u.id} on ${fixTier(u, round)}`).join(', ')}`)
    phase('Fix')
    const fixed = await parallel(culprits.map(u => () =>
      agent(fixPrompt(u, reviewProblem(review), round, MAX_REVIEW_FIXES), {
        label: `fix:${u.id}#r${round}`, phase: 'Fix', schema: WORK, model: fixTier(u, round),
      })))
    reviewFixes.push({
      round, units: culprits.map(u => u.id),
      findings: review.findings.filter(f => SERIOUS.has(f.severity)),
      failedCriteria: review.criteria.filter(c => c.status === 'failed').map(c => c.id),
      fixSummaries: fixed.map(f => (f ? (f.blocked ? `blocked: ${f.blocked}` : f.summary) : 'fixer died')),
    })
    phase('Check')
    const recheck = await runChecks(gate, `check:review${round}`)
    changed = recheck.changedFiles
    if (recheck.failed.length) {
      log(`checks failed after the review fix: ${recheck.failed.map(f => f.cmd).join(' | ')}`)
      review = { ...review, passed: false }
      break
    }
    phase('Review')
    review = (await agent(reviewPrompt(taskId, round + 1), { label: `review#${round + 1}`, phase: 'Review', schema: REVIEW, model: 'opus' })) || review
  }
  if (review && !review.passed) {
    const culprits = blameReview(review, [...built.values()])
    culprits.forEach(u => stuck.push({ id: u.id, goal: u.goal, why: 'independent review still rejects it' }))
  }
}

const planned = units.flatMap(u => u.files)
const inPlan = f => planned.some(p => p === f || (f.endsWith('/') && p.startsWith(f)))
const harness = f => f.startsWith('.haruspex/') || (initialized && (f === 'AGENTS.md' || f === 'CLAUDE.md'))
const outOfScope = changed.filter(f => !userChangesBefore.includes(f) && !preexisting.has(f) && f !== MEMORY && !harness(f) && !inPlan(f))
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

// 2. Fixed failures become rules (or rewrite the rule that failed to prevent them); review findings feed the proposals.
const evidence = [...fixedSigs].map(sig => {
  const x = fixes.find(fx => fx.failed.some(f => f.sig === sig))
  const f = x.failed.find(ff => ff.sig === sig)
  const existing = scored.find(r => r.sig === sig)
  return { sig, cmd: f.cmd, outputTail: f.outputTail, fixSummaries: x.fixSummaries, existingRule: existing ? existing.text : null }
})
const reviewEvidence = reviewFixes.map(r => ({ reviewFindings: r.findings, failedCriteria: r.failedCriteria, fixSummaries: r.fixSummaries }))
let phrased = [], proposals = []
if ((evidence.length || reviewEvidence.length || stuck.length || outOfScope.length) && canSave) {
  const res = await agent(learnPrompt([...evidence, ...reviewEvidence, ...(outOfScope.length ? [{ outOfScope }] : []), ...stuck.map(s => ({ stuck: s }))]),
    { label: 'learn', phase: 'Learn', schema: LEARN, model: 'sonnet' })
  if (res) {
    phrased = res.rules.filter(p => fixedSigs.has(p.sig) && p.text.trim())
    proposals = res.proposals
  } else log('learn agent died; no new rules or proposals this run')
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

// The skill pipes this to `wg save`, which validates it and never overwrites an unreadable file.
const nextMemory = { version: 1, runs: memory.runs + 1, nextId, checks: gate.length ? gate : memory.checks, rules, models }

return {
  status: stuck.length ? 'stuck' : 'ready_for_review',
  branch: reading.branch,
  haruspex: { taskId, initialized, risk: reading.risk, challengeAdded: challenge ? challenge.added : [] },
  verified: gate.length > 0,
  reviewed: !!(review && review.passed),
  review: review ? { passed: review.passed, criteria: review.criteria, findings: review.findings } : null,
  checks: gate,
  checksAlreadyFailing: leftOut,
  done,
  skipped,
  stuck,
  fixRounds: fixes.length,
  reviewFixRounds: reviewFixes.length,
  outOfScope,
  userChangesBefore: committable, // the skill leaves these out of the commit
  models: allocation,
  rules: {
    added, revised, pruned,
    inPlay: [...inPlay],
    top: rules.slice(0, 5).map(r => ({ id: r.id, weight: r2(weight(r)), scope: r.scope, text: r.text })),
  },
  proposals,
  memoryToSave: canSave ? nextMemory : null,
  next: 'Nothing is committed. On the user\'s approval of the build gate and commit, follow the skill\'s verification steps.',
}
