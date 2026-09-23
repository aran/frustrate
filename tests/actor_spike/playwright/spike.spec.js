// @ts-check
//
// Drives the actor-channel benchmark in real Chromium (COOP/COEP served, so
// SharedArrayBuffer is live) and records the numbers the Actor design
// depends on. Results land in /tmp/actor_spike_results.json.
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

test('SAB ring vs postMessage benchmark', async ({ page }) => {
  let resultsJson = null;
  let benchError = null;
  page.on('console', (msg) => {
    const t = msg.text();
    if (t.startsWith('RESULTS ')) resultsJson = t.slice('RESULTS '.length);
    if (t.startsWith('BENCH_ERROR ')) benchError = t;
  });

  await page.goto(serverUrl, { waitUntil: 'networkidle' });
  await page.waitForFunction(
    () =>
      document.getElementById('out').textContent !== 'running…',
    { timeout: 240000 },
  );

  if (benchError) throw new Error(benchError);
  expect(resultsJson).not.toBeNull();
  const results = JSON.parse(resultsJson);
  fs.writeFileSync(
    '/tmp/actor_spike_results.json',
    JSON.stringify(results, null, 2),
  );
  console.log(JSON.stringify(results, null, 2));
});
