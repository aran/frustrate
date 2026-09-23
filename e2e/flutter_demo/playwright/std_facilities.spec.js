// @ts-check
//
// Browser verification of the demo on a facility std: the byte-identical app
// code and bridge crate as web.spec.js, with only the wasm module built
// differently (`frustrate_wasm_module(platform =
// "@frustrate//bazel:wasm32_custom")` — run, in the frustrate repo,
// `dart toolchain/custom_std/tool/build.dart --facilities=clock,random,stdio,thread`,
// then `bazel build //custom_std:app_web_custom_std` here, before this spec).
//
// What this proves that the dart_integration browser suite cannot: that the
// `HashMap` seed **differs between page loads**. Within one instance std caches
// the keys per thread and workers share the module's memory, so no second read
// anywhere inside a single load is a discriminator. Two full loads is the only
// honest form of that assertion, and only a real browser driver can do it.
//
// Unlike web_threaded.spec.js this variant needs no cross-origin isolation —
// it is stock single-threaded wasm32 that happens to link a std whose stubs
// call the host. The shared test server sends COOP/COEP to every bundle it
// serves, so the page gets it anyway and `performance.now()` is the fine 5 µs
// clock rather than the coarsened 100 µs one; nothing here depends on which,
// because the clock is read across real work rather than a tight pair.
const { test, expect } = require('@playwright/test');
const path = require('path');
const {
  createFlutterServer,
  waitForFlutterReady,
  collectConsoleMessages,
} = require('../../_playwright/flutter_helpers');
const { expectStrictCspEnforced } = require('../../_playwright/csp_helpers');

const WEB_DIR = path.resolve(
  __dirname,
  '../bazel-bin/custom_std/app_web_custom_std_web',
);
let server;
let serverUrl;

test.beforeAll(async () => {
  const result = await createFlutterServer(WEB_DIR);
  server = result.server;
  serverUrl = result.url;
});

test.afterAll(async () => {
  if (server) server.close();
});

async function enableAccessibility(page) {
  const placeholder = page.locator('flt-semantics-placeholder');
  await placeholder.evaluate((el) => el.click());
}

// The gallery is a long scrolling list; wheel down over the canvas until an
// aria text regex appears (scrolling also forces Flutter to render frames).
async function scrollUntil(page, re, maxSteps = 20) {
  const view = page.locator('flutter-view');
  for (let i = 0; i < maxSteps; i++) {
    const snap = await view.ariaSnapshot();
    if (re.test(snap)) return;
    await view.hover();
    await page.mouse.wheel(0, 400);
    await page.waitForTimeout(300);
  }
  expect(await view.ariaSnapshot()).toMatch(re);
}

// Tap a button by name — same reasoning as web_threaded.spec.js: Flutter
// paints to a canvas, so a semantics node below the fold cannot be
// auto-scrolled into view by Playwright.
async function clickButton(page, name, maxSteps = 20) {
  const view = page.locator('flutter-view');
  const btn = page.getByRole('button', { name });
  for (let i = 0; i < maxSteps; i++) {
    if ((await view.ariaSnapshot()).includes(name)) {
      try {
        await btn.click({ timeout: 1500 });
        return;
      } catch (_) {
        // Present but not yet actionable (off-viewport / unstable): nudge.
      }
    }
    await view.hover();
    await page.mouse.wheel(0, 300);
    await page.waitForTimeout(300);
  }
  await btn.click({ timeout: 5000 });
}

// Poll in place until a regex matches — for a value that appears after a tap.
async function expectText(page, re, timeout = 60000) {
  const view = page.locator('flutter-view');
  const deadline = Date.now() + timeout;
  let toggle = 0;
  for (;;) {
    const snap = await view.ariaSnapshot();
    if (re.test(snap)) return;
    if (Date.now() > deadline) {
      expect(snap).toMatch(re);
      return;
    }
    await page.mouse.move(5 + (toggle ^= 1), 5);
    await page.waitForTimeout(250);
  }
}

