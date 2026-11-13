// Synthetic loopback world and process helpers for Rust integration tests.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { startFakeWorld, writeCredentials, secretsInText, redactedLogHasNoSecrets, groupLeaks } from './fake-world.mjs';
import crypto from 'node:crypto';
import { developmentRoot } from '../../../scripts/fixture-paths.mjs';
export const sha256 = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
export function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}


export const CONSOLE_TOKEN = 'parity-console-token';
export const OWNER = 8808;
export const GROUP = 420042;
export const GROUP2 = 420043;
export const PRIVATE_FRIEND = 9001;
export const GROUP_KEY = `group:${GROUP}`;
export const GROUP2_KEY = `group:${GROUP2}`;
export const OWNER_KEY = `private:${OWNER}`;
export const TOKENS = { [GROUP_KEY]: 'tok-group-420042-aaaaaaaaaaaaaaaa', [GROUP2_KEY]: 'tok-group-420043-bbbbbbbbbbbbbbbb', [OWNER_KEY]: 'tok-private-8808-cccccccccccccccc', 'private:9001': 'tok-private-9001-dddddddddddddddd' };

/** Realistic stored message (shape of bridge.js appendSocialV2Message). */
export function storedMessage(seq, over = {}) {
  const text = over.text ?? `消息${seq}`;
  return {
    seq, messageId: String(7000 + seq), sender: over.sender ?? '小明', userId: over.userId ?? '10001', text, plain: over.plain ?? text, tail: over.tail ?? text,
    quoteTargetIsSelf: false, isOwner: false, ownerLabel: '', isSelf: false, media: [], hasMedia: false, forwardIds: [], hasForward: false,
    time: 1_760_000_000_000 + seq * 1000, ...over,
  };
}

export function defaultWake(over = {}) {
  return { mode: 'diving', infinite: true, sleepUntil: null, triggers: { atMention: true, nameMention: false, question: false, poke: false, anyMessage: false, probability: 0, keywords: [], speakerIds: [] }, batchWindowMs: 3000, lastWakeAt: 0, wakeCount: 0, noActionCount: 0, confirmedAt: 1_760_000_000_000, confirmedBy: 'set_wake_config', ...over };
}

/** A conversation record that the bridge loads. */
export function convRecord(key, over = {}) {
  return {
    wakeConfig: defaultWake(), recentMessages: [], unread: [], lastWakeReason: '', lastAiReplyAt: 0, lastActionAt: 0, agentToken: TOKENS[key],
    bootstrapSent: true, wakeTimes: [], sendTimes: [], stickerCollectTimes: [], lastIncomingAt: 0, preSleepWaitSatisfiedAt: 0, preSleepWaitObservedAt: 0,
    preSleepWaitAccumMs: 0, lastUnreadSeq: 0, lastReadThroughSeq: 0, activeTopics: [], pendingThoughts: [], memberImpressions: {}, ...over,
  };
}

export const baseSocial = () => ({
  wake: { batchWindowMs: 3000, maxWakePerMinute: 8, maxWakePerHour: 24, preSleepWaitMs: 300000 },
  social: { skipProbability: 0.0 },
  send: { mergeMultiIntoOne: true, maxMessageChars: 500, maxSendPerMinute: 20, maxSendPerHour: 100, burstIntervalMinMs: 0, maxGapMs: 0, firstGapEnabled: false },
  wait: { minMs: 100, defaultMs: 1000, maxMs: 4000, minQuietAfterNewMs: 0, defaultQuietMs: 0 },
  sticker: { enabled: false },
  voice: { enabled: false },
});

export function baseConfig(world, over = {}) {
  return {
    ownerQQ: OWNER,
    allow: { private: [String(OWNER), String(PRIVATE_FRIEND)], groups: [GROUP, GROUP2] },
    socialV2: baseSocial(),
    slang: { enabled: false },
    ...over,
  };
}

