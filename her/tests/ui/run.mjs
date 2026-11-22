import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { request as httpRequest } from 'node:http';
import path from 'node:path';
import { chromium } from 'playwright';
import { developmentRoot } from '../../../scripts/fixture-paths.mjs';
import { validateCatalogue } from './catalogue.mjs';
import { CONSOLE_TOKEN, withFixture } from './fixture.mjs';
import { writeReport } from './report.mjs';
import { allowHttpErrors, openPage, pages, unavailablePages, walks } from './walk.mjs';
import { withWebView } from './webview.mjs';

const catalogue = JSON.parse(await fs.readFile(new URL('./catalogue.json', import.meta.url)));
validateCatalogue(catalogue, walks, unavailablePages);
if (process.argv.includes('--validate-catalogue')) {
  console.log('Console catalogue matches the declared walks');
  process.exit(0);
}

const { HER_TEST_CLI, HER_UI_COMMIT, HER_UI_OUTPUT } = process.env;
assert(HER_TEST_CLI && path.isAbsolute(HER_TEST_CLI), 'Set HER_TEST_CLI to the absolute path of the development her-cli');
assert(HER_UI_OUTPUT && path.isAbsolute(HER_UI_OUTPUT), 'Set HER_UI_OUTPUT to an absolute artifact parent directory');
assert(/^[a-f0-9]{40}$/.test(HER_UI_COMMIT ?? ''), 'Set HER_UI_COMMIT to the full source commit SHA');
assert(['linux', 'win32'].includes(process.platform), 'The hosted matrix supports Linux and Windows');

const PRIMARY_VIEWPORT = { width: 1100, height: 760 };
const NARROW_VIEWPORT = { width: 900, height: 800 };
const LINUX_MATRIX = [
  { viewport: PRIMARY_VIEWPORT, theme: 'light', includeCaptures: true },
  { viewport: NARROW_VIEWPORT, theme: 'dark', includeCaptures: false },
];
const LEGACY_REDIRECTS = [
  ['/personas', '/persona'],
  ['/tasks', '/schedule'],
  ['/settings/conversations', '/conversations'],
  ['/settings/models', '/models'],
];
const SPOT_DARK_ROUTES = ['/', '/conversations', '/models'];
const WEBVIEW_ROUTES = [
  { route: '/', heading: '总览' },
  { route: '/conversations', heading: '会话' },
  { route: '/persona', heading: '人设与提示词' },
  { route: '/models', heading: '模型与用量' },
  { route: '/schedule', heading: '日程与任务' },
  { route: '/settings', heading: '全部设置与变更历史' },
];

const viewportSize = ({ width, height }) => `${width}x${height}`;
const isWindows = process.platform === 'win32';
const platform = isWindows ? 'windows' : 'linux';
const directory = path.join(HER_UI_OUTPUT, `console-ui-${HER_UI_COMMIT}-${platform}`);
developmentRoot(HER_UI_OUTPUT, HER_TEST_CLI);
developmentRoot(directory, HER_TEST_CLI);
await fs.mkdir(HER_UI_OUTPUT, { recursive: true });
await fs.mkdir(directory);
await fs.mkdir(path.join(directory, 'screenshots'));
const report = { commit: HER_UI_COMMIT, platform, browsers: {}, screenshots: [], functions: [], errors: [] };
const checks = new Map();
const blocked = new Map();

