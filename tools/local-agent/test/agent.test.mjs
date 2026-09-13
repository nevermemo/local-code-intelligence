// tools/local-agent/test/agent.test.mjs
//
// Offline unit tests for the exported helpers in ../agent.mjs.
// Run with: node --test test/agent.test.mjs
//
// These tests never contact 127.0.0.1:8765 or 127.0.0.1:8768 and never spawn
// the model or LCI services. Filesystem tests use OS temporary directories.
// Every test that touches global fetch, process.env, or the process streams
// restores them in a finally block, and every temp directory is removed.
// No test ever invokes git inside a temp repository: tool-call scenarios that
// would otherwise need git_diff use an injected harmless fake tool instead.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

import {
  AgentError,
  parseArgs,
  globToRegExp,
  globMatch,
  normalizeRelPath,
  loadPolicy,
  checkAccess,
  resolveRepoPath,
  loadTaskContract,
  contractTaskText,
  AuditLog,
  boundOutput,
  parseSse,
  planVerification,
  buildTools,
  McpClient,
  runAgentLoop,
  main,
} from '../agent.mjs';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// Build a minimal valid policy object (already "resolved" shape) for
// checkAccess / resolveRepoPath / buildTools tests.
function makePolicy({ repoRoot, read = [], write = [], deny = [] }) {
  return {
    schema: 'local-agent.policy/v1',
    policyPath: path.join(repoRoot, 'policy.json'),
    repoRoot,
    read,
    write,
    deny,
    readRe: read.map(globToRegExp),
    writeRe: write.map(globToRegExp),
    denyRe: deny.map(globToRegExp),
    model: { name: 'qwen3.8-27b', endpoint: 'http://127.0.0.1:8765/v1', apiKeyEnv: 'QWEN_API_KEY' },
    lci: { endpoint: 'http://127.0.0.1:8768/mcp' },
    limits: { maxTurns: 24, maxReadLines: 400, maxPatchBytes: 24576, maxToolResultBytes: 16384 },
    checks: { script: 'scripts/Test.ps1', suites: { chunk: 'Chunk' } },
    audit: { jsonlPath: path.join(repoRoot, 'test-results', 'local-agent', 'audit.jsonl') },
  };
}

// Write a policy JSON file (valid or invalid) into a temp repo. Does NOT call
// loadPolicy: rejection tests must invoke loadPolicy inside assert.throws so
// the throw is observed rather than escaping the helper.
function writePolicyFile(repoRoot, overrides = {}) {
  const base = {
    schema: 'local-agent.policy/v1',
    repositoryRoot: '.',
    read: ['src/**', 'docs/**'],
    write: ['src/**'],
    deny: ['secret/**', 'Cargo.lock'],
    model: { name: 'qwen3.8-27b', endpoint: 'http://127.0.0.1:8765/v1', apiKeyEnv: 'QW_API_KEY' },
    lci: { endpoint: 'http://127.0.0.1:8768/mcp' },
    limits: { maxTurns: 24, maxReadLines: 400, maxPatchBytes: 24576, maxToolResultBytes: 16384 },
    checks: { script: 'scripts/Test.ps1', suites: { chunk: 'Chunk' } },
    audit: { jsonlPath: 'test-results/local-agent/audit.jsonl' },
  };
  const merged = { ...base, ...overrides };
  const policyPath = path.join(repoRoot, 'policy.json');
  fs.writeFileSync(policyPath, JSON.stringify(merged, null, 2), 'utf8');
  return policyPath;
}

// Write a task-contract JSON file (valid or invalid) into a temp repo. Does
// NOT call loadTaskContract: rejection tests must invoke loadTaskContract
// inside assert.throws so the throw is observed rather than escaping.
function writeContractFile(repoRoot, overrides = {}) {
  const base = {
    schema: 'local-agent.task/v1',
    id: 'task-1',
    objective: 'SECRET_OBJECTIVE do the thing',
    allowedFiles: ['src/a.rs'],
    acceptance: ['SECRET_ACCEPTANCE criterion'],
    prohibited: ['SECRET_PROHIBITED action'],
    checkSuites: ['chunk'],
  };
  const merged = { ...base, ...overrides };
  const contractPath = path.join(repoRoot, 'task.json');
  fs.writeFileSync(contractPath, JSON.stringify(merged, null, 2), 'utf8');
  return contractPath;
}

async function makeTempDir(prefix) {
  return fsp.mkdtemp(path.join(os.tmpdir(), prefix));
}

// A synchronous temp dir helper for the synchronous loadPolicy tests.
function makeTempDirSync() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'lca-sync-'));
}

// A fake MCP endpoint that records every JSON-RPC request it receives and
// answers the initialize handshake and tools/call. It never touches the real
// LCI server.
//
// The handler is a fetch-compatible function: it receives (url, options) and
// reads the JSON-RPC body from options.body and the headers from
// options.headers. It returns standards-compatible Response objects carrying
// the intended mcp-session-id and JSON/SSE bodies.
function makeFakeMcp() {
  const requests = [];
  const handler = async (url, options = {}) => {
    const body = JSON.parse(String(options.body ?? ''));
    requests.push(body);
    if (body.method === 'initialize') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        result: { protocolVersion: '2024-11-05', capabilities: {}, serverInfo: { name: 'fake', version: '0' } },
      }, { headers: { 'mcp-session-id': 'fake-session-1' } });
    }
    if (body.method === 'notifications/initialized') {
      return new Response(null, { status: 202 });
    }
    if (body.method === 'tools/call') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        result: { content: [{ type: 'text', text: `called:${body.params.name}` }] },
      });
    }
    return Response.json({ jsonrpc: '2.0', id: body.id, error: { code: -32601, message: 'unknown method' } });
  };
  return { handler, requests };
}

// A harmless, injected fake tool for runAgentLoop tests that must exercise a
// tool call without depending on external repository state (e.g. a real Git
// repo). It is added to the tool surface under a name that is not part of the
// production tool set, so it cannot collide with a real tool.
function makeFakeTool(name = 'fake_tool', result = 'fake-result') {
  return { fn: async () => result, schema: {} };
}

// Read the audit JSONL file as an array of parsed entries.
function readAuditLines(policy) {
  return fs.readFileSync(policy.audit.jsonlPath, 'utf8').trim().split('\n').map((l) => JSON.parse(l));
}

// ---------------------------------------------------------------------------
// parseArgs
// ---------------------------------------------------------------------------

test('parseArgs: success with --task and --dry-run', () => {
  const out = parseArgs(['--policy', 'p.json', '--task', 'do the thing', '--dry-run']);
  assert.equal(out.policy, 'p.json');
  assert.equal(out.task, 'do the thing');
  assert.equal(out.taskFile, null);
  assert.equal(out.dryRun, true);
  assert.equal(out.help, false);
});

test('parseArgs: success with --task-file', () => {
  const out = parseArgs(['--policy', 'p.json', '--task-file', 'task.md']);
  assert.equal(out.policy, 'p.json');
  assert.equal(out.task, null);
  assert.equal(out.taskFile, 'task.md');
  assert.equal(out.dryRun, false);
  assert.equal(out.help, false);
});

test('parseArgs: --help short-circuits required flags', () => {
  const out = parseArgs(['--help']);
  assert.equal(out.help, true);
});

test('parseArgs: -h short-circuits required flags', () => {
  const out = parseArgs(['-h']);
  assert.equal(out.help, true);
});

test('parseArgs: missing --policy throws', () => {
  assert.throws(() => parseArgs(['--task', 'x']), AgentError);
});

test('parseArgs: neither --task nor --task-file throws', () => {
  assert.throws(() => parseArgs(['--policy', 'p.json']), AgentError);
});

test('parseArgs: both --task and --task-file is a conflict', () => {
  assert.throws(
    () => parseArgs(['--policy', 'p.json', '--task', 'x', '--task-file', 't.md']),
    (e) => e instanceof AgentError && /exactly one/.test(e.message),
  );
});

test('parseArgs: duplicate --policy throws', () => {
  assert.throws(() => parseArgs(['--policy', 'a.json', '--policy', 'b.json', '--task', 'x']), AgentError);
});

test('parseArgs: duplicate --task throws', () => {
  assert.throws(() => parseArgs(['--policy', 'p.json', '--task', 'a', '--task', 'b']), AgentError);
});

test('parseArgs: duplicate --task-file throws', () => {
  assert.throws(() => parseArgs(['--policy', 'p.json', '--task-file', 'a.md', '--task-file', 'b.md']), AgentError);
});

test('parseArgs: --policy without a value throws', () => {
  assert.throws(() => parseArgs(['--policy', '--task', 'x']), AgentError);
});

test('parseArgs: --task without a value throws', () => {
  assert.throws(() => parseArgs(['--policy', 'p.json', '--task']), AgentError);
});

test('parseArgs: --task-file without a value throws', () => {
  assert.throws(() => parseArgs(['--policy', 'p.json', '--task-file']), AgentError);
});

test('parseArgs: success with --contract', () => {
  const out = parseArgs(['--policy', 'p.json', '--contract', 'task.json', '--dry-run']);
  assert.equal(out.policy, 'p.json');
  assert.equal(out.contract, 'task.json');
  assert.equal(out.task, null);
  assert.equal(out.taskFile, null);
  assert.equal(out.dryRun, true);
  assert.equal(out.help, false);
});

test('parseArgs: --contract without a value throws', () => {
  assert.throws(() => parseArgs(['--policy', 'p.json', '--contract']), AgentError);
});

test('parseArgs: duplicate --contract throws', () => {
  assert.throws(() => parseArgs(['--policy', 'p.json', '--contract', 'a.json', '--contract', 'b.json']), AgentError);
});

test('parseArgs: --contract conflicts with --task', () => {
  assert.throws(
    () => parseArgs(['--policy', 'p.json', '--contract', 't.json', '--task', 'x']),
    (e) => e instanceof AgentError && /exactly one/.test(e.message),
  );
});

test('parseArgs: --contract conflicts with --task-file', () => {
  assert.throws(
    () => parseArgs(['--policy', 'p.json', '--contract', 't.json', '--task-file', 'f.md']),
    (e) => e instanceof AgentError && /exactly one/.test(e.message),
  );
});

test('parseArgs: --help short-circuits even with --contract', () => {
  const out = parseArgs(['--help', '--policy', 'p.json', '--contract', 't.json']);
  assert.equal(out.help, true);
});

test('parseArgs: unknown argument throws with a helpful message', () => {
  assert.throws(
    () => parseArgs(['--policy', 'p.json', '--task', 'x', '--bogus']),
    (e) => e instanceof AgentError && /unknown argument: --bogus/.test(e.message),
  );
});

// ---------------------------------------------------------------------------
// globToRegExp / globMatch
// ---------------------------------------------------------------------------

test('globToRegExp: ** matches any depth', () => {
  const re = globToRegExp('src/**');
  assert.ok(re.test('src/a.rs'));
  assert.ok(re.test('src/deep/nested/a.rs'));
});

test('globToRegExp: * matches within a single segment only', () => {
  const re = globToRegExp('src/*.rs');
  assert.ok(re.test('src/a.rs'));
  assert.ok(!re.test('src/deep/a.rs'));
});

test('globToRegExp: ? matches exactly one non-slash character', () => {
  const re = globToRegExp('src/a?.rs');
  assert.ok(re.test('src/a1.rs'));
  assert.ok(!re.test('src/a12.rs'));
  assert.ok(!re.test('src/a/.rs'));
});

test('globToRegExp: literal characters are matched literally', () => {
  const re = globToRegExp('docs/guide.md');
  assert.ok(re.test('docs/guide.md'));
  assert.ok(!re.test('docs/guide.txt'));
  assert.ok(!re.test('docs/x/guide.md'));
});

test('globToRegExp: regex metacharacters in literals are escaped', () => {
  const re = globToRegExp('src/a+b(c).rs');
  assert.ok(re.test('src/a+b(c).rs'));
  assert.ok(!re.test('src/aXb(c).rs'));
});

