use std::process::Command;

use dialoguer::Select;

use crate::style::{ok, status_label, step, warn, wizard_section, BLUE, CYAN};
use crate::types::{BootstrapOptions, ClusterMode};
use crate::util::{command_success, ensure_tool, kubectl_cluster_access};
use crate::wizard_prompts::wizard_theme;

pub fn parse_cluster_mode(value: &str) -> Result<ClusterMode, String> {
    match value {
        "existing" => Ok(ClusterMode::Existing),
        "single-node" => Ok(ClusterMode::SingleNode),
        other => Err(format!(
            "unknown cluster mode '{other}'. Use 'existing' or 'single-node'"
        )),
    }
}

pub fn prompt_cluster_mode() -> Result<ClusterMode, String> {
    wizard_section(
        "Cluster",
        "Choose an existing context or let Kubarr prepare this machine.",
    );
    let choices = [
        "Use an existing Kubernetes cluster/context",
        "Set up Kubernetes on this system (single-node k3s)",
    ];
    let selected = Select::with_theme(&wizard_theme())
        .with_prompt("Where should Kubarr run?")
        .items(choices)
        .default(0)
        .interact()
        .map_err(|err| err.to_string())?;

    Ok(if selected == 0 {
        ClusterMode::Existing
    } else {
        ClusterMode::SingleNode
    })
}

pub fn setup_cluster_if_needed(options: &BootstrapOptions) {
    match options.cluster_mode {
        ClusterMode::Existing => {
            if options.install.dry_run {
                step("Cluster", "using existing Kubernetes context");
            }
        }
        ClusterMode::SingleNode => setup_single_node_cluster(options.install.dry_run),
    }
}

fn setup_single_node_cluster(dry_run: bool) {
    step("Cluster", "setting up single-node Kubernetes with k3s");
    let install_command = "curl -sfL https://get.k3s.io | sh -s - --write-kubeconfig-mode 644";
    if dry_run {
        println!(
            "   {} sh -c '{}';",
            status_label("plan", CYAN),
            install_command
        );
        println!(
            "   {} export KUBECONFIG=/etc/rancher/k3s/k3s.yaml",
            status_label("plan", CYAN)
        );
        return;
    }

    if command_success("kubectl", &["cluster-info"]) {
        ok("Kubernetes API is already reachable; skipping k3s installation");
        return;
    }

    ensure_tool("curl");
    println!(
        "   {} sh -c '{}'",
        status_label("run", BLUE),
        install_command
    );
    let status = Command::new("sh")
        .args(["-c", install_command])
        .status()
        .unwrap_or_else(|err| panic!("failed to run k3s installer: {err}"));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    if !kubectl_cluster_access() {
        warn("k3s installed, but kubectl is not using it yet");
        warn("try: export KUBECONFIG=/etc/rancher/k3s/k3s.yaml");
    }
}
