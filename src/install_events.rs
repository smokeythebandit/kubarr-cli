use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static INSTALL_LOG: OnceLock<Mutex<Option<InstallSink>>> = OnceLock::new();
static DIAGNOSTICS: OnceLock<Mutex<Option<Arc<Mutex<DiagnosticLog>>>>> = OnceLock::new();
const MAX_DIAGNOSTIC_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    K3s,
    ManagedNfs,
    FluentBit,
    FluentBitPatch,
    FluentBitRollout,
    VictoriaLogs,
    VictoriaLogsRollout,
    VictoriaMetrics,
    VictoriaMetricsRollout,
    Grafana,
    GrafanaRollout,
    PostgreSql,
    PostgreSqlRollout,
    Backend,
    Frontend,
    Gateway,
    Worker,
    StorageSecret,
    SchemaMigrations,
    StorageRecord,
    AdminRecord,
}

impl Operation {
    fn description(self) -> &'static str {
        use Operation::*;
        match self {
            K3s => "Cluster: install local k3s",
            ManagedNfs => "Storage: install/upgrade Helm release managed-nfs (release namespace default; workload namespace kubarr-storage)",
            FluentBit => "Fluent Bit: install/upgrade Helm release fluent-bit (release namespace default; workload namespace fluent-bit)",
            FluentBitPatch => "Fluent Bit: patch DaemonSet readiness (workload namespace fluent-bit)",
            FluentBitRollout => "Fluent Bit: wait for DaemonSet rollout (workload namespace fluent-bit)",
            VictoriaLogs => "VictoriaLogs: install/upgrade Helm release victorialogs (release namespace default; workload namespace victorialogs)",
            VictoriaLogsRollout => "VictoriaLogs: wait for Deployment rollout (workload namespace victorialogs)",
            VictoriaMetrics => "VictoriaMetrics: install/upgrade Helm release victoriametrics (release namespace default; workload namespace victoriametrics)",
            VictoriaMetricsRollout => "VictoriaMetrics: wait for Deployment rollout (workload namespace victoriametrics)",
            Grafana => "Grafana: install/upgrade Helm release grafana (release namespace default; workload namespace grafana)",
            GrafanaRollout => "Grafana: wait for Deployment rollout (workload namespace grafana)",
            PostgreSql => "Database: install/upgrade Helm release postgresql (release namespace default; workload namespace kubarr-database)",
            PostgreSqlRollout => "Database: wait for StatefulSet rollout (workload namespace kubarr-database)",
            Backend => "Kubarr backend: install/upgrade Helm release kubarr-backend (release namespace default; workload namespace kubarr-backend)",
            Frontend => "Kubarr frontend: install/upgrade Helm release kubarr-frontend (release namespace default; workload namespace kubarr-frontend)",
            Gateway => "Gateway: install/upgrade Helm release openresty (release namespace default; workload namespace openresty)",
            Worker => "Kubarr worker: install/upgrade Helm release kubarr-worker (release namespace default; workload namespace kubarr-worker)",
            StorageSecret => "Storage: apply configuration Secret",
            SchemaMigrations => "Database: wait for application schema migrations",
            StorageRecord => "Database: save storage configuration",
            AdminRecord => "Database: create or verify administrator",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ProgressKind {
    Start,
    Wait,
    Success(Option<i32>),
    Failure(Option<i32>),
}

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Cluster,
    Tools,
    Nodes,
    Apis,
    Permissions,
    Optional,
    Storage,
    StorageConfig,
    FluentBit,
    VictoriaMetrics,
    VictoriaLogs,
    Grafana,
    Database,
    Install,
    Done,
}

impl Stage {
    fn name(self) -> &'static str {
        use Stage::*;
        match self {
            Cluster => "Cluster",
            Tools => "Tools",
            Nodes => "Nodes",
            Apis => "APIs",
            Permissions => "Permissions",
            Optional => "Optional",
            Storage => "Storage",
            StorageConfig => "Storage Config",
            FluentBit => "Fluent Bit",
            VictoriaMetrics => "VictoriaMetrics",
            VictoriaLogs => "VictoriaLogs",
            Grafana => "Grafana",
            Database => "Database",
            Install => "Install",
            Done => "Done",
        }
    }

