import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';

// Run the actual embedded script with a small DOM and a fake authenticated server.
// No request is ever sent to a real installer.
const html = readFileSync(new URL('./bootstrap_web.html', import.meta.url), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script);
const accessStep = html.split('<section id="step-1"')[1].split('<section id="step-2"')[0];
const lbNoticeMarkup = accessStep.match(/(<div id="lb-notice"[\s\S]*?<\/div><\/div>)\s*(?=<div id="lb-check")/)?.[1];
assert.ok(lbNoticeMarkup);

test('destination actions and discovered contexts share a labelled radio group', () => {
  const step = html.split('<section id="step-0"')[1].split('<section id="step-1"')[0];
  const group = step.match(/<fieldset class="destination-group" aria-labelledby="title-0">([\s\S]*?)<\/fieldset>/)?.[1];
  assert.ok(group);
  assert.match(group, /<legend>Installation destination<\/legend>/);
  const grid = group.match(/<div class="destination-grid">([\s\S]*?)<\/div>/)?.[1];
  assert.ok(grid);
  const tiles = [...grid.matchAll(/<label class="destination-tile"><input type="radio" name="destination_mode" value="([^"]+)">([\s\S]*?)<\/label>/g)];
  assert.deepEqual(tiles.map(tile => tile[1]), ['single-node', 'upload']);
  for (const tile of tiles) {
    assert.match(tile[2], /<strong>[^<]+<\/strong><small>[^<]+<\/small>/);
    assert.match(tile[2], /class="tile-check"/);
  }
  assert.match(group, /id="context-tiles" class="destination-grid"/);
  assert.doesNotMatch(step, /id="local_target"|id="upload_target"/);
  const details = step.indexOf('class="destination-details"');
  assert.ok(details > step.indexOf('</fieldset>'));
  for (const id of ['k3s-note', 'upload-options']) {
    assert.ok(step.indexOf(`id="${id}"`) > details);
  }
});