/** Read one `name = value` reading out of the std-facilities card. */
async function readReading(page, name) {
  const view = page.locator('flutter-view');
  const re = new RegExp(`std\\.${name}\\s*=\\s*(-?\\d+)`);
  await scrollUntil(page, re);
  const m = (await view.ariaSnapshot()).match(re);
  expect(m, `std.${name} is not on the page`).not.toBeNull();
  return Number(m[1]);
}

/** Load the app fresh and return its `HashMap` seed. */
async function seedFromAFreshLoad(browser) {
  const page = await browser.newPage();
  try {
    await page.goto(serverUrl, { waitUntil: 'networkidle' });
    await waitForFlutterReady(page);
    await enableAccessibility(page);
    return await readReading(page, 'seed');
  } finally {
    await page.close();
  }
}

// The premise the CSP assertions rest on. Its own test so the deliberate
// violation it provokes stays out of the gallery run's console.
test('the served bundle carries a strict CSP, and it is enforced', async ({
  page,
}) => {
  await expectStrictCspEnforced(page, serverUrl, expect);
});

test('the std facilities work through std\'s own APIs', async ({ page }) => {
  const console_ = collectConsoleMessages(page);
  await page.goto(serverUrl, { waitUntil: 'networkidle' });

  await waitForFlutterReady(page);
  await enableAccessibility(page);

  // The module declared the frustrate.* facility imports and the glue served
  // them; had it not, instantiation would have failed with a LinkError and
  // nothing below would render at all.
  await scrollUntil(page, /facilities: present/);

  // SystemTime is the wall clock, not the monotonic one. A monotonic reading
  // has page load for an epoch and would be off by decades, so a generous
  // window around the driver's own clock is a sharp assertion despite the
  // slack: it distinguishes the two sources, which is the thing that can be
  // miswired.
  const wall = await readReading(page, 'wall');
  const nowMicros = Date.now() * 1000;
  expect(Math.abs(wall - nowMicros)).toBeLessThan(60 * 1000 * 1000);

  // Instant advanced across real work (nth_prime(200) in Rust). Never
  // asserted as a strictly increasing tight pair — at 100 µs coarsening two
  // adjacent reads legitimately return the same value.
  const mono = await readReading(page, 'mono');
  expect(mono).toBeGreaterThan(0);

  // available_parallelism reports the machine, not permission to use it:
  // this is a single-threaded build where thread::spawn still fails, and the
  // core count must still be the real one.
  const cores = await readReading(page, 'cores');
  expect(cores).toBe(
    await page.evaluate(() => navigator.hardwareConcurrency),
  );

  // println! reaches console.log, one entry per line — the host buffers per
  // stream until a newline, so the line arrives whole rather than split.
  await clickButton(page, 'println! to the console');
  await expectText(page, /std\.println = \d+/);
  const printed = await readReading(page, 'println');
  expect(printed).toBeGreaterThan(0);
  expect(console_.messages).toContain(
    '[log] frustrate demo: println! from Rust',
  );

  // thread::sleep on an actor's Worker, where waiting is permitted. The
  // busy-wait is imprecise and the clock under it is coarse, so this asserts it
  // waited at all and did not run away.
  await clickButton(page, 'sleep 20ms on an actor');
  await expectText(page, /std\.nap = \d+/);
  const napped = await readReading(page, 'nap');
  expect(napped).toBeGreaterThanOrEqual(0);
  expect(napped).toBeLessThan(5000);

  expect(console_.hasUnexpectedErrors()).toBe(false);
});

test('HashMap seeding differs between page loads', async ({ browser }) => {
  // The assertion this whole spec exists for. On a stock wasm std the seed
  // comes from *allocation addresses* in a deterministic module — std's own
  // comment concedes it "isn't particularly secure, but there isn't really an
  // alternative" — so it is close to constant across loads. With the random
  // facility it comes from crypto.getRandomValues() and must not be.
  const first = await seedFromAFreshLoad(browser);
  const second = await seedFromAFreshLoad(browser);

  // Both halves are needed. "Not the sentinel" alone would pass for a facility
  // that returns some other fixed constant every time; "differs" alone would
  // pass for two different flavours of broken. Together they say the CSPRNG
  // was reached, twice, with different results.
  expect(first).toBeGreaterThan(0);
  expect(second).toBeGreaterThan(0);
  expect(first).not.toBe(second);
});
