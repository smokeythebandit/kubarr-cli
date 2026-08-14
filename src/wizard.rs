use std::io::IsTerminal;

use crate::cluster::{parse_cluster_mode, prompt_cluster_mode};
use crate::types::{
    AdminUser, BootstrapOptions, BootstrapStorageOptions, ClusterMode, InstallOptions,
    StorageModeOption, StorageOptions, MANAGED_NFS_NAMESPACE, MANAGED_NFS_RELEASE,
};
use crate::util::{next_value, parse_port};
use crate::wizard_identity::{prompt_admin_user, prompt_server_name};
use crate::wizard_prompts::{
    prompt_backend_node_port, prompt_external_storage, prompt_managed_storage, prompt_storage_mode,
};

pub fn parse_bootstrap_options(args: Vec<String>) -> Result<BootstrapOptions, String> {
    let mut install = InstallOptions::default_for_bootstrap(false);
    let mut managed = default_managed_storage(false);
    let mut cluster_mode = None;
    let mut storage_mode = None;
    let mut nfs_server = None;
    let mut nfs_path = None;
    let mut admin_username = None;
    let mut admin_email = None;
    let mut admin_password = None;
    let mut skip_cluster_check = false;
    let mut grafana_enabled = false;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--cluster-mode" => cluster_mode = Some(next_value(&mut iter, &arg)?),
            "--namespace" | "-n" => install.namespace = next_value(&mut iter, &arg)?,
            "--release" => install.release = next_value(&mut iter, &arg)?,
            "--server-name" => install.server_name = Some(next_value(&mut iter, &arg)?),
            "--admin-username" => admin_username = Some(next_value(&mut iter, &arg)?),
            "--admin-email" => admin_email = Some(next_value(&mut iter, &arg)?),
            "--admin-password" => admin_password = Some(next_value(&mut iter, &arg)?),
            "--frontend-node-port" => {
                install.frontend_node_port = Some(parse_port(next_value(&mut iter, &arg)?)?)
            }
            "--backend-node-port" => {
                install.backend_node_port = Some(parse_port(next_value(&mut iter, &arg)?)?)
            }
            "--values" | "-f" => install.values_files.push(next_value(&mut iter, &arg)?),
            "--storage-mode" => storage_mode = Some(next_value(&mut iter, &arg)?),
            "--storage-size" => managed.size = next_value(&mut iter, &arg)?,
            "--storage-class" => managed.storage_class = Some(next_value(&mut iter, &arg)?),
            "--nfs-server" => nfs_server = Some(next_value(&mut iter, &arg)?),
            "--nfs-path" => nfs_path = Some(next_value(&mut iter, &arg)?),
            "--grafana" => grafana_enabled = true,
            "--skip-cluster-check" => skip_cluster_check = true,
            "--dry-run" => {
                install.dry_run = true;
                managed.dry_run = true;
            }
            other => return Err(format!("unknown bootstrap option '{other}'")),
        }
    }

    let interactive =
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && !install.dry_run;
    install.admin = build_admin(admin_username, admin_email, admin_password)?;
    let cluster_mode = resolve_cluster_mode(cluster_mode, interactive)?;
    let storage = resolve_storage(storage_mode, managed, nfs_server, nfs_path, interactive)?;
    if interactive {
        install.server_name = Some(prompt_server_name(install.server_name.as_deref())?);
        install.backend_node_port = Some(prompt_backend_node_port(install.backend_node_port)?);
        install.admin = Some(prompt_admin_user(install.admin.as_ref())?);
    }

    Ok(BootstrapOptions {
        cluster_mode,
        install,
        storage,
        skip_cluster_check,
        interactive,
        grafana_enabled,
    })
}

fn build_admin(
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
) -> Result<Option<AdminUser>, String> {
    match (username, email, password) {
        (None, None, None) => Ok(None),
        (Some(username), Some(email), Some(password)) => Ok(Some(AdminUser {
            username,
            email,
            password,
        })),
        _ => Err(
            "admin bootstrap requires --admin-username, --admin-email, and --admin-password".into(),
        ),
    }
}

fn default_managed_storage(dry_run: bool) -> StorageOptions {
    StorageOptions {
        namespace: MANAGED_NFS_NAMESPACE.into(),
        release: MANAGED_NFS_RELEASE.into(),
        size: "1Ti".into(),
        storage_class: None,
        wait: true,
        dry_run,
    }
}

fn resolve_cluster_mode(mode: Option<String>, interactive: bool) -> Result<ClusterMode, String> {
    match mode {
        Some(mode) => parse_cluster_mode(&mode),
        None if interactive => prompt_cluster_mode(),
        None => Ok(ClusterMode::Existing),
    }
}

fn resolve_storage(
    mode: Option<String>,
    managed: StorageOptions,
    nfs_server: Option<String>,
    nfs_path: Option<String>,
    interactive: bool,
) -> Result<BootstrapStorageOptions, String> {
    let mode = match mode {
        Some(mode) => mode,
        None if interactive => prompt_storage_mode()?,
        None => "managed-nfs".to_string(),
    };

    match mode.as_str() {
        "managed-nfs" => Ok(BootstrapStorageOptions {
            mode: StorageModeOption::ManagedNfs(prompt_managed_storage(managed, interactive)?),
        }),
        "external-nfs" => Ok(BootstrapStorageOptions {
            mode: StorageModeOption::ExternalNfs(prompt_external_storage(
                nfs_server,
                nfs_path,
                interactive,
            )?),
        }),
        other => Err(format!(
            "unknown storage mode '{other}'. Use 'managed-nfs' or 'external-nfs'"
        )),
    }
}
