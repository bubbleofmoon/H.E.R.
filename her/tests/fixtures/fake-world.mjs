// Fake external world for Rust process integration tests. The Rust bridge talks to these loopback servers over real sockets. Everything
// is synthetic: fixed credentials, fixed inbound events, scripted DSH turn
// events, and a log of every request in arrival order.
import http from 'node:http';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { developmentRoot } from '../../../scripts/fixture-paths.mjs';

export const SECRET = Buffer.alloc(32, 7);
const WS_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

const b64url = (buf) => Buffer.from(buf).toString('base64').replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');

/** Synthetic credentials readable by both the JS bridge (records form) and Rust (top-level secret). */
export function writeCredentials(dshHome, secretBytes = SECRET) {
  developmentRoot(dshHome);
  fs.mkdirSync(dshHome, { recursive: true });
  const secret = b64url(secretBytes);
  fs.writeFileSync(path.join(dshHome, '.credentials.yaml'),
    `secret: "${secret}"\nrecords:\n  client-connection/browser-session:\n    kind: grant\n    payload:\n      version: 1\n      secret: "${secret}"\n`);
}

/** A cookie the fake DSH accepts (same minting as dsh-client.js): `secretBytes` defaults to the initial secret. */
export function mintCookie(authority, secretBytes = SECRET, { issuedAt = Date.now() - 1000, expiresAt = Date.now() + 60_000 } = {}) {
  const body = b64url(Buffer.from(JSON.stringify({ version: 1, authority, issuedAt, expiresAt })));
  const sig = b64url(crypto.createHmac('sha256', secretBytes).update(body).digest());
  const name = `dsh-auth-${b64url(crypto.createHash('sha256').update(authority).digest())}`;
  return `${name}=v1.${body}.${sig}`;
}

/** `dshNowMs` (optional) applies DSH's time rule too: `issuedAt <= now < expiresAt`, no skew tolerance. */
export function cookieValid(cookie, authority, secretBytes = SECRET, dshNowMs = null) {
  if (!cookie) return false;
  const eq = cookie.indexOf('=');
  if (eq < 0) return false;
  const name = cookie.slice(0, eq);
  const parts = cookie.slice(eq + 1).split('.');
  // Cookie name is 'dsh-auth-' + base64url(sha256(authority)), as dsh-client.js mints it.
  if (name !== `dsh-auth-${b64url(crypto.createHash('sha256').update(authority).digest())}`) return false;
  if (parts.length !== 3 || parts[0] !== 'v1') return false;
  const [, body, sig] = parts;
  let payload;
  try { payload = JSON.parse(Buffer.from(body, 'base64url').toString('utf8')); } catch { return false; }
  if (payload.authority !== authority) return false;
  // Time windows are checked only on request (`setDshClockOffset`): fixtures may use a synthetic advancing epoch.
  if (!(payload.expiresAt > payload.issuedAt)) return false;
  if (dshNowMs !== null && !(payload.issuedAt <= dshNowMs && dshNowMs < payload.expiresAt)) return false;
  const want = crypto.createHmac('sha256', secretBytes).update(body).digest();
  const got = Buffer.from(sig, 'base64url');
  return got.length === want.length && crypto.timingSafeEqual(got, want);
}

// ---- minimal RFC6455 server side -------------------------------------------
function wsAccept(req, socket) {
  const key = req.headers['sec-websocket-key'];
  const accept = crypto.createHash('sha1').update(key + WS_GUID).digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
}

function wsSend(socket, text) {
  const data = Buffer.from(text, 'utf8');
  let header;
  if (data.length < 126) {
    header = Buffer.from([0x81, data.length]);
  } else if (data.length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 126;
    header.writeUInt16BE(data.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(data.length), 2);
  }
  socket.write(Buffer.concat([header, data]));
}

/** Sends a close frame with `code` (RFC6455 control frame), then ends the socket. */
function wsClose(socket, code) {
  const frame = Buffer.alloc(4);
  frame[0] = 0x88;
  frame[1] = 2;
  frame.writeUInt16BE(code, 2);
  socket.write(frame);
  socket.end();
}

