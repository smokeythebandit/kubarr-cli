use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::install_events::Operation;
use crate::style::{detail, ok, step, warn};
use crate::types::{
    BootstrapOptions, ExternalNfsOptions, StorageModeOption, StorageOptions,
    BOOTSTRAP_RELEASE_NAMESPACE, MANAGED_NFS_CHART_REF,
};
use crate::util::{chart_ref, check_tool, try_run_or_print_progress};

pub fn configure_storage(options: &BootstrapOptions) -> Result<(), String> {
    match &options.storage.mode {
        StorageModeOption::ManagedNfs(managed) => configure_managed(managed)?,
        StorageModeOption::ExternalNfs(external) => configure_external(options, external)?,
    }
    Ok(())
}

pub fn write_storage_config(options: &BootstrapOptions) -> Result<(), String> {
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
    )
}

fn configure_managed(managed: &StorageOptions) -> Result<(), String> {
    step("Storage", "installing managed NFS storage");
    perform_storage_install(managed)
}

fn configure_external(
    options: &BootstrapOptions,
    external: &ExternalNfsOptions,
) -> Result<(), String> {
    step("Storage", "using external NFS storage");
    detail("server", &external.server);
    detail("export", &external.export_path);
    if !options.install.dry_run {
        validate_external_nfs(external)?;
    }
    Ok(())
}

fn perform_storage_install(options: &StorageOptions) -> Result<(), String> {
    if !options.dry_run {
        check_tool("helm")?;
    }
    detail("release", &options.release);
    detail("namespace", &options.namespace);
    let chart = chart_ref("managed-nfs", MANAGED_NFS_CHART_REF);
    detail("chart", &chart);
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
        "--server-side=false".into(),
        options.release.clone(),
        chart,
        "-n".into(),
        BOOTSTRAP_RELEASE_NAMESPACE.into(),
        "--set".into(),
        format!("namespace.name={}", options.namespace),
        "--set".into(),
        "namespace.create=true".into(),
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
        args.push("--wait=legacy".to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    try_run_or_print_progress(
        "helm",
        &refs,
        options.dry_run,
        false,
        Some(Operation::ManagedNfs),
    )
}

fn validate_external_nfs(external: &ExternalNfsOptions) -> Result<(), String> {
    if external.server.trim().is_empty() || external.export_path.trim().is_empty() {
        return Err("external NFS server and path are required".into());
    }
    let address = format!("{}:2049", external.server);
    let Ok(mut addrs) = address.to_socket_addrs() else {
        warn("could not resolve external NFS server; continuing");
        return Ok(());
    };
    let Some(addr) = addrs.next() else {
        warn("could not resolve external NFS server; continuing");
        return Ok(());
    };
    match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Ok(_) => ok("external NFS server is reachable on port 2049"),
        Err(_) => warn("external NFS server did not accept a connection on port 2049; Kubernetes nodes may still reach it"),
    }
    Ok(())
}
