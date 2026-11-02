use crate::charts::resolve_version;
use crate::install_events::Operation;
use crate::style::{detail, step};
use crate::types::{
    BootstrapOptions, StorageModeOption, BOOTSTRAP_RELEASE_NAMESPACE, FLOW_EXPORTER_CHART_REF,
    FLOW_EXPORTER_NAMESPACE, FLOW_EXPORTER_RELEASE, FLUENT_BIT_CHART_REF, FLUENT_BIT_NAMESPACE,
    FLUENT_BIT_RELEASE, GRAFANA_CHART_REF, GRAFANA_NAMESPACE, GRAFANA_RELEASE,
    NODE_EXPORTER_CHART_REF, NODE_EXPORTER_NAMESPACE, NODE_EXPORTER_RELEASE,
    VICTORIALOGS_CHART_REF, VICTORIALOGS_NAMESPACE, VICTORIALOGS_RELEASE,
    VICTORIAMETRICS_CHART_REF, VICTORIAMETRICS_NAMESPACE, VICTORIAMETRICS_RELEASE,
};
use crate::util::{chart_ref, check_tool, try_run_or_print_progress};
use std::path::Path;

pub fn install_fluent_bit(options: &BootstrapOptions) -> Result<(), String> {
    step("Fluent Bit", "installing log collector with Helm");
    let mut set_values = storage_set_values(options);
    set_values.extend(indexed_set_values(
        "fluentbit.allowedNamespaces",
        &[
            // Hubble's flow metrics live on the Cilium agents.
            "kube-system",
            "sonarr",
            "radarr",
            "qbittorrent",
            "transmission",
            "deluge",
            "rutorrent",
            "jellyfin",
            "jellyseerr",
            "jackett",
            "sabnzbd",
            "kubarr",
            options.install.namespace.as_str(),
            "victoriametrics",
            "victorialogs",
            "fluent-bit",
            "grafana",
        ],
    ));
    install_chart(
        "fluent-bit",
        FLUENT_BIT_RELEASE,
        FLUENT_BIT_CHART_REF,
        FLUENT_BIT_NAMESPACE,
        &set_values,
        false,
        options.install.dry_run,
    )?;
    patch_fluent_bit_readiness(options.install.dry_run)?;
    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "rollout",
                "status",
                "daemonset/fluent-bit",
                "-n",
                FLUENT_BIT_NAMESPACE,
                crate::util::ROLLOUT_TIMEOUT,
            ],
            options.install.dry_run,
            false,
            Some(Operation::FluentBitRollout),
        )?;
    }
    Ok(())
}

/// Namespaces VictoriaMetrics is allowed to scrape.
///
/// This list replaces the chart's own: `--set key[i]=` builds a fresh list, and
/// Helm overwrites lists rather than merging them, so anything named only in
/// values.yaml is dropped by the install. The three exporter namespaces have to
/// be repeated here, or VM's default-deny egress blocks the scrape and the
/// network page is silently empty.
fn victoriametrics_scrape_namespaces(install_namespace: &str) -> Vec<&str> {
    vec![
        // Cilium's agents expose the Hubble flow metrics.
        "kube-system",
        // Kernel NFS client counters: storage traffic exists nowhere else.
        "node-exporter",
        // Per-class byte counters read from Cilium's conntrack map.
        "kubarr-flow-exporter",
        "sonarr",
        "radarr",
        "qbittorrent",
        "transmission",
        "deluge",
        "rutorrent",
        "jackett",
        "jellyfin",
        "jellyseerr",
        "sabnzbd",
        "victorialogs",
        "fluent-bit",
        install_namespace,
    ]
}

pub fn install_victoriametrics(options: &BootstrapOptions) -> Result<(), String> {
    step("VictoriaMetrics", "installing metrics store with Helm");
    let mut set_values = indexed_set_values(
        "networkPolicy.ingressFrom",
        &["grafana", options.install.namespace.as_str()],
    );
    set_values.extend(indexed_set_values(
        "networkPolicy.scrapeNamespaces",
        &victoriametrics_scrape_namespaces(options.install.namespace.as_str()),
    ));
    set_values.extend(storage_set_values(options));
    install_chart(
        "victoriametrics",
        VICTORIAMETRICS_RELEASE,
        VICTORIAMETRICS_CHART_REF,
        VICTORIAMETRICS_NAMESPACE,
        &set_values,
        false,
        options.install.dry_run,
    )?;
    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "rollout",
                "status",
                "deployment/victoriametrics",
                "-n",
                VICTORIAMETRICS_NAMESPACE,
                crate::util::ROLLOUT_TIMEOUT,
            ],
            options.install.dry_run,
            false,
            Some(Operation::VictoriaMetricsRollout),
        )?;
    }
    Ok(())
}

