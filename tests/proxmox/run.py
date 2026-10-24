#!/usr/bin/env python3
"""Destructive, explicitly scoped VM-104 pre-kubarr snapshot test runner."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
import yaml

from redact import redact_artifacts, scrub

ROOT = Path(__file__).resolve().parents[2]
BASE = 'https://192.168.1.2:8006/api2/json'
VM = '/nodes/bertha/qemu/104'
UUID = '4479f588-379c-4f6a-8600-923af821f825'
KUBECONFIG = Path.home() / '.kube/kubarr-prod-talos.yaml'


def api(path, method='GET'):
    request = urllib.request.Request(BASE + path, method=method,
        headers={'Authorization': 'PVEAPIToken=root@pam!opencode=' + os.environ['PROXMOX_TOKEN']})
    with urllib.request.urlopen(request, context=ssl._create_unverified_context(), timeout=20) as response:
        return json.load(response)['data']


def task(path):
    upid = api(path, 'POST')
    deadline = time.monotonic() + 240
    while time.monotonic() < deadline:
        status = api('/nodes/bertha/tasks/' + urllib.parse.quote(upid, safe='') + '/status')
        if status['status'] == 'stopped':
            if status.get('exitstatus') != 'OK':
                raise RuntimeError('Proxmox operation failed: ' + str(status.get('exitstatus')))
            return
        time.sleep(2)
    raise RuntimeError('Proxmox task did not finish within four minutes')


def kube(*args):
    return subprocess.run(['kubectl', '--kubeconfig', str(KUBECONFIG), '--request-timeout=10s', *args],
        capture_output=True, text=True, timeout=20)


def preload_candidates(out):
    """Transfer local candidate images without publishing or seeding app state."""
    images = [f'kubarr-{component}:installer-e2e' for component in ('backend', 'worker', 'frontend')]
    metadata = json.loads(subprocess.check_output(['docker', 'image', 'inspect', *images], text=True))
    (out / 'candidate-images.json').write_text(json.dumps([
        {'tags': item['RepoTags'], 'id': item['Id'], 'created': item['Created']} for item in metadata
    ], indent=2))
    name = 'kubarr-image-loader-' + secrets.token_hex(4)
    manifest = {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name, 'namespace': 'kube-system'},
        'spec': {'restartPolicy': 'Never', 'automountServiceAccountToken': False,
                 'containers': [{'name': 'loader', 'image': 'docker.io/library/docker:29-dind',
                    'command': ['sleep', '900'], 'securityContext': {'runAsUser': 0},
                    'volumeMounts': [{'name': 'runtime', 'mountPath': '/runtime/containerd.sock'}]}],
                 'volumes': [{'name': 'runtime', 'hostPath': {'path': '/run/containerd/containerd.sock', 'type': 'Socket'}}]}}
    command = ['kubectl', '--kubeconfig', str(KUBECONFIG)]
    print('[lab] Preload explicitly identified local candidate images', flush=True)
    try:
        subprocess.run([*command, 'create', '-f', '-'], input=json.dumps(manifest), text=True, check=True, timeout=30)
        subprocess.run([*command, 'wait', '-n', 'kube-system', f'pod/{name}', '--for=condition=Ready', '--timeout=180s'], check=True, timeout=190)
        exporter = subprocess.Popen(['docker', 'image', 'save', '--platform', 'linux/amd64', *images], stdout=subprocess.PIPE)
        try:
            imported = subprocess.run([*command, 'exec', '-i', '-n', 'kube-system', name, '--',
                'ctr', '--address', '/runtime/containerd.sock', '--namespace', 'k8s.io', 'images', 'import', '-'],
                stdin=exporter.stdout, capture_output=True, timeout=240)
            exporter.stdout.close()
            assert exporter.wait(timeout=20) == 0, 'Image export failed'
            if imported.returncode != 0:
                raise RuntimeError('Candidate image import failed: ' + imported.stderr.decode(errors='replace'))
            (out / 'image-import.txt').write_bytes(imported.stdout)
        finally:
            if exporter.poll() is None:
                exporter.kill()
                exporter.wait()
    finally:
        subprocess.run([*command, 'delete', 'pod', name, '-n', 'kube-system', '--ignore-not-found', '--wait=false'], timeout=30)


def prepare_app_chart_registry(private, out):
    """Serve frozen candidate charts through an isolated in-cluster OCI fixture."""
    name = 'kubarr-lab-charts'
    command = ['kubectl', '--kubeconfig', str(KUBECONFIG)]
    resources = [
        {'apiVersion': 'v1', 'kind': 'Pod', 'metadata': {'name': name, 'namespace': 'kube-system', 'labels': {'app': name}},
         'spec': {'automountServiceAccountToken': False, 'containers': [{
             'name': 'registry', 'image': 'registry:3',
             'env': [{'name': 'REGISTRY_HTTP_ADDR', 'value': '0.0.0.0:5000'}],
             'ports': [{'containerPort': 5000}],
             'readinessProbe': {'httpGet': {'path': '/v2/', 'port': 5000}, 'periodSeconds': 1, 'timeoutSeconds': 2},
             'volumeMounts': [{'name': 'data', 'mountPath': '/var/lib/registry'}],
             'resources': {'requests': {'cpu': '50m', 'memory': '64Mi'}, 'limits': {'memory': '256Mi'}},
         }], 'volumes': [{'name': 'data', 'emptyDir': {}}]}},
        {'apiVersion': 'v1', 'kind': 'Service', 'metadata': {'name': name, 'namespace': 'kube-system'},
         'spec': {'selector': {'app': name}, 'ports': [{'port': 5000, 'targetPort': 5000}]}},
    ]
    print('[lab] Prepare candidate application chart registry', flush=True)
    subprocess.run([*command, 'create', '-f', '-'], input=json.dumps({'apiVersion': 'v1', 'kind': 'List', 'items': resources}),
                   text=True, check=True, timeout=30)
    subprocess.run([*command, 'wait', '-n', 'kube-system', f'pod/{name}', '--for=condition=Ready', '--timeout=180s'], check=True, timeout=190)
    forward_log = private / 'registry-forward.log'
    with forward_log.open('w') as log:
        forward = subprocess.Popen([*command, 'port-forward', '-n', 'kube-system', f'pod/{name}', ':5000'],
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    charts = private / 'charts'
    packages = private / 'chart-packages'
    packages.mkdir()
    versions = {}
    metadata = {}
    try:
        for _ in range(100):
            match = re.search(r'127\.0\.0\.1:(\d+) ->', forward_log.read_text())
            if match:
                port = match[1]
                break
            if forward.poll() is not None:
                raise RuntimeError('Candidate chart registry forwarding failed')
            time.sleep(0.2)
        else:
            raise RuntimeError('Candidate registry forwarding did not become ready')
        for chart in sorted(charts.iterdir()):
            if not (chart / 'Chart.yaml').exists():
                continue
            text = (chart / 'Chart.yaml').read_text()
            document = yaml.safe_load(text)
            metadata[chart.name + '.yaml'] = text
            versions[chart.name] = document['version']
            subprocess.run(['helm', 'package', str(chart), '--destination', str(packages)],
                           check=True, capture_output=True, timeout=60)
            archive = packages / f'{document["name"]}-{document["version"]}.tgz'
            subprocess.run(['helm', 'push', str(archive), f'oci://127.0.0.1:{port}/kubarr-charts', '--plain-http'],
                           check=True, capture_output=True, timeout=60)
    finally:
        if forward.poll() is None:
            os.killpg(forward.pid, signal.SIGTERM)
            forward.wait(timeout=10)
    (out / 'candidate-app-chart-versions.json').write_text(json.dumps(versions, indent=2))
    configure_catalog_mount(charts, name, metadata, versions)
    print('[lab] Candidate app chart fixture ready; application installs remain browser-driven', flush=True)


def configure_catalog_mount(charts, name, metadata, versions):
    for component in ('kubarr-backend', 'kubarr-worker'):
        chart = charts / component
        values = yaml.safe_load((chart / 'values.yaml').read_text())
        values['env'].extend([
            {'name': 'KUBARR_CHARTS_SOURCE_DIR', 'value': '/lab-charts'},
            {'name': 'KUBARR_CHARTS_REGISTRY', 'value': f'oci://{name}.kube-system.svc.cluster.local:5000/kubarr-charts'},
            {'name': 'KUBARR_CHARTS_PLAIN_HTTP', 'value': 'true'},
        ])
        (chart / 'values.yaml').write_text(yaml.safe_dump(values, sort_keys=False))
        config = {'apiVersion': 'v1', 'kind': 'ConfigMap',
                  'metadata': {'name': name, 'namespace': component}, 'data': metadata}
        (chart / 'templates/lab-chart-source.yaml').write_text(yaml.safe_dump(config, sort_keys=False))
        deployment = chart / 'templates/deployment.yaml'
        text = deployment.read_text()
        if text.count('      volumes:\n') != 1 or text.count('          volumeMounts:\n') != 1:
            raise RuntimeError('Unexpected component template; cannot mount candidate catalog')
        items = [{'key': app + '.yaml', 'path': app + '/Chart.yaml'} for app in versions]
        volume = yaml.safe_dump([
            {'name': 'lab-chart-metadata', 'configMap': {'name': name, 'items': items}},
            {'name': 'lab-charts', 'emptyDir': {}},
        ], sort_keys=False)
        mount = yaml.safe_dump([{'name': 'lab-charts', 'mountPath': '/lab-charts', 'readOnly': True}], sort_keys=False)
        init = yaml.safe_dump([{
            'name': 'copy-lab-catalog', 'image': 'busybox:1.38',
            'command': ['sh', '-ec', 'cp -RL /metadata/* /catalog/'],
            'securityContext': {'runAsUser': 1000, 'runAsGroup': 1000, 'runAsNonRoot': True,
                                'allowPrivilegeEscalation': False, 'capabilities': {'drop': ['ALL']}},
            'volumeMounts': [{'name': 'lab-chart-metadata', 'mountPath': '/metadata', 'readOnly': True},
                            {'name': 'lab-charts', 'mountPath': '/catalog'}],
        }], sort_keys=False)
        text = text.replace('      volumes:\n', '      volumes:\n' + ''.join('        ' + line + '\n' for line in volume.splitlines()))
        text = text.replace('          volumeMounts:\n', '          volumeMounts:\n' + ''.join('            ' + line + '\n' for line in mount.splitlines()))
        # Kubernetes ConfigMap projected directories are symlinks. Discovery
        # deliberately avoids following directory symlinks, so copy metadata
        # into a regular read-only catalog tree before the process starts.
        text = text.replace('      containers:\n', '      initContainers:\n' + ''.join('        ' + line + '\n' for line in init.splitlines()) + '      containers:\n')
        deployment.write_text(text)


def main(args):
    out = Path(args.artifacts).resolve()
    out.mkdir(parents=True, mode=0o700, exist_ok=False)
    if args.background:
        command = [sys.executable, __file__, '--binary', str(Path(args.binary).resolve()),
                   '--artifacts', str(out), '--child']
        if args.charts:
            command.extend(['--charts', str(Path(args.charts).resolve())])
        if args.candidate_images:
            command.append('--candidate-images')
        if args.all_apps:
            command.append('--all-apps')
        if args.headless:
            command.append('--headless')
        if args.candidate_app_charts:
            command.append('--candidate-app-charts')
        if args.strict_ui:
            command.append('--strict-ui')
        with (out / 'run.log').open('w') as log:
            child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        print(json.dumps({'pid': child.pid, 'artifacts': str(out)}))
        return
    run(args, out)


def browser_phase(env, out, phase):
    browser_env = dict(env, LAB_PHASE=phase)
    browser = subprocess.Popen([str(ROOT / 'tests/installer/node_modules/.bin/playwright'),
        'test', '--config', str(Path(env['LAB_PRIVATE_DIR']) / 'browser-tests/playwright.config.mjs')], cwd=ROOT, env=browser_env,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
    def expire_browser():
        if browser.poll() is None:
            try:
                os.killpg(browser.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    watchdog = threading.Timer(6 * 60 * 60 if phase == 'apps' else 36 * 60, expire_browser)
    watchdog.start()
    try:
        for line in browser.stdout:
            for value in (env['LAB_PASSWORD'], env['LAB_INVITATION'], env['LAB_INVITATION'].split('#')[-1]):
                line = line.replace(value, '[REDACTED]')
            line = scrub(line.encode(), []).decode()
            print(line, end='', flush=True)
        return browser.wait(timeout=60)
    finally:
        watchdog.cancel()
        if browser.poll() is None:
            expire_browser()
            browser.wait(timeout=15)
        redact_artifacts(out, [env['LAB_PASSWORD'], env['LAB_INVITATION'], env['LAB_INVITATION'].split('#')[-1]], skip_logs=True)


def run(args, out):
    began = time.monotonic()
    if getattr(args, 'candidate_app_charts', False) and not args.charts:
        raise RuntimeError('--candidate-app-charts requires --charts')
    print('[lab] Verify VM identity and snapshot', flush=True)
    config = api(VM + '/config')
    if config.get('name') != 'kubarr-prod' or config.get('smbios1') != 'uuid=' + UUID:
        raise RuntimeError('VM identity does not match the authorized lab target')
    if not any(s['name'] == 'pre-kubarr' for s in api(VM + '/snapshot')):
        raise RuntimeError('Authorized pre-kubarr snapshot is missing')
    source_binary = Path(args.binary).resolve()
    private = Path(tempfile.mkdtemp(prefix='kubarr-proxmox-private-', dir='/tmp/opencode'))
    print('[lab] Private local diagnostics: ' + str(private), flush=True)
    frozen_tests = private / 'browser-tests'
    frozen_tests.mkdir()
    test_digests = {}
    for filename in ('journey.spec.mjs', 'apps.spec.mjs', 'playwright.config.mjs'):
        content = (ROOT / 'tests/proxmox' / filename).read_text()
        test_digests[filename] = hashlib.sha256(content.encode()).hexdigest()
        content = content.replace("'../installer/package.json'", repr((ROOT / 'tests/installer/package.json').as_uri()))
        (frozen_tests / filename).write_text(content)
    (out / 'browser-source-digests.json').write_text(json.dumps(test_digests, indent=2))
    binary = private / 'kubarr'
    shutil.copyfile(source_binary, binary)
    os.chmod(binary, 0o700)
    if args.charts:
        overrides = private / 'charts'
        overrides.mkdir()
        hashes = {}
        for chart in Path(args.charts).resolve().glob('*/*/Chart.yaml'):
            destination = overrides / chart.parent.name
            shutil.copytree(chart.parent, destination)
            digest = hashlib.sha256()
            for path in sorted(destination.rglob('*')):
                if path.is_file():
                    digest.update(str(path.relative_to(destination)).encode())
                    digest.update(path.read_bytes())
            hashes[chart.parent.name] = digest.hexdigest()
        (out / 'candidate-chart-digests.json').write_text(json.dumps(hashes, indent=2))
    (out / 'provenance.json').write_text(json.dumps({
        'vmid': 104, 'node': 'bertha', 'snapshot': 'pre-kubarr',
        'binary': str(source_binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'version': subprocess.check_output([str(binary), '--version'], text=True).strip(),
    }, indent=2))
    print('[lab] Stop VM 104 and restore pre-kubarr', flush=True)
    if api(VM + '/status/current')['status'] != 'stopped':
        task(VM + '/status/stop')
    task(VM + '/snapshot/pre-kubarr/rollback')
    task(VM + '/status/start')
    print('[lab] Await restored Kubernetes node', flush=True)
    boot_started = time.monotonic()
    deadline = boot_started + 600
    next_progress = boot_started + 30
    while time.monotonic() < deadline:
        result = kube('get', 'node', 'kubarr-prod', '-o', 'json')
        if result.returncode == 0:
            node = json.loads(result.stdout)
            if node['status']['nodeInfo']['systemUUID'] != UUID:
                raise RuntimeError('Kubeconfig does not identify the authorized VM')
            if any(c['type'] == 'Ready' and c['status'] == 'True' for c in node['status']['conditions']):
                break
        if time.monotonic() >= next_progress:
            print(f'[lab] Waiting for restored node readiness ({int(time.monotonic() - boot_started)}s)', flush=True)
            next_progress = time.monotonic() + 30
        time.sleep(5)
    else:
        raise RuntimeError('Restored Kubernetes API/node did not become ready')
    (out / 'restore-timing.json').write_text(json.dumps({'seconds': round(time.monotonic() - began, 2)}))
    if args.candidate_images:
        preload_candidates(out)
    if args.candidate_app_charts:
        if not args.charts:
            raise RuntimeError('--candidate-app-charts requires --charts')
        prepare_app_chart_registry(private, out)
    env = {k: v for k, v in os.environ.items() if k not in ('PROXMOX_TOKEN', 'SSH_CONNECTION', 'SSH_CLIENT', 'DISPLAY', 'WAYLAND_DISPLAY')}
    env.update(HOME=str(private), KUBECONFIG=str(private / 'kubeconfig'), LAB_ARTIFACTS=str(out),
               LAB_PASSWORD=secrets.token_urlsafe(24), LAB_PRIVATE_DIR=str(private),
               LAB_AUTH_STATE=str(private / 'browser-auth.json'))
    if args.all_apps:
        env['LAB_ALL_APPS'] = '1'
    if args.headless or args.all_apps:
        env['LAB_HEADLESS'] = '1'
    if args.strict_ui:
        env['LAB_STRICT_UI'] = '1'
    if args.candidate_images:
        env.update(KUBARR_BACKEND_IMAGE='kubarr-backend', KUBARR_FRONTEND_IMAGE='kubarr-frontend',
                   KUBARR_WORKER_IMAGE='kubarr-worker', KUBARR_IMAGE_TAG='installer-e2e', KUBARR_IMAGE_PULL_POLICY='Never')
    if args.charts:
        env['KUBARR_CHARTS_DIR'] = str(private / 'charts')
    shutil.copyfile(KUBECONFIG, private / 'kubeconfig')
    os.chmod(private / 'kubeconfig', 0o600)
    # The password is kept private for follow-up verification on the final run.
    (private / 'administrator-password').write_text(env['LAB_PASSWORD'])
    os.chmod(private / 'administrator-password', 0o600)
    cli_output = private / 'cli.log'
    with cli_output.open('w') as log:
        cli = subprocess.Popen([str(binary), 'bootstrap', '--web-port', '18090'], env=env,
            stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        for _ in range(100):
            text = cli_output.read_text()
            match = re.search(r'http://127\.0\.0\.1:18090/#[A-Za-z0-9_-]+', text)
            if match:
                env['LAB_INVITATION'] = match[0]
                break
            if cli.poll() is not None:
                raise RuntimeError('Installer exited before starting its browser wizard')
            time.sleep(0.2)
        else:
            raise RuntimeError('Installer invitation was not available')
        # Browser binaries are installed in the real user's cache, outside the
        # isolated installer HOME. Only the CLI receives the isolated HOME.
        env['HOME'] = str(Path.home())
        for key in ('DISPLAY', 'WAYLAND_DISPLAY'):
            if key in os.environ:
                env[key] = os.environ[key]
        print('[lab] Playwright driving browser; recording video and screenshots', flush=True)
        installer_code = browser_phase(env, out, 'installer')
        (out / 'installer-result.json').write_text(json.dumps({'passed': installer_code == 0}))
        apps_code = None
        if installer_code == 0 and args.all_apps:
            os.chmod(private / 'browser-auth.json', 0o600)
            catalog = json.loads((out / 'catalog.json').read_text())
            count = sum(not app['is_system'] and not app['is_hidden'] for app in catalog)
            print(f'[lab] Testing all {count} installable catalog apps sequentially', flush=True)
            apps_code = browser_phase(env, out, 'apps')
        (out / 'result.json').write_text(json.dumps({
            'passed': installer_code == 0 and apps_code in (None, 0),
            'installer_passed': installer_code == 0, 'apps_passed': None if apps_code is None else apps_code == 0,
        }))
    finally:
        for name, command in [('pods', ('get', 'pods', '-A', '-o', 'wide')),
                              ('events', ('get', 'events', '-A', '--sort-by=.lastTimestamp')),
                              ('volumes', ('get', 'pvc', '-A'))]:
            result = kube(*command)
            (out / f'{name}.txt').write_text(result.stdout + result.stderr)
        if cli.poll() is None:
            os.killpg(cli.pid, signal.SIGTERM)
            try:
                cli.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(cli.pid, signal.SIGKILL)
                cli.wait(timeout=10)
        print('[lab] Browser evidence: ' + str(out), flush=True)
        print('[lab] Private local diagnostics: ' + str(private), flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True)
    parser.add_argument('--artifacts', required=True)
    parser.add_argument('--background', action='store_true')
    parser.add_argument('--charts')
    parser.add_argument('--candidate-images', action='store_true')
    parser.add_argument('--all-apps', action='store_true')
    parser.add_argument('--headless', action='store_true')
    parser.add_argument('--candidate-app-charts', action='store_true')
    parser.add_argument('--strict-ui', action='store_true')
    parser.add_argument('--child', action='store_true')
    args = parser.parse_args()
    try:
        if args.child:
            run(args, Path(args.artifacts))
        else:
            main(args)
    except Exception as error:
        out = Path(args.artifacts)
        if out.is_dir():
            (out / 'result.json').write_text(json.dumps({'passed': False, 'error': str(error)}))
        raise
