use std::io::Write;
use std::process::{Command, Stdio};

use crate::install_events;
use crate::style::{paint, status_label, BLUE, CYAN, DIM};
use crate::types::{BootstrapStorageOptions, StorageModeOption, STORAGE_SECRET_NAME};

pub fn apply_storage_secret(namespace: &str, storage: &BootstrapStorageOptions, dry_run: bool) {
    let config_json = storage_secret_json(storage);
    let indented = config_json
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let yaml = format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {STORAGE_SECRET_NAME}\n  namespace: {namespace}\ntype: Opaque\nstringData:\n  config.json: |-\n{indented}\n"
    );
    run_kubectl_apply_yaml(&yaml, dry_run);
}

fn storage_secret_json(storage: &BootstrapStorageOptions) -> String {
    let (mode, config_json) = match &storage.mode {
        StorageModeOption::ManagedNfs(m) => managed_json(m.storage_class.as_deref(), &m.size),
        StorageModeOption::ExternalNfs(e) => external_json(&e.server, &e.export_path),
    };
    format!("{{\"mode\":\"{}\",\"mount_path\":\"/data\",\"uid\":1000,\"gid\":1000,\"fs_group\":1000,\"config_json\":{},\"validation_json\":{{\"valid\":true,\"message\":\"Configured by kubarr bootstrap\",\"checks\":[],\"warnings\":[],\"benchmark\":null}}}}", mode, config_json)
}

fn managed_json(storage_class: Option<&str>, size: &str) -> (&'static str, String) {
    let class = storage_class
        .map(|value| format!("\"{}\"", json_escape(value)))
        .unwrap_or_else(|| "null".to_string());
    (
        "managed_nfs",
        format!(
            "{{\"storage_class\":{},\"size\":\"{}\"}}",
            class,
            json_escape(size)
        ),
    )
}

fn external_json(server: &str, export_path: &str) -> (&'static str, String) {
    (
        "external_nfs",
        format!(
            "{{\"server\":\"{}\",\"export_path\":\"{}\"}}",
            json_escape(server),
            json_escape(export_path)
        ),
    )
}

fn run_kubectl_apply_yaml(yaml: &str, dry_run: bool) {
    if dry_run {
        if install_events::emit("[PLAN] kubectl apply -f -") {
            for line in yaml.lines() {
                install_events::emit(format!("     {line}"));
            }
            return;
        }
        println!("   {} kubectl apply -f -", status_label("plan", CYAN));
        for line in yaml.lines() {
            println!("   {}", paint(line, DIM));
        }
        return;
    }
    if install_events::emit("[RUN] kubectl apply -f -") {
        apply_yaml(yaml);
        return;
    }
    println!("   {} kubectl apply -f -", status_label("run", BLUE));
    apply_yaml(yaml);
}

fn apply_yaml(yaml: &str) {
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run kubectl apply: {err}"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    let status = child
        .wait()
        .unwrap_or_else(|err| panic!("failed to wait for kubectl apply: {err}"));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            _ => vec![ch],
        })
        .collect()
}
