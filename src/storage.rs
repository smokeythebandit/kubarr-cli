use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::charts::resolve_version;
use crate::install_events::Operation;
use crate::style::{detail, ok, step, warn};
use crate::types::{
    BootstrapOptions, ExternalNfsOptions, StorageModeOption, StorageOptions,
    BOOTSTRAP_RELEASE_NAMESPACE, MANAGED_NFS_CHART_REF,
};
use crate::util::{chart_ref, check_tool, command_output, try_run_or_print_progress};

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
    let version = resolve_version("managed-nfs", &chart)?;
    let implementation = if options.dry_run {
        "ganesha".to_string()
    } else {
        managed_nfs_implementation(&options.release)?
    };
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
        "--timeout".into(),
        crate::util::HELM_TIMEOUT.into(),
        options.release.clone(),
        chart,
        "-n".into(),
        BOOTSTRAP_RELEASE_NAMESPACE.into(),
        "--set".into(),
        format!("namespace.name={}", options.namespace),
        "--set".into(),
        "namespace.create=true".into(),
        "--set".into(),
        format!("nfs.implementation={implementation}"),
        "--set".into(),
        format!("persistence.size={}", options.size),
    ];
    if let Some(version) = version {
        args.extend(["--version".to_string(), version.to_string()]);
    }
    if let Some(storage_class) = &options.storage_class {
        args.extend([
            "--set".to_string(),
            format!("persistence.storageClassName={storage_class}"),
        ]);
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    try_run_or_print_progress(
        "helm",
        &refs,
        options.dry_run,
        false,
        Some(Operation::ManagedNfs),
    )?;
    if options.wait && !options.dry_run {
        crate::readiness::wait_for_deployment(
            &options.namespace,
            "kubarr-managed-nfs",
            Operation::ManagedNfsRollout,
        )?;
    }
    Ok(())
}

fn choose_nfs_implementation(existing: Option<&serde_json::Value>) -> Result<String, String> {
    // Changing NFS implementations invalidates client file handles. Preserve
    // existing releases, including legacy charts with no implementation value.
    let implementation = match existing.and_then(|values| values.pointer("/nfs/implementation")) {
        Some(value) => value
            .as_str()
            .ok_or("existing managed NFS implementation is unsupported")?,
        None if existing.is_some() => "kernel",
        None => "ganesha",
    };
    if !matches!(implementation, "kernel" | "ganesha") {
        return Err("existing managed NFS implementation is unsupported".into());
    }
    Ok(implementation.into())
}

fn managed_nfs_implementation(release: &str) -> Result<String, String> {
    inspect_managed_nfs(release, command_output)
}

fn inspect_managed_nfs(
    release: &str,
    mut run: impl FnMut(&str, &[&str]) -> Option<String>,
) -> Result<String, String> {
    // Helm 4 includes all release states by default; Helm 3's --all was removed.
    let releases = run(
        "helm",
        &["list", "-n", BOOTSTRAP_RELEASE_NAMESPACE, "-o", "json"],
    )
    .and_then(|output| serde_json::from_str::<Vec<serde_json::Value>>(&output).ok())
    .ok_or("could not inspect existing managed NFS release; installation was not changed")?;
    if !releases
        .iter()
        .any(|item| item["name"].as_str() == Some(release))
    {
        return choose_nfs_implementation(None);
    }
    let values = run(
        "helm",
        &[
            "get",
            "values",
            release,
            "--all",
            "-n",
            BOOTSTRAP_RELEASE_NAMESPACE,
            "-o",
            "json",
        ],
    )
    .and_then(|output| serde_json::from_str::<serde_json::Value>(&output).ok())
    .ok_or("could not inspect existing managed NFS configuration; installation was not changed")?;
    choose_nfs_implementation(Some(&values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fresh_installs_use_userspace_nfs_without_migrating_existing_servers() {
        assert_eq!(choose_nfs_implementation(None).unwrap(), "ganesha");
        assert_eq!(
            choose_nfs_implementation(Some(&json!({}))).unwrap(),
            "kernel"
        );
        assert_eq!(
            choose_nfs_implementation(Some(&json!({"nfs":{"implementation":"ganesha"}}))).unwrap(),
            "ganesha"
        );
        assert!(
            choose_nfs_implementation(Some(&json!({"nfs":{"implementation":"unknown"}}))).is_err()
        );
        assert!(choose_nfs_implementation(Some(&json!({"nfs":{"implementation":false}}))).is_err());
    }

    #[test]
    fn helm4_release_inspection_preserves_failed_releases_and_fails_closed() {
        let inspect = |releases: Option<&str>, values: Option<&str>| {
            inspect_managed_nfs("managed-nfs", |program, args| {
                assert_eq!(program, "helm");
                if args[0] == "list" {
                    assert!(!args.contains(&"--all"), "Helm 4 removed list --all");
                    releases.map(str::to_string)
                } else {
                    assert_eq!(&args[..3], &["get", "values", "managed-nfs"]);
                    values.map(str::to_string)
                }
            })
        };
        assert_eq!(inspect(Some("[]"), None).unwrap(), "ganesha");
        let failed = Some(r#"[{"name":"managed-nfs","status":"failed"}]"#);
        assert_eq!(inspect(failed, Some("{}")).unwrap(), "kernel");
        assert_eq!(
            inspect(failed, Some(r#"{"nfs":{"implementation":"ganesha"}}"#)).unwrap(),
            "ganesha"
        );
        assert!(inspect(None, None).is_err());
        assert!(inspect(Some("not json"), None).is_err());
        assert!(inspect(failed, None).is_err());
    }
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