pub fn install_victorialogs(options: &BootstrapOptions) -> Result<(), String> {
    step("VictoriaLogs", "installing log store with Helm");
    let mut set_values = indexed_set_values(
        "networkPolicy.ingressFrom",
        &["fluent-bit", "grafana", options.install.namespace.as_str()],
    );
    set_values.extend(storage_set_values(options));
    install_chart(
        "victorialogs",
        VICTORIALOGS_RELEASE,
        VICTORIALOGS_CHART_REF,
        VICTORIALOGS_NAMESPACE,
        &set_values,
        false,
        options.install.dry_run,
    )?;
    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "rollout",
                "status",
                "deployment/victorialogs",
                "-n",
                VICTORIALOGS_NAMESPACE,
                crate::util::ROLLOUT_TIMEOUT,
            ],
            options.install.dry_run,
            false,
            Some(Operation::VictoriaLogsRollout),
        )?;
    }
    Ok(())
}

/// Host metrics, and the only place NFS traffic is counted.
///
/// NFS is mounted by the node's kernel rather than by the pods that read it, so
/// the bytes never cross a Cilium endpoint: Hubble and the conntrack map both
/// measured zero while the share was moving hundreds of megabytes. The kernel's
/// own client counters, which this exports, are where that traffic lives.
pub fn install_node_exporter(options: &BootstrapOptions) -> Result<(), String> {
    step("Node Exporter", "installing host metrics agent with Helm");
    install_chart(
        "node-exporter",
        NODE_EXPORTER_RELEASE,
        NODE_EXPORTER_CHART_REF,
        NODE_EXPORTER_NAMESPACE,
        &[],
        false,
        options.install.dry_run,
    )?;
    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "rollout",
                "status",
                "daemonset/node-exporter",
                "-n",
                NODE_EXPORTER_NAMESPACE,
                crate::util::ROLLOUT_TIMEOUT,
            ],
            options.install.dry_run,
            false,
            Some(Operation::NodeExporterRollout),
        )?;
    }
    Ok(())
}

/// Per-class byte counters, read from Cilium's conntrack map.
///
/// Hubble counts connection events and carries no byte field, so the
/// internet/LAN/cluster split needs a second source. Install this after
/// `cni::ensure_hubble_metrics`, which sets `bpf.ctAccounting`: without that
/// flag the map's byte counters stay at zero and this exports nothing.
pub fn install_flow_exporter(options: &BootstrapOptions) -> Result<(), String> {
    step("Flow Exporter", "installing per-class byte counters with Helm");
    install_chart(
        "kubarr-flow-exporter",
        FLOW_EXPORTER_RELEASE,
        FLOW_EXPORTER_CHART_REF,
        FLOW_EXPORTER_NAMESPACE,
        &[],
        false,
        options.install.dry_run,
    )?;
    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "rollout",
                "status",
                "daemonset/kubarr-flow-exporter",
                "-n",
                FLOW_EXPORTER_NAMESPACE,
                crate::util::ROLLOUT_TIMEOUT,
            ],
            options.install.dry_run,
            false,
            Some(Operation::FlowExporterRollout),
        )?;
    }
    Ok(())
}

pub fn install_grafana(options: &BootstrapOptions) -> Result<(), String> {
    step("Grafana", "installing optional dashboards with Helm");
    install_chart(
        "grafana",
        GRAFANA_RELEASE,
        GRAFANA_CHART_REF,
        GRAFANA_NAMESPACE,
        &[],
        true,
        options.install.dry_run,
    )?;
    if options.install.wait {
        try_run_or_print_progress(
            "kubectl",
            &[
                "rollout",
                "status",
                "deployment/grafana",
                "-n",
                GRAFANA_NAMESPACE,
                crate::util::ROLLOUT_TIMEOUT,
            ],
            options.install.dry_run,
            false,
            Some(Operation::GrafanaRollout),
        )?;
    }
    Ok(())
}

