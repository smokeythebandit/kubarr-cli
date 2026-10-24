# Kubarr CLI

The standalone installer and command-line tools for [Kubarr](https://github.com/smokeythebandit/Kubarr). Installation is configured through an embedded browser wizard.

## Install

On Linux x86_64 or ARM64:

```sh
curl -fsSL https://kubarr.com/install.sh | bash
```

The script downloads this repository's latest CLI binary, verifies its SHA-256 checksum, installs `~/.local/bin/kubarr`, and starts the web wizard as your current user. Follow the printed invitation URL and keep the terminal open until setup finishes. Installing the CLI and hosting the wizard do not require administrator privileges. Creating a new local k3s cluster still requires host-administration privileges.

For later commands, add your local binary directory to PATH if needed:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

To start the wizard again:

```sh
kubarr bootstrap
kubarr bootstrap --web-port 8080
```

The wizard supports an existing Kubernetes context, an uploaded kubeconfig, or local single-node k3s. It configures dashboard access, shared storage, the initial administrator, and optional Grafana. See the [bootstrap guide](docs/bootstrap.md) for prerequisites, SSH access, diagnostics, and cluster checks.

Other commands:

```sh
kubarr doctor
kubarr users --help
kubarr --version
```

### Installation waits and failures

Fresh managed-storage installs use userspace NFS-Ganesha, which does not require
the host's kernel `nfsd` support (including on Talos). Existing managed-NFS
releases retain their server implementation. The dedicated storage namespace
declares the server's required Pod Security exception, and readiness checks wait
for the NFS port rather than merely a running container.

Helm operations and workload rollouts have a 15-minute readiness budget for
cold image pulls and storage provisioning. The Linux browser installer also
enforces outer command deadlines: 17 minutes for deployment commands, 30 minutes
for local k3s installation, and two minutes for storage/admin writes. Read-only
installation probes are limited to ten seconds each; the wizard's prerequisite
checks and schema-migration wait retain their own deadlines.

On an outer timeout, the installer terminates the command's process group and
reports the timed-out operation in the browser. Kubernetes resources remain in
place, and a submitted server-side operation may still complete. Inspect the
release/workload state before restarting; the installer does not automatically
replay a timed-out mutation or delete Helm release records. Local command
diagnostics remain available at the private log path printed by the CLI (except
for uploaded-kubeconfig installs).

A release must include all pinned application images and charts. A missing
image tag cannot be fixed by waiting longer. The published-artifact installer
E2E lane must pass before promoting an installer; local candidate-image runs do
not establish that the public downloads are usable.

## Develop

The CLI is a self-contained Rust crate. The browser UI is embedded from `src/bootstrap_web.html`; there is no separate frontend build.

Use Node.js 26 for the browser-wizard tests; `.nvmrc` records the same major version used by CI.

```sh
cargo build --locked
cargo test --locked --bin kubarr
node --test src/bootstrap_web.test.mjs
cargo run --locked -- bootstrap
```

Colour is enabled for interactive terminals and disabled for redirected output, `NO_COLOR`, or `TERM=dumb`.

## Versions and releases

`Cargo.toml` is the source of the version displayed by `kubarr --version`. Version bumps are explicit and independent of the Kubarr application version. After changing the version, run `cargo check` to refresh this package's entry in `Cargo.lock` and commit both files.

The CI workflow runs Rust and browser-wizard tests, then builds native Linux x86_64 and ARM64 binaries:

- A successful push to `main` publishes a commit-specific `cli-<commit>` snapshot and marks it as the latest download.
- A `v*` tag publishes the matching numbered release. CI checks that the tag matches the version embedded in the binary. For example, package version `0.2.4` is released with tag `v0.2.4`.
- Version-tag prereleases such as `v0.2.5-rc1` do not replace the latest download.
- Pull requests test and build without publishing.

Each release contains `kubarr-linux-x86_64.tar.gz`, `kubarr-linux-aarch64.tar.gz`, and their `.sha256` files. Assets are uploaded to a draft before publication so the workflow supports immutable GitHub releases. Rerunning a completed release preserves its published assets.

## Related repositories

- [Kubarr](https://github.com/smokeythebandit/Kubarr): application and integration tests, which pin the CLI revision they use.
- [kubarr-charts](https://github.com/smokeythebandit/kubarr-charts): Helm charts installed by the wizard.
- [kubarr-website](https://github.com/smokeythebandit/kubarr-website): public website and download script.

The CLI history was extracted from `Kubarr/code/cli`, retaining the original commits and authors. Licensed under the [MIT License](LICENSE).
