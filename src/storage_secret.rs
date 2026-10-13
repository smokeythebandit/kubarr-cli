use crate::install_events;
use crate::style::{paint, status_label, BLUE, CYAN, DIM};
use crate::types::{BootstrapStorageOptions, StorageModeOption, STORAGE_SECRET_NAME};
use crate::util::external_command;

pub fn apply_storage_secret(
    namespace: &str,
    storage: &BootstrapStorageOptions,
    dry_run: bool,
) -> Result<(), String> {
    let config_json = storage_secret_json(storage);
    let indented = config_json
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let yaml = format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {STORAGE_SECRET_NAME}\n  namespace: {namespace}\ntype: Opaque\nstringData:\n  config.json: |-\n{indented}\n"
    );
    run_kubectl_apply_yaml(&yaml, dry_run)
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

fn run_kubectl_apply_yaml(yaml: &str, dry_run: bool) -> Result<(), String> {
    if dry_run {
        if install_events::emit("[PLAN] kubectl apply -f -") {
            for line in yaml.lines() {
                install_events::emit(format!("     {line}"));
            }
            return Ok(());
        }
        println!("   {} kubectl apply -f -", status_label("plan", CYAN));
        for line in yaml.lines() {
            println!("   {}", paint(line, DIM));
        }
        return Ok(());
    }
    if install_events::emit("[RUN] kubectl apply -f -") {
        return apply_yaml(yaml);
    }
    println!("   {} kubectl apply -f -", status_label("run", BLUE));
    apply_yaml(yaml)
}

fn apply_yaml(yaml: &str) -> Result<(), String> {
    let mut command = external_command("kubectl");
    command.args(["apply", "-f", "-"]);
    let status = crate::util::run_command_with_progress(
        &mut command,
        "kubectl apply",
        Some(yaml.as_bytes()),
        Some(crate::install_events::Operation::StorageSecret),
    )?;
    if !status.success() {
        return Err(format!("kubectl apply failed ({status})"));
    }
    Ok(())
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
