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
  AuditLog,
  boundOutput,
  parseSse,
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
      ['durationMs', 'error', 'inputBytes', 'kind', 'ok', 'outputBytes', 'tool', 'ts'].sort(),
    );
    assert.equal(line.error, 'patch_mismatch');
    // The audit line must not carry any free-form content fields.
    assert.ok(!('content' in line));
    assert.ok(!('task' in line));
    assert.ok(!('patch' in line));
    assert.ok(!('source' in line));
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
