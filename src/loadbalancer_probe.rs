//! Explicit, selected-context LoadBalancer allocation check. No workload or endpoints.
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::checks::{self, ProbeResult};
use crate::install_access::GATEWAY_NAMESPACE;
use crate::types::{BOOTSTRAP_RELEASE_NAMESPACE, GATEWAY_CHART_REF};
use crate::util::chart_ref;

pub const VALIDITY: Duration = Duration::from_secs(300);
const ALLOCATION: Duration = Duration::from_secs(45);
const CLEANUP: Duration = Duration::from_secs(120);

#[derive(Clone, Serialize)]
pub struct Stage {
    pub id: &'static str,
    pub label: &'static str,
    pub severity: &'static str,
    pub state: &'static str,
    pub message: String,
}

pub fn stages() -> Vec<Stage> {
    [
        ("namespace", "Prepare gateway namespace"),
        ("service", "Create temporary LoadBalancer Service"),
        ("address", "Wait for assigned address"),
        ("cleanup", "Remove temporary Service"),
    ]
    .into_iter()
    .map(|(id, label)| Stage {
        id,
        label,
        severity: "required",
        state: "pending",
        message: String::new(),
    })
    .collect()
}

pub fn eligible(stages: &[Stage]) -> bool {
    stages.len() == 4 && stages.iter().all(|stage| stage.state == "passed")
}

fn random() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| "Could not generate a unique LoadBalancer check name".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct Probe {
    name: String,
    marker: String,
    chart_version: String,
    namespace: Value,
    service: Value,
}

impl Probe {
    fn new(cancelled: &AtomicBool, end: Instant) -> Result<Self, String> {
        let chart = chart_metadata(cancelled, end)?;
        Self::from_chart(chart)
    }

    fn from_chart(chart: ChartMetadata) -> Result<Self, String> {
        let marker = random()?;
        let name = format!("kubarr-lb-check-{marker}");
        let mut namespace = chart.namespace;
        namespace["metadata"]["annotations"]["meta.helm.sh/release-name"] = json!("openresty");
        namespace["metadata"]["annotations"]["meta.helm.sh/release-namespace"] =
            json!(BOOTSTRAP_RELEASE_NAMESPACE);
        namespace["metadata"]["annotations"]["kubarr.io/loadbalancer-check"] = json!(marker);
        let mut service = chart.service;
        service["metadata"]["name"] = json!(name);
        service["metadata"]["annotations"]["kubarr.io/loadbalancer-check"] = json!(marker);
        // No selector, endpoints or workloads: the check tests allocation only.
        service["spec"]
            .as_object_mut()
            .ok_or("Invalid rendered gateway Service")?
            .remove("selector");
        Ok(Self {
            name,
            marker,
            chart_version: chart.version,
            namespace,
            service,
        })
    }

    fn own(&self, value: &Value, kind: &str) -> bool {
        value["kind"] == kind
            && value["metadata"]["name"]
                == if kind == "Service" {
                    &self.name
                } else {
                    GATEWAY_NAMESPACE
                }
            && value["metadata"]["annotations"]["kubarr.io/loadbalancer-check"] == self.marker
            && (kind == "Namespace" || value["metadata"]["namespace"] == GATEWAY_NAMESPACE)
    }

