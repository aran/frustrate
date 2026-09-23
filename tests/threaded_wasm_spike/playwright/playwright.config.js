// @ts-check
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: '.',
  timeout: 300000,
  use: {
    browserName: 'chromium',
    headless: true,
  },
});
