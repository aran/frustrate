// @ts-check
//
// Browser verification of the Tin Can web peer: the same unchanged
// lib/main.dart as macOS, with iroh running as a wasm module under dart2wasm.
//
// What is being proven here that no other platform proves:
//
//  1. `ring`'s C, compiled to WebAssembly, produces working crypto in a
//     browser — the QUIC/TLS 1.3 handshake with peerbot, end to end, carried
//     through the relay as opaque payload. That is ACCEPTANCE 3 below, and it
//     is the only assertion here that exercises ring. In particular the relay
//     leg does *not*: on `wasm_browser` iroh's relay client is
//     `ws_stream_wasm::WsStream` (iroh-relay/src/client/conn.rs), i.e. the
//     browser's own WebSocket, so whatever TLS that leg has is the browser's.
//  2. frustrate's `extern "C"` exports survive the wasm-bindgen post-pass, and
//     its import object plus the generated namespace instantiate one module.
//  3. A relay-only peer — no UDP, no hole punching, ever — is a *functioning*
//     iroh peer.
//
// The bundle's index.html carries a strict CSP meta (no 'unsafe-inline'), so
// this spec doubles as the CSP proof for a bridge whose module needs a second
// import namespace: it is fetched with a dynamic import() from 'self', not
// injected.
//
// # Nothing here contacts a third party
//
// The bundle is //:app_web_hermetic — `presets::Minimal`, so no pkarr, no DNS,
// no n0 relay — and the relay both peers meet through is //relay:relay_dev,
// started by this harness on an ephemeral loopback port. The peer on the other
// end is //peerbot:peerbot on `--preset minimal --relay <ours>`.
//
// A relay is still mandatory, and that has not changed: a browser cannot send
// UDP, so a web peer reaches the world only through one and cannot be dialled
// directly by anything. What changed is whose it is. The native driver
// (//tools:two_peers) needs no relay at all for the same reason in reverse —
// two peers on one host dial each other's loopback addresses directly.
//
// See `startRelay` for why both peers address the relay through this harness's
// own origin instead of its real port. It is the CSP, and it is not incidental.
const { test, expect } = require('@playwright/test');
const path = require('path');
const fs = require('fs');
const { spawn } = require('child_process');
const { createServer, startRelay } = require('./relay_harness');
const { expectStrictCspEnforced } = require('../../_playwright/csp_helpers');

const WEB_DIR = path.resolve(__dirname, '../bazel-bin/app_web_hermetic_web');
const PEERBOT = path.resolve(__dirname, '../bazel-bin/peerbot/peerbot');
const RELAY_DEV = path.resolve(__dirname, '../bazel-bin/relay/relay_dev');

// Must equal //:app_web_hermetic's RELAY_URL in BUILD.bazel, because that value
// is compiled into the bundle. Fixed rather than ephemeral for exactly that
// reason: the app is built before this file runs, so the port cannot be chosen
// here and communicated backwards. A mismatch fails as an unreachable relay.
const ORIGIN_PORT = 3341;
const ORIGIN = `http://127.0.0.1:${ORIGIN_PORT}`;

// Deliberately NO COOP/COEP (`isolate` left off below). This demo is
// single-threaded wasm and needs no SharedArrayBuffer, and cross-origin
// isolation would constrain the requests iroh makes to the relay. The threaded
// bundle is the opposite case — see threads_app.spec.js.
//
// The server and the relay live in ./relay_harness.js, shared with that spec.

