// @ts-check
//
// Browser verification of the frustrate web demo gallery: the same unchanged
// app code as macOS, with the Rust bridge running as a wasm module under
// dart2wasm. Served by e2e/_playwright's static server
// (sets the COOP/COEP headers skwasm and the bridge both assume).
//
// The bundle's index.html carries a strict CSP meta (no 'unsafe-inline';
// injected at build time by web_csp.bzl so the source page stays servable by
// `flutter run -d chrome`), so this spec doubles as
// the standing CSP proof, in two halves. Positive: the first test asserts the
// policy arrived and the browser enforces it. Negative: the bridge init and
// every feature below run without inline scripts, and hasUnexpectedErrors()
// fails the run on any CSP violation. Workers are counted below — the actor
// race must spawn its workers from the served frustrate.js; the only blob:
// worker allowed is the Flutter engine's skwasm render worker (a flutter.js
// constraint, not ours).
const { test, expect } = require('@playwright/test');
const path = require('path');
const {
  createFlutterServer,
  waitForFlutterReady,
  collectConsoleMessages,
} = require('../../_playwright/flutter_helpers');
const { expectStrictCspEnforced } = require('../../_playwright/csp_helpers');

const WEB_DIR = path.resolve(__dirname, '../bazel-bin/app_web_web');
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

// Flutter renders into a canvas, so assertions go through the accessibility
// tree (enabled via the placeholder button Flutter injects).
async function enableAccessibility(page) {
  const placeholder = page.locator('flt-semantics-placeholder');
  await placeholder.evaluate((el) => el.click());
}

// The gallery is a long scrolling list; lazy list children only enter the
// semantics tree once near the viewport. Wheel down over the canvas to bring
// lower sections into view before asserting on them.
async function scrollDown(page, dy = 400) {
  const view = page.locator('flutter-view');
  await view.hover();
  await page.mouse.wheel(0, dy);
  await page.waitForTimeout(300);
}

// Scroll until an aria text regex appears (bounded), then return. Flutter
// nests result text inside card "group" names, so we match against the raw
// ariaSnapshot string rather than a structured `- text:` node.
async function scrollUntil(page, re, maxSteps = 12) {
  const view = page.locator('flutter-view');
  for (let i = 0; i < maxSteps; i++) {
    const snap = await view.ariaSnapshot();
    if (re.test(snap)) return;
    await scrollDown(page);
  }
  const snap = await view.ariaSnapshot();
  expect(snap).toMatch(re); // fail with the final snapshot for diagnostics
}

// Tap a button by its accessible name. Flutter paints to a canvas, so a
// semantics node below the fold can't be auto-scrolled into view by
// Playwright — a plain .click() would then auto-wait until the whole test
// times out. Instead scroll the list until the button is actionable, retrying
// the click with a short per-attempt bound so an off-viewport node just nudges
// and retries rather than hanging.
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
  await btn.click({ timeout: 5000 }); // final attempt surfaces the real error
}

// Poll the aria snapshot (no scrolling) until a regex matches — for results
// that render in place. Flutter web throttles frames (and semantics updates)
// when idle, so nudge the mouse each poll to force a repaint; otherwise a
// setState-driven change stays invisible to the accessibility tree.
async function expectText(page, re, timeout = 30000) {
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
    await page.waitForTimeout(200);
  }
}

// Count blob: workers from before any page script runs. The strict-CSP
// claim is that frustrate never needs one: the bridge's workers must come
// from the served frustrate.js (asserted after the actor race).
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

// The premise every CSP assertion below rests on. Its own test so the
// deliberate violation it provokes stays out of the gallery run's console.
test('the served bundle carries a strict CSP, and it is enforced', async ({
  page,
}) => {
  await expectStrictCspEnforced(page, serverUrl, expect);
});

