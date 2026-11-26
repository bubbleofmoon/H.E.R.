import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { request as httpRequest } from 'node:http';
import path from 'node:path';
import { GROUP2_KEY, GROUP_KEY } from './fixture.mjs';
import { OWNER, TOKENS } from '../fixtures/rust-world.mjs';

export const unavailablePages = {};

export const pages = {
  overview: { route: '/', marker: 'h1', heading: '总览', ready: ['/api/status', '/api/pending'] },
  conversations: {
    route: '/conversations', marker: 'table tbody tr', heading: '会话', ready: ['/api/socialV2/states'],
  },
  persona: { route: '/persona', marker: 'h1', heading: '人设与提示词', ready: ['/api/personas'] },
  memory: { route: '/memory', marker: 'h1', heading: '记忆与知识', ready: [] },
  behavior: { route: '/behavior', marker: 'h1', heading: '参与模式与参数', ready: ['/api/status'] },
  models: { route: '/models', marker: 'h1', heading: '模型与用量', ready: ['/api/settings/models'] },
  settings: { route: '/settings', marker: 'h1', heading: '全部设置与变更历史', ready: ['/api/settings'] },
  schedule: { route: '/schedule', aliases: ['/tasks'], marker: 'h1', heading: '日程与任务', ready: ['/api/schedule'] },
  system: { route: '/system', marker: 'h1', heading: '运维与备份', ready: ['/api/tasks', '/api/health'] },
  audit: { route: '/audit', marker: 'h1', heading: '审计与恢复', ready: ['/api/audit/recent'] },
  legacy: { route: '/legacy', marker: 'h1', heading: '旧版工具隔离区', ready: ['/api/social'] },
  auth: { route: '/login', marker: 'h1', heading: 'HER 控制台登录', ready: ['/api/auth/status'] },
  security: { route: '/security', marker: 'h1', heading: '权限与安全', ready: ['/api/whitelist', '/api/security'] },
};

export async function usable(control, enabled = false) {
  await control.waitFor({ state: 'visible', timeout: 8000 });
  if (enabled) await control.click({ trial: true });
}

function responseMatches(response, method, route) {
  const url = new URL(response.url());
  return response.request().method() === method
    && (typeof route === 'string' ? url.pathname === route : route.test(url.pathname + url.search));
}

export async function observe(page, method, route, action, status = 200, ok = status < 400, checkRequest) {
  const statuses = Array.isArray(status) ? status : [status];
  const [response] = await Promise.all([
    page.waitForResponse(res => responseMatches(res, method, route))
      .catch(error => { throw Error(`Waiting for ${method} ${route}: ${error.message}`); }),
    action(),
  ]);
  if (checkRequest) checkRequest(response.request());
  const body = await response.json();
  assert(body && typeof body === 'object' && !Array.isArray(body),
    `${method} ${route}: expected JSON object response`);
  assert(statuses.includes(response.status()),
    `${method} ${route}: expected HTTP ${statuses.join('|')}, got ${response.status()} ${JSON.stringify(body)}`);
  if (ok !== null && (ok === false || Object.hasOwn(body, 'ok'))) {
    assert.equal(body.ok, ok, `${method} ${route}: ${JSON.stringify(body)}`);
  }
  return body;
}

function click(page, control, method, route, status = 200, ok = (Array.isArray(status) ? status[0] : status) < 400, checkRequest) {
  return observe(page, method, route, async () => {
    await usable(control, true);
    await control.click();
  }, status, ok, checkRequest);
}

async function confirmDialog(page, buttonLabel = /确认|批准|删除|清空|吊销|撤销|执行|重置|放弃|切换|开始导入|立即运行|立即备份|保存白名单/) {
  const dialog = page.locator('[role="dialog"]');
  await dialog.waitFor({ state: 'visible', timeout: 5000 });
  await dialog.getByRole('button', { name: buttonLabel }).last().click();
}

function clickConfirm(page, control, method, route, status = 200, labelRegex, checkRequest) {
  return observe(page, method, route, async () => {
    await usable(control, true);
    await control.click();
    await confirmDialog(page, labelRegex);
  }, status, (Array.isArray(status) ? status[0] : status) < 400, checkRequest);
}

async function switchTab(page, label) {
  const tab = page.getByRole('tab', { name: label });
  await usable(tab, true);
  await tab.click();
}

export async function allowHttpErrors(page, expectedStatuses, fn, pathPrefix = null) {
  page.__herAllowedHttpErrors = {
    statuses: new Set(expectedStatuses),
    pathPrefix,
  };
  try {
    return await fn();
  } finally {
    page.__herAllowedHttpErrors = null;
  }
}

export async function openPage(page, origin, route, spec, token) {
  if (page.url() !== 'about:blank') await page.goto('about:blank');
  const url = new URL(route, origin);
  if (token) url.searchParams.set('token', token);
  const reads = (spec.ready ?? []).map(r => page.waitForResponse(res => responseMatches(res, 'GET', r))
    .then(res => {
      assert.equal(res.status(), 200, `GET ${r} returned HTTP ${res.status()}`);
      return res;
    }));
  const [navigation] = await Promise.all([
    page.goto(url.toString(), { waitUntil: 'domcontentloaded' }),
    ...reads,
  ]);
  assert.equal(navigation?.status(), 200, `GET ${route}: console document`);
  if (token && url.hostname === '127.0.0.1') {
    await page.waitForFunction(() => !new URLSearchParams(window.location.search).has('token'), null, { timeout: 8000 });
    assert(await page.evaluate(value => sessionStorage.getItem('consoleToken') === value
      || localStorage.getItem('consoleToken') === value, token),
    'Desktop origin did not persist bearer token');
  }
  await page.waitForFunction(expected => window.location.pathname === expected, url.pathname, { timeout: 8000 });
  if (spec.heading) {
    await page.locator('h1').filter({ hasText: spec.heading }).first().waitFor({ state: 'visible', timeout: 10_000 });
  } else {
    await page.locator(spec.marker).first().waitFor({ state: 'visible', timeout: 10_000 });
  }
  await page.waitForLoadState('networkidle').catch(() => {});
}

const tomorrow = () => {
  const d = new Date(Date.now() + 24 * 60 * 60 * 1000);
  const pad = n => String(n).padStart(2, '0');
  return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}T${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
};

const convRow = page => page.locator('table tbody tr').filter({ hasText: GROUP_KEY }).first();
const inspectorDrawer = page => page.getByRole('complementary', { name: '右侧详情检视抽屉' });
const settingCard = (page, key) => page.locator('main div.rounded-md, main div.rounded-lg')
  .filter({ has: page.locator('code, .font-mono', { hasText: key }) }).first();