test('globToRegExp: a backslash in the glob is normalized to a slash', () => {
  const re = globToRegExp('src\\a.rs');
  assert.ok(re.test('src/a.rs'));
  assert.ok(!re.test('src\\a.rs'));
});

// globMatch(glob, relPath): the glob is the first argument, the relative path
// the second.
test('globMatch: honors the argument order (glob first, relPath second)', () => {
  assert.ok(globMatch('src/**', 'src/a.rs'));
  assert.ok(!globMatch('src/**', 'secret/a.rs'));
});

test('globMatch: backslashes in the glob are normalized to slashes', () => {
  assert.ok(globMatch('src\\**', 'src/a.rs'));
});

// ---------------------------------------------------------------------------
// normalizeRelPath
// ---------------------------------------------------------------------------

test('normalizeRelPath: converts Windows backslashes and strips leading ./', () => {
  assert.equal(normalizeRelPath('src\\a.rs'), 'src/a.rs');
  assert.equal(normalizeRelPath('./src/a.rs'), 'src/a.rs');
});

test('normalizeRelPath: collapses repeated separators and dot segments', () => {
  assert.equal(normalizeRelPath('src//a/./b.rs'), 'src/a/b.rs');
  assert.equal(normalizeRelPath('a/./b/./c'), 'a/b/c');
});

test('normalizeRelPath: rejects absolute POSIX paths', () => {
  assert.throws(() => normalizeRelPath('/abs/a.rs'), AgentError);
});

test('normalizeRelPath: rejects absolute Windows drive paths', () => {
  assert.throws(() => normalizeRelPath('C:/x/a.rs'), AgentError);
  assert.throws(() => normalizeRelPath('C:\\x\\a.rs'), AgentError);
});

test('normalizeRelPath: rejects NUL bytes', () => {
  assert.throws(() => normalizeRelPath('src/a\0.rs'), AgentError);
});

test('normalizeRelPath: rejects .. at the start', () => {
  assert.throws(() => normalizeRelPath('../a.rs'), AgentError);
});

test('normalizeRelPath: rejects .. in the middle', () => {
  assert.throws(() => normalizeRelPath('src/../../a.rs'), AgentError);
});

test('normalizeRelPath: rejects .. at the end', () => {
  assert.throws(() => normalizeRelPath('src/a.rs/..'), AgentError);
});

test('normalizeRelPath: rejects an empty string', () => {
  assert.throws(() => normalizeRelPath(''), AgentError);
});

test('normalizeRelPath: rejects non-string input', () => {
  assert.throws(() => normalizeRelPath(null), AgentError);
  assert.throws(() => normalizeRelPath(42), AgentError);
  assert.throws(() => normalizeRelPath(undefined), AgentError);
});

// ---------------------------------------------------------------------------
// loadPolicy
// ---------------------------------------------------------------------------

test('loadPolicy: resolves a valid policy', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot);
    const policy = loadPolicy(policyPath);
    assert.equal(policy.schema, 'local-agent.policy/v1');
    assert.ok(policy.repoRoot.length > 0);
    assert.ok(Array.isArray(policy.readRe));
    assert.ok(Array.isArray(policy.writeRe));
    assert.ok(Array.isArray(policy.denyRe));
    assert.equal(policy.limits.maxTurns, 24);
    assert.equal(policy.checks.suites.chunk, 'Chunk');
    assert.ok(policy.audit.jsonlPath.startsWith(policy.repoRoot));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing file', () => {
  assert.throws(() => loadPolicy(path.join(os.tmpdir(), 'does-not-exist-policy.json')), AgentError);
});

