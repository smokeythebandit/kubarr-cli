# Changelog

## 0.2.7

- Select Kubarr `0.3.4` images and platform charts after upstream image-build
  rate limits and stale release-fixture checks blocked previous candidates.
- Retain the installer robustness and release acceptance changes from 0.2.6.

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
