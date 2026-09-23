// @ts-check
//
// Threaded wasm spike in real Chromium (COOP/COEP served, so shared
// WebAssembly.Memory is live): a +atomics module built with nightly
// -Zbuild-std, hand-rolled worker glue (stack + TLS init, spawn_worker
// import), shared atomics + std Mutex correctness, parallel speedup.
const { test, expect } = require('@playwright/test');
const fs = require('fs');
const path = require('path');
const {
  createFlutterServer,
} = require('../../../../rules_flutter/e2e/_playwright/flutter_helpers');

const DIR = path.resolve(__dirname, '..');
let server;
let serverUrl;

test.beforeAll(async () => {
  const result = await createFlutterServer(DIR);
  server = result.server;
  serverUrl = result.url;
});

test.afterAll(async () => {
  if (server) server.close();
});

test('threaded wasm: shared memory, Mutex, parallel speedup', async ({
  page,
}) => {
  let resultsJson = null;
  let spikeError = null;
  page.on('console', (msg) => {
    const t = msg.text();
    if (t.startsWith('RESULTS ')) resultsJson = t.slice('RESULTS '.length);
    if (t.startsWith('SPIKE_ERROR ')) spikeError = t;
  });

  await page.goto(serverUrl, { waitUntil: 'networkidle' });
  await page.waitForFunction(
    () => document.getElementById('out').textContent !== 'running…',
    { timeout: 120000 },
  );

  if (spikeError) throw new Error(spikeError);
  expect(resultsJson).not.toBeNull();
  const results = JSON.parse(resultsJson);
  console.log(JSON.stringify(results, null, 2));
  fs.writeFileSync(
    '/tmp/threaded_wasm_spike_results.json',
    JSON.stringify(results, null, 2),
  );

  expect(results.instantiated).toBe(true);
  // Exact: every atomic increment from every thread landed.
  expect(results.counter).toBe(400000);
  // Exact: the futex-backed std Mutex serialized correctly across threads.
  expect(results.lockedSum).toBe(400000);
  // Threads genuinely run in parallel (cold spawn included).
  expect(results.speedup).toBeGreaterThan(1.5);
});
