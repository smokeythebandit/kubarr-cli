#!/usr/bin/env python3
"""Measure per-request gateway overhead against a real installed cluster.

Throwaway diagnostic: builds the acceptance environment with the normal
Journey, then times identical requests in-cluster so port-forward latency
cannot pollute the comparison.

  gateway/healthz    nginx only, no route lookup (location = /healthz)
  gateway/sonarr     path-routed app: route lookup + app auth per request
  direct/sonarr      straight to the app Service, no gateway at all
  backend/route      the /_kubarr_route_lookup backend endpoint alone
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from run import Journey  # noqa: E402

PROBE_IMAGE = ("docker.io/library/python:3.12-alpine@sha256:"
               "1b668429b3511ab407d8e00648891631b0b1a4d7e15e3ca70f38ab5b91ad4ab4")

# Runs inside the cluster. Times each target sequentially after a warmup.
TIMER = r'''
import json, os, ssl, statistics, time, urllib.request

ROUNDS = int(os.environ["ROUNDS"])
TARGETS = json.loads(os.environ["TARGETS"])
results = {}
for name, spec in TARGETS.items():
    samples = []
    for attempt in range(ROUNDS + 2):
        request = urllib.request.Request(spec["url"], headers=spec.get("headers", {}))
        start = time.perf_counter()
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            error.read()
            status = error.code
        except Exception as error:
            samples.append(None)
            results.setdefault(name + "_error", str(error))
            break
        elapsed = (time.perf_counter() - start) * 1000
        if attempt >= 2:                      # discard warmup
            samples.append(elapsed)
        results[name + "_status"] = status
    clean = [s for s in samples if s is not None]
    if clean:
        clean.sort()
        results[name] = {
            "n": len(clean),
            "min": round(clean[0], 1),
            "median": round(statistics.median(clean), 1),
            "p95": round(clean[min(len(clean) - 1, int(len(clean) * 0.95))], 1),
            "max": round(clean[-1], 1),
        }
print("MEASUREMENT " + json.dumps(results))
'''


def sonarr_api_key(journey):
    return journey.kube("-n", "sonarr", "exec", "deployment/sonarr", "-c", "sonarr", "--",
                        "sh", "-c",
                        r"sed -n 's/.*<ApiKey>\([^<]*\)<\/ApiKey>.*/\1/p' /config/config.xml",
                        timeout=30).strip()


LOGIN = r'''
import json, os, urllib.error, urllib.request

URL = "http://kubarr-backend.kubarr-backend.svc.cluster.local:8000/auth/login"
for identifier in json.loads(os.environ["IDENTIFIERS"]):
    body = json.dumps({"username": identifier, "password": os.environ["PW"]}).encode()
    request = urllib.request.Request(URL, data=body,
                                     headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            print("COOKIE " + (response.headers.get("set-cookie") or ""))
            break
    except urllib.error.HTTPError as error:
        print("ATTEMPT %s -> HTTP %s %r" % (identifier, error.code, error.read()[:200]))
'''


def login_cookie(journey):
    """Authenticate as the administrator the browser journey created.

    The administrator name comes from the wizard, so try both the username and
    the e-mail, and report what the database actually holds when neither is
    accepted rather than leaving a bare 401.
    """
    identifiers = json.dumps(["e2e-admin", "e2e@example.invalid"])
    output = journey.run(
        "kubectl", "--kubeconfig", journey.env["KUBECONFIG"], "run", "kubarr-login",
        "--image", PROBE_IMAGE, "--restart=Never", "--rm", "--attach", "--quiet",
        "--env", f"PW={journey.password}", "--env", f"IDENTIFIERS={identifiers}",
        "--command", "--", "python", "-c", LOGIN,
        timeout=240, check=False)
    for line in output.splitlines():
        if line.startswith("COOKIE ") and len(line.strip()) > len("COOKIE"):
            return line[len("COOKIE "):].split(";")[0]
    accounts = journey.kube(
        "-n", "kubarr-database", "exec", "statefulset/kubarr-db", "--",
        "psql", "-U", "kubarr", "-d", "kubarr", "-At", "-c",
        "select username, email, is_active, is_approved from users",
        timeout=60, check=False)
    raise RuntimeError("could not authenticate against the installed backend.\n"
                       f"login attempts:\n{journey.redact(output)}\n"
                       f"accounts in database:\n{accounts}")


def measure(journey, rounds):
    key = sonarr_api_key(journey)
    cookie = login_cookie(journey)
    gateway = "http://kubarr-gateway.openresty.svc.cluster.local:8080"
    backend = "http://kubarr-backend.kubarr-backend.svc.cluster.local:8000"
    targets = {
        "gateway_healthz": {"url": f"{gateway}/healthz"},
        "gateway_sonarr": {"url": f"{gateway}/sonarr/api/v3/system/status",
                           "headers": {"X-Api-Key": key, "Cookie": cookie}},
        "direct_sonarr": {"url": "http://sonarr.sonarr.svc.cluster.local:8989"
                                 "/api/v3/system/status",
                          "headers": {"X-Api-Key": key}},
        "backend_route_lookup": {
            "url": f"{backend}/api/proxy-auth/route?host=localhost&path=/sonarr/",
            "headers": {"Cookie": cookie}},
    }
    output = journey.run(
        "kubectl", "--kubeconfig", journey.env["KUBECONFIG"], "run", "kubarr-measure",
        "--image", PROBE_IMAGE, "--restart=Never", "--rm", "--attach", "--quiet",
        "--env", f"ROUNDS={rounds}", "--env", f"TARGETS={json.dumps(targets)}",
        "--command", "--", "python", "-c", TIMER, timeout=600)
    for line in output.splitlines():
        if line.startswith("MEASUREMENT "):
            return json.loads(line[len("MEASUREMENT "):])
    raise RuntimeError(f"no measurement produced:\n{journey.redact(output)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=HERE.parents[1] / "target/debug/kubarr")
    parser.add_argument("--charts", type=Path)
    parser.add_argument("--artifacts", type=Path, default=HERE / "artifacts")
    parser.add_argument("--rounds", type=int, default=30)
    args = parser.parse_args()
    args.binary = args.binary.resolve()

    journey = Journey(args)
    measurements = None
    try:
        journey.setup()
        journey.browser()
        print("[measure] environment installed; timing requests in-cluster", flush=True)
        measurements = measure(journey, args.rounds)
        print(json.dumps(measurements, indent=2), flush=True)
        (journey.artifacts / "gateway-latency.json").write_text(
            json.dumps(measurements, indent=2))
    finally:
        journey.diagnostics()
        journey.cleanup()
        print(f"[measure] artifacts: {journey.artifacts}", flush=True)
    return 0 if measurements else 1


if __name__ == "__main__":
    raise SystemExit(main())
