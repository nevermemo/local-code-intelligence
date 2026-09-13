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

Options:
  --policy <path>      Path to a local-agent.policy/v1 JSON file (required).
  --task <text>        Task text (exactly one of --task / --task-file).
  --task-file <path>   Path to a file containing the task text.
  --dry-run            Resolve and validate the policy and task, list the
                       planned tool surface, and exit without contacting the
                       model or writing files.
  --help               Show this help.
`;

export function parseArgs(argv) {
  const out = { policy: null, task: null, taskFile: null, dryRun: false, help: false };
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
    const hasTask = out.task !== null;
    const hasTaskFile = out.taskFile !== null;
    if (hasTask === hasTaskFile) {
      throw new AgentError('exactly one of --task or --task-file is required');
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
    checks,
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
  constructor(policy) {
    this.path = policy.audit.jsonlPath;
    this.dir = path.dirname(this.path);
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

export function buildTools(policy, audit, { dryRun = false } = {}) {
  const maxResult = policy.limits.maxToolResultBytes;

  // The LCI MCP client is injected after construction (see main()). The
  // wrappers below read this closure variable, so `_setMcp` can reassign it
  // and the already-constructed tool functions observe the new client.
  let mcpClient = null;

  async function listFiles({ path: p }) {
    const { rel, abs } = resolveRepoPath(policy, p);
    checkAccess(policy, rel, 'read');
    const st = fs.statSync(abs);
    if (!st.isDirectory()) throw new AgentError(`not a directory: ${rel}`);
    const entries = fs.readdirSync(abs, { withFileTypes: true });
    const lines = entries
      .map((e) => `${e.isDirectory() ? 'd' : '-'} ${e.name}`)
      .sort();
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

  const tools = {
    list_files: { fn: listFiles, schema: { path: 'string (relative)' } },
    read_file: { fn: readFile, schema: { path: 'string (relative)', startLine: 'int', endLine: 'int' } },
    apply_patch: { fn: applyPatch, schema: { path: 'string (relative)', oldText: 'string (exact text to replace)', newText: 'string (replacement text; empty removes)', expectedReplacements: 'int' } },
    git_diff: { fn: gitDiff, schema: {} },
    run_check: { fn: runCheck, schema: { suite: 'string (named suite from policy)' } },
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
  async close() {
    this.initialized = false;
    this.sessionId = null;
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

function buildSystemPrompt(policy) {
  const suites = Object.entries(policy.checks.suites)
    .map(([k, v]) => `  - ${k} -> ${v}`)
    .join('\n');
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
    'Prefer small, exact-text patches and one focused check proportional to the change.',
  ].join('\n');
}

// ---------------------------------------------------------------------------
// Dry-run
// ---------------------------------------------------------------------------

function dryRunReport(policy, task) {
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
    '  planned tool surface:',
    '    list_files, read_file, apply_patch (blocked in dry-run), git_diff,',
    '    run_check (blocked in dry-run), lci_index_status, lci_search_code',
    `  check suites: ${Object.keys(policy.checks.suites).join(', ')}`,
  ];
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

  let task;
  if (args.task !== null) {
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
    process.stdout.write(dryRunReport(policy, task) + '\n');
    return 0;
  }

  const audit = new AuditLog(policy);
  const tools = buildTools(policy, audit, { dryRun: false });
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

  const systemPrompt = buildSystemPrompt(policy);
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
