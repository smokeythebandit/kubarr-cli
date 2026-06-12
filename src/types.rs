pub const CHART_REF: &str = "oci://ghcr.io/smokeythebandit/kubarr-charts/kubarr";
pub const BACKEND_IMAGE: &str = "ghcr.io/smokeythebandit/kubarr-backend";
pub const FRONTEND_IMAGE: &str = "ghcr.io/smokeythebandit/kubarr-frontend";
pub const IMAGE_TAG: &str = "latest";
pub const APP_NAMESPACE: &str = "kubarr-system";
pub const BOOTSTRAP_RELEASE_NAMESPACE: &str = "default";
pub const DATABASE_CHART_REF: &str = "oci://ghcr.io/smokeythebandit/kubarr-charts/postgresql";
pub const DATABASE_NAMESPACE: &str = "kubarr-database";
pub const DATABASE_RELEASE: &str = "postgresql";
pub const FLUENT_BIT_CHART_REF: &str = "oci://ghcr.io/smokeythebandit/kubarr-charts/fluent-bit";
pub const FLUENT_BIT_NAMESPACE: &str = "fluent-bit";
pub const FLUENT_BIT_RELEASE: &str = "fluent-bit";
pub const MANAGED_NFS_CHART_REF: &str = "oci://ghcr.io/smokeythebandit/kubarr-charts/managed-nfs";
pub const MANAGED_NFS_NAMESPACE: &str = "kubarr-storage";
pub const MANAGED_NFS_RELEASE: &str = "managed-nfs";
pub const STORAGE_SECRET_NAME: &str = "kubarr-storage-config";
pub const VICTORIAMETRICS_CHART_REF: &str =
    "oci://ghcr.io/smokeythebandit/kubarr-charts/victoriametrics";
pub const VICTORIAMETRICS_NAMESPACE: &str = "victoriametrics";
pub const VICTORIAMETRICS_RELEASE: &str = "victoriametrics";

pub struct InstallOptions {
    pub namespace: String,
    pub release: String,
    pub frontend_node_port: Option<u16>,
    pub backend_node_port: Option<u16>,
    pub values_files: Vec<String>,
    pub server_name: Option<String>,
    pub admin: Option<AdminUser>,
    pub wait: bool,
    pub dry_run: bool,
}

pub struct AdminUser {
    pub username: String,
    pub email: String,
    pub password: String,
}

pub struct BootstrapOptions {
    pub cluster_mode: ClusterMode,
    pub install: InstallOptions,
    pub storage: BootstrapStorageOptions,
    pub skip_cluster_check: bool,
    pub interactive: bool,
}

#[derive(Clone, Copy)]
pub enum ClusterMode {
    Existing,
    SingleNode,
}

pub struct StorageOptions {
    pub namespace: String,
    pub release: String,
    pub size: String,
    pub storage_class: Option<String>,
    pub wait: bool,
    pub dry_run: bool,
}

pub struct ExternalNfsOptions {
    pub server: String,
    pub export_path: String,
}

pub struct BootstrapStorageOptions {
    pub mode: StorageModeOption,
}

pub enum StorageModeOption {
    ManagedNfs(StorageOptions),
    ExternalNfs(ExternalNfsOptions),
}
