import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  testMatch: '**/*.spec.ts',
  // The Rust server does not include managed computer control yet.
  testIgnore: process.env.GROK_BOT_BACKEND === 'rust' ? ['**/computer.spec.ts'] : [],
  timeout: 120_000,
  expect: { timeout: 15_000 },
  workers: 1,
  fullyParallel: false,
  reporter: 'list',
  outputDir: '/tmp/grok-bot-playwright-results',
  use: {
    browserName: 'chromium',
    headless: true,
    launchOptions: { channel: 'chrome', args: ['--no-sandbox'] },
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
});
