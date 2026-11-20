import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import fs from 'node:fs/promises';
import { createServer } from 'node:http';
import { createServer as reserveSocket } from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { setTimeout as delay } from 'node:timers/promises';
import { developmentRoot } from '../../../scripts/fixture-paths.mjs';
import { startFakeWorld, writeCredentials } from '../fixtures/fake-world.mjs';
import { baseConfig, CONSOLE_TOKEN, convRecord, GROUP_KEY, onebotResponder } from '../fixtures/rust-world.mjs';

export { CONSOLE_TOKEN, GROUP_KEY };

async function releaseFixtureTrust() {
  const keys = await Promise.all(['primary', 'backup'].map(async name => {
    const key = new URL(`../fixtures/release-signing/${name}.pub`, import.meta.url);
    return (await fs.readFile(key, 'utf8')).trim();
  }));
  // Match the empty install root's bootstrap stamp in a release-test-keys build.
  return createHash('sha256').update(keys.join('\n')).digest('hex');
}

async function listen(server) {
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return server.address().port;
}

function close(server) {
  return new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
}

async function startConsole(root, cli, env, configurePort) {
  const attempts = [];
  const triedPorts = new Set();
  for (let attempt = 0; attempt < 3; attempt++) {
    let reservation, port;
    const repeated = [];
    try {
      do {
        reservation = reserveSocket();
        port = await listen(reservation);
        if (triedPorts.has(port)) repeated.push(reservation);
      } while (triedPorts.has(port));
    } finally {
      await Promise.all(repeated.map(close));
    }
    triedPorts.add(port);
    const origin = `http://127.0.0.1:${port}`;
    try {
      await configurePort(port);
    } finally {
      await close(reservation);
    }

    const child = spawn(cli, ['--runtime-root', root, '--console-serve', '--console-port', String(port)],
      { cwd: root, env, windowsHide: true, stdio: ['ignore', 'ignore', 'pipe'] });
    let stderr = '';
    child.stderr.setEncoding('utf8').on('data', chunk => { stderr = (stderr + chunk).slice(-4000); });
    const exited = new Promise(resolve => {
      child.once('close', resolve);
      child.once('error', error => { stderr += String(error); });
    });
    const stop = async () => {
      child.kill('SIGKILL');
      await exited;
    };
    let ready = false;
    try {
      const deadline = Date.now() + 7000;
      while (child.pid && child.exitCode === null && Date.now() < deadline) {
        const health = await fetch(origin + '/api/health', {
          signal: AbortSignal.timeout(400), redirect: 'error',
        }).then(async response => response.status === 200 ? response.json() : null).catch(() => null);
        if (health?.ok && health.pid === child.pid && health.port === port) {
          ready = true;
          return { origin, stop };
        }
        await delay(100);
      }
    } finally {
      if (!ready) await stop();
    }
    attempts.push(`port ${port}, exit ${child.exitCode}: ${stderr}`);
  }
  throw Error('Fixture console never reported its own PID and port\n' + attempts.join('\n'));
}

