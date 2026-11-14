// Disposable DSH process boundary: generated MCP children and the actual digest entry.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import readline from 'node:readline';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { developmentRoot } from './fixture-paths.mjs';

const [exe, runtime, root, support, port] = process.argv.slice(2);
developmentRoot(runtime, exe);
developmentRoot(root, exe);

const require = createRequire(path.join(support, 'package.json'));
const yaml = require('js-yaml');
const children = new Set();
const run = (program, args, env = process.env) => new Promise((resolve, reject) => {
  const child = spawn(program, args, { windowsHide: true, env, stdio: ['ignore', 'pipe', 'pipe'] });
  children.add(child);
  let out = '', err = '';
  child.stdout.on('data', b => out += b); child.stderr.on('data', b => err += b);
  const timer = setTimeout(() => { child.kill(); reject(Error('fixture child timeout')); }, 30000);
  child.on('error', reject);
  child.on('close', code => { clearTimeout(timer); children.delete(child); code === 0 ? resolve(out) : reject(Error(`child exit ${code}: ${err}`)); });
});
let prompts = 0;
const server = http.createServer((req, res) => {
  const chunks = [];
  req.on('data', b => chunks.push(b));
  req.on('end', () => {
    if (req.url === '/api/status' || req.url === '/get_login_info') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(req.url === '/api/status' ? { mode: 'chat', dshReady: true } : { status: 'ok', retcode: 0, data: { user_id: 1, nickname: 'synthetic' } }));
      return;
    }
    if (req.url === '/fixture-web') {
      setTimeout(() => { res.writeHead(200, { 'content-type': 'text/html' }); res.end('<html><body>synthetic release web content</body></html>'); }, 1000);
      return;
    }
    if (req.url === '/v1/chat/completions') {
      prompts++;
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ id: 'synthetic', object: 'chat.completion', created: 1, model: 'synthetic-digest-model',
        choices: [{ index: 0, message: { role: 'assistant', content: JSON.stringify([{ subject: 'synthetic', content: 'synthetic digest memory' }]) }, finish_reason: 'stop' }],
        usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } }));
      return;
    }
    res.writeHead(400); res.end('{}'); return;
  });
});
await new Promise(r => server.listen(process.argv.includes('--fake-console') ? Number(port) : 0, '127.0.0.1', r));
const endpoint = `http://127.0.0.1:${server.address().port}`;
if (process.argv.includes('--fake-console')) {
  fs.writeFileSync(path.join(runtime, 'dependencies.json'), JSON.stringify({ dsh: endpoint, model: endpoint, onebot: endpoint, onebot_ws: endpoint.replace('http:', 'ws:') }));
}
const mcp = [];
try {
  const profile = path.join(root, 'generated-mcp.yml');
  await run(exe, ['dsh-mcp-config', '--patch-file', profile, '--bridge-exe', exe, '--runtime-root', runtime, '--console-port', port]);
  const entries = yaml.load(profile ? fs.readFileSync(profile, 'utf8') : '').flatMap(row => row.insert);
  assert.equal(entries.length, 3);
  for (const { config } of entries) {
    const env = { ...process.env, PATH: path.join(root, 'fake-curl') + path.delimiter + process.env.PATH,
      RELPROOF_CURL_URL: endpoint + '/fixture-web' };
    assert(config.args.includes(runtime) && config.args.includes(port));
    const child = spawn(config.command, config.args, { windowsHide: true, env, cwd: path.dirname(config.command), stdio: ['pipe', 'pipe', 'pipe'] });
    children.add(child);
    let diagnostics = '';
    child.stderr.on('data', b => diagnostics = (diagnostics + b).slice(-4000));
    const lines = readline.createInterface({ input: child.stdout })[Symbol.asyncIterator]();
    let id = 0;
    const request = async (method, params) => {
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: ++id, method, params }) + '\n');
      let timer;
      console.log(`MCP request ${config.serverName}: ${method}`);
      const answer = await Promise.race([lines.next(), new Promise((_, reject) => { timer = setTimeout(() => reject(Error(`MCP response timeout: ${config.serverName}/${method}; ${diagnostics}`)), 20000); })]).finally(() => clearTimeout(timer));
      assert(!answer.done); const row = JSON.parse(answer.value); assert.equal(row.id, id); assert(!row.error, JSON.stringify(row)); return row.result;
    };
    await request('initialize', { protocolVersion: '2025-03-26', capabilities: {}, clientInfo: { name: 'fake-dsh-release', version: '1' } });
    const list = await request('tools/list', {}); assert(list.tools.length > 0);
    const name = { snowluma: 'qq_status', 'snowluma-host': 'snowluma_status', 'web-search-safe': 'web_fetch' }[config.serverName];
    const answer = await request('tools/call', { name, arguments: name === 'web_fetch' ? { url: 'http://93.184.215.14/' } : {} });
    assert(!answer.isError, JSON.stringify(answer));
    if (name === 'web_fetch') assert(JSON.stringify(answer).includes('synthetic release web content'), JSON.stringify(answer));
    child.stdin.end();
    const code = await new Promise(resolve => child.once('close', resolve)); assert.equal(code, 0); children.delete(child);
    mcp.push(config.serverName);
  }
  const selected = path.join(root, 'digest-runtime');
  for (const dir of ['config', 'data', 'resources', 'user/dsh']) fs.mkdirSync(path.join(selected, dir), { recursive: true });
  const reservation = http.createServer();
  await new Promise(r => reservation.listen(0, '127.0.0.1', r));
  const digestPort = reservation.address().port;
  await new Promise(r => reservation.close(r));
  const token = 'synthetic-digest-console';
  fs.writeFileSync(path.join(selected, 'config/config.json'), JSON.stringify({ consoleToken: token, consolePort: digestPort,
    memory: { autoFlush: { enabled: true } }, harness: { model: 'synthetic-digest-model' }, tasks: { nodePath: path.join(selected, 'missing-node') } }));
  fs.writeFileSync(path.join(selected, 'config/harness-api-key'), 'synthetic-digest-key');
  fs.writeFileSync(path.join(selected, 'data/social-v2.json'), JSON.stringify({ conversations: { 'group:7': { agentToken: 'synthetic', recentMessages: [{ seq: 1, sender: 'synthetic', userId: '5', text: 'synthetic digest evidence', isSelf: false }] } } }));
  fs.writeFileSync(path.join(selected, 'dependencies.json'), JSON.stringify({ dsh: endpoint, model: endpoint, onebot: endpoint }));
  fs.copyFileSync(path.join(runtime, 'user/dsh/.credentials.yaml'), path.join(selected, 'user/dsh/.credentials.yaml'));
  const consoleChild = spawn(exe, ['--console-serve', '--console-port', String(digestPort), '--runtime-root', selected],
    { windowsHide: true, stdio: 'ignore' });
  children.add(consoleChild);
  let consoleExit;
  consoleChild.on('error', error => { consoleExit = error.message; });
  consoleChild.on('close', code => { consoleExit = code; children.delete(consoleChild); });
  const consoleRequest = (method, route, body, extra = {}) => new Promise((resolve, reject) => {
    const text = body ? JSON.stringify(body) : '';
    const request = http.request({ hostname: '127.0.0.1', port: digestPort, path: route, method,
      headers: { 'x-console-token': token, Origin: `http://127.0.0.1:${digestPort}`, 'content-type': 'application/json',
        'content-length': Buffer.byteLength(text), ...extra }, timeout: 2000 }, response => {
      let text = ''; response.on('data', b => text += b);
      response.on('end', () => {
        try { assert(response.statusCode >= 200 && response.statusCode < 300, text); resolve(JSON.parse(text)); }
        catch (error) { reject(error); }
      });
    });
    request.on('timeout', () => request.destroy(Error('digest console request timeout')));
    request.on('error', reject); request.end(text);
  });
  const waitFor = async fn => {
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline) {
      assert.equal(consoleExit, undefined, `digest console exited: ${consoleExit}`);
      const result = await fn(); if (result) return result;
      await new Promise(r => setTimeout(r, 50));
    }
    throw Error('digest fixture timeout');
  };
  await waitFor(() => consoleRequest('GET', '/api/health').catch(() => null));
  const started = await consoleRequest('POST', '/api/tasks/memory-digest/run', { requestId: 'release-digest-001' });
  const completed = await waitFor(async () => {
    const { run } = await consoleRequest('GET', `/api/tasks/runs/${started.runId}`);
    return ['queued', 'running'].includes(run.state) ? null : run;
  });
  assert.equal(completed.state, 'succeeded'); assert.equal(completed.pid, null);
  assert.equal(prompts, 1);
    const recalled = await consoleRequest('GET', '/api/memory/search?key=group%3A7&query=synthetic', undefined, { 'x-agent-token': 'synthetic' });
    assert(JSON.stringify(recalled.memories).includes('synthetic digest memory'), 'the digest stored no memory');
  fs.writeFileSync(path.join(root, 'chains.json'), JSON.stringify({ mcp, digestPrompts: prompts }));
  console.log('PASS generated MCP initialize/list/call, native curl through a loopback adapter, and actual digest model call on a fake relay');
} finally {
  await Promise.all([...children].map(child => new Promise(resolve => { child.once('close', resolve); child.kill(); })));
  server.closeAllConnections(); await new Promise(r => server.close(r));
}