    fn adopt(&self, value: &Value, kind: &str) -> Option<String> {
        let manifest = if kind == "Service" {
            &self.service
        } else {
            &self.namespace
        };
        (self.own(value, kind)
            && value["metadata"]["creationTimestamp"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
            && (kind != "Service"
                || (value["spec"]["type"] == "LoadBalancer"
                    && value["spec"]["ports"][0]["port"] == 8080
                    && value["spec"]["ports"][0]["protocol"]
                        == manifest["spec"]["ports"][0]["protocol"]
                    && value["spec"]["ports"][0]["targetPort"]
                        == manifest["spec"]["ports"][0]["targetPort"]
                    && value["spec"]["selector"].is_null()
                    && value["spec"]["loadBalancerClass"]
                        == manifest["spec"]["loadBalancerClass"]))
            && value["metadata"]["labels"] == manifest["metadata"]["labels"]
            && manifest["metadata"]["annotations"]
                .as_object()
                .is_some_and(|annotations| {
                    annotations
                        .iter()
                        .all(|(key, expected)| value["metadata"]["annotations"][key] == *expected)
                })
            && (kind != "Service"
                || [
                    "externalTrafficPolicy",
                    "allocateLoadBalancerNodePorts",
                    "loadBalancerIP",
                    "loadBalancerSourceRanges",
                ]
                .iter()
                .all(|key| {
                    manifest["spec"][key].is_null() || value["spec"][key] == manifest["spec"][key]
                }))
            && (kind != "Namespace"
                || (value["metadata"]["annotations"]["meta.helm.sh/release-name"] == "openresty"
                    && value["metadata"]["annotations"]["meta.helm.sh/release-namespace"]
                        == BOOTSTRAP_RELEASE_NAMESPACE))
            && manifest["metadata"]["name"] == value["metadata"]["name"])
            .then(|| uid(value).map(str::to_owned))
            .flatten()
    }
}

#[derive(Clone)]
struct ChartMetadata {
    version: String,
    namespace: Value,
    service: Value,
}

fn chart_metadata(cancelled: &AtomicBool, end: Instant) -> Result<ChartMetadata, String> {
    #[cfg(test)]
    if let Some(chart) = CHART_MOCK.with(|mock| mock.borrow().clone()) {
        return Ok(chart);
    }
    const UNAVAILABLE: &str = "Could not resolve and render the gateway chart metadata. Check Helm and chart access, then retry the LoadBalancer test.";
    let chart = chart_ref("openresty", GATEWAY_CHART_REF);
    let invoke = |args: &[&str]| {
        checks::probe_with_timeout("helm", args, cancelled, end, Duration::from_secs(25))
            .filter(|out| out.success)
            .map(|out| out.output)
            .ok_or_else(|| UNAVAILABLE.to_string())
    };
    let info = invoke(&["show", "chart", &chart])?;
    let metadata: Value = serde_yaml::from_str(&info).map_err(|_| UNAVAILABLE)?;
    let version = metadata["version"]
        .as_str()
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".+-_".contains(&c))
        })
        .ok_or(UNAVAILABLE)?
        .to_owned();
    if metadata["name"] != "openresty" {
        return Err(UNAVAILABLE.into());
    }
    let local = std::env::var_os("KUBARR_CHARTS_DIR").is_some();
    let mut args = vec![
        "template",
        "openresty",
        &chart,
        "-n",
        BOOTSTRAP_RELEASE_NAMESPACE,
    ];
    if !local {
        args.extend(["--version", &version]);
    }
    args.extend([
        "--set",
        "namespace.name=openresty",
        "--set",
        "service.type=LoadBalancer",
        "--set",
        "service.port=8080",
        "--show-only",
        "templates/namespace.yaml",
        "--show-only",
        "templates/service.yaml",
    ]);
    let rendered = invoke(&args)?;
    parse_rendered(version, &rendered)
}

fn parse_rendered(version: String, rendered: &str) -> Result<ChartMetadata, String> {
    const UNAVAILABLE: &str = "Could not resolve and render the gateway chart metadata. Check Helm and chart access, then retry the LoadBalancer test.";
    let mut namespace = None;
    let mut service = None;
    for doc in serde_yaml::Deserializer::from_str(&rendered) {
        let value: Value = Value::deserialize(doc).map_err(|_| UNAVAILABLE)?;
        match value["kind"].as_str() {
            Some("Namespace") if namespace.is_none() => namespace = Some(value),
            Some("Service") if service.is_none() => service = Some(value),
            _ => return Err(UNAVAILABLE.into()),
        }
    }
    let namespace = namespace.ok_or(UNAVAILABLE)?;
    let service = service.ok_or(UNAVAILABLE)?;
    if namespace["metadata"]["name"] != GATEWAY_NAMESPACE
        || service["metadata"]["name"] != "kubarr-gateway"
        || service["metadata"]["namespace"] != GATEWAY_NAMESPACE
        || service["spec"]["type"] != "LoadBalancer"
        || service["spec"]["ports"]
            .as_array()
            .is_none_or(|ports| ports.len() != 1 || ports[0]["port"] != 8080)
        || namespace["metadata"]["labels"]["app.kubernetes.io/managed-by"] != "Helm"
        || !namespace["metadata"]["labels"].is_object()
        || !service["metadata"]["labels"].is_object()
    {
        return Err(UNAVAILABLE.into());
    }
    Ok(ChartMetadata {
        version,
        namespace,
        service,
    })
}