/** Fake OneBot behaviour shared by both sides (stateless, so both sides see the same answers). */
export function onebotResponder(action, params) {
  switch (action) {
    case 'get_login_info': return { data: { user_id: 401, nickname: '测试机器人' } };
    case 'get_status': return { data: { online: true, good: true } };
    case 'get_group_list': return { data: [{ group_id: GROUP, group_name: '测试群甲' }, { group_id: GROUP2, group_name: '测试群乙' }, { group_id: 999999, group_name: '白名单外的群' }] };
    case 'get_group_member_list': return { data: [{ user_id: 10001, nickname: '小明', card: '明明' }, { user_id: 10002, nickname: '小红', card: '' }] };
    case 'get_group_member_info': return Number(params?.user_id) === 10001 || Number(params?.user_id) === 10002 ? { data: { user_id: Number(params.user_id), nickname: '成员' } } : { retcode: 1200, status: 'failed', wording: '成员不存在', data: null };
    case 'get_group_msg_history': return { data: { messages: [{ message_id: 7001, message_seq: 1, user_id: 10001, raw_message: 'hist' }] } };
    case 'get_msg': {
      const id = Number(params?.message_id);
      if (id === 7003) return { data: { message_id: 7003, group_id: GROUP, message_seq: 3, sender: { user_id: 10001, nickname: '小明' }, message: [{ type: 'text', data: { text: '被引用的话' } }] } };
      if (id === 8888) return { data: { message_id: 8888, group_id: GROUP, message_seq: 88, sender: { user_id: 10001, nickname: '小明', card: '明明' }, message: [{ type: 'text', data: { text: '只在网关里的消息' } }] } };
      return { retcode: 1200, status: 'failed', wording: '消息不存在', data: null };
    }
    case 'send_group_msg': case 'send_private_msg': case 'send_msg': return undefined;
    case 'group_poke': case 'send_poke': return { data: null };
    default: return undefined;
  }
}

function scenarioFor(extra = {}) {
  return {
    onebotTokens: extra.onebotTokens,
    selfId: 401, selfNickname: '测试机器人', inbound: extra.inbound ?? [], inboundDelayMs: extra.inboundDelayMs ?? 600,
    dsh: { sessionId: 's1', workspaceId: 'w1', workspaceCreated: extra.workspaceCreated, uniqueSessions: extra.uniqueSessions === true, settings: extra.settings ?? {namespaces:[]}, turnEvents: extra.turnEvents ?? [], turnEventsForPrompt: extra.turnEventsForPrompt, presets: [{ id: 'qq-chat', isDefault: true }, { id: 'qq-chat-v2', isDefault: false }], provider: 'p', model: 'm', faults: extra.dshFaults },
    onebotResponder: extra.responder ?? onebotResponder,
    onebotFaults: extra.onebotFaults,
  };
}

async function waitHttp(port, timeoutMs = 30_000) {
  const deadline = performance.now() + timeoutMs;
  let last;
  while (performance.now() < deadline) {
    try {
      const request = route => fetch(`http://127.0.0.1:${port}${route}`, { headers: { 'x-console-token': CONSOLE_TOKEN }, signal: AbortSignal.timeout(Math.max(1, Math.min(5000, Math.ceil(deadline - performance.now())))) });
      let r = await request('/api/health');
      if (r.status !== 200) r = await request('/api/roles');
      last = {status: r.status};
      if (r.status === 200) return true;
    } catch (error) { last = {error: String(error)}; }
    await sleep(Math.max(0, Math.min(150, deadline - performance.now())));
  }
  throw new Error(`console HTTP readiness timed out after ${timeoutMs} ms; last observed: ${JSON.stringify(last)}`);
}

