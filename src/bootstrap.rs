use crate::bootstrap_web::run_web;
use crate::checks::check_cluster_prerequisites;
use crate::cluster::setup_cluster_if_needed;
use crate::database::install_database;
use crate::db_bootstrap::bootstrap_database;
use crate::install::perform_bootstrap_install;
use crate::install_access::print_access_hint;
use crate::observability::{
    install_fluent_bit, install_grafana, install_victorialogs, install_victoriametrics,
};
use crate::storage::{configure_storage, write_storage_config};
use crate::style::step;
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

    configure_storage(options)?;
    install_fluent_bit(options)?;
    install_victorialogs(options)?;
    install_victoriametrics(options)?;
    // After the metrics store exists, so the scrape target is there before
    // Hubble starts exporting.
    crate::cni::ensure_hubble_metrics(options)?;
    if options.grafana_enabled {
        install_grafana(options)?;
    }
    install_database(options)?;
    step("Install", "installing Kubarr with Helm");
    perform_bootstrap_install(options)?;
    write_storage_config(options)?;
    bootstrap_database(options)?;
    print_access_hint(&options.install);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_events::{self, InstallEvent, INSTALL_TEST_LOCK};
    use crate::types::{BootstrapStorageOptions, InstallOptions, StorageOptions};
    use std::sync::mpsc;

    #[test]
    fn every_remote_bootstrap_release_uses_an_explicit_chart_revision() {
        let _guard = INSTALL_TEST_LOCK.lock().unwrap();
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
        let commands: Vec<String> = rx
            .into_iter()
            .filter_map(|event| match event {
                InstallEvent::Legacy(line) if line.starts_with("[PLAN] helm upgrade") => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(commands.len(), 10);
        for (name, version) in [
            ("managed-nfs", "12.0.0+4"),
            ("fluent-bit", "5.1.5+14"),
            ("victorialogs", "1.53.0+10"),
            ("victoriametrics", "1.154.1+15"),
            ("grafana", "13.2.4"),
            ("postgresql", "16.15.0+11"),
            ("kubarr-backend", "0.3.5"),
            ("kubarr-frontend", "0.3.5"),
            ("openresty", "1.29.2+5.10"),
            ("kubarr-worker", "0.3.5"),
        ] {
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