function fixtureMessage(error) {
  const message = error instanceof Error ? error.message : String(error);
  return message.replace(/([?&]token=)[^\s&"'#]+/g, '$1[redacted]');
}

function canonicalOrigin(origin) {
  const u = new URL(origin);
  u.hostname = 'her.localhost';
  return u.origin;
}

function allowedOrigins(origin) {
  return new Set([origin, canonicalOrigin(origin)]);
}

async function restrictToConsole(context, origin) {
  const origins = allowedOrigins(origin);
  await context.route('**/*', route => origins.has(new URL(route.request().url()).origin)
    ? route.continue() : route.abort());
  await context.routeWebSocket('**/*', socket => socket.close());
}

function attachIssueWatchers(page) {
  const issues = [];
  const allowedConsoleStatuses = new Set();
  const isAllowedHttp = (status, pathname) => {
    const allowed = page.__herAllowedHttpErrors;
    if (!allowed?.statuses?.has(status)) return false;
    return !allowed.pathPrefix || pathname.startsWith(allowed.pathPrefix);
  };
  const onResponse = res => {
    const status = res.status();
    if (status < 400) return;
    let pathname = '';
    try {
      pathname = new URL(res.url()).pathname;
    } catch {
      pathname = res.url();
    }
    if (isAllowedHttp(status, pathname)) {
      allowedConsoleStatuses.add(status);
      return;
    }
    issues.push(`http ${status}: ${res.request().method()} ${pathname}`);
  };
  const onPageError = err => issues.push(`pageerror: ${fixtureMessage(err)}`);
  const onConsole = msg => {
    if (msg.type() !== 'error') return;
    const text = msg.text();
    const m = text.match(/status of (\d{3})\b/);
    if (m && allowedConsoleStatuses.has(Number(m[1]))) return;
    issues.push(`console.error: ${text}`);
  };
  page.on('response', onResponse);
  page.on('pageerror', onPageError);
  page.on('console', onConsole);
  return {
    reset: async () => {
      // Retain any delayed issues/CSP events emitted between checks so drain() cannot miss them.
      allowedConsoleStatuses.clear();
    },
    drain: async () => {
      const csp = await page.evaluate(() => {
        const list = window.__herCspViolations ?? [];
        window.__herCspViolations = [];
        return list;
      }).catch(() => []);
      const startup = page.__herStartupIssues?.splice(0) ?? [];
      const out = [...startup, ...issues, ...csp.map(v => `csp: ${v}`)];
      issues.length = 0;
      allowedConsoleStatuses.clear();
      return out;
    },
    detach: () => {
      page.off('response', onResponse);
      page.off('pageerror', onPageError);
      page.off('console', onConsole);
    },
  };
}

async function assertNoOverflow(page, label) {
  const metrics = await page.evaluate(() => {
    const main = document.querySelector('main');
    return {
      scrollWidth: document.documentElement.scrollWidth,
      innerWidth: window.innerWidth,
      mainScrollWidth: main ? main.scrollWidth : 0,
      mainClientWidth: main ? main.clientWidth : window.innerWidth,
    };
  });
  assert(metrics.scrollWidth <= metrics.innerWidth + 1,
    `${label}: document horizontal overflow ${metrics.scrollWidth} > ${metrics.innerWidth}`);
  assert(metrics.mainScrollWidth <= metrics.mainClientWidth + 1,
    `${label}: main horizontal overflow ${metrics.mainScrollWidth} > ${metrics.mainClientWidth}`);
}

async function withPage(browser, viewport, origin, run, { theme = 'light', storageState } = {}) {
  const context = await browser.newContext({
    viewport, colorScheme: theme, timezoneId: 'UTC', serviceWorkers: 'block', storageState,
  });
  try {
    await restrictToConsole(context, origin);
    await context.addInitScript(t => {
      try {
        window.localStorage.setItem('herTheme', t);
      } catch {
        // Ignore non-origin storage contexts like about:blank.
      }
      if (!window.__herCspHooked) {
        window.__herCspHooked = true;
        window.__herCspViolations = [];
        document.addEventListener('securitypolicyviolation', ev => {
          window.__herCspViolations.push(`${ev.violatedDirective}:${ev.blockedURI}`);
        });
      }
    }, theme);
    const page = await context.newPage();
    page.setDefaultTimeout(8000);
    page.setDefaultNavigationTimeout(8000);
    await run(page, context);
  } finally {
    await context.close();
  }
}

async function captureRoute(page, { browser, size, route, theme, spec, origin, token, expectedHeading, openDrawer = false }) {
  const routeSlug = (route === '/' ? 'overview' : route.slice(1).replaceAll('/', '-')) + (openDrawer ? '-drawer' : '');
  const name = `${browser}-${size}-${routeSlug}-${theme}`;
  const screenshot = async suffix => {
    const file = `screenshots/${name}${suffix}.png`;
    await page.screenshot({ path: path.join(directory, file), animations: 'disabled', timeout: 8000 });
    return file;
  };
  const watcher = attachIssueWatchers(page);
  const entry = { browser, size, route: openDrawer ? `${route}#drawer` : route, theme, hasDarkStyles: theme !== 'dark' };
  const loadTarget = async () => {
    if (spec) {
      await openPage(page, origin, route, spec, token);
      return;
    }
    const target = new URL(route, origin);
    if (token) target.searchParams.set('token', token);
    const nav = await page.goto(target.toString(), { waitUntil: 'domcontentloaded' });
    assert.equal(nav?.status(), 200, `GET ${route}: unexpected status ${nav?.status()}`);
    await page.waitForFunction(expected => window.location.pathname === expected, route, { timeout: 8000 });
    if (expectedHeading) {
      await page.locator('h1').filter({ hasText: expectedHeading }).first().waitFor({ state: 'visible', timeout: 10_000 });
    } else {
      await page.locator('h1').first().waitFor({ state: 'visible', timeout: 10_000 });
    }
    await page.waitForLoadState('networkidle').catch(() => {});
  };
  try {
    await page.emulateMedia({ colorScheme: theme }).catch(() => {});
    if (route === '/legacy') {
      await allowHttpErrors(page, [503], loadTarget, '/api/dsh/model');
    } else {
      await loadTarget();
    }
    if (theme === 'dark') {
      await page.waitForFunction(() => document.documentElement.classList.contains('dark'), null, { timeout: 5000 });
      entry.hasDarkStyles = await page.evaluate(() => {
        if (!document.documentElement.classList.contains('dark')) return false;
        const bg = getComputedStyle(document.body).backgroundColor;
        const m = bg.match(/^rgb\((\d+),\s*(\d+),\s*(\d+)\)$/);
        if (!m) return false;
        return Number(m[1]) <= 32 && Number(m[2]) <= 32 && Number(m[3]) <= 32;
      });
      assert(entry.hasDarkStyles, `${browser} ${size} ${entry.route} (dark): computed background did not switch to dark canvas`);
    }
    if (openDrawer) {
      const row = page.locator('table tbody tr').first();
      await row.waitFor({ state: 'visible', timeout: 8000 });
      await row.click();
      await page.getByRole('complementary', { name: '右侧详情检视抽屉' }).waitFor({ state: 'visible', timeout: 8000 });
    }
    await assertNoOverflow(page, `${browser} ${size} ${entry.route} (${theme})`);
    const issues = await watcher.drain();
    assert.equal(issues.length, 0, issues.join('; '));
    report.screenshots.push({ ...entry, status: 'pass', file: await screenshot('') });
  } catch (error) {
    const detail = fixtureMessage(error);
    const file = await screenshot('-failed').catch(() => undefined);
    report.screenshots.push({ ...entry, status: 'fail', detail, file });
  } finally {
    watcher.detach();
  }
}

function probeLoopbackGet(origin, pathAndQuery, headers = {}) {
  return new Promise((resolve, reject) => {
    const url = new URL(origin);
    const req = httpRequest({
      host: '127.0.0.1',
      port: Number(url.port),
      path: pathAndQuery,
      method: 'GET',
      headers: { host: url.host, ...headers },
    }, res => {
      res.resume();
      res.on('end', () => resolve({ status: res.statusCode ?? 0, location: res.headers.location ?? null }));
    });
    req.setTimeout(5000, () => req.destroy(new Error(`Redirect probe timeout: ${origin}${pathAndQuery}`)));
    req.on('error', reject);
    req.end();
  });
}

async function verifyLegacyRedirects(browser, fixture, canonical, storageState, viewport) {
  const cookieHeader = (storageState?.cookies ?? []).map(c => `${c.name}=${c.value}`).join('; ');
  for (const [legacyPath, canonicalPath] of LEGACY_REDIRECTS) {
    const resCanonical = await probeLoopbackGet(canonical, legacyPath, cookieHeader ? { cookie: cookieHeader } : {});
    if (resCanonical.status !== 302 || resCanonical.location !== canonicalPath) {
      report.errors.push(`Canonical redirect ${legacyPath} -> ${canonicalPath}: got HTTP ${resCanonical.status} (${resCanonical.location})`);
    }
    const resAlias = await probeLoopbackGet(fixture.origin, legacyPath);
    const expectedAliasLocation = `${canonical}${canonicalPath}`;
    if (resAlias.status !== 302 || resAlias.location !== expectedAliasLocation) {
      report.errors.push(`Alias redirect ${legacyPath} -> ${expectedAliasLocation}: got HTTP ${resAlias.status} (${resAlias.location})`);
    }
  }
  await withPage(browser, viewport, fixture.origin, async page => {
    for (const [legacyPath, canonicalPath] of LEGACY_REDIRECTS) {
      await page.goto(`${canonical}${legacyPath}`, { waitUntil: 'domcontentloaded' });
      await page.waitForFunction(expected => window.location.pathname === expected, canonicalPath, { timeout: 8000 });
    }
  }, { storageState });
}

function recordBlockedWalk(group, size, detail) {
  report.errors.push(`chromium ${size} ${detail}`);
  for (const key of Object.keys(walks[group])) {
    const source = `${group}.${key}`;
    const id = `chromium/${size}/${source}`;
    if (!checks.has(id)) checks.set(id, { source, browser: 'chromium', size, status: 'fail', detail });
  }
}

async function runSingleCheck(page, group, key, fixture, browser, size, sharedState, watcher) {
  const source = `${group}.${key}`;
  const result = { source, browser, size, status: 'pass' };
  await watcher.reset();
  try {
    await walks[group][key]({ page, ...fixture, state: sharedState });
    await assertNoOverflow(page, `${browser} ${size} ${source}`);
    const issues = await watcher.drain();
    assert.equal(issues.length, 0, issues.join('; '));
  } catch (error) {
    await watcher.drain().catch(() => []);
    result.status = 'fail';
    result.detail = fixtureMessage(error);
    const file = `screenshots/${browser}-${size}-walk-${group}-${key}-failed.png`;
    await page.screenshot({ path: path.join(directory, file), animations: 'disabled', timeout: 5000 }).catch(() => {});
    console.error(browser, size, source, result.detail);
  }
  checks.set(`${browser}/${size}/${source}`, result);
}

async function runWalk(page, group, fixture, browser, size, sharedState, existingWatcher) {
  const watcher = existingWatcher ?? attachIssueWatchers(page);
  try {
    for (const key of Object.keys(walks[group])) {
      await runSingleCheck(page, group, key, fixture, browser, size, sharedState, watcher);
    }
  } finally {
    if (!existingWatcher) watcher.detach();
  }
}

async function runLinuxViewport(browser, { viewport, theme, includeCaptures }) {
  const size = viewportSize(viewport);
  // Fixture A: captures + 11 independent functional groups (may mutate social-v2 tokens via v2-reset).
  await withFixture(HER_TEST_CLI, async fixture => {
    const canonical = canonicalOrigin(fixture.origin);
    const sharedState = { token: CONSOLE_TOKEN, canonicalOrigin: canonical };
    let authState;

    await withPage(browser, viewport, fixture.origin, async (page, context) => {
      if (includeCaptures) {
        await captureRoute(page, {
          browser: 'chromium', size, route: '/setup', theme: 'light',
          origin: canonical, expectedHeading: '首次设置管理员密码',
        });
      }
      await page.goto(`${canonical}/setup`, { waitUntil: 'domcontentloaded' });
      await page.locator('#setup-console-token').fill(CONSOLE_TOKEN);
      await page.locator('#setup-password').fill('fixture-phrase-one');
      await page.locator('#setup-password-confirm').fill('fixture-phrase-one');
      await Promise.all([
        page.waitForResponse(res => new URL(res.url()).pathname === '/api/auth/first-run' && res.status() === 200),
        page.getByRole('button', { name: '完成设置并进入控制台' }).click(),
      ]);
      await page.waitForURL(url => url.pathname === '/');
      authState = await context.storageState();
    }, { theme });

    if (includeCaptures) {
      await withPage(browser, viewport, fixture.origin, async page => {
        await captureRoute(page, {
          browser: 'chromium', size, route: '/login', theme: 'light',
          origin: canonical, expectedHeading: 'HER 控制台登录',
        });
      }, { theme: 'light' });
      await withPage(browser, NARROW_VIEWPORT, fixture.origin, async page => {
        await captureRoute(page, {
          browser: 'chromium', size: viewportSize(NARROW_VIEWPORT), route: '/login', theme: 'dark',
          origin: canonical, expectedHeading: 'HER 控制台登录',
        });
      }, { theme: 'dark' });

      await verifyLegacyRedirects(browser, fixture, canonical, authState, viewport);

      const authedPages = Object.entries(pages)
        .filter(([group]) => group !== 'auth')
        .map(([, spec]) => spec);
      await withPage(browser, viewport, fixture.origin, async page => {
        for (const spec of authedPages) {
          await captureRoute(page, {
            browser: 'chromium', size, route: spec.route, theme: 'light',
            spec, origin: canonical, expectedHeading: spec.heading,
          });
        }
      }, { theme: 'light', storageState: authState });

      await withPage(browser, NARROW_VIEWPORT, fixture.origin, async page => {
        for (const route of SPOT_DARK_ROUTES) {
          const spec = Object.values(pages).find(p => p.route === route);
          await captureRoute(page, {
            browser: 'chromium', size: viewportSize(NARROW_VIEWPORT), route, theme: 'dark',
            origin: canonical, expectedHeading: spec?.heading, openDrawer: route === '/conversations',
          });
        }
      }, { theme: 'dark', storageState: authState });
    }

    // Run behavior after audit so behavior.v2-reset cannot cancel fixture-paused-turn prior to audit.turns-*.
    const independentGroups = [
      ...Object.keys(walks).filter(g => g !== 'auth' && g !== 'security' && g !== 'behavior'),
      'behavior',
    ];
    for (const group of independentGroups) {
      const spec = pages[group];
      try {
        await withPage(browser, viewport, fixture.origin, async page => {
          const watcher = attachIssueWatchers(page);
          try {
            if (group === 'legacy') {
              await allowHttpErrors(page, [503], () => openPage(page, canonical, spec.route, spec), '/api/dsh/model');
            } else {
              await openPage(page, canonical, spec.route, spec);
            }
            const startupIssues = await watcher.drain();
            assert.equal(startupIssues.length, 0, `${spec.route} startup: ${startupIssues.join('; ')}`);
            await runWalk(page, group, fixture, 'chromium', size, sharedState, watcher);
          } finally {
            watcher.detach();
          }
        }, { theme, storageState: authState });
      } catch (error) {
        recordBlockedWalk(group, size, `${spec.route} walk: ${fixtureMessage(error)}`);
      }
    }
  });

  // Fixture B: isolated auth + security walks so v2-reset cannot revoke TOKENS[GROUP_KEY] prior to authorize-read.
  await withFixture(HER_TEST_CLI, async fixture => {
    const canonical = canonicalOrigin(fixture.origin);
    const sharedState = { token: CONSOLE_TOKEN, canonicalOrigin: canonical };
    try {
      await withPage(browser, viewport, fixture.origin, async page => {
        const watcher = attachIssueWatchers(page);
        try {
          await runWalk(page, 'auth', fixture, 'chromium', size, sharedState, watcher);
          await runWalk(page, 'security', fixture, 'chromium', size, sharedState, watcher);
        } finally {
          watcher.detach();
        }
      }, { theme });
    } catch (error) {
      recordBlockedWalk('auth', size, `auth/security walk: ${fixtureMessage(error)}`);
      recordBlockedWalk('security', size, `auth/security walk: ${fixtureMessage(error)}`);
    }
  });
}

if (isWindows) {
  const browser = 'WebView2';
  let nativeSize;
  try {
    await withFixture(HER_TEST_CLI, async fixture => {
      await withWebView(HER_TEST_CLI, fixture, async (page, session, version) => {
        report.browsers[browser] = version;
        await restrictToConsole(page.context(), fixture.origin);
        await page.evaluate(() => {
          if (!window.__herCspHooked) {
            window.__herCspHooked = true;
            window.__herCspViolations = [];
            document.addEventListener('securitypolicyviolation', ev => {
              window.__herCspViolations.push(`${ev.violatedDirective}:${ev.blockedURI}`);
            });
          }
        });
        await page.addInitScript(() => {
          if (!window.__herCspHooked) {
            window.__herCspHooked = true;
            window.__herCspViolations = [];
            document.addEventListener('securitypolicyviolation', ev => {
              window.__herCspViolations.push(`${ev.violatedDirective}:${ev.blockedURI}`);
            });
          }
        });
        page.setDefaultTimeout(8000);
        page.setDefaultNavigationTimeout(8000);
        nativeSize = await page.evaluate(() => `${innerWidth}x${innerHeight}`);

        const bannerWatcher = attachIssueWatchers(page);
        try {
          await page.evaluate(() => window.herQqLogin?.('needs-login'));
          const loginBtn = page.getByRole('button', { name: '打开 QQ 登录窗口' });
          await loginBtn.waitFor({ state: 'visible', timeout: 5000 });
          await page.evaluate(() => window.herQqLogin?.('logged-in'));
          await loginBtn.waitFor({ state: 'hidden', timeout: 5000 });
          const bannerIssues = await bannerWatcher.drain();
          assert.equal(bannerIssues.length, 0, bannerIssues.join('; '));
        } catch (error) {
          const file = `screenshots/WebView2-${nativeSize}-qq-login-banner-failed.png`;
          await page.screenshot({ path: path.join(directory, file), animations: 'disabled', timeout: 5000 }).catch(() => {});
          report.screenshots.push({
            browser, size: nativeSize, route: '/#qq-login-banner', theme: 'light',
            status: 'fail', detail: fixtureMessage(error), file,
          });
        } finally {
          bannerWatcher.detach();
        }

        for (const { route, heading } of WEBVIEW_ROUTES) {
          await captureRoute(page, {
            browser, size: nativeSize, route, theme: 'light', origin: fixture.origin, token: session, expectedHeading: heading,
          });
        }
      });
    });
  } catch (error) {
    report.screenshots.push({ browser, status: 'fail', detail: fixtureMessage(error) });
  }
} else {
  let browser;
  try {
    browser = await chromium.launch();
    report.browsers.chromium = browser.version();
    for (const runSpec of LINUX_MATRIX) {
      const size = viewportSize(runSpec.viewport);
      try {
        await runLinuxViewport(browser, runSpec);
      } catch (error) {
        const detail = `Fixture: ${fixtureMessage(error)}`;
        blocked.set(`chromium/${size}`, detail);
        report.errors.push(`chromium ${size}: ${detail}`);
      }
    }
  } catch (error) {
    const detail = `Chromium run: ${fixtureMessage(error)}`;
    blocked.set('chromium', detail);
    report.errors.push(detail);
  } finally {
    await browser?.close().catch(error => report.errors.push(`chromium cleanup: ${fixtureMessage(error)}`));
  }
}

for (const [id, placement] of Object.entries(catalogue.functions)) {
  if (isWindows) {
    report.functions.push({
      id, ...placement, status: 'skipped', detail: '153-function walk runs on Linux Chromium.', checks: [],
    });
    continue;
  }
  const results = placement.sources.flatMap(source => LINUX_MATRIX.map(({ viewport }) => {
    const size = viewportSize(viewport);
    const result = checks.get(`chromium/${size}/${source}`)
      ?? { source, browser: 'chromium', size, status: 'fail',
        detail: blocked.get(`chromium/${size}`) ?? blocked.get('chromium') ?? `No result recorded for ${source}` };
    if (!placement.readOnlySources?.includes(source)) return result;
    return { ...result, kind: 'read-only', status: result.status === 'pass' ? 'read-only' : result.status };
  }));
  const statuses = results.map(result => result.status);
  const status = ['fail', 'pass', 'read-only'].find(s => statuses.includes(s)) ?? 'fail';
  report.functions.push({ id, ...placement, status, checks: results });
}
await writeReport(directory, report);
console.log(`Console UI report: ${directory}`);
process.exitCode = Number(report.errors.length > 0 || report.screenshots.some(result => result.status === 'fail')
  || report.functions.some(result => result.status === 'fail'));
