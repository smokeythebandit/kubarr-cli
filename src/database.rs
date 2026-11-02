use crate::charts::resolve_version;
use crate::install_events::Operation;
use crate::style::{detail, step};
use crate::types::{
    BootstrapOptions, StorageModeOption, BOOTSTRAP_RELEASE_NAMESPACE, DATABASE_CHART_REF,
    DATABASE_NAMESPACE, DATABASE_RELEASE, DB_SECRET_NAME,
};
use crate::util::{chart_ref, check_tool, try_run_or_print_progress};

pub fn install_database(options: &BootstrapOptions) -> Result<(), String> {
    step("Database", "installing PostgreSQL with Helm");
    if !options.install.dry_run {
        check_tool("helm")?;
    }
    detail("release", DATABASE_RELEASE);
    detail("namespace", DATABASE_NAMESPACE);
    let chart = chart_ref("postgresql", DATABASE_CHART_REF);
    detail("chart", &chart);
    let version = resolve_version("postgresql", &chart)?;

    let mut args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        "--server-side=false".to_string(),
        "--timeout".to_string(),
        crate::util::HELM_TIMEOUT.to_string(),
        DATABASE_RELEASE.to_string(),
        chart,
        "-n".to_string(),
        BOOTSTRAP_RELEASE_NAMESPACE.to_string(),
        "--set".to_string(),
        format!("namespace.name={DATABASE_NAMESPACE}"),
        "--set".to_string(),
        "namespace.create=true".to_string(),
        // The password itself never reaches Helm. The chart reads it out of
        // the Secret the CLI wrote into this namespace; only its name travels
        // as a value.
        "--set".to_string(),
        format!("database.existingSecret={DB_SECRET_NAME}"),
    ];

    if let Some(version) = version {
        args.extend(["--version".to_string(), version.to_string()]);
    }
    for (key, value) in storage_set_values(options) {
        args.extend(["--set".to_string(), format!("{}={}", key, value)]);
    }

    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    try_run_or_print_progress(
        "helm",
        &refs,
        options.install.dry_run,
        false,
        Some(Operation::PostgreSql),
    )?;

    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "rollout",
                "status",
                "statefulset/kubarr-db",
                "-n",
                DATABASE_NAMESPACE,
                crate::util::ROLLOUT_TIMEOUT,
            ],
            options.install.dry_run,
            false,
            Some(Operation::PostgreSqlRollout),
        )?;
    }
    Ok(())
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
    use crate::install_events::{self, InstallEvent, install_test_lock};
    use crate::types::{
        BootstrapStorageOptions, ClusterMode, InstallOptions, StorageOptions, DB_SECRET_NAME,
    };
    use std::sync::mpsc;

    fn plan() -> String {
        let _guard = install_test_lock();
        std::env::set_var(crate::charts::VERSION_SOURCE_ENV, "pinned");
        let (tx, rx) = mpsc::channel();
        install_events::set_install_log(tx);
        install_database(&BootstrapOptions {
            cluster_mode: ClusterMode::Existing,
            install: InstallOptions::default_for_bootstrap(true),
            storage: BootstrapStorageOptions {
                mode: StorageModeOption::ManagedNfs(StorageOptions::default_managed(true)),
            },
            grafana_enabled: false,
        })
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
