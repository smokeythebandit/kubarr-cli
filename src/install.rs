use std::env;

use crate::style::{detail, status_label, RED};
use crate::types::{
    BootstrapOptions, InstallOptions, StorageModeOption, APP_NAMESPACE, BACKEND_CHART_REF,
    BACKEND_IMAGE, BOOTSTRAP_RELEASE_NAMESPACE, FRONTEND_CHART_REF, FRONTEND_IMAGE,
    GATEWAY_CHART_REF, IMAGE_TAG, WORKER_CHART_REF, WORKER_IMAGE,
};
use crate::util::{chart_ref, ensure_tool, has_help, next_value, parse_port, run_or_print};

pub fn install(args: Vec<String>) {
    if has_help(&args) {
        println!(
            "Install or upgrade Kubarr component charts with Helm.\n\nUSAGE:\n    kubarr install [OPTIONS]\n\nOPTIONS:\n    --frontend-node-port <port>   Expose OpenResty as NodePort on this port\n    --backend-node-port <port>    Expose backend as NodePort on this port\n    --values <path>               Extra Helm values file; repeatable\n    --wait                        Wait for resources to become ready\n    --dry-run                     Print commands without running them"
        );
        return;
    }

    let options = match InstallOptions::parse(args) {
        Ok(options) => options,
        Err(err) => {
            eprintln!("{} {err}", status_label("error", RED));
            std::process::exit(2);
        }
    };

    perform_install(&options);
}

pub fn perform_install(options: &InstallOptions) {
    perform_install_with_storage(options, &[]);
}

pub fn perform_bootstrap_install(options: &BootstrapOptions) {
    let storage_values = storage_set_values(options);
    perform_install_with_storage(&options.install, &storage_values);
}

fn perform_install_with_storage(options: &InstallOptions, storage_values: &[(String, String)]) {
    if !options.dry_run {
        ensure_tool("helm");
    }
    detail("release", &options.release);
    detail("backend namespace", "kubarr-backend");
    detail("frontend namespace", "kubarr-frontend");
    detail("gateway namespace", "openresty");
    detail("worker namespace", "kubarr-worker");
    let backend_image = env_value("KUBARR_BACKEND_IMAGE", BACKEND_IMAGE);
    let frontend_image = env_value("KUBARR_FRONTEND_IMAGE", FRONTEND_IMAGE);
    let worker_image = env_value("KUBARR_WORKER_IMAGE", WORKER_IMAGE);
    let image_tag = env_value("KUBARR_IMAGE_TAG", IMAGE_TAG);
    let image_pull_policy = env_value("KUBARR_IMAGE_PULL_POLICY", "IfNotPresent");

    let mut backend_sets = vec![
        ("namespace.name".to_string(), "kubarr-backend".to_string()),
        (
            "database.secretName".to_string(),
            "kubarr-db-app".to_string(),
        ),
        ("image.repository".to_string(), backend_image),
        ("image.tag".to_string(), image_tag.clone()),
        ("image.pullPolicy".to_string(), image_pull_policy.clone()),
        (
            "storage.media.existingClaim".to_string(),
            "media-data".to_string(),
        ),
        ("storage.mountPath".to_string(), "/data".to_string()),
    ];
    for (key, value) in storage_values {
        backend_sets.push((key.clone(), value.clone()));
    }
    install_release(
        "kubarr-backend",
        "kubarr-backend",
        BACKEND_CHART_REF,
        options,
        &backend_sets,
    );

    let frontend_sets = vec![
        ("namespace.name".to_string(), "kubarr-frontend".to_string()),
        ("image.repository".to_string(), frontend_image),
        ("image.tag".to_string(), image_tag.clone()),
        ("image.pullPolicy".to_string(), image_pull_policy.clone()),
    ];
    install_release(
        "kubarr-frontend",
        "kubarr-frontend",
        FRONTEND_CHART_REF,
        options,
        &frontend_sets,
    );

    let mut gateway_sets = vec![
        ("namespace.name".to_string(), "openresty".to_string()),
        (
            "backend.service.namespace".to_string(),
            "kubarr-backend".to_string(),
        ),
        (
            "frontend.service.namespace".to_string(),
            "kubarr-frontend".to_string(),
        ),
    ];
    if let Some(port) = options.frontend_node_port {
        gateway_sets.push(("service.type".to_string(), "NodePort".to_string()));
        gateway_sets.push(("service.nodePort".to_string(), port.to_string()));
    }
    install_release(
        "openresty",
        "openresty",
        GATEWAY_CHART_REF,
        options,
        &gateway_sets,
    );

    let mut worker_sets = vec![
        ("namespace.name".to_string(), "kubarr-worker".to_string()),
        (
            "database.secretName".to_string(),
            "kubarr-db-app".to_string(),
        ),
        ("image.repository".to_string(), worker_image),
        ("image.tag".to_string(), image_tag),
        ("image.pullPolicy".to_string(), image_pull_policy),
        (
            "storage.media.existingClaim".to_string(),
            "media-data".to_string(),
        ),
        ("storage.mountPath".to_string(), "/data".to_string()),
    ];
    for (key, value) in storage_values {
        worker_sets.push((key.clone(), value.clone()));
    }
    install_release(
        "kubarr-worker",
        "kubarr-worker",
        WORKER_CHART_REF,
        options,
        &worker_sets,
    );
}

