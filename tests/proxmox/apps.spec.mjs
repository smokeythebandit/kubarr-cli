import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, relative } from 'node:path';
const require = createRequire(new URL('../installer/package.json', import.meta.url));
const { test, expect } = require('@playwright/test');
const root = process.env.LAB_ARTIFACTS;
const base = 'http://192.168.1.20:30080';
const catalog = JSON.parse(readFileSync(join(root, 'catalog.json'), 'utf8'));
const apps = catalog.filter(app => !app.is_system && !app.is_hidden)
  .sort((a, b) => a.name.localeCompare(b.name));
const protectedNames = new Set(['kubarr-backend', 'kubarr-frontend', 'kubarr-worker', 'postgresql',
  'managed-nfs', 'openresty', 'fluent-bit', 'victoriametrics', 'victorialogs', 'cert-manager']);

function kube(args) {
  return execFileSync('kubectl', ['--kubeconfig', process.env.KUBECONFIG, '--request-timeout=10s', ...args],
    { encoding: 'utf8', timeout: 20_000, stdio: ['ignore', 'pipe', 'pipe'] });
}

function diagnostics(name, dir) {
  try {
    const pods = JSON.parse(kube(['get', 'pods', '-n', name, '-o', 'json']));
    writeFileSync(join(dir, 'pods.json'), JSON.stringify(pods.items.map(pod => ({
      name: pod.metadata.name, phase: pod.status.phase,
      containers: [...(pod.status.initContainerStatuses || []), ...(pod.status.containerStatuses || [])]
        .map(container => ({ name: container.name, ready: container.ready, restarts: container.restartCount,
          waiting: container.state?.waiting, terminated: container.state?.terminated?.reason })),
    })), null, 2));
    writeFileSync(join(dir, 'events.txt'), kube(['get', 'events', '-n', name, '--sort-by=.lastTimestamp']));
    writeFileSync(join(dir, 'claims.txt'), kube(['get', 'pvc', '-n', name]));
  } catch {
    writeFileSync(join(dir, 'diagnostics-unavailable.txt'), 'Namespace diagnostics could not be read.');
  }
}

async function select(page, app) {
  await page.goto(`${base}/apps`);
  await page.getByRole('heading', { name: app.display_name, exact: true }).first().click();
  return page.getByRole('region', { name: `${app.display_name} details`, exact: true });
}

async function configureImmichHost(page, result, shot) {
  // No real DNS or public domain is modified: Chromium resolves this .test
  // hostname directly to the same authorized VM for this test context.
  await page.goto(`${base}/settings?section=domains`);
  await page.getByRole('button', { name: 'Add Domain', exact: true }).click();
  let form = page.locator('form');
  await form.getByPlaceholder('example.com', { exact: true }).fill('immich.kubarr.test');
  await form.locator('select').nth(0).selectOption('exact');
  await form.locator('select').nth(1).selectOption('private');
  const created = page.waitForResponse(response => response.request().method() === 'POST'
    && new URL(response.url()).pathname === '/api/domains');
  await form.getByRole('button', { name: 'Save', exact: true }).click();
  const response = await created;
  expect(response.ok()).toBe(true);
  result.lab_domain_id = (await response.json()).id;
  await page.getByRole('button', { name: 'Assign App URL', exact: true }).click();
  form = page.locator('form');
  await form.locator('select').nth(0).selectOption('immich');
  await form.locator('select').nth(1).selectOption({ label: 'immich.kubarr.test' });
  await form.locator('select').nth(2).selectOption('exact_host');
  await form.locator('input:not([type="checkbox"])').fill('immich.kubarr.test');
  for (const checkbox of await form.locator('input[type="checkbox"]').all()) await checkbox.check();
  const assigned = page.waitForResponse(response => response.request().method() === 'POST'
    && new URL(response.url()).pathname === '/api/domains/assignments');
  await form.getByRole('button', { name: 'Save', exact: true }).click();
  expect((await assigned).ok()).toBe(true);
  await shot('05-host-assigned');
  await page.goto('http://immich.kubarr.test:30080/login');
  await page.getByLabel('Username', { exact: true }).fill('installer-test');
  await page.getByLabel('Password', { exact: true }).fill(process.env.LAB_PASSWORD);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await page.waitForURL(url => url.pathname !== '/login');
  const opened = await page.goto('http://immich.kubarr.test:30080/', { waitUntil: 'domcontentloaded' });
  expect(opened.ok()).toBe(true);
  await expect.poll(async () => (await page.locator('body').innerText()).trim().length,
    { timeout: 30_000 }).toBeGreaterThan(0);
  await expect(page.getByText(/Welcome to Immich|Getting Started|Create.*account|Sign [Ii]n/).first()).toBeVisible();
  result.http_status = opened.status();
  result.access = 'reachable_on_dedicated_host';
  result.path_route_supported = false;
  result.access_mode = 'exact_host';
  result.rendered_ui = true;
  result.page_title = await page.title();
  await shot('05-opened-dedicated-host');
}

