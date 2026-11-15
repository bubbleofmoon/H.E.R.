// Disposable loopback stand-in for SnowLuma (OneBot), DSH and the model relay in the #67 move rehearsal.
// OneBot and DSH read fixture-resident.json beside this script; the relay reads the -config file it is given.
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
let config;
let relayKey = '';
const configAt = args.findIndex((arg) => arg === '-config' || arg === '--config');
if (configAt >= 0) {
  const text = fs.readFileSync(args[configAt + 1], 'utf8');
  const port = /^port:\s*["']?(\d{1,5})["']?\s*$/m.exec(text);
  const key = /^api-keys:\s*\r?\n\s+-\s*["']?([^"'\r\n]+?)["']?\s*$/m.exec(text);
  if (!port || !key) throw new Error('invalid fixture relay configuration');
  config = { role: 'relay', port: Number(port[1]) };
  relayKey = key[1];
} else {
  config = JSON.parse(fs.readFileSync(path.join(here, 'fixture-resident.json'), 'utf8'));
}
if (!['dsh', 'onebot', 'relay'].includes(config.role) || !Number.isInteger(config.port) || config.port < 1 || config.port > 65535) {
  throw new Error('invalid fixture resident configuration');
}
// The health-failure scenario: DSH refuses to start from the new dependency folder while the marker exists.
const fromNewDeps = here.toLowerCase().split(path.sep).join('/').includes('/deps/dsh/');
if (config.role === 'dsh' && config.failMarker && fromNewDeps && fs.existsSync(config.failMarker)) process.exit(7);
// The bundle scenario: like the real web profile, DSH loads each bundle through the link in its home's plugins folder.
const loaded = [];
if (config.role === 'dsh' && Array.isArray(config.bundles) && process.env.DSH_HOME) {
  for (const name of config.bundles) {
    const link = path.join(process.env.DSH_HOME, 'plugins', 'node_modules', name);
    if (!fs.existsSync(link)) continue;
    const real = fs.realpathSync(link);
    const manifest = JSON.parse(fs.readFileSync(path.join(real, 'package.json'), 'utf8'));
    loaded.push({ name: manifest.name, real });
  }
}
// Diagnostics: the start and every request, appended to the log fixture-resident.json names (never a source folder).
function note(event) {
  if (!config.requestLog) return;
  try { fs.appendFileSync(config.requestLog, `${JSON.stringify({ t: new Date().toISOString(), pid: process.pid, role: config.role, from: here, ...event })}\n`); } catch { /* diagnostics only */ }
}
note({ event: 'start' });

const server = http.createServer((request, response) => {
  const chunks = [];
  request.on('data', (chunk) => chunks.push(chunk));
  request.on('end', () => {
    note({ event: 'request', method: request.method, url: request.url });
    // The late-child scenarios: DSH starts a child process whose command line names no folder of the layout, like a
    // tool server; only its parentage ties it to DSH.
    if (config.role === 'dsh' && request.url === '/fixture/spawn-child') {
      spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)', 'her-67-late-child'], { stdio: 'ignore' }).unref();
    }
    let status = 200;
    let body;
    if (config.role === 'relay') {
      const authorized = request.headers.authorization === `Bearer ${relayKey}`;
      status = request.url === '/v1/models' && authorized ? 200 : 401;
      body = { object: 'list', data: [] };
    } else if (config.role === 'dsh' && request.url === '/fixture/bundles') {
      body = { loaded };
    } else if (config.role === 'dsh') {
      let rpcId = null;
      try { rpcId = JSON.parse(Buffer.concat(chunks).toString('utf8')).rpcId ?? null; } catch { rpcId = null; }
      body = { type: 'server-response', rpcId, result: { ok: true, value: {} } };
    } else {
      body = { status: 'ok', retcode: 0, data: { online: true, good: true } };
    }
    // An explicit Content-Length and Connection: close, like the real gateway. HER's OneBot client reads the raw body
    // to the end of the connection and does not decode chunked transfer encoding (her/src/onebot.rs read_response_until);
    // writeHead without a length makes Node send the body chunked, which that client cannot parse (the likely cause
    // of the R4 early run's S5 health failure; the harness framing check guards it).
    const text = JSON.stringify(body);
    response.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(text), connection: 'close' });
    response.end(text);
  });
});
if (config.role === 'onebot') {
  server.on('upgrade', (request, socket) => {
    const key = request.headers['sec-websocket-key'];
    if (typeof key !== 'string') { socket.destroy(); return; }
    const accept = crypto.createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    socket.on('error', () => {});
    socket.on('data', () => {});
  });
}
server.listen(config.port, '127.0.0.1');
