// Offline checks for the fault-injection controls of the fake world (ticket 25 phase A).
// Run: node --test her/tests/fixtures/fake-world-faults-selfcheck.mjs
// The default (no faults) behaviour must stay byte-identical for the existing comparators; these
// tests only exercise the opt-in controls.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { developmentRoot } from '../../../scripts/fixture-paths.mjs';
import http from 'node:http';
import { test } from 'node:test';
import { startFakeWorld, mintCookie, writeCredentials, SECRET, groupLeaks, secretsInText, redactedLogHasNoSecrets } from './fake-world.mjs';

const baseScenario = (extra = {}) => ({
  selfId: 1, selfNickname: 'bot', inbound: [],
  dsh: { sessionId: 's1', workspaceId: 'w1', settings: { namespaces: [] }, turnEvents: [], presets: [], provider: 'p', model: 'm', ...extra.dsh },
  ...Object.fromEntries(Object.entries(extra).filter(([k]) => k !== 'dsh')),
});

async function withWorld(extra, fn) {
  const home = developmentRoot(path.join(os.tmpdir(), `fw-selfcheck-${randomUUID()}`));
  fs.mkdirSync(home);
  const world = await startFakeWorld({ scenario: baseScenario(extra), dshHome: path.join(home, 'dsh') });
  try { await fn(world, path.join(home, 'dsh')); } finally { await world.close(); fs.rmSync(home, { recursive: true, force: true }); }
}

const authority = (world) => new URL(world.dsh).host;
async function rpc(world, endpoint, args, { cookie = mintCookie(authority(world)) } = {}) {
  const res = await fetch(`${world.dsh}/api/${endpoint}`, {
    method: 'POST', headers: { 'content-type': 'application/json', cookie },
    body: JSON.stringify({ type: 'server-request', rpcId: 'r1', method: endpoint, payload: { args } }),
  });
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch { /* not json */ }
  return { status: res.status, json };
}
const select = (world, provider, model, effort = 'max', sessionId = 's1') => rpc(world, 'session/selectModel', { sessionId, provider, model, reasoningEffort: effort });

test('default selectModel behaviour is unchanged and recordedSelects exposes provider/model/effort', async () => {
  await withWorld({}, async (world) => {
    const r = await select(world, 'prov-a', 'model-a', 'max');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json.result, { ok: true, value: { selected: { provider: 'p', model: 'm' } } });
    assert.deepEqual(world.recordedSelects(), [{ sessionId: 's1', provider: 'prov-a', model: 'model-a', effort: 'max', ok: true }]);
  });
});

test('selectModel fault matches provider and model, honours times, and reports the rpc error', async () => {
  await withWorld({ dsh: { faults: [{ endpoint: 'session/selectModel', match: { provider: 'bad' }, times: 2, respond: { rpcError: { code: 'not-found', message: 'model not found' } } }] } }, async (world) => {
    const a = await select(world, 'bad', 'x');
    assert.equal(a.status, 200);
    assert.deepEqual(a.json.result, { ok: false, error: { code: 'not-found', message: 'model not found' } });
    assert.equal((await select(world, 'good', 'x')).json.result.ok, true, 'non-matching provider is untouched');
    assert.equal((await select(world, 'bad', 'x')).json.result.ok, false);
    assert.equal((await select(world, 'bad', 'x')).json.result.ok, true, 'fault exhausted after two uses');
    assert.deepEqual(world.recordedSelects().map((s) => [s.provider, s.ok]), [['bad', false], ['good', true], ['bad', false], ['bad', true]]);
    assert.equal(world.dshFaultsApplied(), 2);
  });
});

