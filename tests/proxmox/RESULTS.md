# Proxmox installer investigation — 2026-10-24

Target: bertha, VM 104 (`kubarr-prod`), Talos Kubernetes 1.36.2, 6 vCPU / 12GiB RAM.
Every numbered run restored the VM's `pre-kubarr` snapshot first. No other VM or
the host's external NFS dataset was modified by these installer runs.

## Final verified candidate

**Run 10: installer passed; 18/18 app tests passed.**

- Platform installation: 111 seconds from Start to finished UI.
- Installer/login/catalog browser journey: 2.0 minutes.
- Complete sequential app matrix: 21.0 minutes.
- Every app installed through the dashboard, reached Running with bound claims,
  rendered its native UI/setup screen, and uninstalled through the dashboard.
- Final platform workloads remained Ready; nine platform releases were deployed.
- No live corrective patches were used to make the final run pass.

| Apps tested | Result |
|-------------|--------|
| Deluge, Frigate, Grafana, Headlamp | Passed |
| Jackett, Jellyfin, Jellyseerr | Passed |
| Plex, qBittorrent, Radarr, Redmine | Passed |
| ruTorrent, SABnzbd, SCM-Manager, Sonarr, Transmission | Passed |
| Jenkins | Passed; native Unlock setup screen verified |
| Immich | Passed on a dedicated hostname; subpath mode unsupported upstream |

Immich's domain and assignment were configured using Settings → Domains. A
reserved `.test` hostname resolved inside Chromium only; no LAN/public DNS was
changed. Native account setup, external integrations, VPN, camera streams, media
downloads, GPU acceleration, and interrupted-install recovery are outside these
default-install smoke tests.

## Findings and fixes

1. Managed NFS was rejected by Talos baseline Pod Security. Its chart now declares
   the required exception in the dedicated storage namespace.
2. The legacy kernel NFS server printed startup success without functioning on
   Talos, where host `nfsd` support was absent. Fresh CLI-managed installs now use
   userspace Ganesha; existing releases retain their implementation.
3. NFSv4 named-owner mapping made PostgreSQL see UID/GID 4294967294 instead of
   999 despite correct server-side ownership. Ganesha uses numeric owners.
4. Rewritten HTML retained stale Content-Length, corrupting proxied responses
   for Deluge, Immich, and Plex. The gateway removes stale length/validators
   and requests uncompressed upstream text for this filter.
5. Frigate needed X-Ingress-Path and native frontend rewriting; its login page
   now renders instead of returning a blank HTTP-200 page.
6. Jellyseerr's private configuration claim was mounted at /config instead of
   /app/config. The corrected chart removes the persistence warning. Existing
   affected containers require backup/migration before replacement.
7. Immich requires a root hostname. The gateway now allows authenticated HTML
   navigation to host-routed app /auth pages while preserving Kubarr auth
   mutations, JSON requests, and host-transfer routes.
8. Installer subprocess deadlines, process-group cleanup, explicit readiness
   budgets, and actionable managed-NFS admission failures improve failure handling.

## Run history

| Run | Outcome |
|-----|---------|
| 01 | Managed-NFS Pod Security admission rejection reproduced |
| 02 | Missing host kernel nfsd support reproduced after admission fix |
| 03 | Candidate regression: removed Helm-3 list flag; corrected |
| 04 | PostgreSQL numeric-owner mismatch reproduced |
| 05 | Complete installer → Sonarr browser journey passed |
| 06 | Installer/Sonarr operation passed; final UI assertion failed after unexpected navigation/additional app operation |
| 07 | 18/18 installs/removals; gateway and visual persistence/routing defects identified |
| 08 | Installer/login passed; candidate-catalog fixture directory-symlink defect corrected |
| 09 | 18/18 installs/removals; stricter assertions exposed Immich subpath limitation and Jenkins/Plex assertion errors |
| 10 | Installer and all 18 supported-configuration UI tests passed |

Early deterministic failures were stopped by the investigator after diagnosis;
their individual observation notes disclose those interventions. They are not
represented as installer-detected failures or passing runs.

## Evidence and reproduction

- Local evidence viewer: `http://127.0.0.1:18092`
- Final artifacts: `/tmp/opencode/kubarr-proxmox-run-10/`
- Installer report/video: `report/` and `browser/`
- App report/videos: `apps-report/` and `apps-browser/`
- Per-app results, screenshots and diagnostics: `apps/<name>/`
- See [README.md](README.md) for destructive-run authorization and reproduction.

Candidate binary, images, charts, and browser-source digests identify the tested
inputs. Reports scrub the generated administrator password, invitation, and JWT
sessions; private diagnostics/auth state are stored separately.

## Release status

**These are local candidate results, not public-release validation.** The
investigation found the public 0.3.1 backend/frontend/worker tags unavailable and
the latest downloadable CLI older than the pinned source. Corrected charts and
compatible images must be published, then the installer must pass its real
published-artifact journey before promotion. The final VM uses an explicitly
identified in-cluster candidate chart registry; app releases were removed after
testing and claims follow normal retention behavior.
