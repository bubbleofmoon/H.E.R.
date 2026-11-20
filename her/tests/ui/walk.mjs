import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { GROUP_KEY } from './fixture.mjs';
import { OWNER } from '../fixtures/rust-world.mjs';

export const unavailablePages = {
  root: 'The legacy / page needs the separately distributed console.html. The public fixture has no copy.',
};

export const pages = {
  root: { route: '/', themes: ['light', 'dark'] },
  personas: { route: '/personas', ready: ['/api/personas'], marker: '#list' },
  schedule: {
    route: '/schedule', aliases: ['/tasks'],
    ready: ['/api/releases', '/api/dependencies', '/api/backups/settings', '/api/tasks', '/api/schedule',
      '/api/tasks/runs', '/api/backups'],
    marker: '#legacy tbody tr',
  },
  registry: { route: '/settings', ready: ['/api/settings', '/api/settings/history'], marker: '#list tbody tr' },
  conversations: {
    route: '/settings/conversations', ready: ['/api/settings/conversations'], marker: '#list tbody tr',
  },
  models: { route: '/settings/models', ready: ['/api/settings/models'], marker: '#routes tbody tr' },
};

export async function usable(control, enabled = false) {
  await control.waitFor({ state: 'visible' }); // Strict mode also rejects duplicate matching controls.
  if (enabled) await control.click({ trial: true });
}

function responseMatches(response, method, route) {
  const url = new URL(response.url());
  return response.request().method() === method
    && (typeof route === 'string' ? url.pathname === route : route.test(url.pathname + url.search));
}

export async function observe(page, method, route, action, status = 200, ok = status < 400) {
  const [response] = await Promise.all([
    page.waitForResponse(response => responseMatches(response, method, route)),
    action(),
  ]);
  const body = await response.json();
  assert.equal(response.status(), status, `${method} ${route}: ${JSON.stringify(body)}`);
  assert.equal(body.ok, ok, `${method} ${route}: ${JSON.stringify(body)}`);
  return body;
}

function click(page, control, method, route, status) {
  return observe(page, method, route, async () => {
    await usable(control, true);
    await control.click();
  }, status);
}

export async function openPage(page, origin, route, spec) {
  // Drain the previous document's polling or refresh requests before arming these listeners.
  if (page.url() !== 'about:blank') await page.goto('about:blank');
  const pending = spec.ready.map(route => page.waitForResponse(response => responseMatches(response, 'GET', route)));
  const [navigation, ...responses] = await Promise.all([page.goto(origin + route), ...pending]);
  assert.equal(navigation.status(), 200, route);
  for (const response of responses) {
    assert.equal(response.status(), 200, response.url());
    assert.equal((await response.json()).ok, true, response.url());
  }
  await (spec.marker.endsWith(' tr') ? page.locator(spec.marker).first() : page.locator(spec.marker))
    .waitFor({ state: 'visible' });
}

const tomorrow = () => new Date(Date.now() + 24 * 60 * 60 * 1000).toISOString().slice(0, 16);
const button = (row, name) => row.getByRole('button', { name, exact: true });
const setting = (page, key) => page.locator('#list tbody tr').filter({
  has: page.locator('td:first-child', { hasText: `(${key})` }),
});
const conversation = (page, table) => page.locator(`#${table} tbody tr`).filter({
  has: page.getByRole('cell', { name: GROUP_KEY, exact: true }),
});
const conversationSetting = (page, key) => conversation(page, 'list').filter({
  has: page.getByRole('cell', { name: key, exact: true }),
});
const settingsSet = '/api/settings/set';
const history = '/api/settings/history';
const personaItem = '/api/personas/item';
const setConversation = '/api/settings/conversation/set';
const resetConversation = '/api/settings/conversation/reset';

