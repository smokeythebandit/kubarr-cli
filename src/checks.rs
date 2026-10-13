use crate::checks_data::{REQUIRED_API_RESOURCES, REQUIRED_PERMISSIONS};
use crate::style::{detail, fail, ok, step, warn};
use crate::types::{BootstrapOptions, StorageModeOption};
use crate::util::{
    api_resource_available, can_i, command_output, command_success, default_storage_class,
    kubectl_cluster_access, kubectl_server_git_version, ready_node_count,
};

use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

const PROBE_LIMIT: usize = 32 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
pub const CHECK_DEADLINE: Duration = Duration::from_secs(90);

#[derive(Clone, Serialize)]
pub struct Check {
    pub id: String,
    pub label: String,
    pub severity: &'static str,
    pub state: &'static str,
    pub message: String,
}

fn check(id: impl Into<String>, label: impl Into<String>, severity: &'static str) -> Check {
    Check {
        id: id.into(),
        label: label.into(),
        severity,
        state: "pending",
        message: String::new(),
    }
}

pub fn initial_checks() -> Vec<Check> {
    let mut checks = vec![
        check("kubectl", "kubectl", "required"),
        check("helm", "Helm 4", "required"),
        check("api", "Kubernetes API", "required"),
        check("server-version", "Server version", "advisory"),
        check("nodes", "Ready nodes", "required"),
    ];
    checks.extend(REQUIRED_API_RESOURCES.iter().map(|req| {
        check(
            format!("resource-{}.{}", req.resource, req.api_group),
            format!("{} API", req.label),
            "required",
        )
    }));
    checks.extend(REQUIRED_PERMISSIONS.iter().map(|req| {
        check(
            format!(
                "permission-{}-{}-{}",
                req.verb,
                req.resource,
                req.namespace.unwrap_or("cluster")
            ),
            format!(
                "Sample: {} {} ({})",
                req.verb,
                req.resource,
                req.namespace.unwrap_or("cluster scope")
            ),
            "required",
        )
    }));
    checks.push(check("storage", "Storage classes", "advisory"));
    checks.push(check("metrics", "Metrics API", "advisory"));
    checks.push(check("gpu", "GPU support", "advisory"));
    checks
}

pub fn eligible(checks: &[Check]) -> bool {
    checks
        .iter()
        .all(|item| item.severity != "required" || item.state == "passed")
}

fn simple_check(
    index: &mut usize,
    publish: &mut impl FnMut(usize, &'static str, &'static str),
    cancelled: &AtomicBool,
    deadline: Instant,
    args: &[&str],
    pass: &'static str,
    fail: &'static str,
    advisory: bool,
) -> bool {
    let current = *index;
    *index += 1;
    if cancelled.load(Ordering::Acquire) {
        return false;
    }
    publish(current, "running", "Checking…");
    let success = probe("kubectl", args, cancelled, deadline).is_some_and(|out| {
        out.success
            && if args.first() == Some(&"auth") {
                out.output.trim() == "yes"
            } else if args.first() == Some(&"version") {
                crate::util::kubectl_server_git_version(&out.output).is_some()
            } else {
                true
            }
    });
    if !cancelled.load(Ordering::Acquire) {
        publish(
            current,
            if success {
                "passed"
            } else if advisory {
                "warning"
            } else {
                "failed"
            },
            if success { pass } else { fail },
        );
    }
    success
}

pub struct ProbeResult {
    pub success: bool,
    pub output: String,
}

// Keep this allowlist aligned with code/api/src/services/gpu.rs GPU_RESOURCES.
// In particular, amd.com/gpu is a ROCm compute resource, not shared DRM.
const GPU_RESOURCES: &[&str] = &[
    "gpu.intel.com/i915",
    "gpu.intel.com/xe",
    "nvidia.com/gpu",
    "nvidia.com/gpu.shared",
    "amd.com/dri",
];