// peerbot shuts down on stdin EOF, so the handle stays open for the run.
//
// `--preset minimal --relay ORIGIN` is the fully self-hosted peer: one relay,
// ours, and no n0 service of any kind. It used to be `--preset n0`, because
// n0's relays were the only meeting point available — a browser cannot be
// dialled directly by anything, so the bot had to be reachable through a relay
// too. That is still true; the relay is just ours now.
function startPeerbot() {
  const proc = spawn(
      PEERBOT,
      ['--preset', 'minimal', '--relay', ORIGIN,
       '--nickname', 'bot', '--prefix', 'echo: '],
      { stdio: ['pipe', 'pipe', 'pipe'] });
  const lines = [];
  let buf = '';
  proc.stdout.on('data', (d) => {
    buf += d.toString();
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      lines.push(buf.slice(0, i));
      buf = buf.slice(i + 1);
    }
  });
  const waitFor = (re, ms) => new Promise((resolve, reject) => {
    const t0 = Date.now();
    const tick = setInterval(() => {
      const hit = lines.find((l) => re.test(l));
      if (hit) { clearInterval(tick); resolve(hit); }
      else if (Date.now() - t0 > ms) {
        clearInterval(tick);
        reject(new Error(`peerbot never printed ${re}; saw:\n${lines.join('\n')}`));
      }
    }, 100);
  });
  return { proc, lines, waitFor };
}

let server, serverUrl, bot, relay;

// Everything this harness spawns or serves, checked before anything starts.
//
// `bazel-bin` is a convenience symlink into the *last* configuration built, so
// a `bazel build //:app` (macOS) between building these and running this file
// silently repoints it and the artifacts vanish. Without this check that
// surfaces as a bare ENOENT from a child process, or worse as a 404 for the
// bundle; with it, it says what to build.
function preflight() {
  const missing = [
    [WEB_DIR, '//:app_web_hermetic'],
    [PEERBOT, '//peerbot:peerbot'],
    [RELAY_DEV, '//relay:relay_dev'],
  ].filter(([p]) => !fs.existsSync(p));
  if (missing.length) {
    throw new Error(
        'missing build outputs:\n' +
        missing.map(([p, t]) => `  ${t} -> ${p}`).join('\n') +
        '\n\nRun, from e2e/iroh_demo:\n' +
        '  bazel build //:app_web_hermetic //peerbot:peerbot //relay:relay_dev\n' +
        '(bazel-bin points at the last configuration built, so building a ' +
        'native target in between repoints it.)');
  }
}

test.beforeAll(async () => {
  preflight();
  // Order matters: the relay must be listening and its port known before the
  // origin server can forward to it, and peerbot dials the origin as soon as it
  // starts.
  relay = startRelay(RELAY_DEV);
  const relayPort = await relay.port;
  ({ server, url: serverUrl } =
      await createServer({ root: WEB_DIR, port: ORIGIN_PORT, relayPort }));
  console.log(`RELAY: 127.0.0.1:${relayPort}, served to both peers at ${ORIGIN}/relay`);
  bot = startPeerbot();
});

test.afterAll(async () => {
  if (bot) { bot.proc.stdin.end(); bot.proc.kill(); }
  if (server) server.close();
  if (relay) { relay.proc.stdin.end(); relay.proc.kill(); }
});

// Flutter renders into a canvas, so assertions go through the accessibility
// tree, which only exists once the engine's placeholder is activated.
async function enableAccessibility(page) {
  const placeholder = page.locator('flt-semantics-placeholder');
  await placeholder.evaluate((el) => el.click());
}

// The premise every CSP claim in this spec rests on: the served bundle carries
// the strict policy and the browser enforces it. Its own test, because the
// probe trips a deliberate violation.
test('the served bundle carries a strict CSP, and it is enforced', async ({
  page,
}) => {
  await expectStrictCspEnforced(page, serverUrl, expect);
});

test('a browser peer binds, gets a ticket, and exchanges a message with peerbot', async ({ page }) => {
  const consoleLines = [];
  page.on('console', (m) => consoleLines.push(`[${m.type()}] ${m.text()}`));
  page.on('pageerror', (e) => consoleLines.push(`[pageerror] ${e.message}`));

  // Everything this run knows, printed on any failure. Without it a red run
  // reports only which locator timed out, and the reason — a CSP violation, a
  // refused relay, a Rust panic — sits in a console nobody ever sees. That cost
  // real time on the first hermetic run.
  try {
    await run(page, consoleLines);
  } catch (err) {
    console.log('--- peerbot said ---\n' + bot.lines.join('\n'));
    console.log('--- browser console ---\n' + consoleLines.join('\n'));
    console.log('--- relay said ---\n' + relay.lines.join(''));
    try {
      console.log('--- accessibility tree ---\n' +
          await page.locator('flt-semantics-host').ariaSnapshot());
    } catch (_) { /* the page may be gone; the lines above are the point */ }
    throw err;
  }
});

