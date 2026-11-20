// Local-only supervisor dependency fixture. Never records credentials or headers.
import http from 'node:http';
import fs from 'node:fs';
const cfg = JSON.parse(fs.readFileSync(new URL('./fixture.json', import.meta.url)));
const key = fs.readFileSync(new URL('./client-key', import.meta.url), 'utf8').trim();
fs.appendFileSync(new URL('./attempts', import.meta.url), process.pid + '\n');
// Deliberately hostile output: the supervisor must not retain upstream diagnostics.
console.log(key); console.error(key);
const offlineFile = new URL('./offline', import.meta.url);
let offline = fs.existsSync(offlineFile);
let scriptedProbe = 0;
const server = http.createServer((req, res) => {
  if (req.url !== '/v1/models' || req.method !== 'GET') {
    fs.appendFileSync(new URL('./unexpected-request', import.meta.url), 'unexpected\n');
    res.writeHead(400); res.end(); return;
  }
  fs.appendFileSync(new URL('./probes', import.meta.url), 'probe\n');
  const mode = fs.existsSync(new URL('./mode', import.meta.url))
    ? fs.readFileSync(new URL('./mode', import.meta.url), 'utf8').trim() : 'ready';
  if (mode === 'timeout') return;
  const respond = (requestedStatus) => {
    const status = req.headers.authorization === `Bearer ${key}` ? requestedStatus : 401;
    const body = JSON.stringify(status === 200 ? {object:'list',data:[]}: {error:key});
    res.writeHead(status, {'Content-Length':Buffer.byteLength(body),'Connection':'close'});
    res.end(body);
  };
  if (mode === 'scripted') {
    const index = scriptedProbe++;
    const timer = setInterval(() => {
      const lines = fs.readFileSync(new URL('./probe-responses', import.meta.url), 'utf8').split('\n');
      if (index >= lines.length - 1) return;
      clearInterval(timer);
      const [status, action] = lines[index].trim().split(' ');
      if (action === 'offline') {
        fs.writeFileSync(offlineFile, '');
        updateListener();
      }
      respond(Number(status));
    }, 20);
    res.once('close', () => clearInterval(timer));
  } else if (mode === 'slow' || mode === 'late') {
    const timer = setTimeout(() => respond(200), mode === 'slow' ? 1250 : 4200);
    res.once('close', () => clearTimeout(timer));
  } else {
    respond(mode === 'ready' ? 200 : Number(mode) || 503);
  }
});
function listen() {
  const modeFile = new URL('./mode', import.meta.url);
  const connectTimeout = fs.readFileSync(modeFile, 'utf8').trim() === 'connect-timeout';
  server.listen({port: cfg.port, host: '127.0.0.1', backlog: connectTimeout ? 1 : 511}, () => {
    fs.writeFileSync(new URL('./listening', import.meta.url), 'ready');
    if (connectTimeout) {
      // The Linux test fills the kernel's listen queue while the fixture cannot accept.
      fs.writeFileSync(new URL('./connect-timeout-ready', import.meta.url), 'ready');
      const deadline = performance.now() + 15000;
      const wait = new Int32Array(new SharedArrayBuffer(4));
      while (performance.now() < deadline && fs.readFileSync(modeFile, 'utf8').trim() === 'connect-timeout') {
        Atomics.wait(wait, 0, 0, 20);
      }
    }
  });
}

function updateListener() {
  const shouldBeOffline = fs.existsSync(offlineFile);
  if (offline === shouldBeOffline) return;
  offline = shouldBeOffline;
  if (offline) server.close();
  else listen();
}

if (!offline) listen();
setInterval(updateListener, 20);
setTimeout(() => process.exit(0), 300000).unref();
