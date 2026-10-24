# Fast installer → app E2E

One Chromium journey exercises the real `kubarr` binary against a fresh Kind
cluster, completes the installation wizard, signs into the installed dashboard,
installs Sonarr through the catalog, and checks Sonarr's API through the gateway.
No application releases, administrator users, or database records are seeded by
the harness. The only infrastructure fixture is an external NFS server.

The cluster defaults to baseline Pod Security. Fluent Bit must receive its
host-log admission exception from its own chart. All eight bootstrap workloads
must roll out successfully; installed Helm chart revisions are checked, including
SemVer build metadata such as `+9`. Sonarr must have bound storage and a working
application endpoint, not just an accepted install request.

## Local run

Requirements: Linux, Docker Engine/API 1.48+ with permission to run privileged
containers, Kind **0.33.0**, kubectl **1.35.8**, Helm **4.x**, Python 3.11+, Node.js
22+ (CI uses 26), and sufficient space/memory for one additional cluster. Load
the host's NFS modules once if necessary:

```sh
sudo modprobe nfs
sudo modprobe nfsd
```

From the CLI repository:

```sh
python3 -m venv .venv-installer
. .venv-installer/bin/activate
pip install -r tests/installer/requirements.txt
npm ci --prefix tests/installer --ignore-scripts
npm exec --prefix tests/installer -- playwright install --with-deps chromium
cargo build --locked
python tests/installer/run.py
```

Use `--binary /path/to/kubarr` to test a release binary. The CLI binary and this
checkout must correspond to the same revision: preloading and expected chart
versions are derived from `src/charts.rs` and the probe image from
`src/storage_probe.rs`.

The default run uses published OCI charts and published application images.
Missing release images are a test failure, not a reason to fall back to `latest`.
The application catalog is discovered by the actual backend as in a user install;
the expected Sonarr revision is pinned in `run.py`. Catalog drift fails the
revision assertion and requires a deliberate fixture update.

### Testing application candidates

Build the three application images using the application repository's Dockerfiles,
then pass the installer's existing image overrides:

```sh
KUBARR_BACKEND_IMAGE=kubarr-backend \
KUBARR_FRONTEND_IMAGE=kubarr-frontend \
KUBARR_WORKER_IMAGE=kubarr-worker \
KUBARR_IMAGE_TAG=my-candidate \
python tests/installer/run.py --binary target/debug/kubarr
```

For candidate **bootstrap** charts, additionally pass
`--charts ../kubarr-charts`. Dependencies must already be built. The harness
creates the flat chart-directory layout expected by the installer. Sonarr still
uses the backend's normal published catalog; this option does not replace its
catalog or patch workloads after installation.

## Speed and reports

- One browser, one app, no retry matrix or duplicate installation.
- Chart preparation and image pulls use four concurrent workers.
- Existing Docker images are reused; native-platform images are loaded into Kind
  in one archive. Digest-only probe images are pulled through CRI to preserve
  their digest identity.
- The installer and app waits observe actual state with bounded deadlines.
- The fresh kernel-NFS fixture waits for its NFSv4 recovery grace period to end
  before presenting an external export to the wizard. The wait reads server
  state without disabling grace, and its timing is reported separately.
- Browser stage timings and infrastructure timings are written separately, so
  cold image preparation is distinguishable from installation time.

Artifacts are in `tests/installer/artifacts/<run>/`: result, versions, timings,
curated installer progress, browser failure output, screenshot, workload status,
Kubernetes events, node startup output, and short-lived NFS probe diagnostics.
Password fields are cleared before failure screenshots. Raw request traces,
Secrets, ConfigMaps, kubeconfigs, and private installer logs are not uploaded.

The harness creates an isolated kubeconfig and installer HOME. Cleanup stops its
process groups, deletes its exact cluster, then removes its NFS container and
volume. It does not prune Docker or select the operator's current context. If
cluster removal fails, the NFS fixture is retained for its remaining clients and
the run fails with its resource name.

On heavily used development hosts, Kind can fail with `Too many open files`
despite a high `ulimit`: check `fs.inotify.max_user_instances`. CI sets it to 1024
on its disposable runner; the local harness does not change host sysctls.

## CI

`ci.yml` invokes `installer-e2e.yml` after the binary builds. The E2E job downloads
and verifies the **same x86_64 archive** that will be published. Publishing depends
on this job passing. PRs run the same journey without publishing. The standalone
manual workflow builds a binary from its selected revision.

The initial lane runs x86_64 / Kubernetes 1.35.8 / external NFS / port-forward /
Sonarr. It does not claim coverage of local k3s provisioning, Talos, ARM64 runtime,
LoadBalancer allocation, or NetworkPolicy enforcement (Kind's default CNI does
not enforce policies). Those belong in later compatibility lanes.

Harness checks, without Docker or a cluster:

```sh
python -m unittest discover -s tests/installer -p 'test_*.py'
```
