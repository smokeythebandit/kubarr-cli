#!/usr/bin/env python3
"""One real installer-to-app journey. Owns only a new cluster and NFS fixture."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import subprocess
import tempfile
import threading
import time

import yaml


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
REGISTRY = "oci://ghcr.io/smokeythebandit/kubarr-charts"
KIND_IMAGE = "kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0"
NFS_IMAGE = "itsthenetwork/nfs-server-alpine:12"
SONARR_VERSION = "4.0.20+14"
RELEASES = {
    "fluent-bit": ("fluent-bit", "daemonset/fluent-bit"),
    "victorialogs": ("victorialogs", "deployment/victorialogs"),
    "victoriametrics": ("victoriametrics", "deployment/victoriametrics"),
    "postgresql": ("kubarr-database", "statefulset/kubarr-db"),
    "kubarr-backend": ("kubarr-backend", "deployment/kubarr-backend"),
    "kubarr-frontend": ("kubarr-frontend", "deployment/kubarr-frontend"),
    "openresty": ("openresty", "deployment/kubarr-gateway"),
    "kubarr-worker": ("kubarr-worker", "deployment/kubarr-worker"),
}


def chart_versions(source):
    versions = dict(re.findall(r'"([\w-]+)"\s*=>\s*"([^"]+)"', source))
    if not RELEASES.keys() <= versions.keys():
        raise ValueError("Could not read all bootstrap chart pins from src/charts.rs")
    return {name: versions[name] for name in RELEASES}


def container_images(value):
    """Read containers and initContainers, not unrelated 'image' configuration."""
    images = set()
    if isinstance(value, dict):
        for key, child in value.items():
            if key in ("containers", "initContainers"):
                images.update(container["image"] for container in child)
            else:
                images.update(container_images(child))
    elif isinstance(value, list):
        for child in value:
            images.update(container_images(child))
    return images


def kind_config(admission_path):
    return {
        "kind": "Cluster", "apiVersion": "kind.x-k8s.io/v1alpha4",
        "nodes": [{"role": "control-plane", "extraMounts": [
            {"hostPath": str(admission_path), "containerPath": "/etc/kubarr-e2e-admission.yaml", "readOnly": True},
        ], "kubeadmConfigPatches": ["""kind: ClusterConfiguration
apiServer:
  extraArgs:
    admission-control-config-file: /etc/kubarr-e2e-admission.yaml
  extraVolumes:
    - name: admission
      hostPath: /etc/kubarr-e2e-admission.yaml
      mountPath: /etc/kubarr-e2e-admission.yaml
      readOnly: true
      pathType: File