export const walks = {
  personas: {
    list: ({ page }) => click(page, page.locator('#use'), 'GET', '/api/personas'),
    import: ({ page }) => click(page, page.locator('#import'), 'POST', '/api/personas/import-roles'),
    create: async ({ page, state }) => {
      await page.locator('#name').fill('Fixture persona');
      await page.locator('#core').fill('A synthetic persona for the browser walk.');
      state.persona = (await click(page, page.locator('#save'), 'POST', '/api/personas/create')).id;
      await page.locator('#versions tbody tr').waitFor();
    },
    'edit-load': async ({ page, state }) => {
      await page.locator('#id').fill(state.persona);
      await click(page, page.locator('#load'), 'GET', personaItem);
    },
    versions: async ({ page }) => {
      await click(page, page.locator('#load'), 'GET', personaItem);
      await usable(page.locator('#versions tbody tr').filter({ has: page.getByRole('cell', { name: '1', exact: true }) }));
    },
    'save-version': async ({ page }) => {
      await page.locator('#notes').fill('Fixture notes.');
      const [item, saved] = await Promise.all([
        page.waitForResponse(response => responseMatches(response, 'GET', personaItem)),
        click(page, page.locator('#save'), 'POST', '/api/personas/update'),
      ]);
      assert.equal(saved.version, 2);
      assert.equal(item.status(), 200);
    },
    approve: ({ page }) => click(page, button(page.locator('#versions tbody tr').filter({
      has: page.locator('td:first-child', { hasText: /^2$/ }),
    }), 'Approve'), 'POST', '/api/personas/approve'),
    'assign-default': async ({ page, state }) => {
      await page.locator('#default').fill(state.persona);
      await click(page, page.locator('#setDefault'), 'POST', '/api/personas/assign');
    },
    'assign-override': async ({ page, state }) => {
      await page.locator('#conv').fill(GROUP_KEY);
      await page.locator('#convPersona').fill(state.persona);
      await click(page, page.locator('#setOverride'), 'POST', '/api/personas/assign');
    },
    duplicate: async ({ page, state }) => {
      await page.locator('#name').fill('Fixture copy');
      state.copy = (await click(page, page.locator('#duplicate'), 'POST', '/api/personas/duplicate')).id;
      await page.getByRole('cell', { name: 'Fixture copy', exact: true }).waitFor();
    },
    delete: async ({ page, state }) => {
      await page.locator('#id').fill(state.copy);
      await click(page, page.locator('#delete'), 'POST', '/api/personas/delete');
    },
    'wake-load': async ({ page }) => {
      await page.locator('#wwConv').fill(GROUP_KEY);
      await click(page, page.locator('#wwLoad'), 'GET', '/api/personas/wake-words');
    },
    'wake-undo': ({ page }) => click(page, button(page.locator('#wwLog'), 'Undo'), 'POST', '/api/personas/wake-words/undo'),
  },
  schedule: {
    'release-status': async ({ page, origin }) => {
      await openPage(page, origin, '/schedule', pages.schedule);
      await usable(page.locator('#release-available'));
    },
    // The development image cannot approve installation updates. Exercise the real refusal.
    'release-approve': ({ page }) => click(page, page.locator('#approve-update'), 'POST', '/api/releases/approve', 409),
    'release-policy': async ({ page }) => {
      await page.locator('#update-policy').selectOption('auto');
      await click(page, page.locator('#update-settings button'), 'POST', '/api/releases/settings');
    },
    dependencies: async ({ page, origin }) => {
      await openPage(page, origin, '/schedule', pages.schedule);
      await usable(page.locator('#dependency-list p').filter({ hasText: /^dsh:/ }));
    },
    'dependencies-check': ({ page }) => click(page, page.locator('#check-dependencies'), 'POST', '/api/dependencies/check'),
    'dependencies-approve': ({ page }) => click(page, button(page.locator('#dependency-list p').filter({ hasText: /^dsh:/ }),
      '批准升级'), 'POST', '/api/dependencies/approve'),
    'backup-settings': async ({ page }) => {
      await page.locator('#backup-days').fill('5');
      await click(page, page.locator('#backup-save'), 'POST', '/api/backups/settings');
    },
    'backup-run': async ({ page, api, state }) => {
      const [response, run] = await Promise.all([
        page.waitForResponse(response => responseMatches(response, 'GET', /^\/api\/tasks\/runs\/backup-[^/?]+$/)),
        click(page, page.locator('#backup-settings [data-run="backup"]'), 'POST', '/api/tasks/backup/run', 202),
      ]);
      state.runId = run.runId;
      assert.equal(response.status(), 200);
      assert.equal((await response.json()).run.runId, state.runId);
      await page.locator('#logrun').filter({ hasText: state.runId }).waitFor({ timeout: 30_000 });
      assert.equal((await api('/api/tasks/runs/' + state.runId)).run.state, 'succeeded');
    },
    'backup-list': ({ page }) => click(page, page.locator('#backup-refresh'), 'GET', '/api/backups'),
    'backup-preview': async ({ page, state, prompt }) => {
      const restore = page.locator('#backup-list [data-restore]');
      await usable(restore, true);
      state.archive = await restore.getAttribute('data-restore');
      const dialog = prompt(d => d.dismiss());
      await Promise.all([dialog, click(page, restore, 'POST', '/api/backups/restore/preview')]);
    },
    // Model a stale selection at confirmation, before any console restart can be spawned.
    'backup-restore': async ({ page, state, backupDir, prompt }) => {
      const archive = path.join(backupDir, path.basename(state.archive));
      const parked = archive + '.parked';
      const dialog = prompt(async d => {
        await fs.rename(archive, parked);
        await d.accept();
      });
      try {
        await Promise.all([dialog, click(page, page.locator('#backup-list [data-restore]'), 'POST', '/api/backups/restore', 503)]);
      } finally {
        await fs.rename(parked, archive).catch(error => {
          if (error.code !== 'ENOENT') throw error;
        });
      }
    },
    upcoming: async ({ page, origin }) => {
      await openPage(page, origin, '/schedule', pages.schedule);
      await page.locator('#tab-upcoming').click();
      await usable(page.locator('#schedule-upcoming'));
    },
    'reminder-new': async ({ page, state }) => {
      await page.locator('#create-conversation').fill(GROUP_KEY);
      await page.locator('#create-at').fill(tomorrow());
      await page.locator('#create-text').fill('Fixture reminder');
      const created = await click(page, page.locator('#schedule-create button'), 'POST', '/api/schedule');
      state.reminder = '[data-sched="item:' + created.item.id + '"]';
    },
    'reminder-pause': async ({ page, state }) => {
      await click(page, page.locator(state.reminder + '[data-act="pause"]'), 'POST', /^\/api\/schedule\/item:[^/]+\/pause$/);
      await click(page, page.locator(state.reminder + '[data-act="resume"]'), 'POST', /^\/api\/schedule\/item:[^/]+\/resume$/);
    },
    'reminder-edit': async ({ page, state, prompt }) => {
      const edited = click(page, page.locator(state.reminder + '[data-act="edit"]'), 'POST', /^\/api\/schedule\/item:[^/]+\/edit$/);
      await Promise.all([edited, prompt(d => d.accept(tomorrow())), prompt(d => d.accept('Updated fixture reminder'))]);
    },
    'reminder-cancel': ({ page, state }) => click(page, page.locator(state.reminder + '[data-act="cancel"]'),
      'POST', /^\/api\/schedule\/item:[^/]+\/cancel$/),
    // Restart is a job action on this page. The real route refuses because this console is unmanaged.
    'job-run': async ({ page, prompt }) => {
      await Promise.all([
        prompt(dialog => dialog.accept()), prompt(dialog => dialog.accept()),
        click(page, page.locator('#schedule-upcoming [data-run="console-restart"]'), 'POST', '/api/restart', 503),
      ]);
    },
    'reminder-history': ({ page }) => click(page, page.locator('#tab-history'), 'GET', '/api/schedule/history'),
    'run-history': async ({ page, state }) => {
      await observe(page, 'GET', /^\/api\/tasks\/runs\?(?:[^&]+&)*task=backup(?:&|$)/,
        () => page.locator('#filter').selectOption('backup'));
      await usable(page.locator(`[data-log="${state.runId}"]`), true);
    },
    'run-log': ({ page, state }) => click(page, page.locator(`[data-log="${state.runId}"]`),
      'GET', `/api/tasks/runs/${state.runId}/log`),
    'windows': ({ page }) => usable(page.locator('#legacy')),
    lifecycle: async ({ page, prompt }) => {
      for (const state of ['maintenance', 'stopped']) {
        await Promise.all([
          prompt(dialog => dialog.accept()), prompt(dialog => dialog.accept()),
          click(page, page.locator(`[data-state="${state}"]`), 'POST', '/api/tasks/lifecycle', 503),
        ]);
      }
      await click(page, page.locator('[data-state="running"]'), 'POST', '/api/tasks/lifecycle');
    },
  },
  registry: {
    list: ({ page }) => click(page, page.locator('#use'), 'GET', '/api/settings'),
    set: async ({ page }) => {
      const row = setting(page, 'harness.model');
      await row.locator('input').fill('fixture-alt');
      await click(page, button(row, 'Set'), 'POST', settingsSet);
    },
    reset: ({ page }) => click(page, button(setting(page, 'harness.model'), 'Reset'), 'POST', '/api/settings/reset'),
    'security-confirm': async ({ page, prompt }) => {
      const row = setting(page, 'ownerQQ');
      await row.locator('input').fill(String(OWNER));
      await Promise.all([
        prompt(async dialog => {
          assert.equal(dialog.type(), 'confirm');
          assert.match(dialog.message(), /security setting/);
          await dialog.accept();
        }),
        click(page, button(row, 'Set'), 'POST', settingsSet),
      ]);
    },
    history: async ({ page, origin }) => {
      await openPage(page, origin, '/settings', pages.registry);
      await usable(page.locator('#history'));
    },
    undo: async ({ page }) => {
      const row = page.locator('#history tbody tr').filter({
        has: page.getByRole('cell', { name: 'harness.model', exact: true }),
      }).filter({ has: page.getByRole('cell', { name: '"fixture-alt"', exact: true }).and(page.locator('td:nth-child(3)')) });
      await click(page, button(row, 'Undo'), 'POST', '/api/settings/undo');
    },
    older: async ({ page, api, origin }) => {
      for (let i = 0; i <= 100; i++) await api(settingsSet, { setting: 'harness.model', value: `history-fixture-${i}` });
      await openPage(page, origin, '/settings', pages.registry);
      await click(page, page.locator('#older'), 'GET', /^\/api\/settings\/history\?before=\d+$/);
    },
  },
  conversations: {
    list: ({ page }) => click(page, page.locator('#use'), 'GET', '/api/settings/conversations'),
    set: async ({ page }) => {
      const row = conversationSetting(page, 'conversation.route');
      await row.locator('input').fill('second');
      await click(page, button(row, 'Set'), 'POST', setConversation);
    },
    switch: async ({ page }) => {
      const row = conversationSetting(page, 'conversation.engine');
      await row.locator('select').selectOption('harness');
      await click(page, button(row, 'Switch'), 'POST', '/api/settings/conversation/switch');
    },
    reset: ({ page }) => click(page, button(conversationSetting(page, 'conversation.route'), 'Reset'), 'POST', resetConversation),
    history: ({ page }) => click(page, button(conversationSetting(page, 'conversation.route'), 'History'), 'GET', history),
    undo: async ({ page }) => {
      const reset = page.locator('#history tbody tr').filter({
        has: page.getByRole('cell', { name: '"second"', exact: true }).and(page.locator('td:nth-child(3)')),
      });
      await click(page, button(reset, 'Undo'), 'POST', '/api/settings/undo');
    },
  },
  models: {
    view: ({ page }) => click(page, page.locator('#use'), 'GET', '/api/settings/models'),
    set: async ({ page }) => {
      const row = conversation(page, 'conversations');
      await row.locator('select').selectOption('first');
      await click(page, button(row, 'Set'), 'POST', setConversation);
    },
    reset: ({ page }) => click(page, button(conversation(page, 'conversations'), 'Use configured order'), 'POST', resetConversation),
  },
};
