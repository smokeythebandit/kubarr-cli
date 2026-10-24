import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
const require = createRequire(new URL('../installer/package.json', import.meta.url));
const { test, expect } = require('@playwright/test');
const artifacts = process.env.LAB_ARTIFACTS;

test('restored Talos VM → managed storage → dashboard login', async ({ page }) => {
  let number = 0;
  const began = Date.now();
  const checkpoints = [];
  const shot = async label => {
    await page.screenshot({ path: join(artifacts, `${String(++number).padStart(2, '0')}-${label}.png`) });
    await page.screenshot({ path: join(artifacts, 'latest.png') });
    console.log(`[browser] ${label}`);
    checkpoints.push({ label, elapsedSeconds: (Date.now() - began) / 1000 });
    writeFileSync(join(artifacts, 'browser-timings.json'), JSON.stringify(checkpoints, null, 2));
  };
  try {
    await page.goto(process.env.LAB_INVITATION);
    await expect(page.locator('#wizard')).toBeVisible();
    await shot('destination');
    await page.locator('#context-tiles label').filter({ hasText: 'admin@kubarr-prod' }).click();
    await expect(page.locator('#next')).toBeEnabled({ timeout: 110_000 });
    await shot('cluster-checks-passed');
    await page.locator('#next').click();
    await page.locator('#server_name').fill('Kubarr Proxmox Test');
    await page.locator('input[name="access_mode"][value="nodeport"]').check();
    await shot('access');
    await page.locator('#next').click();
    await page.locator('input[name="storage_mode"][value="managed-nfs"]').check();
    await page.locator('#storage_size').fill('50Gi');
    await shot('managed-storage');
    await page.locator('#next').click();
    await page.locator('#admin_username').fill('installer-test');
    await page.locator('#admin_email').fill('installer-test@example.invalid');
    await page.locator('#admin_password').fill(process.env.LAB_PASSWORD);
    await page.locator('#admin_confirm').fill(process.env.LAB_PASSWORD);
    await shot('administrator');
    await page.locator('#next').click();
    await shot('review');
    await page.locator('#start').click();
    await shot('installation-started');
    let last = '';
    const deadline = Date.now() + 30 * 60_000;
    while (Date.now() < deadline) {
      const response = await page.request.get(new URL('/api/status', page.url()).href);
      expect(response.ok()).toBe(true);
      const status = await response.json();
      writeFileSync(join(artifacts, 'status.json'), JSON.stringify(status, null, 2));
      const event = status.events?.at(-1);
      if (event && event !== last) {
        console.log(`[installer] ${event}`);
        last = event;
      }
      await page.screenshot({ path: join(artifacts, 'latest.png') });
      if (status.phase === 'failed') throw new Error(status.result);
      if (status.phase === 'succeeded') break;
      await page.waitForTimeout(5000);
    }
    await expect(page.getByRole('heading', { name: 'Kubarr is ready', exact: true })).toBeVisible();
    await shot('installation-succeeded');
    const log = await page.request.get(new URL('/api/install-log', page.url()).href);
    writeFileSync(join(artifacts, 'installer.log'), await log.text());

    await page.goto('http://192.168.1.20:30080/login');
    await page.getByLabel('Username', { exact: true }).fill('installer-test');
    await page.getByLabel('Password', { exact: true }).fill(process.env.LAB_PASSWORD);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await expect(page).not.toHaveURL(/\/login/);
    const me = await page.request.get('http://192.168.1.20:30080/api/users/me');
    expect(me.ok()).toBe(true);
    expect((await me.json()).username).toBe('installer-test');
    await shot('dashboard-authenticated');
    await page.goto('http://192.168.1.20:30080/apps');
    await expect(page.getByRole('heading', { name: 'Sonarr', exact: true }).first()).toBeVisible({ timeout: 90_000 });
    await shot('catalog-ready');
    if (process.env.LAB_ALL_APPS === '1') {
      const response = await page.request.get('http://192.168.1.20:30080/api/apps/catalog');
      expect(response.ok()).toBe(true);
      const catalog = (await response.json()).map(app => ({
        name: app.name, display_name: app.display_name, category: app.category,
        is_system: app.is_system, is_hidden: app.is_hidden, is_browseable: app.is_browseable,
      }));
      writeFileSync(join(artifacts, 'catalog.json'), JSON.stringify(catalog, null, 2));
      await page.context().storageState({ path: process.env.LAB_AUTH_STATE });
      return;
    }
    await page.getByRole('heading', { name: 'Sonarr', exact: true }).first().click();
    const details = page.getByRole('region', { name: 'Sonarr details', exact: true });
    await details.getByRole('button', { name: 'Install', exact: true }).click();
    const wizard = page.getByRole('dialog', { name: 'Install sonarr', exact: true });
    await expect(wizard.getByText('Private config PVC', { exact: true })).toBeVisible();
    await shot('app-storage');
    await wizard.getByRole('button', { name: 'Next', exact: true }).click();
    await wizard.getByRole('button', { name: 'Next', exact: true }).click();
    const queued = page.waitForResponse(response => response.request().method() === 'POST'
      && new URL(response.url()).pathname === '/api/apps/install');
    await wizard.getByRole('button', { name: 'Confirm install', exact: true }).click();
    const accepted = await queued;
    expect(accepted.ok()).toBe(true);
    const operation = await accepted.json();
    await shot('app-install-queued');
    await expect.poll(async () => {
      const response = await page.request.get(`http://192.168.1.20:30080/api/apps/operations/${operation.id}`);
      expect(response.ok()).toBe(true);
      const current = await response.json();
      if (['failed', 'cancelled'].includes(current.status)) throw new Error(`Sonarr operation ${current.status}`);
      return current.status;
    }, { timeout: 360_000, intervals: [2000, 5000] }).toBe('succeeded');
    await expect(details.getByText('Running', { exact: true })).toBeVisible({ timeout: 60_000 });
    await shot('app-running');
    const app = await page.goto('http://192.168.1.20:30080/sonarr/');
    expect(app.ok()).toBe(true);
    await expect(page).toHaveTitle(/Sonarr/);
    await shot('sonarr-opened');
  } catch (error) {
    await shot('failure').catch(() => {});
    const log = await page.request.get(new URL('/api/install-log', page.url()).href).catch(() => null);
    if (log?.ok()) writeFileSync(join(artifacts, 'installer.log'), await log.text());
    throw error;
  }
});