test('dashboard access choices describe the gateway and default to its NodePort', () => {
  const step = html.split('<section id="step-1"')[1].split('<section id="step-2"')[0];
  assert.deepEqual([...step.matchAll(/name="access_mode" value="([^"]+)"/g)].map(match => match[1]), ['nodeport','loadbalancer','port-forward']);
  assert.match(step, /Gateway NodePort<input id="backend_port" name="backend_port" type="number" value="30080" min="30000" max="32767" required>/);
  assert.match(step, /LoadBalancer.*public address assigned by your cluster’s load balancer \(service port 8080\)/);
  assert.match(step, /Port-forward only.*gateway service on port 8080/);
  assert.match(lbNoticeMarkup, /Bare-metal clusters need a configured load balancer implementation.*MetalLB or Cilium.*does not install a controller.*external IP may remain pending/);
});

function page(server, { hash = '#one-use-invitation', hostname = '127.0.0.1', storedTheme, storageDisabled = false, systemDark = false, now = 1000000 } = {}) {
  const nodes = new Map();
  function node(id = '') {
    const classes = new Set(['hidden']);
    const attributes = new Map();
    const childNodes = [];
    const children = new Proxy({
      get length() { return childNodes.length; },
      item(index) { return childNodes[index] ?? null; },
      *[Symbol.iterator]() { yield* childNodes; },
    }, { get(collection, key) { return typeof key === 'string' && /^(0|[1-9]\d*)$/.test(key) ? childNodes[Number(key)] : Reflect.get(collection,key); } });
    return {
      id, children, dataset: {}, style: {}, value: '', required: false, disabled: false, open: false,
      classList: { add: v => classes.add(v), remove: v => classes.delete(v), toggle: (v, on) => on ? classes.add(v) : classes.delete(v), contains: v => classes.has(v) },
      set textContent(v) { for (const child of childNodes) child.parentNode = null; childNodes.length = 0; this._text = v; },
      get textContent() { return (this._text || '') + childNodes.map(child => child.textContent).join(''); },
      append(...nodes) { for (const child of nodes) this.insertBefore(child,null); },
      insertBefore(child, reference) {
        if (child.parentNode) child.remove();
        const index = reference === null ? childNodes.length : childNodes.indexOf(reference);
        if (index < 0) throw Error('reference is not a child');
        childNodes.splice(index,0,child); child.parentNode = this;
      },
      remove() { if (this.parentNode) this.parentNode._removeChild(this); },
      _removeChild(child) { const index = childNodes.indexOf(child); if (index !== -1) childNodes.splice(index,1); child.parentNode = null; },
      querySelector(selector) {
        if (selector.startsWith('.')) {
          for (const child of children) {
            if (child.className?.split(' ').includes(selector.slice(1))) return child;
            const match = child.querySelector(selector);
            if (match) return match;
          }
          return null;
        }
        throw Error(`unsupported selector: ${selector}`);
      },
      get firstChild() { return childNodes[0]; },
      setAttribute: (key, value) => attributes.set(key, value),
      removeAttribute: key => attributes.delete(key),
      getAttribute: key => attributes.get(key),
       focus() { server.focused = id; (server.focusHistory ||= []).push(id); },
    };
  }
  const element = id => { if (!nodes.has(id)) nodes.set(id, node(id)); return nodes.get(id); };
  const events = element('events');
  events.textContent = 'Waiting for updates…';
  events.clientHeight = 100;
  Object.defineProperty(events, 'scrollHeight', { get() { return Math.max(100, events.textContent.length * 2); } });
  let scrollTop = 0;
  Object.defineProperty(events, 'scrollTop', {
    get() { return scrollTop; },
    set(value) { scrollTop = Math.max(0, Math.min(value, events.scrollHeight - events.clientHeight)); },
  });
  element('latest-activity').textContent = 'Latest activity: Waiting for updates…';
  element('activity-count').textContent = 'Recent activity · 0 events (up to 500 retained)';
  // Hydrate the static notice text from the real markup; the script controls its visibility.
  element('lb-notice').textContent = lbNoticeMarkup.replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim();
  const defaults = {
    destination_mode: '', server_name: 'Kubarr', storage_mode: 'managed-nfs', storage_size: '1Ti',
    storage_class: '', nfs_server: '', nfs_path: '/data', access_mode: 'nodeport', backend_port: '30080',
    admin_username: 'admin', admin_email: 'admin@example.invalid', admin_password: 'sample-password',
    admin_confirm: 'sample-password', grafana: false,
  };
  const form = element('form');
  const radios = {};
  form.elements = {};
  for (const [key, initial] of Object.entries(defaults)) {
    if (['destination_mode','storage_mode','access_mode'].includes(key)) {
      const group = { destination_mode: ['single-node','upload'], storage_mode: ['managed-nfs','external-nfs'], access_mode: ['nodeport','loadbalancer','port-forward'] }[key];
      radios[key] = group.map(v => { const radio = node(); radio.value = v; radio.checked = v === initial; return radio; });
       form.elements[key] = { get value() { return [...radios[key], ...Array.from(element('context-tiles').children, tile => tile.firstChild)].find(r => r.checked)?.value || ''; } };
    } else { form.elements[key] = element(key); form.elements[key].value = initial; }
  }
  element('kubeconfig_file').files = [];
   const steps = Array.from({ length: 7 }, (_, i) => { const button = node(); button.dataset.step = String(i); return button; });
  element('steps').querySelectorAll = () => steps;
  const location = { hash, pathname: '/', hostname };
  const timers = [], calls = [];
  let replaced = false;
  const themeStorage = new Map();
  if (storedTheme !== undefined) themeStorage.set('kubarr-theme', storedTheme);
  const storage = {
    getItem(key) { if (storageDisabled) throw Error('storage disabled'); return themeStorage.get(key) ?? null; },
    setItem(key, value) { if (storageDisabled) throw Error('storage disabled'); themeStorage.set(key, value); },
  };
  const mediaListeners = new Set();
  const media = { matches: systemDark, addEventListener(type, callback) { if (type === 'change') mediaListeners.add(callback); } };
  const documentClasses = new Set();
  const context = {
    document: { getElementById: element, createElement: () => node(), getElementsByName: name => radios[name] || [], documentElement: { classList: { toggle(name, on) { if (on) documentClasses.add(name); else documentClasses.delete(name); } } } },
    localStorage: storage,
    window: { matchMedia: () => media },
    location,
    history: { replaceState: (_, __, path) => { assert.equal(path, '/'); replaced = true; location.hash = ''; } },
    FormData: class {
      constructor(target) { this.fields = target.elements; }
      entries() { return Object.entries(this.fields).filter(([key]) => key !== 'grafana').map(([key, control]) => [key, control.value]); }
      has(key) { return key === 'grafana' && this.fields.grafana.value === true; }
    },
     TextEncoder, URL,
    Date: class extends Date { static now() { return now; } },
      fetch: async (url, options) => {
        calls.push({ url, options });
        const body = options.body && JSON.parse(options.body);
         const status = url === '/api/status' ? { phase: server.phase, events: server.events || [], result: server.result || null, access: server.access ?? null } : null;
        if (status && server.statusGates?.length) await server.statusGates.shift();
      if (url === '/api/session' && !server.cookie) return { ok: false, text: async () => 'Forbidden' };
      if (url === '/api/redeem') {
        if (server.cookie || server.redeemed || body.token !== 'one-use-invitation') return { ok: false, text: async () => 'Forbidden' };
        server.cookie = true; server.redeemed = true;
      }
       if (url === '/api/status' && server.offline) throw Error('offline');
       if (url === '/api/access') {
         if (server.accessGate) await server.accessGate;
         if (server.accessError) return {ok:false,status:503,text:async () => 'Unavailable'};
         return {ok:true,status:200,json:async () => server.access};
       }
      if (url === '/api/checks') {
        assert.equal(options.headers['X-Kubarr-CSRF'], 'csrf-value');
        if (server.checkStartGates?.length) await server.checkStartGates.shift();
        if (server.checkStartError) return {ok:false,text:async () => server.checkStartError};
        const id = `check-${server.checkSequence = (server.checkSequence || 0) + 1}`;
        (server.sessions ||= []).push({id,target_id:body.target_id});
        return {ok:true,status:202,json:async () => ({check_id:id})};
      }
      if (url === '/api/checks/cancel') {
        assert.equal(options.headers['X-Kubarr-CSRF'], 'csrf-value');
        (server.cancelled ||= []).push(body.check_id);
        return {ok:true,status:200,json:async () => ({cancelled:true})};
      }
      if (url.startsWith('/api/checks/')) {
        if (server.checkGates?.length) await server.checkGates.shift();
        if (server.checkError) return {ok:false,text:async () => server.checkError};
        const session = server.sessions.find(s => s.id === decodeURIComponent(url.split('/').at(-1)));
        const snapshot = server.checkSnapshots?.shift() || {phase:'complete',eligible:true,checks:[{id:'kubectl',label:'kubectl',severity:'required',state:'passed',message:'Found'}]};
        return {ok:true,status:200,json:async () => ({check_id:session.id,target_id:session.target_id,...snapshot})};
      }
      if (url === '/api/loadbalancer-checks') {
        assert.equal(options.headers['X-Kubarr-CSRF'],'csrf-value');
        if (server.lbStartGates?.length) await server.lbStartGates.shift();
        if (server.lbStartError) return {ok:false,status:503,text:async () => server.lbStartError};
        const id = `lb-${server.lbSequence = (server.lbSequence || 0) + 1}`;
        (server.lbSessions ||= []).push({id,target_id:body.target_id});
        return {ok:true,status:202,json:async () => ({check_id:id})};
      }
      if (url === '/api/loadbalancer-checks/cancel') {
        (server.lbCancelled ||= []).push(body.check_id);
        return {ok:true,status:200,json:async () => ({cancelled:true})};
      }
      if (url.startsWith('/api/loadbalancer-checks/')) {
        if (server.lbGates?.length) await server.lbGates.shift();
        if (server.lbGetError) return {ok:false,status:503,text:async () => server.lbGetError};
        const session = server.lbSessions.find(s => s.id === decodeURIComponent(url.split('/').at(-1)));
        const snapshot = server.lbSnapshots?.shift() || lbSuccess;
        return {ok:true,status:200,json:async () => ({check_id:session.id,target_id:session.target_id,...snapshot})};
      }
      if (url === '/api/storage-checks') {
        assert.equal(options.headers['X-Kubarr-CSRF'], 'csrf-value');
        if (server.storageStartGates?.length) await server.storageStartGates.shift();
        if (server.storageStartError) return {ok:false,status:503,text:async () => server.storageStartError};
        const id = `storage-${server.storageSequence = (server.storageSequence || 0) + 1}`;
        (server.storageSessions ||= []).push({id,...body});
        return {ok:true,status:202,json:async () => ({check_id:id})};
      }
      if (url === '/api/storage-checks/cancel') {
        (server.storageCancelled ||= []).push(body.check_id);
        return {ok:true,status:200,json:async () => ({cancelled:true})};
      }
      if (url.startsWith('/api/storage-checks/')) {
        if (server.storageGates?.length) await server.storageGates.shift();
        if (server.storageGetError) return {ok:false,status:503,text:async () => server.storageGetError};
        const session = server.storageSessions.find(s => s.id === decodeURIComponent(url.split('/').at(-1)));
        const snapshot = server.storageSnapshots?.shift() || {phase:'complete',eligible:true,checks:[{id:'cleanup',label:'Cleanup',severity:'required',state:'passed',message:'Unique file removed'}]};
        return {ok:true,status:200,json:async () => ({check_id:session.id,target_id:session.target_id,server:session.server,path:session.path,...snapshot})};
      }
      if (url === '/api/targets/upload') {
        assert.equal(options.headers['X-Kubarr-CSRF'], 'csrf-value');
        if (server.uploadGate) await server.uploadGate;
        if (server.uploadError) return { ok: false, text: async () => server.uploadError };
        server.uploaded = body.kubeconfig;
        return { ok: true, status: 200, json: async () => ({ targets: server.uploadResponses?.shift() || server.uploadTargets || [] }) };
      }
      if (url === '/api/targets') {
        if (server.targetsGate) await server.targetsGate;
        if (server.targetsError) return { ok: false, text: async () => server.targetsError };
        return { ok: true, status: 200, json: async () => ({ targets: server.targets ?? [localTarget] }) };
      }
      if (url.startsWith('/api/preflight')) {
        if (server.preflightGates?.length) await server.preflightGates.shift();
        return { ok: true, status: 200, json: async () => server.preflight || { kubectl:true, helm:true, context:url.includes('target_id=') ? 'chosen' : null, storage_class:'standard' } };
      }
      if (url === '/api/start') {
        assert.equal(options.headers['X-Kubarr-CSRF'], 'csrf-value');
        if (server.startGate) await server.startGate;
        if (server.startError) return { ok: false, status: server.startStatus || 400, text: async () => server.startError };
        server.phase = 'running'; server.payload = body;
        return { ok: true, status: 202 };
      }
      const data = url === '/api/session' || url === '/api/redeem' ? { csrf: 'csrf-value' }
        : url === '/api/status' ? status
        : server.preflight || { kubectl: true, helm: true, context: 'test', storage_class: 'standard' };
      return { ok: true, status: 200, json: async () => data };
    },
    setTimeout: (callback, delay) => { callback.delay = delay; timers.push(callback); },
  };
  vm.runInNewContext(script, context, { filename: 'bootstrap_web.html' });
  function choose(name, value) {
    for (const radio of radios[name]) radio.checked = radio.value === value;
    if (name === 'destination_mode') for (const tile of element('context-tiles').children) tile.firstChild.checked = false;
    radios[name].find(radio => radio.value === value).onchange();
  }
  return { element, form, steps, choose, calls, timers, location, themeStorage, documentClasses, tick(ms) { now += ms; },
    edit(name,value) { form.elements[name].value = value; form.elements[name].oninput?.(); },
    selectTarget(id) {
      const tiles = element('context-tiles').children;
       const selected = [...tiles].find(tile => tile.firstChild.dataset.targetId === id);
      assert.ok(selected, `context tile ${id} exists`);
       for (const radio of [...radios.destination_mode, ...Array.from(tiles, tile => tile.firstChild)]) radio.checked = radio === selected.firstChild;
      selected.firstChild.onchange();
    },
    upload(file) { element('kubeconfig_file').files = [file]; return element('upload').onclick(); },
    changeSystemTheme(dark) { media.matches = dark; for (const callback of mediaListeners) callback({ matches: dark }); },
    get replaced() { return replaced; } };
}

const flush = () => new Promise(resolve => setImmediate(resolve));
const localTarget = { id:'opaque-local', source:'default', label:'Work cluster', context:'work', current:true };
const lbSuccess = {phase:'complete',eligible:true,checks:[{id:'address',label:'Address',severity:'required',state:'passed',message:'Assigned'}, {id:'cleanup',label:'Cleanup',severity:'required',state:'passed',message:'Removed'}]};
function deferred() { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; }
const current = browser => browser.steps.findIndex(b => b.getAttribute('aria-current') === 'step');
function advance(browser, count) { for (let i = 0; i < count; i++) browser.element('next').onclick(); }

test('stored dark and light themes initialize the selector and document class', () => {
  const dark = page({ cookie: true, phase: 'ready' }, { storedTheme: 'dark' });
  assert.equal(dark.element('theme').value, 'dark');
  assert.equal(dark.documentClasses.has('dark'), true);
  const light = page({ cookie: true, phase: 'ready' }, { storedTheme: 'light', systemDark: true });
  assert.equal(light.element('theme').value, 'light');
  assert.equal(light.documentClasses.has('dark'), false);
});

test('system theme follows initial preference and subsequent media changes', () => {
  const b = page({ cookie: true, phase: 'ready' }, { systemDark: true });
  assert.equal(b.element('theme').value, 'system');
  assert.equal(b.documentClasses.has('dark'), true);
  b.changeSystemTheme(false);
  assert.equal(b.documentClasses.has('dark'), false);
  b.changeSystemTheme(true);
  assert.equal(b.documentClasses.has('dark'), true);
});

test('theme selector saves preference and storage failure does not disrupt authentication or flow', async () => {
  const b = page({ cookie: false, phase: 'ready' });
  b.element('theme').value = 'dark'; b.element('theme').onchange();
  assert.equal(b.themeStorage.get('kubarr-theme'), 'dark');
  assert.equal(b.documentClasses.has('dark'), true);

  const unavailable = page({ cookie: false, phase: 'ready' }, { storageDisabled: true });
  await flush();
  unavailable.element('theme').value = 'light'; unavailable.element('theme').onchange();
  assert.equal(unavailable.element('auth').textContent, '');
  assert.equal(unavailable.calls.some(call => call.url === '/api/redeem'), true);
  assert.equal(unavailable.element('wizard').classList.contains('hidden'), false);
  assert.equal(unavailable.documentClasses.has('dark'), false);
  unavailable.element('theme').value = 'dark'; unavailable.element('theme').onchange();
  assert.equal(unavailable.documentClasses.has('dark'), true);
});

test('preflight is informative, navigation validates only active steps and preserves edits', async () => {
  const server = { cookie: false, phase: 'ready', preflight: { kubectl: false, helm: false, context: null, storage_class: null } };
  const b = page(server, { hostname: '192.0.2.10' }); await flush();
  assert.equal(b.location.hash, ''); assert.equal(b.replaced, true);
  assert.equal(b.element('warning').classList.contains('hidden'), false);
  assert.match(b.element('checks').textContent, /Missing/);
  assert.equal(b.steps[4].disabled, true);
  assert.equal(b.element('next').disabled, true);
  assert.equal(server.focused, undefined);
   for (let i = 0; i < 7; i++) assert.equal(b.element('title-' + i).getAttribute('tabindex'), '-1');
  b.steps[4].onclick(); assert.equal(current(b), 0);
  b.selectTarget('opaque-local'); await flush();
  b.element('next').onclick(); assert.equal(current(b), 1); assert.equal(server.focused, 'title-1');
  b.form.elements.server_name.value = '  ';
  b.element('next').onclick(); assert.equal(current(b), 1);
  assert.equal(server.focused, 'server_name'); assert.match(b.element('error').textContent, /Server name/);
  b.form.elements.server_name.value = 'Media room';
  b.choose('destination_mode','single-node'); assert.equal(b.element('k3s-note').classList.contains('hidden'), false);
  b.choose('access_mode','port-forward'); b.form.elements.backend_port.value = 'invalid';
  advance(b, 2); assert.equal(current(b), 3);
  b.element('back').onclick(); assert.equal(current(b), 2);
  b.steps[1].onclick(); assert.equal(b.form.elements.server_name.value, 'Media room');
  assert.equal(b.form.elements.backend_port.required, false);
  b.steps[4].onclick(); assert.equal(current(b), 4);
  assert.match(b.element('summary').textContent, /Media room/);
  assert.doesNotMatch(b.element('summary').textContent, /sample-password|NodePort invalid/);
});

test('conditional storage and access validation follows selected mode', async () => {
  const b = page({ cookie: true, phase: 'ready' }); await flush(); b.choose('destination_mode','single-node'); advance(b, 2);
  b.form.elements.storage_size.value = '0Ti'; b.element('next').onclick();
  assert.equal(current(b), 2); assert.match(b.element('error').textContent, /storage size/i);
  b.choose('storage_mode','external-nfs');
  assert.equal(b.form.elements.storage_size.required, false);
  assert.equal(b.form.elements.nfs_server.required, true);
  b.element('next').onclick(); assert.equal(current(b), 2);
  b.form.elements.nfs_server.value = 'nas.local'; b.form.elements.nfs_path.value = 'relative';
  b.element('next').onclick(); assert.equal(current(b), 2);
  b.form.elements.nfs_path.value = '/media'; b.element('next').onclick(); assert.equal(current(b), 3);
  b.choose('storage_mode','managed-nfs'); b.form.elements.storage_size.value = '1Ti';
  b.form.elements.admin_confirm.value = 'wrong'; b.element('next').onclick(); assert.equal(current(b), 3);
  assert.equal(serverFocused(b), 'admin_confirm');
  b.form.elements.admin_confirm.value = 'sample-password'; b.element('next').onclick();
  assert.equal(current(b), 4); assert.match(b.element('summary').textContent, /Managed NFS/);
  assert.doesNotMatch(b.element('summary').textContent, /nas.local/);
});

test('gateway NodePort validates only while selected; inactive invalid port cannot block other modes', async () => {
  const b = page({cookie:true,phase:'ready'}); await flush();
  b.choose('destination_mode','single-node'); b.element('next').onclick();
  assert.equal(b.form.elements.backend_port.value,'30080');
  assert.equal(b.form.elements.backend_port.required,true);
  for (const invalidPort of ['','30081.5','29999','32768','not-a-port']) {
    b.edit('backend_port',invalidPort); b.element('next').onclick();
    assert.equal(current(b),1);
    assert.match(b.element('error').textContent,/Gateway NodePort must be between 30000 and 32767/);
    assert.equal(b.form.elements.backend_port.getAttribute('aria-invalid'),'true');
  }
  b.choose('access_mode','loadbalancer');
  assert.equal(b.element('port').classList.contains('hidden'),true);
  assert.equal(b.form.elements.backend_port.required,false);
  assert.equal(b.element('lb-notice').classList.contains('hidden'),false);
  b.element('next').onclick(); assert.equal(current(b),1);
  assert.equal(b.element('next').disabled,true);
  assert.equal(b.form.elements.backend_port.getAttribute('aria-invalid'),undefined);
  b.choose('access_mode','port-forward');
  assert.equal(b.element('lb-notice').classList.contains('hidden'),true);
  assert.equal(b.form.elements.backend_port.required,false);
  b.element('next').onclick(); assert.equal(current(b),2);
  b.element('back').onclick(); b.choose('access_mode','nodeport');
  assert.equal(b.element('port').classList.contains('hidden'),false);
  assert.equal(b.form.elements.backend_port.required,true);
  b.element('next').onclick(); assert.equal(current(b),1);
});

test('all dashboard access modes send their distinct access_mode and display accurate review/warning', async () => {
  for (const [mode,review,warning] of [
    ['nodeport',/Gateway NodePort 30080/,false],
    ['loadbalancer',/LoadBalancer · gateway service port 8080 · external address assigned by load balancer/,true],
    ['port-forward',/Port-forward only · gateway service port 8080/,false],
  ]) {
    const server = {cookie:true,phase:'ready'};
    const b = page(server); await flush(); if (mode === 'loadbalancer') { b.selectTarget('opaque-local'); await flush(); } else b.choose('destination_mode','single-node');
    b.element('next').onclick(); b.choose('access_mode',mode);
    if (mode === 'loadbalancer') { b.element('lb-test').onclick(); await flush(); }
    if (mode !== 'nodeport') b.edit('backend_port','invalid hidden port');
    advance(b,3); assert.equal(current(b),4);
    assert.match(b.element('summary').textContent,review);
    if (mode !== 'nodeport') assert.doesNotMatch(b.element('summary').textContent,/NodePort|invalid hidden port/);
    assert.equal(b.element('lb-review-notice').classList.contains('hidden'),!warning);
    assert.equal(b.element('lb-notice').classList.contains('hidden'),!warning);
    await b.element('start').onclick();
    assert.equal(current(b),5);
    assert.equal(server.payload.access_mode,mode);
    assert.equal(server.payload.backend_port,mode === 'nodeport' ? '30080' : 'invalid hidden port'); // Legacy API field, not an LB setting.
    assert.equal('external_port' in server.payload,false);
    assert.equal('loadbalancer_port' in server.payload,false);
  }
});

test('review warning and summary update when switching away from and back to LoadBalancer', async () => {
  const b = page({cookie:true,phase:'ready'}); await flush(); b.selectTarget('opaque-local'); await flush();
  b.element('next').onclick(); b.choose('access_mode','loadbalancer'); b.element('lb-test').onclick(); await flush(); advance(b,3);
  assert.equal(b.element('lb-review-notice').classList.contains('hidden'),false);
  b.steps[1].onclick(); b.choose('access_mode','port-forward'); b.steps[4].onclick();
  assert.equal(b.element('lb-review-notice').classList.contains('hidden'),true);
  assert.match(b.element('summary').textContent,/Port-forward only · gateway service port 8080/);
  b.steps[1].onclick(); b.choose('access_mode','loadbalancer'); b.steps[4].onclick(); assert.equal(current(b),1);
  b.element('lb-test').onclick(); await flush(); b.steps[4].onclick();
  assert.equal(b.element('lb-review-notice').classList.contains('hidden'),false);
  assert.match(b.element('summary').textContent,/LoadBalancer · gateway service port 8080/);
});

function serverFocused(b) { return b.form.elements.admin_confirm.getAttribute('aria-invalid') === 'true' ? 'admin_confirm' : ''; }

test('start sends backend contract once, clears secrets, and reconnects to success', async () => {
  const server = { cookie: false, phase: 'ready' };
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush(); advance(b, 4);
  assert.deepEqual(b.calls.slice(0, 2).map(c => c.url), ['/api/session','/api/redeem']);
  let release; server.startGate = new Promise(resolve => { release = resolve; });
  const starting = b.element('start').onclick(); b.element('start').onclick();
  assert.equal(b.element('start').disabled, true);
  assert.equal(b.calls.filter(c => c.url === '/api/start').length, 1);
  release(); await starting; await flush();
  assert.equal(current(b), 5);
  assert.equal(server.payload.admin_password, 'sample-password');
  assert.equal(server.payload.grafana, false);
  assert.deepEqual(Object.keys(server.payload).sort(), ['grafana','destination','server_name','storage_mode','storage_size','storage_class','nfs_server','nfs_path','access_mode','backend_port','admin_username','admin_email','admin_password','admin_confirm'].sort());
  assert.equal(JSON.stringify(server.payload.destination), '{"kind":"context","target_id":"opaque-local"}');
  assert.equal('cluster_mode' in server.payload, false);
  assert.equal(b.form.elements.admin_password.value, ''); assert.equal(b.form.elements.admin_confirm.value, '');
  server.phase = 'succeeded'; server.events = ['open: http://localhost']; server.result = 'Kubarr is ready';
  await b.timers.shift()();
  assert.equal(b.element('phase').textContent, 'Kubarr is ready');
  assert.equal(b.element('result').textContent, server.result);
   assert.equal(server.focusHistory.filter(id => id === 'title-6').length, 1);
  const reopened = page(server); await flush();
  assert.equal(reopened.calls.some(c => c.url === '/api/redeem'), false);
   assert.equal(current(reopened), 6); assert.equal(reopened.location.hash, '');
});

test('failed start allows retry if ready; interrupted connection reconnects to failure', async () => {
  const server = { cookie: true, phase: 'ready', startError: 'Bad request' };
  const b = page(server); await flush(); b.choose('destination_mode','single-node'); advance(b, 4);
  await b.element('start').onclick();
  assert.equal(current(b), 4); assert.equal(b.element('start').disabled, false);
  assert.match(b.element('error').textContent, /Bad request/);
  assert.equal(b.form.elements.admin_password.value, 'sample-password');
  server.startError = null; await b.element('start').onclick(); await flush();
  server.offline = true; await b.timers.shift()();
  assert.match(b.element('error').textContent, /Lost connection/);
  assert.equal(b.element('reconnect').classList.contains('hidden'), false);
  server.offline = false; server.phase = 'failed'; server.result = 'helm failed';
  await b.element('reconnect').onclick();
  assert.equal(b.element('phase').textContent, 'Installation failed');
  assert.equal(b.element('result').textContent, 'helm failed');
  assert.equal(b.element('error').textContent, '');
  const reopened = page(server); await flush();
  assert.equal(reopened.element('phase').textContent, 'Installation failed');
});

test('stale initial ready status cannot unlock an in-flight start or allow another POST', async () => {
  const initial = deferred(), start = deferred();
  const server = { cookie: true, phase: 'ready', statusGates: [initial.promise], startGate: start.promise };
  const b = page(server); await flush(); b.choose('destination_mode','single-node'); advance(b, 4);
  const starting = b.element('start').onclick();
  initial.resolve(); await flush();
  assert.equal(current(b), 4);
  assert.equal(b.element('start').disabled, true);
  assert.equal(b.element('error').textContent, '');
  b.element('start').onclick();
  assert.equal(b.calls.filter(c => c.url === '/api/start').length, 1);
  start.resolve(); await starting; await flush();
  assert.equal(current(b), 5);
});

test('failed POST retains backend validation details after deferred ready status', async () => {
  const start = deferred(), followup = deferred();
  const server = { cookie: true, phase: 'ready', startGate: start.promise, startError: 'Invalid NFS export path', statusGates: [] };
  const b = page(server); await flush(); b.choose('destination_mode','single-node'); advance(b, 4);
  server.statusGates.push(followup.promise);
  const starting = b.element('start').onclick();
  start.resolve(); await flush();
  assert.equal(b.element('start').disabled, true);
  followup.resolve(); await starting;
  assert.equal(current(b), 4);
  assert.equal(b.element('start').disabled, false);
  assert.match(b.element('error').textContent, /Invalid NFS export path/);
  assert.equal(b.calls.filter(c => c.url === '/api/start').length, 1);
});

test('focus moves on step transitions but progress polling leaves current focus alone', async () => {
  const server = { cookie: true, phase: 'running' };
  const b = page(server); await flush();
  assert.equal(server.focused, 'title-5');
  b.element('events').focus();
  const count = server.focusHistory.length;
  await b.timers.shift()(); await b.timers.shift()();
  assert.equal(server.focused, 'events');
  assert.equal(server.focusHistory.length, count);
});

test('verbose backend events render as exact text, with recent count and terminal access or failure intact', async () => {
  const lines = [
    '2026-10-12T12:00:00Z INFO [charts] component=openresty release=gateway workloadNamespace=openresty releaseNamespace=openresty start: install gateway',
    '2026-10-12T12:00:09Z WARN [wait] component=openresty release=gateway elapsed=9s: waiting for workload',
    '2026-10-12T12:00:11Z ERROR [command] exit=1 <img src=x onerror=alert(1)> & details',
  ];
  const server = {cookie:true,phase:'running',events:lines};
  const b = page(server); await flush();
  assert.equal(b.element('events').textContent, lines.join('\n'));
  assert.equal(b.element('events').children.length, 0);
  assert.equal(b.element('events').getAttribute('aria-live'), undefined); // Static markup opts out of bulk announcements.
  assert.equal(b.element('latest-activity').textContent, `Latest activity: ${lines.at(-1)}`);
  assert.equal(b.element('activity-count').textContent, 'Recent activity · 3 events (up to 500 retained)');
  assert.match(html, /id="events" role="log" aria-live="off"/);
  server.phase = 'succeeded'; server.result = 'Access dashboard at http://localhost:30080';
  await b.timers.shift()();
  assert.equal(b.element('phase').textContent, 'Kubarr is ready');
  assert.match(b.element('status-description').textContent, /Follow the access instructions/);
  assert.equal(b.element('result').textContent, server.result);
  assert.equal(b.element('events').textContent, lines.join('\n'));
  server.phase = 'failed'; server.result = 'Installation failed: inspect CLI output';
  await b.element('reconnect').onclick();
  assert.equal(b.element('phase').textContent, 'Installation failed');
  assert.equal(b.element('result').textContent, server.result);
  assert.equal(b.element('latest-activity').textContent, `Latest activity: ${lines.at(-1)}`);
});

test('empty activity reports actual state, not invented events', async () => {
  const server = {cookie:true,phase:'running',events:[]};
  const b = page(server); await flush();
  assert.equal(b.element('activity-count').textContent, 'Recent activity · 0 events (up to 500 retained)');
  assert.equal(b.element('latest-activity').textContent, 'Latest activity: Installing Kubarr…');
  server.phase = 'failed'; server.result = 'See CLI for details';
  await b.timers.shift()();
  assert.equal(b.element('latest-activity').textContent, 'Latest activity: Installation failed');
  assert.equal(b.element('result').textContent, server.result);
});

test('activity follows new lines until user inspects old ones, then resumes only on request', async () => {
  const server = {cookie:true,phase:'running',events:['a'.repeat(80)]};
  const b = page(server); await flush();
  const log = b.element('events'), button = b.element('follow-events');
  assert.equal(log.scrollTop, log.scrollHeight - log.clientHeight);
  server.events.push('b'.repeat(80)); await b.timers.shift()();
  assert.equal(log.scrollTop, log.scrollHeight - log.clientHeight);
  log.scrollTop = 0; log.onscroll();
  assert.equal(button.textContent, 'Resume follow');
  assert.equal(button.getAttribute('aria-pressed'), 'false');
  const frozen = log.textContent;
  server.events.push('c'.repeat(80)); await b.timers.shift()();
  assert.equal(log.scrollTop, 0);
  assert.equal(log.textContent, frozen);
  assert.match(b.element('activity-count').textContent, /3 events.*newer events available/);
  assert.equal(b.element('latest-activity').textContent, `Latest activity: ${server.events.at(-1)}`);
  button.onclick();
  assert.equal(button.getAttribute('aria-pressed'), 'true');
  assert.equal(log.textContent, server.events.join('\n'));
  assert.equal(log.scrollTop, log.scrollHeight - log.clientHeight);
  server.events.push('d'.repeat(80)); await b.timers.shift()();
  assert.equal(log.scrollTop, log.scrollHeight - log.clientHeight);
});

test('unchanged status polling leaves selection, focus, and scroll untouched', async () => {
  const server = {cookie:true,phase:'running',events:['a'.repeat(120)]};
  const b = page(server); await flush();
  const log = b.element('events');
  log.scrollTop = 12; log.focus();
  const focusCount = server.focusHistory.length;
  // Same backend history should not replace text or attempt a scroll, even before scroll notification.
  let writes = 0;
  const descriptor = Object.getOwnPropertyDescriptor(log, 'textContent');
  Object.defineProperty(log, 'textContent', {
    get() { return descriptor.get.call(this); },
    set(value) { writes++; descriptor.set.call(this, value); },
  });
  await b.timers.shift()();
  assert.equal(writes, 0);
  assert.equal(log.scrollTop, 12);
  assert.equal(server.focusHistory.length, focusCount);
  server.events.push('b'.repeat(120)); await b.timers.shift()();
  assert.equal(log.scrollTop, 12);
   assert.equal(b.element('follow-events').textContent, 'Resume follow');
   assert.equal(server.focused, 'events');
 });

test('paused log freezes through rolling 500-event snapshots and terminal updates; resume shows latest bounded window', async () => {
  const initial = Array.from({length:500}, (_, i) => `2026-10-12 INFO stage=install event-${i}`);
  const server = {cookie:true,phase:'running',events:initial};
  const b = page(server); await flush();
  const log = b.element('events');
  log.scrollTop = 150;
  log.onscroll();
  assert.equal(b.element('follow-events').textContent, 'Resume follow');
  log.focus(); const focusCount = server.focusHistory.length;
  const frozen = log.textContent;
  for (let n = 0; n < 3; n++) {
    server.events = [...server.events.slice(1), `2026-10-12 WARN stage=wait event-${500 + n}`];
    await b.timers.shift()();
    assert.equal(log.textContent, frozen);
    assert.equal(log.scrollTop, 150);
    assert.equal(server.focusHistory.length, focusCount);
    assert.equal(b.element('latest-activity').textContent, `Latest activity: ${server.events.at(-1)}`);
    assert.equal(b.element('activity-count').textContent, 'Recent activity · 500 events (up to 500 retained) · newer events available');
  }
   server.phase = 'succeeded'; server.result = 'Open http://localhost:30080';
   await b.timers.shift()();
   assert.equal(current(b),6);
   assert.equal(server.focusHistory.length,focusCount + 1);
   assert.equal(log.textContent, frozen);
  assert.equal(b.element('phase').textContent, 'Kubarr is ready');
  assert.equal(b.element('result').textContent, server.result);
  // Repeated identical terminal snapshots do not disturb the inspected window.
  await b.element('reconnect').onclick();
  assert.equal(log.textContent, frozen);
  assert.equal(log.scrollTop, 150);
   assert.equal(server.focusHistory.length, focusCount + 1);
  b.element('follow-events').onclick();
   assert.equal(log.textContent, server.events.join('\n'));
   assert.equal(log.scrollTop, log.scrollHeight - log.clientHeight);
   assert.equal(server.focused, 'title-6');
});

test('uncertain start stays locked until status confirms whether retry is safe', async () => {
  const server = { cookie: true, phase: 'ready', startError: 'Gateway unavailable', offline: false };
  const b = page(server); await flush(); b.choose('destination_mode','single-node'); advance(b, 4);
  // The POST fails, and the follow-up status lookup also loses connection.
  server.offline = true;
  await b.element('start').onclick();
  assert.equal(current(b), 5);
  assert.equal(b.element('start').disabled, true);
  assert.equal(b.element('reconnect').classList.contains('hidden'), false);
  server.offline = false;
  await b.element('reconnect').onclick();
  assert.equal(current(b), 4);
  assert.equal(b.element('start').disabled, false);
  assert.equal(b.form.elements.admin_password.value, 'sample-password');
});

test('reloading without a cookie cannot reuse the one-use invitation', async () => {
  const server = { cookie: false, phase: 'ready' };
  page(server); await flush();
  server.cookie = false;
  const b = page(server); await flush();
  assert.match(b.element('auth').textContent, /expired or is invalid/);
  assert.equal(b.element('wizard').classList.contains('hidden'), true);
});

test('initial status outage has a visible reconnect path and returns to setup', async () => {
  const server = { cookie: true, phase: 'ready', offline: true };
  const b = page(server); await flush();
  assert.equal(current(b), 5);
  assert.equal(b.element('reconnect').classList.contains('hidden'), false);
  server.offline = false; await b.element('reconnect').onclick();
  assert.equal(current(b), 0);
  assert.equal(b.element('error').textContent, '');
});

test('expired invitation without cookie fails closed', async () => {
  const b = page({ cookie: false, redeemed: true, phase: 'ready' }); await flush();
  assert.match(b.element('auth').textContent, /expired or is invalid/);
  assert.equal(b.element('wizard').classList.contains('hidden'), true);
  assert.equal(b.location.hash, '');
});

test('discovery renders all sources as selectable tiles, distinguishes duplicate labels and never auto-selects current', async () => {
  const extra = { id:'second', source:'env', label:'Work cluster', context:'work', current:false };
  const other = { id:'third', source:'local', label:'Local', context:'home', current:false };
  const k3s = { id:'fourth', source:'k3s', label:'Local', context:'home', current:false };
  const b = page({ cookie:true, phase:'ready', targets:[localTarget, extra, other, k3s] }); await flush();
  const tiles = b.element('context-tiles').children;
  assert.equal(tiles.length, 4);
   assert.equal([...tiles].filter(tile => tile.firstChild.checked).length, 0);
  assert.match(tiles[0].textContent, /Work cluster · work.*Default kubeconfig.*Current/);
  assert.match(tiles[1].textContent, /Work cluster · work.*Environment kubeconfig/);
  assert.match(tiles[2].textContent, /Local · home.*Local kubeconfig/);
  assert.match(tiles[3].textContent, /Local · home.*Local k3s kubeconfig/);
  assert.equal(b.element('next').disabled, true);
  b.element('next').onclick(); assert.equal(current(b), 0);
  b.selectTarget('second'); await flush(); assert.equal(b.element('next').disabled, false);
  assert.equal(b.calls.find(c => c.url === '/api/checks').options.method, 'POST');
  b.selectTarget('opaque-local'); await flush(); assert.equal(b.calls.filter(c => c.url === '/api/checks').length, 2);
  b.selectTarget('second'); await flush(); advance(b,4);
  assert.match(b.element('summary').textContent, /Environment kubeconfig.*Work cluster · work/);
  await b.element('start').onclick();
  assert.equal(JSON.parse(b.calls.find(call => call.url === '/api/start').options.body).destination.target_id, 'second');
});

test('empty and failed discovery keep action tiles available', async () => {
  const empty = page({ cookie:true, phase:'ready', targets:[] }); await flush();
  assert.equal(empty.element('context-tiles').children.length, 0);
  assert.match(empty.element('local-message').textContent, /No local/);
  empty.choose('destination_mode','single-node'); advance(empty,4);
  await empty.element('start').onclick(); assert.equal(current(empty),5);
  const failed = page({ cookie:true, phase:'ready', targetsError:'unavailable' }); await flush();
  assert.equal(failed.element('context-tiles').children.length, 0);
  assert.match(failed.element('local-message').textContent, /Could not discover/);
  failed.choose('destination_mode','single-node'); assert.equal(failed.element('next').disabled,false);
});

test('untrusted context names remain text and selecting an uploaded tile hides upload credentials', async () => {
  const hostile = {id:'opaque<&>',source:'env',label:'<img src=x onerror=alert(1)>',context:'<script>bad</script>',current:false};
  const b = page({cookie:true,phase:'ready',targets:[hostile]}); await flush();
  const tile = b.element('context-tiles').firstChild;
  assert.equal(tile.children[2].children[0].textContent, '<img src=x onerror=alert(1)> · <script>bad</script>');
  assert.equal(tile.children.length, 3); // No parsed markup was appended.
  b.selectTarget('opaque<&>'); await flush();
  assert.equal(b.calls.some(c => c.url.startsWith('/api/checks/')), true);
  b.choose('destination_mode','upload');
  assert.equal(b.element('upload-options').classList.contains('hidden'), false);
  const upload = {id:'private',source:'upload',label:'Private',context:'secret',current:true};
  // This upload returns an additional tile, without selecting it automatically.
  const server = {cookie:true,phase:'ready',targets:[],uploadTargets:[upload]};
  const uploaded = page(server); await flush(); uploaded.choose('destination_mode','upload');
  await uploaded.upload({size:6,text:async () => 'secret'});
  assert.match(uploaded.element('context-tiles').textContent,/Private · secret.*Uploaded kubeconfig.*Current/);
  assert.equal(uploaded.element('next').disabled,true);
  uploaded.selectTarget('private');
  assert.equal(uploaded.element('upload-options').classList.contains('hidden'),true);
});

test('discovered local k3s kubeconfig is an existing context, not a new installation', async () => {
  const k3s = {id:'opaque-k3s',source:'k3s',label:'Local cluster',context:'default',current:true};
  const server = {cookie:true,phase:'ready',targets:[k3s]};
  const b = page(server); await flush();
  assert.equal(b.element('context-tiles').children.length,1);
  assert.match(b.element('context-tiles').textContent,/Local cluster · default/);
  assert.equal(b.element('local-message').textContent,'');
  assert.equal(b.element('next').disabled,true);
  b.selectTarget('opaque-k3s'); await flush();
  assert.equal(b.element('next').disabled,false);
  assert.equal(JSON.parse(b.calls.find(c => c.url === '/api/checks').options.body).target_id,'opaque-k3s');
  advance(b,4);
  assert.equal(current(b),4);
  assert.match(b.element('summary').textContent,/Existing Kubernetes context.*Local k3s kubeconfig.*Local cluster · default/);
  assert.doesNotMatch(b.element('summary').textContent,/Local k3s · this machine/);
  await b.element('start').onclick();
  assert.equal(JSON.stringify(server.payload.destination),'{"kind":"context","target_id":"opaque-k3s"}');
});

test('upload validates size, rejection, selects returned context, and never includes credentials in review or start', async () => {
  const server = { cookie:true, phase:'ready', targets:[], uploadTargets:[{id:'upload-id',source:'upload',label:'Private',context:'prod',current:false}] };
  const b = page(server); await flush(); b.choose('destination_mode','upload');
  const oversized = {size:262145, text:async () => 'secret'};
  await b.upload(oversized);
  assert.match(b.element('upload-message').textContent, /256 KiB/);
  assert.equal(b.calls.some(c => c.url === '/api/targets/upload'),false);
  server.uploadError = 'Unsupported exec credential';
  await b.upload({size:20,text:async () => 'bad'});
  assert.match(b.element('upload-message').textContent, /Upload failed.*embedded token or client certificate\/key.*no exec.*HTTPS/);
  server.uploadError = null;
  await b.upload({size:20,text:async () => 'embedded-token-secret'});
  assert.equal(b.element('kubeconfig_file').value,'');
  assert.equal(b.element('next').disabled,true);
  assert.match(b.element('context-tiles').textContent, /Private · prod.*Uploaded kubeconfig/);
  b.selectTarget('upload-id'); await flush(); advance(b,4);
  assert.match(b.element('summary').textContent,/Uploaded kubeconfig.*Private · prod/);
  assert.doesNotMatch(b.element('summary').textContent,/embedded-token-secret|upload-id/);
  await b.element('start').onclick();
  assert.equal(JSON.stringify(server.payload.destination),'{"kind":"context","target_id":"upload-id"}');
  assert.doesNotMatch(JSON.stringify(server.payload),/embedded-token-secret|kubeconfig|cluster_mode/);
});

test('pending upload blocks forward and duplicate uploads; stale upload and preflight cannot replace new selection', async () => {
  const upload = deferred(), oldCheck = deferred();
  const uploaded = {id:'retained',source:'upload',label:'Retained',context:'private',current:false};
  const server = { cookie:true, phase:'ready', uploadGate:upload.promise, uploadTargets:[uploaded], preflightGates:[] };
  const b = page(server); await flush();
  b.choose('destination_mode','upload');
  const first = b.upload({size:6,text:async () => 'secret'});
  b.upload({size:6,text:async () => 'second'});
  await flush();
  assert.equal(b.element('next').disabled,true);
  assert.equal(b.calls.filter(c => c.url === '/api/targets/upload').length,1);
  b.choose('destination_mode','single-node');
  upload.resolve(); await first;
  assert.equal(b.element('context-tiles').children.length,2);
  assert.equal(b.element('next').disabled,false);
  b.choose('destination_mode','upload'); b.selectTarget('retained'); await flush();
  assert.equal(b.element('next').disabled,false);
  b.selectTarget('opaque-local');
  server.preflightGates.push(oldCheck.promise);
  b.element('refresh').onclick();
  b.choose('destination_mode','single-node'); await flush();
  oldCheck.resolve(); await flush();
  assert.match(b.element('checks').textContent,/Not detected/);
});

test('successive uploads merge opaque IDs; both contexts remain selectable and reload discovery restores them', async () => {
  const first = {id:'upload-one',source:'upload',label:'North',context:'north',current:true};
  const second = {id:'upload-two',source:'upload',label:'South',context:'south',current:false};
  const server = {cookie:true,phase:'ready',targets:[localTarget],uploadResponses:[[first],[second]]};
  const b = page(server); await flush(); b.choose('destination_mode','upload');
  await b.upload({size:14,text:async () => 'first-secret'});
  b.selectTarget('upload-one'); await flush();
  b.choose('destination_mode','upload');
  await b.upload({size:15,text:async () => 'second-secret'});
  assert.equal(b.element('context-tiles').children.length,3);
   assert.equal([...b.element('context-tiles').children].some(tile => tile.firstChild.checked),false);
  b.selectTarget('upload-two'); await flush(); advance(b,4);
  assert.match(b.element('summary').textContent,/Uploaded kubeconfig.*South · south/);
  assert.doesNotMatch(b.element('summary').textContent,/first-secret|second-secret|upload-two/);
  await b.element('start').onclick();
  assert.equal(JSON.stringify(server.payload.destination),'{"kind":"context","target_id":"upload-two"}');
  assert.doesNotMatch(JSON.stringify(server.payload),/first-secret|second-secret|kubeconfig/);
  const reload = page({cookie:true,phase:'ready',targets:[localTarget,first,second]}); await flush();
  assert.equal(reload.element('context-tiles').children.length,3);
  reload.selectTarget('upload-one'); await flush(); advance(reload,4);
  assert.match(reload.element('summary').textContent,/North · north/);
});

test('live prerequisite rows use real states, counts and advisory eligibility', async () => {
  const server = {cookie:true,phase:'ready',checkSnapshots:[
    {phase:'running',eligible:false,checks:[{id:'a',label:'Access',severity:'required',state:'passed',message:'Connected'}, {id:'b',label:'Storage',severity:'required',state:'running',message:'Inspecting'}, {id:'c',label:'Capacity',severity:'advisory',state:'pending',message:''}]},
    {phase:'complete',eligible:true,checks:[{id:'a',label:'Access',severity:'required',state:'passed',message:'Connected'}, {id:'b',label:'Storage',severity:'required',state:'passed',message:'Ready'}, {id:'c',label:'Capacity',severity:'advisory',state:'warning',message:'Low capacity'}]}
  ]};
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush();
  assert.equal(b.element('next').disabled,true);
  assert.match(b.element('checks').textContent,/1 of 3.*Checking Storage/);
  assert.equal(b.element('checks').children[5].open,false);
  assert.equal(b.element('checks').children[1].getAttribute('aria-valuenow'),'1');
  assert.equal(b.timers[0].delay,650);
  await b.timers.shift()();
  assert.equal(b.element('checks').children[1].getAttribute('aria-valuenow'),'3');
  assert.match(b.element('checks').textContent,/3 of 3.*warning.*Capacity.*Baseline checks passed with advisory warnings.*installation rechecks prerequisites/);
  assert.equal(b.element('next').disabled,false);
   assert.match(b.element('context-tiles').textContent,/Baseline passed/);
  assert.match(html,/prefers-reduced-motion: reduce/);
 });

test('native-style live collections support discovery, selection, badges and polling without array methods', async () => {
  const server = {cookie:true,phase:'ready',targets:[localTarget,{id:'other',source:'env',label:'Other',context:'prod',current:false}],checkSnapshots:[
    {phase:'running',eligible:false,checks:[{id:'access',label:'Access',severity:'required',state:'running',message:'Connecting'}]},
    {phase:'complete',eligible:true,checks:[{id:'access',label:'Access',severity:'required',state:'passed',message:'Connected'}]},
  ]};
  const b = page(server); await flush();
  const tiles = b.element('context-tiles').children;
  assert.equal(typeof tiles.find, 'undefined');
  assert.equal(typeof tiles.map, 'undefined');
  assert.equal(typeof tiles.filter, 'undefined');
  assert.equal(tiles.item(0),tiles[0]);
  b.selectTarget('opaque-local'); await flush();
  assert.match(tiles[0].textContent,/Checking/);
  assert.equal(b.element('next').disabled,true);
  await b.timers.shift()();
  assert.match(tiles[0].textContent,/Baseline passed/);
  assert.equal(b.element('next').disabled,false);
  advance(b,4);
  assert.equal(current(b),4);
  assert.equal(b.calls.some(c => c.url === '/api/start'),false);
});

test('large snapshots stay condensed, expose live group totals, issues and optional GPU warning', async () => {
  const checks = [
    {id:'kubectl',label:'kubectl',severity:'required',state:'passed',message:'Found'},
    {id:'helm',label:'Helm',severity:'required',state:'passed',message:'Found'},
    {id:'api',label:'Kubernetes API',severity:'required',state:'passed',message:'Ready'},
    ...Array.from({length:9},(_,i) => ({id:`resource-${i}`,label:`API ${i}`,severity:'required',state:i < 2 ? 'failed' : 'passed',message:'Unavailable'})),
    ...Array.from({length:10},(_,i) => ({id:`permission-${i}`,label:`Permission ${i}`,severity:'required',state:'passed',message:'Allowed'})),
    {id:'storage',label:'Storage classes',severity:'advisory',state:'passed',message:'Ready'},
    {id:'metrics',label:'Metrics API',severity:'advisory',state:'warning',message:'Unavailable'},
    {id:'gpu',label:'GPU support',severity:'advisory',state:'warning',message:'No GPU detected'},
    {id:'future-check',label:'Future check',severity:'advisory',state:'passed',message:'OK'},
  ];
  const server = {cookie:true,phase:'ready',checkSnapshots:[
    {phase:'running',eligible:false,checks:checks.map(c => c.id === 'gpu' ? {...c,state:'running'} : c)},
    {phase:'complete',eligible:false,checks},
  ]};
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush();
  const card = b.element('checks');
  const [header,track,groups,issues,gpu,details] = card.children;
  assert.equal(groups.children.length,7);
  assert.match(groups.textContent,/APIs7\/9 passed · 2 failed.*Permissions10\/10 passed.*Other checks1\/1 passed/);
  assert.equal(details.open,false);
  assert.equal(details.children[1].children.length,checks.length);
  assert.equal(issues.children.length,2);
  assert.match(issues.textContent,/APIs · 2 failed.*Storage & metrics · 1 warning/);
  assert.equal(gpu.classList.contains('hidden'),true);
  assert.match(header.textContent,/Checking GPU support/);
  assert.equal(track.getAttribute('aria-valuenow'),String(checks.length - 1));
  details.open = true; issues.firstChild.open = true;
  const firstRow = details.children[1].firstChild, icon = firstRow.firstChild, fill = track.firstChild;
  b.element('refresh').focus(); const focused = server.focused;
  await b.timers.shift()();
  assert.equal(details.open,true);
  assert.equal(issues.firstChild.open,true);
  assert.equal(details.children[1].firstChild,firstRow);
  assert.equal(firstRow.firstChild,icon);
  assert.equal(track.firstChild,fill);
  assert.equal(server.focused,focused);
  assert.match(gpu.textContent,/GPU support · No GPU detected.*Optional/);
  assert.equal(gpu.classList.contains('hidden'),false);
  assert.equal(b.element('next').disabled,true); // The two required API failures still block.
  assert.equal(track.getAttribute('aria-valuenow'),String(checks.length));
  assert.equal(issues.firstChild.children[1].children.length,2);
});

test('GPU advisory alone is visible without opening details, remains eligible and clears when passed', async () => {
  const gpu = state => ({id:'gpu',label:'GPU support',severity:'advisory',state,message:state === 'warning' ? 'No GPU detected' : 'Available'});
  const server = {cookie:true,phase:'ready',checkSnapshots:[
    {phase:'running',eligible:false,checks:[gpu('warning')]},
    {phase:'complete',eligible:true,checks:[gpu('warning')]},
    {phase:'complete',eligible:true,checks:[gpu('passed')]},
  ]};
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush();
  const card = b.element('checks');
  assert.match(card.children[4].textContent,/No GPU detected/);
  assert.equal(card.children[5].open,false);
  await b.timers.shift()();
  assert.equal(b.element('next').disabled,false);
  assert.match(card.textContent,/Baseline checks passed with advisory warnings/);
  // A fresh snapshot of the same session updates the persistent GPU callout.
  server.checkSnapshots.unshift({phase:'complete',eligible:true,checks:[gpu('passed')]});
  const poll = b.calls.find(c => c.url.startsWith('/api/checks/'));
  assert.ok(poll);
  b.element('refresh').onclick(); await flush();
  assert.equal(card.children[4].classList.contains('hidden'),true);
  assert.equal(b.element('next').disabled,false);
  assert.match(html,/@media \(prefers-reduced-motion: reduce\) \{[^}]*animation: none; \} \.check-track span \{ transition: none;/);
});

test('polls retain keyed rows, status and progress nodes; only actual state changes reset animation', async () => {
  const access = (state,message) => ({id:'access',label:'Access',severity:'required',state,message});
  const storage = (state,message) => ({id:'storage',label:'Storage',severity:'required',state,message});
  const server = {cookie:true,phase:'ready',checkSnapshots:[
    {phase:'running',eligible:false,checks:[access('running','Connecting'),storage('pending','')]},
    {phase:'running',eligible:false,checks:[access('running','Still connecting'),storage('pending','')]},
    {phase:'running',eligible:false,checks:[access('passed','Connected'),storage('running','Inspecting')]},
    {phase:'complete',eligible:true,checks:[access('passed','Connected'),storage('passed','Ready')]},
  ]};
  const b = page(server); await flush(); b.selectTarget('opaque-local');
  const card = b.element('checks');
  assert.equal(card.children[0].children[0].className,'check-ring running');
  assert.equal(card.children[0].children[0].textContent,'·');
  await flush();
  const [header,track,groups,issues,gpu,details,summary] = card.children;
  const rows = details.children[1];
  const count = header.children[1].children[1];
  const fill = track.firstChild;
  const [first,second] = rows.children;
  const icon = first.firstChild;
  assert.equal(fill.style.width,'0%');
  assert.equal(track.getAttribute('aria-valuenow'),'0');
  assert.equal(count.getAttribute('aria-live'),'polite');
  await b.timers.shift()();
  assert.equal(card.children[0],header);
  assert.equal(header.children[1].children[1],count);
  assert.equal(track.firstChild,fill);
  assert.equal(rows.children[0],first);
  assert.equal(rows.children[1],second);
  assert.equal(first.firstChild,icon);
  assert.equal(first.className,'check-row running');
  assert.match(first.textContent,/Still connecting/);
  assert.equal(fill.style.width,'0%');
  await b.timers.shift()();
  assert.equal(rows.children[0],first);
  assert.equal(first.className,'check-row passed');
  assert.equal(second.className,'check-row running');
  assert.equal(fill.style.width,'50%');
  assert.equal(track.getAttribute('aria-valuenow'),'1');
  await b.timers.shift()();
  assert.equal(rows.children[0],first);
  assert.equal(rows.children[1],second);
  assert.equal(first.firstChild,icon);
  assert.equal(first.className,'check-row passed');
  assert.equal(fill.style.width,'100%');
  assert.equal(track.getAttribute('aria-valuenow'),'2');
  assert.equal(card.children[6],summary);
  assert.match(summary.textContent,/Baseline checks passed.*installation rechecks prerequisites/);
  assert.equal(server.focused,undefined);
  assert.match(html,/\.check-track span \{[^}]*transition: width \.35s ease/);
  assert.match(html,/@media \(prefers-reduced-motion: reduce\) \{[^}]*animation: none; \} \.check-track span \{ transition: none;/);
});

test('required failure prevents navigation and refresh retries after request error', async () => {
  const server = {cookie:true,phase:'ready',checkSnapshots:[{phase:'complete',eligible:false,checks:[{id:'a',label:'Storage',severity:'required',state:'failed',message:'No default StorageClass'}]}]};
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush();
  assert.equal(b.element('next').disabled,true); b.element('next').onclick(); assert.equal(current(b),0);
  assert.match(b.element('checks').textContent,/Needs attention.*Storage.*failed.*No default StorageClass/);
  server.checkStartError = 'Temporarily unavailable'; b.element('refresh').onclick(); await flush();
  assert.match(b.element('checks').textContent,/Temporarily unavailable.*retry/);
  server.checkStartError = null; b.element('refresh').onclick(); await flush();
  assert.equal(b.element('next').disabled,false);
  server.checkError = 'Network error'; b.element('refresh').onclick(); await flush();
  assert.equal(b.element('next').disabled,true); assert.match(b.element('checks').textContent,/Network error/);
});

test('switching destinations cancels the matching session; stale GET and late POST cannot unlock another tile', async () => {
  const oldGet = deferred(), latePost = deferred();
  const other = {id:'other',source:'env',label:'Other',context:'prod',current:false};
  const server = {cookie:true,phase:'ready',targets:[localTarget,other],checkGates:[oldGet.promise]};
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush();
  b.selectTarget('other'); await flush();
  assert.deepEqual(server.cancelled,['check-1']);
  oldGet.resolve(); await flush(); assert.equal(b.element('next').disabled,false);
  server.checkStartGates = [latePost.promise]; b.element('refresh').onclick(); await flush();
  b.choose('destination_mode','upload'); b.selectTarget('opaque-local'); await flush();
  assert.equal(b.element('next').disabled,true);
  latePost.resolve(); await flush();
  assert.ok(server.cancelled.includes('check-3'));
  assert.equal(b.element('next').disabled,false);
  assert.equal(server.sessions.at(-1).target_id,'opaque-local');
});

test('expired eligibility blocks review/start, and backend 409 routes to retry without losing detail', async () => {
  const server = {cookie:true,phase:'ready'};
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush(); advance(b,4);
  assert.equal(current(b),4);
  b.tick(5 * 60 * 1000 + 1);
  b.element('start').onclick(); assert.equal(current(b),0);
  assert.equal(b.calls.some(c => c.url === '/api/start'),false);
  assert.match(b.element('error').textContent,/fresh prerequisite checks/);
  b.element('refresh').onclick(); await flush(); advance(b,4);
  server.startStatus = 409; server.startError = 'Prerequisites expired for selected context';
  await b.element('start').onclick();
  assert.equal(current(b),0);
  assert.match(b.element('error').textContent,/Prerequisites expired.*refresh checks/);
  assert.equal(b.element('next').disabled,true);
});

test('local k3s shows deferred cluster checks and reopened installation never creates check session', async () => {
  const b = page({cookie:true,phase:'ready'}); await flush(); b.choose('destination_mode','single-node'); await flush();
  assert.match(b.element('checks').textContent,/deferred until local k3s is created/);
  assert.equal(b.calls.some(c => c.url === '/api/checks'),false);
  assert.equal(b.element('next').disabled,false);
  const running = page({cookie:true,phase:'running'}); await flush();
  assert.equal(current(running),5);
  assert.equal(running.calls.some(c => c.url === '/api/checks'),false);
});

async function externalPage(server = {cookie:true,phase:'ready'}) {
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush();
  b.element('next').onclick(); b.element('next').onclick(); b.choose('storage_mode','external-nfs');
  b.edit('nfs_server',' nas.local '); b.edit('nfs_path',' /media ');
  return b;
}

test('external Continue validates first, polls real progress, blocks doubles, then advances only after cleanup', async () => {
  const server = {cookie:true,phase:'ready',storageSnapshots:[
    {phase:'running',eligible:false,checks:[{id:'write',label:'Write',severity:'required',state:'running',message:'Mounting'}]},
    {phase:'complete',eligible:true,checks:[{id:'write',label:'Write',severity:'required',state:'passed',message:'Read back'}, {id:'cleanup',label:'Cleanup',severity:'required',state:'passed',message:'Removed'}]},
  ]};
  const b = await externalPage(server);
  b.edit('nfs_server','bad server'); b.element('next').onclick(); assert.equal(current(b),2);
  assert.equal(server.storageSessions,undefined);
  b.edit('nfs_server',' nas.local '); b.element('next').onclick(); b.element('next').onclick(); await flush();
  assert.equal(current(b),2); assert.equal(b.element('next').disabled,true);
  assert.deepEqual(server.storageSessions[0],{id:'storage-1',target_id:'opaque-local',server:'nas.local',path:'/media'});
  assert.match(b.element('storage-check').textContent,/ephemeral pod.*unique NFSv4.1 test file.*Existing data is not touched.*Mounting/);
  assert.equal(b.element('storage-check').querySelector('.check-track').getAttribute('aria-valuenow'),'0');
  assert.equal(b.calls.filter(c => c.url === '/api/storage-checks').length,1);
  await b.timers.shift()(); assert.equal(current(b),3);
  assert.match(b.element('storage-check').textContent,/Cleanup: passed.*removed|Cleanup: passed.*Removed/);
  advance(b,1); await b.element('start').onclick();
  assert.equal(server.payload.nfs_server,'nas.local'); assert.equal(server.payload.nfs_path,'/media');
});

test('NFS uses the node card styling with live five-stage groups, keyed details and stable focus', async () => {
  const stage = (id,label,state,message = '') => ({id,label,severity:'required',state,message});
  const snapshot = (write,cleanup,writeMessage = 'Writing') => ({phase:'running',eligible:false,checks:[
    stage('node','Select node','passed','Ready'), stage('resources','Temporary volume','passed','Created'),
    stage('mount','Mount NFS','passed','Mounted'),stage('write','Read/write',write,writeMessage),
    stage('cleanup','Cleanup',cleanup,'Removing temporary resources'),
  ]});
  const server = {cookie:true,phase:'ready',storageSnapshots:[snapshot('running','pending'),snapshot('running','pending','Still writing'),snapshot('passed','running','Verified'),
    {phase:'complete',eligible:true,checks:snapshot('passed','passed','Verified').checks.map(c => c.id === 'cleanup' ? {...c,message:'Removed'} : c)}]};
  const b = await externalPage(server);
  const card = b.element('storage-check');
  assert.equal(card.classList.contains('check-card'),true);
  assert.equal(card.classList.contains('storage-card'),true);
  assert.equal(card.classList.contains('storage-check'),false);
  assert.match(html,/\.storage-card \{ margin-top: 14px; \}/);
  assert.match(html,/\.check-card \{[^}]*var\(--surface\)/);
  assert.match(html,/@media \(prefers-reduced-motion: reduce\)/);
  assert.equal(server.storageSessions,undefined); // Continue, not an extra button, starts the check.
  b.element('next').onclick(); await flush();
  const [header,track,groups,issues,details] = card.children;
  assert.equal(header.className,'check-header'); assert.equal(header.firstChild.className,'check-ring running');
  const count = header.children[1].children[1], fill = track.firstChild, rows = details.children[1];
  assert.equal(count.getAttribute('aria-live'),'polite');
  assert.match(count.textContent,/3 of 5 stages complete · Checking Read\/write/);
  assert.equal(track.getAttribute('role'),'progressbar'); assert.equal(track.getAttribute('aria-valuemax'),'5');
  assert.equal(track.getAttribute('aria-valuenow'),'3'); assert.equal(fill.style.width,'60%');
  assert.match(groups.textContent,/Connection \/ mount3\/3 passed.*Read \/ write0\/1 passed · 1 running.*Cleanup0\/1 passed/);
  assert.equal(groups.children.length,3); assert.equal(details.open,false);
  assert.equal(rows.children.length,5); assert.equal(card.querySelector('.check-issues'),issues);
  assert.equal(card.querySelector('.check-details'),details);
  assert.equal(card.querySelector('.check-ring'),header.firstChild);
  assert.equal(card.querySelector('.check-group'),groups.firstChild);
  assert.doesNotMatch(card.textContent,/GPU support/);
  details.open = true; details.firstChild.focus(); const focused = server.focused;
  const firstRow = rows.firstChild, writeRow = rows.children[3], writeIcon = writeRow.firstChild;
  await b.timers.shift()();
  assert.equal(current(b),2); assert.equal(details.open,true); assert.equal(server.focused,focused);
  assert.equal(card.children[0],header); assert.equal(header.children[1].children[1],count);
  assert.equal(track.firstChild,fill); assert.equal(rows.firstChild,firstRow);
  assert.equal(rows.children[3],writeRow); assert.equal(writeRow.firstChild,writeIcon);
  assert.equal(writeRow.className,'check-row running'); assert.equal(fill.style.width,'60%');
  await b.timers.shift()();
  assert.equal(current(b),2); assert.equal(track.getAttribute('aria-valuenow'),'4'); assert.equal(fill.style.width,'80%');
  assert.equal(rows.children[3],writeRow); assert.equal(writeRow.className,'check-row passed');
  await b.timers.shift()();
  assert.equal(current(b),3); assert.equal(track.getAttribute('aria-valuenow'),'5'); assert.equal(fill.style.width,'100%');
  assert.equal(details.open,true); assert.equal(rows.children[3],writeRow);
  assert.equal(header.firstChild.className,'check-ring');
});

test('unconfirmed NFS cleanup is visible while collapsed, and retry alone can advance after success', async () => {
  const checks = ['node','resources','mount','write','cleanup'].map(id => ({id,label:id === 'cleanup' ? 'Cleanup' : id,severity:'required',state:id === 'cleanup' ? 'running' : 'passed',message:id === 'cleanup' ? 'Deletion not confirmed' : 'OK'}));
  const server = {cookie:true,phase:'ready',storageSnapshots:[
    {phase:'complete',eligible:true,checks},
    {phase:'complete',eligible:true,checks:checks.map(c => ({...c,state:'passed',message:'Confirmed'}))},
  ]};
  const b = await externalPage(server); b.element('next').onclick(); await flush();
  const card = b.element('storage-check');
  assert.equal(current(b),2); assert.equal(card.querySelector('.check-details').open,false);
  assert.match(card.querySelector('.check-issues').textContent,/Cleanup: running · Deletion not confirmed/);
  assert.equal(card.querySelector('.check-track').getAttribute('aria-valuenow'),'4');
  assert.match(card.textContent,/Cleanup or another required check was not confirmed/);
  const retry = card.children[7]; assert.equal(retry.classList.contains('hidden'),false);
  retry.onclick(); await flush(); assert.equal(current(b),3); assert.equal(server.storageSessions.length,2);
  assert.equal(retry.classList.contains('hidden'),true);
});

test('a complete eligible response without a confirmed cleanup cannot advance', async () => {
  const server = {cookie:true,phase:'ready',storageSnapshots:[
    {phase:'complete',eligible:true,checks:[{id:'write',label:'Read/write',severity:'required',state:'passed',message:'Verified'}]},
  ]};
  const b = await externalPage(server); b.element('next').onclick(); await flush();
  assert.equal(current(b),2);
  assert.match(b.element('storage-check').textContent,/Cleanup or another required check was not confirmed/);
  assert.equal(b.element('storage-check').querySelector('.check-track').getAttribute('aria-valuenow'),'1');
});

test('failed checks and unconfirmed cleanup stay on storage with actionable retry', async () => {
  const server = {cookie:true,phase:'ready',storageSnapshots:[
    {phase:'complete',eligible:false,checks:[{id:'write',label:'Write',severity:'required',state:'failed',message:'Permission denied'}]},
    {phase:'complete',eligible:true,checks:[{id:'cleanup',label:'Cleanup',severity:'required',state:'failed',message:'Could not remove test file'}]},
  ]};
  const b = await externalPage(server);
  b.element('next').onclick(); await flush(); assert.equal(current(b),2);
  assert.match(b.element('error').textContent,/Permission denied.*retry/);
  b.element('next').onclick(); await flush(); assert.equal(current(b),2);
  assert.match(b.element('error').textContent,/Could not remove test file/);
  b.element('next').onclick(); await flush(); assert.equal(current(b),3);
});

test('edits and destination changes cancel matching session; stale GET and late POST cannot advance', async () => {
  const oldGet = deferred(), latePost = deferred();
  const server = {cookie:true,phase:'ready',storageGates:[oldGet.promise]};
  const b = await externalPage(server);
  b.element('next').onclick(); await flush();
  b.edit('nfs_path','/other'); assert.deepEqual(server.storageCancelled,['storage-1']);
  b.element('next').onclick(); await flush(); oldGet.resolve(); await flush(); assert.equal(current(b),3);
  b.element('back').onclick(); server.storageStartGates = [latePost.promise]; b.edit('nfs_path','/third'); b.element('next').onclick(); await flush();
  b.choose('storage_mode','managed-nfs'); b.choose('storage_mode','external-nfs'); b.element('next').onclick();
  latePost.resolve(); await flush();
  assert.ok(server.storageCancelled.includes('storage-3')); assert.equal(current(b),3);
});

test('back while running cancels navigation intent; sidebar cannot bypass storage; TTL requires rerun', async () => {
  const wait = deferred(); const server = {cookie:true,phase:'ready',storageGates:[wait.promise]};
  const b = await externalPage(server); b.element('next').onclick(); await flush();
  b.element('back').onclick(); assert.equal(current(b),1); assert.deepEqual(server.storageCancelled,['storage-1']);
  wait.resolve(); await flush(); assert.equal(current(b),1);
  b.element('next').onclick(); b.steps[4].onclick(); assert.equal(current(b),2);
  b.element('next').onclick(); await flush(); assert.equal(current(b),3);
  b.element('next').onclick(); b.tick(300001); b.element('refresh').onclick(); await flush(); b.element('start').onclick();
  assert.equal(current(b),2); assert.equal(server.payload,undefined);
  b.element('next').onclick(); await flush(); assert.equal(current(b),3);
});

test('managed and new k3s bypass live storage check but local path clearly defers verification', async () => {
  const managed = await externalPage({cookie:true,phase:'ready'});
  managed.choose('storage_mode','managed-nfs'); managed.element('next').onclick(); assert.equal(current(managed),3);
  assert.equal(managed.calls.some(c => c.url === '/api/storage-checks'),false);
  const b = page({cookie:true,phase:'ready'}); await flush(); b.choose('destination_mode','single-node'); advance(b,2);
  b.choose('storage_mode','external-nfs'); b.edit('nfs_server','nas.local'); b.element('next').onclick();
  assert.equal(current(b),3); assert.match(b.element('storage-check').textContent,/after the cluster is created, before any Kubarr components.*not a passing result/);
  assert.equal(b.calls.some(c => c.url === '/api/storage-checks'),false);
});

test('storage-specific 409 returns to storage and preserves password; context 409 retains destination route', async () => {
  const server = {cookie:true,phase:'ready',startStatus:409,startError:'NFS storage check expired'};
  const b = await externalPage(server); b.element('next').onclick(); await flush(); advance(b,1);
  await b.element('start').onclick(); assert.equal(current(b),2);
  assert.match(b.element('error').textContent,/NFS storage check expired.*Storage.*retry/);
  assert.equal(b.form.elements.admin_password.value,'sample-password');
  b.element('next').onclick(); await flush(); assert.equal(current(b),3);
});

async function lbPage(server = {cookie:true,phase:'ready'}) {
  const b = page(server); await flush(); b.selectTarget('opaque-local'); await flush();
  b.element('next').onclick(); b.choose('access_mode','loadbalancer');
  return b;
}

test('LoadBalancer requires an explicit, cost-disclosed test, not selection, Continue, or sidebar', async () => {
  const server = {cookie:true,phase:'ready'}; const b = await lbPage(server);
  assert.match(lbNoticeMarkup,/class="notice hidden" role="note"/);
  assert.match(lbNoticeMarkup,/TEMPORARY, selectorless LoadBalancer Service.*Cloud providers may charge for these billable resources/);
  assert.match(lbNoticeMarkup,/openresty namespace is absent.*prepares it and retains it for the future gateway install.*does not install a load balancer controller/);
  assert.match(lbNoticeMarkup,/Service is absent from the Kubernetes API after deletion \(including finalizers\).*external provider deprovisioning may lag/);
  assert.match(lbNoticeMarkup,/only five minutes.*does not reserve an address or prove actual public reachability or guarantee a future deployment/);
  assert.equal(b.element('lb-notice').classList.contains('hidden'),false);
  const visibleNotice = b.element('lb-notice').textContent;
  assert.match(visibleNotice,/TEMPORARY, selectorless LoadBalancer Service.*Cloud providers may charge for these billable resources/);
  assert.match(visibleNotice,/openresty namespace is absent.*prepares it and retains it for the future gateway install.*does not install a load balancer controller/);
  assert.match(visibleNotice,/absent from the Kubernetes API after deletion \(including finalizers\).*deprovisioning may lag.*only five minutes.*does not reserve an address.*actual public reachability.*future deployment/);
  assert.equal(server.lbSessions,undefined);
  assert.equal(b.element('next').disabled,true);
  b.element('next').onclick(); b.steps[4].onclick();
  assert.equal(current(b),1); assert.equal(server.lbSessions,undefined);
  assert.match(b.element('error').textContent,/Test LoadBalancer/);
  assert.equal(b.element('lb-test').disabled,false);
});

test('LB live stages block until address and cleanup pass, and failed tests retry explicitly', async () => {
  const running = {phase:'running',eligible:false,checks:[{id:'address',label:'Address',severity:'required',state:'running',message:'Waiting for IP'}, {id:'cleanup',label:'Cleanup',severity:'required',state:'pending',message:''}]};
  const failed = {phase:'complete',eligible:false,checks:[{id:'address',label:'Address',severity:'required',state:'failed',message:'No address'}, {id:'cleanup',label:'Cleanup',severity:'required',state:'passed',message:'Removed'}]};
  const server = {cookie:true,phase:'ready',lbSnapshots:[running,failed,{...lbSuccess,checks:[lbSuccess.checks[0],{...lbSuccess.checks[1],state:'running'}]},lbSuccess]};
  const b = await lbPage(server); b.element('lb-test').onclick(); b.element('lb-test').onclick(); await flush();
  assert.equal(server.lbSessions.length,1); assert.equal(b.element('lb-test').disabled,true);
  assert.equal(b.element('next').disabled,true);
  assert.match(b.element('lb-check').textContent,/Address: running · Waiting for IP.*Cleanup: pending/);
  assert.equal(b.element('lb-check').children[2].className,'check-row running');
  assert.equal(b.timers[0].delay,650);
  await b.timers.shift()(); assert.equal(b.element('next').disabled,true);
  assert.match(b.element('lb-check').textContent,/No address.*MetalLB or Cilium/);
  b.element('lb-test').onclick(); await flush(); assert.equal(b.element('next').disabled,true);
  assert.match(b.element('lb-check').textContent,/Cleanup: running/);
  b.element('lb-test').onclick(); await flush(); assert.equal(b.element('next').disabled,false);
  assert.equal(server.lbSessions.length,3);
  b.element('next').onclick(); assert.equal(current(b),2);
});

test('LB request failures, local k3s and expiry keep progress and start gated', async () => {
  const server = {cookie:true,phase:'ready',lbStartError:'Timed out'}; const b = await lbPage(server);
  b.element('lb-test').onclick(); await flush();
  assert.match(b.element('lb-check').textContent,/Timed out.*retry/);
  server.lbStartError = null; server.lbGetError = 'GET timed out'; b.element('lb-test').onclick(); await flush();
  assert.equal(b.element('next').disabled,true); assert.match(b.element('lb-check').textContent,/GET timed out/);
  server.lbGetError = null; b.element('lb-test').onclick(); await flush();
  advance(b,3); assert.equal(current(b),4);
  b.tick(300001); b.timers.find(timer => timer.delay === 300000)?.();
  assert.equal(b.element('start').disabled,true);
  b.element('refresh').onclick(); await flush();
  b.element('start').onclick(); assert.equal(current(b),1);
  assert.equal(b.calls.some(c => c.url === '/api/start'),false);
  assert.match(b.element('error').textContent,/Test LoadBalancer/);
  b.steps[0].onclick(); b.choose('destination_mode','single-node'); b.steps[4].onclick();
  assert.equal(current(b),1); assert.equal(b.element('lb-test').disabled,true);
  assert.match(b.element('lb-check').textContent,/Choose an existing configured cluster, or use NodePort or port-forward/);
});

test('LB changed mode/target cancels; stale GET and late POST cannot unlock another selection', async () => {
  const oldGet = deferred(), latePost = deferred();
  const server = {cookie:true,phase:'ready',targets:[localTarget,{...localTarget,id:'other'}],lbGates:[oldGet.promise]};
  const b = await lbPage(server); b.element('lb-test').onclick(); await flush();
  b.element('back').onclick(); b.selectTarget('other'); await flush();
  assert.deepEqual(server.lbCancelled,['lb-1']);
  b.element('next').onclick(); oldGet.resolve(); await flush(); assert.equal(b.element('next').disabled,true);
  server.lbStartGates = [latePost.promise]; b.element('lb-test').onclick(); await flush();
  b.choose('access_mode','nodeport'); b.choose('access_mode','loadbalancer'); b.element('lb-test').onclick();
  latePost.resolve(); await flush();
  assert.ok(server.lbCancelled.includes('lb-2'));
  assert.equal(server.lbSessions.at(-1).target_id,'other');
  assert.equal(b.element('next').disabled,false);
  b.choose('access_mode','port-forward'); assert.equal(b.element('next').disabled,false);
  assert.ok(server.lbCancelled.includes('lb-3'));
});

test('LB no-address, cleanup pending, and incomplete required stages never unlock Continue', async () => {
  const snapshots = [
    {phase:'complete',eligible:false,checks:[{...lbSuccess.checks[0],state:'failed',message:'No external address'},lbSuccess.checks[1]]},
    {phase:'running',eligible:false,checks:[lbSuccess.checks[0],{...lbSuccess.checks[1],state:'running',message:'Removing Service'}]},
    {phase:'complete',eligible:true,checks:[{...lbSuccess.checks[0],state:'pending'},lbSuccess.checks[1]]},
  ];
  const server = {cookie:true,phase:'ready',lbSnapshots:snapshots}; const b = await lbPage(server);
  b.element('lb-test').onclick(); await flush(); assert.equal(b.element('next').disabled,true);
  assert.match(b.element('lb-check').textContent,/No external address/);
  b.element('lb-test').onclick(); await flush(); assert.equal(b.element('next').disabled,true);
  assert.match(b.element('lb-check').textContent,/Removing Service/);
  await b.timers.shift()(); assert.equal(b.element('next').disabled,true);
  b.element('next').onclick(); assert.equal(current(b),1);
  b.element('lb-test').onclick(); await flush(); assert.equal(b.element('next').disabled,false);
});

test('Back cancels pending LB test without losing the ability to change access', async () => {
  const get = deferred(); const server = {cookie:true,phase:'ready',lbGates:[get.promise]};
  const b = await lbPage(server); b.element('lb-test').onclick(); await flush();
  b.element('back').onclick(); assert.equal(current(b),0);
  assert.deepEqual(server.lbCancelled,['lb-1']);
  get.resolve(); await flush(); b.element('next').onclick(); assert.equal(b.element('next').disabled,true);
  b.choose('access_mode','nodeport'); b.element('next').onclick(); assert.equal(current(b),2);
});

test('LB start 409 returns to Server & access with password and failure details intact', async () => {
  const server = {cookie:true,phase:'ready',startStatus:409,startError:'LoadBalancer check expired'};
  const b = await lbPage(server); b.element('lb-test').onclick(); await flush(); advance(b,3);
  await b.element('start').onclick(); assert.equal(current(b),1);
  assert.match(b.element('error').textContent,/LoadBalancer check expired.*Server & access.*retry Test LoadBalancer/);
  assert.equal(b.form.elements.admin_password.value,'sample-password');
  assert.equal(b.element('next').disabled,true);
});

test('finished NodePort shows safe clickable IPv4, hostname and IPv6 links; log is read-only', async () => {
  for (const url of ['http://192.0.2.12:30080/', 'https://kubarr.example:30080/', 'http://[2001:db8::1]:30080/']) {
    const server = {cookie:true,phase:'succeeded',result:'<img src=x> http://bad.invalid',events:['<script>bad</script>'],access:{kind:'nodeport',url,instructions:['Use the cluster node address'],pending:false}};
    const b = page(server); await flush();
    assert.equal(current(b),6); assert.equal(server.focused,'title-6');
    assert.equal(b.element('mobile-label').textContent,'Step 7 of 7 · Finished');
    assert.equal(b.element('mobile-fill').style.width,'100%');
    assert.equal(b.steps[6].getAttribute('aria-current'),'step');
    assert.equal(b.steps[4].disabled,true);
    const details = b.element('access-details');
    const links = [details.children[2],details.children[3].firstChild];
    assert.deepEqual(links.map(link => link.href),[url,url]);
    for (const link of links) { assert.equal(link.target,'_blank'); assert.equal(link.rel,'noopener noreferrer'); }
    assert.equal(links[0].textContent,url); assert.equal(links[1].textContent,'Open dashboard');
    assert.match(html, /href="\/api\/install-log" download="kubarr-install.log">Download installation log/);
    assert.equal(b.steps[4].disabled,true);
    b.steps[5].onclick(); assert.equal(current(b),5); assert.equal(server.focused,'title-5');
    b.steps[6].onclick(); assert.equal(current(b),6);
    b.element('view-log').onclick(); assert.equal(current(b),5);
    assert.equal(b.element('events').textContent,'<script>bad</script>');
    const download = b.element('step-5').children[0].firstChild;
    assert.equal(download.classList.contains('hidden'),false);
    assert.equal(download.href,'/api/install-log'); assert.equal(download.download,'kubarr-install.log');
    b.element('step-5').children[0].children[1].onclick();
    assert.equal(current(b),6); assert.equal(server.focused,'title-6');
    assert.equal(b.calls.some(call => call.url === '/api/start'),false);
  }
});

test('port-forward instructions require a command first and do not imply verified reachability', async () => {
  const server = {cookie:true,phase:'succeeded',access:{kind:'port-forward',url:'http://127.0.0.1:8080/',pending:false,instructions:['kubectl -n openresty port-forward svc/gateway 8080:8080','On a remote host use SSH forwarding first']}};
  const b = page(server); await flush();
  const details = b.element('access-details');
  assert.match(details.textContent,/run the command below before opening.*loopback.*remote server.*SSH forwarding/);
  assert.match(details.textContent,/kubectl -n openresty port-forward.*SSH forwarding first/);
  assert.equal(details.children[3].firstChild.textContent,'Open dashboard after forwarding');
  assert.equal(details.children[1].children.length,2);
});

test('pending LoadBalancer refreshes manually, handles errors and ignores stale refreshes', async () => {
  const pending = {kind:'loadbalancer',url:null,pending:true,instructions:['Configure a controller']};
  const server = {cookie:true,phase:'succeeded',access:pending};
  const b = page(server); await flush();
  assert.equal(current(b),6);
  assert.match(b.element('access-details').textContent,/Waiting for a LoadBalancer address.*Configure a controller/);
  assert.equal(b.element('access-details').children.length,2);
  assert.equal(b.calls.some(c => c.url === '/api/access'),false);
  server.accessError = true; await b.element('refresh-access').onclick();
  assert.match(b.element('access-error').textContent,/Try again/);
  assert.match(b.element('access-details').textContent,/Waiting for a LoadBalancer/);
  server.accessError = false;
  const gate = deferred(); server.accessGate = gate.promise;
  server.access = {kind:'loadbalancer',url:'https://kubarr.example:8080/',pending:false,instructions:['Use assigned address']};
  const refresh = b.element('refresh-access').onclick();
  assert.equal(b.element('refresh-access').disabled,true);
  gate.resolve(); await refresh;
  assert.equal(b.element('access-error').textContent,'');
  assert.match(b.element('access-details').textContent,/https:\/\/kubarr.example:8080\//);
  const stale = deferred(); server.accessGate = stale.promise;
  server.access = pending;
  const refreshing = b.element('refresh-access').onclick();
  server.phase = 'failed'; server.result = 'Failed'; await b.element('reconnect').onclick();
  stale.resolve(); await refreshing;
  assert.equal(current(b),5);
  assert.equal(b.element('access-details').textContent.includes('Waiting for a LoadBalancer'),false);
  assert.equal(b.element('step-5').children[0].firstChild.classList.contains('hidden'),false);
  assert.equal(b.steps[6].disabled,true);
});

test('unsafe and absent access URLs never create dashboard anchors, even if messages contain URLs', async () => {
  for (const url of [null,'','javascript:alert(1)','data:text/html,hi','http://user:pass@example.test/','http://example.test/\n<script>','http://example.test\\@evil.test/', 'http://', 'pending']) {
    const server = {cookie:true,phase:'succeeded',events:['http://fake.test/'],result:'http://fake.test/',access:{kind:'nodeport',url,pending:false,instructions:['<img src=x>']}};
    const b = page(server); await flush();
    assert.equal(current(b),6);
    assert.equal(b.element('access-details').children.length,3);
    assert.match(b.element('access-details').textContent,/No safe dashboard URL/);
    assert.equal(b.element('access-details').children[1].firstChild.textContent,'<img src=x>');
    assert.match(html, /href="\/api\/install-log" download="kubarr-install.log"/);
  }
});
