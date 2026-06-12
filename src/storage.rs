use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::style::{detail, ok, status_label, step, warn, RED};
use crate::types::{
    BootstrapOptions, ExternalNfsOptions, StorageModeOption, StorageOptions, MANAGED_NFS_CHART_REF,
};
use crate::util::{ensure_tool, run_or_print};

pub fn configure_storage(options: &BootstrapOptions) {
    match &options.storage.mode {
        StorageModeOption::ManagedNfs(managed) => configure_managed(managed),
        StorageModeOption::ExternalNfs(external) => configure_external(options, external),
    }
}

pub fn write_storage_config(options: &BootstrapOptions) {
    let mode = match &options.storage.mode {
        StorageModeOption::ManagedNfs(_) => "managed NFS",
        StorageModeOption::ExternalNfs(_) => "external NFS",
    };
    step(
        "Storage Config",
        &format!("writing Kubarr {mode} configuration"),
    );
    crate::storage_secret::apply_storage_secret(
        &options.install.namespace,
        &options.storage,
        options.install.dry_run,
    );
}

fn configure_managed(managed: &StorageOptions) {
    step("Storage", "installing managed NFS storage");
    perform_storage_install(managed);
}

fn configure_external(options: &BootstrapOptions, external: &ExternalNfsOptions) {
    step("Storage", "using external NFS storage");
    detail("server", &external.server);
    detail("export", &external.export_path);
    if !options.install.dry_run {
        validate_external_nfs(external);
    }
}

fn perform_storage_install(options: &StorageOptions) {
    if !options.dry_run {
        ensure_tool("helm");
    }
    detail("release", &options.release);
    detail("namespace", &options.namespace);
    detail("chart", MANAGED_NFS_CHART_REF);
    detail("size", &options.size);
    detail(
        "storage class",
        options
            .storage_class
            .as_deref()
            .unwrap_or("cluster default"),
    );

    let mut args = vec![
        "upgrade".into(),
        "--install".into(),
        options.release.clone(),
        MANAGED_NFS_CHART_REF.into(),
        "-n".into(),
        options.namespace.clone(),
        "--create-namespace".into(),
        "--set".into(),
        format!("persistence.size={}", options.size),
    ];
    if let Some(storage_class) = &options.storage_class {
        args.extend([
            "--set".to_string(),
            format!("persistence.storageClassName={storage_class}"),
        ]);
    }
    if options.wait {
        args.push("--wait".to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_or_print("helm", &refs, options.dry_run, false);
}

fn validate_external_nfs(external: &ExternalNfsOptions) {
    if external.server.trim().is_empty() || external.export_path.trim().is_empty() {
        eprintln!(
            "{} external NFS server and path are required",
            status_label("error", RED)
        );
        std::process::exit(2);
    }
    let address = format!("{}:2049", external.server);
    let Ok(mut addrs) = address.to_socket_addrs() else {
        warn("could not resolve external NFS server; continuing");
        return;
    };
    let Some(addr) = addrs.next() else {
        warn("could not resolve external NFS server; continuing");
        return;
    };
    match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(_) => ok("external NFS server is reachable on port 2049"),
        Err(_) => warn("external NFS server did not accept a connection on port 2049; Kubernetes nodes may still reach it"),
    }
}
