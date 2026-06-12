use crate::checks_data::{REQUIRED_API_RESOURCES, REQUIRED_PERMISSIONS};
use crate::style::{detail, fail, ok, step, warn};
use crate::types::{BootstrapOptions, StorageModeOption};
use crate::util::{
    api_resource_available, can_i, command_output, command_success, default_storage_class,
    kubectl_cluster_access, ready_node_count,
};

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
    step("Permissions", "checking install permissions");
    passed &= check_permissions();
    check_optional_features(options);
    passed
}

fn report_server_version() {
    if let Some(version) = command_output("kubectl", &["version", "--output=yaml"]) {
        if let Some(line) = version
            .lines()
            .find(|line| line.trim_start().starts_with("gitVersion:"))
        {
            ok(&format!("server {}", line.trim()));
        } else {
            ok("server version detected");
        }
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
