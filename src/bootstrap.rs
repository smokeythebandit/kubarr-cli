use crate::bootstrap_web::run_web;
use crate::checks::check_cluster_prerequisites;
use crate::cluster::setup_cluster_if_needed;
use crate::database::{install_database, install_database_operator, install_database_storage};
use crate::db_bootstrap::bootstrap_database;
use crate::db_credentials::ensure_database_credentials;
use crate::install::perform_bootstrap_install;
use crate::install_access::print_access_hint;
use crate::observability::{
    install_flow_exporter, install_fluent_bit, install_grafana, install_node_exporter,
    install_victorialogs, install_victoriametrics,
};
use crate::storage::{configure_storage, write_storage_config};
use crate::style::{detail, step};
use crate::topology::{detect_database_storage, detect_node_count};
use crate::types::BootstrapOptions;
use crate::types::{ClusterMode, StorageModeOption};
use crate::util::{check_tool, has_help};

pub fn bootstrap(args: Vec<String>) -> Result<(), String> {
    if has_help(&args) {
        println!(
            "Set up Kubarr in the web interface.\n\nUSAGE:\n    kubarr bootstrap [--web] [--web-port <port>]\n\nHosts the browser wizard for all installation configuration.\n\nOPTIONS:\n    --web                Explicitly request the browser wizard\n    --web-port <port>    Listen on port 0-65535 [default: 0, selects an available port]\n\nThe wizard prints the location of a private 0600 local failure log (max 1 MiB).\nIt may contain sensitive subprocess output; remove it when finished."
        );
        return Ok(());
    }

    let mut port = 0;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--web" => {}
            "--web-port" => {
                port = iter
                    .next()
                    .ok_or("missing value for --web-port")?
                    .parse::<u16>()
                    .map_err(|_| "--web-port must be between 0 and 65535")?
            }
            _ => {
                return Err(format!(
                    "unknown bootstrap option '{arg}'. Configure installation in the web interface."
                ));
            }
        }
    }
    run_web(port)
}

