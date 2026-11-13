#!/usr/bin/env node
// Ticket 25 phase B: Rust-only behaviour of the dependency-recovery work, through the fault-injecting fake world
// (fake-world.mjs) and the real candidate processes (live bridge + console). Run by
// `her/tests/dependency_recovery_e2e.rs`, one case per Rust test, or by hand:
//
//   node her/tests/fixtures/dependency-recovery-cases.mjs --candidate <her> [--case <name>] [--out <dir>]
//
// A case passes when every assertion holds; the exit code is 0 only then. Cases poll the fake world's log, the run
// record and the ledger; there are no fixed sleeps except where a bounded absence ("nothing else arrives") is the claim.
import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import { developmentRoot } from '../../../scripts/fixture-paths.mjs';
import { pathToFileURL } from 'node:url';
import {
  startRustSide, httpCall, sleep, waitFor, dshCalls, privateTextsTo, groupLeaksOf, scanSideForSecrets, readState,
  CONSOLE_TOKEN, GROUP, GROUP_KEY, GROUP2_KEY, OWNER, OWNER_KEY, TOKENS, convRecord, defaultWake, onebotResponder, baseConfig,
} from './rust-world.mjs';

const EPOCH = 1760000000000;
let nextId = 25000;
const groupMessage = (text, user = 10001) => ({
  post_type: 'message', message_type: 'group', sub_type: 'normal', message_id: ++nextId, group_id: GROUP, user_id: user, self_id: 401,
  time: Math.floor(Date.now() / 1000), sender: { user_id: user, nickname: 'tester', card: '' }, message: [{ type: 'text', data: { text } }], raw_message: text,
});
const turnEvents = (turn, end) => [
  { seq: turn * 10 + 1, type: 'turn/start', data: { turn } },
  ...(end.kind === 'completed' ? [{ seq: turn * 10 + 2, type: 'assistant/message', data: { turn, step: 1, provider: 'p', message: { role: 'assistant', content: [{ type: 'text', text: 'synthetic reply' }] } } }] : []),
  { seq: turn * 10 + 3, type: 'turn/end', data: { turn, reason: end } },
];
const completed = (turn) => turnEvents(turn, { kind: 'completed' });
const failed = (turn, code, message) => turnEvents(turn, { kind: 'error', error: { code, message } });
const wakeState = () => ({
  paused: false,
  conversations: {
    [GROUP_KEY]: convRecord(GROUP_KEY, { wakeConfig: defaultWake({ mode: 'active', batchWindowMs: 300, triggers: { ...defaultWake().triggers, anyMessage: true } }) }),
    [GROUP2_KEY]: convRecord(GROUP2_KEY),
    [OWNER_KEY]: convRecord(OWNER_KEY),
  },
});
const prompts = (side) => side.world.log.filter((e) => e.side === 'dsh' && e.kind === 'prompt-received');
const promptsWith = (side, marker) => prompts(side).filter((e) => String(e.text).includes(marker));
const api = (side, p, extra = {}) => httpCall(side, { path: p, headers: { 'x-console-token': CONSOLE_TOKEN }, ...extra });
const records = async (side, query = '') => (await api(side, `/api/recovery-record${query}`)).json?.records ?? [];
const recordFile = (side) => path.join(side.dataDir, 'run-record.jsonl');
const turns = async (side) => (await api(side, '/api/turns')).json?.turns ?? [];
const push = (side, event) => assert.equal(side.world.pushInbound(event), 1, 'one connected bridge required');
const until = async (what, predicate, timeoutMs = 40_000) => assert.ok(await waitFor(predicate, { timeoutMs }), `timeout waiting for: ${what}`);
const base = (over = {}) => ({
  mode: 'reserved2', state: wakeState(), responder: onebotResponder, config: { ackMessage: '' },
  turnEventsForPrompt: () => completed(1), ...over,
});

const modelAlert = (provider = 'pa', model = 'ma', cls = 'backend', code = 'upstream') => `[group:420042] 模型/后端回合失败：backend=dsh provider=${provider} model=${model} class=${cls} code=${code}`;

