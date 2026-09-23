// @ts-check
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  // One screen, no scrolling, no network. The generous bound is for the
  // dart2wasm cold start, not for the assertions.
  timeout: 60000,
  use: {
    browserName: 'chromium',
    headless: true,
  },
});