fn indexed_set_values(key: &str, values: &[&str]) -> Vec<(String, String)> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| (format!("{key}[{index}]"), (*value).to_string()))
        .collect()
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

fn patch_fluent_bit_readiness(dry_run: bool) -> Result<(), String> {
    try_run_or_print_progress(
        "kubectl",
        &[
            "patch",
            "daemonset/fluent-bit",
            "-n",
            FLUENT_BIT_NAMESPACE,
            "--type=json",
            "-p",
            r#"[{"op":"replace","path":"/spec/template/spec/containers/0/readinessProbe/httpGet/path","value":"/"}]"#,
        ],
        dry_run,
        false,
        Some(Operation::FluentBitPatch),
    )
}

fn install_chart(
    chart_name: &str,
    release: &str,
    chart: &str,
    namespace: &str,
    set_values: &[(String, String)],
    wait: bool,
    dry_run: bool,
) -> Result<(), String> {
    if !dry_run {
        check_tool("helm")?;
    }
    // Logging charts can be validated locally before their OCI revisions are
    // published, without redirecting unrelated bootstrap charts.
    let chart = if matches!(chart_name, "fluent-bit" | "victorialogs") {
        if let Some(root) = std::env::var_os("KUBARR_LOGGING_CHARTS_DIR") {
            let path = Path::new(&root).join(chart_name);
            if !path.join("Chart.yaml").is_file() {
                return Err(format!("logging chart missing at {}", path.display()));
            }
            path.display().to_string()
        } else {
            chart_ref(chart_name, chart)
        }
    } else {
        chart_ref(chart_name, chart)
    };
    detail("release", release);
    detail("namespace", namespace);
    detail("chart", &chart);
    let version = resolve_version(chart_name, &chart)?;

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
        "--set".to_string(),
        format!("namespace.name={namespace}"),
        "--set".to_string(),
        "namespace.create=true".to_string(),
    ];
    if let Some(version) = version {
        args.extend(["--version".to_string(), version.to_string()]);
    }
    for (key, value) in set_values {
        args.extend(["--set".to_string(), format!("{}={}", key, value)]);
    }
    if wait {
        args.push("--wait=legacy".to_string());
    }

    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let operation =
        operation_for_chart(chart_name).ok_or_else(|| "unknown observability chart".to_string())?;
    try_run_or_print_progress("helm", &refs, dry_run, false, Some(operation))
}

fn operation_for_chart(chart_name: &str) -> Option<Operation> {
    Some(match chart_name {
        "fluent-bit" => Operation::FluentBit,
        "victorialogs" => Operation::VictoriaLogs,
        "victoriametrics" => Operation::VictoriaMetrics,
        "node-exporter" => Operation::NodeExporter,
        "kubarr-flow-exporter" => Operation::FlowExporter,
        "grafana" => Operation::Grafana,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn victoriametrics_scrapes_the_exporters_it_reads() {
        // The scrape list the CLI passes replaces the chart's, so a namespace
        // named only in values.yaml never reaches the policy. Both exporters
        // and the Cilium agents were dropped exactly that way, leaving the
        // network page empty with every chart installed and healthy.
        let namespaces = victoriametrics_scrape_namespaces("kubarr");
        for required in ["kube-system", "node-exporter", "kubarr-flow-exporter"] {
            assert!(
                namespaces.contains(&required),
                "{required} is scraped by VictoriaMetrics but missing from the egress allow-list"
            );
        }
        assert!(namespaces.contains(&"kubarr"), "install namespace dropped");
    }

    #[test]
    fn every_observability_chart_has_an_operation() {
        // install_chart errors out on an unknown name, which would fail the
        // install at the last line of a Helm invocation that already ran.
        for chart in [
            "fluent-bit",
            "victorialogs",
            "victoriametrics",
            "node-exporter",
            "kubarr-flow-exporter",
            "grafana",
        ] {
            assert!(
                operation_for_chart(chart).is_some(),
                "{chart} is installed here but has no install operation"
            );
        }
    }
}