fn gpu_check(result: Option<ProbeResult>) -> (&'static str, &'static str) {
    const UNAVAILABLE: &str = "No supported GPU resources advertised by Ready, schedulable nodes. GPU acceleration is optional; configure passthrough, drivers and a supported device plugin to enable it.";
    const UNVERIFIED: &str = "Could not verify GPU resources on the selected cluster. Check node list permissions and API access; GPU acceleration is optional.";
    let Some(out) = result.filter(|out| out.success) else {
        return ("warning", UNVERIFIED);
    };
    let Ok(nodes) = serde_json::from_str::<serde_json::Value>(&out.output) else {
        return ("warning", UNVERIFIED);
    };
    let Some(items) = nodes.get("items").and_then(|items| items.as_array()) else {
        return ("warning", UNVERIFIED);
    };
    let supported = items.iter().any(|node| {
        let ready = node
            .pointer("/status/conditions")
            .and_then(|conditions| conditions.as_array())
            .is_some_and(|conditions| {
                conditions.iter().any(|condition| {
                    condition.get("type").and_then(|v| v.as_str()) == Some("Ready")
                        && condition.get("status").and_then(|v| v.as_str()) == Some("True")
                })
            });
        let schedulable = matches!(
            node.pointer("/spec/unschedulable"),
            None | Some(serde_json::Value::Bool(false))
        );
        ready
            && schedulable
            && GPU_RESOURCES.iter().any(|resource| {
                node.pointer("/status/allocatable")
                    .and_then(|resources| resources.get(*resource))
                    .and_then(|quantity| quantity.as_str())
                    .and_then(|quantity| quantity.parse::<i64>().ok())
                    .is_some_and(|slots| slots > 0)
            })
    });
    if supported {
        ("passed", "Supported GPU slots are advertised on a Ready, schedulable node. This does not verify free capacity or transcoding capability.")
    } else {
        ("warning", UNAVAILABLE)
    }
}