async function waitGateway(world, previousConnections, child, timeoutMs = 10_000) {
  const deadline = performance.now() + timeoutMs;
  let connections;
  while (performance.now() < deadline) {
    connections = world.log.filter(e => e.side === 'onebot' && e.kind === 'ws-open').length;
    if (connections > previousConnections) return;
    if (child.exitCode !== null) throw new Error(`bridge exited before OneBot connection (${child.exitCode})`);
    await sleep(Math.max(0, Math.min(50, deadline - performance.now())));
  }
  throw new Error(`bridge console is ready but its OneBot connection never opened after ${timeoutMs} ms; last observed: ${connections}, expected > ${previousConnections}`);
}

/** A connected gateway is not login readiness: both bridges fill self asynchronously. */
export async function waitPromptIdentity(port, child, expected, timeoutMs = 10_000) {
  const deadline = performance.now() + timeoutMs;
  let last;
  while (performance.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`bridge exited before login identity (${child.exitCode})`);
    try {
      const response = await fetch(`http://127.0.0.1:${port}/api/socialV2/prompt?key=${GROUP_KEY}`, {
        headers: { 'x-console-token': CONSOLE_TOKEN }, signal: AbortSignal.timeout(Math.max(1, Math.min(5000, Math.ceil(deadline - performance.now())))),
      });
      if (response.status !== 200) throw new Error(`prompt readiness returned HTTP ${response.status}`);
      const result = await response.json();
      last = result.self;
      if (result.self?.nickname === expected.nickname && result.self?.userId === expected.userId) return result;
    } catch (error) {
      if (error.name !== 'TimeoutError' && error.name !== 'TypeError') throw error;
      last = {error: String(error)};
    }
    await sleep(Math.max(0, Math.min(50, deadline - performance.now())));
  }
  throw new Error(`bridge console is ready but the expected login identity never became visible after ${timeoutMs} ms; expected ${JSON.stringify(expected)}; last observed: ${JSON.stringify(last)}`);
}

