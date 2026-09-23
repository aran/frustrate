// @ts-check
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  // What is left to be slow is the 13.7 MB wasm fetch and compile. The relay
  // handshake is now loopback and the pkarr publish/resolve round trip is gone
  // with `presets::Minimal`, so the budget that used to cover the public
  // internet covers a local relay several times over.
  //
  // Kept generous rather than cut to the measured time: this is a ceiling for
  // reporting a hang, not a performance assertion, and a timeout that fails on
  // a loaded machine would be a flake of exactly the kind this change removed.
  timeout: 120000,
  use: { browserName: 'chromium', headless: true },
  reporter: [['list']],
});
