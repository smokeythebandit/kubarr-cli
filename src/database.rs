use crate::charts::resolve_version;
use crate::install_events::Operation;
use crate::style::{detail, step};
use crate::topology::DatabaseStorage;
use crate::types::{
    BootstrapOptions, StorageModeOption, BOOTSTRAP_RELEASE_NAMESPACE, DATABASE_CHART_REF,
    DATABASE_CLUSTER, DATABASE_CLUSTER_CRD, DATABASE_CLUSTER_RESOURCE, DATABASE_NAMESPACE,
    DATABASE_OPERATOR_CHART_REF, DATABASE_OPERATOR_DEPLOYMENT, DATABASE_OPERATOR_NAMESPACE,
    DATABASE_OPERATOR_RELEASE, DATABASE_RELEASE, DB_SECRET_NAME, HA_NODE_COUNT_VALUE,
    LOCAL_PATH_CHART_REF, LOCAL_PATH_RELEASE,
};
use crate::util::{chart_ref, check_tool, try_run_or_print_progress};

/// Node-local volumes for PostgreSQL, when the cluster has none of its own
/// that are not NFS. Made the default class only on a cluster that had none.
pub fn install_database_storage(
    options: &BootstrapOptions,
    storage: &DatabaseStorage,
) -> Result<(), String> {
    if !storage.install_local_path {
        step(
            "Storage",
            "keeping the database on the default StorageClass",
        );
        detail("storage class", &storage.storage_class);
        return Ok(());
    }
    step("Storage", "installing node-local storage for the database");
    let mut sets = Vec::new();
    if storage.make_local_path_default {
        sets.push(("storageClass.default".to_string(), "true".to_string()));
    }
    helm_install(
        options,
        LOCAL_PATH_RELEASE,
        LOCAL_PATH_CHART_REF,
        &sets,
        Operation::LocalPath,
    )
}

/// The CloudNativePG operator. Its CRDs and admission webhook have to be
/// serving before the postgresql chart renders a `Cluster`, or Helm fails on
/// an unknown kind or a refused webhook call.
pub fn install_database_operator(options: &BootstrapOptions) -> Result<(), String> {
    step(
        "Database",
        "installing the CloudNativePG operator with Helm",
    );
    detail("namespace", DATABASE_OPERATOR_NAMESPACE);
    helm_install(
        options,
        DATABASE_OPERATOR_RELEASE,
        DATABASE_OPERATOR_CHART_REF,
        &[
            (
                "namespace.name".to_string(),
                DATABASE_OPERATOR_NAMESPACE.to_string(),
            ),
            ("namespace.create".to_string(), "true".to_string()),
        ],
        Operation::DatabaseOperator,
    )?;
    if !options.install.wait {
        return Ok(());
    }
    if !options.install.dry_run {
        crate::readiness::wait_for_deployment(
            DATABASE_OPERATOR_NAMESPACE,
            DATABASE_OPERATOR_DEPLOYMENT,
            Operation::DatabaseOperatorRollout,
        )?;
    }
    try_run_or_print_progress(
        "kubectl",
        &[
            "wait",
            "--for=condition=Established",
            &format!("crd/{DATABASE_CLUSTER_CRD}"),
            "--timeout=5m",
        ],
        options.install.dry_run,
        false,
        Some(Operation::DatabaseOperatorCrd),
    )
}

pub fn install_database(
    options: &BootstrapOptions,
    node_count: u32,
    storage: &DatabaseStorage,
) -> Result<(), String> {
    step(
        "Database",
        "installing PostgreSQL on CloudNativePG with Helm",
    );
    detail("release", DATABASE_RELEASE);
    detail("namespace", DATABASE_NAMESPACE);
    let mut sets = vec![
        ("namespace.name".to_string(), DATABASE_NAMESPACE.to_string()),
        ("namespace.create".to_string(), "true".to_string()),
        // The password itself never reaches Helm. The Cluster bootstraps its
        // owner from the Secret the CLI wrote into this namespace; only its
        // name travels as a value.
        (
            "database.existingSecret".to_string(),
            DB_SECRET_NAME.to_string(),
        ),
        (HA_NODE_COUNT_VALUE.to_string(), node_count.to_string()),
        (
            "persistence.storageClassName".to_string(),
            storage.storage_class.clone(),
        ),
    ];
    // The media claim is where the nightly dumps land.
    sets.extend(storage_set_values(options));
    helm_install(
        options,
        DATABASE_RELEASE,
        DATABASE_CHART_REF,
        &sets,
        Operation::PostgreSql,
    )?;

    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "wait",
                "--for=condition=Ready",
                &format!("{DATABASE_CLUSTER_RESOURCE}/{DATABASE_CLUSTER}"),
                "-n",
                DATABASE_NAMESPACE,
                "--timeout=10m",
            ],
            options.install.dry_run,
            false,
            Some(Operation::PostgreSqlRollout),
        )?;
    }
    Ok(())
}

