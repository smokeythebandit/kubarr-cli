use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::install_events;
use crate::install_events::{Operation, ProgressKind};
use crate::style::{ok, status_label, step, BLUE, CYAN};
use crate::types::{AdminUser, BootstrapOptions, StorageModeOption, DATABASE_NAMESPACE};
use crate::util::{external_command, run_command_with_progress};

pub fn bootstrap_database(options: &BootstrapOptions) -> Result<(), String> {
    if options.install.dry_run {
        if options.install.server_name.is_some() || options.install.admin.is_some() {
            step(
                "Database",
                "would create storage/admin records directly in PostgreSQL",
            );
            println!(
                "   {} kubectl exec -n {DATABASE_NAMESPACE} statefulset/kubarr-db -- psql ...",
                status_label("plan", CYAN)
            );
        }
        return Ok(());
    }

    if options.install.server_name.is_none() && options.install.admin.is_none() {
        return Ok(());
    }

    step(
        "Database",
        "creating bootstrap records directly in PostgreSQL",
    );
    wait_for_schema(DATABASE_NAMESPACE)?;
    run_sql(
        DATABASE_NAMESPACE,
        &storage_sql(options),
        Operation::StorageRecord,
    )?;
    ok("storage configuration saved");
    if let Some(admin) = &options.install.admin {
        run_sql(
            DATABASE_NAMESPACE,
            &admin_sql(admin)?,
            Operation::AdminRecord,
        )?;
        ok("admin user created or already present");
    }
    Ok(())
}

fn wait_for_schema(namespace: &str) -> Result<(), String> {
    wait_for_schema_with(
        Duration::from_secs(120),
        Duration::from_secs(5),
        Duration::from_secs(2),
        |deadline| schema_ready(namespace, deadline),
    )
}

fn wait_for_schema_with(
    limit: Duration,
    probe_limit: Duration,
    pause: Duration,
    mut probe: impl FnMut(Instant) -> bool,
) -> Result<(), String> {
    step("Database", "waiting for application schema migrations");
    let began = Instant::now();
    let deadline = began + limit;
    install_events::progress(
        Operation::SchemaMigrations,
        ProgressKind::Start,
        Duration::ZERO,
    );
    let mut last_heartbeat = began;
    while Instant::now() < deadline {
        if probe((Instant::now() + probe_limit).min(deadline)) {
            install_events::progress(
                Operation::SchemaMigrations,
                ProgressKind::Success(None),
                began.elapsed(),
            );
            ok("database schema is ready");
            return Ok(());
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(10) {
            install_events::progress(
                Operation::SchemaMigrations,
                ProgressKind::Wait,
                began.elapsed(),
            );
            last_heartbeat = Instant::now();
        }
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            sleep(pause.min(remaining));
        }
    }
    install_events::progress(
        Operation::SchemaMigrations,
        ProgressKind::Failure(None),
        began.elapsed(),
    );
    Err("timed out waiting for database schema migrations".into())
}

fn schema_ready(namespace: &str, deadline: Instant) -> bool {
    let mut command = external_command("kubectl");
    command.args([
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
        ]);
    schema_ready_command(command, deadline)
}

fn schema_ready_command(command: std::process::Command, deadline: Instant) -> bool {
    #[cfg(unix)]
    {
        crate::util::bounded_probe_output(command, deadline, Some(Operation::SchemaMigrations))
            .is_some_and(|output| output.trim() == "t")
    }
    #[cfg(not(unix))]
    {
        let _ = (command, deadline);
        false
    }
}

fn run_sql(namespace: &str, sql: &str, operation: Operation) -> Result<(), String> {
    if install_events::emit("[RUN] kubectl exec statefulset/kubarr-db -- psql") {
        return run_sql_command(namespace, sql, operation);
    }
    println!(
        "   {} kubectl exec statefulset/kubarr-db -- psql",
        status_label("run", BLUE)
    );
    run_sql_command(namespace, sql, operation)
}

fn run_sql_command(namespace: &str, sql: &str, operation: Operation) -> Result<(), String> {
    let mut command = external_command("kubectl");
    command.args([
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
    ]);
    let status = run_command_with_progress(
        &mut command,
        "database bootstrap SQL",
        None,
        Some(operation),
    )?;
    if !status.success() {
        return Err(format!("database bootstrap SQL failed ({status})"));
    }
    Ok(())
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

fn admin_sql(admin: &AdminUser) -> Result<String, String> {
    let hash = bcrypt::hash(&admin.password, bcrypt::DEFAULT_COST)
        .map_err(|err| format!("failed to hash admin password: {err}"))?;
    Ok(format!(
        "WITH admin_role AS (SELECT id FROM roles WHERE name='admin'), new_user AS (INSERT INTO users (username,email,hashed_password,is_active,is_approved,totp_enabled,created_at,updated_at) SELECT '{}','{}','{}',TRUE,TRUE,FALSE,NOW(),NOW() WHERE NOT EXISTS (SELECT 1 FROM users WHERE username='{}' OR email='{}') RETURNING id) INSERT INTO user_roles (user_id, role_id) SELECT new_user.id, admin_role.id FROM new_user, admin_role ON CONFLICT DO NOTHING;",
        sql_escape(&admin.username),
        sql_escape(&admin.email),
        sql_escape(&hash),
        sql_escape(&admin.username),
        sql_escape(&admin.email),
    ))
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

#[cfg(all(test, unix))]
mod progress_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn fake_stuck_kubectl_query_respects_per_query_and_overall_deadlines() {
        let _lock = crate::install_events::INSTALL_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "kubarr-fake-schema-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let fake = root.join("kubectl");
        std::fs::write(&fake, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        crate::install_events::set_install_log(tx);
        let began = Instant::now();
        let result = wait_for_schema_with(
            Duration::from_millis(95),
            Duration::from_millis(35),
            Duration::from_millis(5),
            |deadline| schema_ready_command(std::process::Command::new(&fake), deadline),
        );
        crate::install_events::clear_install_log();
        std::fs::remove_dir_all(root).unwrap();
        assert!(result
            .unwrap_err()
            .contains("timed out waiting for database schema migrations"));
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "schema probe ignored deadline"
        );
        let lines: Vec<_> = rx
            .into_iter()
            .filter_map(|event| event.browser_line())
            .collect();
        assert!(lines
            .iter()
            .any(|line| line.contains("[RUN] Database: wait for application schema migrations")));
        assert!(lines
            .last()
            .unwrap()
            .contains("[FAIL] Database: wait for application schema migrations"));
        assert!(!lines.join(" ").contains("sleep 30"));
    }
}
