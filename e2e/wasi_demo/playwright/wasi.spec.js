// @ts-check
//
// Browser verification that a wasm32-wasip1 bridge module actually works —
// that frustrate's own JS runtime answers every preview1 import the module
// declares, and answers them *correctly*.
//
// This is the half a build_test cannot reach. `//:wasm_module_build_test`
// proves the crate compiles for wasip1 and a module comes out;
// `@frustrate//tests/bazel_rules:wasm_imports_test` proves the module imports
// nothing the host lacks. Neither runs a single instruction. Only a browser can
// tell you the clock is a clock and the CSPRNG is random.
//
// The entropy assertion is the one that has to exist. Every other failure on
// this platform is loud — a missing import throws (the host is a Proxy whose
// unimplemented members throw rather than returning an errno), a missing clock
// aborts, a wrong-unit clock shows as a skew of years. A `random_get` that
// returns success without filling the buffer is silent: measured during this
// platform's bring-up, a stubbed CSPRNG returned zero five times out of five
// with no error reported anywhere. Nothing but sampling catches it.
//
// Run:  bazel build //:app_web && npx playwright test
const { test, expect } = require('@playwright/test');
const path = require('path');
const {
  createFlutterServer,
  waitForFlutterReady,
  collectConsoleMessages,
} = require('../../_playwright/flutter_helpers');

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
  await page.locator('flt-semantics-placeholder').evaluate((el) => el.click());
}

// Poll the aria snapshot until a regex matches. Flutter web throttles frames
// (and semantics updates) when idle, so nudge the mouse each poll to force a
// repaint; otherwise a setState-driven change stays invisible.
async function expectText(page, re, timeout = 30000) {
  const view = page.locator('flutter-view');
  const deadline = Date.now() + timeout;
  let toggle = 0;
  for (;;) {
    const snap = await view.ariaSnapshot();
    if (re.test(snap)) return snap;
    if (Date.now() > deadline) {
      expect(snap).toMatch(re);
      return snap;
    }
    await page.mouse.move(5 + (toggle ^= 1), 5);
    await page.waitForTimeout(200);
  }
}

async function clickButton(page, name, maxSteps = 12) {
  const view = page.locator('flutter-view');
  const btn = page.getByRole('button', { name });
  for (let i = 0; i < maxSteps; i++) {
    if ((await view.ariaSnapshot()).includes(name)) {
      try {
        await btn.click({ timeout: 1500 });
        return;
      } catch (_) {
        // Present but not yet actionable: nudge.
      }
    }
    await view.hover();
    await page.mouse.wheel(0, 300);
    await page.waitForTimeout(300);
  }
  await btn.click({ timeout: 5000 });
}

test('a wasip1 bridge gets a working clock, entropy, environment and stdout',
    async ({ page }) => {
  const console_ = collectConsoleMessages(page);
  await page.goto(serverUrl, { waitUntil: 'networkidle' });
  await waitForFlutterReady(page);
  await enableAccessibility(page);

  // ---------------------------------------------------------------- init --
  // Reaching a rendered check at all proves a good deal: the module
  // instantiated with frustrate's import object, and `EventLog.new_()` ran
  // `Instant::now()` in initState without aborting.
  const snap = await expectText(page, /WASI-ENTROPY PASS/);

  // -------------------------------------------------------- random_get --
  // Assert the *values*, not just PASS, so a future change that widened the
  // health band cannot make this vacuous.
  expect(snap).toMatch(/WASI-ENTROPY PASS healthy=true distinct=64\/64 nil=false/);
  const bits = snap.match(/bits=(\d+)\/(\d+)/);
  expect(bits, 'entropy bit counts missing from the report').not.toBeNull();
  const [setBits, totalBits] = [Number(bits[1]), Number(bits[2])];
  expect(totalBits).toBe(64 * 128);
  // The catastrophic case scores 4/128 per id (version + variant bits only).
  // A real CSPRNG lands near half. This bound is far from both.
  expect(setBits / totalBits).toBeGreaterThan(0.4);
  expect(setBits / totalBits).toBeLessThan(0.6);

  // ------------------------------------------- clock_time_get(REALTIME) --
  // Rust's SystemTime against the browser's own clock. A host returning a
  // constant, or seconds where nanoseconds were wanted, misses by years.
  await expectText(page, /WASI-CLOCK PASS skew_ms=\d+/);
  const skew = Number((await page.locator('flutter-view').ariaSnapshot())
      .match(/WASI-CLOCK PASS skew_ms=(\d+)/)[1]);
  expect(skew).toBeLessThan(5000);

  // ------------------------------------------ clock_time_get(MONOTONIC) --
  await expectText(page, /WASI-MONOTONIC PASS increasing=true/);

  // ---------------------------------- environ_sizes_get + environ_get --
  // Zero is the right answer (frustrate reports an empty environment). It
  // matters that it was *answered*: an unimplemented import would have thrown.
  await expectText(page, /WASI-ENV PASS count=0/);

  // ------------------------------------------------------------ fd_write --
  // The Rust `println!` in EventLog::append. This is the only way a wasm
  // module's stdout is observable, and on wasm32-unknown-unknown it is
  // silently dropped — so a console line here is the whole assertion.
  expect(console_.messages.some((m) => /\[event-log\] [0-9a-f-]{36} clock-probe/.test(m)),
      `no [event-log] line on the console; fd_write is not reaching it.\n` +
      console_.messages.slice(0, 20).join('\n')).toBe(true);

  // ------------------------------------------- entropy again, later in time --
  // A source can be healthy inside one tight loop and stuck between calls.
  // Appending draws fresh UUIDs at a different moment; WASI-IDS counts them.
  await clickButton(page, 'Append entry');
  await clickButton(page, 'Append entry');
  await expectText(page, /WASI-IDS PASS unique=3\/3/);

  // Elapsed time advanced across those appends — the monotonic clock is
  // running, not merely present.
  const after = await page.locator('flutter-view').ariaSnapshot();
  const elapsed = [...after.matchAll(/elapsed_us=(\d+)/g)].map((m) => Number(m[1]));
  expect(elapsed.length).toBeGreaterThanOrEqual(3);
  expect(elapsed[elapsed.length - 1]).toBeGreaterThan(elapsed[0]);

  expect(console_.hasUnexpectedErrors(),
      `unexpected console errors:\n${console_.messages.join('\n')}`).toBe(false);
});
