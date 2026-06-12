use crate::style::{detail, status_label, RED};
use crate::types::{
    InstallOptions, APP_NAMESPACE, BACKEND_IMAGE, BOOTSTRAP_RELEASE_NAMESPACE, CHART_REF,
    DATABASE_NAMESPACE, FRONTEND_IMAGE, IMAGE_TAG,
};
use crate::util::{chart_ref, ensure_tool, has_help, next_value, parse_port, run_or_print};

pub fn install(args: Vec<String>) {
    if has_help(&args) {
        println!(
            "Install or upgrade Kubarr with Helm.\n\nUSAGE:\n    kubarr install [OPTIONS]\n\nOPTIONS:\n    --namespace <name>            Namespace to install into [default: kubarr-system]\n    --release <name>              Helm release name [default: kubarr]\n    --frontend-node-port <port>   Expose frontend as NodePort on this port\n    --backend-node-port <port>    Expose backend as NodePort on this port\n    --values <path>               Extra Helm values file; repeatable\n    --wait                        Wait for resources to become ready\n    --dry-run                     Print commands without running them"
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
    if !options.dry_run {
        ensure_tool("helm");
    }
    detail("release", &options.release);
    detail("namespace", &options.namespace);
    let chart = chart_ref("kubarr", CHART_REF);
    detail("chart", &chart);

    let mut helm_args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        options.release.clone(),
        chart,
        "-n".to_string(),
        BOOTSTRAP_RELEASE_NAMESPACE.to_string(),
        "--set".to_string(),
        format!("namespace.name={}", options.namespace),
        "--set".to_string(),
        "namespace.create=true".to_string(),
        "--set".to_string(),
        format!("database.namespace={DATABASE_NAMESPACE}"),
        "--set".to_string(),
        format!("backend.image.repository={BACKEND_IMAGE}"),
        "--set".to_string(),
        format!("backend.image.tag={IMAGE_TAG}"),
        "--set".to_string(),
        "backend.image.pullPolicy=IfNotPresent".to_string(),
        "--set".to_string(),
        format!("frontend.image.repository={FRONTEND_IMAGE}"),
        "--set".to_string(),
        format!("frontend.image.tag={IMAGE_TAG}"),
        "--set".to_string(),
        "frontend.image.pullPolicy=IfNotPresent".to_string(),
    ];
    if let Some(port) = options.frontend_node_port {
        helm_args.extend([
            "--set".to_string(),
            "frontend.service.type=NodePort".to_string(),
        ]);
        helm_args.extend([
            "--set".to_string(),
            format!("frontend.service.nodePort={port}"),
        ]);
    }
    if let Some(port) = options.backend_node_port {
        helm_args.extend([
            "--set".to_string(),
            "backend.service.type=NodePort".to_string(),
        ]);
        helm_args.extend([
            "--set".to_string(),
            format!("backend.service.nodePort={port}"),
        ]);
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
