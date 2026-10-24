import { test, expect } from '@playwright/test';
import { execFileSync, spawn } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

const required = name => {
  if (!process.env[name]) throw new Error(`Missing ${name}; use run.py`);
  return process.env[name];
};
const kubectl = (...args) => execFileSync('kubectl', [
  '--kubeconfig', required('KUBECONFIG'), '--request-timeout=15s', ...args,
], { encoding: 'utf8', timeout: 30_000 });

test('fresh cluster → browser installer → admin login → working Sonarr', async ({ page }) => {
  const timings = [];
  let forward;
  let installerPage = page;
  const stage = async (name, fn) => test.step(name, async () => {
    const start = Date.now();
    console.log(`[e2e] ${name}`);
    try { await fn(); }
    finally {
      timings.push({ stage: name, seconds: (Date.now() - start) / 1000 });
      writeFileSync(join(required('E2E_ARTIFACTS'), 'browser-timings.json'), JSON.stringify(timings, null, 2));
    }
  });
  try {
    await stage('Configure installer through browser', async () => {
      await page.goto(required('E2E_INVITATION'));
      await expect(page.locator('#wizard')).toBeVisible();
      await page.locator('#context-tiles label').filter({ hasText: required('E2E_CONTEXT') }).click();
      await expect(page.locator('#next')).toBeEnabled({ timeout: 100_000 });
      await page.locator('#next').click();
      await expect(page.locator('#step-1')).toBeVisible();
      await page.locator('#server_name').fill('Installer E2E');
      await page.locator('input[name="access_mode"][value="port-forward"]').check();
      await page.locator('#next').click();
      await page.locator('input[name="storage_mode"][value="external-nfs"]').check();
      await page.locator('#nfs_server').fill(required('E2E_NFS_SERVER'));
      await page.locator('#nfs_path').fill('/');
      await page.locator('#next').click();
      await expect.poll(async () => {
        if (await page.locator('#step-3').isVisible()) return 'passed';
        const message = await page.locator('#storage-check').innerText();
        return message.includes('Needs attention') ? 'failed' : 'running';
      }, { timeout: 150_000 }).not.toBe('running');
      await expect(page.locator('#step-3'), await page.locator('#storage-check').innerText()).toBeVisible({ timeout: 1000 });
      await page.locator('#admin_username').fill('e2e-admin');
      await page.locator('#admin_email').fill('e2e@example.invalid');
      await page.locator('#admin_password').fill(required('E2E_PASSWORD'));
      await page.locator('#admin_confirm').fill(required('E2E_PASSWORD'));
      await page.locator('#next').click();
      await expect(page.locator('#step-4')).toBeVisible();
    });

    await stage('Install Kubarr and observe real progress', async () => {
      await page.locator('#start').click();
      // Poll the authenticated status as well as asserting the finished UI: fail
      // immediately on an installer error rather than waiting out the test deadline.
      await expect.poll(async () => {
        const response = await page.request.get(new URL('/api/status', page.url()).href);
        expect(response.ok()).toBe(true);
        const status = await response.json();
        if (status.phase === 'failed') throw new Error(`Installer failed: ${status.result}`);
        return status.phase;
      }, { timeout: 360_000, intervals: [1000, 2000] }).toBe('succeeded');
      await expect(page.getByRole('heading', { name: 'Kubarr is ready', exact: true })).toBeVisible();
      const log = await page.request.get(new URL('/api/install-log', page.url()).href);
      expect(log.ok()).toBe(true);
      writeFileSync(join(required('E2E_ARTIFACTS'), 'installer.log'), await log.text());
    });

    await stage('Log into installed dashboard through gateway', async () => {
      forward = spawn('kubectl', ['--kubeconfig', required('KUBECONFIG'),
        '-n', 'openresty', 'port-forward', '--address', '127.0.0.1',
        'svc/kubarr-gateway', ':8080'], { stdio: ['ignore', 'pipe', 'pipe'] });
      const port = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('Gateway port-forward timed out')), 30_000);
        forward.once('error', error => { clearTimeout(timer); reject(error); });
        forward.once('exit', code => { clearTimeout(timer); reject(new Error(`Gateway tunnel exited: ${code}`)); });
        forward.stdout.on('data', chunk => {
          const match = String(chunk).match(/127\.0\.0\.1:(\d+) ->/);
          if (match) { clearTimeout(timer); resolve(match[1]); }
        });
      });
      const base = `http://127.0.0.1:${port}`;
      // A separate context prevents installer cookies from affecting app auth.
      const appContext = await page.context().browser().newContext({ baseURL: base });
      page = await appContext.newPage();
      await page.goto('/login');
      await page.getByLabel('Username', { exact: true }).fill('e2e-admin');
      await page.getByLabel('Password', { exact: true }).fill(required('E2E_PASSWORD'));
      await Promise.all([
        page.waitForURL(url => url.pathname !== '/login'),
        page.getByRole('button', { name: 'Sign in', exact: true }).click(),
      ]);
      const me = await page.request.get('/api/users/me');
      expect(me.ok()).toBe(true);
      expect((await me.json()).username).toBe('e2e-admin');
    });

    await stage('Install Sonarr through catalog', async () => {
      await page.goto('/apps');
      const heading = page.getByRole('heading', { name: 'Sonarr', exact: true }).first();
      await expect(heading).toBeVisible({ timeout: 60_000 });
      await heading.click();
      const details = page.getByRole('region', { name: 'Sonarr details', exact: true });
      await details.getByRole('button', { name: 'Install', exact: true }).click();
      const wizard = page.getByRole('dialog', { name: 'Install sonarr', exact: true });
      await expect(wizard.getByText('Private config PVC', { exact: true })).toBeVisible();
      await wizard.getByRole('button', { name: 'Next', exact: true }).click();
      await expect(wizard.getByLabel('VPN provider')).toHaveValue('');
      await wizard.getByRole('button', { name: 'Next', exact: true }).click();
      const queued = page.waitForResponse(response => response.request().method() === 'POST'
        && new URL(response.url()).pathname === '/api/apps/install');
      await wizard.getByRole('button', { name: 'Confirm install', exact: true }).click();
      const response = await queued;
      expect(response.ok()).toBe(true);
      const { id } = await response.json();
      expect(id).toBeTruthy();
      await expect.poll(async () => {
        const response = await page.request.get(`/api/apps/operations/${id}`);
        expect(response.ok()).toBe(true);
        const operation = await response.json();
        if (['failed', 'cancelled'].includes(operation.status)) {
          throw new Error(`Sonarr operation ${id}: ${operation.status}`);
        }
        return operation.status;
      }, { timeout: 180_000, intervals: [1000, 2000] }).toBe('succeeded');
      await expect(details.getByText('Running', { exact: true })).toBeVisible({ timeout: 60_000 });
    });

    await stage('Verify Sonarr storage, workload and proxied application', async () => {
      const deployment = JSON.parse(kubectl('-n', 'sonarr', 'get', 'deployment/sonarr', '-o', 'json'));
      expect(deployment.status.readyReplicas).toBeGreaterThan(0);
      const claims = JSON.parse(kubectl('-n', 'sonarr', 'get', 'pvc', '-o', 'json'));
      expect(claims.items.length).toBeGreaterThanOrEqual(2);
      expect(claims.items.every(claim => claim.status.phase === 'Bound')).toBe(true);
      const key = kubectl('-n', 'sonarr', 'exec', 'deployment/sonarr', '-c', 'sonarr', '--',
        'sh', '-c', "sed -n 's/.*<ApiKey>\\([^<]*\\)<\\/ApiKey>.*/\\1/p' /config/config.xml").trim();
      expect(key.length > 0).toBe(true);
      await expect.poll(async () => {
        const response = await page.request.get('/sonarr/api/v3/system/status', { headers: { 'X-Api-Key': key } });
        return response.ok() ? (await response.json()).appName : `HTTP ${response.status()}`;
      }, { timeout: 60_000 }).toBe('Sonarr');
      const response = await page.goto('/sonarr/');
      expect(response.ok()).toBe(true);
      await expect(page).toHaveTitle(/Sonarr/);
    });
  } catch (error) {
    // No DOM dumps/screenshots of password fields or raw request/response traces.
    for (const target of new Set([installerPage, page])) {
      await target.locator('input[type="password"]').evaluateAll(inputs => {
        for (const input of inputs) input.value = '';
      }).catch(() => {});
    }
    await page.screenshot({ path: join(required('E2E_ARTIFACTS'), 'failure.png') }).catch(() => {});
    const log = await installerPage.request.get(new URL('/api/install-log', installerPage.url()).href).catch(() => null);
    if (log?.ok()) writeFileSync(join(required('E2E_ARTIFACTS'), 'installer.log'), await log.text());
    throw error;
  } finally {
    forward?.kill('SIGTERM');
  }
});