async function waitOperation(page, id, app, timeout = 15 * 60_000) {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    const response = await page.request.get(`${base}/api/apps/operations/${id}`);
    expect(response.ok()).toBe(true);
    const operation = await response.json();
    if (operation.status !== last) {
      console.log(`[app ${app.name}] ${operation.operation}: ${operation.status}`);
      last = operation.status;
    }
    if (operation.status === 'succeeded') return operation;
    if (['failed', 'cancelled', 'retried'].includes(operation.status)) {
      writeFileSync(join(process.env.LAB_PRIVATE_DIR, `${app.name}-${id}.json`), JSON.stringify(operation), { mode: 0o600 });
      throw new Error(`${app.name} ${operation.operation} ${operation.status}; see namespace diagnostics`);
    }
    if (operation.operation === 'install' && operation.status === 'running') {
      let reason;
      try {
        const events = JSON.parse(kube(['get', 'events', '-n', app.name, '-o', 'json']));
        if (events.items.some(event => /violates PodSecurity/.test(event.message || ''))) reason = 'Pod Security admission rejected the app';
        if (events.items.some(event => /manifest unknown|failed to resolve reference.*not found|no match for platform in manifest/i.test(event.message || ''))) reason = 'a required container image is unavailable';
      } catch { /* Keep polling transient API/read failures. */ }
      if (reason) throw new Error(`${app.name}: ${reason}; captured events confirm a non-retryable failure`);
    }
    await page.screenshot({ path: join(root, 'latest.png') });
    await page.waitForTimeout(5000);
  }
  throw new Error(`${app.name} operation deadline exceeded`);
}

