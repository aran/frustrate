// @ts-check
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  // The gallery is a long scrolling list; each assertion drives an aria
  // snapshot of the whole semantics tree, so a full sweep of every card's
  // result runs comfortably under this bound (well above the raw work time).
  timeout: 180000,
  use: {
    browserName: 'chromium',
    headless: true,
  },
});
