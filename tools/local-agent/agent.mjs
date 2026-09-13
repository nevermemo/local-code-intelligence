#!/usr/bin/env node
// tools/local-agent/agent.mjs
//
// Dependency-free Node 24 harness that drives a local OpenAI-compatible model
// against this repository through a small, policy-bounded tool set.
//
// Design notes / conservative choices (documented per task instructions):
// - All repository paths are normalized to forward-slash relative form and
//   resolved against the policy-resolved repositoryRoot. Absolute paths,
//   NUL bytes, and `..` traversal are rejected outright.
// - `deny` globs beat `read`/`write` globs. A path that matches neither a
//   read nor a write glob is rejected (conservative default-deny).
// - Symlink/junction escape: for existing targets (and their parents up to
//   the repository root) we compare `fs.realpathSync` of the root against the
//   expected prefix; if the resolved path escapes the root, the call fails.
// - `apply_patch` is exact-text only: the `oldText` must occur exactly
//   `expectedReplacements` times (default 1) in the current file content, and
//   every occurrence is replaced with `newText`. The write is atomic (write
//   to temp file in same dir, then rename).
// - `run_check` runs only the named suite from policy `checks.suites` via a
//   fixed `powershell -NoProfile -File <script> -Suite <name>` command with
//   `shell: false`. No arbitrary shell.
// - `git_diff` runs a fixed `git -C <root> diff` (read-only) with `shell: false`.
// - MCP client: JSON-RPC over HTTP; accepts `application/json` or
//   `text/event-stream` (SSE) responses. Non-JSON HTTP error bodies are parsed
//   defensively so a raw `JSON.parse` SyntaxError is never leaked.
// - Audit: one JSON line per tool call and per model turn. Metadata only:
//   tool name, ok, error class, byte counts, duration. Never task text,
//   model output, source, or patch content. Audit write failure is fatal.
// - Dry-run: policy is resolved and validated, the task is validated, and the
//   planned tool surface is listed. `apply_patch` and `run_check` are blocked
//   (they would be refused at execution time anyway).
//
// Reliability / recovery notes:
// - Every outbound request is bounded: a per-model-request timeout, a
//   per-MCP-request timeout, and a per-child-process timeout. The overall
//   task deadline additionally bounds the whole run independently of the
//   turn budget: it is checked before every model turn, every tool call, and
//   every MCP request, and each in-flight request's timeout is capped at the
//   remaining deadline, so no single request can outlive the task.
// - Cancellation is graceful: SIGINT/SIGTERM abort in-flight model, MCP, and
//   child-process work, the loop stops at the next safe point, and a
//   checkpoint is written. A cancellation is never reported as a timeout,
//   and a deadline expiry is never reported as a cancellation.
// - Every run has a persistent task id and a compact checkpoint file. The
//   growing conversation is never persisted; only the task contract identity,
//   tool metadata, the accepted change summary (including a SHA-256 hash per
//   accepted file), and the accepted patch-chain identity are, so `--resume` stays
//   small and predictable. `--resume` reconstructs a fresh model
//   conversation from that compact state (a resume summary), never a replayed
//   transcript. MCP initialization happens once per run and is never retried.
// - Checkpoint file-hash validation: `--resume` re-hashes every recorded
//   accepted file and refuses with `state_drift` when a recorded file is
//   missing or its content differs from the recorded SHA-256.
// - Failures are classified (model / MCP / patch rejection / check failure /
//   coordinator cancellation / deadline expiry / state drift / harness, with
//   operation-specific timeout codes) and each class has its own narrow retry
//   budget. A repeated identical failing tool call stops the run and returns
//   control to the coordinator.

import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const SCHEMA = 'local-agent.policy/v1';
const CONTRACT_SCHEMA = 'local-agent.task/v1';
const STATE_SCHEMA = 'local-agent.state/v1';
const CONTRACT_ID_RE = /^[A-Za-z0-9._-]+$/;
const TASK_ID_RE = /^[A-Za-z0-9._-]+$/;

// Default request bounds (milliseconds). Every one is overridable through the
// optional policy `limits` fields of the same name.
export const DEFAULT_LIMITS = Object.freeze({
  modelRequestTimeoutMs: 600000,
  mcpRequestTimeoutMs: 60000,
  checkTimeoutMs: 600000,
  gitTimeoutMs: 60000,
  taskDeadlineMs: 1800000,
});

// Failure classes. The coordinator distinguishes these so a transport problem
// is never confused with a rejected patch or a failing check.
export const FAILURE_CLASSES = Object.freeze([
  'model_failure',
  'mcp_failure',
  'patch_rejected',
  'check_failed',
  'coordinator_cancelled',
  'deadline_exceeded',
  'harness_failure',
]);

// Narrow per-class retry budgets. `coordinator_cancelled`, `deadline_exceeded`
// and `harness_failure` are never retried: they return control immediately.
export const DEFAULT_RETRY_BUDGET = Object.freeze({
  model_failure: 1,
  mcp_failure: 1,
  patch_rejected: 2,
  check_failed: 1,
  coordinator_cancelled: 0,
  deadline_exceeded: 0,
  harness_failure: 0,
});

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

export class AgentError extends Error {
  constructor(message, { fatal = true, code = 'agent_error' } = {}) {
    super(message);
    this.name = 'AgentError';
    this.fatal = fatal;
    this.code = code;
  }
}

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

const HELP_TEXT = `Usage:
  node agent.mjs --task "..." --policy policy.json
  node agent.mjs --task-file task.md --policy policy.json [--dry-run]
  node agent.mjs --contract task.json --policy policy.json [--dry-run]

Options:
  --policy <path>      Path to a local-agent.policy/v1 JSON file (required).
  --task <text>        Task text (exactly one of --task / --task-file / --contract).
  --task-file <path>   Path to a file containing the task text.
  --contract <path>    Path to a local-agent.task/v1 JSON contract. The
                       contract supplies the task text and the allowed-file /
                       check-suite restrictions enforced by the tools.
  --task-id <id>       Persistent task id ([A-Za-z0-9._-]+). Defaults to the
                       contract id, else a stable hash of the task text.
  --state <path>       Checkpoint file path. Defaults to
                       <audit dir>/state/<task-id>.json.
  --resume             Resume from the existing checkpoint instead of starting
                       a new run. The task id and task text must match.
  --stop-after-first-patch
                       Stop as soon as one patch is accepted.
  --stop-after-first-failed-check
                       Stop as soon as one check fails.
  --dry-run            Resolve and validate the policy and task, list the
                       planned tool surface, and exit without contacting the
                       model or writing files.
  --help               Show this help.

Exit codes:
  0 success (awaiting_review, stopped_after_patch, stopped_after_failed_check)
  1 failed        2 no_changes        3 cancelled or deadline_exceeded
`;

