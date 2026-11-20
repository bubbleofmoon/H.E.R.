import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';
import { chromium } from 'playwright';
import { CONSOLE_TOKEN } from './fixture.mjs';

const exec = promisify(execFile);

async function desktopEndpoint(profile, child) {
  const endpointFile = path.join(profile, 'EBWebView', 'DevToolsActivePort');
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline && child.exitCode === null && child.signalCode === null) {
    const endpoint = await fs.readFile(endpointFile, 'utf8').catch(error => {
      if (error.code !== 'ENOENT') throw error;
      return '';
    });
    const [port, socket] = endpoint.trim().split(/\r?\n/);
    if (/^\d{1,5}$/.test(port) && Number(port) > 0 && Number(port) <= 65535
      && /^\/devtools\/browser\/[a-f\d-]+$/i.test(socket)) {
      return `ws://127.0.0.1:${port}${socket}`;
    }
    await delay(100);
  }
  assert.equal(child.exitCode, null, 'HER desktop exited before WebView2 announced its CDP endpoint');
  throw Error('WebView2 did not announce a CDP endpoint in its fixture profile within 30 seconds');
}

export async function withWebView(cli, fixture, run) {
  const { root, env, origin } = fixture;
  const tokenFile = path.join(root, 'data', 'console-token');
  await fs.writeFile(tokenFile, CONSOLE_TOKEN);
  await exec('icacls.exe', [tokenFile, '/inheritance:r', '/grant:r', `${env.USERNAME}:(F)`,
    '*S-1-5-18:(F)', '*S-1-5-32-544:(F)'], { env, windowsHide: true, timeout: 10_000 });
  const profile = path.join(root, 'data', 'desktop-webview2');
  const desktop = path.join(path.dirname(cli), 'her.exe');
  const child = spawn(desktop, ['app', '--runtime-root', root], {
    cwd: root, windowsHide: true, stdio: 'ignore',
    env: {
      ...env, WEBVIEW2_USER_DATA_FOLDER: profile,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: '--remote-debugging-port=0 --disable-background-networking',
    },
  });
  const closed = new Promise(resolve => child.once('close', resolve));
  const spawned = new Promise((resolve, reject) => {
    child.once('spawn', resolve);
    child.once('error', reject);
  });
  let browser;
  try {
    await spawned;
    const endpoint = await desktopEndpoint(profile, child);
    browser = await chromium.connectOverCDP(endpoint, { timeout: 8000, noDefaults: true });
    const [context] = browser.contexts();
    assert(context, 'CDP did not expose the desktop WebView2 context');
    const page = context.pages()[0] ?? await context.waitForEvent('page', { timeout: 8000 });
    await page.waitForURL(url => url.origin === origin && url.searchParams.has('token'),
      { timeout: 30_000, waitUntil: 'domcontentloaded' })
      .catch(() => { throw Error('HER desktop did not sign in and navigate its WebView2 to the fixture console'); });
    const session = new URL(page.url()).searchParams.get('token');
    assert(session && session !== CONSOLE_TOKEN, 'HER desktop did not issue its own console session');
    await run(page, session, browser.version());
  } finally {
    try {
      await browser?.close();
    } finally {
      if (child.pid && child.exitCode === null && child.signalCode === null) child.kill();
      await closed;
    }
  }
}
