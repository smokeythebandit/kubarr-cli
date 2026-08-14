use dialoguer::{theme::ColorfulTheme, Input, Select};

use crate::style::wizard_section;
use crate::types::{ExternalNfsOptions, StorageOptions};
use crate::util::{command_output, parse_port};

pub fn wizard_theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

pub fn prompt_storage_mode() -> Result<String, String> {
    wizard_section(
        "Storage",
        "Choose where Kubarr apps will keep media, configuration, and downloads.",
    );
    let choices = [
        "Managed NFS - Kubarr deploys an NFS server backed by a PVC",
        "External NFS - Use an existing NFS server/export",
    ];
    let selected = Select::with_theme(&wizard_theme())
        .with_prompt("Choose storage")
        .items(choices)
        .default(0)
        .interact()
        .map_err(|err| err.to_string())?;
    Ok(if selected == 0 {
        "managed-nfs"
    } else {
        "external-nfs"
    }
    .to_string())
}

pub fn prompt_managed_storage(
    mut storage: StorageOptions,
    interactive: bool,
) -> Result<StorageOptions, String> {
    if interactive {
        storage.size = prompt_default("Managed NFS size", &storage.size)?;
        storage.storage_class = prompt_storage_class(storage.storage_class.as_deref())?;
    }
    Ok(storage)
}

pub fn prompt_backend_node_port(default: Option<u16>) -> Result<u16, String> {
    wizard_section(
        "Network Access",
        "Pick the backend NodePort used by the dashboard and API.",
    );
    let value = Input::with_theme(&wizard_theme())
        .with_prompt("Backend NodePort")
        .default(default.unwrap_or(30081).to_string())
        .validate_with(|value: &String| -> Result<(), String> {
            let port = value
                .parse::<u16>()
                .map_err(|_| "enter a valid TCP port".to_string())?;
            if (30000..=32767).contains(&port) {
                Ok(())
            } else {
                Err("NodePort must be between 30000 and 32767".to_string())
            }
        })
        .interact_text()
        .map_err(|err| err.to_string())?;
    parse_port(value)
}

pub fn prompt_external_storage(
    server: Option<String>,
    export_path: Option<String>,
    interactive: bool,
) -> Result<ExternalNfsOptions, String> {
    let server = match server {
        Some(value) => value,
        None if interactive => prompt_required("External NFS server")?,
        None => return Err("--nfs-server is required for external-nfs storage".into()),
    };
    let export_path = match export_path {
        Some(value) => value,
        None if interactive => prompt_required("External NFS export path")?,
        None => return Err("--nfs-path is required for external-nfs storage".into()),
    };
    Ok(ExternalNfsOptions {
        server,
        export_path,
    })
}

fn prompt_default(label: &str, default: &str) -> Result<String, String> {
    Input::with_theme(&wizard_theme())
        .with_prompt(label)
        .default(default.to_string())
        .interact_text()
        .map_err(|err| err.to_string())
}

fn prompt_storage_class(current: Option<&str>) -> Result<Option<String>, String> {
    let mut choices = vec!["Cluster default".to_string()];
    choices.extend(available_storage_classes());
    choices.push("Enter manually".to_string());

    let default = current
        .and_then(|value| choices.iter().position(|choice| choice == value))
        .unwrap_or(0);
    let selected = Select::with_theme(&wizard_theme())
        .with_prompt("StorageClass for managed NFS PVC")
        .items(&choices)
        .default(default)
        .interact()
        .map_err(|err| err.to_string())?;

    match choices[selected].as_str() {
        "Cluster default" => Ok(None),
        "Enter manually" => Ok(Some(prompt_required("StorageClass name")?)),
        value => Ok(Some(value.to_string())),
    }
}

fn available_storage_classes() -> Vec<String> {
    command_output(
        "kubectl",
        &[
            "get",
            "storageclass",
            "-o",
            "jsonpath={range .items[*]}{.metadata.name}{\"\\n\"}{end}",
        ],
    )
    .map(|output| {
        output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

fn prompt_required(label: &str) -> Result<String, String> {
    Input::with_theme(&wizard_theme())
        .with_prompt(label)
        .validate_with(|value: &String| {
            if value.trim().is_empty() {
                Err("this value is required")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .map_err(|err| err.to_string())
}
