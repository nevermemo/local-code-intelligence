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

import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const SCHEMA = 'local-agent.policy/v1';
const CONTRACT_SCHEMA = 'local-agent.task/v1';
const CONTRACT_ID_RE = /^[A-Za-z0-9._-]+$/;

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
  --dry-run            Resolve and validate the policy and task, list the
                       planned tool surface, and exit without contacting the
                       model or writing files.
  --help               Show this help.
`;

export function parseArgs(argv) {
  const out = { policy: null, task: null, taskFile: null, contract: null, dryRun: false, help: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
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
  constructor(policy, { contractId = null } = {}) {
    this.path = policy.audit.jsonlPath;
    this.dir = path.dirname(this.path);
    // Contract id only (a short identifier). Never task text, source, patch,
    // or model output.
    this.contractId = contractId;
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
    });
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
// Child process helper (shell: false, fixed argv, bounded output, timeout)
// ---------------------------------------------------------------------------

function runFixedCommand({ argv, cwd, timeoutMs = 120000, maxBytes }) {
  return new Promise((resolve, reject) => {
    const child = spawn(argv[0], argv.slice(1), {
      cwd,
      shell: false,
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });
    let stdout = '';
    let stderr = '';
    let killed = false;
    const timer = setTimeout(() => {
      killed = true;
      child.kill('SIGKILL');
    }, timeoutMs);
    child.stdout.on('data', (d) => {
      stdout += d.toString('utf8');
      if (maxBytes && Buffer.byteLength(stdout, 'utf8') > maxBytes * 4) {
        // Hard cap: kill if output is wildly over budget.
        child.kill('SIGKILL');
      }
    });
    child.stderr.on('data', (d) => {
      stderr += d.toString('utf8');
    });
    child.on('error', (e) => {
      clearTimeout(timer);
      reject(new AgentError(`failed to spawn ${argv[0]}: ${e.message}`, { code: 'spawn_error' }));
    });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      if (killed) {
        reject(new AgentError(`command timed out after ${timeoutMs}ms: ${argv.join(' ')}`, { code: 'timeout' }));
        return;
      }
      resolve({ code, signal, stdout, stderr });
    });
  });
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

export function buildTools(policy, audit, { dryRun = false, contract = null } = {}) {
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
      throw new AgentError(
        `apply_patch failed: expected ${expected} occurrence(s) of oldText, found ${count} in ${rel}`,
        { code: 'patch_mismatch' },
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
      timeoutMs: 60000,
      maxBytes: maxResult,
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
      timeoutMs: 600000,
      maxBytes: maxResult,
    });
    const out = boundOutput((stdout || '') + (stderr ? `\n[stderr]\n${stderr}` : ''), maxResult);
    if (code !== 0) {
      throw new AgentError(`run_check ${suite} failed (exit ${code}):\n${out}`, { code: 'check_failed' });
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

// ---------------------------------------------------------------------------
// MCP client (JSON-RPC over HTTP; JSON or SSE)
// ---------------------------------------------------------------------------

export class McpClient {
  constructor(endpoint) {
    this.endpoint = endpoint;
    this.sessionId = null;
    this.nextId = 1;
    this.initialized = false;
  }

  async _post(body) {
    const headers = { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream' };
    if (this.sessionId) headers['Mcp-Session-Id'] = this.sessionId;
    const res = await fetch(this.endpoint, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });
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

async function chatCompletion(policy, messages) {
  const apiKey = process.env[policy.model.apiKeyEnv];
  if (!apiKey) {
    throw new AgentError(`API key environment variable ${policy.model.apiKeyEnv} is not set`, { code: 'no_api_key' });
  }
  const url = policy.model.endpoint.replace(/\/$/, '') + '/chat/completions';
  const res = await fetch(url, {
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
  });
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

// Run the model/tool loop. Returns the final assistant text.
export async function runAgentLoop({ policy, audit, tools, task, systemPrompt }) {
  const messages = [
    { role: 'system', content: systemPrompt },
    { role: 'user', content: task },
  ];
  const maxTurns = policy.limits.maxTurns;
  let failures = 0;
  for (let turn = 1; turn <= maxTurns; turn++) {
    const t0 = Date.now();
    const inputBytes = Buffer.byteLength(JSON.stringify(messages), 'utf8');
    let choice;
    try {
      choice = await chatCompletion(policy, messages);
    } catch (e) {
      audit.modelTurn({ turn, ok: false, error: e.code || e.name, inputBytes, outputBytes: 0, durationMs: Date.now() - t0 });
      throw e;
    }
    const message = choice.message || {};
    const outputBytes = Buffer.byteLength(JSON.stringify(message), 'utf8');
    audit.modelTurn({ turn, ok: true, error: null, inputBytes, outputBytes, durationMs: Date.now() - t0 });

    const toolCalls = message.tool_calls;
    if (!Array.isArray(toolCalls) || toolCalls.length === 0) {
      // Final answer.
      return message.content ?? '';
    }
    // Preserve the assistant message with tool_calls verbatim.
    messages.push({ role: 'assistant', content: message.content ?? null, tool_calls: toolCalls });
    for (const call of toolCalls) {
      const name = call?.function?.name;
      let args = {};
      try {
        args = call?.function?.arguments ? JSON.parse(call.function.arguments) : {};
      } catch (e) {
        args = {};
      }
      const tool = tools[name];
      const t1 = Date.now();
      const inputBytes2 = Buffer.byteLength(JSON.stringify(args), 'utf8');
      let resultText;
      let ok = true;
      let errorCode = null;
      try {
        if (!tool) {
          throw new AgentError(`unknown tool: ${name}`, { code: 'unknown_tool' });
        }
        resultText = await tool.fn(args);
      } catch (e) {
        ok = false;
        errorCode = e.code || e.name;
        resultText = `ERROR: ${e.message}`;
        if (e.fatal === false) {
          // Non-fatal tool error: report to model and continue.
        } else {
          audit.toolCall({ tool: name, ok: false, error: errorCode, inputBytes: inputBytes2, outputBytes: Buffer.byteLength(resultText, 'utf8'), durationMs: Date.now() - t1 });
          failures++;
          // Fatal tool error: stop the loop.
          messages.push({ role: 'tool', tool_call_id: call.id, content: resultText });
          throw e;
        }
      }
      audit.toolCall({
        tool: name,
        ok,
        error: errorCode,
        inputBytes: inputBytes2,
        outputBytes: Buffer.byteLength(resultText, 'utf8'),
        durationMs: Date.now() - t1,
      });
      if (!ok) failures++;
      messages.push({ role: 'tool', tool_call_id: call.id, content: resultText });
    }
  }
  throw new AgentError(`exceeded maxTurns (${maxTurns})`, { code: 'max_turns' });
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
    'After making edits, call plan_verification with the changed repository-relative paths and run at most the returned contract-authorized focused suites.',
    'Do not run the deferred finalSuite (e.g. full) during an ordinary slice; it is run once after all accepted slices by the coordinator final integration.',
  ].join('\n');
}

// ---------------------------------------------------------------------------
// Dry-run
// ---------------------------------------------------------------------------

function dryRunReport(policy, task, contract = null) {
  const lines = [
    'Dry-run: policy resolved and task validated. No model contact, no writes.',
    `  policy: ${policy.policyPath}`,
    `  schema: ${policy.schema}`,
    `  repoRoot: ${policy.repoRoot}`,
    `  model: ${policy.model.name} @ ${policy.model.endpoint} (key env: ${policy.model.apiKeyEnv})`,
    `  lci: ${policy.lci.endpoint}`,
    `  audit: ${policy.audit.jsonlPath}`,
    `  limits: maxTurns=${policy.limits.maxTurns} maxReadLines=${policy.limits.maxReadLines} maxPatchBytes=${policy.limits.maxPatchBytes} maxToolResultBytes=${policy.limits.maxToolResultBytes}`,
    `  task bytes: ${Buffer.byteLength(task, 'utf8')}`,
  ];
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

  if (args.dryRun) {
    process.stdout.write(dryRunReport(policy, task, contract) + '\n');
    return 0;
  }

  const audit = new AuditLog(policy, { contractId: contract ? contract.id : null });
  const tools = buildTools(policy, audit, { dryRun: false, contract });
  const mcp = new McpClient(policy.lci.endpoint);
  // Initialize the MCP session once, before the model loop can call any LCI
  // tool. This is the live path only: --help, --dry-run, imports, and offline
  // unit tests never reach here. A failure to initialize is fatal and must
  // surface as an actionable MCP error (nonzero exit).
  try {
    await mcp.initialize();
  } catch (e) {
    const msg = e instanceof AgentError ? e.message : `MCP initialize failed: ${e.message}`;
    throw new AgentError(
      `MCP client failed to initialize against ${policy.lci.endpoint}: ${msg} ` +
        '(is the LCI MCP server running and reachable?)',
      { code: 'mcp_init_failed' },
    );
  }
  tools._setMcp(mcp);

  const systemPrompt = buildSystemPrompt(policy, contract);
  try {
    const finalText = await runAgentLoop({ policy, audit, tools, task, systemPrompt });
    process.stdout.write(finalText + '\n');
    return 0;
  } catch (e) {
    process.stderr.write(`agent failed: ${e.message}\n`);
    return 1;
  } finally {
    // Best-effort cleanup of the MCP session we own.
    try {
      if (typeof mcp.close === 'function') await mcp.close();
    } catch {
      // Cleanup is best-effort; never mask the original outcome.
    }
  }
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