fn install_release(
    release: &str,
    chart_name: &str,
    chart_ref_default: &str,
    options: &InstallOptions,
    set_values: &[(String, String)],
) {
    let chart = chart_ref(chart_name, chart_ref_default);
    detail("chart", &chart);

    let mut helm_args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        release.to_string(),
        chart,
        "-n".to_string(),
        BOOTSTRAP_RELEASE_NAMESPACE.to_string(),
    ];
    if let Some(port) = options.backend_node_port {
        if release == "kubarr-backend" {
            helm_args.extend(["--set".to_string(), "service.type=NodePort".to_string()]);
            helm_args.extend(["--set".to_string(), format!("service.nodePort={port}")]);
        }
    }

    for (key, value) in set_values {
        helm_args.extend(["--set".to_string(), format!("{}={}", key, value)]);
    }
    for values in &options.values_files {
        helm_args.extend(["--values".to_string(), values.clone()]);
    }
    if options.wait {
        helm_args.push("--wait".to_string());
    }

    let helm_refs: Vec<&str> = helm_args.iter().map(String::as_str).collect();
    run_or_print("helm", &helm_refs, options.dry_run, false);
}

fn env_value(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn storage_set_values(options: &BootstrapOptions) -> Vec<(String, String)> {
    let mut values = vec![
        (
            "storage.media.existingClaim".to_string(),
            "media-data".to_string(),
        ),
        ("storage.mountPath".to_string(), "/data".to_string()),
    ];

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

impl InstallOptions {
    pub fn parse(args: Vec<String>) -> Result<Self, String> {
        let mut options = Self::default_for_bootstrap(false);
        options.frontend_node_port = None;
        options.backend_node_port = None;
        options.wait = false;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--namespace" | "-n" => options.namespace = next_value(&mut iter, &arg)?,
                "--release" => options.release = next_value(&mut iter, &arg)?,
                "--frontend-node-port" => {
                    options.frontend_node_port = Some(parse_port(next_value(&mut iter, &arg)?)?)
                }
                "--backend-node-port" => {
                    options.backend_node_port = Some(parse_port(next_value(&mut iter, &arg)?)?)
                }
                "--values" | "-f" => options.values_files.push(next_value(&mut iter, &arg)?),
                "--wait" => options.wait = true,
                "--dry-run" => options.dry_run = true,
                other => return Err(format!("unknown install option '{other}'")),
            }
        }
        Ok(options)
    }

    pub fn default_for_bootstrap(dry_run: bool) -> Self {
        Self {
            namespace: APP_NAMESPACE.into(),
            release: "kubarr".into(),
            frontend_node_port: None,
            backend_node_port: Some(30081),
            values_files: vec![],
            server_name: None,
            admin: None,
            wait: true,
            dry_run,
        }
    }
}