export async function withFixture(cli, action) {
  const parent = await fs.realpath(os.tmpdir());
  const workspace = await fs.mkdtemp(path.join(parent, 'her-console-ui-'));
  const root = path.join(workspace, 'runtime');
  const home = path.join(workspace, 'profile');
  const dshHome = path.join(root, 'user', 'dsh');
  let world, feed, console;
  try {
    developmentRoot(root, cli);
    for (const dir of ['config', 'data/lifecycle', 'resources/roles']) {
      await fs.mkdir(path.join(root, dir), { recursive: true });
    }
    for (const dir of ['Roaming', 'Local', 'tmp']) {
      await fs.mkdir(path.join(home, dir), { recursive: true });
    }
    await fs.mkdir(dshHome, { recursive: true });
    world = await startFakeWorld({
      dshHome,
      scenario: {
        selfId: 401, inbound: [], onebotResponder,
        dsh: { sessionId: 'fixture', workspaceId: 'fixture', presets: [], provider: 'fixture', model: 'fixture' },
      },
    });
    writeCredentials(dshHome);
    feed = createServer((request, response) => {
      const registry = request.url.toLowerCase() === '/@deepseek-ai%2fdsh';
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify(registry ? { 'dist-tags': { latest: '2.0.0' }, versions: { '2.0.0': {} } } : []));
    });
    const feedUrl = `http://127.0.0.1:${await listen(feed)}`;
    const write = (file, value) => fs.writeFile(path.join(root, file), JSON.stringify(value));
    const config = baseConfig(world, {
      consoleToken: CONSOLE_TOKEN,
      dsh: { baseUrl: world.dsh, provider: 'fixture', model: 'fixture' },
      snowluma: { httpUrl: world.onebot, wsUrl: world.onebotWs },
      update: { policy: 'notify', feedUrl },
      dependencies: { dsh: { registry: feedUrl, version: '1.0.0' }, snowluma: { feedUrl } },
      harness: { routes: [{ id: 'first', model: 'fixture' }, { id: 'second', model: 'fixture' }] },
      backup: { dir: path.join(workspace, 'backups') },
      time: { zone: 'UTC' },
    });
    await write('dependencies.json', { dsh: world.dsh, model: world.model, onebot: world.onebot, onebot_ws: world.onebotWs });
    await write('data/mode.json', { mode: 'chat' });
    await write('data/releases.json', {
      version: 3, trust: await releaseFixtureTrust(), feedUrl, available: { tag: 'v99.0.0', assets: [] },
    });
    await write('data/social-v2.json', { conversations: { [GROUP_KEY]: convRecord(GROUP_KEY, {
      wakeWordLog: [{ id: 1, at: 1, by: 'her', added: { keywords: [], dropNames: [] },
        removed: { keywords: ['fixture'], dropNames: [] } }],
    }) } });
    const memory = new DatabaseSync(path.join(root, 'data/memory.db'));
    try {
      memory.exec('CREATE TABLE fixture (note TEXT)');
    } finally {
      memory.close();
    }
    await fs.writeFile(path.join(root, 'resources/roles/Fixture.md'), 'A synthetic role for the console fixture.\n');
    const env = {
      PATH: process.env.PATH, SystemRoot: process.env.SystemRoot, WINDIR: process.env.WINDIR,
      USERNAME: process.env.USERNAME, HOME: home, USERPROFILE: home, DSH_HOME: dshHome,
      APPDATA: path.join(home, 'Roaming'), LOCALAPPDATA: path.join(home, 'Local'),
      TEMP: path.join(home, 'tmp'), TMP: path.join(home, 'tmp'), TZ: 'UTC',
      HER_INSTALL_ROOT: path.join(workspace, 'unused-install-root'),
      HER_TASK_SCHEDULER_FILE: path.join(workspace, 'scheduler.xml'),
    };
    console = await startConsole(root, cli, env, consolePort => write('config/config.json', { ...config, consolePort }));
    const api = async (route, body) => {
      const response = await fetch(console.origin + route, {
        method: body === undefined ? 'GET' : 'POST', redirect: 'error', signal: AbortSignal.timeout(5000),
        headers: { 'x-console-token': CONSOLE_TOKEN, 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      const data = await response.json();
      assert.equal(response.status, 200, `${route}: ${JSON.stringify(data)}`);
      assert.equal(data.ok, true, route);
      return data;
    };
    await action({ origin: console.origin, api, backupDir: path.join(workspace, 'backups'), root, env });
  } finally {
    try {
      await Promise.allSettled([console?.stop(), world?.close(), feed?.listening ? close(feed) : undefined])
        .then(results => {
          const failures = results.filter(result => result.status === 'rejected');
          if (failures.length) throw new AggregateError(failures.map(result => result.reason), 'Fixture cleanup failed');
        });
    } finally {
      await fs.rm(workspace, { recursive: true, force: true, maxRetries: 10 });
    }
  }
}
