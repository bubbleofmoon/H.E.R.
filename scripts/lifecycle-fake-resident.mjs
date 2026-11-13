// Disposable loopback-only DSH/OneBot stand-in for the Windows lifecycle fixture.
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const directory = path.dirname(fileURLToPath(import.meta.url));
const config = JSON.parse(fs.readFileSync(path.join(directory, 'fixture-resident.json'), 'utf8'));
if (!['dsh', 'onebot'].includes(config.role) || !Number.isInteger(config.port) || config.port < 1 || config.port > 65535) {
  throw new Error('invalid fixture resident configuration');
}
const server = http.createServer((request, response) => {
  request.resume();
  response.writeHead(200, { 'content-type': 'application/json' });
  response.end(JSON.stringify({ status: 'ok', retcode: 0, data: { good: true } }));
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
