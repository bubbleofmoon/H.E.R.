import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { chromium, firefox, webkit } from 'playwright';
import { validateCatalogue } from './catalogue.mjs';
import { withFixture } from './fixture.mjs';
import { writeReport } from './report.mjs';
import { openPage, pages, unavailablePages, walks } from './walk.mjs';
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

const dimensions = [
  { width: 1920, height: 1080 }, { width: 1366, height: 768 }, { width: 900, height: 800 },
];
const themes = ['light', 'dark'];
const viewportSize = ({ width, height }) => `${width}x${height}`;
const isWindows = process.platform === 'win32';
const platform = isWindows ? 'windows' : 'linux';
const browsers = isWindows ? { msedge: [chromium, { channel: 'msedge' }] }
  : { chromium: [chromium], firefox: [firefox], webkit: [webkit] };
const directory = path.join(HER_UI_OUTPUT, `console-ui-${HER_UI_COMMIT}-${platform}`);
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

async function restrictToConsole(context, origin) {
  await context.route('**/*', route => new URL(route.request().url()).origin === origin
    ? route.continue() : route.abort());
  await context.routeWebSocket('**/*', socket => socket.close());
}

async function withPage(browser, viewport, origin, run) {
  const context = await browser.newContext({
    viewport, colorScheme: 'light', timezoneId: 'UTC', serviceWorkers: 'block',
  });
  try {
    await restrictToConsole(context, origin);
    const page = await context.newPage();
    page.setDefaultTimeout(8000);
    page.setDefaultNavigationTimeout(8000);
    await run(page);
  } finally {
    await context.close();
  }
}

async function capture(page, entry, origin, spec, token) {
  const { browser, size, route, theme } = entry;
  const name = `${browser}-${size}-${route.slice(1).replaceAll('/', '-')}-${theme}`;
  const screenshot = async suffix => {
    const file = `screenshots/${name}${suffix}.png`;
    await page.screenshot({ path: path.join(directory, file), animations: 'disabled', timeout: 8000 });
    return file;
  };
  try {
    await page.emulateMedia({ colorScheme: theme });
    await openPage(page, origin, route, spec, token);
    report.screenshots.push({ ...entry, status: 'pass', file: await screenshot('') });
  } catch (error) {
    const detail = fixtureMessage(error);
    const file = await screenshot('-failed').catch(() => undefined);
    report.screenshots.push({ ...entry, status: 'fail', detail, file });
  }
}

async function capturePages(browser, size, takeScreenshot, skipReason) {
  for (const [group, spec] of Object.entries(pages)) {
    for (const route of [spec.route, ...(spec.aliases ?? [])]) {
      for (const theme of themes) {
        const entry = { browser, size, route, theme, hasDarkStyles: spec.hasDarkStyles ?? null };
        const detail = skipReason ?? unavailablePages[group];
        if (detail) {
          report.screenshots.push({ ...entry, status: 'skipped', detail });
        } else {
          try {
            await takeScreenshot(entry, spec);
          } catch (error) {
            report.screenshots.push({ ...entry, status: 'fail', detail: fixtureMessage(error) });
          }
        }
      }
    }
  }
}

async function runWalk(page, group, fixture, browser, size) {
  const state = {};
  for (const [key, check] of Object.entries(walks[group])) {
    const source = `${group}.${key}`;
    const result = { source, browser, size, status: 'pass' };
    const prompts = [];
    const onDialog = async dialog => {
      const next = prompts.shift();
      try {
        if (next) {
          clearTimeout(next.timer);
          next.resolve(await next.handle(dialog));
        }
        else if (dialog.type() === 'prompt') await dialog.dismiss();
        else await dialog.accept();
      } catch (error) {
        await dialog.dismiss().catch(() => {});
        next?.reject(error);
      }
    };
    const prompt = handle => new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(Error('Expected a browser dialog')), 8000);
      prompts.push({ handle, resolve, reject, timer });
    });
    page.on('dialog', onDialog);
    try {
      await check({ page, ...fixture, state, prompt });
      assert.equal(prompts.length, 0, 'Expected a dialog before the API response');
    } catch (error) {
      result.status = 'fail';
      result.detail = fixtureMessage(error);
      console.error(browser, size, source, result.detail);
    } finally {
      page.off('dialog', onDialog);
      for (const { timer } of prompts) clearTimeout(timer);
    }
    checks.set(`${browser}/${size}/${source}`, result);
  }
}

