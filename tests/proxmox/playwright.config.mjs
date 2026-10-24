import { createRequire } from 'node:module';
const require = createRequire(new URL('../installer/package.json', import.meta.url));
const { defineConfig } = require('@playwright/test');

export default defineConfig({
  testDir: '.',
  testMatch: process.env.LAB_PHASE === 'apps' ? 'apps.spec.mjs' : 'journey.spec.mjs',
  workers: 1,
  retries: 0,
  timeout: 35 * 60_000,
  expect: { timeout: 30_000 },
  outputDir: `${process.env.LAB_ARTIFACTS}/${process.env.LAB_PHASE === 'apps' ? 'apps-browser' : 'browser'}`,
  reporter: [['line'], ['html', { outputFolder: `${process.env.LAB_ARTIFACTS}/${process.env.LAB_PHASE === 'apps' ? 'apps-report' : 'report'}`, open: 'never' }]],
  use: {
    browserName: 'chromium',
    headless: process.env.LAB_HEADLESS === '1' || (!process.env.DISPLAY && !process.env.WAYLAND_DISPLAY),
    storageState: process.env.LAB_PHASE === 'apps' ? process.env.LAB_AUTH_STATE : undefined,
    viewport: { width: 1440, height: 1100 },
    launchOptions: { slowMo: 180, args: process.env.LAB_IMMICH_HOST === '1'
      ? ['--host-resolver-rules=MAP immich.kubarr.test 192.168.1.20'] : [] },
    video: 'on',
    trace: 'off',
    screenshot: 'only-on-failure',
    actionTimeout: 30_000,
    navigationTimeout: 30_000,
  },
});
