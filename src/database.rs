use crate::storage_claim::apply_media_claim;
use crate::style::{detail, step};
use crate::types::{BootstrapOptions, DATABASE_CHART_REF, DATABASE_NAMESPACE, DATABASE_RELEASE};
use crate::util::{ensure_tool, run_or_print};

pub fn install_database(options: &BootstrapOptions) {
    step("Database Storage", "creating PostgreSQL media-data PVC");
    apply_media_claim(
        DATABASE_NAMESPACE,
        &options.storage,
        options.install.dry_run,
    );
    step("Database", "installing PostgreSQL with Helm");
    if !options.install.dry_run {
        ensure_tool("helm");
    }
    detail("release", DATABASE_RELEASE);
    detail("namespace", DATABASE_NAMESPACE);
    detail("chart", DATABASE_CHART_REF);

    let mut args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        DATABASE_RELEASE.to_string(),
        DATABASE_CHART_REF.to_string(),
        "-n".to_string(),
        DATABASE_NAMESPACE.to_string(),
        "--create-namespace".to_string(),
    ];
    if options.install.wait {
        args.push("--wait".to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_or_print("helm", &refs, options.install.dry_run, false);
}