"""]}],
    }


def admission_config():
    return {"apiVersion": "apiserver.config.k8s.io/v1", "kind": "AdmissionConfiguration",
            "plugins": [{"name": "PodSecurity", "configuration": {
                "apiVersion": "pod-security.admission.config.k8s.io/v1", "kind": "PodSecurityConfiguration",
                "defaults": {"enforce": "baseline", "enforce-version": "latest",
                             "audit": "restricted", "audit-version": "latest",
                             "warn": "restricted", "warn-version": "latest"},
                "exemptions": {"usernames": [], "runtimeClasses": [],
                               "namespaces": ["kube-system", "local-path-storage"]},
            }}]}


class Journey:
    def __init__(self, args):
        self.args = args
        self.name = "kubarr-installer-" + secrets.token_hex(5)
        self.work = Path(tempfile.mkdtemp(prefix=self.name + "-"))
        self.artifacts = args.artifacts.resolve() / self.name
        self.artifacts.mkdir(parents=True, mode=0o700)
        self.env = dict(os.environ, KUBECONFIG=str(self.work / "kubeconfig"))
        self.password = secrets.token_urlsafe(24)
        self.invitation = ""
        self.processes = []
        self.handles = []
        self.cluster_created = False
        self.nfs_created = False
        self.volume_created = False
        self.timings = []
        self.observer_stop = threading.Event()
        self.versions = chart_versions((ROOT / "src/charts.rs").read_text())

    def redact(self, text):
        for value in (self.password, self.invitation):
            if value:
                text = text.replace(value, "[REDACTED]")
        return re.sub(r"http://[^\s]+/#[A-Za-z0-9_-]+", "[INSTALLER INVITATION]", text)

    def run(self, *command, timeout=120, check=True, **kwargs):
        result = subprocess.run([str(arg) for arg in command], env=self.env,
                                capture_output=True, text=True, timeout=timeout, **kwargs)
        if check and result.returncode:
            raise RuntimeError(self.redact(f"{command[0]} failed ({result.returncode}):\n{result.stdout}\n{result.stderr}"))
        return result.stdout

    def kube(self, *args, **kwargs):
        return self.run("kubectl", "--kubeconfig", self.env["KUBECONFIG"], "--request-timeout=20s", *args, **kwargs)

    @contextmanager
    def stage(self, name):
        print(f"[e2e] {name}", flush=True)
        start = time.monotonic()
        try:
            yield
        finally:
            elapsed = round(time.monotonic() - start, 2)
            self.timings.append({"stage": name, "seconds": elapsed})
            (self.artifacts / "timings.json").write_text(json.dumps(self.timings, indent=2))
            print(f"[e2e] {name}: {elapsed}s", flush=True)

    def background(self, command, filename, env):
        handle = (self.work / filename).open("w")
        self.handles.append(handle)
        process = subprocess.Popen(command, env=env, stdout=handle, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        self.processes.append(process)
        return process

    def pull_image(self, image):
        result = self.run("docker", "image", "inspect", image, check=False)
        if not result.strip() or result.strip() == "[]":
            self.run("docker", "pull", image, timeout=240)

    def prepare_chart(self, name, version):
        destination = self.work / "charts" / name
        destination.mkdir(parents=True)
        if self.args.charts and name in RELEASES:
            candidates = [path.parent for path in self.args.charts.rglob("Chart.yaml")
                          if path.parent.name == name]
            if len(candidates) != 1:
                raise ValueError(f"Expected one candidate chart for {name}")
            chart = candidates[0].resolve()
            version = str(yaml.safe_load((chart / "Chart.yaml").read_text())["version"])
        else:
            self.run("helm", "pull", f"{REGISTRY}/{name}", "--version", version,
                     "--destination", destination, timeout=90)
            chart, = destination.glob("*.tgz")
        extra = []
        if name in ("kubarr-backend", "kubarr-frontend", "kubarr-worker"):
            key = name.removeprefix("kubarr-").upper()
            for variable, setting in [(f"KUBARR_{key}_IMAGE", "image.repository"),
                                      ("KUBARR_IMAGE_TAG", "image.tag")]:
                if self.env.get(variable):
                    extra += ["--set", f"{setting}={self.env[variable]}"]
        rendered = self.run("helm", "template", name, chart, *extra)
        images = container_images(list(yaml.safe_load_all(rendered)))
        return name, version, chart, images

    def setup(self):
        with self.stage("Prepare pinned charts and images"):
            requested = {**self.versions, "sonarr": SONARR_VERSION}
            with ThreadPoolExecutor(max_workers=4) as pool:
                charts = list(pool.map(lambda item: self.prepare_chart(*item), requested.items()))
            self.versions = {name: version for name, version, _, _ in charts}
            probe_image = re.search(r'const IMAGE: &str = "([^"]+)"', (ROOT / "src/storage_probe.rs").read_text())[1]
            images = {NFS_IMAGE, probe_image}
            for _, _, _, chart_images in charts:
                images.update(chart_images)
            manifest = {"binary": self.run(self.args.binary, "--version").strip(),
                        "kind_image": KIND_IMAGE, "charts": self.versions, "images": sorted(images),
                        "chart_mode": "candidate" if self.args.charts else "published"}
            (self.artifacts / "versions.json").write_text(json.dumps(manifest, indent=2))
            with ThreadPoolExecutor(max_workers=4) as pool:
                list(pool.map(self.pull_image, sorted(images)))
            # Local development may supply chart sources. Published/release runs
            # deliberately leave the installer's OCI resolution untouched.
            if self.args.charts:
                overrides = self.work / "overrides"
                overrides.mkdir()
                for name, _, chart, _ in charts:
                    if name in RELEASES:
                        (overrides / name).symlink_to(chart)
                self.env["KUBARR_CHARTS_DIR"] = str(overrides)
            self.images = sorted(images - {NFS_IMAGE})

        with self.stage("Create baseline-enforcing Kind cluster"):
            admission = self.work / "admission.yaml"
            admission.write_text(yaml.safe_dump(admission_config()))
            config = self.work / "kind.yaml"
            config.write_text(yaml.safe_dump(kind_config(admission)))
            # Mark before creation so a partially created cluster is also removed.
            self.cluster_created = True
            self.run("kind", "create", "cluster", "--name", self.name, "--kubeconfig", self.env["KUBECONFIG"],
                     "--image", KIND_IMAGE, "--config", config, "--retain", "--wait", "120s", timeout=180)
            if self.kube("config", "current-context").strip() != f"kind-{self.name}":
                raise RuntimeError("Unexpected context in isolated kubeconfig")

        with self.stage("Load images and start external NFS fixture"):
            self.load_images()
            self.run("docker", "volume", "create", "--label", f"kubarr.installer.run={self.name}", self.name)
            self.volume_created = True
            self.run("docker", "run", "-d", "--name", self.name + "-nfs", "--network", "kind",
                     "--label", f"kubarr.installer.run={self.name}", "--privileged",
                     "-v", f"{self.name}:/nfsshare", "-e", "SHARED_DIRECTORY=/nfsshare", NFS_IMAGE)
            self.nfs_created = True
            self.nfs_ip = self.run("docker", "inspect", "--format",
                                    '{{(index .NetworkSettings.Networks "kind").IPAddress}}', self.name + "-nfs").strip()
            # A fresh kernel NFSv4 server rejects new stateful operations during
            # its recovery grace period. The fixture must be ready before the
            # user's one-shot storage check; do not hide that with UI retries.
            with self.stage("Wait for fresh NFS server recovery grace"):
                self.wait_nfs_grace()
            self.configure_nfs_permissions()

    def load_images(self):
        platform = self.run("docker", "version", "--format", "{{.Server.Os}}/{{.Server.Arch}}").strip()
        # Docker save strips the original multi-platform digest reference.
        # Pull digest-only probe images through CRI instead of importing a
        # dangling containerd import-* alias (CreateContainerError).
        in_node = [image for image in self.images if "@sha256:" in image]
        archived = [image for image in self.images if "@sha256:" not in image]
        while archived:
            archive = self.work / "images.tar"
            try:
                self.run("docker", "image", "save", "--platform", platform, "-o", archive, *archived, timeout=180)
            except RuntimeError as error:
                # Docker's containerd image store refuses to export some pulled
                # images by platform even though they run and remain complete in
                # the registry. Let the node pull those itself rather than lose
                # the run, and never drop an image silently.
                rejected = re.findall(r"image with reference (\S+) was found but does not provide", str(error))
                remaining = [image for image in archived if image not in rejected]
                if not rejected or remaining == archived:
                    raise
                print(f"[e2e] Docker cannot export {' '.join(rejected)}; pulling in the node instead", flush=True)
                in_node += rejected
                archived = remaining
                continue
            self.run("kind", "load", "image-archive", "--name", self.name, archive, timeout=180)
            archive.unlink()
            break
        for image in in_node:
            self.run("docker", "exec", self.name + "-control-plane", "crictl", "pull", image, timeout=120)

    def record_result(self, success, error):
        (self.artifacts / "result.json").write_text(json.dumps(
            {"passed": success, "run": self.name, "error": self.redact(error)}))

    def configure_nfs_permissions(self):
        # Reproduce a normal external share: the media user can write, whereas
        # kubelet's root identity is squashed and cannot create app subPaths.
        exports = self.run("docker", "exec", self.name + "-nfs", "sh", "-ec",
                           "chown 1000:1000 /nfsshare; chmod 0775 /nfsshare; "
                           "sed -i 's/no_root_squash/root_squash/g' /etc/exports; "
                           "exportfs -ra; exportfs -v", timeout=15)
        if "no_root_squash" in exports or "root_squash" not in exports:
            raise RuntimeError("NFS acceptance fixture did not enable root squashing")

    def wait_nfs_grace(self):
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            ready = self.run("docker", "exec", self.name + "-nfs", "sh", "-c",
                             "cat /proc/fs/nfsd/v4_end_grace", timeout=10, check=False).strip()
            if ready == "Y":
                return
            time.sleep(2)
        raise RuntimeError("Fresh NFS fixture did not finish its recovery grace period")

    def browser(self):
        with self.stage("Real browser installer and Sonarr journey"):
            home = self.work / "home"
            home.mkdir()
            env = dict(self.env, HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                       E2E_PASSWORD=self.password, E2E_CONTEXT=f"kind-{self.name}",
                       E2E_NFS_SERVER=self.nfs_ip, E2E_ARTIFACTS=str(self.artifacts),
                       E2E_PRIVATE_RESULTS=str(self.work / "playwright"))
            # Ensure inherited desktop/SSH configuration cannot open a browser or
            # make the installer bind publicly during an automated local run.
            for name in ("DISPLAY", "WAYLAND_DISPLAY", "SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"):
                env.pop(name, None)
            installer = self.background([str(self.args.binary), "bootstrap", "--web", "--web-port", "0"],
                                        "installer-private.log", env)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                output = (self.work / "installer-private.log").read_text()
                match = re.search(r"http://127\.0\.0\.1:\d+/#[A-Za-z0-9_-]+", output)
                if match:
                    self.invitation = match[0]
                    break
                if installer.poll() is not None:
                    raise RuntimeError(self.redact(output))
                time.sleep(0.1)
            else:
                raise RuntimeError("Installer did not print an invitation within 20s")
            # Browser cache belongs to the invoking user, not the isolated CLI HOME.
            browser_env = dict(self.env, E2E_INVITATION=self.invitation,
                               **{key: value for key, value in env.items() if key.startswith("E2E_")})
            browser = self.background(["node", str(HERE / "node_modules/@playwright/test/cli.js"),
                                       "test", "--config", str(HERE / "playwright.config.mjs")],
                                      "browser-private.log", browser_env)
            code = browser.wait(timeout=780)
            output = self.redact((self.work / "browser-private.log").read_text())
            (self.artifacts / "browser.log").write_text(output)
            print(output, flush=True)
            if code:
                raise RuntimeError("Browser journey failed; see browser.log and installer.log")

    def observe_probes(self):
        """Capture this installer's short-lived NFS probe output before it cleans up."""
        seen = set()
        while not self.observer_stop.wait(0.5):
            try:
                pods = json.loads(self.kube("get", "pods", "-A", "-l", "kubarr.io/storage-check", "-o", "json"))
                for pod in pods.get("items", []):
                    meta = pod["metadata"]
                    (self.artifacts / "nfs-probe-status.json").write_text(json.dumps(pod.get("status", {}), indent=2))
                    if meta["uid"] in seen:
                        continue
                    seen.add(meta["uid"])
                    self.background(["kubectl", "--kubeconfig", self.env["KUBECONFIG"],
                                     "logs", "-f", "--pod-running-timeout=120s", "-n", meta["namespace"], meta["name"]],
                                    "probe.log", self.env)
            except (ValueError, RuntimeError, subprocess.TimeoutExpired):
                pass

    def verify(self):
        with self.stage("Verify chart revisions and ready workloads"):
            releases = json.loads(self.run("helm", "list", "-A", "-o", "json"))
            installed = {release["name"]: release["chart"] for release in releases}
            for name, version in self.versions.items():
                if installed.get(name) != f"{name}-{version}":
                    raise RuntimeError(f"Wrong chart revision for {name}: {installed.get(name)}; expected {version}")
            for namespace, workload in RELEASES.values():
                self.kube("-n", namespace, "rollout", "status", workload, "--timeout=30s", timeout=40)
            namespace = json.loads(self.kube("get", "namespace", "fluent-bit", "-o", "json"))
            if namespace["metadata"]["labels"].get("pod-security.kubernetes.io/enforce") != "privileged":
                raise RuntimeError("Fluent Bit namespace is missing its host-log admission exception")
            pods = json.loads(self.kube("-n", "fluent-bit", "get", "pods", "-o", "json"))
            for pod in pods["items"]:
                if any(c["restartCount"] for c in pod["status"].get("containerStatuses", [])):
                    raise RuntimeError("Fluent Bit restarted during installation")

    def diagnostics(self):
        # Keep credentials, Secrets, ConfigMaps, and raw installer SQL out of artifacts.
        commands = {
            "pods.txt": ("get", "pods", "-A", "-o", "wide"),
            "events.txt": ("get", "events", "-A", "--sort-by=.metadata.creationTimestamp"),
            "workloads.txt": ("get", "deployments,daemonsets,statefulsets,pvc", "-A"),
        }
        if self.cluster_created:
            if (self.work / "probe.log").exists():
                (self.artifacts / "nfs-probe.log").write_text(self.redact((self.work / "probe.log").read_text()))
            try:
                logs = subprocess.run(["docker", "logs", "--tail", "100", self.name + "-control-plane"],
                                      capture_output=True, text=True, timeout=10)
                (self.artifacts / "node.log").write_text(self.redact(logs.stdout + logs.stderr))
            except (OSError, subprocess.TimeoutExpired):
                pass
            for filename, command in commands.items():
                try:
                    (self.artifacts / filename).write_text(self.redact(self.kube(*command, timeout=25, check=False)))
                except (OSError, subprocess.TimeoutExpired):
                    pass

    def cleanup(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
        # Drain clients before removing their NFS server. Never prune unrelated resources.
        if self.cluster_created:
            self.run("kind", "delete", "cluster", "--name", self.name, timeout=120)
        if self.nfs_created:
            self.run("docker", "rm", "-f", self.name + "-nfs", timeout=30)
        if self.volume_created:
            self.run("docker", "volume", "rm", self.name, timeout=30)
        for handle in self.handles:
            handle.close()
        shutil.rmtree(self.work)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/kubarr")
    parser.add_argument("--charts", type=Path, help="Candidate chart repository; default uses published OCI pins")
    parser.add_argument("--artifacts", type=Path, default=HERE / "artifacts")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    if not args.binary.is_file():
        parser.error("Build the CLI with cargo build --locked or provide --binary")
    for tool in ("docker", "kind", "kubectl", "helm", "node"):
        if not shutil.which(tool):
            parser.error(f"Missing tool: {tool}")
    if not (HERE / "node_modules/@playwright/test/cli.js").is_file():
        parser.error("Run npm ci --prefix tests/installer and install Playwright Chromium first")
    if not {"nfs", "nfsd"} <= set(Path("/proc/filesystems").read_text().split()):
        parser.error("Host NFS modules required: sudo modprobe nfs && sudo modprobe nfsd")
    # Avoid silently changing the meaning of a published-artifact acceptance run.
    for variable in ("KUBARR_CHARTS_DIR", "KUBARR_LOGGING_CHARTS_DIR"):
        if os.environ.get(variable):
            parser.error(f"Unset {variable}; use the explicit --charts option for candidates")
    journey = Journey(args)
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    success = False
    reason = ""
    try:
        journey.setup()
        observer = threading.Thread(target=journey.observe_probes, daemon=True)
        observer.start()
        journey.browser()
        journey.verify()
        success = True
    except (Exception, KeyboardInterrupt) as error:
        reason = f"{type(error).__name__}: {error}"
        print(f"[e2e] FAILED: {journey.redact(str(error))}", flush=True)
    finally:
        journey.observer_stop.set()
        if 'observer' in locals():
            observer.join(timeout=25)
        journey.diagnostics()
        try:
            with journey.stage("Cleanup owned resources"):
                journey.cleanup()
        except Exception as error:
            success = False
            reason = reason or f"Cleanup failed for {journey.name}: {error}"
            print(f"[e2e] Cleanup failed for {journey.name}: {error}", flush=True)
        journey.record_result(success, reason)
        print(f"[e2e] {'PASSED' if success else 'FAILED'}; artifacts: {journey.artifacts}", flush=True)
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