/** Starts the frozen JS bridge. `seed(root)` runs before start; `opts.config` is merged over baseConfig. */
export async function startRustSide(runRoot, exePath, opts = {}) {
  developmentRoot(runRoot, exePath);
  developmentRoot(path.join(runRoot, 'rust-root'), exePath);
  fs.rmSync(runRoot, { recursive: true, force: true });
  const root = path.join(runRoot, 'rust-root');
  for (const dir of ['config', 'data', 'resources/roles', 'user/dsh']) fs.mkdirSync(path.join(root, dir), { recursive: true });
  const home = path.join(runRoot, 'rust-home');
  fs.mkdirSync(home, { recursive: true });
  const world = await startFakeWorld({ scenario: scenarioFor(opts), dshHome: path.join(root, 'user', 'dsh') });
  writeCredentials(path.join(root, 'user', 'dsh'));
  if (opts.startDshDown) await world.dshDown();
  const consolePort = await freePort();
  const config = baseConfig(world, opts.config ?? {});
  Object.assign(config, {
    consoleToken: CONSOLE_TOKEN, consolePort,
    dsh: { baseUrl: world.dsh, provider: 'p', model: 'm', reasoningEffort: 'medium', fallbacks: [], ...(opts.dsh ?? {}) },
    snowluma: { accessToken: 'parity-onebot-token', httpUrl: world.onebot, wsUrl: world.onebotWs },
    rollout: { enabled: false, rustGroups: [] },
  });
  opts.configure?.(config, world, root);
  fs.writeFileSync(path.join(root, 'config', 'config.json'), JSON.stringify(config, null, 2));
  fs.writeFileSync(path.join(root, 'data', 'mode.json'), JSON.stringify({ mode: opts.mode ?? 'reserved2' }));
  fs.writeFileSync(path.join(root, 'data', 'social-v2.json'), JSON.stringify(opts.state ?? { conversations: {} }));
  fs.writeFileSync(path.join(root, 'dependencies.json'), JSON.stringify({ dsh: world.dsh, model: world.model, onebot: world.onebot, onebot_ws: world.onebotWs }));
  opts.seed?.(root, world);
  const env = {
    PATH: process.env.PATH, SystemRoot: process.env.SystemRoot, HOME: home, USERPROFILE: home, DSH_HOME: path.join(root, 'user', 'dsh'),
    APPDATA: path.join(home, 'Roaming'), LOCALAPPDATA: path.join(home, 'Local'), TZ: 'UTC', TEMP: path.join(home, 'tmp'), TMP: path.join(home, 'tmp'), ...(opts.env ?? {}),
  };
  if (opts.clock) Object.assign(env, { HER_PARITY_EPOCH_MS: String(opts.clock), HER_PARITY_RANDOM_SEED: '1' });
  fs.mkdirSync(env.TEMP, { recursive: true });
  const liveGroups = (opts.liveGroups ?? [GROUP_KEY, GROUP2_KEY]).join(',');
  let child = null;
  let consoleChild = null;
  let stdout = '';
  let stderr = '';
  const spawnProc = (args, extraEnv = {}) => {
    const p = spawn(exePath, args, { cwd: root, env: { ...env, ...extraEnv }, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    p.stdout.on('data', (c) => { stdout += c; });
    p.stderr.on('data', (c) => { stderr += c; });
    return p;
  };
  const startConsole = async () => {
    consoleChild = spawnProc(['--runtime-root', root, '--console-serve', '--console-port', String(consolePort)]);
    if (!(await waitHttp(consolePort, opts.readinessTimeoutMs))) {
      consoleChild.kill('SIGKILL');
      throw new Error(`Rust console did not start: ${stderr.slice(-400)}`);
    }
  };
  const liveArgs = () => ['--runtime-root', root, '--live', '--live-groups', liveGroups, ...(opts.reconnect ? [] : ['--no-reconnect']), '--console-port', String(consolePort)];
  const start = async (extraEnv = {}) => {
    // The live bridge (inbound, wake, turns) and the console (HTTP routes MCP talks to) are separate processes
    // sharing the runtime root, exactly as in the isolated live e2e tests.
    const connections = world.log.filter(e => e.side === 'onebot' && e.kind === 'ws-open').length;
    child = spawnProc(liveArgs(), extraEnv);
    await startConsole();
    await waitGateway(world, connections, child, opts.readinessTimeoutMs);
    const login = (opts.responder?.('get_login_info', {}) ?? onebotResponder('get_login_info', {})).data;
    await waitPromptIdentity(consolePort, child, { nickname: login.nickname, userId: String(login.user_id) }, opts.readinessTimeoutMs);
  };
  try {
    await start();
  } catch (error) {
    // A failed start must not leave the candidate processes or the fake world's servers running (they keep the runner alive).
    child?.kill('SIGKILL');
    consoleChild?.kill('SIGKILL');
    await new Promise((r) => setTimeout(r, 300));
    await world.close();
    throw error;
  }
  return {
    kind: 'rust', root, port: consolePort, world, exeSha256: sha256(exePath), stdout: () => stdout, stderr: () => stderr,
    mcpMaxBufferSize: opts.mcpMaxBufferSize,
    config, statePath: path.join(root, 'data', 'social-v2.json'), dataDir: path.join(root, 'data'), configPath: path.join(root, 'config', 'config.json'),
    mcpCommand: () => ({ command: exePath, args: ['--mcp-serve', '--runtime-root', root, '--console-port', String(consolePort)], cwd: root, env: { ...env, QQ_SESSION_KEY: opts.mcpEnv?.QQ_SESSION_KEY ?? '', QQ_SESSION_TOKEN: opts.mcpEnv?.QQ_SESSION_TOKEN ?? '' } }),
    restart: async () => { child.kill('SIGKILL'); consoleChild.kill('SIGKILL'); await new Promise((r) => setTimeout(r, 500)); await start(); },
    /** Start only the live bridge (no readiness waits) with extra environment, e.g. a fault seam; returns the child process. */
    spawnLive: (extraEnv = {}) => { child = spawnProc(liveArgs(), extraEnv); return child; },
    killLive: async () => { child?.kill('SIGKILL'); await new Promise((r) => setTimeout(r, 300)); },
    liveExit: () => (child && child.exitCode !== null ? child.exitCode : null),
    restartConsole: async () => { consoleChild.kill('SIGKILL'); await new Promise((r) => setTimeout(r, 500)); await startConsole(); },
    stop: async () => { child?.kill('SIGKILL'); consoleChild?.kill('SIGKILL'); await new Promise((r) => setTimeout(r, 300)); await world.close(); },
  };
}

export async function httpCall(side, step) {
  const headers = { ...(step.consoleAuth === false ? {} : { 'x-console-token': CONSOLE_TOKEN }), ...(step.headers ?? {}) };
  let body;
  if (step.rawBody !== undefined) { body = step.rawBody; headers['content-type'] = step.contentType ?? 'application/json'; }
  else if (step.body !== undefined) { body = JSON.stringify(step.body); headers['content-type'] = step.contentType ?? 'application/json'; }
  const res = await fetch(`http://127.0.0.1:${side.port}${step.path}`, { method: step.method ?? 'GET', headers, body, signal: AbortSignal.timeout(step.timeoutMs ?? 30_000) });
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch { json = null; }
  return { status: res.status, json, text: json === null ? text : undefined };
}

export function readState(side) {
  try { return JSON.parse(fs.readFileSync(side.statePath, 'utf8')); } catch { return null; }
}

export function writeState(side, state) {
  fs.writeFileSync(side.statePath, JSON.stringify(state));
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- failure / recovery helpers (ticket 25) --------------------------------------------------
// Opt-in `startRustSide` options: `dshFaults` and `onebotFaults` (fault scripts, see
// fake-world.mjs), `dsh` (merged over the side's `dsh` config: provider, model, reasoningEffort, fallbacks).
// The side's world exposes dshDown/dshUp, rotateSecret, recordedSelects, closeOnebotSockets, ...

/** Secrets/paths in what the side printed (stdout/stderr) plus optional files/directories (its redacted logs). */
export function scanSideForSecrets(side, { files = [], extra = [] } = {}) {
  const output = secretsInText(`${side.stdout()}\n${side.stderr()}`, { extra });
  const fileScan = files.length ? redactedLogHasNoSecrets(files, { extra }) : { clean: true, hits: [], missing: [] };
  return { clean: output.length === 0 && fileScan.clean, output, files: fileScan };
}

/** Group sends that leaked internal error, restart or model text (owner private alerts are not counted). */
export const groupLeaksOf = (side, opts) => groupLeaks(side.world.log, opts);

/** Private messages sent to `userId` (default: the owner) in arrival order, as plain text. */
export function privateTextsTo(side, userId = OWNER) {
  return side.world.log.filter((e) => e.side === 'onebot' && e.kind === 'action' && e.action === 'send_private_msg' && Number(e.params?.user_id) === Number(userId))
    .map((e) => (typeof e.params.message === 'string' ? e.params.message : Array.isArray(e.params.message) ? e.params.message.map((seg) => seg?.data?.text ?? '').join('') : ''));
}

/** DSH RPCs seen for `endpoint`, optionally only those with a valid cookie. */
export const dshCalls = (side, endpoint, { validOnly = false } = {}) => side.world.log.filter((e) => e.side === 'dsh' && e.kind === 'rpc' && e.endpoint === endpoint && (!validOnly || e.cookieValid));

/** Polls `predicate` until true (no fixed sleeps in tests); resolves true, or false after `timeoutMs`. */
export async function waitFor(predicate, { timeoutMs = 15_000, stepMs = 50 } = {}) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    if (await predicate()) return true;
    if (Date.now() > end) return false;
    await sleep(stepMs);
  }
}
export { pathToFileURL, os, net };
