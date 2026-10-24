import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: '.',
  testMatch: 'journey.spec.mjs',
  workers: 1,
  retries: 0,
  timeout: 720_000,
  expect: { timeout: 30_000 },
  outputDir: process.env.E2E_PRIVATE_RESULTS,
  reporter: [['line']],
  use: {
    browserName: 'chromium',
    headless: true,
    actionTimeout: 30_000,
    navigationTimeout: 30_000,
    // Credentials are entered during this journey. Export only curated diagnostics.
    trace: 'off',
    screenshot: 'off',
    video: 'off',
  },
});