/** Parses masked client frames; calls onText(string) for each complete text frame. */
function wsReader(socket, onText, onClose) {
  let buf = Buffer.alloc(0);
  socket.on('data', (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const op = buf[0] & 0x0f;
      let len = buf[1] & 0x7f;
      const masked = (buf[1] & 0x80) !== 0;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        off = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        off = 10;
      }
      const maskLen = masked ? 4 : 0;
      if (buf.length < off + maskLen + len) return;
      const mask = masked ? buf.subarray(off, off + 4) : null;
      const payload = Buffer.from(buf.subarray(off + maskLen, off + maskLen + len));
      if (mask) for (let i = 0; i < payload.length; i += 1) payload[i] ^= mask[i % 4];
      buf = buf.subarray(off + maskLen + len);
      if (op === 0x8) {
        onClose();
        socket.end();
        return;
      }
      if (op === 0x1) onText(payload.toString('utf8'));
    }
  });
  socket.on('close', onClose);
  socket.on('error', onClose);
  // A killed client sends FIN on Linux (RST on Windows); the upgraded socket is half-open until it answers.
  socket.on('end', () => { onClose(); socket.end(); });
}

/** Drops volatile fields from RPC args so the log can be compared across runs. */
function normalizeArgs(args) {
  if (!args || typeof args !== 'object') return null;
  const copy = structuredClone(args);
  delete copy.requestId;
  return copy;
}

/**
 * Start the fake world. `scenario` is the shared JSON scenario (see scenarios/).
 * Returns base URLs, the ordered request log, and close().
 */
