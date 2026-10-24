# Recorded Proxmox installer runs

This destructive runner is intentionally restricted to **bertha / VM 104 /
kubarr-prod / snapshot pre-kubarr**, with a matching Kubernetes system UUID.
Every run stops the VM, restores that snapshot, starts it, and waits for the
Talos Kubernetes node. Use only with explicit permission to discard VM changes.

Requirements: Python 3, kubectl, the dependencies and Chromium installed for
`tests/installer`, `~/.kube/kubarr-prod-talos.yaml`, and a Proxmox API token in
`PROXMOX_TOKEN` for `root@pam!opencode`. The token is not passed to the installer
or browser. The runner uses unverified TLS for this lab's private Proxmox endpoint.

```sh
cargo build --locked
python3 tests/proxmox/run.py --binary target/debug/kubarr \
  --artifacts /tmp/opencode/kubarr-proxmox-run-N --background
```

Use a new artifact directory for every run. Playwright runs with a visible browser
when a desktop display is available and always records viewport video, numbered
screenshots, an HTML report, and a rolling `latest.png`. It selects the actual
Talos context in the wizard, managed NFS (50Gi), and gateway NodePort access.
Success requires browser login, a populated catalog, and installing/opening Sonarr
through the dashboard's actual storage wizard and worker queue.
No application resources or administrator records are seeded by the harness.

For source-chart candidates, add `--charts ../kubarr-charts` (build chart
dependencies first). Add `--candidate-images` to preload the three local
`kubarr-{backend,worker,frontend}:installer-e2e` images into this VM's containerd.
The temporary loader Pod is removed after the transfer. Candidate image IDs
are recorded and the installer receives explicit local image overrides with
`pullPolicy: Never`. This mode validates candidate behavior, not public registry
availability. Every run still restores the snapshot first and seeds no app data.

The API invitation and generated administrator password stay outside reports.
Passwords appear only in masked browser fields. HTTP traces are disabled.
Private kubeconfig/password/CLI output are retained in the printed private
temporary directory for diagnosing and accessing the final VM state. The VM is
left in the observed state after each run; the next run restores the snapshot.

## Full app matrix

Add `--all-apps --headless` to restore once, run the complete installer, and then
discover every non-system, non-hidden app from the authenticated live catalog.
Playwright installs each app through its actual wizard with default storage,
no VPN, and no optional GPU. Each app has its own test, screenshots, video,
readiness/PVC checks, gateway response, and removal result. Apps are removed
sequentially to keep VM resource pressure from invalidating later tests.
Native application authentication (HTTP 401) is reported separately from a
failed upstream. Third-party account setup and external integrations are not
configured automatically.

The normal uninstall path uses the dashboard. Failed partial installs can use
an explicitly reported API cleanup fallback; if that also fails, an app-scoped
Helm uninstall is attempted and the test remains failed. Deterministic missing
images and Pod Security rejection are captured early, then the active operation
is stopped before cleanup. Per-app results are under `apps/<name>/result.json`,
with a separate `apps-report/` HTML report and `apps-browser/` recordings.

Playwright's HTML report embeds API-step arguments even with tracing disabled.
The runner therefore scrubs the administrator password and invitation from raw
output and embedded report ZIPs. Browser auth state remains in the private
temporary directory. Report and target-guard tests run without touching Proxmox:

```sh
python3 -B -m unittest discover -s tests/proxmox -p 'test_*.py' -v
```

For unpublished app chart fixes, add `--candidate-app-charts` with `--charts`.
The runner packages the frozen charts, uploads them to a temporary in-cluster
OCI registry, and gives the API/worker a read-only catalog metadata mount and
explicit registry override. This uses the application's existing local-source
and plain-HTTP development options; no app releases are installed by the fixture.
The registry lives inside VM 104 and disappears on the next snapshot rollback.
`--strict-ui` additionally requires rendered text and checks Jellyseerr's
configuration persistence warning. Native Jenkins login is checked separately
from its anonymous-home 403 response.

Set `LAB_IMMICH_HOST=1` for Immich's supported root-host route. Playwright creates
an exact-host assignment through Settings → Domains, resolves the reserved
`immich.kubarr.test` name to VM 104 inside Chromium only, signs into that gateway
hostname, and checks Immich's actual welcome screen. It then deletes the test
domain. No LAN/public DNS is changed. This records
`path_route_supported: false`: Immich upstream does not support `/immich/`.
Jenkins's initial Unlock screen and Plex's framed sign-in screen are valid native
setup states; these checks do not create third-party accounts or claim that
external service integrations have been configured.

The CLI binary, candidate charts, and browser test sources are copied into the
private run directory before restoration, with hashes recorded in the public
artifacts. Editing the checkout during a run cannot alter that run's tests.
