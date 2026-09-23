// @ts-check
//
// The same Tin Can web peer as web.spec.js — same lib/main.dart, same bridge
// crate — with its wasm module built for threaded wasm and post-passed by
// wasm-bindgen.
//
// # What this proves, and what it cannot
//
// Every `#[bridge]` member in //bridge:src/api.rs belongs to the `Node` actor,
// so this app dispatches through the *actor* pump: a Worker with its own
// instance and its own private shared memory, thread 0 of it. So what a green
// run here says is
//
//   * a post-passed +atomics module instantiates under the real Dart runtime,
//     on the main thread and in an actor Worker, with frustrate's memory
//     supplied under the sidecar's namespace rather than the sidecar's own;
//   * wasm-bindgen's `__wbindgen_start` bootstrap runs in place of the deleted
//     `__wasm_init_tls`, in both realms;
//   * a full bridge round trip works through it — `Node.open` is a bridged
//     actor method whose Rust reaches the browser's WebSocket through a hundred
//     and fifty wasm-bindgen shims, and the ticket it returns is real;
//   * the page is cross-origin isolated from SERVED HEADERS, with no browser
//     flag — which is what makes the module's `SharedArrayBuffer` legal.
//
//   * and the POOL, which everything above does not touch: `Node` is an actor,
//     so the assertions above ride the actor pump — a Worker with its own
//     memory, thread 0 of it. `peerIdFor` is the one member of this bridge
//     that is not an actor method, so its body goes to `pool::spawn_call` and
//     calling it makes the page spawn a pool worker against the page's OWN
//     shared memory. That is the path wasm-bindgen's thread transform
//     rewrote — the glue has to skip the `__wasm_init_tls` bootstrap the
//     transform deleted and let `__wbindgen_start` do it instead — and this
//     is the only place in the repo where those lines execute.
//
// threads_mechanism.spec.js covers the same transform contract one layer down,
// driving the raw artifact rather than frustrate's glue, and says why it has to
// exist.
//
// Scoped deliberately to boot + online + ticket. Two-peer pairing proves iroh,
// which web.spec.js already owns end to end on the single-threaded build; the
// module shape is what differs here, and it has all differed by the time a
// ticket renders.
//
// Nothing here contacts a third party: `PRESET=minimal` plus //relay:relay_dev
// on loopback, reached through this harness's own origin.
const { test, expect } = require('@playwright/test');
const path = require('path');
const fs = require('fs');
const { createServer, startRelay } = require('./relay_harness');
const { expectStrictCspEnforced } = require('../../_playwright/csp_helpers');

const WEB_DIR = path.resolve(__dirname, '../bazel-bin/threaded/app_web_threaded_web');
const RELAY_DEV = path.resolve(__dirname, '../bazel-bin/relay/relay_dev');

// Must equal //threaded:app_web_threaded's RELAY_URL, which is compiled into
// the bundle — so it cannot be chosen here. Different from web.spec.js's port
// because playwright runs spec files in parallel workers.
const ORIGIN_PORT = 3342;

let server, serverUrl, relay;

test.beforeAll(async () => {
  // bazel-bin points at the last configuration built, so a native build in
  // between silently repoints it and the artifacts vanish. Say what to build.
  const missing = [
    [WEB_DIR, '//threaded:app_web_threaded'],
    [RELAY_DEV, '//relay:relay_dev'],
  ].filter(([p]) => !fs.existsSync(p));
  if (missing.length) {
    throw new Error(
        'missing build outputs:\n' +
        missing.map(([p, t]) => `  ${t} -> ${p}`).join('\n') +
        '\n\nRun, from e2e/iroh_demo:\n' +
        '  bazel build //threaded:app_web_threaded //relay:relay_dev\n' +
        '(//threaded is tagged manual: threaded wasm needs the locally built ' +
        '+atomics std — `dart toolchain/custom_std/tool/build.dart` in the ' +
        'frustrate repo.)');
  }
  relay = startRelay(RELAY_DEV);
  const relayPort = await relay.port;
  ({ server, url: serverUrl } = await createServer(
      { root: WEB_DIR, port: ORIGIN_PORT, relayPort, isolate: true }));
});

test.afterAll(() => {
  if (server) server.close();
  if (relay) { relay.proc.stdin.end(); relay.proc.kill(); }
});

