use crate::checks::check_cluster_prerequisites;
use crate::cluster::setup_cluster_if_needed;
use crate::database::install_database;
use crate::db_bootstrap::bootstrap_database;
use crate::install::perform_install;
use crate::install_access::print_access_hint;
use crate::observability::{install_fluent_bit, install_victorialogs, install_victoriametrics};
use crate::storage::{configure_storage, write_storage_config};
use crate::style::{print_banner, status_label, step, RED};
use crate::util::{ensure_tool, has_help};
use crate::wizard::parse_bootstrap_options;
use crate::wizard_summary::confirm_bootstrap_plan;

pub fn bootstrap(args: Vec<String>) {
    if has_help(&args) {
        println!(
            "Set up Kubarr from scratch.\n\nUSAGE:\n    kubarr bootstrap [OPTIONS]\n\nOPTIONS:\n    --cluster-mode <mode>         Cluster mode: existing or single-node\n    --namespace <name>            Namespace to install into [default: kubarr-system]\n    --release <name>              Helm release name [default: kubarr]\n    --server-name <name>          Display name for this Kubarr server\n    --admin-username <name>       Initial admin username\n    --admin-email <email>         Initial admin email\n    --admin-password <password>   Initial admin password\n    --backend-node-port <port>    Backend NodePort [default: 30081]\n    --values <path>               Extra Helm values file; repeatable\n    --storage-mode <mode>         Storage mode: managed-nfs or external-nfs\n    --storage-size <size>         Managed NFS PVC size [default: 1Ti]\n    --storage-class <name>        StorageClass for managed NFS PVC\n    --nfs-server <host>           External NFS server hostname or IP\n    --nfs-path <path>             External NFS export path\n    --skip-cluster-check          Do not check kubectl cluster access before installing\n    --dry-run                     Print commands without running them"
        );
        return;
    }

    print_banner();
    let options = match parse_bootstrap_options(args) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("{} {err}", status_label("error", RED));
            std::process::exit(2);
        }
    };

    if options.interactive {
        confirm_bootstrap_plan(&options);
    }

    setup_cluster_if_needed(&options);
    if !options.install.dry_run {
        ensure_tool("kubectl");
        ensure_tool("helm");
        if !options.skip_cluster_check && !check_cluster_prerequisites(&options) {
            eprintln!(
                "\n{} Bootstrap stopped because required cluster checks failed.",
                status_label("error", RED)
            );
            std::process::exit(1);
        }
    } else {
        step("Plan", "dry run requested; cluster checks are skipped");
    }

    configure_storage(&options);
    install_fluent_bit(&options);
    install_victorialogs(&options);
    install_victoriametrics(&options);
    install_database(&options);
    step("Install", "installing Kubarr with Helm");
    perform_install(&options.install);
    write_storage_config(&options);
    bootstrap_database(&options);
    print_access_hint(&options.install);
}
