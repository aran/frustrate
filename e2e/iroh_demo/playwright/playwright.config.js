// @ts-check
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  // What is slow is the wasm fetch and compile. The relay handshake is
  // loopback and `presets::Minimal` has no pkarr round trip, so the budget
  // covers a local relay several times over.
  //
  // Kept generous rather than cut to the measured time: this is a ceiling for
  // reporting a hang, not a performance assertion, and a timeout that fails on
  // a loaded machine would be a flake.
  timeout: 120000,
  use: { browserName: 'chromium', headless: true },
  reporter: [['list']],
});
