use crate::storage_claim::apply_media_claim;
use crate::style::{detail, step};
use crate::types::{
    BootstrapOptions, BOOTSTRAP_RELEASE_NAMESPACE, DATABASE_CHART_REF, DATABASE_NAMESPACE,
    DATABASE_RELEASE,
};
use crate::util::{chart_ref, ensure_tool, run_or_print};

pub fn install_database(options: &BootstrapOptions) {
    step("Database", "installing PostgreSQL with Helm");
    if !options.install.dry_run {
        ensure_tool("helm");
    }
    detail("release", DATABASE_RELEASE);
    detail("namespace", DATABASE_NAMESPACE);
    let chart = chart_ref("postgresql", DATABASE_CHART_REF);
    detail("chart", &chart);

    let args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        DATABASE_RELEASE.to_string(),
        chart,
        "-n".to_string(),
        BOOTSTRAP_RELEASE_NAMESPACE.to_string(),
        "--set".to_string(),
        format!("namespace.name={DATABASE_NAMESPACE}"),
        "--set".to_string(),
        "namespace.create=true".to_string(),
    ];
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_or_print("helm", &refs, options.install.dry_run, false);

    step("Database Storage", "creating PostgreSQL media-data PVC");
    apply_media_claim(
        DATABASE_NAMESPACE,
        &options.storage,
        options.install.dry_run,
    );

    if options.install.wait {
        run_or_print(
            "kubectl",
            &[
                "rollout",
                "status",
                "statefulset/kubarr-db",
                "-n",
                DATABASE_NAMESPACE,
                "--timeout=300s",
            ],
            options.install.dry_run,
            false,
        );
    }
}