async function runMatrix(browserName, browser, viewport) {
  const size = viewportSize(viewport);
  await withFixture(HER_TEST_CLI, async fixture => {
    await capturePages(browserName, size, (entry, spec) =>
      withPage(browser, viewport, fixture.origin, page => capture(page, entry, fixture.origin, spec)));
    // Start each walk on the same page state a fresh browser sees. /tasks shares the schedule walk.
    for (const group of Object.keys(walks)) {
      const spec = pages[group];
      try {
        await withPage(browser, viewport, fixture.origin, async page => {
          await openPage(page, fixture.origin, spec.route, spec);
          await runWalk(page, group, fixture, browserName, size);
        });
      } catch (error) {
        const detail = `${spec.route} walk: ${fixtureMessage(error)}`;
        report.errors.push(`${browserName} ${size} ${detail}`);
        for (const key of Object.keys(walks[group])) {
          const source = `${group}.${key}`;
          const id = `${browserName}/${size}/${source}`;
          if (!checks.has(id)) checks.set(id, { source, browser: browserName, size, status: 'fail', detail });
        }
      }
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
        page.setDefaultTimeout(8000);
        page.setDefaultNavigationTimeout(8000);
        nativeSize = await page.evaluate(() => `${innerWidth}x${innerHeight}`);
        await capturePages(browser, nativeSize, (entry, spec) => capture(page, entry, fixture.origin, spec, session));
      });
    });
  } catch (error) {
    report.screenshots.push({ browser, status: 'fail', detail: fixtureMessage(error) });
  }
  const skipReason = nativeSize
    ? `WebView2 captures at the app viewport (${nativeSize}).`
    : 'The app viewport was unavailable. See the WebView2 failure.';
  for (const size of dimensions.map(viewportSize)) {
    if (size !== nativeSize) await capturePages(browser, size, undefined, skipReason);
  }
}

for (const [name, [engine, options]] of Object.entries(browsers)) {
  let browser;
  try {
    browser = await engine.launch(options);
    report.browsers[name] = browser.version();
    for (const viewport of dimensions) {
      try {
        await runMatrix(name, browser, viewport);
      } catch (error) {
        const size = viewportSize(viewport);
        const detail = `Fixture: ${fixtureMessage(error)}`;
        blocked.set(`${name}/${size}`, detail);
        report.errors.push(`${name} ${size}: ${detail}`);
      }
    }
  } catch (error) {
    const detail = `Browser launch: ${fixtureMessage(error)}`;
    blocked.set(name, detail);
    report.errors.push(`${name}: ${detail}`);
  } finally {
    await browser?.close().catch(error => report.errors.push(`${name} cleanup: ${fixtureMessage(error)}`));
  }
}

for (const [id, placement] of Object.entries(catalogue.functions)) {
  const results = placement.sources.flatMap(source => {
    const group = source.split('.')[0];
    if (Object.hasOwn(unavailablePages, group)) return [{ source, status: 'not yet in this UI', detail: unavailablePages[group] }];
    return Object.keys(browsers).flatMap(browser => dimensions.map(viewport => {
      const size = viewportSize(viewport);
      const result = checks.get(`${browser}/${size}/${source}`)
        ?? { source, browser, size, status: 'fail',
          detail: blocked.get(`${browser}/${size}`) ?? blocked.get(browser) ?? `No result recorded for ${source}` };
      if (!placement.readOnlySources?.includes(source)) return result;
      return { ...result, kind: 'read-only', status: result.status === 'pass' ? 'read-only' : result.status };
    }));
  });
  const statuses = results.map(result => result.status);
  const status = ['fail', 'pass', 'read-only'].find(status => statuses.includes(status)) ?? 'not yet in this UI';
  report.functions.push({ id, ...placement, status, checks: results });
}
await writeReport(directory, report);
console.log(`Console UI report: ${directory}`);
process.exitCode = Number(report.errors.length > 0 || report.screenshots.some(result => result.status === 'fail')
  || report.functions.some(result => result.status === 'fail'));