test('gallery: bridge init, every feature family, CSP + real workers', async ({
  page,
}) => {
  const console_ = collectConsoleMessages(page);
  const blobWorkers = await countBlobWorkers(page);
  await page.goto(serverUrl, { waitUntil: 'networkidle' });

  try {
    await waitForFlutterReady(page);
    await enableAccessibility(page);

    // The served glue ran and registered itself (the strict-CSP delivery).
    expect(
      await page.evaluate(() => globalThis.$frustrateGlueUrl),
    ).toMatch(/frustrate\.js$/);
    const engineBlobWorkers = await blobWorkers();
    expect(engineBlobWorkers).toBeLessThanOrEqual(1);

    const view = page.locator('flutter-view');

    // --- Basics: sync free fn, async free fn ---
    await expectText(page, /Hello, Flutter — from Rust/);
    await expectText(page, /The 10000th prime is 104729/);

    // --- Values & data types ---
    // Scalars: crab() mints the astral char U+1F980; u128 max arrives as BigInt.
    await scrollUntil(page, /U\+1F980/);
    await scrollUntil(page, /340282366920938463463374607431768211455/);
    // Typed list: the 26 a-z counts for "hello world" cross as an Int32List.
    await scrollUntil(page, /tallest bucket: l = 3/);
    // Ordered containers: BTreeMap/BTreeSet arrive sorted; VecDeque rotated.
    await scrollUntil(page, /1:1\s+3:2\s+5:3/);
    await scrollUntil(page, /\{1, 3, 5\}/);
    await scrollUntil(page, /5, 1, 3, 5, 5, 3/);
    // Records: a Rust tuple as a nested Dart record.
    await scrollUntil(page, /9 chars, 9 bytes/);
    // Data classes: value equality + copyWith, and the no_eq identity sibling.
    await scrollUntil(page, /Passport == local: true/);
    await scrollUntil(page, /Set keeps 1/);
    await scrollUntil(page, /holder = null/);
    await scrollUntil(page, /Tickets == : false/);
    await scrollUntil(page, /Set keeps 2/);

    // --- Concurrency: Frozen snapshot renders (sync count + async join) ---
    await scrollUntil(page, /frozen · shared · immutable/);

    // --- Locked: an async write updates the ledger balance ---
    await scrollUntil(page, /Ledger — Locked/);
    await clickButton(page, 'add 10');
    await scrollUntil(page, /balance 10/);

    // --- Actors: the same work fanned across four executors beats one.
    // Each actor is a Worker hosting its own wasm instance — genuine
    // parallelism in the browser, on stable Rust. ---
    await scrollUntil(page, /Race 4 actors against 1/);
    await clickButton(page, 'Race 4 actors against 1');
    await scrollUntil(page, /4 actors ran [\d.]+x faster than one/);
    const raceSnap = await view.ariaSnapshot();
    const speedup = parseFloat(raceSnap.match(/4 actors ran ([\d.]+)x/)[1]);
    expect(speedup).toBeGreaterThan(1.5);
    // The race spawned its actor workers from the served frustrate.js, not
    // blob: URLs.
    expect(await blobWorkers()).toBe(engineBlobWorkers);

    // --- Async fn: a real Rust `async fn` whose body `.await`s, surfaced as a
    // Dart Future. It runs on the cooperative executor even here on
    // single-threaded web (there is no pool). Firing 1000 at once is the
    // in-app multiplexing proof: all resolve on the one thread because each
    // suspended future is heap data, not a parked thread — a block_on-per-call
    // model would deadlock. ---
    await scrollUntil(page, /cooperativeYield\(21, rounds: 3\) = 42/);
    await clickButton(page, 'Fire 1000 concurrently');
    await expectText(page, /1000\/1000 resolved in \d+ms \(all correct\)/, 20000);

    // --- Streams & callbacks: a splice on a Confined doc pushes a patch to
    // the stored StreamSink and fires the stored Dart closure (the length). ---
    await scrollUntil(page, /length \(via callback\): 0/);
    await clickButton(page, 'append hi');
    // Poll in place (no scrolling) so the async callback + stream patch have
    // time to render without scrolling the live-doc pane out of view.
    await expectText(page, /length \(via callback\): 2/, 8000);
    // Quotes are backslash-escaped in the aria serialization, so match the
    // patch by its index rather than the quoted text.
    await expectText(page, /Splice @0/, 8000);

    // --- Streams & callbacks: the PORTABLE awaited callback. An async fn
    // awaits a value back from a Dart closure via call_async — it runs here on
    // single-threaded web, unlike the blocking transform_sum below. ---
    await scrollUntil(page, /transform\(9\) = 81/);

    // --- Streams & callbacks: transform_sum is a plain pool fn that blocks a
    // worker per item, so it is compile-time absent from the web surface — the
    // web build shows the capability-gate card, not a result. ---
    await scrollUntil(page, /compile-time absent from the web surface/);

    // --- Streams & callbacks: blockingRead opts onto the web surface with
    // web="runtime_fail" — present so this call compiles, but throwing a loud,
    // attributable UnsupportedError when actually invoked on web. ---
    await scrollUntil(page, /blockingRead — opt-in runtime-fail/);
    await clickButton(page, 'read now');
    await expectText(page, /blockingRead is not available on web/, 8000);

    // --- Traits: one Dart interface, several Rust impls behind newGreeter ---
    await scrollUntil(page, /Greeter — trait object/);
    await clickButton(page, 'pirate');
    await scrollUntil(page, /ahoy Flutter/);

    // --- External types: the protobuf-style FakePlan crosses as bytes ---
    await scrollUntil(page, /FakePlan — external/);
    await clickButton(page, 'bump revision in Rust');
    await scrollUntil(page, /roadmap — revision 2/);

    // --- Data enum: every TextPatch variant round-trips and switch-matches ---
    await scrollUntil(page, /Splice @2/);
    await scrollUntil(page, /Mark bold=1/);

    // --- The std facilities, absent: this is the STOCK web build, where
    // std links the unsupported PAL and every reading is the -1 sentinel.
    // The negative control for std_facilities.spec.js, which asserts the same
    // card reads "present" with real values on //custom_std. Without this
    // pair, a card that silently rendered sentinels everywhere would still
    // pass over there — the facility spec would be a gate that can only pass.
    await scrollUntil(page, /facilities: absent/);
    await scrollUntil(page, /std\.seed\s*=\s*-1/);
    await scrollUntil(page, /std\.cores\s*=\s*-1/);

    // --- Watching the bridge: the interception seam. Nothing is recorded until
    // the decorator is activated, and the first row is what says so — a tracer
    // that got its rows from anywhere but `aroundSync`/`aroundAsync` would
    // already have some, having sat through the whole sweep above. ---
    await scrollUntil(page, /Call trace/);
    await expectText(page, /trace: off/, 8000);
    await clickButton(page, 'start tracing');
    await expectText(page, /tracing on/, 8000);
    // The @actor row — the thing the card exists for — is deliberately not
    // asserted here. Every actor button in the gallery is *above* this card,
    // and clickButton only ever nudges downward (a Flutter semantics node
    // painted to canvas cannot be auto-scrolled into view), so a row that
    // reached for one would depend on how much of the page a given viewport
    // happens to hold.

    // --- Watching the bridge: Rust's `log` records arriving as a Dart stream.
    // The bridge crate calls `log::info!` while doing its ordinary work, so the
    // first two rows were produced by cards far above this one: `greet` logs
    // from _GreetingCard's build, `nth_prime` from wherever the async body ran
    // (here the calling thread — this is single-threaded web). The target is
    // the logging module path, which `log` fills in and no frustrate code
    // touches. ---
    await scrollUntil(page, /log::info!/);
    await expectText(page, /demo_rust::api: greeting Flutter/, 8000);
    await expectText(page, /demo_rust::api: nth_prime\(10000\)/, 8000);
    // A record produced on demand, which separates "the logger is installed and
    // nothing is logging" from "records are not arriving". Tracing is still on,
    // so the same press is a sync row in the card above.
    await clickButton(page, 'log a line from Rust');
    await expectText(page, /demo_rust::api: a line logged on request/, 8000);
    await expectText(page, /sync\s+emit_demo_log/, 8000);

    await page.screenshot({ path: '/tmp/demo_web.png', fullPage: false });
  } catch (e) {
    console.log('Console messages:', console_.messages.join('\n'));
    await page.screenshot({ path: '/tmp/demo_web_failure.png' });
    throw e;
  }

  expect(console_.hasUnexpectedErrors()).toBe(false);
});
