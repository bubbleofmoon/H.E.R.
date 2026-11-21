// Exercise the selected console in an isolated Chromium profile through DevTools.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';

const [port, profile] = process.argv.slice(2);
const cookie = fs.readFileSync(0, 'utf8').trim();
const separator = cookie.indexOf('=');
assert.ok(port && profile && separator > 0);
const name = cookie.slice(0, separator);
const value = cookie.slice(separator + 1);
const origin = `http://her.localhost:${port}`;
const alias = `http://127.0.0.1:${port}`;
const chrome = process.env.HER_TEST_CHROMIUM || (process.platform === 'win32'
  ? [process.env.ProgramFiles, process.env['ProgramFiles(x86)'], process.env.LOCALAPPDATA]
    .filter(Boolean).map(dir => path.join(dir, 'Google', 'Chrome', 'Application', 'chrome.exe')).find(file => fs.existsSync(file))
  : 'google-chrome');
assert.ok(chrome, 'Chromium is required for the console origin fixture');

const options = [
  '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`,
  '--disable-background-networking', '--disable-component-update', '--disable-extensions',
  '--disable-gpu', '--disable-dev-shm-usage',
  '--no-default-browser-check', '--no-first-run', '--no-proxy-server', 'about:blank',
];
if (process.getuid?.() === 0 || (process.platform === 'linux' && process.env.CI)) options.unshift('--no-sandbox');
const browser = spawn(chrome, options, { windowsHide: true, stdio: ['ignore', 'ignore', 'pipe'] });
let browserStderr = '';
browser.stderr?.on('data', chunk => {
  if (browserStderr.length < 4096) browserStderr += chunk.toString('utf8').slice(0, 4096 - browserStderr.length);
});
const closed = once(browser, 'close').catch(() => {});
let socket;
let next = 0;
const pending = new Map();

async function until(probe, description) {
  const deadline = performance.now() + 30_000;
  while (performance.now() < deadline) {
    if (browser.exitCode !== null) {
      throw new Error(`Chromium exited before the origin fixture completed${browserStderr ? `: ${browserStderr}` : ''}`);
    }
    const result = await probe();
    if (result) return result;
    await delay(30);
  }
  throw new Error(`${description}${browserStderr ? `: ${browserStderr}` : ''}`);
}

function send(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++next;
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`Chromium did not answer ${method}`));
    }, 4_000);
    pending.set(id, response => {
      clearTimeout(timeout);
      if (response.error) reject(new Error(`Chromium refused ${method}`));
      else resolve(response.result);
    });
    socket.send(JSON.stringify({ id, method, params }));
  });
}

async function navigate(url, expected, content) {
  const result = await send('Page.navigate', { url });
  assert.ok(!result.errorText, 'Chromium must reach the IPv4 console listener without a DNS override');
  await until(async () => {
    try {
      const response = await send('Runtime.evaluate', {
        expression: '({url:location.href,text:document.body?.textContent})', returnByValue: true,
      });
      const page = response.result.value;
      return page?.url === expected && (!content || page.text?.includes(content));
    } catch {
      // The navigation can discard the JavaScript context between probes.
      return false;
    }
  }, 'Chromium did not reach the expected console page');
}

try {
  await once(browser, 'spawn');
  const devtools = await until(async () => {
    try {
      const debugPort = fs.readFileSync(path.join(profile, 'DevToolsActivePort'), 'utf8').split(/\r?\n/)[0]?.trim();
      if (!debugPort) return undefined;
      const response = await fetch(`http://127.0.0.1:${debugPort}/json/list`, { signal: AbortSignal.timeout(500) });
      return (await response.json()).find(tab => tab.type === 'page')?.webSocketDebuggerUrl;
    } catch {
      return undefined;
    }
  }, 'Chromium did not publish its DevTools endpoint');
  socket = new WebSocket(devtools);
  socket.addEventListener('message', ({ data }) => {
    const response = JSON.parse(data);
    if (pending.has(response.id)) {
      const callback = pending.get(response.id);
      pending.delete(response.id);
      callback(response);
    }
  });
  await Promise.race([
    once(socket, 'open'),
    delay(4_000, null, { ref: false }).then(() => { throw new Error('Chromium DevTools connection timed out'); }),
  ]);
  await navigate(`${origin}/api/health`, `${origin}/api/health`, '"ok":true');
  const set = await send('Network.setCookie', { url: origin, name, value, path: '/', httpOnly: true, sameSite: 'Strict' });
  assert.equal(set.success, true, 'Chromium must accept the loopback cookie attributes');
  await navigate(`${origin}/api/settings`, `${origin}/api/settings`, '"ok":true');
  for (const host of [alias, `http://localhost:${port}`, `http://[::1]:${port}`]) {
    const { cookies } = await send('Network.getCookies', { urls: [host] });
    assert.ok(cookies.every(cookie => cookie.name !== name), 'the host-only cookie must stay off every alias');
  }
  await navigate(`${alias}/settings?discard=fixture`, `${origin}/settings`);
  console.log('Chromium reached the canonical console and scoped its cookie to the hostname');
} finally {
  socket?.close();
  if (browser.exitCode === null && browser.pid !== undefined) browser.kill();
  await closed;
}
