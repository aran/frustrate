// @ts-check
//
// Browser verification of the demo on the threaded web transport: the
// byte-identical app code and bridge crate as web.spec.js, with only the
// wasm module built differently (nightly + locally built +atomics std via
// `frustrate_wasm_module(platform = "@frustrate//bazel:wasm32_threads")` — run
// `dart toolchain/custom_std/tool/build.dart` in the frustrate repo,
// then `bazel build //threaded:app_web_threaded` here, before this spec).
//
// This spec is the "COOP/COEP served headers in the Flutter integration"
// proof: crossOriginIsolated must be true from headers alone — no Chrome
// SharedArrayBuffer flag anywhere (unlike the dart test harness) — and the
// shared-memory pool must deliver real parallelism through those headers.
//
// The bundle's index.html carries the same strict CSP meta as the default
// app — injected at build time by web_csp.bzl, asserted present and enforced
// by the first test below — so this spec is also
// the CSP proof for the threaded transport: the pool's worker threads AND the
// actor workers must come from the served frustrate.js — blob: workers are
// counted and only the Flutter engine's skwasm render worker may be one.
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
  '../bazel-bin/threaded/app_web_threaded_web',
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
async function scrollUntil(page, re, maxSteps = 14) {
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

// Tap a button by name: Flutter paints to a canvas, so a semantics node below
// the fold can't be auto-scrolled into view by Playwright — a plain .click()
// would auto-wait until the whole test times out. Scroll the list until the
// button is actionable, retrying with a short per-attempt bound.
async function clickButton(page, name, maxSteps = 16) {
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

// Poll in place (no scrolling) until a regex matches — for a computed result
// that appears after a button tap. Nudge the mouse each poll so Flutter web's
// idle frame throttling doesn't defer the semantics update.
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

// Count blob: workers from before any page script runs (see web.spec.js).
async function countBlobWorkers(page) {
  await page.addInitScript(() => {
    window.__blobWorkerCount = 0;
    const OrigWorker = window.Worker;
    // @ts-ignore
    window.Worker = function (url, opts) {
      if (String(url).startsWith('blob:')) window.__blobWorkerCount++;
      return new OrigWorker(url, opts);
    };
  });
  return () => page.evaluate(() => window.__blobWorkerCount);
}

// The premise the CSP assertions below rest on. Its own test so the
// deliberate violation it provokes stays out of the gallery run's console.
test('the served bundle carries a strict CSP, and it is enforced', async ({
  page,
}) => {
  await expectStrictCspEnforced(page, serverUrl, expect);
});

test('threaded: served COOP/COEP, pool parallelism, actors intact', async ({
  page,
}) => {
  const console_ = collectConsoleMessages(page);
  const blobWorkers = await countBlobWorkers(page);
  await page.goto(serverUrl, { waitUntil: 'networkidle' });

  try {
    // The served headers, not a browser flag, are what make the shared
    // WebAssembly.Memory legal — the whole point of this variant.
    expect(await page.evaluate(() => crossOriginIsolated)).toBe(true);

    await waitForFlutterReady(page);
    await enableAccessibility(page);

    // The served glue ran and registered itself; the pool workers the
    // bridge spawned during init came from it (any blob: worker so far is
    // the engine's skwasm render bootstrap).
    expect(
      await page.evaluate(() => globalThis.$frustrateGlueUrl),
    ).toMatch(/frustrate\.js$/);
    const engineBlobWorkers = await blobWorkers();
    expect(engineBlobWorkers).toBeLessThanOrEqual(1);

    const view = page.locator('flutter-view');

    // The bridge came up on the shared-memory module: sync and async work
    // through the same surface as single-threaded web.
    await scrollUntil(page, /Hello, Flutter — from Rust/);
    await scrollUntil(page, /The 10000th prime is 104729/);

    // Values & data types render identically on the shared-memory module.
    // These cards sit above the concurrency section, so assert them before
    // scrolling down to the races.
    await scrollUntil(page, /U\+1F980/);
    await scrollUntil(page, /340282366920938463463374607431768211455/);
    await scrollUntil(page, /tallest bucket: l = 3/);
    await scrollUntil(page, /1:1\s+3:2\s+5:3/);
    await scrollUntil(page, /\{1, 3, 5\}/);
    await scrollUntil(page, /5, 1, 3, 5, 5, 3/);
    await scrollUntil(page, /9 chars, 9 bytes/);
    await scrollUntil(page, /Passport == local: true/);
    await scrollUntil(page, /holder = null/);
    await scrollUntil(page, /Tickets == : false/);

    // Actors keep working unchanged on the threaded module (each actor
    // worker instantiates the same module with its own memory). Tested first
    // because its card sits above the pool race in the list.
    await scrollUntil(page, /Race 4 actors against 1/);
    await clickButton(page, 'Race 4 actors against 1');
    await expectText(page, /4 actors ran [\d.]+x faster than one/);
    const actorSnapshot = await view.ariaSnapshot();
    const actorSpeedup = parseFloat(
      actorSnapshot.match(/4 actors ran ([\d.]+)x/)[1],
    );
    expect(actorSpeedup).toBeGreaterThan(1.5);

    // The pool race: plain async calls, genuinely parallel — the threaded
    // build's reason to exist. Single-threaded this renders ~1.0x; here the
    // shared-memory pool must clear the same threshold the integration
    // suite's pool bench asserts.
    await scrollUntil(page, /Race the async pool/);
    await clickButton(page, 'Race the async pool');
    await expectText(page, /async pool ran [\d.]+x faster than serial/);
    const poolSnapshot = await view.ariaSnapshot();
    const poolSpeedup = parseFloat(
      poolSnapshot.match(/async pool ran ([\d.]+)x/)[1],
    );
    expect(poolSpeedup).toBeGreaterThan(1.5);

    // Pool threads and five actor workers all rode the served
    // frustrate.js — zero new blob: workers since Flutter booted.
    expect(await blobWorkers()).toBe(engineBlobWorkers);

    // Async fn on the threaded module: a real Rust `async fn` (.await) driven
    // by the executor on pool worker threads here. 1000 in flight still all
    // resolve — the same multiplexing surface as single-threaded web.
    await scrollUntil(page, /cooperativeYield\(21, rounds: 3\) = 42/);
    await clickButton(page, 'Fire 1000 concurrently');
    await expectText(page, /1000\/1000 resolved in \d+ms \(all correct\)/);

    // Streams & callbacks: the portable awaited callback resolves on the
    // threaded module too (async fn awaiting call_async), and blockingRead's
    // web="runtime_fail" body still throws its attributable UnsupportedError.
    await scrollUntil(page, /transform\(9\) = 81/);
    await scrollUntil(page, /blockingRead — opt-in runtime-fail/);
    await clickButton(page, 'read now');
    await expectText(page, /blockingRead is not available on web/);

    // The std facilities are absent HERE TOO, and that is the interesting
    // case: this build does link a locally built std, but the `atomics`
    // flavour replaces no stubs — it only turns on shared memory. So the
    // readings must still be sentinels. If they were not, the crate feature
    // would be leaking onto a platform whose std cannot honour it, and
    // Instant::now() would panic inside std in a Worker rather than report
    // -1. That is the exact failure //bazel:wasm_facility_std_build's
    // wasm_threads_disabled constraint exists to prevent, asserted end to end.
    await scrollUntil(page, /facilities: absent/);
    await scrollUntil(page, /std\.seed\s*=\s*-1/);

    await page.screenshot({ path: '/tmp/demo_web_threaded.png' });
  } catch (e) {
    console.log('Console messages:', console_.messages.join('\n'));
    await page.screenshot({ path: '/tmp/demo_web_threaded_failure.png' });
    throw e;
  }

  expect(console_.hasUnexpectedErrors()).toBe(false);
});
