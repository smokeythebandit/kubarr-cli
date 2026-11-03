//! `kubarr db restore`: put a nightly dump back into the platform database.
//!
//! The postgresql chart's CronJob writes `pg_dump -Fc` archives to the shared
//! export under `system/postgresql-backups/kubarr-<UTC timestamp>.dump`. A
//! restore runs a one-off pod in the database namespace that mounts that same
//! directory through the namespace's `media-data` claim and replays the newest
//! (or a named) archive with `pg_restore --clean --if-exists`.
//!
//! The backend and the worker are scaled to zero first, so nothing writes to
//! the tables while they are dropped and recreated, and are put back to the
//! replica counts they had afterwards -- also when the restore fails, so a
//! failed restore never leaves the platform switched off.

use std::process::Stdio;
use std::thread::sleep;
use std::time::Duration;

use serde_json::json;

use crate::style::{ok, status_label, step, BLUE};
use crate::types::{
    APP_NAMESPACE, DATABASE_NAMESPACE, DB_SECRET_NAME, WORKER_NAMESPACE, WORKER_RELEASE,
};
use crate::util::{check_tool, external_command, has_help};

const RESTORE_POD: &str = "kubarr-db-restore";
/// Same major as the Cluster's PostgreSQL, so pg_restore reads its archives.
const RESTORE_IMAGE: &str = "postgres:16.15-alpine";
const BACKUP_SOURCE: &str = "system/postgresql-backups";
const MEDIA_CLAIM: &str = "media-data";
/// The Deployments that write to the database, by namespace and name. Each is
/// selected by `app.kubernetes.io/name=<name>`.
const WRITERS: &[(&str, &str)] = &[
    (APP_NAMESPACE, "kubarr-backend"),
    (WORKER_NAMESPACE, WORKER_RELEASE),
];
/// The database's NetworkPolicy admits the backup job's pods by this label;
/// the restore pod connects the same way.
const CLIENT_LABEL: (&str, &str) = ("app.kubernetes.io/component", "database-backup");

const RESTORE_SCRIPT: &str = r#"cd /backups
file="${1:-}"
if [ -z "$file" ]; then
  file=$(ls -1 kubarr-*.dump 2>/dev/null | sort | tail -n 1)
fi
if [ -z "$file" ] || [ ! -f "$file" ]; then
  echo "no backup to restore in system/postgresql-backups" >&2
  exit 1
fi
echo "restoring $file"
pg_restore --clean --if-exists -d "$DATABASE_URI" "$file"
"#;

pub fn db(args: Vec<String>) -> Result<(), String> {
    let mut iter = args.into_iter();
    match iter.next().as_deref() {
        Some("restore") => restore(iter.collect()),
        None | Some("help" | "--help" | "-h") => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown db command '{other}'. Run 'kubarr db --help'."
        )),
    }
}

fn print_help() {
    println!(
        "Manage the platform database.\n\nUSAGE:\n    kubarr db <COMMAND> [OPTIONS]\n\nCOMMANDS:\n    restore   Replace the database with a nightly backup\n\nRun 'kubarr db restore --help' for details."
    );
}

fn print_restore_help() {
    println!(
        "Replace the platform database with a nightly backup.\n\nUSAGE:\n    kubarr db restore [--file <name>]\n\nRestores the newest kubarr-*.dump from system/postgresql-backups on the shared\nstorage, or the named one. The backend and worker are stopped during the restore\nand started again afterwards. Everything written since that backup is lost.\n\nOPTIONS:\n    --file <name>   Archive file name, e.g. kubarr-20261103T031700Z.dump"
    );
}

fn restore(args: Vec<String>) -> Result<(), String> {
    if has_help(&args) {
        print_restore_help();
        return Ok(());
    }
    let mut file = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--file" => file = Some(iter.next().ok_or("missing value for --file")?),
            _ => return Err(format!("unknown restore option '{arg}'")),
        }
    }
    if let Some(name) = &file {
        validate_dump_name(name)?;
    }
    check_tool("kubectl")?;
    step("Database", "restoring the platform database from a backup");
    restore_with(file.as_deref(), kubectl, Duration::from_secs(5))?;
    ok("database restored; backend and worker restarted");
    Ok(())
}

/// A bare file name in the backups directory: nothing that walks out of it.
fn validate_dump_name(name: &str) -> Result<(), String> {
    let safe = !name.is_empty()
        && !name.starts_with('.')
        && name.ends_with(".dump")
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));
    if safe {
        Ok(())
    } else {
        Err(format!(
            "'{name}' is not a backup file name; expected something like kubarr-20261103T031700Z.dump"
        ))
    }
}

fn kubectl(args: &[String]) -> Result<String, String> {
    println!("   {} kubectl {}", status_label("run", BLUE), summary(args));
    let output = external_command("kubectl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("failed to run kubectl: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "kubectl {} failed ({}){}{}",
            args.first().map(String::as_str).unwrap_or_default(),
            output.status,
            if stdout.is_empty() {
                String::new()
            } else {
                format!("\n{stdout}")
            },
            if stderr.is_empty() {
                String::new()
            } else {
                format!("\n{stderr}")
            },
        ))
    }
}