for (const app of apps) {
  test(`${app.display_name}: install, readiness, access, uninstall`, async ({ page }, testInfo) => {
    if (!/^[a-z0-9][a-z0-9-]*$/.test(app.name) || protectedNames.has(app.name)) {
      throw new Error('Catalog entry is not a permitted application test target');
    }
    const dir = join(root, 'apps', app.name);
    mkdirSync(dir, { recursive: true });
    const result = { name: app.name, display_name: app.display_name, install: 'not_started',
      access: 'not_tested', cleanup: 'not_needed', started_at: new Date().toISOString(),
      video: relative(root, testInfo.outputPath('video.webm')) };
    const save = () => writeFileSync(join(dir, 'result.json'), JSON.stringify(result, null, 2));
    const shot = async label => {
      await page.screenshot({ path: join(dir, `${label}.png`) });
      await page.screenshot({ path: join(root, 'latest.png') });
      console.log(`[app ${app.name}] ${label}`);
    };
    let operationId;
    let failure;
    const networkFailures = [];
    page.on('response', response => {
      if (response.status() >= 400) networkFailures.push({ status: response.status(), path: new URL(response.url()).pathname });
    });
    page.on('requestfailed', request => networkFailures.push({ path: new URL(request.url()).pathname, error: request.failure()?.errorText }));
    page.on('pageerror', error => networkFailures.push({ page_error: String(error.message).replaceAll(process.env.LAB_PASSWORD, '[REDACTED]') }));
    save();
    try {
      const details = await select(page, app);
      await shot('01-details');
      await details.getByRole('button', { name: 'Install', exact: true }).click();
      const wizard = page.getByRole('dialog', { name: `Install ${app.name}`, exact: true });
      await expect(wizard).toBeVisible();
      for (let step = 0; step < 4; step++) {
        await shot(`02-wizard-${step}`);
        if (await wizard.getByRole('button', { name: 'Confirm install', exact: true }).isVisible()) break;
        await expect(wizard.getByRole('button', { name: 'Next', exact: true })).toBeEnabled();
        await wizard.getByRole('button', { name: 'Next', exact: true }).click();
      }
      const queued = page.waitForResponse(response => response.request().method() === 'POST'
        && new URL(response.url()).pathname === '/api/apps/install'
        && response.request().postDataJSON()?.app_name === app.name);
      await wizard.getByRole('button', { name: 'Confirm install', exact: true }).click();
      const accepted = await queued;
      expect(accepted.ok()).toBe(true);
      result.chart_version = accepted.request().postDataJSON().chart_version;
      operationId = (await accepted.json()).id;
      result.operation_id = operationId;
      result.install = 'running'; save();
      await shot('03-installing');
      const operation = await waitOperation(page, operationId, app);
      result.install = 'succeeded';
      result.install_seconds = (Date.parse(operation.finished_at) - Date.parse(operation.started_at)) / 1000;
      save();
      const installed = await select(page, app);
      await expect(installed.getByText('Running', { exact: true })).toBeVisible({ timeout: 90_000 });
      result.readiness = 'running';
      const claims = JSON.parse(kube(['get', 'pvc', '-n', app.name, '-o', 'json']));
      result.claims = claims.items.map(claim => ({ name: claim.metadata.name, phase: claim.status.phase }));
      expect(claims.items.every(claim => claim.status.phase === 'Bound')).toBe(true);
      await shot('04-running');
      if (app.name === 'immich' && process.env.LAB_IMMICH_HOST === '1') {
        await configureImmichHost(page, result, shot);
      } else if (app.is_browseable) {
        // Observe the actual gateway response and render the app in a browser.
        // Native app authentication (401) is distinct from a broken upstream.
        let response = await page.request.get(`${base}/${app.name}/`, { maxRedirects: 10 });
        result.http_status = response.status();
        let openUrl = `${base}/${app.name}/`;
        if (app.name === 'jenkins' && response.status() === 403) {
          // Jenkins denies the anonymous home page during initial setup. Its
          // native login page must render; a generic gateway 403 is not enough.
          openUrl = `${base}/jenkins/login`;
          response = await page.request.get(openUrl);
          result.login_http_status = response.status();
        }
        result.access = response.status() === 401 ? 'native_auth_required' : response.ok() ? 'reachable' : 'failed';
        expect(response.ok() || response.status() === 401).toBe(true);
        if (response.ok()) {
          await page.goto(openUrl, { waitUntil: 'domcontentloaded' });
          await page.waitForTimeout(1500);
          result.page_title = await page.title();
          if (process.env.LAB_STRICT_UI === '1') {
            await expect.poll(async () => {
              // Plex renders its native sign-in inside an iframe. The outer
              // document has no text, so inspect the visible framed UI too.
              const frames = app.name === 'plex' ? page.frames() : [page.mainFrame()];
              const content = await Promise.all(frames.map(frame => frame.locator('body').innerText().catch(() => '')));
              return content.join('').trim().length;
            },
              { timeout: 20_000 }).toBeGreaterThan(0);
            result.rendered_ui = true;
            if (app.name === 'jenkins') {
              const unlock = page.getByRole('heading', { name: 'Unlock Jenkins', exact: true });
              await expect(unlock.or(page.getByRole('textbox', { name: /user|name/i })).first()).toBeVisible();
              result.access = await unlock.isVisible() ? 'native_setup_required' : 'native_login_required';
            }
            if (app.name === 'jellyseerr') {
              await expect(page.getByText(/volume mount was not configured properly/)).not.toBeVisible();
              result.config_mount_warning = false;
            }
          }
          await shot('05-opened');
        }
      } else result.access = 'not_browseable';
    } catch (error) {
      // Playwright request errors can include Cookie headers in their call log.
      // Keep the diagnostic category, not the credential-bearing call details.
      failure = new Error(`${app.name}: ${String(error.message).split('\n')[0]}`);
      if (result.install === 'running') result.install = 'failed';
      if (result.install === 'succeeded' && result.access === 'not_tested') result.access = 'failed';
      result.error = failure.message;
      await shot('failure').catch(() => {});
    } finally {
      diagnostics(app.name, dir);
      if (operationId) {
        try {
          const currentResponse = await page.request.get(`${base}/api/apps/operations/${operationId}`);
          const current = await currentResponse.json();
          if (['queued', 'paused', 'running'].includes(current.status)) {
            await page.request.post(`${base}/api/apps/operations/${operationId}/cancel`);
            for (let i = 0; i < 30; i++) {
              const updated = await (await page.request.get(`${base}/api/apps/operations/${operationId}`)).json();
              if (!['queued', 'paused', 'running'].includes(updated.status)) break;
              await page.waitForTimeout(1000);
            }
          }
          const details = await select(page, app);
          const button = details.getByRole('button', { name: 'Uninstall', exact: true });
          let response;
          if (await button.isVisible()) {
            const removed = page.waitForResponse(response => response.request().method() === 'DELETE'
              && new URL(response.url()).pathname === `/api/apps/${app.name}`);
            await button.click();
            response = await removed;
            result.cleanup_method = 'browser';
          } else {
            // Failed partial installs may not expose an Uninstall button. Keep
            // this fallback explicit in the result instead of treating it as UI success.
            response = await page.request.delete(`${base}/api/apps/${app.name}`);
            result.cleanup_method = 'api_after_failed_install';
          }
          expect(response.ok()).toBe(true);
          const removed = await response.json();
          await waitOperation(page, removed.id, app, 240_000);
          result.cleanup = 'succeeded';
          await shot('06-uninstalled');
        } catch (error) {
          result.cleanup = 'failed';
          result.cleanup_error = String(error.message).split('\n')[0];
          failure ||= new Error(`${app.name} cleanup: ${result.cleanup_error}`);
          // Keep subsequent app tests independent when UI/API removal fails.
          // This is restricted to the non-system app's own release namespace.
          try {
            execFileSync('helm', ['--kubeconfig', process.env.KUBECONFIG, 'uninstall', app.name,
              '-n', app.name, '--wait=legacy', '--timeout', '120s'],
            { encoding: 'utf8', timeout: 140_000, stdio: ['ignore', 'pipe', 'pipe'] });
            result.cleanup_fallback = 'helm_uninstall';
          } catch {
            result.cleanup_fallback = 'failed';
          }
        }
      }
      result.finished_at = new Date().toISOString();
      if (result.lab_domain_id) {
        const response = await page.request.delete(`${base}/api/domains/${result.lab_domain_id}`).catch(() => null);
        result.domain_cleanup = response?.ok() ? 'succeeded' : 'failed';
        if (result.domain_cleanup === 'failed') failure ||= new Error('Test domain cleanup failed');
      }
      result.passed = !failure;
      writeFileSync(join(dir, 'browser-errors.json'), JSON.stringify(networkFailures, null, 2));
      save();
    }
    if (failure) throw failure;
  });
}