/// Probes own a process group so timeouts/replacements stop ordinary helpers.
/// Detached helpers outside that group cannot hold our nonblocking pipe open
/// past the deadline, even if they retain a duplicate stdout descriptor.
pub fn probe(
    program: &str,
    args: &[&str],
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Option<ProbeResult> {
    probe_with_timeout(program, args, cancelled, deadline, PROBE_TIMEOUT)
}

pub fn probe_with_timeout(
    program: &str,
    args: &[&str],
    cancelled: &AtomicBool,
    deadline: Instant,
    timeout: Duration,
) -> Option<ProbeResult> {
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return None;
    }
    let mut command = crate::util::external_command(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return Some(ProbeResult {
                success: false,
                output: String::new(),
            })
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    #[cfg(unix)]
    {
        let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut data = Vec::new();
    let mut eof = false;
    let mut status = None;
    let end = deadline.min(Instant::now() + timeout);
    loop {
        // Cancellation/timeout closes our pipe even if a plugin escaped the
        // kubectl process group and is still holding a duplicate write fd.
        if cancelled.load(Ordering::Acquire) || Instant::now() >= end {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        if !eof {
            let mut buffer = [0u8; 4096];
            for _ in 0..16 {
                match stdout.read(&mut buffer) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(size) => {
                        let remain = PROBE_LIMIT.saturating_sub(data.len());
                        data.extend_from_slice(&buffer[..size.min(remain)]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(-(child.id() as i32), libc::SIGKILL);
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exited)) => {
                    status = Some(exited);
                    // Ordinary helpers in this process group should finish;
                    // escaped helpers are handled by the same deadline above.
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        if eof {
            if let Some(exited) = status {
                return Some(ProbeResult {
                    success: exited.success(),
                    output: String::from_utf8(data).unwrap_or_default(),
                });
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Publish only curated text. In particular, never echo kubectl's stderr,
/// server version, context, StorageClass name, or authentication output.
pub fn run_web_checks(
    cancelled: Arc<AtomicBool>,
    mut publish: impl FnMut(usize, &'static str, &'static str),
) {
    let deadline = Instant::now() + CHECK_DEADLINE;
    let mut index = 0;
    // Tool discovery does not execute a context or authentication plugin.
    for (tool, pass, fail) in [
        (
            "kubectl",
            "kubectl is available",
            "Install kubectl and add it to PATH.",
        ),
        (
            "helm",
            "Helm 4 is available",
            "Install Helm 4 (target 4.3.0) and add it to PATH.",
        ),
    ] {
        let current = index;
        index += 1;
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        publish(current, "running", "Checking…");
        let found = crate::util::command_exists(tool);
        let valid = if found && tool == "helm" {
            probe(
                "helm",
                &["version", "--template", "{{.Version}}"],
                &cancelled,
                deadline,
            )
            .is_some_and(|out| out.success && crate::util::helm_version_is_4(&out.output))
        } else {
            found
        };
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        publish(
            current,
            if valid { "passed" } else { "failed" },
            if valid { pass } else { fail },
        );
    }
    let access = simple_check(
        &mut index,
        &mut publish,
        &cancelled,
        deadline,
        &["cluster-info"],
        "Kubernetes API is reachable",
        "Cannot reach the selected Kubernetes API. Check credentials, network and context.",
        false,
    );
    // An inaccessible API cannot prove any subsequent cluster requirements.
    if !access {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        for i in index..initial_checks().len() {
            let advisory = initial_checks()[i].severity == "advisory";
            publish(
                i,
                if advisory { "warning" } else { "failed" },
                "Cannot verify until the selected Kubernetes API is reachable.",
            );
        }
        return;
    }
    // Version is informational; the installer does not impose a server-version floor.
    simple_check(
        &mut index,
        &mut publish,
        &cancelled,
        deadline,
        &["version", "-o", "json"],
        "Server version is available",
        "Could not read server version; check Kubernetes API access.",
        true,
    );
    let current = index;
    index += 1;
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    publish(current, "running", "Checking…");
    let ready = probe(
        "kubectl",
        &["get", "nodes", "--no-headers"],
        &cancelled,
        deadline,
    )
    .is_some_and(|out| {
        out.success
            && out
                .output
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some("Ready"))
    });
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    publish(
        current,
        if ready { "passed" } else { "failed" },
        if ready {
            "At least one Ready node is available"
        } else {
            "No Ready nodes found, or nodes cannot be listed. Check node health and list permissions."
        },
    );
    for req in REQUIRED_API_RESOURCES {
        let current = index;
        index += 1;
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        publish(current, "running", "Checking…");
        let found = probe(
            "kubectl",
            &[
                "api-resources",
                "--api-group",
                req.api_group,
                "--no-headers",
            ],
            &cancelled,
            deadline,
        )
        .is_some_and(|out| {
            out.success
                && out
                    .output
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some(req.resource))
        });
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        publish(
            current,
            if found { "passed" } else { "failed" },
            if found {
                "Required API available"
            } else {
                "Required Kubernetes API unavailable. Check API discovery and cluster configuration."
            },
        );
    }
    for req in REQUIRED_PERMISSIONS {
        let mut args = vec!["auth", "can-i", req.verb, req.resource];
        if let Some(namespace) = req.namespace {
            args.extend(["-n", namespace]);
        }
        simple_check(&mut index, &mut publish, &cancelled, deadline, &args, "Sample permission granted in this scope", "Sample permission denied or could not be verified in this scope. Check RBAC in the deployment namespaces too.", false);
    }
    let current = index;
    index += 1;
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    publish(current, "running", "Checking…");
    let storage = probe(
        "kubectl",
        &["get", "storageclass", "-o", "json"],
        &cancelled,
        deadline,
    );
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    let state = match storage {
        Some(out) if out.success => {
            let parsed: Option<serde_json::Value> = serde_json::from_str(&out.output).ok();
            match parsed.and_then(|v| v.get("items").and_then(|i| i.as_array()).cloned()) {
                Some(items) if items.iter().any(|item| item.pointer("/metadata/annotations/storageclass.kubernetes.io~1is-default-class").and_then(|v| v.as_str()) == Some("true") || item.pointer("/metadata/annotations/storageclass.beta.kubernetes.io~1is-default-class").and_then(|v| v.as_str()) == Some("true")) => ("passed", "A default StorageClass is available"),
                Some(_) => ("warning", "No default StorageClass found. Managed storage needs an explicit StorageClass or a cluster default."),
                None => ("warning", "Could not read storage classes. Verify storage access before using managed storage."),
            }
        }
        _ => (
            "warning",
            "Could not read storage classes. Verify storage access before using managed storage.",
        ),
    };
    publish(current, state.0, state.1);
    simple_check(&mut index, &mut publish, &cancelled, deadline, &["get", "--raw", "/apis/metrics.k8s.io/v1beta1"], "Metrics API is available", "Metrics API unavailable; resource charts may be limited until metrics-server is installed.", true);
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    publish(index, "running", "Checking…");
    let gpu = probe(
        "kubectl",
        &["get", "nodes", "-o", "json"],
        &cancelled,
        deadline,
    );
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    let (state, message) = gpu_check(gpu);
    publish(index, state, message);
}

#[cfg(test)]
mod web_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_helper_holding_stdout_cannot_block_probe_after_parent_exits() {
        struct Escaped(std::path::PathBuf);
        impl Drop for Escaped {
            fn drop(&mut self) {
                if let Ok(pid) = std::fs::read_to_string(&self.0) {
                    if let Ok(pid) = pid.parse::<i32>() {
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                    }
                }
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let mut bytes = [0u8; 12];
        getrandom::fill(&mut bytes).unwrap();
        let pid_file = Escaped(std::env::temp_dir().join(format!(
            "kubarr-escaped-{}-{:x?}.pid",
            std::process::id(),
            bytes
        )));
        // The direct shell exits successfully; the detached helper inherits
        // stdout but leaves kubectl's process group. The old blocking join
        // could not return until sleep finished.
        let script = "/usr/bin/setsid /bin/sh -c 'printf %s \"$$\" > \"$1\"; exec sleep 20' helper \"$1\" & printf 'direct child exited\\n'; exit 0";
        let start = Instant::now();
        let result = probe(
            "/bin/sh",
            &["-c", script, "fixture", pid_file.0.to_str().unwrap()],
            &AtomicBool::new(false),
            start + Duration::from_millis(450),
        );
        assert!(
            result.is_none(),
            "escaped writer must not produce a successful probe"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            pid_file.0.exists(),
            "escaped helper started and held the pipe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn selected_target_is_pinned_for_synthetic_kubectl_probe() {
        use std::os::unix::fs::PermissionsExt;
        let mut bytes = [0u8; 12];
        getrandom::fill(&mut bytes).unwrap();
        let directory =
            std::env::temp_dir().join(format!("kubarr-probe-{}-{:x?}", std::process::id(), bytes));
        std::fs::create_dir(&directory).unwrap();
        let kubectl = directory.join("kubectl");
        std::fs::write(
            &kubectl,
            "#!/bin/sh\nprintf '%s\\n' \"$KUBECONFIG\" \"$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = crate::util::CommandTarget {
            kubeconfig: directory.join("synthetic.yaml"),
            context: "synthetic-context".into(),
            _snapshot: None,
        };
        let _scope = crate::util::scope_target(Some(target.clone()));
        let result = probe(
            kubectl.to_str().unwrap(),
            &["cluster-info"],
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert!(result.success);
        assert_eq!(
            result.output.lines().collect::<Vec<_>>(),
            vec![
                target.kubeconfig.to_str().unwrap(),
                "--context=synthetic-context"
            ]
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn synthetic_stdout_is_bounded_and_status_is_real() {
        let cancelled = AtomicBool::new(false);
        let result = probe(
            "/bin/sh",
            &["-c", "yes fake-secret | dd bs=1024 count=256 2>/dev/null"],
            &cancelled,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert!(result.success);
        assert_eq!(result.output.len(), PROBE_LIMIT);
        let failure = probe(
            "/bin/sh",
            &["-c", "printf 'private-token'; exit 7"],
            &cancelled,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert!(!failure.success);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_cancellation_reap_shell_and_child_processes() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        assert!(probe(
            "/bin/sh",
            &["-c", "sleep 20 & wait"],
            &cancelled,
            start + Duration::from_millis(100)
        )
        .is_none());
        assert!(start.elapsed() < Duration::from_secs(2));
        let stop = cancelled.clone();
        let handle = thread::spawn(move || {
            probe(
                "/bin/sh",
                &["-c", "sleep 20 & wait"],
                &stop,
                Instant::now() + Duration::from_secs(10),
            )
        });
        thread::sleep(Duration::from_millis(80));
        cancelled.store(true, Ordering::Release);
        assert!(handle.join().unwrap().is_none());
    }

    #[test]
    fn manifest_tracks_installer_requirements_and_advisories() {
        let mut manifest = initial_checks();
        assert_eq!(
            manifest.len(),
            8 + REQUIRED_API_RESOURCES.len() + REQUIRED_PERMISSIONS.len()
        );
        assert_eq!(manifest[0].id, "kubectl");
        assert_eq!(manifest[1].id, "helm");
        assert_eq!(manifest[2].id, "api");
        assert_eq!(manifest[3].id, "server-version");
        assert_eq!(manifest[4].id, "nodes");
        assert_eq!(manifest[manifest.len() - 3].id, "storage");
        assert_eq!(manifest[manifest.len() - 2].id, "metrics");
        assert_eq!(manifest.last().unwrap().id, "gpu");
        assert_eq!(manifest.last().unwrap().label, "GPU support");
        assert_eq!(manifest.last().unwrap().severity, "advisory");
        assert_eq!(manifest.last().unwrap().state, "pending");
        assert!(!eligible(&manifest));
        for item in &mut manifest {
            item.state = "passed";
        }
        assert!(eligible(&manifest));
        manifest.last_mut().unwrap().state = "warning";
        assert!(eligible(&manifest));
        manifest
            .iter_mut()
            .find(|item| item.id == "storage")
            .unwrap()
            .state = "warning";
        assert!(eligible(&manifest));
        manifest
            .iter_mut()
            .find(|item| item.id == "nodes")
            .unwrap()
            .state = "failed";
        assert!(!eligible(&manifest));
    }

    #[test]
    fn gpu_resources_require_positive_allocatable_on_ready_schedulable_nodes() {
        use serde_json::{json, Value};
        let node = |resource: &str, amount: Value| {
            json!({"spec": {}, "status": {"conditions": [{"type": "Ready", "status": "True"}],
                "allocatable": {resource: amount}}})
        };
        let evaluate = |nodes: Vec<Value>| {
            gpu_check(Some(ProbeResult {
                success: true,
                output: json!({"items": nodes}).to_string(),
            }))
            .0
        };
        for resource in GPU_RESOURCES {
            assert_eq!(
                evaluate(vec![node(resource, json!("1"))]),
                "passed",
                "{resource}"
            );
        }
        for resource in ["amd.com/gpu", "example.com/gpu", "gpu.intel.com/i915.extra"] {
            assert_eq!(evaluate(vec![node(resource, json!("8"))]), "warning");
        }
        for amount in [
            json!("0"),
            json!("-1"),
            json!("1.5"),
            json!("1Gi"),
            json!("invalid"),
            json!(1),
            Value::Null,
        ] {
            assert_eq!(evaluate(vec![node("nvidia.com/gpu", amount)]), "warning");
        }
        let mut not_ready = node("nvidia.com/gpu", json!("2"));
        not_ready["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(evaluate(vec![not_ready.clone()]), "warning");
        not_ready["status"]["conditions"] = json!([]);
        assert_eq!(evaluate(vec![not_ready]), "warning");
        let mut cordoned = node("amd.com/dri", json!("1"));
        cordoned["spec"]["unschedulable"] = json!(true);
        assert_eq!(evaluate(vec![cordoned.clone()]), "warning");
        cordoned["spec"]["unschedulable"] = json!("false");
        assert_eq!(evaluate(vec![cordoned.clone()]), "warning");
        cordoned["spec"]["unschedulable"] = json!(true);
        assert_eq!(
            evaluate(vec![cordoned, node("gpu.intel.com/xe", json!("3"))]),
            "passed"
        );
        let mut capacity_only = node("gpu.intel.com/i915", json!("2"));
        capacity_only["status"]
            .as_object_mut()
            .unwrap()
            .remove("allocatable");
        capacity_only["status"]["capacity"] = json!({"gpu.intel.com/i915": "2"});
        assert_eq!(evaluate(vec![capacity_only]), "warning");
        assert_eq!(evaluate(vec![]), "warning");
    }

    #[test]
    fn gpu_api_failures_and_malformed_json_only_warn_without_reflecting_output() {
        for result in [
            None,
            Some(ProbeResult {
                success: false,
                output: "private-token".into(),
            }),
            Some(ProbeResult {
                success: true,
                output: "private-token".into(),
            }),
            Some(ProbeResult {
                success: true,
                output: "{}".into(),
            }),
        ] {
            let (state, message) = gpu_check(result);
            assert_eq!(state, "warning");
            assert!(!message.contains("private-token"));
            assert!(message.contains("node list permissions"));
        }
    }
}

pub fn check_cluster_prerequisites(options: &BootstrapOptions) -> bool {
    let mut passed = true;
    step("Tools", "checking local executables");
    ok("kubectl found");
    ok("helm found");

    step("Cluster", "checking Kubernetes API access");
    let context = command_output("kubectl", &["config", "current-context"])
        .unwrap_or_else(|| "unknown".to_string());
    detail("context", &context);
    if !kubectl_cluster_access() {
        fail("kubectl cannot reach the Kubernetes API");
        return false;
    }
    ok("Kubernetes API is reachable");
    report_server_version();

    step("Nodes", "checking schedulable capacity");
    passed &= check_ready_nodes();
    step("APIs", "checking required Kubernetes resources");
    passed &= check_api_resources();
    step("Permissions", "checking baseline install permissions");
    passed &= check_permissions();
    check_optional_features(options);
    passed
}

fn report_server_version() {
    if let Some(version) = command_output("kubectl", &["version", "-o", "json"])
        .and_then(|output| kubectl_server_git_version(&output))
    {
        ok(&format!("server gitVersion {version}"));
    } else {
        warn("could not read server version");
    }
}

fn check_ready_nodes() -> bool {
    match ready_node_count() {
        Some(count) if count > 0 => {
            ok(&format!("{count} ready node(s) available"));
            true
        }
        Some(_) => {
            fail("no Ready nodes found");
            false
        }
        None => {
            fail("cannot list nodes");
            false
        }
    }
}

fn check_api_resources() -> bool {
    let mut passed = true;
    for req in REQUIRED_API_RESOURCES {
        if api_resource_available(req.api_group, req.resource) {
            ok(&format!("{} API available", req.label));
        } else {
            fail(&format!("{} API is not available", req.label));
            passed = false;
        }
    }
    passed
}

fn check_permissions() -> bool {
    let mut passed = true;
    for permission in REQUIRED_PERMISSIONS {
        if can_i(permission) {
            ok(&format!("can {} {}", permission.verb, permission.resource));
        } else {
            fail(&format!(
                "cannot {} {}",
                permission.verb, permission.resource
            ));
            passed = false;
        }
    }
    passed
}

fn check_optional_features(options: &BootstrapOptions) {
    step("Optional", "checking recommended cluster features");
    if command_success("kubectl", &["get", "storageclass"]) {
        ok("storage classes are available");
    } else {
        warn("no storage class access detected; media app storage may need manual setup");
    }
    if let StorageModeOption::ManagedNfs(managed) = &options.storage.mode {
        if managed.storage_class.is_none() && default_storage_class().is_none() {
            warn("no default StorageClass detected; pass --storage-class or managed storage PVC may stay Pending");
        }
    }
    if command_success("kubectl", &["get", "--raw", "/apis/metrics.k8s.io/v1beta1"]) {
        ok("metrics API is available");
    } else {
        warn("metrics API is not available; resource charts may be limited until metrics-server is installed");
    }
}