const CASES = {
  // 401 after a DSH restart changed the signing secret: every call heals once, nothing is retried more than once, and the
  // prompt is delivered exactly once.
  async 'reauth-401-rotation-call-counts'(ctx) {
    const side = await ctx.start(base({ turnEventsForPrompt: () => completed(1) }));
    // Every turn builds its own client from the credentials as they are at its start, so a 401 happens when the secret
    // changes while a turn is in flight: hold the model selection, rotate the secret, let the turn continue.
    side.world.addDshFault({ endpoint: 'session/selectModel', respond: { delayMs: 1500 }, times: 1 });
    push(side, groupMessage('MARK-A first'));
    await until('the selection is in flight', () => dshCalls(side, 'session/selectModel').length >= 1);
    side.world.rotateSecret();
    await until('the prompt reached DSH after the rotation', () => promptsWith(side, 'MARK-A').length >= 1);
    await until('the turn ended', async () => (await turns(side)).some((t) => ['completed', 'failed'].includes(t.state)));
    const calls = side.world.log.filter((e) => e.side === 'dsh' && e.kind === 'rpc');
    const invalid = calls.filter((e) => !e.cookieValid);
    const validPrompts = calls.filter((e) => e.endpoint === 'session/prompt' && e.cookieValid && JSON.stringify(e.args).includes('MARK-A'));
    assert.ok(invalid.length >= 1, 'the rotation was observed as a 401');
    assert.equal(validPrompts.length, 1, 'the prompt is delivered exactly once after the 401');
    assert.ok(invalid.length <= 3, `a 401 storm: ${invalid.map((e) => e.endpoint).join(',')}`);
    for (const e of invalid) {
      assert.equal(invalid.filter((x) => x.endpoint === e.endpoint).length, 1, `${e.endpoint} was refused more than once`);
      assert.ok(calls.some((v) => v.endpoint === e.endpoint && v.cookieValid && v.seq > e.seq), `${e.endpoint} was refused and never repeated with the new secret`);
    }
    ctx.note({ refusedThenRepeated: invalid.map((e) => e.endpoint) });
    assert.ok((await records(side)).some((r) => r.component === 'dsh' && r.event === 'reauth-401'), 'the re-authentication is on the run record');
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },

  // A DSH outage: the message waits (bounded calls), the recovery is one alert and one record, the replay goes through
  // the access checks again (a conversation revoked meanwhile gets nothing), and the prompt is delivered once.
  async 'dsh-outage-queues-then-replays-through-the-access-checks'(ctx) {
    const side = await ctx.start(base({ mode: 'chat', state: { conversations: {} }, turnEventsForPrompt: () => completed(1), configure: (config) => { config.socialV2.turnTimeoutSecs = 20; } }));
    // outage: every DSH call answers 503
    side.world.addDshFault({ endpoint: '*', respond: { status: 503, body: '{"error":"down"}' } });
    await until('the bridge noticed the outage', async () => (await api(side, '/api/status')).json?.dshReady === false, 30_000);
    const mark = side.world.log.length;
    push(side, groupMessage('queued-while-down one'));
    push(side, groupMessage('queued-while-down two', 10002));
    await until('both messages waited (no prompt reached DSH)', () => dshCalls(side, 'session/prompt').length === 0 && /\[chat\] group:420042 DSH 不可用，消息排队等待恢复/.test(side.stdout()), 20_000);
    // bounded call count over the outage window (about 12 s): probes only
    await sleep(12_000);
    const calls = side.world.log.slice(mark).filter((e) => e.side === 'dsh' && e.kind === 'rpc');
    assert.ok(calls.length <= 8, `a request storm during the outage: ${calls.length} DSH calls (${calls.map((e) => e.endpoint).join(',')})`);
    assert.equal(dshCalls(side, 'session/prompt').length, 0, 'nothing is submitted while DSH is known down');
    // revoke the second sender's conversation access while waiting: allow-list change through the administrator route
    // (the whole conversation is the group, so revoke a different thing: switch the mode away and back is not needed -
    // revoke the group itself in a second run of the case below)
    side.world.clearDshFaults();
    await until('DSH reported ready again', async () => (await api(side, '/api/status')).json?.dshReady === true, 30_000);
    await until('the queued messages were replayed', () => prompts(side).length >= 1, 40_000);
    const recs = await records(side);
    assert.ok(recs.some((r) => r.component === 'dsh' && r.event === 'down' && r.outcome === 'failed'), 'the down edge is on the record');
    assert.equal(recs.filter((r) => r.component === 'dsh' && r.event === 'down' && !r.repeat).length, 1, 'one down edge, not one line per probe');
    assert.ok(recs.some((r) => r.component === 'dsh' && r.event === 'up' && r.outcome === 'ok'), 'the up edge is on the record');
    await until('the recovery alert reached the administrator', () => privateTextsTo(side).some((t) => t.includes('DSH 已恢复')), 20_000);
    assert.equal(privateTextsTo(side).filter((t) => t.includes('DSH 已恢复')).length, 1, 'one recovery alert');
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },

  async 'dsh-outage-replay-honours-a-revocation-made-while-down'(ctx) {
    const side = await ctx.start(base({ mode: 'chat', state: { conversations: {} }, turnEventsForPrompt: () => completed(1) }));
    side.world.addDshFault({ endpoint: '*', respond: { status: 503, body: '{"error":"down"}' } });
    await until('the bridge noticed the outage', async () => (await api(side, '/api/status')).json?.dshReady === false, 30_000);
    push(side, groupMessage('revoked-while-down'));
    await until('the message waits in the conversation worker', () => /\[chat\] group:420042 DSH 不可用，消息排队等待恢复/.test(side.stdout()), 20_000);
    // The administrator removes the group from the allow list while DSH is down.
    const cfg = JSON.parse(fs.readFileSync(side.configPath, 'utf8'));
    cfg.allow = { private: cfg.allow.private, groups: [] };
    const r = await httpCall(side, { method: 'POST', path: '/api/whitelist', headers: { 'x-console-token': CONSOLE_TOKEN }, body: { allow: { private: cfg.allow.private, groups: [] } } });
    assert.equal(r.status, 200, `whitelist change refused: ${r.status} ${JSON.stringify(r.json)}`);
    side.world.clearDshFaults();
    await until('DSH ready again', async () => (await api(side, '/api/status')).json?.dshReady === true, 30_000);
    // the replay is checked and dropped: the revoked conversation never reaches the model; bounded absence after recovery
    await until('the replay was refused by the access check', () => /补投排队的消息[\s\S]*丢弃/.test(side.stdout()), 20_000);
    assert.equal(promptsWith(side, 'revoked-while-down').length, 0, 'a revoked conversation got its old message delivered after recovery');
    assert.equal(prompts(side).length, 0, 'nothing at all was submitted');
  },

  // First-generation reserved mode (JS: queued while dshReady is false, replayed on recovery): while DSH is known down no call is
  // made for the waiting message; after recovery it goes through the authority re-check and is delivered exactly once.
  async 'reserved-outage-waits-for-dsh-then-replays'(ctx) {
    const social = { triggerProbability: 0, proactiveEnabled: false, activeDurationEnabled: false, activeCheckMinMs: 0, activeCheckMaxMs: 0,
      activeReplyDelayMinMs: 0, activeReplyDelayMaxMs: 0, idleWindowMs: 120000, idleRetryProbability: 0, skipProbability: 0, longGapProbability: 0,
      burstIntervalMinMs: 1, burstIntervalMaxMs: 1 };
    const side = await ctx.start({ mode: 'reserved', state: { conversations: {} }, responder: onebotResponder, config: { ackMessage: '', social }, turnEventsForPrompt: () => completed(1) });
    side.world.addDshFault({ endpoint: '*', respond: { status: 503, body: '{"error":"down"}' } });
    await until('the bridge noticed the outage', async () => (await api(side, '/api/status')).json?.dshReady === false, 30_000);
    const mark = side.world.log.length;
    push(side, groupMessage('@401 reserved-waits-for-dsh'));
    await until('the message waits for DSH', () => /\[reserved\] group:420042 DSH 不可用，消息排队等待恢复/.test(side.stdout()), 20_000);
    await sleep(8_000);
    const calls = side.world.log.slice(mark).filter((e) => e.side === 'dsh' && e.kind === 'rpc' && e.endpoint !== 'settings/describe');
    assert.equal(calls.length, 0, `DSH was called for waiting work: ${calls.map((e) => e.endpoint).join(',')}`);
    side.world.clearDshFaults();
    await until('DSH reported ready again', async () => (await api(side, '/api/status')).json?.dshReady === true, 30_000);
    await until('the waiting message was delivered', () => promptsWith(side, 'reserved-waits-for-dsh').length >= 1, 40_000);
    await until('the turn ended', async () => (await turns(side)).some((t) => t.state === 'completed'), 40_000);
    await sleep(1500);
    assert.equal(promptsWith(side, 'reserved-waits-for-dsh').length, 1, 'delivered exactly once');
    assert.ok(side.world.log.some((e) => e.side === 'onebot' && e.action === 'send_group_msg' && JSON.stringify(e.params).includes('synthetic reply')), 'the reply reached the group');
    assert.deepEqual(groupLeaksOf(side).filter((t) => !String(t).includes('synthetic reply')), [], 'no internal text in the group');
  },

  // Reserved mode 2: a wake that falls due while DSH is known down is not dispatched (no call at all) and is delivered once after recovery.
  async 'reserved2-outage-waits-for-dsh-then-replays'(ctx) {
    const side = await ctx.start(base({ turnEventsForPrompt: () => completed(1) }));
    side.world.addDshFault({ endpoint: '*', respond: { status: 503, body: '{"error":"down"}' } });
    await until('the bridge noticed the outage', async () => (await api(side, '/api/status')).json?.dshReady === false, 30_000);
    const mark = side.world.log.length;
    push(side, groupMessage('MARK-R2 waits for dsh'));
    await until('the wake waits for DSH', () => /\[reserved2\] group:420042 DSH 不可用，唤醒排队等待恢复/.test(side.stdout()), 20_000);
    await sleep(8_000);
    const calls = side.world.log.slice(mark).filter((e) => e.side === 'dsh' && e.kind === 'rpc' && e.endpoint !== 'settings/describe');
    assert.equal(calls.length, 0, `DSH was called for waiting work: ${calls.map((e) => e.endpoint).join(',')}`);
    assert.equal(promptsWith(side, 'MARK-R2').length, 0, 'nothing is submitted while DSH is known down');
    side.world.clearDshFaults();
    await until('DSH reported ready again', async () => (await api(side, '/api/status')).json?.dshReady === true, 30_000);
    await until('the waiting wake was delivered', () => promptsWith(side, 'MARK-R2').length >= 1, 40_000);
    await until('the turn ended', async () => (await turns(side)).some((t) => t.state === 'completed'), 40_000);
    await sleep(1500);
    assert.equal(promptsWith(side, 'MARK-R2').length, 1, 'delivered exactly once');
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },

  // An explicit CLI choice is independent of DSH, including when it is added after a wake was parked.
  async 'reserved2-cli-choice-releases-waiting-work-during-dsh-outage'(ctx) {
    const side = await ctx.start(base());
    side.world.addDshFault({ endpoint: '*', respond: { status: 503, body: '{"error":"down"}' } });
    await until('the bridge noticed the outage', async () => (await api(side, '/api/status')).json?.dshReady === false, 30_000);
    push(side, groupMessage('MARK-CLI waiting wake'));
    await until('the DSH-dependent wake was parked', () => /\[reserved2\] group:420042 DSH 不可用，唤醒排队等待恢复/.test(side.stdout()), 20_000);
    const mark = side.world.log.length;
    const config = JSON.parse(fs.readFileSync(side.configPath, 'utf8'));
    config.backends = {
      sessions: { [GROUP_KEY]: 'missing-cli' },
      list: [{ id: 'missing-cli', kind: 'cli', cmd: 'her-test-command-that-does-not-exist' }],
    };
    const updatedConfig = `${side.configPath}.test-tmp`;
    fs.writeFileSync(updatedConfig, JSON.stringify(config));
    fs.renameSync(updatedConfig, side.configPath);
    await until('the selected CLI was attempted', () => {
      const log = path.join(side.dataDir, 'logs', 'opencode-trial.jsonl');
      return fs.existsSync(log) && fs.readFileSync(log, 'utf8').includes('missing-cli');
    }, 20_000);
    await until('the failed CLI turn was settled', async () => (await turns(side)).some((t) => t.key === GROUP_KEY && t.state === 'failed'));
    const calls = side.world.log.slice(mark).filter((e) => e.side === 'dsh' && e.kind === 'rpc' && e.endpoint !== 'settings/describe');
    assert.equal(calls.length, 0, `a failed CLI fell back to unavailable DSH: ${calls.map((e) => e.endpoint).join(',')}`);
    assert.equal(promptsWith(side, 'MARK-CLI waiting wake').length, 0);
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },

  // Q3: a reply timeout never re-delivers the whole prompt to the next candidate; the turn settles per ticket 10.
  async 'turn-timeout-never-redelivers-the-prompt'(ctx) {
    const side = await ctx.start(base({
      // The short collector override belongs to chat; reserved2 derives its collector from its cap.
      mode: 'chat',
      dsh: { provider: 'pa', model: 'ma', reasoningEffort: 'low', fallbacks: [{ provider: 'pb', model: 'mb' }] },
      configure: (config) => { config.socialV2.turnTimeoutSecs = 3; },
      turnEventsForPrompt: () => [], // accepted, then silence: the reply times out
    }));
    push(side, groupMessage('timeout-once'));
    await until('the prompt was accepted', () => promptsWith(side, 'timeout-once').length >= 1);
    await until('the turn settled', async () => (await turns(side)).some((t) => ['failed', 'abandoned', 'paused'].includes(t.state)), 40_000);
    const rotatedToken = readState(side)?.conversations?.[GROUP_KEY]?.agentToken;
    assert.ok(typeof rotatedToken === 'string' && rotatedToken.length > 0, 'the selected conversation must retain its new epoch');
    assert.notEqual(rotatedToken, TOKENS[GROUP_KEY], 'the abandoned collector epoch must be revoked');
    assert.equal(promptsWith(side, 'timeout-once').length, 1, 'the prompt was delivered to a second candidate after a timeout');
    const selects = side.world.recordedSelects().map((s) => `${s.provider}/${s.model}`);
    assert.ok(!selects.includes('pb/mb'), `the fallback candidate was selected after a timeout: ${selects}`);
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
    // The abandoned attempt's token is dead: a late send with it is refused and reaches no group.
    const late = await httpCall(side, { method: 'POST', path: '/api/socialV2/send-message', consoleAuth: false, headers: { 'x-agent-token': TOKENS[GROUP_KEY] }, body: { key: GROUP_KEY, text: 'late message from the abandoned attempt' } });
    assert.ok([401, 403].includes(late.status), `a late send with the abandoned attempt's token answered ${late.status} ${JSON.stringify(late.json)}`);
    assert.ok(!side.world.log.some((e) => e.side === 'onebot' && e.action === 'send_group_msg' && JSON.stringify(e.params).includes('late message')), 'the late send reached the group');
    const rec = await records(side);
    assert.ok(rec.some((r) => r.component === 'model-chain' && ['turn-abandoned', 'turn-unverified'].includes(r.event)), 'the abandonment is on the record');
  },

  // A model error fails over and sends one content-free administrator alert; the group sees nothing,
  // and no secret reaches the run record, the activity log or the process output.
  async 'model-error-fails-over-with-alerts-and-no-leaks'(ctx) {
    const side = await ctx.start(base({
      dsh: { provider: 'pa', model: 'ma', reasoningEffort: 'low', fallbacks: [{ provider: 'pb', model: 'mb' }] },
      turnEventsForPrompt: (text, { selected }) => (selected?.provider === 'pa' ? failed(1, 'upstream', `provider exploded Bearer abcdefabcdefghijabcdefabcdefghij at /root/secret/place`) : completed(2)),
    }));
    push(side, groupMessage('please fail once'));
    await until('the fallback delivered the prompt', () => promptsWith(side, 'please fail once').length >= 2);
    await until('one metadata-only owner alert', () => privateTextsTo(side).includes(modelAlert()));
    const texts = privateTextsTo(side);
    assert.deepEqual(texts, [modelAlert()], 'one alert with metadata, no provider error text');
    assert.ok(!texts.some((t) => t.includes('abcdefabcdefghijabcdef') || t.includes('/root/secret')), 'a credential or path reached the administrator');
    assert.deepEqual(groupLeaksOf(side), [], 'the group saw internal text');
    const scan = scanSideForSecrets(side, { files: [recordFile(side), path.join(side.dataDir, 'qq-activity.log')], extra: ['abcdefabcdefghijabcdefabcdefghij'] });
    assert.ok(scan.clean, `secrets in logs/records: ${JSON.stringify(scan)}`);
  },

  // Every candidate fails: report one conversation/class alert and retain the full local chain evidence.
  async 'all-candidates-fail-exhausted-alert-and-texts'(ctx) {
    const side = await ctx.start(base({
      dsh: { provider: 'pa', model: 'ma', reasoningEffort: 'low', fallbacks: [{ provider: 'pb', model: 'mb' }] },
      turnEventsForPrompt: (text, { selected }) => (text.includes('【提醒】') ? completed(3) : failed(1, 'upstream', 'everything exploded')),
    }));
    push(side, groupMessage('fail all'));
    await until('the chain ended failed', async () => (await turns(side)).some(t => t.state === 'failed'));
    await until('one private alert', () => privateTextsTo(side).length >= 1);
    await sleep(2500); // bounded absence: no duplicate alert from the exhausted candidate
    assert.deepEqual(privateTextsTo(side), [modelAlert()], 'one content-free alert for this conversation/class');
    assert.deepEqual(side.world.recordedSelects().map((s) => `${s.provider}/${s.model}`), ['pa/ma', 'pb/mb']);
    const recs = await records(side, '?limit=200');
    assert.ok(recs.some((r) => r.component === 'model-chain' && r.event === 'failover-step'), 'the step is on the record');
    assert.ok(recs.some((r) => r.component === 'model-chain' && r.event === 'candidates-exhausted'), 'the exhaustion is on the record');
    assert.ok(recs.some((r) => r.component === 'alert' && r.event === 'owner-alert-model-failure-group-420042-backend' && r.outcome === 'skipped'), 'the deduplicated message is on the record');
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },

  // Two failures of one class inside a minute share one private metadata alert.
  async 'second-failover-step-alert-is-deduplicated-inside-a-minute'(ctx) {
    const side = await ctx.start(base({
      dsh: { provider: 'pa', model: 'ma', reasoningEffort: 'low', fallbacks: [{ provider: 'pb', model: 'mb' }, { provider: 'pc', model: 'mc' }] },
      turnEventsForPrompt: (text, { selected }) => (text.includes('【提醒】') ? completed(3) : ['pa', 'pb'].includes(selected?.provider) ? failed(1, 'upstream', 'twice exploded') : completed(2)),
    }));
    push(side, groupMessage('fail twice'));
    await until('one administrator message', () => privateTextsTo(side).length >= 1);
    await until('both failed attempts were reported', async () => (await records(side, '?limit=200')).filter((r) => r.event === 'turn-error').length >= 2);
    await sleep(2500);
    assert.deepEqual(privateTextsTo(side), [modelAlert()], 'same class within the window is deduplicated');
    assert.deepEqual(side.world.recordedSelects().map((s) => `${s.provider}/${s.model}`), ['pa/ma', 'pb/mb', 'pc/mc'], 'the chain was still walked');
    assert.ok((await records(side, '?limit=200')).filter((r) => r.event === 'failover-step').length >= 1);
  },

  // A failed fallback selection closes the turn and retains exhaustion evidence without a duplicate class alert.
  async 'selection-failure-after-a-failover-step-sends-the-exhausted-alert'(ctx) {
    const side = await ctx.start(base({
      dsh: { provider: 'pa', model: 'ma', reasoningEffort: 'low', fallbacks: [{ provider: 'pb', model: 'mb' }] },
      turnEventsForPrompt: (text, { selected }) => (selected?.provider === 'pa' ? failed(1, 'upstream', 'primary exploded') : completed(2)),
    }));
    side.world.addDshFault({ endpoint: 'session/selectModel', match: { provider: 'pb' }, respond: { rpcError: { code: 'not-found', message: 'model not found' } } });
    push(side, groupMessage('select fails after step'));
    await until('one metadata-only failure alert', () => privateTextsTo(side).includes(modelAlert()));
    await sleep(2500);
    const texts = privateTextsTo(side);
    assert.deepEqual(texts, [modelAlert()], 'selection failure does not duplicate the existing class alert');
    assert.equal(promptsWith(side, 'select fails after step').length, 1, 'the prompt was delivered once, to the primary only');
    await until('the turn ended failed', async () => (await turns(side)).some((t) => t.state === 'failed'));
    assert.ok((await records(side, '?limit=200')).some((r) => r.component === 'model-chain' && r.event === 'candidates-exhausted'), 'the exhaustion is on the record');
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },

  // A model error noted by an earlier attempt must not hide a different failure of the last attempt: the second candidate
  // accepts the prompt and then goes silent (the reply times out), and the chat worker still tells the administrator.
  async 'a-stale-model-error-note-does-not-hide-a-later-different-failure'(ctx) {
    const side = await ctx.start(base({
      mode: 'chat', state: { conversations: {} },
      dsh: { provider: 'pa', model: 'ma', reasoningEffort: 'low', fallbacks: [{ provider: 'pb', model: 'mb' }] },
      configure: (config) => { config.socialV2.turnTimeoutSecs = 3; },
      turnEventsForPrompt: (text, { selected }) => (selected?.provider === 'pa' ? failed(1, 'upstream', 'primary exploded') : []),
    }));
    push(side, groupMessage('stale note one'));
    await until('both candidates were tried', () => promptsWith(side, 'stale note one').length >= 2);
    await until('the timeout of the last candidate was reported independently', () => privateTextsTo(side).includes(modelAlert('pb', 'mb', 'timeout', 'TIMEOUT')), 40_000);
    assert.deepEqual(privateTextsTo(side), [modelAlert(), modelAlert('pb', 'mb', 'timeout', 'TIMEOUT')]);
  },

  // The retry cap of two holds per conversation across messages (JS failoverRetries), and the degraded candidate is sticky.
  async 'retry-cap-holds-across-messages-and-the-degraded-candidate-is-sticky'(ctx) {
    const side = await ctx.start(base({
      dsh: { provider: 'pa', model: 'ma', reasoningEffort: 'low', fallbacks: [{ provider: 'pb', model: 'mb' }, { provider: 'pc', model: 'mc' }, { provider: 'pd', model: 'md' }] },
      // A completed reminder legitimately resets the retry cap. This case needs two failed
      // messages with no successful turn between them, so use only the original closure check.
      configure: (config) => { config.socialV2.wake.maxWakeConfigReminders = 1; },
      turnEventsForPrompt: (text, { selected }) => (text.includes('【提醒】') ? completed(3) : ['pa', 'pb', 'pc'].includes(selected?.provider) ? failed(1, 'upstream', 'still exploding') : completed(2)),
    }));
    const names = () => side.world.recordedSelects().map((s) => `${s.provider}/${s.model}`);
    push(side, groupMessage('cap one'));
    await until('message one walked two failovers and gave up', () => promptsWith(side, 'cap one').length >= 3 && names().length >= 3);
    await until('message one ended', async () => (await turns(side)).some((t) => t.state === 'failed'));
    // An ordinary arrival while cleanup still owns the turn is suppressed as in-turn.
    await until('the failed turn finished its wake closure', () => side.stdout().includes(`[reserved2] ${GROUP_KEY} turn ended:`)
      && readState(side)?.conversations?.[GROUP_KEY]?.wakeConfig?.mode === 'diving'
      && readState(side)?.conversations?.[GROUP_KEY]?.wakeConfig?.triggers?.anyMessage === false);
    assert.deepEqual(names(), ['pa/ma', 'pb/mb', 'pc/mc'], 'two failovers spent, the third candidate is where it stopped');
    const spent = side.world.log.filter((e) => e.side === 'dsh' && e.kind === 'prompt-received').length;
    // a second message of the same conversation: the cap is still spent, it starts on the degraded candidate, no failover
    const wakeAgain = await httpCall(side, { method: 'POST', path: '/api/socialV2/wake-config', body: { key: GROUP_KEY, config: { mode: 'active', infinite: true, triggers: { anyMessage: true }, batchWindowMs: 300 } }, headers: { 'x-agent-token': 'tok-group-420042-aaaaaaaaaaaaaaaa' } });
    assert.equal(wakeAgain.status, 200, 'the conversation is armed for a second message');
    push(side, groupMessage('cap two'));
    await until('message two reached the model', () => promptsWith(side, 'cap two').length >= 1);
    await until('message two ended', async () => (await turns(side)).filter((t) => t.state === 'failed').length >= 2);
    await sleep(1500);
    assert.deepEqual(names(), ['pa/ma', 'pb/mb', 'pc/mc'], 'no new selection: the sticky candidate was used and the cap held (nothing fell back to the fourth candidate)');
    assert.equal(promptsWith(side, 'cap two').length, 1, 'message two was delivered once, not retried on another candidate');
    assert.ok(prompts(side).every((p) => !String(p.text).includes('【提醒】')), 'no successful reminder may reset the retry budget between the failed messages');
    assert.ok(spent >= 3);
  },

  // The bridge writes the failure of its dependencies before the console is up and serves it behind the token, paginated.
  async 'startup-failure-is-on-the-run-record-and-served-behind-the-token'(ctx) {
    const side = await ctx.start(base({ startDshDown: true }));
    assert.ok(fs.existsSync(recordFile(side)), 'the run record exists');
    const unauth = await httpCall(side, { path: '/api/recovery-record', consoleAuth: false });
    assert.ok([401, 403].includes(unauth.status), `the route answered ${unauth.status} without the console token`);
    const page = await api(side, '/api/recovery-record?limit=1');
    assert.equal(page.status, 200);
    assert.equal(page.json.records.length, 1);
    const all = await records(side, '?limit=200');
    const start = all.find((r) => r.component === 'process' && r.event === 'start');
    const dsh = all.find((r) => r.component === 'dsh' && r.event === 'startup-check');
    assert.ok(start && dsh, `startup records missing: ${JSON.stringify(all.map((r) => `${r.component}/${r.event}`))}`);
    assert.equal(dsh.outcome, 'failed');
    assert.equal(dsh.trigger, 'startup');
    assert.ok(start.seq < dsh.seq, `the start record (seq ${start.seq}) must precede the DSH startup check (seq ${dsh.seq})`);
    const next = page.json.nextBefore;
    if (all.length > 1) assert.ok(next, 'a further page exists');
    const stat = fs.statSync(recordFile(side));
    assert.ok(stat.size < 600 * 1024, 'the record is bounded');
    assert.ok(scanSideForSecrets(side, { files: [recordFile(side)] }).files.clean, 'no secret or path in the record');
  },

  // DSH login (ticket 10): a DSH clock somewhat behind ours is absorbed by the 60 s back-date; a clock so far behind
  // that every fresh cookie is future-dated for DSH is two rejections in a row per probe, and the administrator hears
  // it once, with the likely causes, however many probes keep failing.
  async 'dsh-login-rejected-twice-alerts-once-naming-the-likely-causes'(ctx) {
    const side = await ctx.start(base());
    const describes = (from) => side.world.log.slice(from).filter((e) => e.side === 'dsh' && e.kind === 'rpc' && e.endpoint === 'settings/describe');
    side.world.setDshClockOffset(-30_000);
    let mark = side.world.log.length;
    await until('two probes under a DSH clock 30 s behind', () => describes(mark).length >= 2, 20_000);
    assert.ok(describes(mark).every((e) => e.cookieValid), 'the back-dated cookie is accepted by a DSH 30 s behind');
    side.world.setDshClockOffset(-5 * 60_000);
    mark = side.world.log.length;
    const isLoginAlert = (t) => t.includes('clock skew') && t.includes('DSH version change');
    await until('the administrator was told', () => privateTextsTo(side).some(isLoginAlert), 30_000);
    await sleep(16_000); // several more probes, every fresh cookie still rejected
    const rejected = describes(mark).filter((e) => !e.cookieValid);
    assert.ok(rejected.length >= 6, `the probes kept being rejected: ${rejected.length}`);
    assert.equal(describes(mark).length, rejected.length, 'no cookie got through');
    assert.equal(privateTextsTo(side).filter(isLoginAlert).length, 1, 'one alert, not one per probe');
    const recs = await records(side, '?limit=200');
    assert.equal(recs.filter((r) => r.component === 'dsh' && r.event === 'login-rejected' && !r.repeat).length, 1, 'one record of the failing login');
    side.world.setDshClockOffset(null);
    await until('the login recovered', async () => (await records(side, '?limit=200')).some((r) => r.component === 'dsh' && r.event === 'login-accepted'), 20_000);
    assert.equal(privateTextsTo(side).filter(isLoginAlert).length, 1, 'recovery sends no second login alert');
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },

  // A long gateway outage is paced by the backoff: no storm, one streak warning, and recovery resets the pace.
  async 'gateway-reconnects-back_off-instead-of-storming'(ctx) {
    const side = await ctx.start(base({ reconnect: true }));
    const attempts = () => side.world.log.filter((e) => e.side === 'onebot' && e.kind === 'onebot-upgrade-refused').length;
    side.world.setOnebotUpgradeFault({ status: 401 });
    side.world.closeOnebotSockets({ code: 1006 });
    const t0 = Date.now();
    await sleep(45_000);
    const n = attempts();
    assert.ok(n >= 3, `the bridge stopped trying: ${n}`);
    assert.ok(n <= 8, `a reconnect storm: ${n} refused upgrades in ${(Date.now() - t0) / 1000}s (the old flat 5 s loop would make ~9)`);
    const rec = await records(side, '?limit=200');
    assert.ok(rec.some((r) => r.component === 'onebot' && r.event === 'connect-streak'), 'the streak warning is on the record');
    assert.equal(rec.filter((r) => r.component === 'onebot' && r.event === 'connect-streak' && !r.repeat).length, 1, 'one warning per outage');
    side.world.setOnebotUpgradeFault(null);
    await until('the bridge reconnected', () => rec.length >= 0 && side.world.log.filter((e) => e.side === 'onebot' && e.kind === 'ws-open').length >= 2, 60_000);
    await until('reconnect is recorded', async () => (await records(side, '?limit=200')).some((r) => r.component === 'onebot' && r.event === 'reconnected'), 20_000);
  },

  // SnowLuma self-heal: the token in config.json is stale; the account file has the working ones.
  async 'snowluma-token-self-heal-writes-only-the-selected-config'(ctx) {
    const home = path.join(ctx.dir, 'snowluma-home');
    const side = await ctx.start(base({
      reconnect: true,
      onebotTokens: { http: 'new-http-token-25', ws: 'new-ws-token-25' },
      configure: (config, world) => {
        fs.mkdirSync(path.join(home, 'config'), { recursive: true });
        const port = Number(new URL(world.onebot).port);
        fs.writeFileSync(path.join(home, 'config', 'onebot_401.json'), JSON.stringify({ networks: {
          httpServers: [{ accessToken: 'new-http-token-25', host: '127.0.0.1', port }],
          wsServers: [{ accessToken: 'new-ws-token-25', host: '127.0.0.1', port }],
        } }));
        config.snowluma = { ...config.snowluma, accessToken: 'stale-token-25', homeDir: home };
      },
    }));
    const cfg = JSON.parse(fs.readFileSync(side.configPath, 'utf8'));
    assert.equal(cfg.snowluma.accessToken, 'new-http-token-25', 'the healed HTTP token was written to the selected config');
    assert.ok(side.world.log.some((e) => e.side === 'onebot' && e.kind === 'ws-open' && e.auth === 'Bearer new-ws-token-25'), 'the bridge connected with the account\'s WebSocket token');
    assert.ok(!side.stdout().includes('new-http-token-25') && !side.stdout().includes('new-ws-token-25') && !side.stderr().includes('new-ws-token-25'), 'a token reached the process output');
    assert.ok(scanSideForSecrets(side, { files: [recordFile(side)], extra: ['new-http-token-25', 'new-ws-token-25', 'stale-token-25'] }).files.clean, 'a token reached the run record');
    assert.ok((await records(side, '?limit=200')).some((r) => r.component === 'snowluma' && r.event === 'token-heal' && r.outcome === 'ok'));
  },

  async 'snowluma-follow-discovered-endpoint-false-never-writes-the-endpoint'(ctx) {
    const home = path.join(ctx.dir, 'snowluma-home');
    const side = await ctx.start(base({
      reconnect: true,
      onebotTokens: { http: 'new-http-token-25', ws: 'new-ws-token-25' },
      configure: (config, world) => {
        fs.mkdirSync(path.join(home, 'config'), { recursive: true });
        const port = Number(new URL(world.onebot).port);
        // the account file names ANOTHER loopback port: with followDiscoveredEndpoint:false it must be ignored
        fs.writeFileSync(path.join(home, 'config', 'onebot_401.json'), JSON.stringify({ networks: {
          httpServers: [{ accessToken: 'new-http-token-25', host: '127.0.0.1', port }],
          wsServers: [{ accessToken: 'new-ws-token-25', host: '127.0.0.1', port: port + 1 }],
        } }));
        config.snowluma = { ...config.snowluma, accessToken: 'stale-token-25', homeDir: home, followDiscoveredEndpoint: false };
      },
    }));
    const cfg = JSON.parse(fs.readFileSync(side.configPath, 'utf8'));
    assert.equal(cfg.snowluma.accessToken, 'new-http-token-25', 'the token is healed');
    assert.ok(!('httpUrl' in cfg.snowluma && cfg.snowluma.httpUrl.endsWith(`:${Number(new URL(side.world.onebot).port) + 1}`)), 'the discovered endpoint was not written');
    assert.ok(!JSON.stringify(cfg.snowluma).includes(`${Number(new URL(side.world.onebot).port) + 1}`), 'no discovered port was persisted');
  },

  // Kill between "the turn is proven not executed" and "its events were handed back": nothing runs twice, the event
  // stays interrupted for the administrator, and the administrator is told once.
  async 'a-crash-after-a-turn-ended-resumed-is-visible-and-never-reruns-it'(ctx) {
    const side = await ctx.start(base({ mode: 'chat', state: { conversations: {} } }));
    // stall selectModel so the turn is open in the ledger (begun, never attempted) when the process is killed
    side.world.addDshFault({ endpoint: 'session/selectModel', respond: { delayMs: 60_000, status: 500 } });
    push(side, groupMessage('crash-window'));
    await until('the turn began and is stalled before its prompt', async () => (await turns(side)).some((t) => t.state === 'begun'), 30_000);
    await side.killLive();
    side.world.clearDshFaults();
    // restart with the fault seam: recovery proves the turn never executed, ends it `resumed`, and the process exits
    const child = side.spawnLive({ HER_FAULT_EXIT_AFTER_RESUMED: '1' });
    await until('the process exited at the seam', () => side.liveExit() !== null, 30_000);
    assert.equal(side.liveExit(), 86, `the fault seam did not fire (is this a --features parity-clock build?): exit ${side.liveExit()} ${side.stderr().slice(-300)}`);
    void child;
    // normal restart: the event is interrupted, never executed, and the administrator is told once
    await side.restart();
    await until('the administrator was told', () => privateTextsTo(side).some((t) => t.includes('交回消息前停止')), 40_000);
    assert.equal(privateTextsTo(side).filter((t) => t.includes('交回消息前停止')).length, 1, 'told once');
    assert.equal(promptsWith(side, 'crash-window').length, 0, 'the message was executed although its turn was proven not executed and never handed back');
    assert.ok((await records(side, '?limit=200')).some((r) => r.component === 'queue' && r.event === 'resumed-not-handed-back'), 'the record names it');
    const gaps = await api(side, '/api/inbound/gaps');
    assert.ok(JSON.stringify(gaps.json).includes('interrupted'), 'the administrator view shows the interrupted event');
    assert.deepEqual(groupLeaksOf(side), [], 'no internal text in the group');
  },
};

