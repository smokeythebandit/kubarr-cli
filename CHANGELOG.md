# Changelog

## 0.2.10

- Prepare every writable NFS subdirectory from one shared chart helper as the
  application's own unprivileged identity. Eleven applications previously left
  their directories for kubelet to create as root, which a normal export
  squashes, so they could not start on a root-squashed share at all.
- Replace the privileged `mkdir`/`chown` preparation in Jenkins, SCM-Manager,
  Redmine and Immich, which root squashing defeated, and cover every writable
  mount rather than only each workload's required target.
- Give Sonarr `/media/shows` and Radarr `/media/movies`, matching their location
  in the export, and register each as a root folder without manual setup.
- Serve the library to Jellyfin and Plex read-only at `/media`, so every
  application agrees on one path.
- Pin the regenerated database, logging, metrics, worker and backend charts.

Requires a library stored under `media/shows` and `media/movies`. An existing
installation keeps its files where they are: move them and re-point the root
folders and media libraries before upgrading.

## 0.2.9

- Pin database chart `16.15.0+9`, which prepares its own NFS subdirectory as an
  unprivileged user. Installations against a root-squashed export previously
  stalled on a database rollout that could never finish.
- Report a storage check killed by its deadline as a timeout, naming an
  unreachable or slow server and the NFSv4 recovery grace period after a
  restart, instead of claiming the read/write test went unconfirmed.
- Load acceptance images that Docker's containerd store cannot export by
  platform through the node's own registry pull, naming each one, so a single
  unexportable image no longer ends the run.
- Keep the redacted failure reason in the acceptance run's `result.json`, so a
  failed run is diagnosable from its artifacts alone.

## 0.2.8

- Prepare monitoring storage subdirectories as unprivileged users before NFS
  subPath mounts, supporting fresh external exports with root squashing enabled.
- Preserve VictoriaMetrics' existing UID while allowing the shared media group
  to access prepared directories; retain single-writer monitoring rollouts.
- Update pinned monitoring charts and gateway `1.29.2+5.6`, which preserves API
  error statuses during application outages.
- Exercise the packaged installer against a root-squashed NFS fixture owned by
  UID/GID 1000 rather than a world-writable export.

## 0.2.7

- Select Kubarr `0.3.4` images and platform charts after upstream image-build
  rate limits and stale release-fixture checks blocked previous candidates.
- Retain the installer robustness and release acceptance changes from 0.2.6.
- Pin gateway `1.29.2+5.5`, preserving anonymous OAuth discovery/login while
  keeping OAuth administration and app APIs authenticated.

## 0.2.6

- Use userspace NFS-Ganesha for fresh managed-storage installations, supporting
  Talos without requiring host kernel NFS-server modules. Preserve the server
  implementation of existing managed-NFS releases.
- Pin managed-NFS chart `12.0.0+3` and gateway chart `1.29.2+5.4`, including the
  Pod Security, numeric ownership, HTTP response-framing, and app-routing fixes.
- Wait for actual managed-NFS readiness and report admission failures promptly.
- Give cold image pulls and storage provisioning explicit 15-minute readiness
  budgets, with bounded outer execution deadlines and process-group cleanup.
- Preserve private failure diagnostics and identify timed-out operations in the
  browser without exposing subprocess output or credentials.
- Gate release publication on a real browser installer-to-Sonarr journey using
  the packaged CLI and published charts/images.
- Add recorded clean-snapshot Proxmox validation and an 18-app installation,
  readiness, browser-access, and removal matrix.

Requires the compatible Kubarr `0.3.2` application images. Immich requires a
dedicated hostname; upstream does not support `/immich` subpath hosting. See the
chart migration guide before replacing an older Jellyseerr container affected
by the corrected `/app/config` mount.