test('selectModel arguments wrapped as { request } (the real clients) are recorded and matched', async () => {
  await withWorld({ dsh: { faults: [{ endpoint: 'session/selectModel', match: { provider: 'bad' }, respond: { rpcError: { message: 'nope' } } }] } }, async (world) => {
    const wrapped = (provider) => rpc(world, 'session/selectModel', { request: { sessionId: 's1', provider, model: 'm', reasoningEffort: 'max' } });
    assert.equal((await wrapped('bad')).json.result.ok, false);
    assert.equal((await wrapped('good')).json.result.ok, true);
    assert.deepEqual(world.recordedSelects(), [
      { sessionId: 's1', provider: 'bad', model: 'm', effort: 'max', ok: false },
      { sessionId: 's1', provider: 'good', model: 'm', effort: 'max', ok: true },
    ]);
  });
});

test('fault can skip the first N matching calls (after) and match by function', async () => {
  await withWorld({ dsh: { faults: [{ endpoint: 'settings/describe', after: 1, times: 1, respond: { status: 503 } }, { endpoint: 'session/selectModel', match: (args) => args.model === 'm-fn', respond: { status: 500 } }] } }, async (world) => {
    assert.equal((await rpc(world, 'settings/describe', {})).status, 200);
    assert.equal((await rpc(world, 'settings/describe', {})).status, 503);
    assert.equal((await rpc(world, 'settings/describe', {})).status, 200);
    assert.equal((await select(world, 'p', 'm-fn')).status, 500);
    assert.equal((await select(world, 'p', 'other')).status, 200);
  });
});

test('fault respond status 401 and 5xx apply regardless of a valid cookie', async () => {
  await withWorld({ dsh: { faults: [{ endpoint: 'agentPresets/list', times: 1, respond: { status: 401 } }] } }, async (world) => {
    assert.equal((await rpc(world, 'agentPresets/list', {})).status, 401);
    assert.equal((await rpc(world, 'agentPresets/list', {})).status, 200);
  });
});

test('fault closeSocket resets the connection and delayMs delays the answer', async () => {
  await withWorld({ dsh: { faults: [{ endpoint: 'settings/describe', times: 1, respond: { closeSocket: true } }, { endpoint: 'agentPresets/list', times: 1, respond: { delayMs: 150 } }] } }, async (world) => {
    await assert.rejects(rpc(world, 'settings/describe', {}));
    assert.equal((await rpc(world, 'settings/describe', {})).status, 200);
    const t0 = Date.now();
    assert.equal((await rpc(world, 'agentPresets/list', {})).status, 200);
    assert.ok(Date.now() - t0 >= 140);
  });
});

test('faults can be added and cleared at runtime', async () => {
  await withWorld({}, async (world) => {
    world.addDshFault({ endpoint: 'settings/describe', respond: { status: 503 } });
    assert.equal((await rpc(world, 'settings/describe', {})).status, 503);
    world.clearDshFaults();
    assert.equal((await rpc(world, 'settings/describe', {})).status, 200);
  });
});

test('turnEventsForPrompt receives the model last selected for the session', async () => {
  const seen = [];
  await withWorld({ dsh: { uniqueSessions: false, turnEventsForPrompt: (text, ctx) => { seen.push(ctx?.selected); return []; } } }, async (world) => {
    await select(world, 'prov-a', 'model-a', 'max');
    await rpc(world, 'session/prompt', { sessionId: 's1', request: { content: [{ text: 'hi' }] } });
    await new Promise((r) => setTimeout(r, 50));
    assert.deepEqual(seen, [{ provider: 'prov-a', model: 'model-a', effort: 'max' }]);
  });
});

test('dshDown refuses connections and dshUp restores the same port', async () => {
  await withWorld({}, async (world) => {
    const before = world.dsh;
    await world.dshDown();
    await assert.rejects(rpc(world, 'settings/describe', {}));
    await world.dshUp();
    assert.equal(world.dsh, before);
    assert.equal((await rpc(world, 'settings/describe', {})).status, 200);
    assert.ok(world.log.some((e) => e.kind === 'dsh-down') && world.log.some((e) => e.kind === 'dsh-up'));
  });
});

