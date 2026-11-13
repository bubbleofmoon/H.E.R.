// Local-only supervisor dependency fixture. Never records credentials or headers.
import http from 'node:http';
import fs from 'node:fs';
const cfg = JSON.parse(fs.readFileSync(new URL('./fixture.json', import.meta.url)));
const key = fs.readFileSync(new URL('./client-key', import.meta.url), 'utf8').trim();
fs.appendFileSync(new URL('./attempts', import.meta.url), process.pid + '\n');
// Deliberately hostile output: the supervisor must not retain upstream diagnostics.
console.log(key); console.error(key);
const server = http.createServer((req, res) => {
  if (req.url !== '/v1/models' || req.method !== 'GET') {
    fs.appendFileSync(new URL('./unexpected-request', import.meta.url), 'unexpected\n');
    res.writeHead(400); res.end(); return;
  }
  fs.appendFileSync(new URL('./probes', import.meta.url), 'probe\n');
  const mode = fs.existsSync(new URL('./mode', import.meta.url))
    ? fs.readFileSync(new URL('./mode', import.meta.url), 'utf8').trim() : 'ready';
  if (mode === 'timeout') return;
  const status = req.headers.authorization !== `Bearer ${key}` ? 401
    : mode === 'ready' ? 200 : Number(mode) || 503;
  const body = JSON.stringify(status === 200 ? {object:'list',data:[]}: {error:key});
  res.writeHead(status, {'Content-Length':Buffer.byteLength(body),'Connection':'close'});
  res.end(body);
});
server.listen(cfg.port, '127.0.0.1', () => fs.writeFileSync(new URL('./listening', import.meta.url), 'ready'));
setTimeout(() => process.exit(0), 300000).unref();
