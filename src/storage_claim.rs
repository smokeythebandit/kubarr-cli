use std::io::Write;
use std::process::{Command, Stdio};

use crate::style::{paint, status_label, BLUE, CYAN, DIM};
use crate::types::{BootstrapStorageOptions, StorageModeOption, MANAGED_NFS_NAMESPACE};
use crate::util::command_output;

pub fn apply_media_claim(namespace: &str, storage: &BootstrapStorageOptions, dry_run: bool) {
    let (server, path, size) = nfs_target(storage, dry_run);
    let pv_name = format!("kubarr-media-{namespace}");
    let yaml = format!(
        "apiVersion: v1\nkind: PersistentVolume\nmetadata:\n  name: {pv_name}\nspec:\n  capacity:\n    storage: {size}\n  accessModes:\n    - ReadWriteMany\n  persistentVolumeReclaimPolicy: Retain\n  nfs:\n    server: {server}\n    path: {path}\n  claimRef:\n    namespace: {namespace}\n    name: media-data\n---\napiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: media-data\n  namespace: {namespace}\nspec:\n  volumeName: {pv_name}\n  accessModes:\n    - ReadWriteMany\n  resources:\n    requests:\n      storage: {size}\n"
    );
    run_apply(&yaml, dry_run);
}

fn nfs_target(storage: &BootstrapStorageOptions, dry_run: bool) -> (String, String, String) {
    match &storage.mode {
        StorageModeOption::ManagedNfs(managed) => (
            managed_nfs_server(dry_run),
            "/".into(),
            managed.size.clone(),
        ),
        StorageModeOption::ExternalNfs(external) => (
            external.server.clone(),
            external.export_path.clone(),
            "1Ti".into(),
        ),
    }
}

fn managed_nfs_server(dry_run: bool) -> String {
    if dry_run {
        return format!("kubarr-managed-nfs.{MANAGED_NFS_NAMESPACE}.svc.cluster.local");
    }
    command_output(
        "kubectl",
        &[
            "get",
            "svc",
            "kubarr-managed-nfs",
            "-n",
            MANAGED_NFS_NAMESPACE,
            "-o",
            "jsonpath={.spec.clusterIP}",
        ],
    )
    .unwrap_or_else(|| panic!("failed to read managed NFS service ClusterIP"))
}

fn run_apply(yaml: &str, dry_run: bool) {
    if dry_run {
        println!("    {} kubectl apply -f -", status_label("plan", CYAN));
        for line in yaml.lines() {
            println!("    {}", paint(line, DIM));
        }
        return;
    }
    println!("    {} kubectl apply -f -", status_label("run", BLUE));
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run kubectl apply: {err}"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    let status = child
        .wait()
        .unwrap_or_else(|err| panic!("failed to wait for kubectl apply: {err}"));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}