export function parseArgs(argv) {
  const out = {
    policy: null,
    task: null,
    taskFile: null,
    contract: null,
    taskId: null,
    state: null,
    resume: false,
    stopAfterFirstPatch: false,
    stopAfterFirstFailedCheck: false,
    dryRun: false,
    help: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
      case '--task-id':
        if (out.taskId !== null) throw new AgentError('--task-id given more than once');
        out.taskId = argv[++i];
        if (out.taskId === undefined) throw new AgentError('--task-id requires a value');
        if (!TASK_ID_RE.test(out.taskId)) throw new AgentError('--task-id must match [A-Za-z0-9._-]+');
        break;
      case '--state':
        if (out.state !== null) throw new AgentError('--state given more than once');
        out.state = argv[++i];
        if (out.state === undefined) throw new AgentError('--state requires a value');
        break;
      case '--resume':
        out.resume = true;
        break;
      case '--stop-after-first-patch':
        out.stopAfterFirstPatch = true;
        break;
      case '--stop-after-first-failed-check':
        out.stopAfterFirstFailedCheck = true;
        break;
      case '--policy':
        if (out.policy !== null) throw new AgentError('--policy given more than once');
        out.policy = argv[++i];
        if (out.policy === undefined) throw new AgentError('--policy requires a value');
        break;
      case '--task':
        if (out.task !== null) throw new AgentError('--task given more than once');
        out.task = argv[++i];
        if (out.task === undefined) throw new AgentError('--task requires a value');
        break;
      case '--task-file':
        if (out.taskFile !== null) throw new AgentError('--task-file given more than once');
        out.taskFile = argv[++i];
        if (out.taskFile === undefined) throw new AgentError('--task-file requires a value');
        break;
      case '--contract':
        if (out.contract !== null) throw new AgentError('--contract given more than once');
        out.contract = argv[++i];
        if (out.contract === undefined) throw new AgentError('--contract requires a value');
        break;
      case '--dry-run':
        out.dryRun = true;
        break;
      case '--help':
      case '-h':
        out.help = true;
        break;
      default:
        throw new AgentError(`unknown argument: ${a} (see --help)`);
    }
  }
  if (!out.help) {
    if (!out.policy) throw new AgentError('missing required --policy <path>');
    const sources = [out.task, out.taskFile, out.contract].filter((x) => x !== null).length;
    if (sources !== 1) {
      throw new AgentError('exactly one of --task, --task-file, or --contract is required');
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// Glob matching (safe **/*/? only)
// ---------------------------------------------------------------------------

// Compile a policy glob into a RegExp. Supported: literal chars, `?` (one
// path char, not `/`), `*` (one or more chars, not `/`), `**/` (zero or more
// path segments), and trailing `**` (everything below). Anything else is
// treated as a literal.
export function globToRegExp(glob) {
  let re = '';
  let i = 0;
  const s = String(glob).replace(/\\/g, '/');
  while (i < s.length) {
    const c = s[i];
    if (c === '*') {
      if (s[i + 1] === '*') {
        // ** segment
        if (s[i + 2] === '/') {
          re += '(?:[^/]+/)*'; // zero or more full segments
          i += 3;
        } else if (i + 2 === s.length) {
          re += '.*'; // trailing **: everything
          i += 2;
        } else {
          // `**` mid-pattern without slash: treat as `.*`
          re += '.*';
          i += 2;
        }
      } else {
        re += '[^/]+';
        i += 1;
      }
    } else if (c === '?') {
      re += '[^/]';
      i += 1;
    } else {
      re += c.replace(/[.+^${}()|[\]\\]/g, '\\$&');
      i += 1;
    }
  }
  return new RegExp(`^${re}$`);
}

export function globMatch(glob, relPath) {
  return globToRegExp(glob).test(relPath);
}

// ---------------------------------------------------------------------------
// Path normalization and safety
// ---------------------------------------------------------------------------

// Normalize a user-supplied relative path to forward-slash form. Backslashes
// are converted to `/` before splitting so Windows-style separators are
// handled uniformly. Rejects absolute POSIX/Windows paths, NUL bytes, `..`
// traversal, and empty/non-string input.
export function normalizeRelPath(p) {
  if (typeof p !== 'string' || p.length === 0) {
    throw new AgentError('path must be a non-empty string');
  }
  if (p.includes('\0')) throw new AgentError('path contains NUL byte');
  // Convert backslashes to forward slashes before any absolute-path check or
  // split, so `C:\x` and `src\a.rs` are both handled consistently.
  const normalized = p.replace(/\\/g, '/');
  if (path.isAbsolute(normalized) || /^[a-zA-Z]:/.test(normalized)) {
    throw new AgentError(`absolute paths are not allowed: ${p}`);
  }
  const parts = normalized.split('/').filter((x) => x.length > 0 && x !== '.');
  for (const part of parts) {
    if (part === '..') throw new AgentError(`path traversal is not allowed: ${p}`);
  }
  return parts.join('/');
}

// ---------------------------------------------------------------------------
// Policy loading and validation
// ---------------------------------------------------------------------------

function requirePositiveInt(obj, key, where) {
  const v = obj?.[key];
  if (typeof v !== 'number' || !Number.isInteger(v) || v <= 0) {
    throw new AgentError(`${where}.${key} must be a positive integer`);
  }
  return v;
}

function requireString(obj, key, where) {
  const v = obj?.[key];
  if (typeof v !== 'string' || v.length === 0) {
    throw new AgentError(`${where}.${key} must be a non-empty string`);
  }
  return v;
}

function requireGlobArray(obj, key, where) {
  const v = obj?.[key];
  if (!Array.isArray(v) || v.some((g) => typeof g !== 'string' || g.length === 0)) {
    throw new AgentError(`${where}.${key} must be an array of non-empty glob strings`);
  }
  return v;
}

// Load and validate the policy file. Resolves repositoryRoot, endpoints,
// model, and audit path relative to the policy file location.
export function loadPolicy(policyPath) {
  const absPolicy = path.resolve(policyPath);
  if (!fs.existsSync(absPolicy)) {
    throw new AgentError(`policy file not found: ${absPolicy}`);
  }
  let raw;
  try {
    raw = JSON.parse(fs.readFileSync(absPolicy, 'utf8'));
  } catch (e) {
    throw new AgentError(`policy file is not valid JSON: ${e.message}`);
  }
  if (raw.schema !== SCHEMA) {
    throw new AgentError(`unsupported policy schema: ${String(raw.schema)} (expected ${SCHEMA})`);
  }

  const policyDir = path.dirname(absPolicy);
  const repoRoot = path.resolve(policyDir, requireString(raw, 'repositoryRoot', 'policy'));
  if (!fs.existsSync(repoRoot) || !fs.statSync(repoRoot).isDirectory()) {
    throw new AgentError(`repositoryRoot does not exist or is not a directory: ${repoRoot}`);
  }

  const read = requireGlobArray(raw, 'read', 'policy').map(normalizeRelPath);
  const write = requireGlobArray(raw, 'write', 'policy').map(normalizeRelPath);
  const deny = requireGlobArray(raw, 'deny', 'policy').map(normalizeRelPath);

  const model = {
    name: requireString(raw.model, 'name', 'model'),
    endpoint: requireString(raw.model, 'endpoint', 'model'),
    apiKeyEnv: requireString(raw.model, 'apiKeyEnv', 'model'),
  };
  const lci = { endpoint: requireString(raw.lci, 'endpoint', 'lci') };

  const limits = {
    maxTurns: requirePositiveInt(raw.limits, 'maxTurns', 'limits'),
    maxReadLines: requirePositiveInt(raw.limits, 'maxReadLines', 'limits'),
    maxPatchBytes: requirePositiveInt(raw.limits, 'maxPatchBytes', 'limits'),
    maxToolResultBytes: requirePositiveInt(raw.limits, 'maxToolResultBytes', 'limits'),
  };
  // Optional request bounds. Absent fields fall back to DEFAULT_LIMITS so an
  // existing policy keeps working unchanged.
  for (const key of Object.keys(DEFAULT_LIMITS)) {
    limits[key] = raw.limits[key] === undefined
      ? DEFAULT_LIMITS[key]
      : requirePositiveInt(raw.limits, key, 'limits');
  }
  // Optional per-failure-class retry budget. Unknown classes are rejected;
  // absent classes keep the default budget.
  const retryBudget = { ...DEFAULT_RETRY_BUDGET };
  if (raw.limits.retryBudget !== undefined) {
    const rb = raw.limits.retryBudget;
    if (typeof rb !== 'object' || rb === null || Array.isArray(rb)) {
      throw new AgentError('limits.retryBudget must be an object of failure class -> count');
    }
    for (const [cls, count] of Object.entries(rb)) {
      if (!FAILURE_CLASSES.includes(cls)) {
        throw new AgentError(`limits.retryBudget references unknown failure class: ${cls}`);
      }
      if (typeof count !== 'number' || !Number.isInteger(count) || count < 0) {
        throw new AgentError(`limits.retryBudget.${cls} must be a non-negative integer`);
      }
      retryBudget[cls] = count;
    }
  }
  limits.retryBudget = retryBudget;

  const checks = {
    script: requireString(raw.checks, 'script', 'checks'),
    suites: {},
  };
  if (typeof raw.checks.suites !== 'object' || raw.checks.suites === null) {
    throw new AgentError('checks.suites must be an object of name -> suite');
  }
  for (const [name, suite] of Object.entries(raw.checks.suites)) {
    if (typeof suite !== 'string' || suite.length === 0) {
      throw new AgentError(`checks.suites.${name} must be a non-empty string`);
    }
    checks.suites[name] = suite;
  }

  // Optional change-aware verification planning fields. Backward
  // compatibility: absent pathRules means an empty array; absent finalSuite
  // means null.
  const pathRulesRaw = raw.checks.pathRules;
  const pathRules = [];
  if (pathRulesRaw !== undefined) {
    if (!Array.isArray(pathRulesRaw)) {
      throw new AgentError('checks.pathRules must be an array of rule objects');
    }
    const seenRule = new Set();
    for (const rule of pathRulesRaw) {
      if (typeof rule !== 'object' || rule === null || Array.isArray(rule)) {
        throw new AgentError('checks.pathRules entries must be objects');
      }
      const glob = requireString(rule, 'glob', 'checks.pathRules entry');
      const suites = rule.suites;
      if (!Array.isArray(suites) || suites.length === 0) {
        throw new AgentError('checks.pathRules entry.suites must be a non-empty array');
      }
      const seenSuite = new Set();
      for (const s of suites) {
        if (typeof s !== 'string' || s.length === 0) {
          throw new AgentError('checks.pathRules entry.suites entries must be non-empty strings');
        }
        if (seenSuite.has(s)) {
          throw new AgentError(`checks.pathRules entry.suites contains a duplicate: ${s}`);
        }
        seenSuite.add(s);
        if (!(s in checks.suites)) {
          throw new AgentError(`checks.pathRules entry.suites references unknown suite: ${s}`);
        }
      }
      const reason = requireString(rule, 'reason', 'checks.pathRules entry');
      const normalizedGlob = normalizeRelPath(glob);
      const ruleKey = `${normalizedGlob}|${suites.join(',')}|${reason}`;
      if (seenRule.has(ruleKey)) {
        throw new AgentError(`checks.pathRules contains a duplicate rule: ${normalizedGlob}`);
      }
      seenRule.add(ruleKey);
      pathRules.push({ glob: normalizedGlob, suites, reason });
    }
  }
  let finalSuite = null;
  if (raw.checks.finalSuite !== undefined) {
    if (typeof raw.checks.finalSuite !== 'string' || raw.checks.finalSuite.length === 0) {
      throw new AgentError('checks.finalSuite must be a non-empty string');
    }
    if (!(raw.checks.finalSuite in checks.suites)) {
      throw new AgentError(`checks.finalSuite references unknown suite: ${raw.checks.finalSuite}`);
    }
    finalSuite = raw.checks.finalSuite;
  }

  const audit = { jsonlPath: requireString(raw.audit, 'jsonlPath', 'audit') };

  // Resolve policy-relative paths to absolute.
  const resolved = {
    schema: SCHEMA,
    policyPath: absPolicy,
    repoRoot,
    read,
    write,
    deny,
    model,
    lci,
    limits,
    checks: { ...checks, pathRules, finalSuite },
    audit: { jsonlPath: path.resolve(repoRoot, audit.jsonlPath) },
  };
  // Pre-compile glob regexes.
  resolved.readRe = read.map(globToRegExp);
  resolved.writeRe = write.map(globToRegExp);
  resolved.denyRe = deny.map(globToRegExp);
  return resolved;
}

// Decide read/write access for a normalized relative path. Deny beats allow.
export function checkAccess(policy, relPath, mode) {
  const denied = policy.denyRe.some((re) => re.test(relPath));
  if (denied) {
    throw new AgentError(`path is denied by policy: ${relPath}`);
  }
  const allowed = (mode === 'read' ? policy.readRe : policy.writeRe).some((re) => re.test(relPath));
  if (!allowed) {
    throw new AgentError(`path is not ${mode === 'read' ? 'readable' : 'writable'} per policy: ${relPath}`);
  }
  return true;
}

// ---------------------------------------------------------------------------
// Deterministic change-aware verification planning
// ---------------------------------------------------------------------------

// Pure planner: given a resolved policy, the changed repository-relative
// paths, and an optional active task contract, produce a stable,
// JSON-serializable verification plan. No filesystem, process, MCP, model,
// or Git action is performed.
//
// - changedPaths must be a nonempty array of unique normalized
//   repository-relative paths (absolute / traversal / NUL / non-string
//   entries are rejected).
// - Every path is matched against every policy checks.pathRules entry using
//   the existing globMatch. Suggested suites are collected and deduped in
//   policy declaration order, with the reasons and matched paths attached.
// - When a contract is active, suggested suites not listed in
//   contract.checkSuites go to `deferred` (reason: not authorized by active
//   contract); allowed ones go to `focused`.
// - policy.checks.finalSuite, when set, is always `deferred` (reason: run
//   once after all accepted slices) unless it is already a focused suite.
//   It is never recommended during an ordinary slice.
// - Unmatched paths are reported in `unmatchedPaths`; they are not
//   automatically escalated to the full suite.
export function planVerification(policy, changedPaths, { contract = null } = {}) {
  if (!Array.isArray(changedPaths) || changedPaths.length === 0) {
    throw new AgentError('planVerification requires a non-empty changedPaths array');
  }
  const normalized = [];
  const seen = new Set();
  for (const p of changedPaths) {
    const rel = normalizeRelPath(p); // rejects absolute / traversal / NUL / non-string
    if (seen.has(rel)) {
      throw new AgentError(`planVerification changedPaths contains a duplicate after normalization: ${rel}`);
    }
    seen.add(rel);
    normalized.push(rel);
  }

  const rules = Array.isArray(policy.checks?.pathRules) ? policy.checks.pathRules : [];
  const finalSuite = policy.checks?.finalSuite ?? null;
  const contractSuites = contract ? new Set(contract.checkSuites) : null;

  // Collect suggested suites in policy declaration order (rule order, then
  // the suites order within each rule), deduped.
  const focused = [];
  const deferred = [];
  const focusedSet = new Set();
  const deferredSet = new Set();
  const unmatchedPaths = [];

  for (const rel of normalized) {
    let matchedAny = false;
    for (const rule of rules) {
      if (!globMatch(rule.glob, rel)) continue;
      matchedAny = true;
      for (const suite of rule.suites) {
        if (focusedSet.has(suite) || deferredSet.has(suite)) continue;
        const authorized = contractSuites === null || contractSuites.has(suite);
        if (authorized) {
          focusedSet.add(suite);
          focused.push({ suite, reasons: [rule.reason], matchedPaths: [rel] });
        } else {
          deferredSet.add(suite);
          deferred.push({
            suite,
            reasons: ['not authorized by active contract'],
            matchedPaths: [rel],
          });
        }
      }
    }
    if (!matchedAny) unmatchedPaths.push(rel);
  }

  // Attach additional reasons / matched paths to suites already planned by
  // earlier rules or paths (declaration order is preserved).
  for (const rel of normalized) {
    for (const rule of rules) {
      if (!globMatch(rule.glob, rel)) continue;
      for (const suite of rule.suites) {
        const entry = focusedSet.has(suite)
          ? focused.find((e) => e.suite === suite)
          : deferredSet.has(suite)
            ? deferred.find((e) => e.suite === suite)
            : null;
        if (!entry) continue;
        if (!entry.reasons.includes(rule.reason)) entry.reasons.push(rule.reason);
        if (!entry.matchedPaths.includes(rel)) entry.matchedPaths.push(rel);
      }
    }
  }

  // finalSuite is always deferred (run once after all accepted slices)
  // unless it is already a focused suite.
  if (finalSuite !== null && !focusedSet.has(finalSuite)) {
    if (!deferredSet.has(finalSuite)) {
      deferredSet.add(finalSuite);
      deferred.push({
        suite: finalSuite,
        reasons: ['run once after all accepted slices'],
        matchedPaths: [],
      });
    } else {
      const entry = deferred.find((e) => e.suite === finalSuite);
      if (!entry.reasons.includes('run once after all accepted slices')) {
        entry.reasons.push('run once after all accepted slices');
      }
    }
  }

  return { focused, deferred, unmatchedPaths };
}

// ---------------------------------------------------------------------------
// Task contracts (local-agent.task/v1)
// ---------------------------------------------------------------------------

// Load and validate a task contract. The contract supplies the task text and
// the restrictions (allowedFiles, checkSuites) that buildTools enforces.
// Validation is pure policy work: it never touches the model or the
// filesystem beyond reading the contract file itself, so an invalid contract
// fails before any model contact or mutation.
export function loadTaskContract(contractPath, policy) {
  const absContract = path.resolve(contractPath);
  if (!fs.existsSync(absContract)) {
    throw new AgentError(`contract file not found: ${absContract}`);
  }
  let raw;
  try {
    raw = JSON.parse(fs.readFileSync(absContract, 'utf8'));
  } catch (e) {
    throw new AgentError(`contract file is not valid JSON: ${e.message}`);
  }
  if (raw.schema !== CONTRACT_SCHEMA) {
    throw new AgentError(`unsupported contract schema: ${String(raw.schema)} (expected ${CONTRACT_SCHEMA})`);
  }

  const id = raw.id;
  if (typeof id !== 'string' || !CONTRACT_ID_RE.test(id)) {
    throw new AgentError('contract.id must match [A-Za-z0-9._-]+');
  }

  const objective = raw.objective;
  if (typeof objective !== 'string' || objective.trim().length === 0) {
    throw new AgentError('contract.objective must be a non-empty string');
  }

  // allowedFiles: nonempty, unique after normalization, relative, and each
  // must pass the base policy write access and deny rules.
  const allowedFilesRaw = raw.allowedFiles;
  if (!Array.isArray(allowedFilesRaw) || allowedFilesRaw.length === 0) {
    throw new AgentError('contract.allowedFiles must be a non-empty array');
  }
  const allowedFiles = [];
  const seenAllowed = new Set();
  for (const f of allowedFilesRaw) {
    if (typeof f !== 'string' || f.length === 0) {
      throw new AgentError('contract.allowedFiles entries must be non-empty strings');
    }
    const rel = normalizeRelPath(f); // rejects absolute / traversal / NUL
    if (seenAllowed.has(rel)) {
      throw new AgentError(`contract.allowedFiles contains a duplicate after normalization: ${rel}`);
    }
    seenAllowed.add(rel);
    checkAccess(policy, rel, 'write'); // deny rules + write globs
    allowedFiles.push(rel);
  }

  // acceptance: nonempty, unique strings.
  const acceptance = raw.acceptance;
  if (!Array.isArray(acceptance) || acceptance.length === 0) {
    throw new AgentError('contract.acceptance must be a non-empty array of strings');
  }
  const seenAcceptance = new Set();
  for (const a of acceptance) {
    if (typeof a !== 'string' || a.length === 0) {
      throw new AgentError('contract.acceptance entries must be non-empty strings');
    }
    if (seenAcceptance.has(a)) {
      throw new AgentError(`contract.acceptance contains a duplicate: ${a}`);
    }
    seenAcceptance.add(a);
  }

  // prohibited: unique nonempty strings; may be empty.
  const prohibited = raw.prohibited;
  if (!Array.isArray(prohibited)) {
    throw new AgentError('contract.prohibited must be an array of non-empty strings');
  }
  const seenProhibited = new Set();
  for (const p of prohibited) {
    if (typeof p !== 'string' || p.length === 0) {
      throw new AgentError('contract.prohibited entries must be non-empty strings');
    }
    if (seenProhibited.has(p)) {
      throw new AgentError(`contract.prohibited contains a duplicate: ${p}`);
    }
    seenProhibited.add(p);
  }

  // checkSuites: unique strings; may be empty; each must exist in policy checks.
  const checkSuites = raw.checkSuites;
  if (!Array.isArray(checkSuites)) {
    throw new AgentError('contract.checkSuites must be an array of strings');
  }
  const seenSuites = new Set();
  for (const s of checkSuites) {
    if (typeof s !== 'string' || s.length === 0) {
      throw new AgentError('contract.checkSuites entries must be non-empty strings');
    }
    if (seenSuites.has(s)) {
      throw new AgentError(`contract.checkSuites contains a duplicate: ${s}`);
    }
    seenSuites.add(s);
    if (!(s in policy.checks.suites)) {
      throw new AgentError(`contract.checkSuites references unknown suite: ${s}`);
    }
  }

  return {
    schema: CONTRACT_SCHEMA,
    id,
    objective,
    allowedFiles,
    acceptance,
    prohibited,
    checkSuites,
  };
}

// Compact, labeled task text derived from the contract. This is the only
// contract content that reaches the model; audit records never carry it.
export function contractTaskText(contract) {
  const lines = [
    `Task contract: ${contract.id}`,
    `Objective: ${contract.objective}`,
    `Allowed files (apply_patch restricted to these):`,
    ...contract.allowedFiles.map((f) => `  - ${f}`),
    `Acceptance criteria:`,
    ...contract.acceptance.map((a) => `  - ${a}`),
  ];
  if (contract.prohibited.length > 0) {
    lines.push('Prohibited:');
    for (const p of contract.prohibited) lines.push(`  - ${p}`);
  }
  lines.push(`Allowed check suites: ${contract.checkSuites.length ? contract.checkSuites.join(', ') : '(none)'}`);
  return lines.join('\n');
}

// Resolve a normalized relative path to an absolute path inside repoRoot and
// reject symlink/junction escape for existing targets.
export function resolveRepoPath(policy, relPath) {
  const rel = normalizeRelPath(relPath);
  const abs = path.resolve(policy.repoRoot, rel);
  // Must stay inside repoRoot.
  const rootWithSep = policy.repoRoot + path.sep;
  if (abs !== policy.repoRoot && !abs.startsWith(rootWithSep)) {
    throw new AgentError(`path escapes repository root: ${rel}`);
  }
  // Symlink/junction escape check: walk from the nearest existing ancestor
  // up to (and including) repoRoot, ensuring realpath stays under the
  // realpath of repoRoot.
  const realRoot = fs.realpathSync(policy.repoRoot);
  let probe = abs;
  while (true) {
    if (fs.existsSync(probe)) {
      const real = fs.realpathSync(probe);
      const realRootSep = realRoot + path.sep;
      if (real !== realRoot && !real.startsWith(realRootSep)) {
        throw new AgentError(`path escapes repository root via symlink/junction: ${rel}`);
      }
      break;
    }
    const parent = path.dirname(probe);
    if (parent === probe) break; // reached filesystem root without existing
    probe = parent;
  }
  return { rel, abs };
}

// ---------------------------------------------------------------------------
// Audit (JSONL, metadata only; failure is fatal)
// ---------------------------------------------------------------------------

export class AuditLog {
  constructor(policy, { contractId = null, taskId = null } = {}) {
    this.path = policy.audit.jsonlPath;
    this.dir = path.dirname(this.path);
    // Contract id only (a short identifier). Never task text, source, patch,
    // or model output.
    this.contractId = contractId;
    this.taskId = taskId;
    fs.mkdirSync(this.dir, { recursive: true });
  }
  append(entry) {
    const line = JSON.stringify(entry) + '\n';
    try {
      fs.appendFileSync(this.path, line, 'utf8');
    } catch (e) {
      throw new AgentError(`audit write failed (fatal): ${e.message}`, { code: 'audit_failure' });
    }
  }
  toolCall({ tool, ok, error, inputBytes, outputBytes, durationMs }) {
    this.append({
      ts: new Date().toISOString(),
      kind: 'tool',
      tool,
      ok,
      error: error ?? null,
      inputBytes,
      outputBytes,
      durationMs,
      contractId: this.contractId,
      taskId: this.taskId,
    });
  }
  modelTurn({ turn, ok, error, inputBytes, outputBytes, durationMs }) {
    this.append({
      ts: new Date().toISOString(),
      kind: 'model_turn',
      turn,
      ok,
      error: error ?? null,
      inputBytes,
      outputBytes,
      durationMs,
      contractId: this.contractId,
      taskId: this.taskId,
    });
  }
  // Run-lifecycle metadata: retries, checkpoints, and terminal status. Like
  // every other record, this carries identifiers and classes only.
  lifecycle({ event, turn = null, failureClass = null, error = null, status = null }) {
    this.append({
      ts: new Date().toISOString(),
      kind: 'lifecycle',
      event,
      turn,
      failureClass,
      error,
      status,
      contractId: this.contractId,
      taskId: this.taskId,
    });
  }
}

// ---------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------

// Map an AgentError code to one of FAILURE_CLASSES. The coordinator needs
// these apart: a model transport problem, an MCP problem, a rejected patch, a
// failing check, an explicit cancellation, and a blown deadline all call for
// different recovery.
export function classifyFailure(err) {
  switch (err?.code) {
    case 'model_timeout':
    case 'model_error':
    case 'model_bad_json':
    case 'model_no_choices':
    case 'model_empty':
    case 'no_api_key':
      return 'model_failure';
    case 'mcp_timeout':
    case 'mcp_error':
    case 'mcp_tool_error':
    case 'mcp_init_failed':
      return 'mcp_failure';
    case 'bad_patch':
    case 'patch_mismatch':
    case 'patch_too_large':
    case 'write_error':
    case 'contract_path_denied':
      return 'patch_rejected';
    case 'check_failed':
    case 'check_timeout':
      return 'check_failed';
    case 'git_timeout':
    case 'output_limit':
      return 'harness_failure';
    case 'cancelled':
      return 'coordinator_cancelled';
    case 'deadline_exceeded':
      return 'deadline_exceeded';
    default:
      return 'harness_failure';
  }
}

// Only transport-shaped failures are worth repeating verbatim. A missing API
// key or a denied path is a configuration decision, not a transient fault, so
// it is never retried even though its class has a budget.
const RETRYABLE_CODES = new Set([
  'model_timeout',
  'model_error',
  'model_bad_json',
  'model_no_choices',
  'model_empty',
  'mcp_timeout',
  'mcp_error',
  'mcp_tool_error',
  'patch_mismatch',
  'check_failed',
]);

export function isRetryable(err) {
  return RETRYABLE_CODES.has(err?.code);
}

// ---------------------------------------------------------------------------
// Persistent task state (checkpoint)
// ---------------------------------------------------------------------------

export function hashText(text) {
  return crypto.createHash('sha256').update(String(text), 'utf8').digest('hex');
}

// Compact, resumable run record. The growing conversation is deliberately not
// persisted: only the task identity, the tool surface, the accepted change
// summary (paths plus a SHA-256 hash per accepted file), and the current diff
// identity are, so resumption stays small and predictable and a resume can
// detect file drift.
export class TaskState {
  constructor({ statePath, taskId, taskHash, contractId = null, contractHash = null, tools = [] }) {
    if (typeof taskId !== 'string' || !TASK_ID_RE.test(taskId)) {
      throw new AgentError('task id must match [A-Za-z0-9._-]+', { code: 'bad_task_id' });
    }
    this.statePath = statePath;
    this.record = {
      schema: STATE_SCHEMA,
      task_id: taskId,
      status: 'running',
      turn: 0,
      patches_applied: 0,
      checks_run: [],
      files_changed: [],
      file_hashes: {},
      last_failure: null,
      task_hash: taskHash,
      contract_id: contractId,
      contract_hash: contractHash,
      tools: [...tools].sort(),
      diff_id: null,
      retries_used: {},
      created_at: new Date().toISOString(),
      updated_at: null,
    };
  }

  // Load an existing checkpoint and verify it describes the same task. A
  // different task id, task text, or contract must not silently resume.
  // `repoRoot`, when given, enables the checkpoint file-hash validation
  // against the current working tree (missing or drifted recorded files are
  // refused with `state_drift`).
  static load(statePath, { taskId, taskHash, contractId = null, contractHash = null, repoRoot = null } = {}) {
    if (!fs.existsSync(statePath)) {
      throw new AgentError(`state file not found: ${statePath}`, { code: 'state_missing' });
    }
    let raw;
    try {
      raw = JSON.parse(fs.readFileSync(statePath, 'utf8'));
    } catch (e) {
      throw new AgentError(`state file is not valid JSON: ${e.message}`, { code: 'state_corrupt' });
    }
    if (raw?.schema !== STATE_SCHEMA) {
      throw new AgentError(`unsupported state schema: ${String(raw?.schema)} (expected ${STATE_SCHEMA})`, { code: 'state_corrupt' });
    }
    if (taskId !== undefined && raw.task_id !== taskId) {
      throw new AgentError(`state task_id mismatch: ${raw.task_id} != ${taskId}`, { code: 'state_mismatch' });
    }
    if (taskHash !== undefined && raw.task_hash !== taskHash) {
      throw new AgentError('state task_hash mismatch: the task text changed since the checkpoint', { code: 'state_mismatch' });
    }
    if (contractId !== null && raw.contract_id !== contractId) {
      throw new AgentError(`state contract_id mismatch: ${String(raw.contract_id)} != ${contractId}`, { code: 'state_mismatch' });
    }
    if (contractHash !== null && raw.contract_hash !== contractHash) {
      throw new AgentError('state contract_hash mismatch: the contract changed since the checkpoint', { code: 'state_mismatch' });
    }
    // Checkpoint file-hash validation: every accepted file recorded in the
    // checkpoint must still exist with exactly the recorded SHA-256. A
    // missing or drifted file means the checkpoint no longer describes the
    // working tree, so the resume is refused with a dedicated state-drift
    // error instead of continuing on a stale premise.
    if (repoRoot) {
      const hashes = raw.file_hashes;
      if (raw.patches_applied > 0 && (!hashes || typeof hashes !== 'object' || Array.isArray(hashes))) {
        throw new AgentError('state is missing file hashes for accepted patches', { code: 'state_corrupt' });
      }
      if (hashes && typeof hashes === 'object' && Object.keys(hashes).length > 0) {
        for (const [rel, sha256] of Object.entries(hashes)) {
          let normalized;
          try {
            normalized = normalizeRelPath(rel);
          } catch (e) {
            throw new AgentError(`state contains an unsafe recorded path: ${String(rel)}`, { code: 'state_corrupt' });
          }
          if (normalized !== rel || typeof sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(sha256)) {
            throw new AgentError(`state contains invalid file hash metadata: ${String(rel)}`, { code: 'state_corrupt' });
          }
          const abs = path.resolve(repoRoot, normalized);
          const relativeToRoot = path.relative(path.resolve(repoRoot), abs);
          if (relativeToRoot.startsWith('..') || path.isAbsolute(relativeToRoot)) {
            throw new AgentError(`state contains a path outside the repository: ${rel}`, { code: 'state_corrupt' });
          }
          if (!fs.existsSync(abs)) {
            throw new AgentError(
              `state drift: recorded file is missing: ${rel}`,
              { code: 'state_drift' },
            );
          }
          const actual = hashText(fs.readFileSync(abs, 'utf8'));
          if (actual !== sha256) {
            throw new AgentError(
              `state drift: file changed since the checkpoint: ${rel}`,
              { code: 'state_drift' },
            );
          }
        }
      }
    }
    const state = Object.create(TaskState.prototype);
    state.statePath = statePath;
    raw.file_hashes ??= {};
    state.record = raw;
    return state;
  }

  // Atomic checkpoint write: temp file in the same directory, then rename.
  save() {
    this.record.updated_at = new Date().toISOString();
    const dir = path.dirname(this.statePath);
    fs.mkdirSync(dir, { recursive: true });
    const tmp = path.join(dir, `.${path.basename(this.statePath)}.${crypto.randomBytes(6).toString('hex')}.tmp`);
    try {
      fs.writeFileSync(tmp, JSON.stringify(this.record, null, 2) + '\n', 'utf8');
      fs.renameSync(tmp, this.statePath);
    } catch (e) {
      try { fs.unlinkSync(tmp); } catch { /* ignore */ }
      throw new AgentError(`state write failed: ${e.message}`, { code: 'state_write_failed' });
    }
    return this.record;
  }

  setTurn(turn) {
    this.record.turn = turn;
  }

  setStatus(status) {
    this.record.status = status;
  }

  // Diff identity without shelling out to Git after every patch: a chained
  // hash over the accepted patches, so an identical sequence of accepted
  // changes yields an identical id.
  // Record an accepted patch and store the current SHA-256 of the file in
  // the checkpoint so a later `--resume` can detect drift (a missing or
  // differently-hashed recorded file refuses the resume with `state_drift`).
  notePatch(relPath, contentHash) {
    this.record.patches_applied += 1;
    if (!this.record.files_changed.includes(relPath)) {
      this.record.files_changed.push(relPath);
      this.record.files_changed.sort();
    }
    this.record.file_hashes[relPath] = contentHash;
    this.record.diff_id = hashText(`${this.record.diff_id ?? ''}|${relPath}|${contentHash}`);
    this.record.last_failure = null;
  }

  noteCheck(suite, result) {
    this.record.checks_run.push({ suite, result });
  }

  noteFailure({ failureClass, code, turn }) {
    this.record.last_failure = {
      class: failureClass,
      code: code ?? null,
      turn: turn ?? this.record.turn,
      at: new Date().toISOString(),
    };
  }

  noteRetry(failureClass) {
    this.record.retries_used[failureClass] = (this.record.retries_used[failureClass] ?? 0) + 1;
    return this.record.retries_used[failureClass];
  }

  retriesUsed(failureClass) {
    return this.record.retries_used[failureClass] ?? 0;
  }

  // Compact resume brief. The conversation is not persisted, so a resumed run
  // is re-seeded with this summary instead of a replayed transcript.
  resumeSummary() {
    const r = this.record;
    const lines = [
      `Resuming task ${r.task_id} from checkpoint (previous status: ${r.status}).`,
      `Turns already spent: ${r.turn}. Patches already accepted: ${r.patches_applied}.`,
      `Files already changed: ${r.files_changed.length ? r.files_changed.join(', ') : '(none)'}`,
      `Checks already run: ${r.checks_run.length ? r.checks_run.map((c) => `${c.suite}=${c.result}`).join(', ') : '(none)'}`,
    ];
    if (r.last_failure) {
      lines.push(`Last failure: class=${r.last_failure.class} code=${String(r.last_failure.code)} at turn ${r.last_failure.turn}.`);
    }
    lines.push('Re-read the current file content before patching: earlier edits are already on disk.');
    return lines.join('\n');
  }
}


// ---------------------------------------------------------------------------
// Output bounding
// ---------------------------------------------------------------------------

export function boundOutput(text, maxBytes) {
  const buf = Buffer.from(String(text), 'utf8');
  if (buf.byteLength <= maxBytes) return String(text);
  // Truncate on a UTF-8 boundary.
  let end = maxBytes;
  while (end > 0 && (buf[end] & 0xc0) === 0x80) end--;
  return buf.subarray(0, end).toString('utf8') + `\n[truncated: ${buf.byteLength - end} more bytes]`;
}

// ---------------------------------------------------------------------------
// Cancellation, deadlines, and bounded requests
// ---------------------------------------------------------------------------

// Combine an optional cancellation signal with a per-request timeout. The
// timeout signal's timer is unref'd by Node, so it never keeps the process
// alive on its own.
export function combineSignals(signal, timeoutMs) {
  const parts = [];
  if (Number.isFinite(timeoutMs) && timeoutMs > 0) parts.push(AbortSignal.timeout(timeoutMs));
  if (signal) parts.push(signal);
  if (parts.length === 0) return undefined;
  if (parts.length === 1) return parts[0];
  return AbortSignal.any(parts);
}

// Overall task deadline, independent of the turn budget. It truly bounds the
// whole run: it is checked before every model turn, tool call, and MCP
// request (assertLive), and `boundedTimeout` caps every individual request
// timeout at the remaining time, so no single model request, MCP request,
// run_check, or git_diff can outlive the deadline.
export function createDeadline(totalMs, startedAt = Date.now()) {
  const at = startedAt + totalMs;
  const controller = new AbortController();
  const delay = at - Date.now();
  if (delay <= 0) controller.abort(new Error('task deadline exceeded'));
  else setTimeout(() => controller.abort(new Error('task deadline exceeded')), delay).unref?.();
  return {
    at,
    signal: controller.signal,
    remainingMs: () => at - Date.now(),
    expired: () => Date.now() >= at,
    assertLive(where) {
      if (this.expired()) {
        throw new AgentError(
          `task deadline exceeded before ${where} (deadline bound expired)`,
          { code: 'deadline_exceeded' },
        );
      }
    },
    // The effective timeout for one request: never longer than what is left,
    // so even a slow request is aborted the moment the deadline passes.
    // Once the deadline has expired the value is 0: a request started then
    // must not run at all (it is refused with `deadline_exceeded`).
    boundedTimeout(timeoutMs) {
      const left = this.remainingMs();
      return left > 0 ? Math.min(timeoutMs, left) : 0;
    },
  };
}

// Overall-deadline abort signal. When the deadline passes, this signal fires,
// so an in-flight request is aborted by the deadline bound (classified as
// `deadline_exceeded`) rather than by the per-request timeout bound.
// A single bounded fetch. Aborts are translated into classified AgentErrors
// so the three outcomes stay distinct: a coordinator cancellation, an
// overall deadline expiry, and an operation-specific timeout are never
// confused with one another. A `timeoutMs` of 0 means the deadline has
// already passed: the request must not start at all.
async function boundedFetch(url, init, { signal = null, timeoutMs, label, timeoutCode, errorCode, deadline = null }) {
  const deadlineSignal = deadline?.signal ?? null;
  if (Number.isFinite(timeoutMs) && timeoutMs <= 0) {
    throw new AgentError(`${label} aborted: the overall task deadline has already expired`, {
      code: deadlineCodeFor(timeoutCode),
    });
  }
  try {
    const parts = [];
    if (Number.isFinite(timeoutMs) && timeoutMs > 0) parts.push(AbortSignal.timeout(timeoutMs));
    if (deadlineSignal) parts.push(deadlineSignal);
    if (signal) parts.push(signal);
    const combined = parts.length === 1 ? parts[0] : (parts.length > 1 ? AbortSignal.any(parts) : undefined);
    return await fetch(url, { ...init, signal: combined });
  } catch (e) {
    if (signal?.aborted) {
      throw new AgentError(`${label} cancelled by the coordinator`, { code: 'cancelled' });
    }
    if (deadlineSignal?.aborted) {
      throw new AgentError(`${label} aborted: the overall task deadline expired`, { code: 'deadline_exceeded' });
    }
    if (e?.name === 'TimeoutError' || e?.name === 'AbortError') {
      throw new AgentError(`${label} timed out after ${timeoutMs}ms`, { code: timeoutCode });
    }
    throw new AgentError(`${label} transport error: ${e.message}`, { code: errorCode });
  }
}

// The deadline-expiry code for a request kind: the deadline is one failure
// class (`deadline_exceeded`), reported with the same dedicated code.
function deadlineCodeFor(_timeoutCode) {
  return 'deadline_exceeded';
}

// ---------------------------------------------------------------------------
// Child process helper (shell: false, fixed argv, bounded output, timeout)
// ---------------------------------------------------------------------------

export function runFixedCommand({
  argv,
  cwd,
  timeoutMs = 120000,
  timeoutCode = 'command_timeout',
  maxBytes,
  signal = null,
  deadline = null,
}) {
  return new Promise((resolve, reject) => {
    if (deadline?.expired()) {
      reject(new AgentError(`task deadline exceeded before running ${argv[0]}`, { code: 'deadline_exceeded' }));
      return;
    }
    if (signal?.aborted) {
      reject(new AgentError(`cancelled before running ${argv[0]}`, { code: 'cancelled' }));
      return;
    }
    const child = spawn(argv[0], argv.slice(1), {
      cwd,
      shell: false,
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });
    let stdout = '';
    let stderr = '';
    let killed = false;
    let cancelled = false;
    const effectiveTimeout = deadline ? deadline.boundedTimeout(timeoutMs) : timeoutMs;
    const timer = setTimeout(() => {
      killed = true;
      child.kill('SIGKILL');
    }, effectiveTimeout);
    // Graceful cancellation: kill the child and report a cancellation rather
    // than a timeout, so the coordinator sees the real cause.
    const onAbort = () => {
      cancelled = true;
      child.kill('SIGKILL');
    };
    const onDeadline = () => child.kill('SIGKILL');
    if (signal) signal.addEventListener('abort', onAbort, { once: true });
    if (deadline?.signal) deadline.signal.addEventListener('abort', onDeadline, { once: true });
    const cleanup = () => {
      clearTimeout(timer);
      if (signal) signal.removeEventListener('abort', onAbort);
      if (deadline?.signal) deadline.signal.removeEventListener('abort', onDeadline);
    };
    child.stdout.on('data', (d) => {
      if (!maxBytes || Buffer.byteLength(stdout, 'utf8') < maxBytes * 4) stdout += d.toString('utf8');
    });
    child.stderr.on('data', (d) => {
      if (!maxBytes || Buffer.byteLength(stderr, 'utf8') < maxBytes * 4) stderr += d.toString('utf8');
    });
    child.on('error', (e) => {
      cleanup();
      reject(new AgentError(`failed to spawn ${argv[0]}: ${e.message}`, { code: 'spawn_error' }));
    });
    child.on('close', (code, signalName) => {
      cleanup();
      if (cancelled) {
        reject(new AgentError(`command cancelled: ${argv.join(' ')}`, { code: 'cancelled' }));
        return;
      }
      if (deadline?.signal?.aborted) {
        reject(new AgentError(`task deadline exceeded while running ${argv.join(' ')}`, { code: 'deadline_exceeded' }));
        return;
      }
      if (killed) {
        reject(new AgentError(`command timed out after ${effectiveTimeout}ms: ${argv.join(' ')}`, { code: timeoutCode }));
        return;
      }
      resolve({ code, signal: signalName, stdout, stderr });
    });
  });
}


// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

export function buildTools(policy, audit, { dryRun = false, contract = null, signal = null, deadline = null } = {}) {
  const maxResult = policy.limits.maxToolResultBytes;
  // Contract restrictions (null when no contract is active). When active,
  // apply_patch is limited to contract.allowedFiles and run_check to
  // contract.checkSuites, on top of the base policy rules.
  const allowedFilesSet = contract ? new Set(contract.allowedFiles) : null;
  const checkSuitesSet = contract ? new Set(contract.checkSuites) : null;

  // The LCI MCP client is injected after construction (see main()). The
  // wrappers below read this closure variable, so `_setMcp` can reassign it
  // and the already-constructed tool functions observe the new client.
  let mcpClient = null;

  // True when the path itself is readable, or (for directories) when it
  // could contain readable descendants: a readable glob that matches the
  // path itself or anything below it. Deny rules still beat allow.
  function hasReadableDescendants(rel) {
    const denied = policy.denyRe.some((re) => re.test(rel));
    if (denied) return false;
    const selfReadable = policy.readRe.some((re) => re.test(rel));
    if (selfReadable) return true;
    // A read glob can match a descendant of `rel` only if it is a
    // directory-shaped prefix of the glob (e.g. `tools/local-agent/**`
    // matches `tools/local-agent/agent.mjs`).
    const directoryPrefix = `${rel}/`;
    return policy.read.some((glob) => {
      const normalizedGlob = String(glob).replace(/\\/g, '/');
      const wildcard = normalizedGlob.search(/[?*]/);
      const literalPrefix = wildcard === -1 ? normalizedGlob : normalizedGlob.slice(0, wildcard);
      return literalPrefix.startsWith(directoryPrefix);
    });
  }

  // Recursively collect descendants of a directory, keeping traversal and
  // symlink safety: every existing path is resolved through resolveRepoPath
  // (which rejects `..` traversal and symlink/junction escape), and every
  // returned descendant is filtered through read access + deny rules.
  // Directories are only listed (and descended into) when they could
  // contain readable descendants; denied entries are never exposed.
  function collectDescendants(baseRel, baseAbs, out) {
    const entries = fs.readdirSync(baseAbs, { withFileTypes: true });
    for (const e of entries) {
      const childRel = baseRel ? `${baseRel}/${e.name}` : e.name;
      const childAbs = path.join(baseAbs, e.name);
      // Symlink/junction escape check for existing targets (and parents).
      const { rel } = resolveRepoPath(policy, childRel);
      // Filter every returned entry: include it only if the entry itself is
      // readable, or it is a directory that could contain readable
      // descendants. Never expose denied entries.
      let include = false;
      try {
        checkAccess(policy, rel, 'read');
        include = true;
      } catch {
        if (e.isDirectory()) include = hasReadableDescendants(rel);
      }
      if (!include) continue;
      const st = fs.statSync(childAbs);
      if (st.isDirectory()) {
        out.push(`d ${rel}`);
        collectDescendants(rel, childAbs, out);
      } else {
        out.push(`- ${rel}`);
      }
    }
  }

  async function listFiles({ path: p }) {
    const { rel, abs } = resolveRepoPath(policy, p);
    const st = fs.statSync(abs);
    if (!st.isDirectory()) throw new AgentError(`not a directory: ${rel}`);
    // The directory itself must be readable, or it must have potentially
    // readable descendants (so an authorized directory like
    // tools/local-agent is listable even when the readable glob is
    // `tools/local-agent/**` and the directory path itself does not match).
    let dirReadable = true;
    try {
      checkAccess(policy, rel, 'read');
    } catch {
      dirReadable = false;
    }
    if (!dirReadable && !hasReadableDescendants(rel)) {
      throw new AgentError(`directory is not readable per policy: ${rel}`);
    }
    const out = [];
    collectDescendants(rel, abs, out);
    const lines = out.sort();
    return boundOutput(lines.join('\n') || '(empty directory)', maxResult);
  }

  async function readFile({ path: p, startLine = 1, endLine = null }) {
    const { rel, abs } = resolveRepoPath(policy, p);
    checkAccess(policy, rel, 'read');
    const st = fs.statSync(abs);
    if (!st.isFile()) throw new AgentError(`not a file: ${rel}`);
    const maxLines = policy.limits.maxReadLines;
    const start = Math.max(1, Math.floor(startLine) || 1);
    const end = endLine == null ? start + maxLines - 1 : Math.max(start, Math.floor(endLine));
    if (end - start + 1 > maxLines) {
      throw new AgentError(`read range exceeds maxReadLines (${maxLines})`);
    }
    const content = fs.readFileSync(abs, 'utf8');
    const allLines = content.split('\n');
    const total = allLines.length;
    const slice = allLines.slice(start - 1, end);
    const header = `// ${rel} lines ${start}-${Math.min(end, total)} of ${total}\n`;
    return boundOutput(header + slice.join('\n'), maxResult);
  }

  async function applyPatch({ path: p, oldText, newText = '', expectedReplacements = 1 }) {
    if (dryRun) {
      throw new AgentError('apply_patch is blocked in dry-run mode', { code: 'dry_run_blocked' });
    }
    // Validate the patch shape before touching the filesystem.
    if (typeof oldText !== 'string' || oldText.length === 0) {
      throw new AgentError('apply_patch requires a non-empty oldText string', { code: 'bad_patch' });
    }
    if (typeof newText !== 'string') {
      throw new AgentError('apply_patch requires newText to be a string', { code: 'bad_patch' });
    }
    // Byte budget is enforced over the combined old+new patch payload.
    const patchBytes = Buffer.byteLength(oldText, 'utf8') + Buffer.byteLength(newText, 'utf8');
    if (patchBytes > policy.limits.maxPatchBytes) {
      throw new AgentError(`patch exceeds maxPatchBytes (${policy.limits.maxPatchBytes})`, { code: 'patch_too_large' });
    }
    const { rel, abs } = resolveRepoPath(policy, p);
    checkAccess(policy, rel, 'write');
    if (allowedFilesSet && !allowedFilesSet.has(rel)) {
      throw new AgentError(`path is not in the task contract allowedFiles: ${rel}`, { code: 'contract_path_denied' });
    }
    if (!fs.existsSync(abs)) throw new AgentError(`file does not exist: ${rel}`);
    const st = fs.statSync(abs);
    if (!st.isFile()) throw new AgentError(`not a file: ${rel}`);
    const current = fs.readFileSync(abs, 'utf8');
    const expected = Math.max(1, Math.floor(expectedReplacements) || 1);
    const count = countOccurrences(current, oldText);
    if (count !== expected) {
      // Patch precondition mismatch: hand back the current nearby source so
      // the next attempt can be an exact patch instead of a guess.
      const context = nearbySourceContext(current, oldText, rel);
      throw new AgentError(
        `apply_patch failed: expected ${expected} occurrence(s) of oldText, found ${count} in ${rel}\n` +
          'The file on disk does not contain your oldText exactly. Current nearby source follows; ' +
          'copy the exact text from it and send one fresh apply_patch.\n' +
          boundOutput(context, Math.min(maxResult, 4096)),
        { code: 'patch_mismatch', fatal: false },
      );
    }
    // Exact-text replacement: replace every occurrence (count === expected)
    // with newText atomically. An empty newText removes the matched text.
    const updated = current.split(oldText).join(newText);
    // Atomic write: temp file in same directory, then rename.
    const dir = path.dirname(abs);
    const tmp = path.join(dir, `.${path.basename(abs)}.${crypto.randomBytes(6).toString('hex')}.tmp`);
    try {
      fs.writeFileSync(tmp, updated, 'utf8');
      fs.renameSync(tmp, abs);
    } catch (e) {
      try { fs.unlinkSync(tmp); } catch { /* ignore */ }
      throw new AgentError(`apply_patch write failed: ${e.message}`, { code: 'write_error' });
    }
    return `applied ${expected} replacement(s) to ${rel}`;
  }

  async function gitDiff() {
    const { stdout, code, stderr } = await runFixedCommand({
      argv: ['git', '-C', policy.repoRoot, 'diff'],
      cwd: policy.repoRoot,
      timeoutMs: policy.limits.gitTimeoutMs ?? DEFAULT_LIMITS.gitTimeoutMs,
      timeoutCode: 'git_timeout',
      maxBytes: maxResult,
      signal,
      deadline,
    });
    if (code !== 0) {
      throw new AgentError(`git diff failed (exit ${code}): ${boundOutput(stderr, 2048)}`, { code: 'git_error' });
    }
    return boundOutput(stdout || '(no changes)', maxResult);
  }

  async function runCheck({ suite }) {
    if (dryRun) {
      throw new AgentError('run_check is blocked in dry-run mode', { code: 'dry_run_blocked' });
    }
    if (typeof suite !== 'string' || !(suite in policy.checks.suites)) {
      const known = Object.keys(policy.checks.suites).join(', ');
      throw new AgentError(`unknown check suite: ${String(suite)} (known: ${known})`, { code: 'bad_suite' });
    }
    if (checkSuitesSet && !checkSuitesSet.has(suite)) {
      throw new AgentError(`check suite is not in the task contract checkSuites: ${suite}`, { code: 'contract_suite_denied' });
    }
    const suiteValue = policy.checks.suites[suite];
    const scriptAbs = path.resolve(policy.repoRoot, policy.checks.script);
    if (!fs.existsSync(scriptAbs)) {
      throw new AgentError(`check script not found: ${policy.checks.script}`, { code: 'script_missing' });
    }
    const isWin = process.platform === 'win32';
    const argv = isWin
      ? ['powershell.exe', '-NoProfile', '-NonInteractive', '-File', scriptAbs, '-Suite', suiteValue]
      : ['pwsh', '-NoProfile', '-NonInteractive', '-File', scriptAbs, '-Suite', suiteValue];
    const { stdout, code, stderr } = await runFixedCommand({
      argv,
      cwd: policy.repoRoot,
      timeoutMs: policy.limits.checkTimeoutMs ?? DEFAULT_LIMITS.checkTimeoutMs,
      timeoutCode: 'check_timeout',
      maxBytes: maxResult,
      signal,
      deadline,
    });
    const out = boundOutput((stdout || '') + (stderr ? `\n[stderr]\n${stderr}` : ''), maxResult);
    if (code !== 0) {
      // A failing check is a recoverable result, not a harness fault: the
      // model gets only the failure output and may send one correction.
      throw new AgentError(`run_check ${suite} failed (exit ${code}):\n${out}`, { code: 'check_failed', fatal: false });
    }
    return out;
  }

  // MCP tools are wired in the agent loop (they need the MCP client), but we
  // expose them here as thin wrappers so the tool surface is uniform. Both
  // read the shared `mcpClient` closure variable set by `_setMcp`.
  async function lciIndexStatus({ workspacePath }) {
    if (!mcpClient) throw new AgentError('MCP client not initialized');
    const wp = workspacePath ?? policy.repoRoot;
    const result = await mcpClient.callTool('index_status', { workspace_path: wp });
    return boundOutput(typeof result === 'string' ? result : JSON.stringify(result, null, 2), maxResult);
  }

  async function lciSearchCode({ query, workspacePath, topK = 10 }) {
    // Validate the query before requiring the MCP client so a bad query is
    // reported as a query error rather than an MCP-not-initialized error.
    if (typeof query !== 'string' || query.length === 0) {
      throw new AgentError('lci_search_code requires a non-empty query');
    }
    if (!mcpClient) throw new AgentError('MCP client not initialized');
    const wp = workspacePath ?? policy.repoRoot;
    const result = await mcpClient.callTool('search_code', {
      workspace_path: wp,
      query,
      top_k: Math.max(1, Math.floor(topK) || 10),
    });
    return boundOutput(typeof result === 'string' ? result : JSON.stringify(result, null, 2), maxResult);
  }

  // Deterministic change-aware verification planner. Pure: it reads only the
  // already-resolved policy and the active contract (if any) and returns a
  // bounded pretty-JSON plan. It performs no filesystem, process, MCP,
  // model, or Git action.
  async function planVerificationTool({ changedPaths }) {
    const plan = planVerification(policy, changedPaths, { contract });
    return boundOutput(JSON.stringify(plan, null, 2), maxResult);
  }

  const tools = {
    list_files: { fn: listFiles, schema: { path: 'string (relative)' } },
    read_file: { fn: readFile, schema: { path: 'string (relative)', startLine: 'int', endLine: 'int' } },
    apply_patch: { fn: applyPatch, schema: { path: 'string (relative)', oldText: 'string (exact text to replace)', newText: 'string (replacement text; empty removes)', expectedReplacements: 'int' } },
    git_diff: { fn: gitDiff, schema: {} },
    run_check: { fn: runCheck, schema: { suite: 'string (named suite from policy)' } },
    plan_verification: { fn: planVerificationTool, schema: { changedPaths: 'array of repository-relative paths' } },
    lci_index_status: { fn: lciIndexStatus, schema: { workspacePath: 'string (optional)' } },
    lci_search_code: { fn: lciSearchCode, schema: { query: 'string', workspacePath: 'string (optional)', topK: 'int' } },
  };
  // Inject the MCP client into the LCI wrappers. Because the wrappers read the
  // `mcpClient` closure variable, a single assignment updates both tools.
  tools._setMcp = (client) => {
    mcpClient = client;
  };
  return tools;
}

function countOccurrences(haystack, needle) {
  if (needle.length === 0) return 0;
  let count = 0;
  let idx = haystack.indexOf(needle);
  while (idx !== -1) {
    count++;
    idx = haystack.indexOf(needle, idx + needle.length);
  }
  return count;
}

// Current source around the most plausible anchor for a failed patch. The
// anchor is the first non-blank line of oldText (exact, then trimmed); with no
// anchor the head of the file is returned. Line numbers are 1-based so the
// next attempt can quote exact text.
export function nearbySourceContext(content, oldText, relPath, { radius = 12, maxLines = 60 } = {}) {
  const lines = content.split('\n');
  const anchorLine = String(oldText).split('\n').find((l) => l.trim().length > 0) ?? '';
  const anchor = anchorLine.trim();
  let index = -1;
  if (anchor.length > 0) {
    index = lines.findIndex((l) => l === anchorLine);
    if (index === -1) index = lines.findIndex((l) => l.includes(anchor));
  }
  const start = index === -1 ? 0 : Math.max(0, index - radius);
  const end = Math.min(lines.length, start + Math.min(maxLines, radius * 2 + 1));
  const header =
    index === -1
      ? `// ${relPath}: no anchor line from oldText was found; showing lines ${start + 1}-${end} of ${lines.length}`
      : `// ${relPath}: current source lines ${start + 1}-${end} of ${lines.length}`;
  const body = lines.slice(start, end).map((l, i) => `${start + i + 1}: ${l}`);
  return [header, ...body].join('\n');
}

// ---------------------------------------------------------------------------
// MCP client (JSON-RPC over HTTP; JSON or SSE)
// ---------------------------------------------------------------------------

export class McpClient {
  constructor(endpoint, { timeoutMs = DEFAULT_LIMITS.mcpRequestTimeoutMs, signal = null, deadline = null } = {}) {
    this.endpoint = endpoint;
    this.timeoutMs = timeoutMs;
    this.signal = signal;
    this.deadline = deadline;
    this.sessionId = null;
    this.nextId = 1;
    this.initialized = false;
  }

  async _post(body) {
    const headers = { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream' };
    if (this.sessionId) headers['Mcp-Session-Id'] = this.sessionId;
    const res = await boundedFetch(
      this.endpoint,
      { method: 'POST', headers, body: JSON.stringify(body) },
      {
        signal: this.signal,
        timeoutMs: this.timeoutMs,
        label: `MCP request ${String(body.method)}`,
        timeoutCode: 'mcp_timeout',
        errorCode: 'mcp_error',
        deadline: this.deadline,
      },
    );
    const sessionId = res.headers.get('mcp-session-id');
    if (sessionId) this.sessionId = sessionId;
    const text = await res.text();
    const contentType = res.headers.get('content-type') || '';
    // Parse defensively: a non-JSON HTTP error body must never leak a raw
    // JSON.parse SyntaxError.
    let parsed = null;
    if (contentType.includes('text/event-stream')) {
      parsed = parseSse(text);
    } else if (text.length) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = null;
      }
    }
    if (!res.ok) {
      const msg =
        (parsed && typeof parsed === 'object' && (parsed.error?.message || parsed.message)) ||
        (text.length ? boundOutput(text, 512) : `HTTP ${res.status}`);
      throw new AgentError(`MCP request failed: ${msg}`, { code: 'mcp_error' });
    }
    // A successful HTTP response may still carry a JSON-RPC error object.
    if (parsed && typeof parsed === 'object' && parsed.error) {
      const msg = parsed.error.message || JSON.stringify(parsed.error);
      throw new AgentError(`MCP request failed: ${msg}`, { code: 'mcp_error' });
    }
    return parsed;
  }

  async initialize() {
    const res = await this._post({
      jsonrpc: '2.0',
      id: this.nextId++,
      method: 'initialize',
      params: {
        protocolVersion: '2024-11-05',
        capabilities: {},
        clientInfo: { name: 'local-agent', version: '1.0.0' },
      },
    });
    // Mark initialized only after a valid, non-error result was returned.
    if (!res || typeof res !== 'object' || res.error || !res.result) {
      throw new AgentError('MCP initialize returned no valid result', { code: 'mcp_error' });
    }
    this.initialized = true;
    // Send initialized notification (no id). Some servers reject
    // notifications; that rejection is intentionally fail-open (documented).
    try {
      await this._post({ jsonrpc: '2.0', method: 'notifications/initialized' });
    } catch {
      // Some servers reject notifications; non-fatal.
    }
    return res.result;
  }

  async callTool(name, args) {
    if (!this.initialized) await this.initialize();
    const res = await this._post({
      jsonrpc: '2.0',
      id: this.nextId++,
      method: 'tools/call',
      params: { name, arguments: args },
    });
    if (res?.error) {
      throw new AgentError(`MCP tool ${name} error: ${res.error.message || JSON.stringify(res.error)}`, { code: 'mcp_tool_error' });
    }
    const content = res?.result?.content;
    if (Array.isArray(content)) {
      return content.map((c) => c.text ?? JSON.stringify(c)).join('\n');
    }
    return res?.result ? JSON.stringify(res.result) : '';
  }

  // Best-effort session teardown. The current LCI server does not require a
  // close handshake, so this is a no-op unless a future client adds one.
  // Idempotent and overlap-safe: a single in-flight close promise guards the
  // teardown, the session is cleared exactly once, and no close/delete
  // request is ever sent twice.
  async close() {
    if (this._closePromise) return this._closePromise;
    this._closePromise = (async () => {
      // Clear the session exactly once.
      this.initialized = false;
      this.sessionId = null;
    })();
    try {
      return await this._closePromise;
    } catch {
      // A failed teardown must not prevent a later close from retrying.
      this._closePromise = null;
      throw new AgentError('MCP close failed', { code: 'mcp_error' });
    }
  }
}

// Parse an SSE body into the last JSON-RPC message.
export function parseSse(text) {
  const lines = String(text).split('\n');
  let last = null;
  let data = '';
  for (const line of lines) {
    if (line.startsWith('data:')) {
      data += line.slice(5).trimStart();
    } else if (line.trim() === '' && data.length) {
      try {
        last = JSON.parse(data);
      } catch {
        // ignore malformed event
      }
      data = '';
    }
  }
  if (data.length) {
    try { last = JSON.parse(data); } catch { /* ignore */ }
  }
  return last;
}

// ---------------------------------------------------------------------------
// OpenAI-compatible chat completions tool loop
// ---------------------------------------------------------------------------

const TOOL_DEFINITIONS = [
  {
    type: 'function',
    function: {
      name: 'list_files',
      description: 'List files and directories under a repository-relative path.',
      parameters: {
        type: 'object',
        properties: { path: { type: 'string', description: 'Repository-relative directory path' } },
        required: ['path'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'read_file',
      description: 'Read a bounded line range of a repository-relative file.',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string' },
          startLine: { type: 'integer' },
          endLine: { type: 'integer' },
        },
        required: ['path'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'apply_patch',
      description: 'Replace exact-text occurrences in a file. oldText must occur exactly expectedReplacements times; every occurrence is replaced with newText (an empty newText removes the matched text).',
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string' },
          oldText: { type: 'string', description: 'Exact text to replace (non-empty)' },
          newText: { type: 'string', description: 'Replacement text; empty string removes the matched text' },
          expectedReplacements: { type: 'integer', description: 'Expected number of occurrences (default 1)' },
        },
        required: ['path', 'oldText'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'git_diff',
      description: 'Show the read-only git diff of the working tree.',
      parameters: { type: 'object', properties: {} },
    },
  },
  {
    type: 'function',
    function: {
      name: 'run_check',
      description: 'Run a named Test.ps1 suite declared in the policy.',
      parameters: {
        type: 'object',
        properties: { suite: { type: 'string', description: 'Named suite key from policy checks.suites' } },
        required: ['suite'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'plan_verification',
      description: 'Plan deterministic change-aware verification for the given changed repository-relative paths using the current policy pathRules and active contract. Returns {focused, deferred, unmatchedPaths}. Performs no filesystem, process, MCP, model, or Git action.',
      parameters: {
        type: 'object',
        properties: {
          changedPaths: {
            type: 'array',
            items: { type: 'string' },
            description: 'Nonempty array of unique repository-relative changed paths',
          },
        },
        required: ['changedPaths'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'lci_index_status',
      description: 'Call LCI index_status via the MCP endpoint.',
      parameters: {
        type: 'object',
        properties: { workspacePath: { type: 'string' } },
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'lci_search_code',
      description: 'Call LCI search_code via the MCP endpoint.',
      parameters: {
        type: 'object',
        properties: {
          query: { type: 'string' },
          workspacePath: { type: 'string' },
          topK: { type: 'integer' },
        },
        required: ['query'],
      },
    },
  },
];

async function chatCompletion(policy, messages, { signal = null, timeoutMs = null, deadline = null } = {}) {
  const apiKey = process.env[policy.model.apiKeyEnv];
  if (!apiKey) {
    throw new AgentError(`API key environment variable ${policy.model.apiKeyEnv} is not set`, { code: 'no_api_key' });
  }
  const url = policy.model.endpoint.replace(/\/$/, '') + '/chat/completions';
  const res = await boundedFetch(
    url,
    {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${apiKey}`,
      },
      body: JSON.stringify({
        model: policy.model.name,
        messages,
        tools: TOOL_DEFINITIONS,
      }),
    },
    {
      signal,
      timeoutMs: timeoutMs ?? policy.limits.modelRequestTimeoutMs ?? DEFAULT_LIMITS.modelRequestTimeoutMs,
      label: 'model request',
      timeoutCode: 'model_timeout',
      errorCode: 'model_error',
      deadline,
    },
  );
  const text = await res.text();
  if (!res.ok) {
    throw new AgentError(`model request failed (HTTP ${res.status}): ${boundOutput(text, 2048)}`, { code: 'model_error' });
  }
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    throw new AgentError(`model returned non-JSON response: ${e.message}`, { code: 'model_bad_json' });
  }
  const choice = parsed?.choices?.[0];
  if (!choice) throw new AgentError('model response had no choices', { code: 'model_no_choices' });
  return choice;
}

// A short correction prompt, used when the model produced no response content
// and no saved change. It is deliberately tiny: the full task is already in
// the conversation, and a long re-statement makes the next turn worse.
const NO_CHANGE_CORRECTION =
  'Your last reply requested no file change. Make the smallest exact-text apply_patch ' +
  'that satisfies the task, inside the allowed files. Reply with the tool call, not prose.';

// Stable signature of a tool call, used to detect a repeated identical call.
function toolCallSignature(name, args) {
  return hashText(`${name}|${JSON.stringify(args ?? {})}`);
}

// Run the model/tool loop. Returns a recovery-aware outcome:
// `{ status, finalText, failure, state }`. The caller maps the status to an
// exit code; `state` is the (already checkpointed) TaskState when one is used.
export async function runAgentLoop({
  policy,
  audit,
  tools,
  task,
  systemPrompt,
  state = null,
  signal = null,
  deadline = null,
  resumeSummary = null,
  stopAfterFirstPatch = false,
  stopAfterFirstFailedCheck = false,
}) {
  const messages = [
    { role: 'system', content: systemPrompt },
    { role: 'user', content: task },
  ];
  if (resumeSummary) messages.push({ role: 'user', content: resumeSummary });
  const maxTurns = policy.limits.maxTurns;
  const retryBudget = policy.limits.retryBudget ?? DEFAULT_RETRY_BUDGET;
  const clock = deadline ?? createDeadline(policy.limits.taskDeadlineMs ?? DEFAULT_LIMITS.taskDeadlineMs);
  const retriesUsed = {};
  // Signature -> { code, count } for failing tool calls, so the same failing
  // call is never repeated indefinitely.
  const failedCalls = new Map();
  let patchesApplied = state ? state.record.patches_applied : 0;
  let noChangeCorrectionSent = false;

  const startTurn = state ? state.record.turn : 0;

  // Record a failure on the checkpoint and decide whether the class still has
  // retry budget left.
  const consumeRetry = (err, turn) => {
    const failureClass = classifyFailure(err);
    state?.noteFailure({ failureClass, code: err.code, turn });
    const budget = retryBudget[failureClass] ?? 0;
    const used = state ? state.retriesUsed(failureClass) : (retriesUsed[failureClass] ?? 0);
    if (!isRetryable(err) || used >= budget) return { failureClass, allowed: false };
    if (state) state.noteRetry(failureClass);
    else retriesUsed[failureClass] = used + 1;
    audit.lifecycle({ event: 'retry', turn, failureClass, error: err.code ?? null });
    return { failureClass, allowed: true };
  };

  const finish = (status, failure = null, finalText = '') => {
    if (state) {
      state.setStatus(status);
      state.save();
    }
    audit.lifecycle({ event: 'finished', turn: state?.record.turn ?? null, status, failureClass: failure?.class ?? null, error: failure?.code ?? null });
    return { status, finalText, failure, state };
  };

  for (let turn = startTurn + 1; turn <= startTurn + maxTurns; turn++) {
    if (signal?.aborted) {
      return finish('cancelled', { class: 'coordinator_cancelled', code: 'cancelled', message: 'cancelled before the model turn' });
    }
    if (clock.expired()) {
      return finish('deadline_exceeded', { class: 'deadline_exceeded', code: 'deadline_exceeded', message: 'task deadline exceeded' });
    }
    state?.setTurn(turn);

    const t0 = Date.now();
    const inputBytes = Buffer.byteLength(JSON.stringify(messages), 'utf8');
    let choice = null;
    let modelError = null;
    // Transport timeout: retry the same request once, within the class budget.
    for (let attempt = 0; ; attempt++) {
      try {
        choice = await chatCompletion(policy, messages, {
          signal,
          timeoutMs: clock.boundedTimeout(policy.limits.modelRequestTimeoutMs ?? DEFAULT_LIMITS.modelRequestTimeoutMs),
          deadline: clock,
        });
        modelError = null;
        break;
      } catch (e) {
        modelError = e;
        const { failureClass, allowed } = consumeRetry(e, turn);
        if (!allowed) {
          audit.modelTurn({ turn, ok: false, error: e.code || e.name, inputBytes, outputBytes: 0, durationMs: Date.now() - t0 });
          const status = failureClass === 'coordinator_cancelled'
            ? 'cancelled'
            : failureClass === 'deadline_exceeded'
              ? 'deadline_exceeded'
              : 'failed';
          return finish(status, { class: failureClass, code: e.code ?? null, message: e.message });
        }
      }
    }
    const message = choice.message || {};
    const outputBytes = Buffer.byteLength(JSON.stringify(message), 'utf8');
    audit.modelTurn({ turn, ok: true, error: null, inputBytes, outputBytes, durationMs: Date.now() - t0 });

    const toolCalls = message.tool_calls;
    if (!Array.isArray(toolCalls) || toolCalls.length === 0) {
      const finalText = message.content ?? '';
      // A final response that requested no file change is not an accepted
      // result. Send one short correction, then stop and report no_changes.
      if (patchesApplied === 0) {
        if (!noChangeCorrectionSent) {
          noChangeCorrectionSent = true;
          audit.lifecycle({ event: 'no_change_correction', turn });
          messages.push({ role: 'assistant', content: finalText || null });
          messages.push({ role: 'user', content: NO_CHANGE_CORRECTION });
          continue;
        }
        return finish('no_changes', null, finalText);
      }
      return finish('awaiting_review', null, finalText);
    }
    // Preserve the assistant message with tool_calls verbatim.
    messages.push({ role: 'assistant', content: message.content ?? null, tool_calls: toolCalls });

    for (const call of toolCalls) {
      if (signal?.aborted) {
        messages.push({ role: 'tool', tool_call_id: call.id, content: 'ERROR: cancelled by the coordinator' });
        return finish('cancelled', { class: 'coordinator_cancelled', code: 'cancelled', message: 'cancelled before a tool call' });
      }
      if (clock.expired()) {
        messages.push({ role: 'tool', tool_call_id: call.id, content: 'ERROR: task deadline exceeded' });
        return finish('deadline_exceeded', { class: 'deadline_exceeded', code: 'deadline_exceeded', message: 'task deadline exceeded' });
      }
      const name = call?.function?.name;
      let args = {};
      try {
        args = call?.function?.arguments ? JSON.parse(call.function.arguments) : {};
      } catch {
        args = {};
      }
      const signature = toolCallSignature(name, args);
      const tool = tools[name];
      const t1 = Date.now();
      const inputBytes2 = Buffer.byteLength(JSON.stringify(args), 'utf8');
      let resultText;
      let ok = true;
      let errorCode = null;
      let thrown = null;
      try {
        if (!tool) {
          throw new AgentError(`unknown tool: ${name}`, { code: 'unknown_tool' });
        }
        resultText = await tool.fn(args);
      } catch (e) {
        ok = false;
        thrown = e;
        errorCode = e.code || e.name;
        resultText = `ERROR: ${e.message}`;
      }
      audit.toolCall({
        tool: name,
        ok,
        error: errorCode,
        inputBytes: inputBytes2,
        outputBytes: Buffer.byteLength(resultText, 'utf8'),
        durationMs: Date.now() - t1,
      });
      messages.push({ role: 'tool', tool_call_id: call.id, content: resultText });

      if (ok) {
        failedCalls.delete(signature);
        // Checkpoint after every accepted patch.
        if (name === 'apply_patch' && state) {
          try {
            const { rel, abs } = resolveRepoPath(policy, args.path);
            state.notePatch(rel, hashText(fs.readFileSync(abs, 'utf8')));
            state.save();
            audit.lifecycle({ event: 'checkpoint', turn, status: state.record.status });
          } catch (e) {
            throw new AgentError(`checkpoint after patch failed: ${e.message}`, { code: 'state_write_failed' });
          }
        }
        if (name === 'apply_patch') {
          patchesApplied++;
          if (stopAfterFirstPatch) {
            return finish('stopped_after_patch', null, 'stopped after the first accepted patch');
          }
        }
        if (name === 'run_check' && state) {
          state.noteCheck(args.suite ?? '(unknown)', 'passed');
          state.save();
        }
        continue;
      }

      // A repeated identical failing call means the model is stuck: stop and
      // return control to the coordinator instead of burning the budget.
      const previous = failedCalls.get(signature);
      if (previous && previous.code === errorCode) {
        previous.count += 1;
        failedCalls.set(signature, previous);
        state?.noteFailure({ failureClass: classifyFailure(thrown), code: errorCode, turn });
        audit.lifecycle({ event: 'repeated_failure', turn, failureClass: classifyFailure(thrown), error: errorCode });
        return finish('failed', {
          class: classifyFailure(thrown),
          code: errorCode,
          message: `repeated identical failing ${name} call (${errorCode}); returning control to the coordinator`,
          repeated: true,
        });
      }
      failedCalls.set(signature, { code: errorCode, count: 1 });

      if (name === 'run_check' && errorCode === 'check_failed' && state) {
        state.noteCheck(args.suite ?? '(unknown)', 'failed');
        state.save();
      }

      if (name === 'run_check' && errorCode === 'check_failed' && stopAfterFirstFailedCheck) {
        return finish('stopped_after_failed_check', { class: 'check_failed', code: errorCode, message: thrown.message });
      }
      const { failureClass, allowed } = consumeRetry(thrown, turn);
      if (allowed) {
        // Recoverable within budget: the model already has the failure text
        // (a patch mismatch also carries the current nearby source) and may
        // send one correction.
        continue;
      }
      const status = failureClass === 'coordinator_cancelled'
        ? 'cancelled'
        : failureClass === 'deadline_exceeded'
          ? 'deadline_exceeded'
          : 'failed';
      return finish(status, { class: failureClass, code: errorCode, message: thrown.message });
    }
  }
  return finish('failed', {
    class: 'harness_failure',
    code: 'max_turns',
    message: `exceeded maxTurns (${maxTurns})`,
  });
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

function buildSystemPrompt(policy, contract = null) {
  const suites = Object.entries(policy.checks.suites)
    .map(([k, v]) => `  - ${k} -> ${v}`)
    .join('\n');
  const contractLines = [];
  if (contract) {
    // Only the contract id and the allowed file/suite names are stated here;
    // the task text already carries the criteria, so no other contract
    // content is duplicated.
    contractLines.push(
      `Active task contract: ${contract.id}`,
      'Tool-enforced contract restrictions apply: apply_patch is restricted to these files: ' +
        (contract.allowedFiles.length ? contract.allowedFiles.join(', ') : '(none)'),
      'run_check is restricted to these suites: ' +
        (contract.checkSuites.length ? contract.checkSuites.join(', ') : '(none)'),
    );
  }
  return [
    'You are a bounded implementation subagent for a local code repository.',
    'Follow AGENTS.md. Use LCI (lci_index_status / lci_search_code) before broad file reading.',
    'You have filesystem tools and must make the edits yourself.',
    'Never touch .agents/skills/rust-skills.',
    'Do not run commands or tests beyond the named run_check suites.',
    'Make no behavior or schema changes beyond the task.',
    'Read at most four targeted files before the first useful write.',
    'apply_patch replaces exact text: oldText must occur exactly expectedReplacements times and every occurrence is replaced with newText (an empty newText removes the matched text). Preserve all required content.',
    `Repository root: ${policy.repoRoot}`,
    `Readable globs: ${policy.read.join(', ')}`,
    `Writable globs: ${policy.write.join(', ')}`,
    `Denied globs: ${policy.deny.join(', ')}`,
    `Limits: maxTurns=${policy.limits.maxTurns} maxReadLines=${policy.limits.maxReadLines} maxPatchBytes=${policy.limits.maxPatchBytes} maxToolResultBytes=${policy.limits.maxToolResultBytes}`,
    'Available check suites:',
    suites,
    ...contractLines,
    'Prefer small, exact-text patches and one focused check proportional to the change.',
    'If apply_patch reports a mismatch, use the current nearby source it returns and send one fresh exact patch; never repeat the same failing call.',
    'A reply that requests no file change is not an accepted result: make the edit through apply_patch.',
    'After making edits, call plan_verification with the changed repository-relative paths and run at most the returned contract-authorized focused suites.',
    'Do not run the deferred finalSuite (e.g. full) during an ordinary slice; it is run once after all accepted slices by the coordinator final integration.',
  ].join('\n');
}

// ---------------------------------------------------------------------------
// Dry-run
// ---------------------------------------------------------------------------

function dryRunReport(policy, task, contract = null, { taskId = null, statePath = null, args = null } = {}) {
  const lines = [
    'Dry-run: policy resolved and task validated. No model contact, no writes.',
    `  policy: ${policy.policyPath}`,
    `  schema: ${policy.schema}`,
    `  repoRoot: ${policy.repoRoot}`,
    `  model: ${policy.model.name} @ ${policy.model.endpoint} (key env: ${policy.model.apiKeyEnv})`,
    `  lci: ${policy.lci.endpoint}`,
    `  audit: ${policy.audit.jsonlPath}`,
    `  limits: maxTurns=${policy.limits.maxTurns} maxReadLines=${policy.limits.maxReadLines} maxPatchBytes=${policy.limits.maxPatchBytes} maxToolResultBytes=${policy.limits.maxToolResultBytes}`,
    `  timeouts: model=${policy.limits.modelRequestTimeoutMs}ms mcp=${policy.limits.mcpRequestTimeoutMs}ms check=${policy.limits.checkTimeoutMs}ms git=${policy.limits.gitTimeoutMs}ms deadline=${policy.limits.taskDeadlineMs}ms`,
    `  retry budget: ${Object.entries(policy.limits.retryBudget).map(([k, v]) => `${k}=${v}`).join(' ')}`,
    `  task bytes: ${Buffer.byteLength(task, 'utf8')}`,
  ];
  if (taskId) lines.push(`  task id: ${taskId}`);
  if (statePath) lines.push(`  checkpoint: ${statePath}`);
  if (args) {
    lines.push(
      `  resume: ${args.resume} stopAfterFirstPatch: ${args.stopAfterFirstPatch} stopAfterFirstFailedCheck: ${args.stopAfterFirstFailedCheck}`,
    );
  }
  if (contract) {
    // Contract summary only: the id plus the allowed-file/check counts.
    // Never the contract text.
    lines.push(
      `  contract: ${contract.id}`,
      `  contract allowed files: ${contract.allowedFiles.length}`,
      `  contract check suites: ${contract.checkSuites.length}`,
    );
  }
  lines.push(
    '  planned tool surface:',
    '    list_files, read_file, apply_patch (blocked in dry-run), git_diff,',
    '    run_check (blocked in dry-run), plan_verification, lci_index_status, lci_search_code',
    `  check suites: ${Object.keys(policy.checks.suites).join(', ')}`,
  );
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

export async function main(argv = process.argv.slice(2)) {
  const args = parseArgs(argv);
  if (args.help) {
    process.stdout.write(HELP_TEXT);
    return 0;
  }
  const policy = loadPolicy(args.policy);

  // Contract path: load and validate the task contract before any dry-run,
  // model contact, or mutation. An invalid contract throws here.
  let contract = null;
  let task;
  if (args.contract !== null) {
    contract = loadTaskContract(args.contract, policy);
    task = contractTaskText(contract);
  } else if (args.task !== null) {
    task = args.task;
  } else {
    const absTaskFile = path.resolve(args.taskFile);
    if (!fs.existsSync(absTaskFile)) {
      throw new AgentError(`task file not found: ${absTaskFile}`);
    }
    task = fs.readFileSync(absTaskFile, 'utf8');
  }
  if (task.trim().length === 0) {
    throw new AgentError('task text is empty');
  }

  // Persistent task identity: the explicit --task-id, else the contract id,
  // else a stable short hash of the task text.
  const taskHash = hashText(task);
  const taskId = args.taskId ?? (contract ? contract.id : `task-${taskHash.slice(0, 12)}`);
  const contractHash = contract ? hashText(JSON.stringify(contract)) : null;
  const statePath = args.state
    ? path.resolve(args.state)
    : path.join(path.dirname(policy.audit.jsonlPath), 'state', `${taskId}.json`);

  if (args.dryRun) {
    process.stdout.write(dryRunReport(policy, task, contract, { taskId, statePath, args }) + '\n');
    return 0;
  }

  const audit = new AuditLog(policy, { contractId: contract ? contract.id : null, taskId });

  // Graceful cancellation: SIGINT/SIGTERM abort in-flight model, MCP, and
  // child-process work; the loop stops at the next safe point and checkpoints.
  const controller = new AbortController();
  const onSignal = () => {
    if (!controller.signal.aborted) {
      audit.lifecycle({ event: 'cancellation_requested' });
      controller.abort();
    }
  };
  process.on('SIGINT', onSignal);
  process.on('SIGTERM', onSignal);

  const deadline = createDeadline(policy.limits.taskDeadlineMs ?? DEFAULT_LIMITS.taskDeadlineMs);
  const tools = buildTools(policy, audit, {
    dryRun: false,
    contract,
    signal: controller.signal,
    deadline,
  });
  const toolNames = Object.keys(tools).filter((k) => !k.startsWith('_'));

  let state;
  let resumeSummary = null;
  if (args.resume) {
    try {
      state = TaskState.load(statePath, {
        taskId,
        taskHash,
        contractId: contract ? contract.id : null,
        contractHash,
        repoRoot: policy.repoRoot,
      });
    } catch (e) {
      process.removeListener('SIGINT', onSignal);
      process.removeListener('SIGTERM', onSignal);
      throw e;
    }
    resumeSummary = state.resumeSummary();
    state.setStatus('running');
    audit.lifecycle({ event: 'resumed', turn: state.record.turn, status: state.record.status });
  } else {
    state = new TaskState({ statePath, taskId, taskHash, contractId: contract ? contract.id : null, contractHash, tools: toolNames });
    audit.lifecycle({ event: 'started', status: 'running' });
  }
  state.save();

  const mcp = new McpClient(policy.lci.endpoint, {
    timeoutMs: policy.limits.mcpRequestTimeoutMs ?? DEFAULT_LIMITS.mcpRequestTimeoutMs,
    signal: controller.signal,
    deadline,
  });
  // Initialize the MCP session once, before the model loop can call any LCI
  // tool. This is the live path only: --help, --dry-run, imports, and offline
  // unit tests never reach here. A failure to initialize is fatal and must
  // surface as an actionable MCP error (nonzero exit).
  try {
    await mcp.initialize();
  } catch (e) {
    const failureClass = classifyFailure(e);
    if (failureClass === 'coordinator_cancelled' || failureClass === 'deadline_exceeded') {
      const status = failureClass === 'coordinator_cancelled' ? 'cancelled' : 'deadline_exceeded';
      state.noteFailure({ failureClass, code: e.code, turn: state.record.turn });
      state.setStatus(status);
      state.save();
      audit.lifecycle({ event: 'finished', status, failureClass, error: e.code });
      process.removeListener('SIGINT', onSignal);
      process.removeListener('SIGTERM', onSignal);
      process.stderr.write(`agent failed: ${e.message}\n`);
      return exitCodeForStatus(status);
    }
    const msg = e instanceof AgentError ? e.message : `MCP initialize failed: ${e.message}`;
    const failure = { class: 'mcp_failure', code: 'mcp_init_failed', message: msg };
    state.noteFailure({ failureClass: 'mcp_failure', code: 'mcp_init_failed', turn: state.record.turn });
    state.setStatus('failed');
    state.save();
    process.removeListener('SIGINT', onSignal);
    process.removeListener('SIGTERM', onSignal);
    throw new AgentError(
      `MCP client failed to initialize against ${policy.lci.endpoint}: ${failure.message} ` +
        '(is the LCI MCP server running and reachable?)',
      { code: 'mcp_init_failed' },
    );
  }
  tools._setMcp(mcp);

  const systemPrompt = buildSystemPrompt(policy, contract);
  try {
    const outcome = await runAgentLoop({
      policy,
      audit,
      tools,
      task,
      systemPrompt,
      state,
      signal: controller.signal,
      deadline,
      resumeSummary,
      stopAfterFirstPatch: args.stopAfterFirstPatch,
      stopAfterFirstFailedCheck: args.stopAfterFirstFailedCheck,
    });
    if (outcome.finalText) process.stdout.write(outcome.finalText + '\n');
    process.stdout.write(runSummary(outcome, statePath) + '\n');
    const code = exitCodeForStatus(outcome.status);
    if (code !== 0) {
      process.stderr.write(
        outcome.failure
          ? `agent failed: ${outcome.failure.message}\n`
          : 'agent finished with no requested file changes\n',
      );
    }
    return code;
  } catch (e) {
    // Only a harness fault reaches here: the loop classifies everything else.
    state.noteFailure({ failureClass: classifyFailure(e), code: e.code ?? null, turn: state.record.turn });
    state.setStatus('failed');
    state.save();
    process.stderr.write(`agent failed: ${e.message}\n`);
    return 1;
  } finally {
    process.removeListener('SIGINT', onSignal);
    process.removeListener('SIGTERM', onSignal);
    // Best-effort cleanup of the MCP session we own.
    try {
      if (typeof mcp.close === 'function') await mcp.close();
    } catch {
      // Cleanup is best-effort; never mask the original outcome.
    }
  }
}

// Terminal status -> process exit code. Kept in one place so the coordinator
// can branch on the outcome without parsing text.
export function exitCodeForStatus(status) {
  switch (status) {
    case 'awaiting_review':
    case 'stopped_after_patch':
    case 'stopped_after_failed_check':
      return 0;
    case 'no_changes':
      return 2;
    case 'cancelled':
    case 'deadline_exceeded':
      return 3;
    default:
      return 1;
  }
}

// Compact human-readable summary of the terminal outcome. Metadata only.
function runSummary(outcome, statePath) {
  const r = outcome.state?.record;
  const lines = [
    `run status: ${outcome.status}`,
    `  task id: ${r?.task_id ?? '(none)'}`,
    `  turns: ${r?.turn ?? 0}  patches: ${r?.patches_applied ?? 0}  checks: ${r?.checks_run?.length ?? 0}`,
    `  files changed: ${r?.files_changed?.length ? r.files_changed.join(', ') : '(none)'}`,
    `  diff id: ${r?.diff_id ?? '(none)'}`,
    `  checkpoint: ${statePath}`,
  ];
  if (outcome.failure) {
    lines.push(`  failure: class=${outcome.failure.class} code=${String(outcome.failure.code)}`);
    lines.push(`  detail: ${outcome.failure.message}`);
  }
  return lines.join('\n');
}

// Run main only when executed directly (not when imported for tests).
// fileURLToPath is Windows-safe (new URL(import.meta.url).pathname is not:
// it drops the drive letter and mishandles percent-encoded characters).
const isDirectRun = (() => {
  try {
    const entry = process.argv[1] ? path.resolve(process.argv[1]) : null;
    if (!entry) return false;
    return entry === path.resolve(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
})();

if (isDirectRun) {
  main().then(
    (code) => process.exit(code),
    (e) => {
      process.stderr.write(`fatal: ${e.message}\n`);
      process.exit(1);
    },
  );
}