/// For a local override, a changed chart version invalidates the earlier
/// render; OCI installs instead use Helm's --version for the gateway release.
pub fn chart_version_still_selected(version: &str) -> bool {
    let Some(dir) = std::env::var_os("KUBARR_CHARTS_DIR") else {
        return true;
    };
    let path = std::path::Path::new(&dir).join("openresty/Chart.yaml");
    std::fs::read_to_string(path)
        .ok()
        .filter(|content| content.len() <= 32 * 1024)
        .and_then(|content| serde_yaml::from_str::<Value>(&content).ok())
        .is_some_and(|chart| chart["name"] == "openresty" && chart["version"] == version)
}

#[cfg(test)]
thread_local! {
    static CHART_MOCK: std::cell::RefCell<Option<ChartMetadata>> = const { std::cell::RefCell::new(None) };
}

fn uid(value: &Value) -> Option<&str> {
    value["metadata"]["uid"].as_str().filter(|v| !v.is_empty())
}

// Private manifest file: stdin can hang behind authentication plugins. Never
// reflect stdout/stderr into the browser. Existing NFS and baseline probes use
// the same bounded process-group executor.
fn kubectl(
    args: &[&str],
    manifest: Option<&Value>,
    cancelled: &AtomicBool,
    end: Instant,
) -> Option<ProbeResult> {
    #[cfg(test)]
    if let Some(result) =
        MOCK.with(|mock| mock.borrow().as_ref().map(|runner| runner(args, manifest)))
    {
        return result;
    }
    if let Some(manifest) = manifest {
        let path = std::env::temp_dir().join(format!("kubarr-lb-manifest-{}.json", random().ok()?));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).ok()?;
        let result = (|| {
            file.write_all(serde_json::to_string(manifest).ok()?.as_bytes())
                .ok()?;
            drop(file);
            let mut command = if args.is_empty() {
                vec!["create", "-o", "json"]
            } else {
                args.to_vec()
            };
            command.extend(["-f", path.to_str()?]);
            checks::probe("kubectl", &command, cancelled, end)
        })();
        let _ = fs::remove_file(path);
        result
    } else {
        checks::probe("kubectl", args, cancelled, end)
    }
}

#[cfg(test)]
thread_local! {
    static MOCK: std::cell::RefCell<Option<Box<dyn Fn(&[&str], Option<&Value>) -> Option<ProbeResult>>>> = std::cell::RefCell::new(None);
}

fn read(
    kind: &str,
    name: &str,
    namespace: Option<&str>,
    stop: &AtomicBool,
    end: Instant,
) -> Result<Option<Value>, ()> {
    let mut args = vec!["get", kind, name, "--ignore-not-found", "-o", "json"];
    if let Some(namespace) = namespace {
        args.extend(["-n", namespace]);
    }
    let output = kubectl(&args, None, stop, end)
        .filter(|out| out.success)
        .ok_or(())?;
    if output.output.trim().is_empty() {
        Ok(None)
    } else {
        serde_json::from_str(&output.output)
            .map(Some)
            .map_err(|_| ())
    }
}

fn assigned(value: &Value) -> bool {
    value
        .pointer("/status/loadBalancer/ingress")
        .and_then(Value::as_array)
        .is_some_and(|ingress| {
            ingress.iter().any(|entry| {
                entry["ip"]
                    .as_str()
                    .is_some_and(|ip| ip.parse::<std::net::IpAddr>().is_ok())
                    || entry["hostname"]
                        .as_str()
                        .is_some_and(crate::install_access::valid_hostname)
            })
        })
}