// The premise every CSP claim in this spec rests on: the served bundle carries
// the strict policy and the browser enforces it. Its own test, because the
// probe trips a deliberate violation.
test('the served bundle carries a strict CSP, and it is enforced', async ({
  page,
}) => {
  await expectStrictCspEnforced(page, serverUrl, expect);
});

test('the threaded, post-passed peer boots isolated and gets a ticket',
    async ({ page }) => {
  const lines = [];
  page.on('console', (m) => lines.push(`[${m.type()}] ${m.text()}`));
  page.on('pageerror', (e) => lines.push(`[pageerror] ${e.message}`));

  try {
    await page.goto(serverUrl);

    // The headers, not a browser flag — asserted on the header rather than on
    // the app working, because on desktop Chrome those are not the same claim.
    // Measured: with the headers off this app still comes online, because
    // Chrome's carve-out still lets a non-isolated page build a shared
    // `WebAssembly.Memory` even though it blocks the `SharedArrayBuffer`
    // constructor and blocks postMessaging one. An actor never crosses that
    // line (it creates its memory inside its own Worker), so an actor-only app
    // gets away with it; the pool cannot, since handing a worker the page's
    // memory IS that postMessage. Serving the headers is the product
    // requirement (CHARTER.md), so the header is what this checks.
    expect(await page.evaluate(() => crossOriginIsolated)).toBe(true);

    await page.waitForSelector('flt-semantics-placeholder', { timeout: 60000 });
    // Flutter renders into a canvas, so assertions go through the
    // accessibility tree, which only exists once the placeholder is activated.
    await page.locator('flt-semantics-placeholder').evaluate((el) => el.click());
    const body = page.locator('flt-semantics-host');

    // `PeerEvent::Online` fires only after the relay handshake completes, so
    // this is a live endpoint inside a post-passed threaded module — the whole
    // stack, not just a module that loaded.
    await expect(body).toContainText(/online/i, { timeout: 60000 });

    const snapshot = await body.ariaSnapshot();
    const ticket = snapshot.match(/endpointa[a-z0-9]+/)[0];
    expect(ticket.length).toBeGreaterThan(100);

    // The pool, and the reason this spec is not scoped to boot alone.
    //
    // A malformed ticket rather than a real one: the answer comes back the
    // same way either way, and this needs no second peer. `connect()` hands it
    // to `peerIdFor` before dialing, so a visible rejection means the job was
    // pushed, a pool worker bootstrapped itself through `__wbindgen_start`,
    // ran the body, and answered. A worker that failed to bootstrap would
    // leave the job on the queue with nothing to run it, and this would time
    // out instead of failing fast.
    // Typed rather than `fill()`ed, and retried: Flutter attaches its DOM
    // editing connection asynchronously after the field is tapped, so a
    // synthesised value can be dropped without a trace — web.spec.js carries
    // the same dance for the message box and says why. A lost fill leaves
    // `connect()` with an empty string, which it returns on silently, so the
    // symptom is an assertion that never comes true rather than an error.
    await expect(async () => {
      await page.mouse.move(400, 400);
      await page.mouse.wheel(0, -3000);
      const field = page.getByRole('textbox').nth(1);
      await field.click();
      await page.waitForTimeout(500);
      await page.keyboard.press('Meta+a');
      await page.keyboard.press('Delete');
      await page.keyboard.type('not-a-ticket', { delay: 25 });
      await page.getByRole('button', { name: 'Connect' }).click();
      // The diagnostics panel is below the fold, and Flutter puts only what is
      // on screen into the semantics tree.
      await page.mouse.wheel(0, 2000);
      // On the message the RUST body produces, not the wrapper `connect()`
      // puts around it: its `catch (e)` renders "that is not a ticket: …" for
      // *any* failure of the call, a pool that never answered included. Only a
      // real round trip into `parse_ticket` yields this string. Verified by
      // forcing the glue to take the bootstrap path the transform deleted —
      // the wrapper text still appears, this does not.
      await expect(body).toContainText(/not a ticket and not an endpoint id/i,
          { timeout: 4000 });
    }).toPass({ timeout: 90000 });
  } catch (err) {
    console.log('--- browser console ---\n' + lines.join('\n'));
    console.log('--- relay said ---\n' + relay.lines.join(''));
    try {
      console.log('--- accessibility tree ---\n' +
          await page.locator('flt-semantics-host').ariaSnapshot());
    } catch (_) { /* the page may be gone; the lines above are the point */ }
    throw err;
  }
});
