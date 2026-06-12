use std::process::Command;

use crate::style::{ok, status_label, step, BLUE, CYAN};
use crate::types::{AdminUser, BootstrapOptions, DATABASE_NAMESPACE};

pub fn bootstrap_database(options: &BootstrapOptions) {
    if options.install.dry_run {
        if options.install.server_name.is_some() || options.install.admin.is_some() {
            step(
                "Database",
                "would create server/admin records directly in PostgreSQL",
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
    if let Some(server_name) = &options.install.server_name {
        run_sql(DATABASE_NAMESPACE, &server_sql(server_name));
        ok("server name saved");
    }
    if let Some(admin) = &options.install.admin {
        run_sql(DATABASE_NAMESPACE, &admin_sql(admin));
        ok("admin user created or already present");
    }
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

fn server_sql(server_name: &str) -> String {
    format!(
        "INSERT INTO server_config (name, storage_path, nfs_server, created_at) SELECT '{}', '/data', NULL, NOW() WHERE NOT EXISTS (SELECT 1 FROM server_config);",
        sql_escape(server_name)
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