fn delete(probe: &Probe, expected: &str, end: Instant) -> bool {
    let stop = AtomicBool::new(false);
    let Ok(Some(found)) = read("Service", &probe.name, Some(GATEWAY_NAMESPACE), &stop, end) else {
        return false;
    };
    if !probe.own(&found, "Service") || uid(&found) != Some(expected) {
        return false;
    }
    let path = format!(
        "/api/v1/namespaces/{GATEWAY_NAMESPACE}/services/{}",
        probe.name
    );
    let options =
        json!({"apiVersion":"v1","kind":"DeleteOptions","preconditions":{"uid":expected}});
    if !kubectl(&["delete", "--raw", &path], Some(&options), &stop, end)
        .is_some_and(|out| out.success)
    {
        return false;
    }
    while Instant::now() < end {
        match read("Service", &probe.name, Some(GATEWAY_NAMESPACE), &stop, end) {
            Ok(None) => return true,
            Ok(Some(value)) if probe.own(&value, "Service") && uid(&value) == Some(expected) => {}
            _ => return false,
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false // Never remove provider finalizers.
}

fn namespace_ready(value: &Value, probe: &Probe, created: bool) -> bool {
    value["kind"] == "Namespace"
        && value["metadata"]["name"] == GATEWAY_NAMESPACE
        && value["metadata"]["labels"]["app.kubernetes.io/managed-by"] == "Helm"
        && value["metadata"]["annotations"]["meta.helm.sh/release-name"] == "openresty"
        && value["metadata"]["annotations"]["meta.helm.sh/release-namespace"]
            == BOOTSTRAP_RELEASE_NAMESPACE
        && probe.namespace["metadata"]["labels"]
            .as_object()
            .is_some_and(|labels| {
                labels
                    .iter()
                    .all(|(key, expected)| value["metadata"]["labels"][key] == *expected)
            })
        && probe.namespace["metadata"]["annotations"]
            .as_object()
            .is_some_and(|annotations| {
                annotations
                    .iter()
                    .filter(|(key, _)| key.as_str() != "kubarr.io/loadbalancer-check")
                    .all(|(key, expected)| value["metadata"]["annotations"][key] == *expected)
            })
        && (!created || probe.own(value, "Namespace"))
}

fn report(
    publish: &mut dyn FnMut(usize, &'static str, String),
    i: usize,
    state: &'static str,
    message: &str,
) {
    publish(i, state, message.into());
}

pub fn run(
    cancelled: &AtomicBool,
    publish: &mut dyn FnMut(usize, &'static str, String),
) -> Result<String, String> {
    run_with_limit(cancelled, publish, ALLOCATION)
}

fn run_with_limit(
    cancelled: &AtomicBool,
    publish: &mut dyn FnMut(usize, &'static str, String),
    allocation: Duration,
) -> Result<String, String> {
    report(
        publish,
        0,
        "running",
        "Resolving the selected gateway chart's labels and version…",
    );
    let probe = match Probe::new(cancelled, Instant::now() + Duration::from_secs(60)) {
        Ok(probe) => probe,
        Err(message) => {
            report(publish, 0, "failed", &message);
            return Err(message);
        }
    };
    let end = Instant::now() + allocation;
    let mut namespace_attempted = false;
    let mut namespace_uid: Option<String> = None;
    let mut service_attempted = false;
    let mut service_uid = None;
    let mut uncertain = false;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report(publish, 0, "running", "Checking the gateway namespace…");
        let existing = read("namespace", GATEWAY_NAMESPACE, None, cancelled, end)
            .map_err(|_| "Could not inspect the gateway namespace; check API access and RBAC.")?;
        if existing.is_none() {
            namespace_attempted = true;
            let created = kubectl(&[], Some(&probe.namespace), cancelled, end);
            uncertain |= created.is_none();
            namespace_uid = created.as_ref().filter(|out| out.success).and_then(|out| serde_json::from_str::<Value>(&out.output).ok())
                .and_then(|value| probe.adopt(&value, "Namespace"));
            if namespace_uid.is_none() { return Err("Could not create the temporary gateway namespace. Check namespace permissions and admission."); }
        } else if !namespace_ready(existing.as_ref().unwrap(), &probe, false) {
            return Err("Existing openresty namespace does not have the selected gateway chart labels and Helm ownership. Kubarr will not modify it; inspect its ownership before retrying.");
        }
        report(publish, 0, "passed", if namespace_attempted { "Prepared openresty namespace with Helm ownership; it will be retained for the gateway." } else { "Using the existing Helm-owned openresty namespace without modifying it." });
        report(publish, 1, "running", "Requesting a temporary LoadBalancer address (provider charges may apply)…");
        service_attempted = true;
        let created = kubectl(&[], Some(&probe.service), cancelled, end);
        uncertain |= created.is_none();
            service_uid = created.as_ref().filter(|out| out.success).and_then(|out| serde_json::from_str::<Value>(&out.output).ok())
                .and_then(|value| probe.adopt(&value, "Service"));
        if service_uid.is_none() { return Err("Could not create the temporary LoadBalancer Service. Check service RBAC, admission and quota."); }
        report(publish, 1, "passed", "Selectorless Service created with no endpoints or Pods.");
        report(publish, 2, "running", "Waiting up to 45 seconds for a LoadBalancer IP or hostname…");
        loop {
            if cancelled.load(Ordering::Acquire) { return Err("LoadBalancer check cancelled."); }
            if Instant::now() >= end { return Err("No LoadBalancer address was assigned within 45 seconds. Configure an allocator such as MetalLB or Cilium, verify its address pool, then retry."); }
            let service = read("service", &probe.name, Some(GATEWAY_NAMESPACE), cancelled, end)
                .map_err(|_| "Could not read the temporary Service status. Check API access and RBAC.")?
                .ok_or("Temporary Service disappeared before address allocation.")?;
            if !probe.own(&service, "Service") || uid(&service) != service_uid.as_deref() {
                return Err("Temporary Service identity changed. Check it before cleanup.");
            }
            if assigned(&service) { break; }
            std::thread::sleep(Duration::from_millis(500));
        }
        report(publish, 2, "passed", "The selected cluster assigned an IP or hostname to the temporary Service.");
        Ok(())
    })).unwrap_or(Err("LoadBalancer check interrupted unexpectedly; verifying cleanup."));

    report(
        publish,
        3,
        "running",
        "Deleting the temporary Service and waiting for Kubernetes API absence…",
    );
    let cleanup_end = Instant::now() + CLEANUP;
    let stop = AtomicBool::new(false);
    // A timed-out create may have reached the API. Only adopt an exact tagged
    // object with matching critical spec, creation time and freshly read UID.
    if service_attempted && service_uid.is_none() {
        if let Ok(Some(value)) = read(
            "service",
            &probe.name,
            Some(GATEWAY_NAMESPACE),
            &stop,
            cleanup_end,
        ) {
            service_uid = probe.adopt(&value, "Service");
        }
    }
    let service_clean = !service_attempted
        || (match service_uid.as_deref() {
            Some(uid) => delete(&probe, uid, cleanup_end),
            None if !uncertain => matches!(
                read(
                    "service",
                    &probe.name,
                    Some(GATEWAY_NAMESPACE),
                    &stop,
                    cleanup_end
                ),
                Ok(None)
            ),
            None => false,
        });
    let namespace_ready_after = read("namespace", GATEWAY_NAMESPACE, None, &stop, cleanup_end)
        .is_ok_and(|found| {
            found.is_some_and(|value| {
                namespace_ready(&value, &probe, namespace_attempted)
                    && (!namespace_attempted || uid(&value) == namespace_uid.as_deref())
            })
        });
    if !service_clean
        || (result.is_ok() && !namespace_ready_after)
        || (namespace_attempted && !namespace_ready_after)
        || uncertain
    {
        let message = format!("Temporary Service deletion or prepared namespace identity could not be confirmed (or a create timed out). Inspect Service {}/{} and retained namespace {} in the selected context; do not remove provider finalizers or unrelated resources.", GATEWAY_NAMESPACE, probe.name, GATEWAY_NAMESPACE);
        eprintln!("{message}");
        report(publish, 3, "failed", &message);
        return Err(message);
    }
    report(publish, 3, "passed", "Temporary Service absent from Kubernetes API; openresty namespace retained. Provider-side release may lag if no finalizer was used.");
    if let Err(reason) = result {
        if !cancelled.load(Ordering::Acquire) {
            report(
                publish,
                if service_attempted {
                    if service_uid.is_some() {
                        2
                    } else {
                        1
                    }
                } else {
                    0
                },
                "failed",
                reason,
            );
        }
        return Err(reason.into());
    }
    if cancelled.load(Ordering::Acquire) {
        return Err("LoadBalancer check cancelled".into());
    }
    Ok(probe.chart_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn chart() -> ChartMetadata {
        let labels = json!({"app.kubernetes.io/name":"kubarr-gateway","app.kubernetes.io/instance":"openresty",
            "app.kubernetes.io/managed-by":"Helm","helm.sh/chart":"openresty-1.29.2_5.3"});
        ChartMetadata {
            version: "1.29.2+5.3".into(),
            namespace: json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":GATEWAY_NAMESPACE,"labels":labels}}),
            service: json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"kubarr-gateway","namespace":GATEWAY_NAMESPACE,
                "labels":{"app.kubernetes.io/name":"kubarr-gateway","app.kubernetes.io/instance":"openresty",
                    "app.kubernetes.io/managed-by":"Helm","app.kubernetes.io/component":"gateway","helm.sh/chart":"openresty-1.29.2_5.3"}},
                "spec":{"type":"LoadBalancer","ports":[{"name":"http","protocol":"TCP","port":8080,"targetPort":"http"}],
                    "selector":{"app.kubernetes.io/name":"kubarr-gateway"}}}),
        }
    }

    fn fixture_probe() -> Probe {
        Probe::from_chart(chart()).unwrap()
    }

    #[test]
    fn rendered_labels_include_chart_version_and_no_probe_selector_label() {
        let template = chart();
        let docs = format!(
            "{}\n---\n{}",
            serde_yaml::to_string(&template.namespace).unwrap(),
            serde_yaml::to_string(&template.service).unwrap()
        );
        let parsed = parse_rendered(template.version.clone(), &docs).unwrap();
        let probe = Probe::from_chart(parsed).unwrap();
        assert_eq!(
            probe.service["metadata"]["labels"],
            template.service["metadata"]["labels"]
        );
        assert_eq!(
            probe.namespace["metadata"]["labels"],
            template.namespace["metadata"]["labels"]
        );
        assert_eq!(
            probe.service["metadata"]["labels"]["helm.sh/chart"],
            "openresty-1.29.2_5.3"
        );
        assert!(probe.service["metadata"]["labels"]["kubarr.io/loadbalancer-check"].is_null());
        assert!(probe.namespace["metadata"]["labels"]["kubarr.io/loadbalancer-check"].is_null());
        assert!(probe.service["spec"]["selector"].is_null());
        // A pool's "helm.sh/chart DoesNotExist" selector must not match the
        // probe when the eventual gateway has the rendered chart label.
        assert!(probe.service["metadata"]["labels"]["helm.sh/chart"].is_string());
        assert!(parse_rendered(
            template.version,
            &serde_yaml::to_string(&template.service).unwrap()
        )
        .is_err());
    }

    #[test]
    fn existing_foreign_namespace_is_never_modified_or_approved() {
        CHART_MOCK.with(|mock| *mock.borrow_mut() = Some(chart()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let foreign = json!({"kind":"Namespace","metadata":{"name":GATEWAY_NAMESPACE,"uid":"foreign",
            "labels":{"app.kubernetes.io/managed-by":"Other"},"annotations":{"meta.helm.sh/release-name":"someone-else"}}});
        MOCK.with(|mock| {
            *mock.borrow_mut() = Some(Box::new(move |args, _| {
                seen.lock()
                    .unwrap()
                    .push(args.first().copied().unwrap_or("").to_string());
                Some(ProbeResult {
                    success: true,
                    output: foreign.to_string(),
                })
            }))
        });
        let mut stages = stages();
        let result = run(&AtomicBool::new(false), &mut |i, state, message| {
            stages[i].state = state;
            stages[i].message = message;
        });
        MOCK.with(|mock| *mock.borrow_mut() = None);
        CHART_MOCK.with(|mock| *mock.borrow_mut() = None);
        assert!(result.is_err());
        assert!(!eligible(&stages));
        let calls = calls.lock().unwrap();
        assert!(!calls
            .iter()
            .any(|call| call == "create" || call == "delete" || call == "patch"));
    }

    #[derive(Default)]
    struct Fake {
        namespace: Option<Value>,
        service: Option<Value>,
        unrelated_pod: bool,
        calls: Vec<String>,
    }

    fn simulate(
        existing: bool,
        hostname: bool,
        pending: bool,
        extra: bool,
        stuck: bool,
        timeout_create: bool,
        replacement: bool,
    ) -> (Result<String, String>, Vec<Stage>, Vec<String>) {
        let state = Arc::new(Mutex::new(Fake::default()));
        if existing {
            let probe = fixture_probe();
            let mut namespace = probe.namespace;
            namespace["metadata"]["uid"] = json!("existing");
            namespace["metadata"]["annotations"]
                .as_object_mut()
                .unwrap()
                .remove("kubarr.io/loadbalancer-check");
            state.lock().unwrap().namespace = Some(namespace);
        }
        CHART_MOCK.with(|mock| *mock.borrow_mut() = Some(chart()));
        let saved = state.clone();
        MOCK.with(|mock| {
            *mock.borrow_mut() = Some(Box::new(move |args, manifest| {
                let mut state = saved.lock().unwrap();
                let ok = |output: String| {
                    Some(ProbeResult {
                        success: true,
                        output,
                    })
                };
                let fail = || {
                    Some(ProbeResult {
                        success: false,
                        output: "private credentials".into(),
                    })
                };
                if args.is_empty() {
                    let value = manifest.unwrap();
                    let kind = value["kind"].as_str().unwrap();
                    state.calls.push(format!("create {kind}"));
                    let mut created = value.clone();
                    created["metadata"]["uid"] = json!(format!("owned-{kind}"));
                    created["metadata"]["creationTimestamp"] = json!("2026-10-12T00:00:00Z");
                    if kind == "Namespace" {
                        state.namespace = Some(created.clone());
                    } else {
                        created["status"] = if pending {
                            json!({"loadBalancer":{"ingress":[]}})
                        } else if hostname {
                            json!({"loadBalancer":{"ingress":[{"hostname":"lb.example.org"}]}})
                        } else {
                            json!({"loadBalancer":{"ingress":[{"ip":"192.0.2.4"}]}})
                        };
                        state.service = Some(created.clone());
                        if extra {
                            state.unrelated_pod = true;
                        }
                    }
                    if kind == "Service" && timeout_create {
                        return None;
                    }
                    return ok(created.to_string());
                }
                if args.first() == Some(&"delete") {
                    state.calls.push("delete Service".into());
                    let expected = manifest.unwrap()["preconditions"]["uid"].as_str().unwrap();
                    let object = &mut state.service;
                    if replacement {
                        object.as_mut().unwrap()["metadata"]["uid"] = json!("replacement");
                    }
                    if object.as_ref().and_then(uid) != Some(expected) {
                        return fail();
                    }
                    if !stuck {
                        *object = None;
                    }
                    return ok("{}".into());
                }
                if args.first() == Some(&"get") {
                    let object = if args[1].eq_ignore_ascii_case("service") {
                        &state.service
                    } else {
                        &state.namespace
                    };
                    return ok(object.as_ref().map(Value::to_string).unwrap_or_default());
                }
                fail()
            }))
        });
        let mut stages = stages();
        let result = run_with_limit(
            &AtomicBool::new(false),
            &mut |i, stage, message| {
                stages[i].state = stage;
                stages[i].message = message;
            },
            if pending {
                Duration::from_millis(10)
            } else {
                Duration::from_secs(2)
            },
        );
        MOCK.with(|mock| *mock.borrow_mut() = None);
        CHART_MOCK.with(|mock| *mock.borrow_mut() = None);
        if extra {
            assert!(
                state.lock().unwrap().unrelated_pod,
                "concurrent Pod must remain untouched"
            );
        }
        if !existing {
            let saved = state.lock().unwrap();
            let ns = saved
                .namespace
                .as_ref()
                .expect("prepared namespace retained");
            assert_eq!(
                ns["metadata"]["annotations"]["meta.helm.sh/release-name"],
                "openresty"
            );
            assert_eq!(
                ns["metadata"]["annotations"]["meta.helm.sh/release-namespace"],
                "default"
            );
            assert_eq!(
                ns["metadata"]["labels"]["helm.sh/chart"],
                "openresty-1.29.2_5.3"
            );
        } else {
            let saved = state.lock().unwrap();
            let ns = saved
                .namespace
                .as_ref()
                .expect("existing namespace retained");
            assert_eq!(ns["metadata"]["uid"], "existing");
            assert!(ns["metadata"]["annotations"]["kubarr.io/loadbalancer-check"].is_null());
        }
        let calls = state.lock().unwrap().calls.clone();
        (result, stages, calls)
    }

    #[test]
    fn address_assignment_retains_prepared_namespace_with_helm_ownership() {
        for hostname in [true, false] {
            let (result, stages, calls) =
                simulate(false, hostname, false, false, false, false, false);
            assert!(
                result.is_ok(),
                "{result:?} {calls:?} {:?}",
                stages
                    .iter()
                    .map(|s| (&s.id, &s.state, &s.message))
                    .collect::<Vec<_>>()
            );
            assert!(eligible(&stages));
            assert_eq!(
                calls,
                ["create Namespace", "create Service", "delete Service"]
            );
        }
        let (result, stages, calls) = simulate(true, false, false, false, false, false, false);
        assert!(result.is_ok(), "{result:?}");
        assert!(eligible(&stages));
        assert_eq!(calls, ["create Service", "delete Service"]);
    }

    #[test]
    fn pending_address_replacement_and_uncertain_creation_never_pass() {
        let (result, stages, calls) = simulate(true, false, true, false, false, false, false);
        assert!(result.unwrap_err().contains("No LoadBalancer address"));
        assert!(!eligible(&stages));
        assert!(calls.contains(&"delete Service".to_string()));
        let (result, stages, _) = simulate(true, false, false, false, false, true, false);
        assert!(result.is_err());
        assert!(!eligible(&stages));
        let (result, stages, _) = simulate(true, false, false, false, false, false, true);
        assert!(result.is_err());
        assert_eq!(stages[3].state, "failed");
    }

    #[test]
    fn concurrent_user_pod_does_not_trigger_cascading_deletion() {
        let (result, stages, calls) = simulate(false, false, false, true, false, false, false);
        assert!(result.is_ok());
        assert!(eligible(&stages));
        assert!(!calls.contains(&"delete Namespace".to_string()));
    }

    #[test]
    fn finalizer_pending_stays_present_and_uid_delete_never_forces_it() {
        let probe = fixture_probe();
        let object = json!({"kind":"Service","metadata":{"name":probe.name,"namespace":GATEWAY_NAMESPACE,"uid":"owned-Service",
            "labels":probe.service["metadata"]["labels"],"annotations":probe.service["metadata"]["annotations"],"finalizers":["service.kubernetes.io/load-balancer-cleanup"]}});
        MOCK.with(|mock| {
            *mock.borrow_mut() = Some(Box::new(move |args, manifest| {
                if args.first() == Some(&"delete") {
                    assert_eq!(manifest.unwrap()["preconditions"]["uid"], "owned-Service");
                    return Some(ProbeResult {
                        success: true,
                        output: "{}".into(),
                    });
                }
                Some(ProbeResult {
                    success: true,
                    output: object.to_string(),
                })
            }))
        });
        assert!(!delete(
            &probe,
            "owned-Service",
            Instant::now() + Duration::from_millis(5)
        ));
        MOCK.with(|mock| *mock.borrow_mut() = None);
    }

    #[test]
    fn admission_denied_create_reports_curated_failure_and_never_approves() {
        CHART_MOCK.with(|mock| *mock.borrow_mut() = Some(chart()));
        let probe = fixture_probe();
        let mut existing = probe.namespace;
        existing["metadata"]["uid"] = json!("existing");
        MOCK.with(|mock| {
            *mock.borrow_mut() = Some(Box::new(move |args, _| {
                if args.first() == Some(&"get") {
                    return Some(ProbeResult {
                        success: true,
                        output: if args.get(1) == Some(&"namespace") {
                            existing.to_string()
                        } else {
                            String::new()
                        },
                    });
                }
                Some(ProbeResult {
                    success: false,
                    output: "private cloud token".into(),
                })
            }))
        });
        let mut stages = stages();
        let result = run(&AtomicBool::new(false), &mut |i, state, message| {
            stages[i].state = state;
            stages[i].message = message;
        });
        MOCK.with(|mock| *mock.borrow_mut() = None);
        CHART_MOCK.with(|mock| *mock.borrow_mut() = None);
        assert!(result.unwrap_err().contains("RBAC, admission and quota"));
        assert!(!eligible(&stages));
        assert!(!stages
            .iter()
            .any(|s| s.message.contains("private cloud token")));
    }
}
