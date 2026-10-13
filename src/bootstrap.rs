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
            _ => return Err(format!("unknown bootstrap option '{arg}'. Configure installation in the web interface.")),
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