test('loadPolicy: rejects invalid JSON', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = path.join(repoRoot, 'policy.json');
    fs.writeFileSync(policyPath, '{ not json', 'utf8');
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /not valid JSON/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a wrong schema', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { schema: 'wrong/v0' });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /unsupported policy schema/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing repositoryRoot', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { repositoryRoot: null });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a repositoryRoot that is not a directory', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { repositoryRoot: 'no-such-dir' });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /repositoryRoot/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a non-array read glob list', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { read: 'src/**' });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a write glob list containing an empty string', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { write: ['src/**', ''] });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a deny glob list containing a non-string', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { deny: [42] });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing model.name', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: '', endpoint: 'http://127.0.0.1:8765/v1', apiKeyEnv: 'QWEN_API_KEY' },
    });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing model.endpoint', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: '', apiKeyEnv: 'QWEN_API_KEY' },
    });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing model.apiKeyEnv', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: 'http://127.0.0.1:8765/v1', apiKeyEnv: '' },
    });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing lci.endpoint', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { lci: { endpoint: '' } });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects limits.maxTurns that is zero', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, {
      limits: { maxTurns: 0, maxReadLines: 400, maxPatchBytes: 24576, maxToolResultBytes: 16384 },
    });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /maxTurns/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects limits.maxReadLines that is negative', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, {
      limits: { maxTurns: 24, maxReadLines: -1, maxPatchBytes: 24576, maxToolResultBytes: 16384 },
    });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /maxReadLines/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects limits.maxPatchBytes that is not an integer', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, {
      limits: { maxTurns: 24, maxReadLines: 400, maxPatchBytes: 1.5, maxToolResultBytes: 16384 },
    });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /maxPatchBytes/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects limits.maxToolResultBytes that is a string', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, {
      limits: { maxTurns: 24, maxReadLines: 400, maxPatchBytes: 24576, maxToolResultBytes: '16384' },
    });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /maxToolResultBytes/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing checks.script', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { checks: { script: '', suites: { chunk: 'Chunk' } } });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects checks.suites that is not an object', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { checks: { script: 'scripts/Test.ps1', suites: 'nope' } });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /checks.suites/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a checks.suites entry that is not a non-empty string', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { checks: { script: 'scripts/Test.ps1', suites: { chunk: '' } } });
    assert.throws(() => loadPolicy(policyPath), (e) => e instanceof AgentError && /checks.suites.chunk/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: rejects a missing audit.jsonlPath', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policyPath = writePolicyFile(repoRoot, { audit: { jsonlPath: '' } });
    assert.throws(() => loadPolicy(policyPath), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// loadTaskContract / contractTaskText
// ---------------------------------------------------------------------------

test('loadTaskContract: resolves a valid contract and builds compact labeled task text', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const contractPath = writeContractFile(repoRoot);
    const contract = loadTaskContract(contractPath, policy);
    assert.equal(contract.schema, 'local-agent.task/v1');
    assert.equal(contract.id, 'task-1');
    assert.deepEqual(contract.allowedFiles, ['src/a.rs']);
    assert.deepEqual(contract.checkSuites, ['chunk']);
    const text = contractTaskText(contract);
    assert.ok(text.includes('Task contract: task-1'));
    assert.ok(text.includes('Objective: SECRET_OBJECTIVE do the thing'));
    assert.ok(text.includes('  - src/a.rs'));
    assert.ok(text.includes('  - SECRET_ACCEPTANCE criterion'));
    assert.ok(text.includes('  - SECRET_PROHIBITED action'));
    assert.ok(text.includes('Allowed check suites: chunk'));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects a missing file', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    assert.throws(
      () => loadTaskContract(path.join(repoRoot, 'nope.json'), policy),
      (e) => e instanceof AgentError && /contract file not found/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects invalid JSON', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const contractPath = path.join(repoRoot, 'task.json');
    fs.writeFileSync(contractPath, '{ not json', 'utf8');
    assert.throws(
      () => loadTaskContract(contractPath, policy),
      (e) => e instanceof AgentError && /not valid JSON/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects a wrong schema', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const contractPath = writeContractFile(repoRoot, { schema: 'wrong/v0' });
    assert.throws(
      () => loadTaskContract(contractPath, policy),
      (e) => e instanceof AgentError && /unsupported contract schema/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects an invalid or empty id', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const badId = writeContractFile(repoRoot, { id: 'bad id!' });
    assert.throws(() => loadTaskContract(badId, policy), (e) => e instanceof AgentError && /contract.id/.test(e.message));
    const emptyId = writeContractFile(repoRoot, { id: '' });
    assert.throws(() => loadTaskContract(emptyId, policy), (e) => e instanceof AgentError && /contract.id/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects an empty objective', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const contractPath = writeContractFile(repoRoot, { objective: '   ' });
    assert.throws(
      () => loadTaskContract(contractPath, policy),
      (e) => e instanceof AgentError && /contract.objective/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects an empty allowedFiles list', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const contractPath = writeContractFile(repoRoot, { allowedFiles: [] });
    assert.throws(
      () => loadTaskContract(contractPath, policy),
      (e) => e instanceof AgentError && /allowedFiles/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects duplicate allowedFiles after normalization', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const contractPath = writeContractFile(repoRoot, { allowedFiles: ['src/a.rs', './src/a.rs'] });
    assert.throws(
      () => loadTaskContract(contractPath, policy),
      (e) => e instanceof AgentError && /duplicate after normalization/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects absolute and traversal allowedFiles paths', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['**'], write: ['**'] });
    const absPath = writeContractFile(repoRoot, { allowedFiles: ['/etc/passwd'] });
    assert.throws(() => loadTaskContract(absPath, policy), (e) => e instanceof AgentError && /absolute/.test(e.message));
    const traversal = writeContractFile(repoRoot, { allowedFiles: ['src/../../a.rs'] });
    assert.throws(() => loadTaskContract(traversal, policy), (e) => e instanceof AgentError && /traversal/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects an allowedFile denied or unwritable by the base policy', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['**'], write: ['src/**'], deny: ['secret/**'] });
    const denied = writeContractFile(repoRoot, { allowedFiles: ['secret/a.rs'] });
    assert.throws(
      () => loadTaskContract(denied, policy),
      (e) => e instanceof AgentError && /denied by policy/.test(e.message),
    );
    const unwritable = writeContractFile(repoRoot, { allowedFiles: ['other/a.rs'] });
    assert.throws(
      () => loadTaskContract(unwritable, policy),
      (e) => e instanceof AgentError && /not writable/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects empty or duplicate acceptance entries', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const empty = writeContractFile(repoRoot, { acceptance: [] });
    assert.throws(() => loadTaskContract(empty, policy), (e) => e instanceof AgentError && /acceptance/.test(e.message));
    const blank = writeContractFile(repoRoot, { acceptance: [''] });
    assert.throws(() => loadTaskContract(blank, policy), (e) => e instanceof AgentError && /acceptance/.test(e.message));
    const dup = writeContractFile(repoRoot, { acceptance: ['a', 'a'] });
    assert.throws(() => loadTaskContract(dup, policy), (e) => e instanceof AgentError && /duplicate/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects invalid or duplicate prohibited entries', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const invalid = writeContractFile(repoRoot, { prohibited: ['ok', ''] });
    assert.throws(() => loadTaskContract(invalid, policy), (e) => e instanceof AgentError && /prohibited/.test(e.message));
    const dup = writeContractFile(repoRoot, { prohibited: ['x', 'x'] });
    assert.throws(() => loadTaskContract(dup, policy), (e) => e instanceof AgentError && /duplicate/.test(e.message));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('loadTaskContract: rejects invalid, duplicate, or unknown checkSuites', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const invalid = writeContractFile(repoRoot, { checkSuites: ['chunk', ''] });
    assert.throws(() => loadTaskContract(invalid, policy), (e) => e instanceof AgentError && /checkSuites/.test(e.message));
    const dup = writeContractFile(repoRoot, { checkSuites: ['chunk', 'chunk'] });
    assert.throws(() => loadTaskContract(dup, policy), (e) => e instanceof AgentError && /duplicate/.test(e.message));
    const unknown = writeContractFile(repoRoot, { checkSuites: ['nope'] });
    assert.throws(
      () => loadTaskContract(unknown, policy),
      (e) => e instanceof AgentError && /unknown suite/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// checkAccess / resolveRepoPath
// ---------------------------------------------------------------------------

test('checkAccess: read allowed by glob', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'] });
    assert.doesNotThrow(() => checkAccess(policy, 'src/a.rs', 'read'));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('checkAccess: write allowed by glob', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, write: ['src/**'] });
    assert.doesNotThrow(() => checkAccess(policy, 'src/a.rs', 'write'));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('checkAccess: default-deny when no read glob matches', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'] });
    assert.throws(() => checkAccess(policy, 'other/a.rs', 'read'), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('checkAccess: default-deny when no write glob matches', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, write: ['src/**'] });
    assert.throws(() => checkAccess(policy, 'other/a.rs', 'write'), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('checkAccess: deny beats read', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['**'], deny: ['secret/**'] });
    assert.throws(() => checkAccess(policy, 'secret/a.rs', 'read'), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('checkAccess: deny beats write', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, write: ['**'], deny: ['secret/**'] });
    assert.throws(() => checkAccess(policy, 'secret/a.rs', 'write'), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('resolveRepoPath: resolves inside the root', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'] });
    const { rel, abs } = resolveRepoPath(policy, 'src/a.rs');
    assert.equal(rel, 'src/a.rs');
    assert.ok(abs.startsWith(repoRoot));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('resolveRepoPath: rejects escape via ..', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['**'] });
    assert.throws(() => resolveRepoPath(policy, '../a.rs'), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('resolveRepoPath: rejects absolute paths', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['**'] });
    assert.throws(() => resolveRepoPath(policy, '/etc/passwd'), AgentError);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('resolveRepoPath: rejects an existing symlink that escapes the root', async () => {
  const repoRoot = await makeTempDir('lca-symlink-');
  const outside = await makeTempDir('lca-symlink-outside-');
  try {
    const policy = makePolicy({ repoRoot, read: ['**'] });
    const target = path.join(outside, 'outside.txt');
    fs.writeFileSync(target, 'outside', 'utf8');
    const linkPath = path.join(repoRoot, 'link.txt');
    let linkCreated = false;
    try {
      fs.symlinkSync(target, linkPath);
      linkCreated = true;
    } catch {
      // Symlink creation denied on this platform/OS; skip the escape check.
      return;
    }
    if (!linkCreated) return;
    assert.throws(
      () => resolveRepoPath(policy, 'link.txt'),
      (e) => e instanceof AgentError && /symlink|junction/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
    fs.rmSync(outside, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// boundOutput
// ---------------------------------------------------------------------------

test('boundOutput: passes through short strings', () => {
  assert.equal(boundOutput('hello', 100), 'hello');
});

test('boundOutput: truncates long strings with a marker', () => {
  const out = boundOutput('x'.repeat(500), 100);
  assert.ok(out.length < 500);
  assert.ok(out.includes('truncated'));
});

test('boundOutput: truncation never splits a UTF-8 code point', () => {
  // 3-byte UTF-8 character; a 100-byte budget that lands mid-code-point must
  // back off to the previous boundary instead of emitting invalid UTF-8.
  const text = 'é'.repeat(200); // 600 bytes
  const out = boundOutput(text, 100);
  assert.ok(out.includes('truncated'));
  // The truncated prefix must be valid UTF-8 (decoding it back must not
  // produce replacement characters).
  const prefix = out.split('\n[truncated')[0];
  assert.ok(!prefix.includes('\uFFFD'));
  assert.ok(Buffer.byteLength(prefix, 'utf8') <= 100);
});

test('boundOutput: reports the number of remaining bytes', () => {
  const out = boundOutput('x'.repeat(500), 100);
  assert.ok(/\[truncated: 400 more bytes\]/.test(out));
});

// ---------------------------------------------------------------------------
// parseSse
// ---------------------------------------------------------------------------

test('parseSse: returns the last JSON-RPC message (LF)', () => {
  const sse = 'data: {"jsonrpc":"2.0","id":1,"result":"a"}\n\ndata: {"jsonrpc":"2.0","id":2,"result":"b"}\n\n';
  const last = parseSse(sse);
  assert.equal(last.id, 2);
  assert.equal(last.result, 'b');
});

test('parseSse: handles CRLF line endings', () => {
  const sse = 'data: {"jsonrpc":"2.0","id":1,"result":"a"}\r\n\r\ndata: {"jsonrpc":"2.0","id":2,"result":"b"}\r\n\r\n';
  const last = parseSse(sse);
  assert.equal(last.id, 2);
  assert.equal(last.result, 'b');
});

test('parseSse: multiple events return the last valid one', () => {
  const sse = [
    'data: {"jsonrpc":"2.0","id":1,"result":"a"}',
    '',
    'data: {"jsonrpc":"2.0","id":2,"result":"b"}',
    '',
    'data: {"jsonrpc":"2.0","id":3,"result":"c"}',
    '',
  ].join('\n');
  const last = parseSse(sse);
  assert.equal(last.id, 3);
});

test('parseSse: ignores malformed events', () => {
  const sse = 'data: not-json\n\ndata: {"jsonrpc":"2.0","id":3,"result":"c"}\n\n';
  const last = parseSse(sse);
  assert.equal(last.id, 3);
});

test('parseSse: returns null for an empty body', () => {
  assert.equal(parseSse(''), null);
});

test('parseSse: returns null when only malformed events are present', () => {
  assert.equal(parseSse('data: nope\n\ndata: also-nope\n\n'), null);
});

// ---------------------------------------------------------------------------
// buildTools (offline; no MCP contact)
// ---------------------------------------------------------------------------

test('buildTools: exposes the full tool surface', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    for (const name of ['list_files', 'read_file', 'apply_patch', 'git_diff', 'run_check', 'lci_index_status', 'lci_search_code']) {
      assert.ok(tools[name], `missing tool ${name}`);
      assert.equal(typeof tools[name].fn, 'function');
    }
    assert.equal(typeof tools._setMcp, 'function');
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('buildTools: lci tools fail cleanly before MCP is injected', async () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => tools.lci_index_status.fn({}),
      (e) => e instanceof AgentError && /MCP client not initialized/.test(e.message),
    );
    await assert.rejects(
      () => tools.lci_search_code.fn({ query: 'x' }),
      (e) => e instanceof AgentError && /MCP client not initialized/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

// lci_search_code validates the query BEFORE requiring the MCP client, so an
// empty query is reported as a query error even when no client is injected.
test('buildTools: lci_search_code validates an empty query before MCP injection', async () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    // No MCP client is injected here: the query validation must fire first.
    await assert.rejects(
      () => tools.lci_search_code.fn({ query: '' }),
      (e) => e instanceof AgentError && /non-empty query/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// apply_patch tool
//
// Production interface: { path, oldText, newText, expectedReplacements }.
// oldText must occur exactly expectedReplacements times (default 1); every
// occurrence is replaced with newText (an empty newText removes the text).
// ---------------------------------------------------------------------------

test('apply_patch: replaces the exact text once and reports the count', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const file = path.join(repoRoot, 'src', 'a.rs');
    fs.writeFileSync(file, 'line1\nREMOVE_ME\nline3\n', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    const out = await tools.apply_patch.fn({ path: 'src/a.rs', oldText: 'REMOVE_ME', newText: '' });
    assert.equal(out, 'applied 1 replacement(s) to src/a.rs');
    assert.equal(fs.readFileSync(file, 'utf8'), 'line1\n\nline3\n');
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('apply_patch: a count mismatch leaves the file untouched', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const file = path.join(repoRoot, 'src', 'a.rs');
    const original = 'x\nREMOVE_ME\nREMOVE_ME\ny\n';
    fs.writeFileSync(file, original, 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => tools.apply_patch.fn({ path: 'src/a.rs', oldText: 'REMOVE_ME', newText: '', expectedReplacements: 1 }),
      (e) => e instanceof AgentError && e.code === 'patch_mismatch',
    );
    assert.equal(fs.readFileSync(file, 'utf8'), original, 'file must be unchanged on mismatch');
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('apply_patch: replaces every occurrence when expectedReplacements matches', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const file = path.join(repoRoot, 'src', 'a.rs');
    fs.writeFileSync(file, 'A\nREMOVE_ME\nB\nREMOVE_ME\nC\n', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    const out = await tools.apply_patch.fn({ path: 'src/a.rs', oldText: 'REMOVE_ME', newText: '', expectedReplacements: 2 });
    assert.equal(out, 'applied 2 replacement(s) to src/a.rs');
    assert.equal(fs.readFileSync(file, 'utf8'), 'A\n\nB\n\nC\n');
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('apply_patch: rejects a patch larger than maxPatchBytes', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const file = path.join(repoRoot, 'src', 'a.rs');
    fs.writeFileSync(file, 'x', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    policy.limits.maxPatchBytes = 10;
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => tools.apply_patch.fn({ path: 'src/a.rs', oldText: 'x', newText: 'y'.repeat(11) }),
      (e) => e instanceof AgentError && e.code === 'patch_too_large',
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('apply_patch: writes atomically and leaves no temp files behind', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const file = path.join(repoRoot, 'src', 'a.rs');
    fs.writeFileSync(file, 'before\nMARK\nafter\n', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await tools.apply_patch.fn({ path: 'src/a.rs', oldText: 'MARK', newText: '' });
    const dirEntries = fs.readdirSync(path.join(repoRoot, 'src'));
    assert.deepEqual(dirEntries, ['a.rs'], 'no temp file may remain in the target directory');
    assert.equal(fs.readFileSync(file, 'utf8'), 'before\n\nafter\n');
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('apply_patch: rejects a path that is not writable per policy', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'other'), { recursive: true });
    fs.writeFileSync(path.join(repoRoot, 'other', 'a.rs'), 'x', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => tools.apply_patch.fn({ path: 'other/a.rs', oldText: 'x', newText: 'y' }),
      (e) => e instanceof AgentError && /not writable/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// buildTools with an active task contract
// ---------------------------------------------------------------------------

test('buildTools contract: apply_patch succeeds on an allowed file and rejects a different policy-writable file without mutation', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const allowed = path.join(repoRoot, 'src', 'a.rs');
    const other = path.join(repoRoot, 'src', 'b.rs');
    fs.writeFileSync(allowed, 'MARK\n', 'utf8');
    fs.writeFileSync(other, 'MARK\n', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy, { contractId: 'task-1' });
    const contract = {
      schema: 'local-agent.task/v1',
      id: 'task-1',
      objective: 'x',
      allowedFiles: ['src/a.rs'],
      acceptance: ['a'],
      prohibited: [],
      checkSuites: ['chunk'],
    };
    const tools = buildTools(policy, audit, { dryRun: false, contract });
    const out = await tools.apply_patch.fn({ path: 'src/a.rs', oldText: 'MARK', newText: 'DONE' });
    assert.equal(out, 'applied 1 replacement(s) to src/a.rs');
    assert.equal(fs.readFileSync(allowed, 'utf8'), 'DONE\n');
    await assert.rejects(
      () => tools.apply_patch.fn({ path: 'src/b.rs', oldText: 'MARK', newText: 'DONE' }),
      (e) => e instanceof AgentError && e.code === 'contract_path_denied',
    );
    assert.equal(fs.readFileSync(other, 'utf8'), 'MARK\n', 'the non-allowed file must be unchanged');
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('buildTools contract: a listed check suite reaches the configured script check; an unlisted otherwise-valid suite is rejected before spawn', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'scripts'), { recursive: true });
    // Harmless temporary fixture: the fixed runner is `powershell -File` on
    // Windows and `pwsh -File` elsewhere; neither binary is guaranteed to
    // exist, so the listed-suite case is expected to fail at spawn (proof it
    // passed the contract gate), while the unlisted suite must fail earlier
    // with the contract denial.
    const script = path.join(repoRoot, 'scripts', 'Test.ps1');
    fs.writeFileSync(script, 'throw "intentional test failure"', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    // Add a second policy suite that the contract does NOT list.
    policy.checks.suites.full = 'Full';
    const audit = new AuditLog(policy, { contractId: 'task-1' });
    const contract = {
      schema: 'local-agent.task/v1',
      id: 'task-1',
      objective: 'x',
      allowedFiles: ['src/a.rs'],
      acceptance: ['a'],
      prohibited: [],
      checkSuites: ['chunk'],
    };
    const tools = buildTools(policy, audit, { dryRun: false, contract });
    // Listed suite: accepted by the contract gate, so it reaches the
    // configured fixed runner (spawn) and fails there, not at the gate.
    await assert.rejects(
      () => tools.run_check.fn({ suite: 'chunk' }),
      (e) => e instanceof AgentError && e.code !== 'contract_suite_denied' && e.code !== 'bad_suite',
    );
    // Unlisted but policy-valid suite: rejected before any process is spawned.
    await assert.rejects(
      () => tools.run_check.fn({ suite: 'full' }),
      (e) => e instanceof AgentError && e.code === 'contract_suite_denied',
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Dry-run and check suites
// ---------------------------------------------------------------------------

test('dry-run: apply_patch is blocked', async () => {
  const repoRoot = makeTempDirSync();
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    fs.writeFileSync(path.join(repoRoot, 'src', 'a.rs'), 'x', 'utf8');
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: true });
    await assert.rejects(
      () => tools.apply_patch.fn({ path: 'src/a.rs', oldText: 'x', newText: 'y' }),
      (e) => e instanceof AgentError && e.code === 'dry_run_blocked',
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('dry-run: run_check is blocked', async () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: true });
    await assert.rejects(
      () => tools.run_check.fn({ suite: 'chunk' }),
      (e) => e instanceof AgentError && e.code === 'dry_run_blocked',
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('run_check: an unknown suite is rejected before any process is spawned', async () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => tools.run_check.fn({ suite: 'nope' }),
      (e) => e instanceof AgentError && e.code === 'bad_suite' && /unknown check suite/.test(e.message),
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('main: --dry-run prints the report and never contacts the model', async () => {
  const repoRoot = await makeTempDir('lca-dryrun-');
  const realFetch = globalThis.fetch;
  const realStdout = process.stdout.write;
  let modelContacted = false;
  let captured = '';
  process.stdout.write = (chunk) => { captured += String(chunk); return true; };
  globalThis.fetch = async () => {
    modelContacted = true;
    return Response.json({ choices: [{ message: { content: 'nope' } }] });
  };
  try {
    const policyPath = writePolicyFile(repoRoot);
    const code = await main(['--policy', policyPath, '--task', 'do the thing', '--dry-run']);
    assert.equal(code, 0);
    assert.ok(captured.includes('Dry-run: policy resolved and task validated'));
    assert.ok(captured.includes('apply_patch (blocked in dry-run)'));
    assert.ok(captured.includes('run_check (blocked in dry-run)'));
    assert.equal(modelContacted, false, 'dry-run must not contact the model');
  } finally {
    process.stdout.write = realStdout;
    globalThis.fetch = realFetch;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// AuditLog (metadata only)
// ---------------------------------------------------------------------------

test('AuditLog: records tool and model turns as metadata', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    audit.toolCall({ tool: 'read_file', ok: true, error: null, inputBytes: 10, outputBytes: 20, durationMs: 1 });
    audit.modelTurn({ turn: 1, ok: true, error: null, inputBytes: 30, outputBytes: 40, durationMs: 2 });
    const lines = readAuditLines(policy);
    assert.equal(lines.length, 2);
    assert.equal(lines[0].kind, 'tool');
    assert.equal(lines[0].tool, 'read_file');
    assert.equal(lines[0].ok, true);
    assert.equal(lines[1].kind, 'model_turn');
    assert.equal(lines[1].turn, 1);
    assert.equal(lines[1].ok, true);
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('AuditLog: never records secret content, only metadata fields', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    audit.toolCall({ tool: 'apply_patch', ok: false, error: 'patch_mismatch', inputBytes: 5, outputBytes: 7, durationMs: 3 });
    const raw = fs.readFileSync(policy.audit.jsonlPath, 'utf8');
    const line = JSON.parse(raw.trim());
    assert.deepEqual(
      Object.keys(line).sort(),
      ['contractId', 'durationMs', 'error', 'inputBytes', 'kind', 'ok', 'outputBytes', 'tool', 'ts'].sort(),
    );
    assert.equal(line.error, 'patch_mismatch');
    // No contract is active here, so the id is null (never a contract body).
    assert.equal(line.contractId, null);
    // The audit line must not carry any free-form content fields.
    assert.ok(!('content' in line));
    assert.ok(!('task' in line));
    assert.ok(!('patch' in line));
    assert.ok(!('source' in line));
    assert.ok(!('model' in line));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('AuditLog: an active contract records contractId on model and tool records and never records contract contents', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy, { contractId: 'task-1' });
    audit.toolCall({ tool: 'apply_patch', ok: true, error: null, inputBytes: 5, outputBytes: 7, durationMs: 3 });
    audit.modelTurn({ turn: 1, ok: true, error: null, inputBytes: 30, outputBytes: 40, durationMs: 2 });
    const raw = fs.readFileSync(policy.audit.jsonlPath, 'utf8');
    const lines = raw.trim().split('\n').map((l) => JSON.parse(l));
    assert.equal(lines.length, 2);
    assert.equal(lines[0].contractId, 'task-1');
    assert.equal(lines[1].contractId, 'task-1');
    // Only the short identifier is recorded: no contract text of any kind.
    assert.ok(!raw.includes('SECRET_OBJECTIVE'));
    assert.ok(!raw.includes('SECRET_ACCEPTANCE'));
    assert.ok(!raw.includes('SECRET_PROHIBITED'));
    assert.ok(!raw.includes('objective'));
    assert.ok(!raw.includes('acceptance'));
    assert.ok(!raw.includes('prohibited'));
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

test('AuditLog: a failed audit write is fatal with code audit_failure', () => {
  const repoRoot = makeTempDirSync();
  try {
    const policy = makePolicy({ repoRoot, read: ['src/**'], write: ['src/**'] });
    const audit = new AuditLog(policy);
    // Make the audit path unwritable by replacing the file with a directory.
    fs.rmSync(policy.audit.jsonlPath, { force: true });
    fs.mkdirSync(policy.audit.jsonlPath);
    assert.throws(
      () => audit.toolCall({ tool: 'read_file', ok: true, error: null, inputBytes: 1, outputBytes: 1, durationMs: 1 }),
      (e) => e instanceof AgentError && e.code === 'audit_failure',
    );
  } finally {
    fs.rmSync(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// runAgentLoop (offline; scripted model, no MCP contact)
//
// Tool-call tests inject a harmless fake tool rather than relying on git_diff
// inside a non-Git temp directory, so they never depend on external repo
// state and never invoke git.
// ---------------------------------------------------------------------------

test('runAgentLoop: returns the final assistant text with no tool calls', async () => {
  const repoRoot = await makeTempDir('lca-loop-final-');
  const realFetch = globalThis.fetch;
  const modelEndpoint = 'http://127.0.0.1:0/v1';
  globalThis.fetch = async () => Response.json({ choices: [{ message: { content: 'all done' } }] });
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: modelEndpoint, apiKeyEnv: 'QWEN_API_KEY' },
    });
    const policy = loadPolicy(policyPath);
    process.env.QWEN_API_KEY = 'test-key';
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    const finalText = await runAgentLoop({ policy, audit, tools, task: 'hi', systemPrompt: 'test' });
    assert.equal(finalText, 'all done');
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.QWEN_API_KEY;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('runAgentLoop: executes a harmless fake tool call then returns the final text', async () => {
  const repoRoot = await makeTempDir('lca-loop-tool-');
  const realFetch = globalThis.fetch;
  const modelEndpoint = 'http://127.0.0.1:0/v1';
  const calls = [];
  globalThis.fetch = async () => {
    calls.push(1);
    if (calls.length === 1) {
      return Response.json({
        choices: [{
          message: {
            content: null,
            tool_calls: [{
              id: 'call_1',
              type: 'function',
              function: { name: 'fake_tool', arguments: '{}' },
            }],
          },
        }],
      });
    }
    return Response.json({ choices: [{ message: { content: 'finished' } }] });
  };
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: modelEndpoint, apiKeyEnv: 'QWEN_API_KEY' },
    });
    const policy = loadPolicy(policyPath);
    process.env.QWEN_API_KEY = 'test-key';
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    // Inject a harmless fake tool so the loop exercises a real tool call
    // without depending on a Git repository.
    tools.fake_tool = makeFakeTool('fake_tool', 'fake-result');
    const finalText = await runAgentLoop({ policy, audit, tools, task: 'diff', systemPrompt: 'test' });
    assert.equal(finalText, 'finished');
    assert.equal(calls.length, 2);
    const lines = readAuditLines(policy);
    const toolLine = lines.find((l) => l.kind === 'tool' && l.tool === 'fake_tool');
    assert.ok(toolLine, 'fake tool call was not audited');
    assert.equal(toolLine.ok, true);
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.QWEN_API_KEY;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('runAgentLoop: a fatal tool error stops the loop and is audited', async () => {
  const repoRoot = await makeTempDir('lca-loop-fatal-');
  const realFetch = globalThis.fetch;
  const modelEndpoint = 'http://127.0.0.1:0/v1';
  globalThis.fetch = async () => Response.json({
    choices: [{
      message: {
        content: null,
        tool_calls: [{
          id: 'call_1',
          type: 'function',
          function: { name: 'read_file', arguments: '{"path":"../escape.rs"}' },
        }],
      },
    }],
  });
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: modelEndpoint, apiKeyEnv: 'QWEN_API_KEY' },
    });
    const policy = loadPolicy(policyPath);
    process.env.QWEN_API_KEY = 'test-key';
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => runAgentLoop({ policy, audit, tools, task: 'read', systemPrompt: 'test' }),
      AgentError,
    );
    const lines = readAuditLines(policy);
    const toolLine = lines.find((l) => l.kind === 'tool' && l.tool === 'read_file');
    assert.ok(toolLine, 'tool call was not audited');
    assert.equal(toolLine.ok, false);
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.QWEN_API_KEY;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// An unknown tool is fatal (AgentError.fatal defaults to true), so the loop
// must reject with code unknown_tool rather than recover via the model.
test('runAgentLoop: an unknown tool name is fatal with code unknown_tool', async () => {
  const repoRoot = await makeTempDir('lca-loop-unknown-');
  const realFetch = globalThis.fetch;
  const modelEndpoint = 'http://127.0.0.1:0/v1';
  globalThis.fetch = async () => Response.json({
    choices: [{
      message: {
        content: null,
        tool_calls: [{
          id: 'call_1',
          type: 'function',
          function: { name: 'no_such_tool', arguments: '{}' },
        }],
      },
    }],
  });
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: modelEndpoint, apiKeyEnv: 'QWEN_API_KEY' },
    });
    const policy = loadPolicy(policyPath);
    process.env.QWEN_API_KEY = 'test-key';
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => runAgentLoop({ policy, audit, tools, task: 'x', systemPrompt: 'test' }),
      (e) => e instanceof AgentError && e.code === 'unknown_tool',
    );
    const lines = readAuditLines(policy);
    const toolLine = lines.find((l) => l.kind === 'tool' && l.tool === 'no_such_tool');
    assert.ok(toolLine, 'unknown tool call was not audited');
    assert.equal(toolLine.ok, false);
    assert.equal(toolLine.error, 'unknown_tool');
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.QWEN_API_KEY;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// maxTurns is exercised with a harmless fake tool (not git_diff) so the loop
// keeps calling a tool until it exceeds the turn budget.
test('runAgentLoop: throws when maxTurns is exceeded', async () => {
  const repoRoot = await makeTempDir('lca-loop-maxturns-');
  const realFetch = globalThis.fetch;
  const modelEndpoint = 'http://127.0.0.1:0/v1';
  globalThis.fetch = async () => Response.json({
    choices: [{
      message: {
        content: null,
        tool_calls: [{
          id: 'call_1',
          type: 'function',
          function: { name: 'fake_tool', arguments: '{}' },
        }],
      },
    }],
  });
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: modelEndpoint, apiKeyEnv: 'QWEN_API_KEY' },
      limits: { maxTurns: 2, maxReadLines: 400, maxPatchBytes: 24576, maxToolResultBytes: 16384 },
    });
    const policy = loadPolicy(policyPath);
    process.env.QWEN_API_KEY = 'test-key';
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    tools.fake_tool = makeFakeTool('fake_tool', 'fake-result');
    await assert.rejects(
      () => runAgentLoop({ policy, audit, tools, task: 'x', systemPrompt: 'test' }),
      (e) => e instanceof AgentError && e.code === 'max_turns',
    );
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.QWEN_API_KEY;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('runAgentLoop: fails with no_api_key when the key env var is unset', async () => {
  const repoRoot = await makeTempDir('lca-loop-nokey-');
  const realFetch = globalThis.fetch;
  const modelEndpoint = 'http://127.0.0.1:0/v1';
  globalThis.fetch = async () => Response.json({ choices: [{ message: { content: 'x' } }] });
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: modelEndpoint, apiKeyEnv: 'QWEN_API_KEY' },
    });
    const policy = loadPolicy(policyPath);
    delete process.env.QWEN_API_KEY;
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    await assert.rejects(
      () => runAgentLoop({ policy, audit, tools, task: 'x', systemPrompt: 'test' }),
      (e) => e instanceof AgentError && e.code === 'no_api_key',
    );
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.QWEN_API_KEY;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('runAgentLoop: audits model turns and tool calls with metadata only', async () => {
  const repoRoot = await makeTempDir('lca-loop-audit-');
  const realFetch = globalThis.fetch;
  const modelEndpoint = 'http://127.0.0.1:0/v1';
  const calls = [];
  globalThis.fetch = async () => {
    calls.push(1);
    if (calls.length === 1) {
      return Response.json({
        choices: [{
          message: {
            content: null,
            tool_calls: [{
              id: 'call_1',
              type: 'function',
              function: { name: 'fake_tool', arguments: '{}' },
            }],
          },
        }],
      });
    }
    return Response.json({ choices: [{ message: { content: 'done' } }] });
  };
  try {
    const policyPath = writePolicyFile(repoRoot, {
      model: { name: 'qwen3.8-27b', endpoint: modelEndpoint, apiKeyEnv: 'QWEN_API_KEY' },
    });
    const policy = loadPolicy(policyPath);
    process.env.QWEN_API_KEY = 'test-key';
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    tools.fake_tool = makeFakeTool('fake_tool', 'fake-result');
    await runAgentLoop({ policy, audit, tools, task: 'x', systemPrompt: 'test' });
    const lines = readAuditLines(policy);
    const modelTurns = lines.filter((l) => l.kind === 'model_turn');
    const toolLines = lines.filter((l) => l.kind === 'tool');
    assert.equal(modelTurns.length, 2, 'both model turns must be audited');
    assert.ok(modelTurns.every((l) => l.ok === true));
    assert.equal(toolLines.length, 1);
    assert.equal(toolLines[0].tool, 'fake_tool');
    // Metadata only: no free-form content fields anywhere.
    for (const l of lines) {
      assert.ok(!('content' in l));
      assert.ok(!('task' in l));
    }
  } finally {
    globalThis.fetch = realFetch;
    delete process.env.QWEN_API_KEY;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// McpClient (offline; scripted fetch, no real MCP contact)
// ---------------------------------------------------------------------------

test('McpClient: initialize performs the handshake and stores the session id', async () => {
  const realFetch = globalThis.fetch;
  const { handler, requests } = makeFakeMcp();
  globalThis.fetch = handler;
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await client.initialize();
    assert.equal(client.initialized, true);
    assert.equal(client.sessionId, 'fake-session-1');
    assert.equal(requests.length, 2);
    assert.equal(requests[0].method, 'initialize');
    assert.equal(requests[1].method, 'notifications/initialized');
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('McpClient: callTool sends a tools/call request and returns the text', async () => {
  const realFetch = globalThis.fetch;
  const { handler, requests } = makeFakeMcp();
  globalThis.fetch = handler;
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await client.initialize();
    const text = await client.callTool('index_status', { workspace_path: '/tmp/x' });
    assert.equal(text, 'called:index_status');
    const call = requests.find((r) => r.method === 'tools/call');
    assert.ok(call, 'tools/call request was not sent');
    assert.equal(call.params.name, 'index_status');
    assert.deepEqual(call.params.arguments, { workspace_path: '/tmp/x' });
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('McpClient: close resets the session state', async () => {
  const realFetch = globalThis.fetch;
  const { handler } = makeFakeMcp();
  globalThis.fetch = handler;
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await client.initialize();
    assert.equal(client.initialized, true);
    assert.equal(client.sessionId, 'fake-session-1');
    await client.close();
    assert.equal(client.initialized, false);
    assert.equal(client.sessionId, null);
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('McpClient: a JSON-RPC error object in a 200 response is an mcp_error', async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({
    jsonrpc: '2.0',
    id: 1,
    error: { code: -32601, message: 'boom' },
  });
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await assert.rejects(
      () => client.initialize(),
      (e) => e instanceof AgentError && e.code === 'mcp_error' && e.message === 'MCP request failed: boom',
    );
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('McpClient: a non-OK plain-text body is an mcp_error with the exact message', async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response('boom', { status: 500, headers: { 'Content-Type': 'text/plain' } });
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await assert.rejects(
      () => client.initialize(),
      (e) => e instanceof AgentError && e.code === 'mcp_error' && e.message === 'MCP request failed: boom',
    );
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('McpClient: a non-OK JSON body uses the error message field', async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => Response.json({ error: { message: 'json boom' } }, { status: 502 });
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await assert.rejects(
      () => client.initialize(),
      (e) => e instanceof AgentError && e.code === 'mcp_error' && e.message === 'MCP request failed: json boom',
    );
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('McpClient: an SSE response body is parsed to the last JSON-RPC message', async () => {
  const realFetch = globalThis.fetch;
  const sseBody = [
    'data: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}',
    '',
    '',
  ].join('\n');
  globalThis.fetch = async () => new Response(sseBody, {
    status: 200,
    headers: { 'Content-Type': 'text/event-stream', 'mcp-session-id': 'sse-session' },
  });
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    const result = await client.initialize();
    assert.equal(result.protocolVersion, '2024-11-05');
    assert.equal(client.sessionId, 'sse-session');
  } finally {
    globalThis.fetch = realFetch;
  }
});

test('McpClient: a tool-level JSON-RPC error surfaces as mcp_error', async () => {
  const realFetch = globalThis.fetch;
  const { handler } = makeFakeMcp();
  globalThis.fetch = async (url, options) => {
    const body = JSON.parse(String(options.body ?? ''));
    if (body.method === 'tools/call') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        error: { code: -32000, message: 'tool exploded' },
      });
    }
    return handler(url, options);
  };
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await client.initialize();
    await assert.rejects(
      () => client.callTool('search_code', { query: 'x' }),
      (e) => e instanceof AgentError && e.code === 'mcp_error' && /tool exploded/.test(e.message),
    );
  } finally {
    globalThis.fetch = realFetch;
  }
});

// ---------------------------------------------------------------------------
// Lifecycle: main() with a fake MCP endpoint (fetch(url, options))
// ---------------------------------------------------------------------------

test('main: initializes MCP before any tools/call and audits a successful run', async () => {
  const repoRoot = await makeTempDir('lca-lifecycle-');
  const realFetch = globalThis.fetch;
  const realStdout = process.stdout.write;
  const realStderr = process.stderr.write;
  const requests = [];
  let modelContacted = false;
  let capturedOut = '';
  let capturedErr = '';
  process.stdout.write = (chunk) => { capturedOut += String(chunk); return true; };
  process.stderr.write = (chunk) => { capturedErr += String(chunk); return true; };
  globalThis.fetch = async (url, options = {}) => {
    const u = String(url);
    if (u.includes('/chat/completions')) {
      modelContacted = true;
      return Response.json({ choices: [{ message: { content: 'final answer' } }] });
    }
    // MCP endpoint: record the JSON-RPC method in order.
    const body = JSON.parse(String(options.body ?? ''));
    requests.push(body.method);
    if (body.method === 'initialize') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        result: { protocolVersion: '2024-11-05', capabilities: {}, serverInfo: { name: 'fake', version: '0' } },
      }, { headers: { 'mcp-session-id': 'lifecycle-session' } });
    }
    if (body.method === 'notifications/initialized') {
      return new Response(null, { status: 202 });
    }
    if (body.method === 'tools/call') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        result: { content: [{ type: 'text', text: `called:${body.params.name}` }] },
      });
    }
    return Response.json({ jsonrpc: '2.0', id: body.id, error: { code: -32601, message: 'unknown method' } });
  };
  const prevQwApiKey = process.env.QW_API_KEY;
  try {
    process.env.QW_API_KEY = 'test-key';
    const policyPath = writePolicyFile(repoRoot);
    const code = await main(['--policy', policyPath, '--task', 'do the thing']);
    assert.equal(code, 0);
    assert.ok(modelContacted, 'the model must be contacted on the live path');
    assert.ok(capturedOut.includes('final answer'));
    assert.equal(capturedErr, '', 'no stderr output on success');
    // The initialize handshake must precede any tools/call request.
    const initIdx = requests.indexOf('initialize');
    const firstCallIdx = requests.findIndex((m) => m === 'tools/call');
    assert.ok(initIdx !== -1, 'initialize was never sent');
    if (firstCallIdx !== -1) {
      assert.ok(initIdx < firstCallIdx, 'initialize must precede tools/call');
    }
    // The audit log must record a successful model turn.
    const policy = loadPolicy(policyPath);
    const lines = readAuditLines(policy);
    const okTurn = lines.find((l) => l.kind === 'model_turn' && l.ok === true);
    assert.ok(okTurn, 'a successful model turn must be audited');
  } finally {
    process.stdout.write = realStdout;
    process.stderr.write = realStderr;
    globalThis.fetch = realFetch;
    if (prevQwApiKey === undefined) delete process.env.QW_API_KEY;
    else process.env.QW_API_KEY = prevQwApiKey;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// main() failure paths (offline; scripted fetch)
// ---------------------------------------------------------------------------

test('main: MCP init failure rejects with mcp_init_failed and never contacts the model', async () => {
  const repoRoot = await makeTempDir('lca-main-initfail-');
  const realFetch = globalThis.fetch;
  let modelContacted = false;
  globalThis.fetch = async (url, options = {}) => {
    const u = String(url);
    if (u.includes('/chat/completions')) {
      modelContacted = true;
      return Response.json({ choices: [{ message: { content: 'nope' } }] });
    }
    return new Response('boom', { status: 500, headers: { 'Content-Type': 'text/plain' } });
  };
  try {
    const policyPath = writePolicyFile(repoRoot);
    await assert.rejects(
      () => main(['--policy', policyPath, '--task', 'do the thing']),
      (e) => e instanceof AgentError && e.code === 'mcp_init_failed',
    );
    assert.equal(modelContacted, false, 'the model must not be contacted when MCP init fails');
  } finally {
    globalThis.fetch = realFetch;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('main: --help prints usage and returns 0 without loading a policy', async () => {
  const realStdout = process.stdout.write;
  let captured = '';
  process.stdout.write = (chunk) => { captured += String(chunk); return true; };
  try {
    const code = await main(['--help']);
    assert.equal(code, 0);
    assert.ok(captured.includes('Usage:'));
    assert.ok(captured.includes('--policy'));
  } finally {
    process.stdout.write = realStdout;
  }
});

test('main: a missing task file is rejected', async () => {
  const repoRoot = await makeTempDir('lca-main-missing-');
  try {
    const policyPath = writePolicyFile(repoRoot);
    await assert.rejects(
      () => main(['--policy', policyPath, '--task-file', path.join(repoRoot, 'nope.md')]),
      (e) => e instanceof AgentError && /task file not found/.test(e.message),
    );
  } finally {
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('main: an empty task is rejected', async () => {
  const repoRoot = await makeTempDir('lca-main-empty-');
  try {
    const policyPath = writePolicyFile(repoRoot);
    await assert.rejects(
      () => main(['--policy', policyPath, '--task', '   ']),
      (e) => e instanceof AgentError && /task text is empty/.test(e.message),
    );
  } finally {
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('main: a model HTTP error surfaces as model_error', async () => {
  const repoRoot = await makeTempDir('lca-main-modelerr-');
  const realFetch = globalThis.fetch;
  const realStderr = process.stderr.write;
  let capturedErr = '';
  process.stderr.write = (chunk) => { capturedErr += String(chunk); return true; };
  globalThis.fetch = async (url, options = {}) => {
    const u = String(url);
    if (u.includes('/chat/completions')) {
      return new Response('model exploded', { status: 500, headers: { 'Content-Type': 'text/plain' } });
    }
    const body = JSON.parse(String(options.body ?? ''));
    if (body.method === 'initialize') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        result: { protocolVersion: '2024-11-05', capabilities: {}, serverInfo: { name: 'fake', version: '0' } },
      }, { headers: { 'mcp-session-id': 'modelerr-session' } });
    }
    if (body.method === 'notifications/initialized') {
      return new Response(null, { status: 202 });
    }
    return Response.json({ jsonrpc: '2.0', id: body.id, error: { code: -32601, message: 'unknown method' } });
  };
  const prevQwApiKey = process.env.QW_API_KEY;
  try {
    process.env.QW_API_KEY = 'test-key';
    const policyPath = writePolicyFile(repoRoot);
    const code = await main(['--policy', policyPath, '--task', 'do the thing']);
    assert.equal(code, 1, 'main must return a nonzero exit code on model failure');
    assert.ok(capturedErr.includes('agent failed:'), 'the failure must be reported on stderr');
    assert.ok(capturedErr.includes('model request failed (HTTP 500)'), 'the model error must be surfaced');
  } finally {
    process.stderr.write = realStderr;
    globalThis.fetch = realFetch;
    if (prevQwApiKey === undefined) delete process.env.QW_API_KEY;
    else process.env.QW_API_KEY = prevQwApiKey;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Remaining public-behavior regressions
// ---------------------------------------------------------------------------

// list_files must accept a parent directory that is authorized only because a
// read glob matches its descendants (e.g. `tools/local-agent/**`), return the
// readable immediate descendants, and omit any child matched by a deny glob.
test('list_files: accepts an authorized parent directory under a descendant-only read glob and omits denied children', async () => {
  const repoRoot = await makeTempDir('lca-listfiles-');
  try {
    fs.mkdirSync(path.join(repoRoot, 'tools', 'local-agent'), { recursive: true });
    fs.writeFileSync(path.join(repoRoot, 'tools', 'local-agent', 'agent.mjs'), 'x', 'utf8');
    fs.writeFileSync(path.join(repoRoot, 'tools', 'local-agent', 'secret.txt'), 'x', 'utf8');
    const policy = makePolicy({
      repoRoot,
      read: ['tools/local-agent/**'],
      write: ['tools/local-agent/**'],
      deny: ['tools/local-agent/secret.txt'],
    });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    const out = await tools.list_files.fn({ path: 'tools/local-agent' });
    assert.ok(out.includes('- tools/local-agent/agent.mjs'), 'readable immediate descendant must be listed');
    assert.ok(!out.includes('secret.txt'), 'denied child must be omitted');
  } finally {
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// McpClient.close must be idempotent and overlap-safe: concurrent closes plus
// a later close send no more than one closing HTTP request and leave
// sessionId null.
test('McpClient: close invoked twice concurrently and once later sends at most one closing request and leaves sessionId null', async () => {
  const realFetch = globalThis.fetch;
  const { handler } = makeFakeMcp();
  let closeRequests = 0;
  globalThis.fetch = async (url, options) => {
    const body = JSON.parse(String(options.body ?? ''));
    if (body.method === 'initialize' || body.method === 'notifications/initialized') {
      return handler(url, options);
    }
    closeRequests += 1;
    return new Response(null, { status: 202 });
  };
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await client.initialize();
    const [r1, r2] = await Promise.all([client.close(), client.close()]);
    assert.equal(r1, r2, 'concurrent closes must share the same teardown result');
    await client.close();
    assert.equal(client.sessionId, null, 'sessionId must be null after close');
    assert.equal(client.initialized, false);
    assert.ok(closeRequests <= 1, `at most one closing request may be sent, got ${closeRequests}`);
  } finally {
    globalThis.fetch = realFetch;
  }
});

// main with a valid --contract and --dry-run must perform no fetch at all,
// print the contract id and the numeric allowed-file/check counts, and never
// leak the contract objective, acceptance, or prohibited text.
test('main: valid --contract --dry-run performs no fetch and reports only the contract id and counts', async () => {
  const repoRoot = await makeTempDir('lca-contract-dryrun-');
  const realFetch = globalThis.fetch;
  const realStdout = process.stdout.write;
  let fetchCalls = 0;
  let captured = '';
  process.stdout.write = (chunk) => { captured += String(chunk); return true; };
  globalThis.fetch = async () => {
    fetchCalls += 1;
    return Response.json({ choices: [{ message: { content: 'nope' } }] });
  };
  try {
    const policyPath = writePolicyFile(repoRoot);
    const contractPath = writeContractFile(repoRoot, {
      id: 'contract-dry-1',
      allowedFiles: ['src/a.rs', 'src/b.rs'],
      checkSuites: ['chunk'],
    });
    const code = await main(['--policy', policyPath, '--contract', contractPath, '--dry-run']);
    assert.equal(code, 0);
    assert.equal(fetchCalls, 0, 'dry-run must not perform any fetch');
    assert.ok(captured.includes('contract: contract-dry-1'), 'output must contain the contract id');
    assert.ok(captured.includes('contract allowed files: 2'), 'output must contain the numeric allowed-file count');
    assert.ok(captured.includes('contract check suites: 1'), 'output must contain the numeric check count');
    assert.ok(!captured.includes('SECRET_OBJECTIVE'), 'objective text must not leak');
    assert.ok(!captured.includes('SECRET_ACCEPTANCE'), 'acceptance text must not leak');
    assert.ok(!captured.includes('SECRET_PROHIBITED'), 'prohibited text must not leak');
  } finally {
    process.stdout.write = realStdout;
    globalThis.fetch = realFetch;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Additional public-behavior regressions (appended)
// ---------------------------------------------------------------------------

// list_files must accept a readable parent directory and filter out children
// that are denied by the policy, while still listing readable children.
test('list_files accepts readable parent and filters denied children', async () => {
  const repoRoot = await makeTempDir('lca-listfiles-denied-');
  try {
    fs.mkdirSync(path.join(repoRoot, 'tools', 'local-agent'), { recursive: true });
    fs.writeFileSync(path.join(repoRoot, 'tools', 'local-agent', 'visible.txt'), 'visible', 'utf8');
    fs.writeFileSync(path.join(repoRoot, 'tools', 'local-agent', 'secret.txt'), 'secret', 'utf8');
    const policy = makePolicy({
      repoRoot,
      read: ['tools/local-agent/**'],
      write: ['tools/local-agent/**'],
      deny: ['**/secret.txt'],
    });
    const audit = new AuditLog(policy);
    const tools = buildTools(policy, audit, { dryRun: false });
    const out = await tools.list_files.fn({ path: 'tools/local-agent' });
    assert.ok(out.includes('tools/local-agent/visible.txt'), 'readable child must be listed');
    assert.ok(!out.includes('secret.txt'), 'denied child must be filtered out');
  } finally {
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// McpClient.close must be idempotent across concurrent and repeated calls:
// no more than one closing request is sent and the session id is cleared.
test('McpClient close is idempotent across concurrent and repeated calls', async () => {
  const realFetch = globalThis.fetch;
  const { handler } = makeFakeMcp();
  let closeRequests = 0;
  globalThis.fetch = async (url, options) => {
    const body = JSON.parse(String(options.body ?? ''));
    if (body.method === 'initialize' || body.method === 'notifications/initialized') {
      return handler(url, options);
    }
    closeRequests += 1;
    return new Response(null, { status: 202 });
  };
  try {
    const client = new McpClient('http://127.0.0.1:0/mcp');
    await client.initialize();
    assert.equal(client.sessionId, 'fake-session-1', 'initialize must store the session id');
    await Promise.all([client.close(), client.close()]);
    await client.close();
    assert.ok(closeRequests <= 1, `at most one closing request may be sent, got ${closeRequests}`);
    assert.equal(client.sessionId, null, 'sessionId must be null after close');
    assert.equal(client.initialized, false, 'initialized must be false after close');
  } finally {
    globalThis.fetch = realFetch;
  }
});

// main with a valid --contract and --dry-run must report only the contract
// metadata (id and counts) and never leak the contract text.
test('main contract dry-run reports metadata without leaking contract text', async () => {
  const repoRoot = await makeTempDir('lca-contract-dryrun-meta-');
  const realFetch = globalThis.fetch;
  const realStdout = process.stdout.write;
  let fetchCalls = 0;
  let captured = '';
  process.stdout.write = (chunk) => { captured += String(chunk); return true; };
  globalThis.fetch = async () => {
    fetchCalls += 1;
    throw new Error('fetch must not be called during dry-run');
  };
  try {
    const policyPath = writePolicyFile(repoRoot);
    const contractPath = writeContractFile(repoRoot, {
      id: 'contract-meta-1',
      allowedFiles: ['src/a.rs', 'src/b.rs'],
      checkSuites: ['chunk'],
    });
    const code = await main(['--policy', policyPath, '--contract', contractPath, '--dry-run']);
    assert.equal(code, 0, 'dry-run must return 0');
    assert.equal(fetchCalls, 0, 'dry-run must not perform any fetch');
    assert.ok(captured.includes('contract: contract-meta-1'), 'output must contain the contract id');
    assert.ok(captured.includes('contract allowed files: 2'), 'output must contain the allowed-file count');
    assert.ok(captured.includes('contract check suites: 1'), 'output must contain the check-suite count');
    assert.ok(!captured.includes('SECRET_OBJECTIVE'), 'objective text must not leak');
    assert.ok(!captured.includes('SECRET_ACCEPTANCE'), 'acceptance text must not leak');
    assert.ok(!captured.includes('SECRET_PROHIBITED'), 'prohibited text must not leak');
  } finally {
    process.stdout.write = realStdout;
    globalThis.fetch = realFetch;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// main must reject a contract whose allowedFiles are not writable by the base
// policy before any model contact or mutation.
test('main rejects unauthorized contract before model contact or mutation', async () => {
  const repoRoot = await makeTempDir('lca-contract-unauth-');
  const realFetch = globalThis.fetch;
  let fetchCalls = 0;
  globalThis.fetch = async () => {
    fetchCalls += 1;
    return Response.json({ choices: [{ message: { content: 'nope' } }] });
  };
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const target = path.join(repoRoot, 'src', 'a.rs');
    const original = 'MARK\n';
    fs.writeFileSync(target, original, 'utf8');
    const policyPath = writePolicyFile(repoRoot);
    const contractPath = writeContractFile(repoRoot, { allowedFiles: ['docs/no.md'] });
    await assert.rejects(
      () => main(['--policy', policyPath, '--contract', contractPath]),
      (e) => e instanceof AgentError && /not writable/.test(e.message),
    );
    assert.equal(fetchCalls, 0, 'no fetch may occur before contract validation');
    assert.equal(fs.readFileSync(target, 'utf8'), original, 'target file must be unchanged');
  } finally {
    globalThis.fetch = realFetch;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// main with a valid non-dry-run --contract and mocked MCP/model must return 0
// and record the contractId on the audit model_turn record.
test('main live contract path audits contract id', async () => {
  const repoRoot = await makeTempDir('lca-contract-live-audit-');
  const realFetch = globalThis.fetch;
  const realStdout = process.stdout.write;
  const realStderr = process.stderr.write;
  const mcpMethods = [];
  let modelContacted = false;
  let capturedOut = '';
  let capturedErr = '';
  process.stdout.write = (chunk) => { capturedOut += String(chunk); return true; };
  process.stderr.write = (chunk) => { capturedErr += String(chunk); return true; };
  globalThis.fetch = async (url, options = {}) => {
    const u = String(url);
    if (u.includes('/chat/completions')) {
      modelContacted = true;
      return Response.json({ choices: [{ message: { content: 'live contract final answer' } }] });
    }
    const body = JSON.parse(String(options.body ?? ''));
    mcpMethods.push(body.method);
    if (body.method === 'initialize') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        result: { protocolVersion: '2024-11-05', capabilities: {}, serverInfo: { name: 'fake', version: '0' } },
      }, { headers: { 'mcp-session-id': 'live-contract-session' } });
    }
    if (body.method === 'notifications/initialized') {
      return new Response(null, { status: 202 });
    }
    return Response.json({ jsonrpc: '2.0', id: body.id, error: { code: -32601, message: 'unknown method' } });
  };
  const prevQwApiKey = process.env.QW_API_KEY;
  try {
    process.env.QW_API_KEY = 'test-key';
    const policyPath = writePolicyFile(repoRoot);
    const contractPath = writeContractFile(repoRoot, { id: 'contract-live-audit-1' });
    const code = await main(['--policy', policyPath, '--contract', contractPath]);
    assert.equal(code, 0, 'live contract run must return 0');
    assert.ok(modelContacted, 'the model must be contacted on the live path');
    assert.ok(capturedOut.includes('live contract final answer'));
    assert.ok(mcpMethods.includes('initialize'), 'MCP initialize must be mocked and used');
    const policy = loadPolicy(policyPath);
    const lines = readAuditLines(policy);
    const modelLine = lines.find((l) => l.kind === 'model_turn');
    assert.ok(modelLine, 'a model turn must be audited');
    assert.equal(modelLine.contractId, 'contract-live-audit-1', 'the audit model record must carry the contractId');
  } finally {
    process.stdout.write = realStdout;
    process.stderr.write = realStderr;
    globalThis.fetch = realFetch;
    if (prevQwApiKey === undefined) delete process.env.QW_API_KEY;
    else process.env.QW_API_KEY = prevQwApiKey;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// main with an invalid contract, or a contract whose allowedFiles are not
// authorized by the base policy, must reject before any fetch or model
// contact and leave the target file unchanged.
test('main: invalid or base-policy-unauthorized --contract rejects before any fetch and leaves the target file unchanged', async () => {
  const repoRoot = await makeTempDir('lca-contract-reject-');
  const realFetch = globalThis.fetch;
  let fetchCalls = 0;
  globalThis.fetch = async () => {
    fetchCalls += 1;
    return Response.json({ choices: [{ message: { content: 'nope' } }] });
  };
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const target = path.join(repoRoot, 'src', 'a.rs');
    const original = 'MARK\n';
    fs.writeFileSync(target, original, 'utf8');
    const policyPath = writePolicyFile(repoRoot);

    // Case 1: invalid contract (bad schema).
    const invalidPath = writeContractFile(repoRoot, { schema: 'wrong/v0' });
    await assert.rejects(
      () => main(['--policy', policyPath, '--contract', invalidPath]),
      (e) => e instanceof AgentError && /unsupported contract schema/.test(e.message),
    );

    // Case 2: contract allowedFile not writable by the base policy.
    const unauthorizedPath = writeContractFile(repoRoot, { allowedFiles: ['other/a.rs'] });
    await assert.rejects(
      () => main(['--policy', policyPath, '--contract', unauthorizedPath]),
      (e) => e instanceof AgentError && /not writable/.test(e.message),
    );

    assert.equal(fetchCalls, 0, 'no fetch may occur before contract validation');
    assert.equal(fs.readFileSync(target, 'utf8'), original, 'target file must be unchanged');
  } finally {
    globalThis.fetch = realFetch;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// main with a valid non-dry-run --contract and mocked MCP initialize/close
// plus a mocked model final response must return 0 and record the contractId
// on the audit model record.
test('main: valid non-dry-run --contract with mocked MCP and model returns 0 and audits the contractId', async () => {
  const repoRoot = await makeTempDir('lca-contract-live-');
  const realFetch = globalThis.fetch;
  const realStdout = process.stdout.write;
  const realStderr = process.stderr.write;
  const mcpMethods = [];
  let modelContacted = false;
  let capturedOut = '';
  let capturedErr = '';
  process.stdout.write = (chunk) => { capturedOut += String(chunk); return true; };
  process.stderr.write = (chunk) => { capturedErr += String(chunk); return true; };
  globalThis.fetch = async (url, options = {}) => {
    const u = String(url);
    if (u.includes('/chat/completions')) {
      modelContacted = true;
      return Response.json({ choices: [{ message: { content: 'contract final answer' } }] });
    }
    const body = JSON.parse(String(options.body ?? ''));
    mcpMethods.push(body.method);
    if (body.method === 'initialize') {
      return Response.json({
        jsonrpc: '2.0',
        id: body.id,
        result: { protocolVersion: '2024-11-05', capabilities: {}, serverInfo: { name: 'fake', version: '0' } },
      }, { headers: { 'mcp-session-id': 'contract-live-session' } });
    }
    if (body.method === 'notifications/initialized') {
      return new Response(null, { status: 202 });
    }
    return Response.json({ jsonrpc: '2.0', id: body.id, error: { code: -32601, message: 'unknown method' } });
  };
  const prevQwApiKey = process.env.QW_API_KEY;
  try {
    process.env.QW_API_KEY = 'test-key';
    const policyPath = writePolicyFile(repoRoot);
    const contractPath = writeContractFile(repoRoot, { id: 'contract-live-1' });
    const code = await main(['--policy', policyPath, '--contract', contractPath]);
    assert.equal(code, 0);
    assert.ok(modelContacted, 'the model must be contacted on the live path');
    assert.ok(capturedOut.includes('contract final answer'));
    assert.ok(mcpMethods.includes('initialize'), 'MCP initialize must be mocked and used');
    const policy = loadPolicy(policyPath);
    const lines = readAuditLines(policy);
    const modelLine = lines.find((l) => l.kind === 'model_turn');
    assert.ok(modelLine, 'a model turn must be audited');
    assert.equal(modelLine.contractId, 'contract-live-1', 'the audit model record must carry the contractId');
    // MARKER-PLANNER-SECTION-ANCHOR
  } finally {
    process.stdout.write = realStdout;
    process.stderr.write = realStderr;
    globalThis.fetch = realFetch;
    if (prevQwApiKey === undefined) delete process.env.QW_API_KEY;
    else process.env.QW_API_KEY = prevQwApiKey;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Planner policy validation and planVerification (offline, pure)
// ---------------------------------------------------------------------------
// These tests cover loadPolicy's backward-compatible planner defaults,
// valid and malformed pathRules/finalSuite fields, the pure
// planVerification planner (changed-path validation, stable deduped
// multi-file planning, active-contract filtering, final-suite deferral,
// unmatched paths, overlapping-rule aggregation), and the
// plan_verification tool wrapper. None of them contact a model, an MCP
// endpoint, or a repository process, and the in-memory planner tests never
// touch the filesystem.

function makePlanPolicy({ pathRules, finalSuite = 'full' } = {}) {
  const policy = makePolicy({ repoRoot: 'C:\\plan' });
  policy.checks = {
    script: 'scripts/Test.ps1',
    suites: { chunk: 'Chunk', filter: 'Filter', evaluation: 'Evaluation', full: 'Full' },
    pathRules: pathRules ?? [
      { glob: 'src/**', suites: ['chunk'], reason: 'chunk sources changed' },
      { glob: 'src/filter/**', suites: ['filter', 'chunk'], reason: 'filter sources changed' },
      { glob: 'tests/**', suites: ['evaluation'], reason: 'evaluation tests changed' },
    ],
    finalSuite,
  };
  return policy;
}

test('loadPolicy: absent planner fields keep backward-compatible defaults and plan as unmatched', async () => {
  const repoRoot = await makeTempDir('lca-planner-defaults-');
  try {
    const policyPath = writePolicyFile(repoRoot); // base checks has no pathRules/finalSuite
    const policy = loadPolicy(policyPath);
    assert.deepEqual(policy.checks.pathRules, [], 'absent pathRules must resolve to []');
    assert.equal(policy.checks.finalSuite, null, 'absent finalSuite must resolve to null');
    assert.deepEqual(planVerification(policy, ['src/a.rs']), {
      focused: [],
      deferred: [],
      unmatchedPaths: ['src/a.rs'],
    });
  } finally {
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('loadPolicy: valid pathRules/finalSuite resolve and malformed planner fields are rejected', async () => {
  const repoRoot = await makeTempDir('lca-planner-policy-');
  try {
    const suites = { chunk: 'Chunk', filter: 'Filter', full: 'Full' };
    const withPlanner = (pathRules, finalSuite) => {
      const checks = { script: 'scripts/Test.ps1', suites, pathRules };
      if (finalSuite !== undefined) checks.finalSuite = finalSuite;
      return checks;
    };

    // Valid shape: globs are normalized, suites keep rule order, finalSuite resolves.
    const validPath = writePolicyFile(repoRoot, {
      checks: withPlanner(
        [
          { glob: 'src\\filter/**', suites: ['filter', 'chunk'], reason: 'filter sources changed' },
          { glob: 'tests/**', suites: ['chunk'], reason: 'tests changed' },
        ],
        'full',
      ),
    });
    const policy = loadPolicy(validPath);
    assert.deepEqual(policy.checks.pathRules, [
      { glob: 'src/filter/**', suites: ['filter', 'chunk'], reason: 'filter sources changed' },
      { glob: 'tests/**', suites: ['chunk'], reason: 'tests changed' },
    ]);
    assert.equal(policy.checks.finalSuite, 'full');

    const badCases = [
      [{ checks: withPlanner('not-an-array', 'full') }, /checks\.pathRules must be an array/],
      [{ checks: withPlanner([42], 'full') }, /checks\.pathRules entries must be objects/],
      [{ checks: withPlanner([{ glob: 'src/**', suites: ['chunk'] }], 'full') }, /entry\.reason must be a non-empty string/],
      [{ checks: withPlanner([{ suites: ['chunk'], reason: 'r' }], 'full') }, /entry\.glob must be a non-empty string/],
      [{ checks: withPlanner([{ glob: 'src/**' }], 'full') }, /entry\.suites must be a non-empty array/],
      [{ checks: withPlanner([{ glob: 'src/**', suites: [], reason: 'r' }], 'full') }, /entry\.suites must be a non-empty array/],
      [{ checks: withPlanner([{ glob: 'src/**', suites: [7], reason: 'r' }], 'full') }, /entry\.suites entries must be non-empty strings/],
      [{ checks: withPlanner([{ glob: 'src/**', suites: ['chunk', 'chunk'], reason: 'r' }], 'full') }, /entry\.suites contains a duplicate: chunk/],
      [{ checks: withPlanner([{ glob: 'src/**', suites: ['nope'], reason: 'r' }], 'full') }, /references unknown suite: nope/],
      [{ checks: withPlanner([{ glob: 'src/**', suites: ['chunk'], reason: 'r' }, { glob: 'src/**', suites: ['chunk'], reason: 'r' }], 'full') }, /duplicate rule: src\/\*\*/],
      [{ checks: withPlanner([], 0) }, /finalSuite must be a non-empty string/],
      [{ checks: withPlanner([], 'nope') }, /finalSuite references unknown suite: nope/],
    ];
    for (const [overrides, pattern] of badCases) {
      const badPath = writePolicyFile(repoRoot, overrides);
      assert.throws(
        () => loadPolicy(badPath),
        (e) => e instanceof AgentError && pattern.test(e.message),
        `expected rejection for ${JSON.stringify(overrides)}`,
      );
    }
  } finally {
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});

test('planVerification: changed-path validation rejects empty, duplicate, unsafe, and non-string inputs', () => {
  const policy = makePlanPolicy();
  assert.throws(
    () => planVerification(policy, []),
    (e) => e instanceof AgentError && /non-empty changedPaths/.test(e.message),
  );
  assert.throws(
    () => planVerification(policy, 'src/a.rs'),
    (e) => e instanceof AgentError && /non-empty changedPaths/.test(e.message),
  );
  assert.throws(
    () => planVerification(policy, null),
    (e) => e instanceof AgentError && /non-empty changedPaths/.test(e.message),
  );
  assert.throws(
    () => planVerification(policy, ['src/a.rs', './src/a.rs']),
    (e) => e instanceof AgentError && /duplicate after normalization: src\/a\.rs/.test(e.message),
  );
  const unsafeCases = [
    [['src/a.rs', '/abs/a.rs'], /absolute paths are not allowed/],
    [['src/a.rs', 'C:\\abs\\a.rs'], /absolute paths are not allowed/],
    [['src/a.rs', '../a.rs'], /path traversal is not allowed/],
    [['src/a.rs', 'src/\x00a.rs'], /NUL byte/],
    [['src/a.rs', 42], /non-empty string/],
    [['src/a.rs', null], /non-empty string/],
  ];
  for (const [input, pattern] of unsafeCases) {
    assert.throws(
      () => planVerification(policy, input),
      (e) => e instanceof AgentError && pattern.test(e.message),
      `expected rejection for ${JSON.stringify(input)}`,
    );
  }
});

test('planVerification: stable deduped multi-file planning keeps policy declaration order', () => {
  const policy = makePlanPolicy();
  const input = ['src/filter/x.rs', 'tests/t.rs'];
  const expected = {
    focused: [
      { suite: 'chunk', reasons: ['chunk sources changed', 'filter sources changed'], matchedPaths: ['src/filter/x.rs'] },
      { suite: 'filter', reasons: ['filter sources changed'], matchedPaths: ['src/filter/x.rs'] },
      { suite: 'evaluation', reasons: ['evaluation tests changed'], matchedPaths: ['tests/t.rs'] },
    ],
    deferred: [
      { suite: 'full', reasons: ['run once after all accepted slices'], matchedPaths: [] },
    ],
    unmatchedPaths: [],
  };
  assert.deepEqual(planVerification(policy, input), expected);
  assert.deepEqual(planVerification(policy, [...input]), expected, 'identical inputs must yield an identical plan');
  // Multiple files under the same rule collapse into one deduped entry.
  const multi = planVerification(policy, ['src/app/b.rs', 'src/a.rs']);
  assert.deepEqual(multi.focused, [
    { suite: 'chunk', reasons: ['chunk sources changed'], matchedPaths: ['src/app/b.rs', 'src/a.rs'] },
  ]);
  assert.deepEqual(multi.deferred, [
    { suite: 'full', reasons: ['run once after all accepted slices'], matchedPaths: [] },
  ]);
});

test('planVerification: active contract moves unauthorized suites to deferred', () => {
  const policy = makePlanPolicy();
  const plan = planVerification(policy, ['src/filter/x.rs', 'tests/t.rs'], {
    contract: { id: 'slice-1', checkSuites: ['filter'] },
  });
  assert.deepEqual(plan, {
    focused: [
      { suite: 'filter', reasons: ['filter sources changed'], matchedPaths: ['src/filter/x.rs'] },
    ],
    deferred: [
      { suite: 'chunk', reasons: ['not authorized by active contract', 'chunk sources changed', 'filter sources changed'], matchedPaths: ['src/filter/x.rs'] },
      { suite: 'evaluation', reasons: ['not authorized by active contract', 'evaluation tests changed'], matchedPaths: ['tests/t.rs'] },
      { suite: 'full', reasons: ['run once after all accepted slices'], matchedPaths: [] },
    ],
    unmatchedPaths: [],
  });
  // An empty contract suite authorizes nothing; every suggested suite defers.
  const none = planVerification(policy, ['src/a.rs'], { contract: { checkSuites: [] } });
  assert.deepEqual(none.focused, []);
  assert.deepEqual(none.deferred, [
    { suite: 'chunk', reasons: ['not authorized by active contract', 'chunk sources changed'], matchedPaths: ['src/a.rs'] },
    { suite: 'full', reasons: ['run once after all accepted slices'], matchedPaths: [] },
  ]);
});

test('planVerification: finalSuite deferral is focused-first, idempotent, and mergeable', () => {
  // Rule-matched change: finalSuite is deferred once with empty matchedPaths.
  const policy = makePlanPolicy();
  const plan = planVerification(policy, ['tests/t.rs']);
  assert.deepEqual(plan.focused, [
    { suite: 'evaluation', reasons: ['evaluation tests changed'], matchedPaths: ['tests/t.rs'] },
  ]);
  assert.deepEqual(plan.deferred, [
    { suite: 'full', reasons: ['run once after all accepted slices'], matchedPaths: [] },
  ]);
  assert.deepEqual(plan.unmatchedPaths, []);

  // A focused finalSuite is never double-listed as deferred.
  const focusedPolicy = makePlanPolicy({ pathRules: [{ glob: 'src/**', suites: ['full'], reason: 'full sources changed' }] });
  const focusedPlan = planVerification(focusedPolicy, ['src/a.rs']);
  assert.deepEqual(focusedPlan.focused, [
    { suite: 'full', reasons: ['full sources changed'], matchedPaths: ['src/a.rs'] },
  ]);
  assert.deepEqual(focusedPlan.deferred, []);

  // A contract-deferred finalSuite gains the deferral reason on the same entry.
  const mergedPlan = planVerification(focusedPolicy, ['src/a.rs'], { contract: { checkSuites: [] } });
  assert.deepEqual(mergedPlan.focused, []);
  assert.deepEqual(mergedPlan.deferred, [
    {
      suite: 'full',
      reasons: ['not authorized by active contract', 'full sources changed', 'run once after all accepted slices'],
      matchedPaths: ['src/a.rs'],
    },
  ]);

  // Without a finalSuite nothing is deferred at all.
  const noFinal = makePlanPolicy({ finalSuite: null, pathRules: [{ glob: 'src/**', suites: ['chunk'], reason: 'chunk sources changed' }] });
  assert.deepEqual(planVerification(noFinal, ['src/a.rs']), {
    focused: [{ suite: 'chunk', reasons: ['chunk sources changed'], matchedPaths: ['src/a.rs'] }],
    deferred: [],
    unmatchedPaths: [],
  });
});

test('planVerification: unmatched paths are reported without escalation to any suite', () => {
  const policy = makePlanPolicy();
  const plan = planVerification(policy, ['docs/readme.md', 'src/a.rs', 'tools/local-agent/x.mjs']);
  assert.deepEqual(plan.unmatchedPaths, ['docs/readme.md', 'tools/local-agent/x.mjs']);
  assert.ok(plan.focused.some((e) => e.suite === 'chunk'), 'matched paths still plan normally');
  for (const entry of [...plan.focused, ...plan.deferred]) {
    for (const p of entry.matchedPaths) {
      assert.ok(!plan.unmatchedPaths.includes(p), `${p} must not appear in both buckets`);
    }
  }
});

test('planVerification: overlapping rules aggregate into one deduped entry per suite', () => {
  const policy = makePlanPolicy({
    pathRules: [
      { glob: 'src/**', suites: ['chunk'], reason: 'chunk sources changed' },
      { glob: 'src/filter/**', suites: ['chunk', 'filter'], reason: 'filter sources changed' },
    ],
  });
  const plan = planVerification(policy, ['src/filter/x.rs', 'src/other/y.rs']);
  // chunk is created once in declaration order and aggregates both rules'
  // reasons plus every matched path; filter is focused from the narrow rule.
  assert.deepEqual(plan, {
    focused: [
      {
        suite: 'chunk',
        reasons: ['chunk sources changed', 'filter sources changed'],
        matchedPaths: ['src/filter/x.rs', 'src/other/y.rs'],
      },
      { suite: 'filter', reasons: ['filter sources changed'], matchedPaths: ['src/filter/x.rs'] },
    ],
    deferred: [
      { suite: 'full', reasons: ['run once after all accepted slices'], matchedPaths: [] },
    ],
    unmatchedPaths: [],
  });
});

test('plan_verification tool: matches the pure planner and performs no fetch or file mutation', async () => {
  const repoRoot = await makeTempDir('lca-planner-tool-');
  const realFetch = globalThis.fetch;
  let fetchCalls = 0;
  globalThis.fetch = async () => {
    fetchCalls += 1;
    throw new Error('plan_verification must not fetch');
  };
  try {
    fs.mkdirSync(path.join(repoRoot, 'src'), { recursive: true });
    const target = path.join(repoRoot, 'src', 'a.rs');
    const original = 'MARK\n';
    fs.writeFileSync(target, original, 'utf8');
    const policyPath = writePolicyFile(repoRoot, {
      checks: {
        script: 'scripts/Test.ps1',
        suites: { chunk: 'Chunk', filter: 'Filter', full: 'Full' },
        pathRules: [
          { glob: 'src/**', suites: ['chunk'], reason: 'chunk sources changed' },
          { glob: 'src/filter/**', suites: ['filter'], reason: 'filter sources changed' },
        ],
        finalSuite: 'full',
      },
    });
    const policy = loadPolicy(policyPath);
    const tools = buildTools(policy, {}, {});
    assert.ok(tools.plan_verification, 'plan_verification must be exposed as a tool');

    const changedPaths = ['src/a.rs', 'src/filter/b.rs'];
    const expected = planVerification(policy, changedPaths);
    const out = await tools.plan_verification.fn({ changedPaths });
    assert.equal(out, JSON.stringify(expected, null, 2), 'tool output must be the pretty JSON of the pure plan');
    assert.deepEqual(JSON.parse(out), expected);

    // The active contract flows through the tool into the same pure planner.
    const contractPath = writeContractFile(repoRoot, { checkSuites: ['chunk'] });
    const contract = loadTaskContract(contractPath, policy);
    const contractOut = await buildTools(policy, {}, { contract }).plan_verification.fn({ changedPaths });
    assert.deepEqual(JSON.parse(contractOut), planVerification(policy, changedPaths, { contract }));

    // Validation errors surface through the same pure planner.
    await assert.rejects(
      () => tools.plan_verification.fn({ changedPaths: [] }),
      (e) => e instanceof AgentError && /non-empty changedPaths/.test(e.message),
    );
    await assert.rejects(
      () => tools.plan_verification.fn({ changedPaths: ['../x.rs'] }),
      (e) => e instanceof AgentError && /path traversal/.test(e.message),
    );

    assert.equal(fetchCalls, 0, 'no fetch may occur for plan_verification');
    assert.equal(fs.readFileSync(target, 'utf8'), original, 'repository files must be untouched');
    assert.deepEqual(
      fs.readdirSync(repoRoot).sort(),
      ['policy.json', 'src', 'task.json'].sort(),
      'no audit, temp, or process artifacts may appear',
    );
  } finally {
    globalThis.fetch = realFetch;
    await fsp.rm(repoRoot, { recursive: true, force: true });
  }
});