/// The overrides document is long JSON; the progress line only needs its verb.
fn summary(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.starts_with("--overrides=") {
                "--overrides=<restore pod>"
            } else {
                arg.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// The whole restore, against any kubectl. Writers are recorded before
/// anything changes and always put back.
fn restore_with(
    file: Option<&str>,
    mut kubectl: impl FnMut(&[String]) -> Result<String, String>,
    poll: Duration,
) -> Result<(), String> {
    let mut replicas = Vec::new();
    for (namespace, name) in WRITERS {
        let count = kubectl(&strings(&[
            "get",
            "deployment",
            name,
            "-n",
            namespace,
            "-o",
            "jsonpath={.spec.replicas}",
        ]))?;
        let count = count
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("could not read the replica count of {name}"))?;
        replicas.push((*namespace, *name, count));
    }

    let result =
        stop_writers(&mut kubectl, poll).and_then(|()| run_restore_pod(file, &mut kubectl));

    let mut restart_error = None;
    for (namespace, name, count) in &replicas {
        if let Err(error) = kubectl(&scale_args(namespace, name, *count)) {
            restart_error.get_or_insert(error);
        }
    }
    result?;
    restart_error.map_or(Ok(()), Err)
}

fn scale_args(namespace: &str, name: &str, replicas: u32) -> Vec<String> {
    vec![
        "scale".into(),
        format!("deployment/{name}"),
        "-n".into(),
        namespace.into(),
        format!("--replicas={replicas}"),
    ]
}

fn stop_writers(
    kubectl: &mut impl FnMut(&[String]) -> Result<String, String>,
    poll: Duration,
) -> Result<(), String> {
    for (namespace, name) in WRITERS {
        kubectl(&scale_args(namespace, name, 0))?;
    }
    // A terminating pod can still commit; wait until none is left.
    for (namespace, name) in WRITERS {
        let selector = format!("app.kubernetes.io/name={name}");
        let mut remaining = 60;
        while !kubectl(&strings(&[
            "get", "pods", "-n", namespace, "-l", &selector, "-o", "name",
        ]))?
        .trim()
        .is_empty()
        {
            remaining -= 1;
            if remaining == 0 {
                return Err(format!(
                    "{name} pods did not stop; the database was not changed"
                ));
            }
            sleep(poll);
        }
    }
    Ok(())
}

fn run_restore_pod(
    file: Option<&str>,
    kubectl: &mut impl FnMut(&[String]) -> Result<String, String>,
) -> Result<(), String> {
    // A pod left behind by an interrupted restore would make `run` fail.
    kubectl(&strings(&[
        "delete",
        "pod",
        RESTORE_POD,
        "-n",
        DATABASE_NAMESPACE,
        "--ignore-not-found",
        "--wait=true",
    ]))?;
    let output = kubectl(&restore_run_args(file))?;
    if !output.is_empty() {
        println!("{output}");
    }
    Ok(())
}

/// `kubectl run` attached and removed afterwards, so its exit status is
/// pg_restore's and no pod outlives the command.
fn restore_run_args(file: Option<&str>) -> Vec<String> {
    vec![
        "run".into(),
        RESTORE_POD.into(),
        "-n".into(),
        DATABASE_NAMESPACE.into(),
        "--rm".into(),
        "-i".into(),
        "--restart=Never".into(),
        "--pod-running-timeout=5m".into(),
        format!("--image={RESTORE_IMAGE}"),
        format!("--overrides={}", restore_pod_overrides(file)),
    ]
}

/// The pod spec. The namespace enforces the restricted Pod Security profile,
/// and the archive is read through a read-only mount.
fn restore_pod_overrides(file: Option<&str>) -> String {
    json!({
        "apiVersion": "v1",
        "metadata": {
            "labels": {
                "app.kubernetes.io/name": RESTORE_POD,
                CLIENT_LABEL.0: CLIENT_LABEL.1,
            }
        },
        "spec": {
            "restartPolicy": "Never",
            "securityContext": {
                "runAsNonRoot": true,
                "runAsUser": 1000,
                "runAsGroup": 1000,
                "supplementalGroups": [1000],
                "seccompProfile": {"type": "RuntimeDefault"}
            },
            "containers": [{
                // Must match the pod name, or kubectl merges in a second container.
                "name": RESTORE_POD,
                "image": RESTORE_IMAGE,
                "stdin": true,
                "stdinOnce": true,
                "command": ["sh", "-ec", RESTORE_SCRIPT, "restore"],
                "args": file.map(|name| vec![name]).unwrap_or_default(),
                "env": [{
                    "name": "DATABASE_URI",
                    "valueFrom": {"secretKeyRef": {"name": DB_SECRET_NAME, "key": "uri"}}
                }],
                "volumeMounts": [{
                    "name": "backups",
                    "mountPath": "/backups",
                    "subPath": BACKUP_SOURCE,
                    "readOnly": true
                }],
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": {"drop": ["ALL"]}
                }
            }],
            "volumes": [{
                "name": "backups",
                "persistentVolumeClaim": {"claimName": MEDIA_CLAIM, "readOnly": true}
            }]
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn pod(file: Option<&str>) -> Value {
        let args = restore_run_args(file);
        let overrides = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--overrides="))
            .expect("no overrides");
        serde_json::from_str(overrides).unwrap()
    }

    #[test]
    fn the_restore_pod_replays_a_dump_from_the_backups_directory_with_pg_restore() {
        let args = restore_run_args(None);
        for expected in ["run", "kubarr-db-restore", "--rm", "-i", "--restart=Never"] {
            assert!(args.iter().any(|arg| arg == expected), "{args:?}");
        }
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-n", "kubarr-database"]));

        let pod = pod(None);
        let container = &pod["spec"]["containers"][0];
        assert_eq!(container["name"], "kubarr-db-restore");
        let script = container["command"][2].as_str().unwrap();
        assert!(script.contains(r#"pg_restore --clean --if-exists -d "$DATABASE_URI" "$file""#));
        // Newest first: the timestamps in the names sort chronologically.
        assert!(script.contains("ls -1 kubarr-*.dump 2>/dev/null | sort | tail -n 1"));
        assert_eq!(container["args"], serde_json::json!([]));
        assert_eq!(
            container["env"][0]["valueFrom"]["secretKeyRef"],
            serde_json::json!({"name": "kubarr-db-app", "key": "uri"})
        );
        let mount = &container["volumeMounts"][0];
        assert_eq!(mount["subPath"], "system/postgresql-backups");
        assert_eq!(mount["readOnly"], true);
        assert_eq!(
            pod["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"],
            "media-data"
        );
        assert_eq!(pod["spec"]["securityContext"]["runAsNonRoot"], true);
        assert_eq!(
            container["securityContext"]["allowPrivilegeEscalation"],
            false
        );
    }

    #[test]
    fn a_named_dump_is_passed_as_an_argument_never_spliced_into_the_script() {
        let pod = pod(Some("kubarr-20261103T031700Z.dump"));
        let container = &pod["spec"]["containers"][0];
        assert_eq!(
            container["args"],
            serde_json::json!(["kubarr-20261103T031700Z.dump"])
        );
        assert!(!container["command"][2]
            .as_str()
            .unwrap()
            .contains("kubarr-20261103T031700Z"));
    }

    #[test]
    fn only_bare_dump_names_are_accepted() {
        assert!(validate_dump_name("kubarr-20261103T031700Z.dump").is_ok());
        for bad in [
            "",
            "../kubarr.dump",
            "/etc/passwd",
            "kubarr.sql",
            ".hidden.dump",
            "a b.dump",
            "x;rm -rf.dump",
        ] {
            assert!(validate_dump_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// Replays canned answers and records every call.
    fn fake(
        calls: &mut Vec<String>,
        fail_run: bool,
    ) -> impl FnMut(&[String]) -> Result<String, String> + '_ {
        move |args: &[String]| {
            let line = summary(args);
            calls.push(line.clone());
            match args[0].as_str() {
                "get" if args[1] == "deployment" => Ok(if args[2] == "kubarr-backend" {
                    "3"
                } else {
                    "2"
                }
                .into()),
                "run" if fail_run => Err("pg_restore failed".into()),
                _ => Ok(String::new()),
            }
        }
    }

    #[test]
    fn writers_are_stopped_before_the_restore_and_put_back_to_their_replicas() {
        let mut calls = Vec::new();
        restore_with(None, fake(&mut calls, false), Duration::ZERO).unwrap();
        let index = |needle: &str| {
            calls
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} never ran: {calls:#?}"))
        };
        let run = index("run kubarr-db-restore");
        assert!(index("scale deployment/kubarr-backend -n kubarr-backend --replicas=0") < run);
        assert!(index("scale deployment/kubarr-worker -n kubarr-worker --replicas=0") < run);
        assert!(index("get pods -n kubarr-backend -l app.kubernetes.io/name=kubarr-backend") < run);
        assert!(index("delete pod kubarr-db-restore") < run);
        assert!(index("scale deployment/kubarr-backend -n kubarr-backend --replicas=3") > run);
        assert!(index("scale deployment/kubarr-worker -n kubarr-worker --replicas=2") > run);
    }

    #[test]
    fn a_failed_restore_still_restarts_the_platform_and_reports_the_failure() {
        let mut calls = Vec::new();
        let error = restore_with(None, fake(&mut calls, true), Duration::ZERO).unwrap_err();
        assert!(error.contains("pg_restore failed"));
        assert!(calls.iter().any(|line| line.ends_with("--replicas=3")));
        assert!(calls.iter().any(|line| line.ends_with("--replicas=2")));
    }

    #[test]
    fn nothing_is_stopped_when_the_current_replicas_cannot_be_read() {
        let mut calls = Vec::new();
        let result = restore_with(
            None,
            |args: &[String]| {
                calls.push(summary(args));
                Err("forbidden".to_string())
            },
            Duration::ZERO,
        );
        assert!(result.is_err());
        assert!(calls.iter().all(|line| !line.starts_with("scale")));
    }
}