async function main() {
  const arg = (k, def) => (process.argv.includes(k) ? process.argv[process.argv.indexOf(k) + 1] : def);
  const candidate = path.resolve(arg('--candidate', process.env.HER_PARITY_CANDIDATE ?? ''));
  const out = path.resolve(arg('--out', path.join(process.env.TMPDIR ?? '/tmp', `dependency-recovery-${process.pid}`)));
  assert.ok(fs.existsSync(candidate), '--candidate <her> is required');
  developmentRoot(out, candidate);
  fs.mkdirSync(out, { recursive: true });
  const only = arg('--case', '');
  const names = Object.keys(CASES).filter((n) => !only || n === only);
  assert.ok(names.length, `no such case: ${only}`);
  let failedAny = false;
  for (const name of names) {
    const dir = path.join(out, name);
    let side = null;
    const notes = [];
    const ctx = {
      dir,
      note: (n) => notes.push(n),
      start: async (opts) => { side = await startRustSide(dir, candidate, opts); return side; },
    };
    try {
      await CASES[name](ctx);
      console.log(`PASS ${name}${notes.length ? ` ${JSON.stringify(notes)}` : ''}`);
    } catch (error) {
      failedAny = true;
      console.log(`FAIL ${name}\n${error.stack}`);
      if (side) console.log(`--- stderr tail ---\n${side.stderr().slice(-1500)}\n--- stdout tail ---\n${side.stdout().slice(-1500)}`);
    } finally {
      if (side) {
        fs.writeFileSync(path.join(out, `${name}.raw.json`), JSON.stringify({ world: side.world.log, stdout: side.stdout(), stderr: side.stderr() }, null, 2));
        await side.stop();
      }
    }
  }
  process.exitCode = failedAny ? 1 : 0;
}
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) main().catch((e) => { console.error(e); process.exitCode = 1; });
export { CASES };