fn helm_install(
    options: &BootstrapOptions,
    release: &str,
    chart_default: &str,
    sets: &[(String, String)],
    operation: Operation,
) -> Result<(), String> {
    if !options.install.dry_run {
        check_tool("helm")?;
    }
    let chart = chart_ref(release, chart_default);
    detail("chart", &chart);
    let version = resolve_version(release, &chart)?;
    let mut args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        "--server-side=false".to_string(),
        "--timeout".to_string(),
        crate::util::HELM_TIMEOUT.to_string(),
        release.to_string(),
        chart,
        "-n".to_string(),
        BOOTSTRAP_RELEASE_NAMESPACE.to_string(),
    ];
    if let Some(version) = version {
        args.extend(["--version".to_string(), version]);
    }
    for (key, value) in sets {
        args.extend(["--set".to_string(), format!("{key}={value}")]);
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    try_run_or_print_progress(
        "helm",
        &refs,
        options.install.dry_run,
        false,
        Some(operation),
    )
}

fn storage_set_values(options: &BootstrapOptions) -> Vec<(String, String)> {
    let mut values = vec![(
        "storage.media.existingClaim".to_string(),
        "media-data".to_string(),
    )];

    match &options.storage.mode {
        StorageModeOption::ManagedNfs(storage) => {
            values.push(("storage.media.nfs.size".to_string(), storage.size.clone()));
        }
        StorageModeOption::ExternalNfs(storage) => {
            values.push((
                "storage.media.nfs.server".to_string(),
                storage.server.clone(),
            ));
            values.push((
                "storage.media.nfs.path".to_string(),
                storage.export_path.clone(),
            ));
        }
    }

    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_events::{self, install_test_lock, InstallEvent};
    use crate::types::{
        BootstrapStorageOptions, ClusterMode, InstallOptions, StorageOptions, DB_SECRET_NAME,
    };
    use std::sync::mpsc;

    fn plan() -> String {
        let _guard = install_test_lock();
        std::env::set_var(crate::charts::VERSION_SOURCE_ENV, "pinned");
        let (tx, rx) = mpsc::channel();
        install_events::set_install_log(tx);
        install_database(
            &BootstrapOptions {
                cluster_mode: ClusterMode::Existing,
                install: InstallOptions::default_for_bootstrap(true),
                storage: BootstrapStorageOptions {
                    mode: StorageModeOption::ManagedNfs(StorageOptions::default_managed(true)),
                },
                grafana_enabled: false,
            },
            3,
            &crate::topology::plan_database_storage(r#"{"items":[]}"#),
        )
        .unwrap();
        install_events::clear_install_log();
        std::env::remove_var(crate::charts::VERSION_SOURCE_ENV);
        rx.into_iter()
            .filter_map(|event| match event {
                InstallEvent::Legacy(line) if line.starts_with("[PLAN] helm upgrade") => Some(line),
                _ => None,
            })
            .next()
            .expect("no planned helm command")
    }

    #[test]
    fn the_database_install_names_the_credentials_secret_and_carries_no_password() {
        let command = plan();
        assert!(
            command.contains(&format!("--set database.existingSecret={DB_SECRET_NAME}")),
            "{command}"
        );
        // The chart refuses to render without one of the two, so the absence
        // of database.password is what proves the Secret is doing the work.
        assert!(!command.contains("database.password"), "{command}");
        assert!(command.contains("--set kubarr.ha.nodeCount=3"), "{command}");
        assert!(!command.contains("POSTGRES_PASSWORD"), "{command}");
        // Nothing that looks like a credential may appear as a value at all.
        for word in command.split_whitespace() {
            assert!(
                !word.to_ascii_lowercase().contains("password") || word == "--set",
                "suspicious helm argument {word:?} in {command}"
            );
        }
    }
}