    fn from_heading(heading: &str) -> Option<Self> {
        use Stage::*;
        Some(match heading {
            "Cluster" => Cluster,
            "Tools" => Tools,
            "Nodes" => Nodes,
            "APIs" => Apis,
            "Permissions" => Permissions,
            "Optional" => Optional,
            "Storage" => Storage,
            "Storage Config" => StorageConfig,
            "Fluent Bit" => FluentBit,
            "VictoriaMetrics" => VictoriaMetrics,
            "VictoriaLogs" => VictoriaLogs,
            "Grafana" => Grafana,
            "Database" => Database,
            "Install" => Install,
            "Done" => Done,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub enum InstallEvent {
    Legacy(String),
    Access(crate::install_access::Access),
    Stage {
        stage: Stage,
        since_start: Duration,
    },
    Progress {
        operation: Operation,
        kind: ProgressKind,
        elapsed: Duration,
        since_start: Duration,
    },
}

struct InstallSink {
    sender: Sender<InstallEvent>,
    started: Instant,
}

impl InstallEvent {
    pub fn browser_line(&self) -> Option<String> {
        if let Self::Stage { stage, since_start } = self {
            return Some(format!(
                "[{minutes:02}:{seconds:02}] [STEP] {}",
                stage.name(),
                minutes = since_start.as_secs() / 60,
                seconds = since_start.as_secs() % 60
            ));
        }
        let Self::Progress {
            operation,
            kind,
            elapsed,
            since_start,
        } = self
        else {
            return None;
        };
        let (label, suffix) = match kind {
            ProgressKind::Start => ("RUN", String::new()),
            ProgressKind::Wait => ("WAIT", format!(" · elapsed {}s", elapsed.as_secs())),
            ProgressKind::Success(code) => (
                "OK",
                format!(
                    " · elapsed {}s{}",
                    elapsed.as_secs(),
                    code.map(|code| format!(" · exit {code}"))
                        .unwrap_or_default()
                ),
            ),
            ProgressKind::Failure(code) => (
                "FAIL",
                format!(
                    " · elapsed {}s{}",
                    elapsed.as_secs(),
                    code.map(|code| format!(" · exit {code}"))
                        .unwrap_or_default()
                ),
            ),
        };
        Some(format!(
            "[{minutes:02}:{seconds:02}] [{label}] {}{suffix}",
            operation.description(),
            minutes = since_start.as_secs() / 60,
            seconds = since_start.as_secs() % 60
        ))
    }
}

pub struct DiagnosticLog {
    file: File,
    remaining: usize,
}

pub fn create_diagnostic_log() -> Result<(PathBuf, Arc<Mutex<DiagnosticLog>>), String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|err| format!("cannot generate diagnostic filename: {err}"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = std::env::temp_dir().join(format!("kubarr-bootstrap-{suffix}.log"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(&path)
        .map_err(|err| format!("cannot open private diagnostic log: {err}"))?;
    Ok((
        path,
        Arc::new(Mutex::new(DiagnosticLog {
            file,
            remaining: MAX_DIAGNOSTIC_BYTES,
        })),
    ))
}

pub fn set_diagnostic_log(log: Option<Arc<Mutex<DiagnosticLog>>>) {
    *DIAGNOSTICS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("diagnostic mutex poisoned") = log;
}

/// This file is local-only and may contain raw secrets returned by external tools.
/// Never return its contents in a browser response or print them to stdout.
pub fn record_command_failure(program: &str, stdout: &[u8], stderr: &[u8]) {
    let Some(log) = DIAGNOSTICS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("diagnostic mutex poisoned")
        .clone()
    else {
        return;
    };
    let mut log = log.lock().expect("diagnostic file mutex poisoned");
    let title = format!("\n{program} failed; stdout:\n");
    for part in [title.as_bytes(), stdout, b"\nstderr:\n", stderr, b"\n"] {
        let size = part.len().min(log.remaining);
        if size == 0 {
            if log.remaining == 0 {
                break;
            }
            continue;
        }
        if log.file.write_all(&part[..size]).is_err() {
            break;
        }
        log.remaining -= size;
    }
}

#[cfg(test)]
pub static INSTALL_TEST_LOCK: Mutex<()> = Mutex::new(());

pub fn set_install_log(sender: Sender<InstallEvent>) {
    *slot().lock().expect("install log mutex poisoned") = Some(InstallSink {
        sender,
        started: Instant::now(),
    });
}

pub fn clear_install_log() {
    *slot().lock().expect("install log mutex poisoned") = None;
}

pub fn emit(message: impl Into<String>) -> bool {
    let Some(sender) = slot()
        .lock()
        .expect("install log mutex poisoned")
        .as_ref()
        .map(|sink| sink.sender.clone())
    else {
        return false;
    };
    sender.send(InstallEvent::Legacy(message.into())).is_ok()
}

pub fn access(value: crate::install_access::Access) {
    if let Some(sender) = slot()
        .lock()
        .expect("install log mutex poisoned")
        .as_ref()
        .map(|sink| sink.sender.clone())
    {
        let _ = sender.send(InstallEvent::Access(value));
    }
}

pub fn progress(operation: Operation, kind: ProgressKind, elapsed: Duration) -> bool {
    let Some((sender, since_start)) = slot()
        .lock()
        .expect("install log mutex poisoned")
        .as_ref()
        .map(|sink| (sink.sender.clone(), sink.started.elapsed()))
    else {
        return false;
    };
    sender
        .send(InstallEvent::Progress {
            operation,
            kind,
            elapsed,
            since_start,
        })
        .is_ok()
}

pub fn stage(heading: &str) -> bool {
    let Some(stage) = Stage::from_heading(heading) else {
        return false;
    };
    let Some((sender, since_start)) = slot()
        .lock()
        .expect("install log mutex poisoned")
        .as_ref()
        .map(|sink| (sink.sender.clone(), sink.started.elapsed()))
    else {
        return false;
    };
    sender
        .send(InstallEvent::Stage { stage, since_start })
        .is_ok()
}

pub fn is_active() -> bool {
    slot().lock().expect("install log mutex poisoned").is_some()
}

fn slot() -> &'static Mutex<Option<InstallSink>> {
    INSTALL_LOG.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_file_stays_bounded_across_repeated_failures() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let (path, log) = create_diagnostic_log().unwrap();
        set_diagnostic_log(Some(log));
        for _ in 0..40 {
            record_command_failure("helm", &[b'x'; 32 * 1024], &[b'y'; 32 * 1024]);
        }
        set_diagnostic_log(None);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_DIAGNOSTIC_BYTES as u64
        );
        std::fs::remove_file(path).unwrap();
    }
}