export async function startFakeWorld({ scenario, dshHome }) {
  const log = [];
  const record = (entry) => { log.push({ seq: log.length + 1, ...entry }); };
  writeCredentials(dshHome);
  // Opt-in fault controls (ticket 25). None of this changes behaviour or the log unless a fault/control is used.
  let secret = SECRET;
  const selects = []; // every session/selectModel the fake saw: { sessionId, provider, model, effort, ok }
  const selectedBySession = new Map(); // sessionId -> { provider, model, effort } of the last successful selectModel
  const dshFaults = (scenario.dsh.faults ?? []).map((f) => ({ ...f, seen: 0, applied: 0 }));
  const onebotFaults = (scenario.onebotFaults ?? []).map((f) => ({ ...f, seen: 0, applied: 0 }));
  let onebotUpgradeFault = null;
  let dshClockOffsetMs = null; // null: cookie time windows unchecked; a number: DSH's clock is host time plus this
  const dshNow = () => (dshClockOffsetMs === null ? null : Date.now() + dshClockOffsetMs);
  // Opt-in OneBot credentials (ticket 25 SnowLuma self-heal): { http, ws } bearer tokens; a mismatch is refused like SnowLuma does.
  let onebotTokens = scenario.onebotTokens ? { ...scenario.onebotTokens } : null;
  const bearerOf = (req) => String(req.headers.authorization ?? '').replace(/^Bearer\s+/i, '');
  let dshFaultsApplied = 0;
  // The frozen JS client wraps selectModel arguments as { request: {...} }; a flat form is accepted as well.
  const selectArgs = (args) => args?.request ?? args ?? {};
  const matches = (match, args, ctx) => {
    if (match === undefined || match === null) return true;
    if (typeof match === 'function') return Boolean(match(args, ctx));
    const sel = selectArgs(args);
    const wants = { provider: sel.provider, model: sel.model, effort: sel.reasoningEffort, sessionId: sel.sessionId ?? args?.request?.sessionId };
    for (const [k, v] of Object.entries(match)) {
      if (k === 'textIncludes') { if (!String(ctx.text ?? '').includes(v)) return false; continue; }
      if (wants[k] !== v) return false;
    }
    return true;
  };
  /** First fault of the list that matches this call and still has budget (`after` skips the first N matches; `times` bounds applications). */
  const selectEntry = (args, ok) => { const a = selectArgs(args); return { sessionId: a.sessionId ?? null, provider: a.provider ?? null, model: a.model ?? null, effort: a.reasoningEffort ?? null, ok }; };
  const pickFault = (faults, target, args, ctx, field = 'endpoint') => {
    for (const f of faults) {
      if ((f[field] !== target && f[field] !== '*') || !matches(f.match, args, ctx)) continue; // '*' matches every endpoint/action
      f.seen += 1;
      if (f.seen <= (f.after ?? 0)) continue;
      if (f.applied >= (f.times ?? Infinity)) continue;
      f.applied += 1;
      return f;
    }
    return null;
  };

  // ---- DSH: RPC over POST /api/<endpoint>, mux over WS /api/remote.mux -----
  const followers = new Map(); // sessionId -> { socket, streamId }
  const eventClients = new Map(); // socket -> remote-event stream (ticket 13)
  const sessionId = scenario.dsh.sessionId;
  let activeSession = sessionId;
  let sessionNumber = 0;
  const pushEvents = (sid, events) => {
    const f = followers.get(sid);
    if (!f) return false;
    for (const event of events) {
      wsSend(f.socket, JSON.stringify({ type: 'item', streamId: f.streamId, value: { type: 'event', event } }));
      record({ side: 'dsh', kind: 'turn-event', eventType: event.type, turn: event.data?.turn ?? null, text: event.data?.message?.content?.[0]?.text ?? null });
    }
    return true;
  };
  const dshServer = http.createServer((req, res) => {
    const authority = req.headers.host;
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      let envelope = {};
      try { envelope = JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}'); } catch { envelope = {}; }
      const endpoint = req.url.replace(/^\/api\//, '').split('?')[0];
      const valid = cookieValid(req.headers.cookie, authority, secret, dshNow());
      const args = envelope.payload?.args ?? null;
      record({ side: 'dsh', kind: 'rpc', endpoint, cookieValid: valid, args: normalizeArgs(args) });
      if (!valid) {
        res.writeHead(401, { 'content-type': 'application/json' });
        res.end('{"error":"unauthenticated"}');
        return;
      }
      const promptText = endpoint === 'session/prompt' ? (args?.request?.content?.[0]?.text ?? '') : undefined;
      const fault = dshFaults.length ? pickFault(dshFaults, endpoint, args, { endpoint, text: promptText }) : null;
      if (fault) {
        dshFaultsApplied += 1;
        record({ side: 'dsh', kind: 'fault-applied', endpoint, respond: Object.keys(fault.respond ?? {}).join(',') });
        const r = fault.respond ?? {};
        if (endpoint === 'session/selectModel' && (r.status || r.rpcError || r.closeSocket)) selects.push(selectEntry(args, false));
        if (r.closeSocket) { req.socket.destroy(); return; }
        const injected = r.status || r.rpcError;
        const answer = () => {
          if (r.status) {
            res.writeHead(r.status, { 'content-type': 'application/json' });
            res.end(r.body ?? '{"error":"fault"}');
          } else {
            res.writeHead(200, { 'content-type': 'application/json' });
            res.end(JSON.stringify({ type: 'server-response', rpcId: envelope.rpcId, result: { ok: false, error: { code: r.rpcError?.code ?? 'fault', message: r.rpcError?.message ?? 'injected fault' } } }));
          }
        };
        if (injected) {
          if (r.delayMs) setTimeout(answer, r.delayMs); else answer();
          return;
        }
        if (r.delayMs) { setTimeout(() => serve(), r.delayMs); return; }
      }
      serve();
      function serve() {
      let value = {};
      if (endpoint === 'workspace/create') {
        value = { workspace: { workspaceId: scenario.dsh.workspaceId } };
        if (scenario.dsh.workspaceCreated !== undefined) value.created = scenario.dsh.workspaceCreated;
      }
      else if (endpoint === 'session/create') {
        sessionNumber += 1;
        activeSession = scenario.dsh.uniqueSessions ? `s${sessionNumber}` : sessionId;
        value = { sessionId: activeSession };
      }
      else if (endpoint === 'session/prompt') {
        const text = args?.request?.content?.[0]?.text ?? '';
        // uniqueSessions: prompts target the session named in the request (falling back to the newest one); otherwise the single synthetic session.
        const target = scenario.dsh.uniqueSessions ? (args?.request?.sessionId ?? args?.sessionId ?? activeSession) : sessionId;
        record({ side: 'dsh', kind: 'prompt-received', sessionId: scenario.dsh.uniqueSessions ? target : (args?.sessionId ?? null), text });
        // The scripted turn is delivered on the follow stream after the RPC response is written.
        setImmediate(() => {
          const events = scenario.dsh.turnEventsForPrompt ? scenario.dsh.turnEventsForPrompt(text, { sessionId: target, selected: selectedBySession.get(args?.request?.sessionId ?? args?.sessionId ?? target) ?? selectedBySession.get(target) ?? null }) : scenario.dsh.turnEvents;
          const ok = pushEvents(target, events);
          record({ side: 'dsh', kind: 'turn-script-pushed', ok });
        });
        value = { accepted: true };
      } else if (endpoint === 'settings/describe') value = scenario.dsh.settings ?? { namespaces: [] };
      else if (endpoint === 'agentPresets/list') value = { presets: scenario.dsh.presets };
      else if (endpoint === 'session/selectModel') {
        const entry = selectEntry(args, true);
        selects.push(entry);
        selectedBySession.set(entry.sessionId, { provider: entry.provider, model: entry.model, effort: entry.effort });
        value = { selected: { provider: scenario.dsh.provider, model: scenario.dsh.model } };
      }
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ type: 'server-response', rpcId: envelope.rpcId, result: { ok: true, value } }));
      }
    });
  });
  dshServer.on('upgrade', (req, socket) => {
    if (req.url !== '/api/remote.mux') {
      socket.destroy();
      return;
    }
    const valid = cookieValid(req.headers.cookie, req.headers.host, secret, dshNow());
    wsAccept(req, socket);
    record({ side: 'dsh', kind: 'mux-open', cookieValid: valid });
    if (!valid) {
      socket.destroy();
      return;
    }
    wsReader(socket, (text) => {
      let frame;
      try { frame = JSON.parse(text); } catch { return; }
      if (frame.type === 'open' && frame.endpoint === 'session/follow') {
        const sid = frame.payload?.args?.request?.address?.sessionId ?? null;
        followers.set(sid, { socket, streamId: frame.streamId });
        record({ side: 'dsh', kind: 'follow-open', sessionId: sid });
        wsSend(socket, JSON.stringify({ type: 'item', streamId: frame.streamId, value: { type: 'snapshot', records: [] } }));
      } else if (frame.type === 'open' && frame.endpoint === '$events') {
        const clientId = `fixture-client-${eventClients.size + 1}`;
        eventClients.set(socket, { streamId: frame.streamId, clientId });
        wsSend(socket, JSON.stringify({ type: 'item', streamId: frame.streamId, value: { type: 'ready', clientId } }));
        record({ side: 'dsh', kind: 'stream-open', endpoint: frame.endpoint, clientId });
        socket.on('close', () => eventClients.delete(socket));
      } else if (frame.type === 'open') {
        record({ side: 'dsh', kind: 'stream-open', endpoint: frame.endpoint });
      }
    }, () => {});
  });

  // `scenario.onebotResponder(action, params)` (optional) may return { data, retcode, wording, status } to override
  // the built-in synthetic answers; returning undefined keeps the default. Every action is still recorded.
  const baseActionData = (action, params) => (action === 'get_login_info' ? { user_id: scenario.selfId, nickname: scenario.selfNickname }
    : action === 'set_input_status' ? {}
    : action === 'send_group_msg' ? { message_id: 5000 + log.length }
      : action === 'get_msg' ? { message_id: params?.message_id, message: [] }
        : {});
  const onebotFaultAnswer = (r, echo) => ({ status: r.status ?? (r.retcode ? 'failed' : 'ok'), retcode: r.retcode ?? 0, data: r.data ?? null, wording: r.wording, echo });
  const onebotAnswer = (action, params, echo) => {
    const custom = scenario.onebotResponder?.(action, params);
    if (custom !== undefined) {
      return { status: custom.status ?? 'ok', retcode: custom.retcode ?? 0, data: custom.data ?? null, wording: custom.wording, echo };
    }
    return { status: 'ok', retcode: 0, data: baseActionData(action, params), echo };
  };

  // ---- OneBot (SnowLuma) WS: calls + inbound events -------------------------
  const onebotSockets = new Set();
  const onebotServer = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const action = req.url.split('?')[0].replace(/^\//, '');
      if (req.method === 'POST' && /^[a-z_]+$/.test(action)) {
        // OneBot HTTP action (the Rust candidate sends over HTTP): recorded like WS actions.
        let params = {};
        try { params = JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}'); } catch { params = {}; }
        record({ side: 'onebot', kind: 'action', action, params, transport: 'http' });
        if (onebotTokens?.http !== undefined && bearerOf(req) !== onebotTokens.http) {
          record({ side: 'onebot', kind: 'http-refused', action });
          res.writeHead(401, { 'content-type': 'application/json' });
          res.end(JSON.stringify({ status: 'failed', retcode: 1401, data: null, wording: 'unauthorized' }));
          return;
        }
        const fault = onebotFaults.length ? pickFault(onebotFaults, action, params, { action }, 'action') : null;
        const respond = (status, body) => { res.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body) }); res.end(body); };
        if (fault) {
          const r = fault.respond ?? {};
          record({ side: 'onebot', kind: 'fault-applied', action, respond: Object.keys(r).join(',') });
          if (r.closeSocket) { req.socket.destroy(); return; }
          if (r.hang) return; // never answered: the caller's own timeout must bound it
          const body = JSON.stringify(r.httpStatus ? { status: 'failed', retcode: r.retcode ?? 1, data: null } : onebotFaultAnswer(r, null));
          if (r.delayMs) setTimeout(() => respond(r.httpStatus ?? 200, body), r.delayMs); else respond(r.httpStatus ?? 200, body);
          return;
        }
        respond(200, JSON.stringify(onebotAnswer(action, params, null)));
        return;
      }
      record({ side: 'onebot', kind: 'http', method: req.method, url: req.url });
      res.writeHead(404, { 'content-type': 'application/json', 'content-length': 2 });
      res.end('{}');
    });
  });
  onebotServer.on('upgrade', (req, socket) => {
    if (onebotUpgradeFault && onebotUpgradeFault.applied < (onebotUpgradeFault.times ?? Infinity)) {
      onebotUpgradeFault.applied += 1;
      const status = onebotUpgradeFault.status ?? 401;
      record({ side: 'onebot', kind: 'onebot-upgrade-refused', status });
      socket.end(`HTTP/1.1 ${status} ${status === 401 ? 'Unauthorized' : 'Refused'}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n`);
      return;
    }
    if (onebotTokens?.ws !== undefined && bearerOf(req) !== onebotTokens.ws) {
      record({ side: 'onebot', kind: 'onebot-upgrade-refused', status: 401, reason: 'token' });
      socket.end('HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
      return;
    }
    wsAccept(req, socket);
    onebotSockets.add(socket);
    record({ side: 'onebot', kind: 'ws-open', auth: req.headers.authorization ?? null });
    wsReader(socket, (text) => {
      let msg;
      try { msg = JSON.parse(text); } catch { return; }
      record({ side: 'onebot', kind: 'action', action: msg.action, params: msg.params ?? {} });
      const fault = onebotFaults.length ? pickFault(onebotFaults, msg.action, msg.params ?? {}, { action: msg.action }, 'action') : null;
      if (fault) {
        const r = fault.respond ?? {};
        record({ side: 'onebot', kind: 'fault-applied', action: msg.action, respond: Object.keys(r).join(',') });
        if (r.closeSocket) { socket.destroy(); return; }
        if (r.hang) return;
        const answer = JSON.stringify(onebotFaultAnswer(r, msg.echo));
        if (r.delayMs) setTimeout(() => wsSend(socket, answer), r.delayMs); else wsSend(socket, answer);
        return;
      }
      wsSend(socket, JSON.stringify(onebotAnswer(msg.action, msg.params ?? {}, msg.echo)));
    }, () => onebotSockets.delete(socket));
    // Scripted inbound events are sent after the bridge has connected.
    setTimeout(() => {
      for (const ev of scenario.inbound) {
        wsSend(socket, JSON.stringify(ev));
        record({ side: 'onebot', kind: 'inbound-sent', message_id: ev.message_id ?? null, group_id: ev.group_id ?? null, user_id: ev.user_id ?? null });
      }
    }, scenario.inboundDelayMs ?? 300);
  });

  // ---- Model: must never be reached on the DSH-mediated path ---------------
  const modelServer = http.createServer((req, res) => {
    record({ side: 'model', kind: 'hit', method: req.method, url: req.url });
    res.writeHead(500);
    res.end('{"error":"model fake: unexpected direct model call"}');
  });

  const listen = (server) => new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve(`127.0.0.1:${server.address().port}`));
  });
  let dshPort = 0;
  const dsh = await listen(dshServer);
  dshPort = dshServer.address().port;
  const onebot = await listen(onebotServer);
  const model = await listen(modelServer);
  return {
    dsh: `http://${dsh}`,
    onebot: `http://${onebot}`,
    onebotWs: `ws://${onebot}`,
    model: `http://${model}`,
    log,
    // ---- fault / lifecycle controls (ticket 25); inert unless called -------------------------------
    /** Every session/selectModel seen, in order: { sessionId, provider, model, effort, ok }. */
    recordedSelects: () => selects.map((e) => ({ ...e })),
    addDshFault: (fault) => { dshFaults.push({ ...fault, seen: 0, applied: 0 }); },
    clearDshFaults: () => { dshFaults.length = 0; },
    dshFaultsApplied: () => dshFaultsApplied,
    addOnebotFault: (fault) => { onebotFaults.push({ ...fault, seen: 0, applied: 0 }); },
    clearOnebotFaults: () => { onebotFaults.length = 0; },
    /** The next `times` WebSocket upgrades on the OneBot port are refused with HTTP `status` (default 401). */
    /** Require these bearer tokens ({ http, ws }) from now on; `null` stops checking. */
    setOnebotTokens: (tokens) => { onebotTokens = tokens ? { ...tokens } : null; },
    setOnebotUpgradeFault: (fault) => { onebotUpgradeFault = fault ? { ...fault, applied: 0 } : null; },
    /** Drops every connected OneBot WebSocket. `code` 1006 (default) is an abrupt drop; other codes send a close frame. Returns the number dropped. */
    closeOnebotSockets: ({ code = 1006 } = {}) => {
      const n = onebotSockets.size;
      for (const s of onebotSockets) { if (code === 1006) s.destroy(); else wsClose(s, code); }
      record({ side: 'onebot', kind: 'sockets-closed', code, count: n });
      return n;
    },
    /** Stops the DSH listener (connections reset, followers and event streams dropped); the port is kept for dshUp. */
    dshDown: () => new Promise((resolve) => {
      record({ side: 'dsh', kind: 'dsh-down' });
      for (const f of followers.values()) f.socket.destroy();
      followers.clear();
      for (const s of eventClients.keys()) s.destroy();
      eventClients.clear();
      dshServer.close(() => resolve());
      dshServer.closeAllConnections();
    }),
    dshUp: () => new Promise((resolve, reject) => {
      dshServer.once('error', reject);
      dshServer.listen(dshPort, '127.0.0.1', () => { dshServer.off('error', reject); record({ side: 'dsh', kind: 'dsh-up' }); resolve(); });
    }),
    /** Changes the DSH signing secret and rewrites the credentials file, so cookies minted before are 401 and a relogin must read the new secret. */
    rotateSecret: (next = crypto.randomBytes(32)) => { secret = next; writeCredentials(dshHome, secret); record({ side: 'dsh', kind: 'secret-rotated' }); },
    currentSecret: () => secret,
    /** DSH's clock runs `ms` off the host from now on and cookies must satisfy its time rule; `null` stops checking. */
    setDshClockOffset: (ms) => { dshClockOffsetMs = ms; record({ side: 'dsh', kind: 'clock-offset', ms }); },
    /** Deliver an explicitly released scripted turn to a synthetic DSH follower (default: the first session). */
    pushDshEvents: (events, sid = sessionId) => pushEvents(sid, events),
    setDshSettings: (value) => { scenario.dsh.settings = value; },
    pushInteraction: (value) => {
      for (const [socket, client] of eventClients) wsSend(socket, JSON.stringify({ type: 'item', streamId: client.streamId, value }));
      record({ side: 'dsh', kind: 'interaction-injected', value });
      return eventClients.size;
    },
    /** Sends one OneBot event to every connected gateway client now (the JS bridge ingests it; Rust does not subscribe in console tests). */
    pushInbound: (event) => {
      for (const s of onebotSockets) wsSend(s, JSON.stringify(event));
      record({ side: 'onebot', kind: 'inbound-sent', message_id: event.message_id ?? null, group_id: event.group_id ?? null, user_id: event.user_id ?? null });
      return onebotSockets.size;
    },
    close: () => new Promise((resolve) => {
      for (const s of onebotSockets) s.destroy();
      for (const f of followers.values()) f.socket.destroy();
      for (const s of eventClients.keys()) s.destroy();
      let pending = 3;
      const done = () => { pending -= 1; if (pending === 0) resolve(); };
      for (const srv of [dshServer, onebotServer, modelServer]) {
        srv.close(done);
        srv.closeAllConnections();
      }
    }),
  };
}