test('rotateSecret invalidates old cookies, rewrites credentials, and the new secret authenticates', async () => {
  await withWorld({}, async (world, dshHome) => {
    const oldCookie = mintCookie(authority(world));
    const before = fs.readFileSync(path.join(dshHome, '.credentials.yaml'), 'utf8');
    world.rotateSecret();
    const after = fs.readFileSync(path.join(dshHome, '.credentials.yaml'), 'utf8');
    assert.notEqual(before, after);
    assert.equal((await rpc(world, 'settings/describe', {}, { cookie: oldCookie })).status, 401);
    assert.equal((await rpc(world, 'settings/describe', {}, { cookie: mintCookie(authority(world), world.currentSecret()) })).status, 200);
  });
  assert.equal(SECRET.length, 32, 'exported default secret is unchanged');
});

test('OneBot http fault: retcode, status and hang; per action, times limited', async () => {
  await withWorld({ onebotFaults: [{ action: 'send_private_msg', times: 1, respond: { retcode: 1200, status: 'failed', wording: 'nope' } }, { action: 'get_status', respond: { data: { online: false, good: false } } }, { action: 'send_group_msg', times: 1, respond: { httpStatus: 502 } }, { action: 'get_msg', times: 1, respond: { hang: true } }] }, async (world) => {
    const post = async (action, body) => (await fetch(`${world.onebot}/${action}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body), signal: AbortSignal.timeout(400) }));
    const a = await (await post('send_private_msg', { user_id: 1, message: 'x' })).json();
    assert.equal(a.retcode, 1200);
    const b = await (await post('send_private_msg', { user_id: 1, message: 'x' })).json();
    assert.equal(b.retcode, 0, 'second call is no longer faulted');
    assert.deepEqual((await (await post('get_status', {})).json()).data, { online: false, good: false });
    assert.equal((await post('send_group_msg', { group_id: 2, message: 'x' })).status, 502);
    await assert.rejects(post('get_msg', { message_id: 1 }));
    assert.equal(world.log.filter((e) => e.side === 'onebot' && e.kind === 'action' && e.action === 'send_private_msg').length, 2, 'faulted calls are still recorded');
  });
});

test('OneBot websocket upgrade fault answers 401 N times; closeOnebotSockets drops connected clients', async () => {
  await withWorld({}, async (world) => {
    world.setOnebotUpgradeFault({ status: 401, times: 1 });
    const upgrade = () => new Promise((resolve) => {
      const port = new URL(world.onebot).port;
      const req = http.request({ host: '127.0.0.1', port, headers: { Connection: 'Upgrade', Upgrade: 'websocket', 'Sec-WebSocket-Key': 'dGhlIHNhbXBsZSBub25jZQ==', 'Sec-WebSocket-Version': '13' } });
      req.on('response', (res) => { resolve({ status: res.statusCode, socket: null }); res.resume(); });
      req.on('upgrade', (res, socket) => resolve({ status: res.statusCode, socket }));
      req.on('error', () => resolve({ status: 0 }));
      req.end();
    });
    assert.equal((await upgrade()).status, 401);
    const ok = await upgrade();
    assert.equal(ok.status, 101);
    let closed = false;
    ok.socket.on('close', () => { closed = true; });
    assert.equal(world.closeOnebotSockets({ code: 1006 }), 1);
    await new Promise((r) => setTimeout(r, 100));
    assert.ok(closed);
    assert.deepEqual(world.log.filter((e) => e.kind === 'onebot-upgrade-refused').map((e) => e.status), [401]);
  });
});

test('group leak scan flags internal error, restart and model text sent to a group but not owner private alerts', () => {
  const log = [
    { side: 'onebot', kind: 'action', action: 'send_group_msg', params: { group_id: 1, message: '正常回复' } },
    { side: 'onebot', kind: 'action', action: 'send_group_msg', params: { group_id: 1, message: '⚠️ agent 处理出错（group:1）：boom' } },
    { side: 'onebot', kind: 'action', action: 'send_group_msg', params: { group_id: 1, message: [{ type: 'text', data: { text: '模型调用失败 deepseek-official/deepseek-flash' } }] } },
    { side: 'onebot', kind: 'action', action: 'send_private_msg', params: { user_id: 8808, message: '⚠️ agent 处理出错（group:1）：boom' } },
    { side: 'onebot', kind: 'action', action: 'send_group_msg', params: { group_id: 1, message: 'bridge restarted' } },
  ];
  const leaks = groupLeaks(log);
  assert.deepEqual(leaks.map((l) => l.params.group_id), [1, 1, 1]);
  assert.equal(groupLeaks(log, { extra: [/正常/] }).length, 4);
  assert.deepEqual(groupLeaks([log[0], log[3]]), []);
});

test('secret scan finds fixture tokens, cookie values and paths in text; clean text passes', () => {
  const dirty = 'token=parity-onebot-token cookie dsh-auth-abc=v1.x.y /root/secret/file C:\\qqbot\\config.json';
  const found = secretsInText(dirty, { extra: ['hello'] });
  assert.ok(found.includes('parity-onebot-token') && found.some((f) => f.startsWith('dsh-auth-')) && found.some((f) => f.includes('/root/')) && found.some((f) => /qqbot/i.test(f)));
  assert.deepEqual(secretsInText('component=dsh event=down outcome=failed reason=connection refused'), []);
  const dir = developmentRoot(path.join(os.tmpdir(), `fw-scan-${randomUUID()}`));
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'a.jsonl'), '{"reason":"ok"}\n');
  fs.writeFileSync(path.join(dir, 'b.log'), 'leaked parity-agent-token');
  const res = redactedLogHasNoSecrets([path.join(dir, 'a.jsonl'), path.join(dir, 'b.log')]);
  assert.equal(res.clean, false);
  assert.deepEqual(res.hits.map((h) => path.basename(h.file)), ['b.log']);
  fs.rmSync(dir, { recursive: true, force: true });
});

test('an endpoint "*" fault answers every DSH endpoint until it is cleared (ticket 25 phase B outage)', async () => {
  await withWorld({ dsh: { faults: [{ endpoint: '*', respond: { status: 503, body: '{"error":"down"}' } }] } }, async (world) => {
    assert.equal((await rpc(world, 'settings/describe', {})).status, 503);
    assert.equal((await select(world, 'p', 'm')).status, 503);
    world.clearDshFaults();
    assert.equal((await rpc(world, 'settings/describe', {})).status, 200);
  });
});

test('OneBot bearer tokens are enforced only when configured: HTTP action and WebSocket upgrade', async () => {
  await withWorld({ onebotTokens: { http: 'http-ok', ws: 'ws-ok' } }, async (world) => {
    const call = (token) => fetch(`${world.onebot}/get_status`, { method: 'POST', headers: { 'content-type': 'application/json', ...(token ? { authorization: `Bearer ${token}` } : {}) }, body: '{}' });
    assert.equal((await call('stale')).status, 401);
    assert.equal((await call('http-ok')).status, 200);
    const upgrade = (token) => new Promise((resolve) => {
      const req = http.request(`${world.onebot}/`, { headers: { connection: 'Upgrade', upgrade: 'websocket', 'sec-websocket-key': 'dGhlIHNhbXBsZSBub25jZQ==', 'sec-websocket-version': '13', ...(token ? { authorization: `Bearer ${token}` } : {}) } });
      req.on('response', (res) => { resolve({ status: res.statusCode }); res.resume(); });
      req.on('upgrade', (res, socket) => { resolve({ status: res.statusCode, socket }); });
      req.on('error', () => resolve({ status: 0 }));
      req.end();
    });
    assert.equal((await upgrade('http-ok')).status, 401, 'the HTTP token is not the WebSocket token');
    const ok = await upgrade('ws-ok');
    assert.equal(ok.status, 101);
    ok.socket.destroy();
    world.setOnebotTokens(null);
    assert.equal((await call('anything')).status, 200, 'enforcement stops when cleared');
  });
});
