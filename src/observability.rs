use crate::storage_claim::apply_media_claim;
use crate::style::{detail, step};
use crate::types::{
    BootstrapOptions, BOOTSTRAP_RELEASE_NAMESPACE, FLUENT_BIT_CHART_REF, FLUENT_BIT_NAMESPACE,
    FLUENT_BIT_RELEASE, VICTORIALOGS_CHART_REF, VICTORIALOGS_NAMESPACE, VICTORIALOGS_RELEASE,
    VICTORIAMETRICS_CHART_REF, VICTORIAMETRICS_NAMESPACE, VICTORIAMETRICS_RELEASE,
};
use crate::util::{chart_ref, ensure_tool, run_or_print};

pub fn install_fluent_bit(options: &BootstrapOptions) {
    step("Fluent Bit", "installing log collector with Helm");
    let allowed_namespaces = indexed_set_values(
        "fluentbit.allowedNamespaces",
        &[
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
    );
    install_chart(
        "fluent-bit",
        FLUENT_BIT_RELEASE,
        FLUENT_BIT_CHART_REF,
        FLUENT_BIT_NAMESPACE,
        &allowed_namespaces,
        false,
        options.install.dry_run,
    );
    patch_fluent_bit_readiness(options.install.dry_run);
    if options.install.wait {
        run_or_print(
            "kubectl",
            &[
                "rollout",
                "status",
                "daemonset/fluent-bit",
                "-n",
                FLUENT_BIT_NAMESPACE,
                "--timeout=300s",
            ],
            options.install.dry_run,
            false,
        );
    }
}

pub fn install_victoriametrics(options: &BootstrapOptions) {
    step("VictoriaMetrics", "installing metrics store with Helm");
    let mut set_values = indexed_set_values(
        "networkPolicy.ingressFrom",
        &["grafana", options.install.namespace.as_str()],
    );
    set_values.extend(indexed_set_values(
        "networkPolicy.scrapeNamespaces",
        &[
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
            options.install.namespace.as_str(),
        ],
    ));
    install_chart(
        "victoriametrics",
        VICTORIAMETRICS_RELEASE,
        VICTORIAMETRICS_CHART_REF,
        VICTORIAMETRICS_NAMESPACE,
        &set_values,
        false,
        options.install.dry_run,
    );

    step(
        "VictoriaMetrics Storage",
        "creating VictoriaMetrics media-data PVC",
    );
    apply_media_claim(
        VICTORIAMETRICS_NAMESPACE,
        &options.storage,
        options.install.dry_run,
    );

    if options.install.wait {
        run_or_print(
            "kubectl",
            &[
                "rollout",
                "status",
                "deployment/victoriametrics",
                "-n",
                VICTORIAMETRICS_NAMESPACE,
                "--timeout=300s",
            ],
            options.install.dry_run,
            false,
        );
    }
}

pub fn install_victorialogs(options: &BootstrapOptions) {
    step("VictoriaLogs", "installing log store with Helm");
    let set_values = indexed_set_values(
        "networkPolicy.ingressFrom",
        &["fluent-bit", "grafana", options.install.namespace.as_str()],
    );
    install_chart(
        "victorialogs",
        VICTORIALOGS_RELEASE,
        VICTORIALOGS_CHART_REF,
        VICTORIALOGS_NAMESPACE,
        &set_values,
        false,
        options.install.dry_run,
    );

    step(
        "VictoriaLogs Storage",
        "creating VictoriaLogs media-data PVC",
    );
    apply_media_claim(
        VICTORIALOGS_NAMESPACE,
        &options.storage,
        options.install.dry_run,
    );

    if options.install.wait {
        run_or_print(
            "kubectl",
            &[
                "rollout",
                "status",
                "deployment/victorialogs",
                "-n",
                VICTORIALOGS_NAMESPACE,
                "--timeout=300s",
            ],
            options.install.dry_run,
            false,
        );
    }
}

fn indexed_set_values(key: &str, values: &[&str]) -> Vec<(String, String)> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| (format!("{key}[{index}]"), (*value).to_string()))
        .collect()
}

fn patch_fluent_bit_readiness(dry_run: bool) {
    run_or_print(
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
    );
}

fn install_chart(
    chart_name: &str,
    release: &str,
    chart: &str,
    namespace: &str,
    set_values: &[(String, String)],
    wait: bool,
    dry_run: bool,
) {
    if !dry_run {
        ensure_tool("helm");
    }
    let chart = chart_ref(chart_name, chart);
    detail("release", release);
    detail("namespace", namespace);
    detail("chart", &chart);

    let mut args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        release.to_string(),
        chart,
        "-n".to_string(),
        BOOTSTRAP_RELEASE_NAMESPACE.to_string(),
        "--set".to_string(),
        format!("namespace.name={namespace}"),
        "--set".to_string(),
        "namespace.create=true".to_string(),
    ];
    for (key, value) in set_values {
        args.extend(["--set".to_string(), format!("{}={}", key, value)]);
    }
    if wait {
        args.push("--wait".to_string());
    }

    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_or_print("helm", &refs, dry_run, false);
}