// ---- redaction / leak scans (ticket 25) ------------------------------------------------------
// Pure helpers: they read a request log or text and report findings; they never change a world.

/** Fixture credentials the harness uses; none of them may appear in a redacted log or an alert. */
export const FIXTURE_TOKENS = ['parity-agent-token', 'parity-onebot-token', 'parity-console-token'];

const SECRET_PATTERNS = [
  /dsh-auth-[A-Za-z0-9_-]+(?:=v1\.[A-Za-z0-9_.-]+)?/g, // cookie name and value
  /v1\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}/g, // signed cookie body.signature
  /tok-(?:group|private)-\d+-[a-z]{8,}/g, // per-conversation agent tokens of mcp-world
  /(?<![A-Za-z0-9])[A-Za-z]:(?:\\|\/(?!\/))[^\s"',;)]+/g, // Windows absolute path (not a URL scheme such as ws://)
  /\/(?:root|home|tmp|usr|var|etc|opt|mnt)\/[^\s"',;)]*/g, // POSIX absolute path
];

/** Returns the secrets, tokens and absolute paths found in `text` (empty array when clean). `extra` adds literal strings to look for. */
export function secretsInText(text, { extra = [] } = {}) {
  const found = [];
  const body = String(text ?? '');
  for (const literal of [...FIXTURE_TOKENS, b64url(SECRET), ...extra]) if (literal && body.includes(literal)) found.push(literal);
  for (const re of SECRET_PATTERNS) for (const m of body.matchAll(re)) found.push(m[0]);
  return found;
}

/** Scans files (or every file below a directory) for secrets. { clean, hits: [{ file, found }], missing: [path] }. */
export function redactedLogHasNoSecrets(paths, { extra = [], maxBytes = 8_000_000 } = {}) {
  const hits = [];
  const missing = [];
  const visit = (p) => {
    let st;
    try { st = fs.statSync(p); } catch { missing.push(p); return; }
    if (st.isDirectory()) { for (const n of fs.readdirSync(p)) visit(path.join(p, n)); return; }
    const fd = fs.openSync(p, 'r');
    try {
      const buf = Buffer.alloc(Math.min(st.size, maxBytes));
      fs.readSync(fd, buf, 0, buf.length, 0);
      const found = secretsInText(buf.toString('utf8'), { extra });
      if (found.length) hits.push({ file: p, found });
    } finally { fs.closeSync(fd); }
  };
  for (const p of Array.isArray(paths) ? paths : [paths]) visit(p);
  return { clean: hits.length === 0, hits, missing };
}

/** Text of an OneBot message parameter (string or segment array). */
export function messageText(message) {
  if (typeof message === 'string') return message;
  if (Array.isArray(message)) return message.map((seg) => (typeof seg === 'string' ? seg : seg?.data?.text ?? '')).join('');
  return '';
}

const INTERNAL_TEXT = /处理出错|模型调用失败|所有候选模型|已自动切到|上次运行崩了|崩了|重启|restart|crash|failover|fallback|selectModel|candidate|provider|stack trace|ECONN|ETIMEDOUT|dsh[ -]?(?:down|error)|session\/|turn\/end|Error:|⚠️/i;

/**
 * Group sends (send_group_msg / send_msg with a group_id) whose text carries internal error, restart or model text.
 * Owner private alerts are not group sends and never count. `extra` regexes widen the pattern.
 */
export function groupLeaks(log, { extra = [] } = {}) {
  return log.filter((e) => {
    if (e.side !== 'onebot' || e.kind !== 'action') return false;
    if (!(e.action === 'send_group_msg' || (e.action === 'send_msg' && e.params?.group_id !== undefined) || e.action === 'send_group_forward_msg')) return false;
    const text = messageText(e.params?.message);
    return [INTERNAL_TEXT, ...extra].some((re) => re.test(text));
  });
}