async function run(page, consoleLines) {
  const botTicket = (await bot.waitFor(/^peerbot: ticket /, 60000))
      .replace('peerbot: ticket ', '').trim();
  await bot.waitFor(/^peerbot: ready/, 60000);

  await page.goto(serverUrl);
  await page.waitForSelector('flt-semantics-placeholder', { timeout: 60000 });
  await enableAccessibility(page);

  // ---- ACCEPTANCE 1 + 2: the bridge initialised and the endpoint bound. ----
  // `Listening` is the first event the Rust side can possibly emit, and the
  // ticket it carries is a real EndpointTicket over a real EndpointId. If any
  // part of the merge were wrong — a missing import, the wasm-bindgen
  // handshake out of order, ring's crypto miscompiled — nothing would appear.
  const body = page.locator('flt-semantics-host');

  // A relay-only peer is still online: `PeerEvent::Online` fires only after the
  // relay handshake completes, so the wasm module has really reached a relay
  // and been accepted by it.
  //
  // This comment used to claim the event proved "a TLS 1.3 exchange performed
  // by ring's C compiled to WebAssembly". It does not, and did not before this
  // spec moved to a local relay: on `wasm_browser` the relay client is
  // `ws_stream_wasm::WsStream`, so the socket — and any TLS on it — belongs to
  // the browser, not to ring. ring's exercise is ACCEPTANCE 3.
  await expect(body).toContainText(/online/i, { timeout: 45000 });

  // The ticket is rendered as selectable text; it reaches the accessibility
  // tree through the identity field's label rather than as its own node.
  const snapshot = await body.ariaSnapshot();
  const ticket = snapshot.match(/endpointa[a-z0-9]+/)[0];
  console.log('BROWSER TICKET: ' + ticket);
  expect(ticket.length).toBeGreaterThan(100);
  expect(ticket).not.toEqual(botTicket);

  // ---- ACCEPTANCE 3: a message crosses. ----
  // Textboxes in document order: nickname, peer ticket, message.
  await page.getByRole('textbox').nth(1).fill(botTicket);
  await page.getByRole('button', { name: 'Connect' }).click();
  // The bot's nickname arrives as the first frame of the stream, so the peer
  // row carrying it means the ALPN negotiated, the bidirectional stream
  // opened, and a length-prefixed frame was decoded. The same row carries the
  // path badge, and on web it reads `relay` permanently — a browser has no
  // path to hole-punch onto.
  const peerRow = page.getByRole('button', { name: /bot \(.*connected relay/ });
  await expect(peerRow).toBeVisible({ timeout: 45000 });
  console.log('PEER ROW: ' + await peerRow.getAttribute('aria-label'));

  // Click Send rather than pressing Enter.
  //
  // This used to be `keyboard.press('Enter')` against `onSubmitted`, on the
  // grounds that the Send button might not reach the semantics tree. It does —
  // the accessibility tree carries `button "Send"` — and the keystroke was the
  // less reliable half: a synthesised Enter has to land on whatever Flutter's
  // canvas renderer currently considers focused, and when it does not, the
  // message is never sent and *nothing reports it*. `send()` is fire-and-forget
  // by design, so a keystroke that goes nowhere is indistinguishable from a
  // peer that never answered — which is precisely the shape this spec used to
  // fail with about half the time, and which was read as relay flakiness.
  // Getting text into a Flutter canvas text field is the flakiest step in this
  // file, and it is worth saying exactly why, because it was misdiagnosed for a
  // long time as relay flakiness.
  //
  // Flutter keeps the real text in a Dart `TextEditingController` and attaches
  // its DOM editing connection *asynchronously* after the field is tapped.
  // Input that arrives before that lands is dropped, and the two ways of
  // getting it wrong fail differently:
  //
  //   - `fill()` sets the input's value directly, which Flutter may never
  //     adopt. The controller stays empty.
  //   - `keyboard.type()` too early loses its leading characters — observed
  //     here as "llo from a browser".
  //
  // Neither is reported. `_send` reads the controller, and on an empty string
  // it *returns without error*: no message, no log line, no failure. On a
  // truncated one it cheerfully sends the wrong text. Both are then
  // indistinguishable from a peer that never answered — which is precisely the
  // shape this spec used to fail with about half the time.
  //
  // Reading the field back is **not** enough, and that is the subtle part:
  // `inputValue()` reads the DOM input, which holds every keystroke, while
  // Flutter's controller holds only what its editing connection adopted. A run
  // that verified the DOM and then sent produced `me: wser` from a typed
  // "hello from a browser" — the check passed and the wrong text went out.
  //
  // `insertText` avoids per-character loss by dispatching the whole string as
  // one input event, so the failure mode becomes all-or-nothing rather than
  // truncation. The assertion that actually pins it is after the send: the app
  // echoes what it queued into its own message log, and that line comes from
  // the Dart controller, so it is the only view of the text that cannot lie.
  // So the loop retries until the app's *own* message log shows something was
  // queued, and only that log is trusted. `_send` reads the controller and
  // returns silently on an empty string, so a lost fill leaves the log
  // untouched and the retry is free — nothing was sent, nothing is duplicated.
  //
  // The message field is the one most likely to need a retry because it is
  // mounted only once a peer is selected, so it is the youngest widget on the
  // page at the moment it is typed into.
  // So the loop retries until the app's own message log shows the **exact**
  // line, and only that log is trusted. It has to be the exact line, not a
  // `me: ` prefix: the activity log renders `nickname: bot`, which contains
  // "me: ", so a laxer marker matches before anything has been sent at all.
  //
  // Retrying is cheap and safe. A lost fill means `_send` saw an empty string
  // and returned without queuing, so nothing was duplicated; a truncated one
  // may put a short message on the wire, which peerbot echoes and no assertion
  // depends on. The message field needs this more than the others because it is
  // mounted only once a peer is selected, so it is the youngest widget on the
  // page at the moment it is typed into.
  const MESSAGE = 'hello from a browser';
  const message = page.getByRole('textbox').nth(2);
  await expect(async () => {
    await message.click();
    // Let the editing connection attach before typing. Without this the first
    // keystroke is swallowed every time — the observed queue was
    // `me: ello from a browser`, one character short, run after run — because
    // the tap and the attach are asynchronous with respect to each other and
    // synthesised input arrives immediately.
    await page.waitForTimeout(500);
    // Select-all + Delete rather than `fill('')`: both the clear and the type
    // have to go through the same keyboard path Flutter is listening on, or a
    // previous attempt's partial text survives in the controller and the next
    // one appends to it.
    await page.keyboard.press('Meta+a');
    await page.keyboard.press('Delete');
    await page.keyboard.type(MESSAGE, { delay: 25 });
    await page.getByRole('button', { name: 'Send' }).click();
    await expect(body).toContainText(`me: ${MESSAGE}`, { timeout: 2000 });
  }).toPass({ timeout: 45000 });

  // Two independent witnesses, which is the point: the app's own log, and
  // peerbot's stdout, which knows nothing about the browser.
  await expect(body).toContainText("echo: hello from a browser", { timeout: 45000 });
  await bot.waitFor(/hello from a browser/, 30000);

  console.log('PEERBOT SAID:\n' + bot.lines.join('\n'));
  const violations = consoleLines.filter((l) => /Content Security Policy/i.test(l));
  expect(violations, 'no CSP violations').toEqual([]);
}
