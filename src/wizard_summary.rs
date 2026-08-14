use dialoguer::Confirm;

use crate::style::{detail, status_label, step, CYAN};
use crate::types::{BootstrapOptions, ClusterMode, StorageModeOption};
use crate::wizard_prompts::wizard_theme;

pub fn confirm_bootstrap_plan(options: &BootstrapOptions) {
    step(
        "Launch Plan",
        "review the final bootstrap plan before anything changes",
    );
    detail("cluster", cluster_label(options.cluster_mode));
    detail("namespace", &options.install.namespace);
    detail("release", &options.install.release);
    if let Some(server_name) = &options.install.server_name {
        detail("server name", server_name);
    }
    if let Some(admin) = &options.install.admin {
        detail("admin username", &admin.username);
        detail("admin email", &admin.email);
    }
    if let Some(port) = options.install.backend_node_port {
        detail("backend nodeport", &port.to_string());
    }
    storage_summary(options);
    detail(
        "grafana",
        if options.grafana_enabled {
            "enabled"
        } else {
            "skipped"
        },
    );

    let proceed = Confirm::with_theme(&wizard_theme())
        .with_prompt("Continue with bootstrap?")
        .default(true)
        .interact()
        .unwrap_or(false);
    if !proceed {
        println!("{} bootstrap cancelled", status_label("info", CYAN));
        std::process::exit(0);
    }
}

fn cluster_label(mode: ClusterMode) -> &'static str {
    match mode {
        ClusterMode::Existing => "use current Kubernetes context",
        ClusterMode::SingleNode => "install single-node k3s on this system",
    }
}

fn storage_summary(options: &BootstrapOptions) {
    match &options.storage.mode {
        StorageModeOption::ManagedNfs(storage) => {
            detail("storage", "managed NFS");
            detail("storage namespace", &storage.namespace);
            detail("storage release", &storage.release);
            detail("storage size", &storage.size);
            detail(
                "storage class",
                storage
                    .storage_class
                    .as_deref()
                    .unwrap_or("cluster default"),
            );
        }
        StorageModeOption::ExternalNfs(storage) => {
            detail("storage", "external NFS");
            detail("nfs server", &storage.server);
            detail("nfs export", &storage.export_path);
        }
    }
}