pub fn run_bootstrap_install(options: &BootstrapOptions) -> Result<(), String> {
    if !options.install.dry_run {
        if crate::install_events::is_active() {
            step("Tools", "checking Helm 4 availability");
        }
        check_tool("helm")?;
    }
    setup_cluster_if_needed(options)?;
    if !options.install.dry_run {
        check_tool("kubectl")?;
        if !check_cluster_prerequisites(options) {
            return Err("bootstrap stopped because required cluster checks failed".into());
        }
    } else {
        step("Plan", "dry run requested; cluster checks are skipped");
    }

    // The web wizard cannot test storage before creating a fresh local k3s
    // cluster. Do this now, before any application components are installed.
    if !options.install.dry_run && matches!(options.cluster_mode, ClusterMode::SingleNode) {
        if let StorageModeOption::ExternalNfs(external) = &options.storage.mode {
            let input = crate::storage_probe::canonical(&external.server, &external.export_path)
                .map_err(str::to_string)?;
            step("Storage", "verifying external NFSv4.1 from local k3s");
            crate::storage_probe::verify_after_k3s(&input)?;
        }
    }

    let node_count = detect_node_count(options.install.dry_run)?;
    step("Nodes", "sizing the installation to the schedulable nodes");
    detail("schedulable nodes", &node_count.to_string());

    configure_storage(options)?;
    // PostgreSQL keeps its data on node-local disk, never on the NFS share.
    let database_storage = detect_database_storage(options.install.dry_run)?;
    install_database_storage(options, &database_storage)?;
    install_database_operator(options)?;
    install_fluent_bit(options)?;
    install_victorialogs(options)?;
    install_victoriametrics(options)?;
    // After the metrics store exists, so the scrape target is there before
    // Hubble starts exporting.
    install_node_exporter(options)?;
    crate::cni::ensure_hubble_metrics(options)?;
    // Last of the exporters: it reads byte counters that Cilium only keeps once
    // ensure_hubble_metrics has set bpf.ctAccounting.
    install_flow_exporter(options)?;
    if options.grafana_enabled {
        install_grafana(options)?;
    }
    // Before the database chart, and before the backend and worker charts
    // that read the same Secret: all three now refuse to render without a
    // password, and the Secret has to exist in each namespace before their
    // pods start, or the waited rollout never completes.
    ensure_database_credentials(options.install.dry_run)?;
    install_database(options, node_count, &database_storage)?;
    step("Install", "installing Kubarr with Helm");
    perform_bootstrap_install(options, node_count)?;
    write_storage_config(options)?;
    bootstrap_database(options, node_count)?;
    print_access_hint(&options.install);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_events::{self, install_test_lock, InstallEvent};
    use crate::types::{BootstrapStorageOptions, InstallOptions, StorageOptions};
    use std::sync::mpsc;

    #[test]
    fn every_remote_bootstrap_release_uses_an_explicit_chart_revision() {
        let _guard = install_test_lock();
        // Resolve from the pins, not the registry. A unit test that reaches
        // the network is slow, fails offline, and here would hold the shared
        // install lock across a dozen HTTPS round trips.
        std::env::set_var(crate::charts::VERSION_SOURCE_ENV, "pinned");
        let (tx, rx) = mpsc::channel();
        install_events::set_install_log(tx);
        let result = run_bootstrap_install(&BootstrapOptions {
            cluster_mode: ClusterMode::Existing,
            install: InstallOptions::default_for_bootstrap(true),
            storage: BootstrapStorageOptions {
                mode: StorageModeOption::ManagedNfs(StorageOptions::default_managed(true)),
            },
            grafana_enabled: true,
        });
        install_events::clear_install_log();
        std::env::remove_var(crate::charts::VERSION_SOURCE_ENV);
        result.unwrap();
        let plans: Vec<String> = rx
            .into_iter()
            .filter_map(|event| match event {
                InstallEvent::Legacy(line) if line.starts_with("[PLAN]") => Some(line),
                _ => None,
            })
            .collect();
        let commands: Vec<String> = plans
            .iter()
            .filter(|line| line.starts_with("[PLAN] helm upgrade"))
            .cloned()
            .collect();
        // A dry run plans for a cluster with no default StorageClass, so the
        // local-path provisioner is in the plan, set as the default.
        assert_eq!(commands.len(), 14);
        let position = |name: &str| {
            commands
                .iter()
                .position(|line| line.split_whitespace().any(|word| word == name))
                .unwrap_or_else(|| panic!("{name} is not installed: {commands:?}"))
        };
        // The operator's CRDs and webhook have to exist before the chart that
        // renders a Cluster, and the volumes' StorageClass before either.
        assert!(position("managed-nfs") < position("local-path-provisioner"));
        assert!(position("local-path-provisioner") < position("cloudnative-pg"));
        assert!(position("cloudnative-pg") < position("postgresql"));
        assert!(position("postgresql") < position("kubarr-backend"));
        assert!(commands[position("local-path-provisioner")]
            .contains("--set storageClass.default=true"));
        assert!(commands[position("postgresql")]
            .contains("--set persistence.storageClassName=local-path"));
        // Every chart that runs replicas is told how many nodes there are; a
        // dry run reads no cluster and plans for one.
        for name in [
            "openresty",
            "kubarr-backend",
            "kubarr-frontend",
            "kubarr-worker",
            "postgresql",
        ] {
            assert!(
                commands[position(name)].contains("--set kubarr.ha.nodeCount=1"),
                "{name} install carries no node count: {}",
                commands[position(name)]
            );
        }
        // Single-replica components stay out of it.
        for name in [
            "managed-nfs",
            "grafana",
            "victoriametrics",
            "cloudnative-pg",
        ] {
            assert!(!commands[position(name)].contains("kubarr.ha.nodeCount"));
        }
        // The postgresql release waits on the Cluster, not a StatefulSet.
        assert!(
            plans.iter().any(|line| line.contains(
                "kubectl wait --for=condition=Ready cluster.postgresql.cnpg.io/kubarr-db -n kubarr-database --timeout=10m"
            )),
            "{plans:?}"
        );
        assert!(plans.iter().all(|line| !line.contains("statefulset")));
        assert!(plans.iter().any(|line| line
            .contains("kubectl wait --for=condition=Established crd/clusters.postgresql.cnpg.io")));
        // Every chart that reads the platform database is told the Secret's
        // name and nothing else; the password reaches the cluster only in the
        // Secret the installer applied before any of these ran.
        for name in ["postgresql", "kubarr-backend", "kubarr-worker"] {
            let command = commands
                .iter()
                .find(|line| line.split_whitespace().any(|word| word == name))
                .unwrap();
            assert!(
                command.contains("--set database.existingSecret=kubarr-db-app"),
                "{name} install did not point at the credentials Secret: {command}"
            );
        }
        for command in &commands {
            assert!(
                !command.to_ascii_lowercase().contains("password"),
                "a Helm argument list carried a password: {command}"
            );
        }
        // The expected revision is read from the pin table rather than copied
        // here: a second copy of those versions is just somewhere else for
        // them to go stale, which is what this test exists to prevent.
        for name in [
            "managed-nfs",
            "local-path-provisioner",
            "cloudnative-pg",
            "fluent-bit",
            "victorialogs",
            "victoriametrics",
            "node-exporter",
            "kubarr-flow-exporter",
            "grafana",
            "postgresql",
            "kubarr-backend",
            "kubarr-frontend",
            "openresty",
            "kubarr-worker",
        ] {
            let version = crate::charts::pinned_version(name, "oci://pin-table")
                .unwrap_or_else(|error| panic!("{name}: {error}"))
                .unwrap_or_else(|| panic!("{name} has no pinned revision"));
            let command = commands
                .iter()
                .find(|line| line.split_whitespace().any(|word| word == name))
                .unwrap();
            assert!(
                command.contains("--timeout 15m"),
                "missing cold-start budget: {command}"
            );
            if command
                .split_whitespace()
                .any(|word| word.starts_with("oci://"))
            {
                assert!(
                    command.contains(&format!("--version {version}")),
                    "unversioned or wrong revision: {command}"
                );
            } else {
                assert!(
                    !command.contains("--version"),
                    "local override was pinned: {command}"
                );
            }
        }
    }
}