export const walks = {
  overview: {
    refresh: async ({ page }) => {
      const status = await click(page, page.getByRole('button', { name: '立即刷新' }), 'GET', '/api/status');
      assert.equal(typeof status.mode, 'string', `Expected mode in /api/status: ${JSON.stringify(status)}`);
    },
    activity: async ({ page }) => {
      await usable(page.getByText('近期活动日志（最新 100 条）').first());
      await usable(page.getByText(/暂无活动记录|\d+\s*行/).first());
    },
    dark: async ({ page }) => {
      const wasDark = await page.evaluate(() => document.documentElement.classList.contains('dark'));
      const firstToggle = page.getByRole('button', { name: wasDark ? '切换到浅色模式' : '切换到深色模式' });
      await usable(firstToggle, true);
      await firstToggle.click();
      await page.waitForFunction(expected => document.documentElement.classList.contains('dark') === expected, !wasDark);
      const restoreToggle = page.getByRole('button', { name: wasDark ? '切换到深色模式' : '切换到浅色模式' });
      await usable(restoreToggle, true);
      await restoreToggle.click();
      await page.waitForFunction(expected => document.documentElement.classList.contains('dark') === expected, wasDark);
    },
    pending: async ({ page }) => {
      await usable(page.getByText('待处理审批与提问').first());
      await usable(page.getByText('当前无阻塞的待办项').first());
    },
  },

  conversations: {
    list: async ({ page }) => {
      const sessions = await click(page, page.getByRole('button', { name: '刷新', exact: true }).first(), 'GET', '/api/sessions');
      assert(Array.isArray(sessions.sessions) && sessions.sessions.some(s => s.key === GROUP_KEY),
        `Expected ${GROUP_KEY} in /api/sessions: ${JSON.stringify(sessions)}`);
      const row = convRow(page);
      await usable(row);
      await usable(row.getByText('已建立会话映射'));
    },
    'wake-states-all': async ({ page }) => {
      const states = await click(page, page.getByRole('button', { name: '刷新', exact: true }).first(), 'GET', '/api/socialV2/states');
      assert(Array.isArray(states.conversations) && states.conversations.some(c => c.key === GROUP_KEY),
        `Expected ${GROUP_KEY} in /api/socialV2/states: ${JSON.stringify(states)}`);
      const row = convRow(page);
      await usable(row);
      await usable(row.getByText(/diving|潜水/));
    },
    scope: async ({ page }) => {
      const extraId = String(OWNER + 1);
      const extraKey = `private:${extraId}`;
      await page.getByRole('button', { name: '添加会话' }).click();
      await page.locator('label', { hasText: '会话类型' }).locator('select').selectOption('private');
      await page.getByPlaceholder('输入纯数字 ID，例如 123456').fill(extraId);
      await clickConfirm(page, page.getByRole('button', { name: '确认接入' }), 'POST', '/api/conversations/scope', 200, /确认接入/);
      const row = page.locator('table tbody tr').filter({ hasText: extraKey }).first();
      await usable(row, true);
      await row.click();
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await switchTab(drawer, '单会话设置');
      await clickConfirm(page, drawer.getByRole('button', { name: '移出会话' }), 'POST', '/api/conversations/scope', 200, /移出|确认/);
      await drawer.getByRole('button', { name: '关闭抽屉' }).click().catch(() => {});
    },
    'wake-state': async ({ page }) => {
      const row = convRow(page);
      await usable(row, true);
      const stateRes = await click(
        page,
        row,
        'GET',
        new RegExp(`^/api/socialV2/state\\?key=${encodeURIComponent(GROUP_KEY)}$`),
      );
      assert.equal(stateRes.ok, true);
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await usable(drawer.getByRole('button', { name: '立即唤醒' }), true);
    },
    'wake-now': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await click(page, drawer.getByRole('button', { name: '立即唤醒' }), 'POST', '/api/socialV2/wake');
    },
    'initial-load': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      const input = drawer.getByPlaceholder('为该会话下一次新建立时注入一次性引导说明…');
      await usable(input);
      await page.waitForFunction(
        el => el.value === 'Seeded fixture prompt',
        await input.elementHandle(),
        { timeout: 8000 },
      );
      assert.equal(await input.inputValue(), 'Seeded fixture prompt');
    },
    'initial-save': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await drawer.getByPlaceholder('为该会话下一次新建立时注入一次性引导说明…').fill('Fixture initial prompt');
      await click(page, drawer.getByRole('button', { name: '保存' }), 'POST', '/api/socialV2/initial-prompt');
    },
    'initial-delete': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await drawer.getByRole('button', { name: '清空草稿' }).click();
      await click(page, drawer.getByRole('button', { name: '保存' }), 'POST', '/api/socialV2/initial-prompt');
    },
    'memory-load': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await usable(drawer.getByPlaceholder('追加轻记忆内容…'));
      await usable(drawer.getByText('Seeded fixture topic').first());
    },
    'memory-add-edit': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await drawer.getByPlaceholder('追加轻记忆内容…').fill('Fixture topic A');
      await click(page, drawer.getByRole('button', { name: '添加' }), 'POST', '/api/socialV2/memory-append');
      const topicRow = drawer.locator('div')
        .filter({ hasText: 'Fixture topic A' })
        .filter({ has: page.getByRole('button', { name: '编辑话题' }) })
        .last();
      const editBtn = topicRow.getByRole('button', { name: '编辑话题' });
      await usable(editBtn, true);
      await editBtn.click();
      const editInput = drawer.getByPlaceholder('正文内容');
      await usable(editInput);
      await editInput.fill('Fixture topic B');
      await click(page, drawer.getByRole('button', { name: '保存修改' }), 'POST', '/api/socialV2/memory-update');
    },
    'memory-clear-category': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      const select = drawer.locator('select').first();
      await select.selectOption('pendingThought');
      await drawer.getByPlaceholder('追加轻记忆内容…').fill('Fixture thought');
      await click(page, drawer.getByRole('button', { name: '添加' }), 'POST', '/api/socialV2/memory-append');
      await clickConfirm(page, drawer.getByRole('button', { name: '清空当前类' }),
        'POST', '/api/socialV2/memory-clear', 200, /确认清空该类/,
        req => assert.equal(req.postDataJSON().category, 'pendingThought'));
      await select.selectOption('activeTopic');
    },
    'memory-remove': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      const removeTopic = drawer.locator('div')
        .filter({ hasText: 'Fixture topic B' })
        .filter({ has: page.getByRole('button', { name: '移除' }) })
        .last()
        .getByRole('button', { name: '移除' });
      await clickConfirm(page, removeTopic, 'POST', '/api/socialV2/memory-remove', 200, /确认删除/);
    },
    'memory-clear-all': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      const clearBtn = drawer.getByRole('button', { name: '清空', exact: true });
      await clickConfirm(page, clearBtn, 'POST', '/api/socialV2/memory-clear', 200, /确认清空/,
        req => assert.equal(req.postDataJSON().category, undefined));
    },
    'session-reset': async ({ page }) => {
      // Reset GROUP2_KEY rather than GROUP_KEY so GROUP_KEY keeps its seeded wakeWordLog and paused turn.
      await page.locator('table tbody tr').filter({ hasText: GROUP2_KEY }).first().click();
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await switchTab(drawer, '恢复与队列');
      await clickConfirm(page, drawer.getByRole('button', { name: '重置会话映射' }),
        'POST', '/api/session/reset', 200, /确认|重置/,
        req => assert.equal(req.postDataJSON().key, GROUP2_KEY));
      await convRow(page).click();
      await usable(drawer);
    },
    'settings-list': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await switchTab(drawer, '单会话设置');
      const routeSelect = drawer.locator('label', { hasText: '单会话模型路由' }).locator('select');
      const engineSelect = drawer.locator('label', { hasText: '单会话回合引擎' }).locator('select');
      await usable(routeSelect);
      await usable(engineSelect);
      await routeSelect.locator('option[value="second"]').waitFor({ state: 'attached', timeout: 8000 });
      await page.waitForFunction(
        el => el.value === 'dsh',
        await engineSelect.elementHandle(),
        { timeout: 8000 },
      );
      assert.equal(await routeSelect.locator('option[value="second"]').count(), 1);
      assert.equal(await routeSelect.inputValue(), '');
      assert.equal(await engineSelect.inputValue(), 'dsh');
    },
    'settings-set': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await drawer.locator('label', { hasText: '单会话模型路由' }).locator('select').selectOption('second');
      await click(page, drawer.getByRole('button', { name: '保存', exact: true }), 'POST', '/api/settings/conversation/set');
    },
    'engine-switch': async ({ page }) => {
      // Switch GROUP2_KEY rather than GROUP_KEY so GROUP_KEY keeps fixture-paused-turn reserved for Audit.
      await page.locator('table tbody tr').filter({ hasText: GROUP2_KEY }).first().click();
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await switchTab(drawer, '单会话设置');
      await drawer.locator('label', { hasText: '单会话回合引擎' }).locator('select').selectOption('harness');
      await clickConfirm(page, drawer.getByRole('button', { name: '立即切换回合引擎（等待排空）' }),
        'POST', '/api/settings/conversation/switch', 200, /确认|切换/,
        req => assert.equal(req.postDataJSON().conversation, GROUP2_KEY));
    },
    'settings-reset': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await switchTab(drawer, '单会话设置');
      await clickConfirm(page, drawer.getByRole('button', { name: '重置为默认' }),
        'POST', '/api/settings/conversation/reset', 200, /确认|重置/,
        req => {
          const body = req.postDataJSON();
          assert.equal(body.conversation, GROUP2_KEY);
          assert.equal(body.setting, 'conversation.engine');
        });
      const engineSelect = drawer.locator('label', { hasText: '单会话回合引擎' }).locator('select');
      await page.waitForFunction(el => el.value === 'dsh', await engineSelect.elementHandle());
      await convRow(page).click();
      await usable(drawer);
      await switchTab(drawer, '单会话设置');
      const routeSelect = drawer.locator('label', { hasText: '单会话模型路由' }).locator('select');
      await routeSelect.selectOption('');
      await click(page, drawer.getByRole('button', { name: '保存', exact: true }),
        'POST', '/api/settings/conversation/reset', 200, true,
        req => {
          const body = req.postDataJSON();
          assert.equal(body.conversation, GROUP_KEY);
          assert.equal(body.setting, 'conversation.route');
        });
      await page.waitForFunction(el => el.value === '', await routeSelect.elementHandle());
    },
  },

  persona: {
    list: async ({ page }) => {
      const res = await click(page, page.getByRole('button', { name: '刷新', exact: true }).first(), 'GET', '/api/personas');
      assert(Array.isArray(res.personas), `Expected personas array in /api/personas: ${JSON.stringify(res)}`);
      await usable(page.getByRole('button', { name: '新建人设' }));
      await usable(page.getByText('暂无人设').first());
    },
    import: ({ page }) => clickConfirm(page, page.getByRole('button', { name: /从角色卡导入|已导入角色卡/ }),
      'POST', '/api/personas/import-roles', 200, /开始导入/),
    create: async ({ page, state }) => {
      await page.getByRole('button', { name: '新建人设' }).click();
      await page.getByPlaceholder('人设名称（单行，≤ 40 字符）*').fill('Fixture persona');
      await page.getByPlaceholder('人设核心 (core, ≤ 20000 字符) *').fill('A synthetic persona for V3 UI walk.');
      const created = await click(page, page.getByRole('button', { name: '创建人设' }), 'POST', '/api/personas/create');
      state.persona = created.id;
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await drawer.getByRole('button', { name: '关闭抽屉' }).click();
    },
    'edit-load': async ({ page }) => {
      const row = page.getByText('Fixture persona').first();
      await usable(row, true);
      await observe(page, 'GET', '/api/personas/item', () => row.click());
      await usable(inspectorDrawer(page));
    },
    'save-version': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await drawer.locator('textarea').first().fill('Updated core persona for V3 UI walk.');
      await click(page, drawer.getByRole('button', { name: '保存为新版本' }), 'POST', '/api/personas/update');
    },
    versions: async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await switchTab(drawer, '版本历史');
      await usable(drawer.getByRole('button', { name: /批准|切换/ }).first());
    },
    approve: async ({ page }) => {
      const drawer = inspectorDrawer(page);
      const approveBtn = drawer.getByRole('button', { name: /批准并激活/ }).first();
      await clickConfirm(page, approveBtn, 'POST', '/api/personas/approve', 200, /批准/);
    },
    rename: async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await switchTab(drawer, '属性与设定');
      await drawer.getByLabel('人设名称（单行，≤ 40 字符）').fill('Fixture persona v2');
      await click(page, drawer.getByRole('button', { name: '保存为新版本' }), 'POST', '/api/personas/update');
    },
    'assign-default': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await click(page, drawer.getByRole('button', { name: '设为全局默认' }), 'POST', '/api/personas/assign');
    },
    'assign-clear': async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await click(page, drawer.getByRole('button', { name: '取消全局默认' }), 'POST', '/api/personas/assign');
    },
    duplicate: async ({ page, state }) => {
      const drawer = inspectorDrawer(page);
      await drawer.getByRole('button', { name: '复制人设' }).click();
      await drawer.getByPlaceholder('输入新人设名称…').fill('Fixture copy');
      const dup = await click(page, drawer.getByRole('button', { name: '确定复制' }), 'POST', '/api/personas/duplicate');
      state.copy = dup.id;
      await drawer.getByRole('button', { name: '关闭抽屉' }).click();
    },
    delete: async ({ page }) => {
      const copyRow = page.getByText('Fixture copy').first();
      await usable(copyRow, true);
      await observe(page, 'GET', '/api/personas/item', () => copyRow.click());
      const drawer = inspectorDrawer(page);
      await clickConfirm(page, drawer.getByRole('button', { name: '删除', exact: true }),
        'POST', '/api/personas/delete', 200, /确认删除/);
    },
    'assign-override': async ({ page, state }) => {
      await page.locator('button[title="打开/关闭右侧详情抽屉"]').click();
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await switchTab(drawer, '单会话设置');
      const select = drawer.locator('label', { hasText: '单会话人设覆盖' }).locator('select');
      await select.selectOption(state.persona);
      await click(page, drawer.getByRole('button', { name: '保存', exact: true }), 'POST', '/api/personas/assign');
      await select.selectOption('');
      await click(page, drawer.getByRole('button', { name: '保存', exact: true }), 'POST', '/api/personas/assign');
      await drawer.getByRole('button', { name: '关闭抽屉' }).click();
    },
    'inject-limit': async ({ page }) => {
      const card = page.locator('section').filter({ hasText: '注入上限' }).first();
      await card.locator('input[type="number"]').fill('6000');
      await click(page, card.getByRole('button', { name: '保存上限' }), 'POST', '/api/roles/limit');
    },
    'wake-load': async ({ page }) => {
      await page.getByLabel('选择查看唤醒词的会话').selectOption(GROUP_KEY);
      await click(page, page.getByRole('button', { name: '刷新唤醒词记录' }).or(
        page.locator('section').filter({ hasText: '唤醒词' }).getByRole('button', { name: '刷新' }),
      ).first(), 'GET', '/api/personas/wake-words');
    },
    'wake-undo': async ({ page }) => {
      const undoBtn = page.locator('section').filter({ hasText: '唤醒词' }).getByRole('button', { name: '撤销' }).first();
      await clickConfirm(page, undoBtn, 'POST', '/api/personas/wake-words/undo', 200, /撤销|确认/);
    },
    'sim-prompt-show': async ({ page }) => {
      await switchTab(page, '仿真提示词');
      await usable(page.getByLabel('仿真预设提示词正文'));
    },
    'sim-prompt-save': async ({ page }) => {
      const box = page.getByLabel('仿真预设提示词正文');
      const current = await box.inputValue();
      await box.fill(`${current}\n# fixture check line`);
      await click(page, page.getByRole('button', { name: '保存', exact: true }).last(), 'POST', '/api/preset/sim-prompt');
    },
    'sim-prompt-backups': async ({ page }) => {
      await usable(page.getByRole('button', { name: '还原' }).first());
    },
    'sim-prompt-restore': async ({ page }) => {
      await clickConfirm(page, page.getByRole('button', { name: '还原' }).first(),
        'POST', '/api/preset/sim-prompt/restore', 200, /还原|确认/);
    },
    'effective-prompt': async ({ page }) => {
      await page.getByLabel('选择提示词预览会话').selectOption(GROUP_KEY);
      await click(page, page.getByRole('button', { name: '加载预览' }), 'GET', '/api/socialV2/prompt');
    },
  },

  memory: {
    // Console callers without conversation x-agent-token are refused (403) on write/search/preview/index.
    write: async ({ page }) => {
      await page.getByLabel('选择记忆库目标会话').selectOption(GROUP_KEY);
      await page.getByRole('button', { name: '写入记忆' }).click();
      await page.getByPlaceholder(/写入记忆正文/).fill('Fixture memory entry alpha');
      await allowHttpErrors(page, [403], () =>
        click(page, page.getByRole('button', { name: '提交写入' }), 'POST', '/api/memory/write', 403, false), '/api/memory/write');
    },
    search: async ({ page }) => {
      await page.getByPlaceholder(/输入关键词检索/).fill('Fixture memory');
      await allowHttpErrors(page, [403], () =>
        click(page, page.getByRole('button', { name: '搜索' }).first(), 'POST', '/api/memory/search', 403, false), '/api/memory/search');
    },
    preview: ({ page }) => allowHttpErrors(page, [403], () =>
      click(page, page.getByRole('button', { name: '召回预览' }), 'POST', '/api/memory/preview', 403, false), '/api/memory/preview'),
    index: ({ page }) => allowHttpErrors(page, [403], () =>
      click(page, page.getByRole('button', { name: '索引词' }), 'POST', '/api/memory/index', 403, false), '/api/memory/index'),
    archive: async ({ page }) => {
      await page.getByLabel('按 ID 归档的记忆编号').fill('1');
      const arch = await click(page, page.getByRole('button', { name: '归档', exact: true }).first(), 'POST', '/api/memory/archive');
      assert.equal(arch.changed, true, 'Expected seeded memory #1 to archive');
      assert.equal(arch.archived, true);
      const unarch = await click(page, page.getByRole('button', { name: '取消归档', exact: true }).first(), 'POST', '/api/memory/archive');
      assert.equal(unarch.changed, true, 'Expected seeded memory #1 to unarchive');
      assert.equal(unarch.archived, false);
    },
    'slang-tabs': async ({ page }) => {
      await switchTab(page, '黑话词库');
      await switchTab(page, '已确认');
      await switchTab(page, '已拒绝');
      await switchTab(page, '候选词');
    },
    'slang-list': ({ page }) => click(page, page.locator('button', { hasText: '刷新' }).first(), 'GET', '/api/slang'),
    'slang-add': async ({ page }) => {
      await page.getByRole('button', { name: '新增黑话' }).click();
      await page.getByLabel('新增黑话初始状态').selectOption('candidate');
      await page.getByPlaceholder('词条 (≤ 50 字) *').fill('yyds-fixture');
      await page.getByPlaceholder('含义 (≤ 300 字，已确认必填)').fill('Fixture slang definition');
      await click(page, page.getByRole('button', { name: '保存词条' }), 'POST', '/api/slang');
    },
    'slang-edit': async ({ page }) => {
      const card = page.locator('div.rounded-md').filter({ hasText: 'yyds-fixture' }).first();
      await card.getByRole('button', { name: '编辑', exact: true }).click();
      await page.getByPlaceholder('用法 (≤ 300 字)').fill('Fixture usage note');
      await click(page, page.getByRole('button', { name: '保存修改' }), 'PATCH', /^\/api\/slang\/[^/]+$/);
    },
    'slang-confirm': async ({ page }) => {
      const card = page.locator('div.rounded-md').filter({ hasText: 'yyds-fixture' }).first();
      await click(page, card.getByRole('button', { name: '确认' }), 'POST', /^\/api\/slang\/[^/]+\/confirm$/);
    },
    'slang-reject': async ({ page }) => {
      await switchTab(page, '候选词');
      await page.getByRole('button', { name: '新增黑话' }).click();
      await page.getByLabel('新增黑话初始状态').selectOption('candidate');
      await page.getByPlaceholder('词条 (≤ 50 字) *').fill('reject-fixture');
      await page.getByPlaceholder('含义 (≤ 300 字，已确认必填)').fill('Reject candidate');
      await click(page, page.getByRole('button', { name: '保存词条' }), 'POST', '/api/slang');
      const card = page.locator('div.rounded-md').filter({ hasText: 'reject-fixture' }).first();
      await click(page, card.getByRole('button', { name: '拒绝' }), 'POST', /^\/api\/slang\/[^/]+\/reject$/);
    },
    'slang-restore': async ({ page }) => {
      await switchTab(page, '已拒绝');
      const card = page.locator('div.rounded-md').filter({ hasText: 'reject-fixture' }).first();
      await click(page, card.getByRole('button', { name: /恢复为候选/ }).first(), 'PATCH', /^\/api\/slang\/[^/]+$/);
    },
    'slang-batch-research': async ({ page }) => {
      await switchTab(page, '候选词');
      await page.getByRole('button', { name: /全选 \/ 取消/ }).click();
      // slang.enabled is false in fixture, so research cleanly returns 400.
      await allowHttpErrors(page, [400], () =>
        click(page, page.getByRole('button', { name: /研究已选|研究全部候选/ }).first(),
          'POST', '/api/slang/research', 400, false), '/api/slang/research');
    },
    'slang-batch-reject': async ({ page }) => {
      await switchTab(page, '候选词');
      await page.getByRole('button', { name: /全选 \/ 取消/ }).click();
      await click(page, page.getByRole('button', { name: '批量拒绝' }), 'POST', '/api/slang/batch-reject');
    },
    'slang-batch-confirm': async ({ page }) => {
      await switchTab(page, '已拒绝');
      const card = page.locator('div.rounded-md').filter({ hasText: 'reject-fixture' }).first();
      await click(page, card.getByRole('button', { name: /恢复为候选/ }).first(), 'PATCH', /^\/api\/slang\/[^/]+$/);
      await switchTab(page, '候选词');
      await page.getByRole('button', { name: /全选 \/ 取消/ }).click();
      await click(page, page.getByRole('button', { name: '批量确认' }), 'POST', '/api/slang/batch-confirm');
    },
    'slang-delete': async ({ page }) => {
      await switchTab(page, '已确认');
      await clickConfirm(page, page.getByRole('button', { name: '删除黑话 yyds-fixture' }),
        'DELETE', /^\/api\/slang\/[^/]+$/, 200, /删除|确认/);
    },
    'slang-batch-delete': async ({ page }) => {
      await switchTab(page, '已确认');
      await page.getByRole('button', { name: /全选 \/ 取消/ }).click();
      await clickConfirm(page, page.getByRole('button', { name: '批量删除' }), 'POST', '/api/slang/batch-delete', 200, /删除|确认/);
    },
    'slang-extract': async ({ page }) => {
      await page.getByLabel('选择黑话提取目标会话').selectOption(GROUP_KEY);
      await allowHttpErrors(page, [400], () =>
        click(page, page.getByRole('button', { name: '立即提取黑话' }), 'POST', '/api/slang/extract', 400, false), '/api/slang/extract');
    },
    'slang-config': async ({ page }) => {
      await page.getByPlaceholder('2, 4, 8').fill('2, 4');
      await click(page, page.getByRole('button', { name: '保存', exact: true }).last(), 'POST', '/api/slang/config');
    },
    'stickers-list': async ({ page }) => {
      await switchTab(page, '表情包与语音');
      await usable(page.getByText('测试贴纸').first());
    },
    'stickers-sync': ({ page }) => click(page, page.getByRole('button', { name: '同步 QQ 表情包' }), 'POST', '/api/stickers/sync'),
    'stickers-note': async ({ page }) => {
      await page.getByRole('button', { name: '编辑备注' }).first().click();
      await page.getByPlaceholder('本地备注 (localNote, ≤ 200)').fill('Updated local sticker note');
      await click(page, page.getByRole('button', { name: '保存本地备注' }), 'POST', '/api/stickers/note');
    },
    'stickers-remark': async ({ page }) => {
      await page.getByRole('button', { name: '编辑备注' }).first().click();
      await page.getByLabel('QQ 远端表情备注').fill('Updated remote remark');
      await click(page, page.getByRole('button', { name: '更新 QQ 备注' }), 'POST', '/api/stickers/remark');
      await page.getByRole('button', { name: '关闭', exact: true }).click();
    },
    'stickers-preview': async ({ page }) => {
      await page.getByLabel('选择表情包预览鉴权会话').selectOption(GROUP_KEY);
      await allowHttpErrors(page, [500], () =>
        click(page, page.getByRole('button', { name: '预览图像' }).first(), 'GET', '/api/socialV2/sticker-image', 500, false), '/api/socialV2/sticker-image');
    },
    'stickers-collect': async ({ page }) => {
      await page.getByRole('button', { name: '收藏消息表情' }).click();
      await page.getByLabel('选择收藏表情来源会话').selectOption(GROUP_KEY);
      await page.getByPlaceholder('messageId（二选一）').fill('7003');
      // Message 7003 is absent from seeded recentMessages, so collect-sticker rejects cleanly (500).
      await allowHttpErrors(page, [500], () =>
        click(page, page.getByRole('button', { name: '收藏', exact: true }), 'POST', '/api/socialV2/collect-sticker', 500, false), '/api/socialV2/collect-sticker');
      await page.getByRole('button', { name: '收藏消息表情' }).click();
    },
    'stickers-send-policy': async ({ page }) => {
      await usable(page.getByText(/AI 主动发表情仅在二代仿真回合中通过会话令牌执行/).first());
    },
    'voice-list': async ({ page }) => {
      await click(page, page.getByRole('button', { name: '刷新并探测' }), 'GET', '/api/voice/list');
      await usable(page.getByText('fixture.wav').first());
    },
    'voice-send': async ({ page }) => {
      await page.getByLabel('选择语音发送目标会话').selectOption(GROUP_KEY);
      await page.getByLabel('语音文件名或路径').fill('fixture.wav');
      const dryRun = await allowHttpErrors(page, [400], () =>
        click(page, page.getByRole('button', { name: '仅预检 (dryRun)' }), 'POST', '/api/voice/send', [200, 400], null,
          req => assert.equal(req.postDataJSON().dryRun, true)), '/api/voice/send');
      if (!dryRun.ok) {
        assert.match(String(dryRun.error ?? ''), /ffprobe/i, `Unexpected voice dryRun error: ${JSON.stringify(dryRun)}`);
      }
      const liveSend = await allowHttpErrors(page, [400], () =>
        click(page, page.getByRole('button', { name: '测试发送' }), 'POST', '/api/voice/send', [200, 400], null,
          req => assert.equal(req.postDataJSON().dryRun, false)), '/api/voice/send');
      if (!liveSend.ok) {
        assert.match(String(liveSend.error ?? ''), /ffprobe/i, `Unexpected voice send error: ${JSON.stringify(liveSend)}`);
      }
    },
  },

  behavior: {
    mode: async ({ page }) => {
      await page.getByLabel('选择运行模式').selectOption('chat');
      await click(page, page.getByRole('button', { name: '应用运行模式' }), 'POST', '/api/mode');
      await page.getByLabel('选择运行模式').selectOption('reserved2');
      await click(page, page.getByRole('button', { name: '应用运行模式' }), 'POST', '/api/mode');
    },
    silent: async ({ page }) => {
      await click(page, page.getByRole('button', { name: /silent · 静默观察/ }), 'POST', '/api/role-mode');
      await click(page, page.getByRole('button', { name: /active · 主动发言/ }), 'POST', '/api/role-mode');
    },
    'v2-pause': ({ page }) => click(page, page.getByRole('switch', { name: '暂停社交调度' }),
      'POST', '/api/socialV2/activity', 200, true, req => assert.equal(req.postDataJSON().paused, true)),
    'v2-resume': ({ page }) => click(page, page.getByRole('switch', { name: '暂停社交调度' }),
      'POST', '/api/socialV2/activity', 200, true, req => assert.equal(req.postDataJSON().paused, false)),
    'v2-config': async ({ page }) => {
      const card = settingCard(page, 'reply.wakeQuietMs');
      const input = card.locator('input[type="number"]').first();
      const saveBtn = page.getByRole('button', { name: '保存', exact: true });
      await input.fill('3001');
      await click(page, saveBtn, 'POST', '/api/settings/set');
      await saveBtn.waitFor({ state: 'hidden', timeout: 8000 });
      await input.fill('3000');
      await click(page, saveBtn, 'POST', '/api/settings/set');
      await saveBtn.waitFor({ state: 'hidden', timeout: 8000 });
    },
    feedback: async ({ page }) => {
      await switchTab(page, '诊断日志');
      await click(page, page.getByRole('button', { name: '刷新' }).first(), 'GET', '/api/socialV2/feedback');
      await usable(page.getByText('Fixture feedback item').first());
    },
    'feedback-clear': ({ page }) => clickConfirm(page, page.getByRole('button', { name: '清空', exact: true }).first(),
      'POST', '/api/socialV2/feedback-clear', 200, /清空|确认/),
    'tool-log': async ({ page }) => {
      await click(page, page.getByRole('button', { name: '刷新' }).nth(1), 'GET', '/api/socialV2/tool-log');
      await usable(page.getByText('fixture_tool').first());
    },
    'tool-log-clear': ({ page }) => clickConfirm(page, page.getByRole('button', { name: '清空', exact: true }).nth(1),
      'POST', '/api/socialV2/tool-log/clear', 200, /清空|确认/),
    'send-outbox': async ({ page }) => {
      await page.getByLabel('选择会话查看发送队列').selectOption(GROUP_KEY);
      await click(page, page.getByRole('button', { name: '查询批次' }), 'GET', '/api/socialV2/send-outbox');
    },
    'v2-reset': async ({ page }) => {
      // Run last so reset cannot clear GROUP_KEY before send-outbox reads its dropdown option.
      await switchTab(page, '模式与参数');
      await clickConfirm(page, page.getByRole('button', { name: '重置 socialV2 状态' }),
        'POST', '/api/socialV2/reset', 200, /重置|确认/);
    },
  },

  models: {
    view: async ({ page }) => {
      await usable(page.getByText('first').first());
      await usable(page.getByText('second').first());
    },
    set: async ({ page }) => {
      const row = page.locator('table tbody tr').filter({ hasText: GROUP_KEY }).first();
      await usable(row, true);
      await row.getByRole('button', { name: '检视' }).click();
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await switchTab(drawer, '单会话设置');
      await drawer.locator('label', { hasText: '单会话模型路由' }).locator('select').selectOption('first');
      await click(page, drawer.getByRole('button', { name: '保存', exact: true }), 'POST', '/api/settings/conversation/set');
    },
    reset: async ({ page }) => {
      const drawer = inspectorDrawer(page);
      await drawer.locator('label', { hasText: '单会话模型路由' }).locator('select').selectOption('');
      await click(page, drawer.getByRole('button', { name: '保存', exact: true }), 'POST', '/api/settings/conversation/reset');
      await drawer.getByRole('button', { name: '关闭抽屉' }).click();
    },
    'spend-series': async ({ page, state }) => {
      await switchTab(page, '用量与花费');
      const [series, summary, provider, attribution, recent] = await Promise.all([
        observe(page, 'GET', '/api/tokens/series', async () => {}),
        observe(page, 'GET', '/api/tokens/summary', async () => {}),
        observe(page, 'GET', '/api/tokens/by-provider', async () => {}),
        observe(page, 'GET', '/api/usage/attribution', async () => {}),
        observe(page, 'GET', '/api/tokens/recent', async () => {}),
        page.getByRole('button', { name: '刷新数据' }).click(),
      ]);
      state.spendData = { series, summary, provider, attribution, recent };
      await usable(page.getByText(/时间序列趋势/).first());
    },
    'spend-summary': async ({ page, state }) => {
      assert.equal(state.spendData?.summary?.ok, true);
      await usable(page.getByText('按会话花费排行与回合钻取').first());
      await usable(convRow(page));
    },
    'spend-provider': async ({ page, state }) => {
      assert.equal(state.spendData?.provider?.ok, true);
      await usable(page.getByText('按模型提供商分布').first());
      await usable(page.getByText('fixture').first());
    },
    'usage-attribution': async ({ page, state }) => {
      assert.equal(state.spendData?.attribution?.ok, true);
      await usable(page.getByText(/用量归因对照/).first());
    },
    'spend-turns': ({ page }) => click(page, convRow(page), 'GET', '/api/tokens/turns'),
    'spend-recent': async ({ page, state }) => {
      assert.equal(state.spendData?.recent?.ok, true);
      await usable(page.getByText('最近用量流水（Usage fact）').first());
      const recentRow = page.locator('table').filter({ hasText: 'Tokens（命中/未命中/输出）' })
        .locator('tbody tr').filter({ hasText: GROUP_KEY }).first();
      await usable(recentRow);
    },
    'spend-prices': async ({ page, state }) => {
      assert(state.spendData?.summary?.priceTable);
      await usable(page.getByText(/当前计费价目表/).first());
    },
    'spend-reset': ({ page }) => allowHttpErrors(page, [503], () =>
      clickConfirm(page, page.getByRole('button', { name: '重置用量账本' }),
        'POST', '/api/tokens/reset', 503, /确认|重置/), '/api/tokens/reset'),
  },

  settings: {
    list: async ({ page }) => {
      await usable(page.getByPlaceholder('搜索设置键名、标签、说明或分组…'));
    },
    set: async ({ page }) => {
      await page.getByPlaceholder('搜索设置键名、标签、说明或分组…').fill('harness.model');
      const card = settingCard(page, 'harness.model');
      await card.locator('input').first().fill('fixture-alt');
      await click(page, page.getByRole('button', { name: '保存', exact: true }), 'POST', '/api/settings/set');
    },
    reset: async ({ page }) => {
      const card = settingCard(page, 'harness.model');
      await clickConfirm(page, card.getByRole('button', { name: /恢复默认|重置/ }).first(),
        'POST', '/api/settings/reset', 200, /确认恢复/);
    },
    'security-confirm': async ({ page }) => {
      await page.getByPlaceholder('搜索设置键名、标签、说明或分组…').fill('ownerQQ');
      const card = settingCard(page, 'ownerQQ');
      await card.locator('input').first().fill(String(OWNER + 1));
      await clickConfirm(page, page.getByRole('button', { name: '保存', exact: true }),
        'POST', '/api/settings/set', 200, /确认/);
      await card.locator('input').first().fill(String(OWNER));
      await clickConfirm(page, page.getByRole('button', { name: '保存', exact: true }),
        'POST', '/api/settings/set', 200, /确认/);
    },
  },

  schedule: {
    upcoming: async ({ page }) => {
      await usable(page.getByRole('button', { name: '新建提醒' }));
      await usable(page.getByText('system:backup').first());
    },
    'reminder-new': async ({ page, state }) => {
      await page.getByRole('button', { name: '新建提醒' }).click();
      await page.getByPlaceholder('group:<群号> 或 private:<QQ号>').fill(GROUP_KEY);
      await page.locator('input[type="datetime-local"]').first().fill(tomorrow());
      await page.getByPlaceholder('输入到期时投递给 HER 的提醒事项…').fill('Fixture V3 reminder');
      const created = await click(page, page.getByRole('button', { name: '确认创建' }), 'POST', '/api/schedule');
      state.reminderId = created.item.id;
    },
    'reminder-pause': async ({ page }) => {
      const card = page.locator('div.rounded-md, tr').filter({ hasText: 'Fixture V3 reminder' }).first();
      await click(page, card.getByRole('button', { name: '暂停', exact: true }), 'POST', /^\/api\/schedule\/item:[^/]+\/pause$/);
      await click(page, card.getByRole('button', { name: '恢复', exact: true }), 'POST', /^\/api\/schedule\/item:[^/]+\/resume$/);
    },
    'reminder-edit': async ({ page }) => {
      const card = page.locator('div.rounded-md, tr').filter({ hasText: 'Fixture V3 reminder' }).first();
      await card.getByRole('button', { name: '编辑', exact: true }).click();
      await page.locator('label', { hasText: '提醒文本' }).locator('input').fill('Updated V3 reminder');
      await click(page, page.getByRole('button', { name: '保存修改' }), 'POST', /^\/api\/schedule\/item:[^/]+\/edit$/);
    },
    'reminder-cancel': async ({ page }) => {
      const card = page.locator('div.rounded-md, tr').filter({ hasText: 'Updated V3 reminder' }).first();
      await clickConfirm(page, card.getByRole('button', { name: /取消|撤回/ }).first(),
        'POST', /^\/api\/schedule\/item:[^/]+\/cancel$/, 200, /确认|取消/);
    },
    'reminder-history': async ({ page, state }) => {
      const hist = await click(
        page,
        page.getByRole('button', { name: '刷新', exact: true }).first(),
        'GET',
        /^\/api\/schedule\/history/,
      );
      assert.equal(hist.ok, true);
      const entry = (hist.history ?? []).find(item => item.id === state.reminderId);
      assert(entry, `Missing cancelled reminder ${state.reminderId} in schedule history`);
      assert.match(String(entry.summary ?? ''), /已取消/);
      await usable(page.getByText(/近期提醒历史/).first());
      await usable(page.getByText('已取消').first());
    },
    'job-run': async ({ page, state }) => {
      const row = page.locator('table tbody tr').filter({ hasText: 'system:backup' }).first();
      const started = await clickConfirm(page, row.getByRole('button', { name: '立即运行' }),
        'POST', '/api/schedule/system:backup/run', 202, /确认执行/);
      assert(started.runId, `Expected runId from backup trigger: ${JSON.stringify(started)}`);
      state.schedRunId = started.runId;
      await switchTab(page, '任务运行历史');
    },
    'run-history': async ({ page, state }) => {
      await usable(page.getByText(state.schedRunId).first());
    },
    'run-log': async ({ page, state }) => {
      const collapse = page.getByRole('button', { name: '收起日志' }).or(page.getByRole('button', { name: '关闭' })).first();
      if (await collapse.isVisible().catch(() => false)) await collapse.click();
      const row = page.locator('table tbody tr').filter({ hasText: state.schedRunId }).first();
      await click(page, row.getByRole('button', { name: '查看日志' }), 'GET', `/api/tasks/runs/${state.schedRunId}/log`);
    },
    windows: async ({ page }) => {
      await usable(page.getByText(/旧版 Windows 计划任务处置对照/).first());
      await usable(page.getByText(/暂无旧版计划任务清单|QQBot-/).first());
    },
  },

  system: {
    lifecycle: async ({ page }) => {
      await allowHttpErrors(page, [503], async () => {
        await clickConfirm(page, page.getByRole('button', { name: '进入维护模式' }), 'POST', '/api/tasks/lifecycle', 503, /确认/);
        await clickConfirm(page, page.getByRole('button', { name: '请求停止服务' }), 'POST', '/api/tasks/lifecycle', 503, /确认/);
      }, '/api/tasks/lifecycle');
      await clickConfirm(page, page.getByRole('button', { name: '恢复正常运行' }), 'POST', '/api/tasks/lifecycle', 200, /确认切换/);
    },
    'qq-login': async ({ page }) => {
      await click(page, page.getByRole('button', { name: '刷新', exact: true }).first(), 'GET', '/api/qq/login');
      await usable(page.getByText(/已登录|等待登录|状态未知/).first());
    },
    'test-send': async ({ page }) => {
      await page.getByPlaceholder('输入白名单内的群号或 QQ 号…').fill('420042');
      await page.getByPlaceholder('测试文本…').fill('fixture-test-msg');
      await allowHttpErrors(page, [503], () =>
        click(page, page.getByRole('button', { name: '发送测试' }), 'POST', '/api/test-send', 503, false), '/api/test-send');
    },
    restart: ({ page }) => allowHttpErrors(page, [503], () =>
      clickConfirm(page, page.getByRole('button', { name: '重启控制台进程' }), 'POST', '/api/restart', 503, /重启|确认/), '/api/restart'),
    'workspace-reset': ({ page }) => allowHttpErrors(page, [503], () =>
      clickConfirm(page, page.getByRole('button', { name: '重置 QQ 聊天工作区' }),
        'POST', '/api/workspace/reset', 503, /重置|确认/), '/api/workspace/reset'),
    'backup-settings': async ({ page }) => {
      await switchTab(page, '备份与还原');
      await page.locator('input[type="number"]').first().fill('5');
      await click(page, page.getByRole('button', { name: '保存备份设置' }), 'POST', '/api/backups/settings');
    },
    'backup-run': async ({ page, state }) => {
      const started = await clickConfirm(page, page.getByRole('button', { name: '立即备份' }),
        'POST', '/api/tasks/backup/run', 202, /立即备份/);
      state.backupRunId = started.runId;
      const previewBtn = page.getByRole('button', { name: '预览还原' }).first();
      const deadline = Date.now() + 20_000;
      while (!(await previewBtn.isVisible().catch(() => false))) {
        if (Date.now() > deadline) break;
        await page.getByRole('button', { name: '刷新', exact: true }).first().click().catch(() => {});
        await page.waitForTimeout(400);
      }
      await usable(previewBtn);
    },
    'backup-list': async ({ page }) => {
      const res = await click(page, page.getByRole('button', { name: '刷新', exact: true }).first(), 'GET', '/api/backups');
      assert.equal(res.ok, true);
      assert(Array.isArray(res.backups) && res.backups.length > 0, `Expected non-empty backups list: ${JSON.stringify(res)}`);
      await usable(page.getByText(res.backups[0].name).first());
      await usable(page.getByRole('button', { name: '预览还原' }).first());
    },
    'backup-preview': async ({ page, state }) => {
      const row = page.locator('table tbody tr').filter({ has: page.getByRole('button', { name: '预览还原' }) }).first();
      state.archiveName = (await row.locator('td').first().textContent())?.trim();
      await click(page, row.getByRole('button', { name: '预览还原' }), 'POST', '/api/backups/restore/preview');
    },
    'backup-restore': async ({ page, backupDir, state }) => {
      const archive = path.join(backupDir, state.archiveName);
      const parked = `${archive}.parked`;
      await fs.rename(archive, parked);
      try {
        await allowHttpErrors(page, [503], () =>
          clickConfirm(page, page.getByRole('button', { name: '确认执行还原' }),
            'POST', '/api/backups/restore', 503, /确认执行还原/), '/api/backups/restore');
      } finally {
        await fs.rename(parked, archive).catch(error => { if (error.code !== 'ENOENT') throw error; });
      }
    },
    'release-status': async ({ page }) => {
      await switchTab(page, '更新与依赖');
      await usable(page.getByText('v99.0.0').first());
    },
    'release-approve': ({ page }) => allowHttpErrors(page, [409], () =>
      clickConfirm(page, page.getByRole('button', { name: '批准更新' }),
        'POST', '/api/releases/approve', 409, /批准|确认/), '/api/releases/approve'),
    'release-policy': async ({ page }) => {
      await page.locator('section').filter({ hasText: '更新策略' }).locator('select').first().selectOption('auto');
      await click(page, page.getByRole('button', { name: '保存更新策略' }), 'POST', '/api/releases/settings');
    },
    dependencies: async ({ page }) => {
      await usable(page.getByText('1.0.0').first());
    },
    'dependencies-check': ({ page }) => click(page, page.getByRole('button', { name: '检查依赖更新' }), 'POST', '/api/dependencies/check'),
    'dependencies-approve': ({ page }) => clickConfirm(page, page.getByRole('button', { name: '批准升级' }).first(),
      'POST', '/api/dependencies/approve', 200, /批准升级/),
  },

  audit: {
    recent: async ({ page }) => {
      await usable(page.locator('table tbody tr').first());
    },
    older: async ({ page, api }) => {
      for (let i = 0; i < 55; i++) {
        await api('/api/socialV2/config', { autoReplyCheckMs: 4000 + i });
      }
      await click(page, page.getByRole('button', { name: '刷新视图' }), 'GET', '/api/audit/recent');
      await usable(page.locator('table tbody tr').first());
      await click(page, page.getByRole('button', { name: /加载更多历史记录/ }), 'GET', /^\/api\/audit\/recent\?limit=100$/);
    },
    undo: async ({ page }) => {
      const inspectBtn = page.getByRole('button', { name: '检视 / 撤销' }).first();
      await usable(inspectBtn, true);
      await inspectBtn.click();
      const drawer = inspectorDrawer(page);
      await usable(drawer);
      await clickConfirm(page, drawer.getByRole('button', { name: '撤销该变更' }), 'POST', '/api/audit/undo', 200, /确认撤销/);
      await drawer.getByRole('button', { name: '关闭抽屉' }).click().catch(() => {});
    },
    'turns-stuck': async ({ page }) => {
      await switchTab(page, '恢复与中断记录');
      await usable(page.getByText('fixture-paused-turn').first());
    },
    'turns-resolve': ({ page }) => clickConfirm(page, page.getByRole('button', { name: '重试回合' }).first(),
      'POST', '/api/turns/resolve', 200, /重试|确认/),
    'inbound-gaps': async ({ page }) => {
      await usable(page.getByText('Fixture gap').first());
    },
    'inbound-retry': ({ page }) => clickConfirm(page, page.getByRole('button', { name: '重试缺口' }).first(),
      'POST', '/api/inbound/gaps/retry', 200, /确认重试缺口/),
    'recovery-record': async ({ page }) => {
      const rec = await click(
        page,
        page.getByRole('button', { name: '刷新视图' }).first(),
        'GET',
        /^\/api\/recovery-record/,
      );
      assert.equal(rec.ok, true);
      await usable(page.getByText(/依赖与恢复运行记录/).first());
      await usable(page.getByText('Fixture recovery entry').first());
    },
  },

  legacy: {
    'v1-timeout': async ({ page }) => {
      await page.getByRole('switch', { name: '启用活跃持续上限' }).click();
      await click(page, page.getByRole('button', { name: '保存参数' }), 'POST', '/api/social');
    },
    'v1-burst': async ({ page }) => {
      await page.getByRole('switch', { name: '启用分条发送' }).click();
      await click(page, page.getByRole('button', { name: '保存参数' }), 'POST', '/api/social');
    },
    'v1-topic': async ({ page }) => {
      await page.getByRole('switch', { name: '启用主动开话题' }).click();
      await click(page, page.getByRole('button', { name: '保存参数' }), 'POST', '/api/social');
    },
    'v1-flush': ({ page }) => allowHttpErrors(page, [503], async () => {
      const modelRefresh = page.waitForResponse(res => new URL(res.url()).pathname === '/api/dsh/model');
      await click(page, page.getByRole('button', { name: '刷新全部待处理队列' }), 'POST', '/api/social/flush');
      await modelRefresh;
    }, '/api/dsh/model'),
    'v1-phase': ({ page }) => allowHttpErrors(page, [503], async () => {
      const row = page.locator('table tbody tr').filter({ hasText: GROUP_KEY }).first();
      await usable(row);
      const idleModelRefresh = page.waitForResponse(res => new URL(res.url()).pathname === '/api/dsh/model');
      const toIdle = await click(page, row.getByRole('button', { name: '切换为空闲' }),
        'POST', '/api/social/state', 200, true, req => {
          const body = req.postDataJSON();
          assert.equal(body.key, GROUP_KEY);
          assert.equal(body.phase, 'idle');
        });
      assert.equal(toIdle.phase, 'idle');
      await idleModelRefresh;
      const activeModelRefresh = page.waitForResponse(res => new URL(res.url()).pathname === '/api/dsh/model');
      const toActive = await click(page, row.getByRole('button', { name: '切换为活跃' }),
        'POST', '/api/social/state', 200, true, req => {
          const body = req.postDataJSON();
          assert.equal(body.key, GROUP_KEY);
          assert.equal(body.phase, 'active');
        });
      assert.equal(toActive.phase, 'active');
      await activeModelRefresh;
    }, '/api/dsh/model'),
    'dsh-model': ({ page }) => allowHttpErrors(page, [503], () =>
      click(page, page.getByRole('button', { name: '查看 DSH 模型' }), 'GET', '/api/dsh/model', 503, false), '/api/dsh/model'),
    'dsh-effort': ({ page }) => allowHttpErrors(page, [503], () =>
      click(page, page.getByRole('button', { name: '保存思考强度' }), 'POST', '/api/dsh/effort', 503, false), '/api/dsh/effort'),
    'dsh-notify': async ({ page }) => {
      await page.getByPlaceholder('group:<群号> 或 private:<QQ号>').fill(GROUP_KEY);
      await page.getByPlaceholder('输入要通知 DSH agent 的提醒内容…').fill('Fixture DSH notice');
      await allowHttpErrors(page, [503], () =>
        click(page, page.getByRole('button', { name: '发送通知' }), 'POST', '/api/console/notify-ai', 503, false), '/api/console/notify-ai');
    },
    'dsh-reference': async ({ page }) => {
      await usable(page.getByText('qq_status').first());
      await usable(page.getByText('qq_send_group_message').first());
    },
  },

  auth: {
    setup: async ({ page, state }) => {
      const initialPhrase = 'fixture-phrase-one';
      const canonical = state.canonicalOrigin;
      await page.goto(`${canonical}/setup`, { waitUntil: 'domcontentloaded' });
      await page.locator('#setup-console-token').fill(state.token);
      await page.locator('#setup-password').fill(initialPhrase);
      await page.locator('#setup-password-confirm').fill(initialPhrase);
      await click(page, page.getByRole('button', { name: '完成设置并进入控制台' }), 'POST', '/api/auth/first-run');
      await page.waitForURL(url => url.pathname === '/');
      state.authPhrase = initialPhrase;
    },
    login: async ({ page, state }) => {
      if (new URL(page.url()).pathname !== '/login') {
        const signOutBtn = page.getByRole('button', { name: '退出登录' });
        if (await signOutBtn.isVisible().catch(() => false)) {
          await click(page, signOutBtn, 'POST', '/api/auth/logout');
          await page.waitForURL(url => url.pathname === '/login');
        } else {
          await page.goto(`${state.canonicalOrigin}/login`, { waitUntil: 'domcontentloaded' });
        }
      }
      await page.getByPlaceholder('输入管理员密码…').fill(state.authPhrase);
      await click(page, page.getByRole('button', { name: '登录' }).first(), 'POST', '/api/auth/login');
      await page.waitForURL(url => url.pathname === '/');
    },
  },

  security: {
    'browser-sessions': async ({ page, state }) => {
      const canonical = new URL(state.canonicalOrigin);
      const body = JSON.stringify({ password: state.authPhrase });
      await new Promise((resolve, reject) => {
        const req = httpRequest({
          host: '127.0.0.1',
          port: Number(canonical.port),
          path: '/api/auth/login',
          method: 'POST',
          headers: {
            host: canonical.host,
            origin: state.canonicalOrigin,
            'content-type': 'application/json',
            'content-length': Buffer.byteLength(body),
          },
        }, res => {
          const chunks = [];
          res.on('data', chunk => chunks.push(chunk));
          res.on('end', () => {
            if (res.statusCode === 200) resolve();
            else reject(new Error(`Secondary browser login HTTP ${res.statusCode}: ${Buffer.concat(chunks).toString('utf8')}`));
          });
        });
        req.setTimeout(5000, () => req.destroy(new Error('Secondary browser login timeout')));
        req.on('error', reject);
        req.write(body);
        req.end();
      });
      await page.goto(`${state.canonicalOrigin}/security`, { waitUntil: 'domcontentloaded' });
      await page.locator('h1').filter({ hasText: '权限与安全' }).waitFor({ state: 'visible' });
      await usable(page.getByText('当前会话').first());
      const res = await clickConfirm(page, page.getByRole('button', { name: '注销其他会话' }),
        'POST', '/api/auth/sessions/revoke', 200, /确认吊销/,
        req => assert.equal(req.postDataJSON().others, true));
      assert.equal(res.revoked, 1);
    },
    'password-change': async ({ page, state }) => {
      const nextPhrase = 'fixture-phrase-two';
      await page.getByPlaceholder('输入当前管理员密码').fill(state.authPhrase);
      await page.getByPlaceholder('输入新密码', { exact: true }).fill(nextPhrase);
      await page.getByPlaceholder('再次输入新密码', { exact: true }).fill(nextPhrase);
      await click(page, page.getByRole('button', { name: '更新密码' }), 'POST', '/api/auth/change-password');
      state.authPhrase = nextPhrase;
    },
    'local-password': async ({ page }) => {
      const sw = page.getByRole('switch', { name: '本机回环仍需密码登录' });
      await clickConfirm(page, sw, 'POST', '/api/security', 200, /确认修改/);
      await clickConfirm(page, sw, 'POST', '/api/security', 200, /确认修改/);
    },
    'allow-list': async ({ page }) => {
      await page.getByPlaceholder('每行一个 QQ 号').fill(`${OWNER}\n90002`);
      await clickConfirm(page, page.getByRole('button', { name: '保存白名单' }), 'POST', '/api/settings/set', 200, /保存白名单/);
    },
    'deny-list': async ({ page }) => {
      await page.getByPlaceholder('每行一个拦截 QQ 号').fill('90001');
      await clickConfirm(page, page.getByRole('button', { name: '保存白名单' }), 'POST', '/api/settings/set', 200, /保存白名单/);
    },
    owner: async ({ page }) => {
      await page.getByPlaceholder('留空表示取消绑定主人 QQ').fill(String(OWNER + 1));
      await clickConfirm(page, page.getByRole('button', { name: '保存白名单' }), 'POST', '/api/settings/set', 200, /保存白名单/);
    },
    'security-notify': async ({ page }) => {
      await clickConfirm(page, page.getByRole('switch', { name: '出站拦截后通知主人' }), 'POST', '/api/security', 200, /确认修改/);
    },
    'security-harden': ({ page }) => click(page, page.getByRole('button', { name: '重新加固目录' }), 'POST', '/api/security'),
    'authorize-read': async ({ page }) => {
      await page.getByPlaceholder('例如 private:10001 或 group:10001').fill(GROUP_KEY);
      await page.getByPlaceholder('输入待验证的会话令牌').fill(TOKENS[GROUP_KEY]);
      await click(page, page.getByRole('button', { name: '验证读取权限' }), 'POST', '/api/authorize/read');
    },
    'memory-access': async ({ page }) => {
      await page.getByPlaceholder('readerId (如 private:10001)').fill(GROUP_KEY);
      await page.getByPlaceholder('目标 readId').fill(GROUP_KEY);
      await click(page, page.getByRole('button', { name: '添加 ACL 授权' }), 'POST', '/api/memory/acl');
      await clickConfirm(page, page.getByRole('button', { name: '撤销', exact: true }).first(), 'POST', '/api/memory/acl', 200, /撤销|确认/);
    },
    'token-rotate': async ({ page }) => {
      await page.getByPlaceholder('可选：手动指定新控制台令牌（留空则自动生成）').fill('fixture-rotated-console-token-01');
      await clickConfirm(page, page.getByRole('button', { name: '轮换控制台令牌' }), 'POST', '/api/console/token', 200, /轮换|确认/);
    },
  },
};
