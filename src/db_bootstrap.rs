use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use crate::style::{ok, status_label, step, BLUE, CYAN};
use crate::types::{AdminUser, BootstrapOptions, StorageModeOption, DATABASE_NAMESPACE};

pub fn bootstrap_database(options: &BootstrapOptions) {
    if options.install.dry_run {
        if options.install.server_name.is_some() || options.install.admin.is_some() {
            step(
                "Database",
                "would create storage/admin records directly in PostgreSQL",
            );
            println!(
                "    {} kubectl exec -n {DATABASE_NAMESPACE} statefulset/kubarr-db -- psql ...",
                status_label("plan", CYAN)
            );
        }
        return;
    }

    if options.install.server_name.is_none() && options.install.admin.is_none() {
        return;
    }

    step(
        "Database",
        "creating bootstrap records directly in PostgreSQL",
    );
    wait_for_schema(DATABASE_NAMESPACE);
    run_sql(DATABASE_NAMESPACE, &storage_sql(options));
    ok("storage configuration saved");
    if let Some(admin) = &options.install.admin {
        run_sql(DATABASE_NAMESPACE, &admin_sql(admin));
        ok("admin user created or already present");
    }
}

fn wait_for_schema(namespace: &str) {
    step("Database", "waiting for application schema migrations");
    for _ in 0..60 {
        if schema_ready(namespace) {
            ok("database schema is ready");
            return;
        }
        sleep(Duration::from_secs(2));
    }
    eprintln!(
        "{} timed out waiting for database schema migrations",
        status_label("error", crate::style::RED)
    );
    std::process::exit(1);
}

fn schema_ready(namespace: &str) -> bool {
    Command::new("kubectl")
        .args([
            "exec",
            "-n",
            namespace,
            "statefulset/kubarr-db",
            "--",
            "psql",
            "-U",
            "kubarr",
            "-d",
            "kubarr",
            "-tAc",
            "SELECT to_regclass('public.storage_config') IS NOT NULL AND to_regclass('public.roles') IS NOT NULL;",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|output| output.trim() == "t")
}

fn run_sql(namespace: &str, sql: &str) {
    println!(
        "    {} kubectl exec statefulset/kubarr-db -- psql",
        status_label("run", BLUE)
    );
    let status = Command::new("kubectl")
        .args([
            "exec",
            "-n",
            namespace,
            "statefulset/kubarr-db",
            "--",
            "psql",
            "-U",
            "kubarr",
            "-d",
            "kubarr",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
        .status()
        .unwrap_or_else(|err| panic!("failed to run database bootstrap SQL: {err}"));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn storage_sql(options: &BootstrapOptions) -> String {
    let (mode, config_json) = match &options.storage.mode {
        StorageModeOption::ManagedNfs(storage) => {
            managed_storage_json(storage.storage_class.as_deref(), &storage.size)
        }
        StorageModeOption::ExternalNfs(storage) => {
            external_storage_json(&storage.server, &storage.export_path)
        }
    };
    let validation_json = "{\"valid\":true,\"message\":\"Configured by kubarr bootstrap\",\"checks\":[],\"warnings\":[],\"benchmark\":null}";
    format!(
        "WITH updated AS (UPDATE storage_config SET mode='{}', mount_path='/data', uid=1000, gid=1000, fs_group=1000, config_json='{}', validation_json='{}', updated_at=NOW() RETURNING id) INSERT INTO storage_config (mode,mount_path,uid,gid,fs_group,config_json,validation_json,created_at,updated_at) SELECT '{}','/data',1000,1000,1000,'{}','{}',NOW(),NOW() WHERE NOT EXISTS (SELECT 1 FROM updated);",
        mode,
        sql_escape(&config_json),
        sql_escape(validation_json),
        mode,
        sql_escape(&config_json),
        sql_escape(validation_json),
    )
}

fn managed_storage_json(storage_class: Option<&str>, size: &str) -> (&'static str, String) {
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

fn external_storage_json(server: &str, export_path: &str) -> (&'static str, String) {
    (
        "external_nfs",
        format!(
            "{{\"server\":\"{}\",\"export_path\":\"{}\"}}",
            json_escape(server),
            json_escape(export_path)
        ),
    )
}

fn admin_sql(admin: &AdminUser) -> String {
    let hash = bcrypt::hash(&admin.password, bcrypt::DEFAULT_COST)
        .unwrap_or_else(|err| panic!("failed to hash admin password: {err}"));
    format!(
        "WITH admin_role AS (SELECT id FROM roles WHERE name='admin'), new_user AS (INSERT INTO users (username,email,hashed_password,is_active,is_approved,totp_enabled,created_at,updated_at) SELECT '{}','{}','{}',TRUE,TRUE,FALSE,NOW(),NOW() WHERE NOT EXISTS (SELECT 1 FROM users WHERE username='{}' OR email='{}') RETURNING id) INSERT INTO user_roles (user_id, role_id) SELECT new_user.id, admin_role.id FROM new_user, admin_role ON CONFLICT DO NOTHING;",
        sql_escape(&admin.username),
        sql_escape(&admin.email),
        sql_escape(&hash),
        sql_escape(&admin.username),
        sql_escape(&admin.email),
    )
}

fn sql_escape(value: &str) -> String {
    value.replace('\'', "''")
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
