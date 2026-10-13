use std::collections::HashSet;
use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::bootstrap::run_bootstrap_install;
use crate::checks::{self, Check};
use crate::checks_data::{REQUIRED_API_RESOURCES, REQUIRED_PERMISSIONS};
use crate::install_access::{
    self, valid_hostname, Access, AccessKind, GATEWAY_FORWARD, LB_ERROR, LB_PENDING,
};
use crate::install_events;
use crate::loadbalancer_probe;
use crate::storage_probe::{self, Input};
use crate::targets::{Choice, Registry, MAX_CONFIG};
use crate::types::{
    AdminUser, BootstrapOptions, BootstrapStorageOptions, ClusterMode, ExternalNfsOptions,
    InstallOptions, StorageModeOption, StorageOptions, WebAccess,
};
use crate::util::{
    command_exists, command_output, current_target, parse_port, scope_target, CommandTarget,
};

const MAX_EVENTS: usize = 500;
const MAX_EVENT_BYTES: usize = 1024;
const MAX_INSTALL_LOG_BYTES: usize = 2 * 1024 * 1024;
const LOG_HEADER: &str = "Kubarr installation activity (curated)\n";
const LOG_TRUNCATED: &str = "[Earlier installation activity truncated]\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Environment {
    Ssh,
    Desktop,
    Headless,
}

fn environment(ssh: bool, display: bool) -> Environment {
    if ssh {
        Environment::Ssh
    } else if display {
        Environment::Desktop
    } else {
        Environment::Headless
    }
}

fn detect_environment() -> Environment {
    environment(
        env::var_os("SSH_CONNECTION").is_some() || env::var_os("SSH_TTY").is_some(),
        env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("DISPLAY").is_some(),
    )
}

fn token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| format!("could not generate session token: {err}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn candidate_hosts(mode: Environment) -> Vec<String> {
    if mode != Environment::Ssh {
        return vec!["127.0.0.1".into()];
    }
    let mut hosts = vec![];
    if let Ok(connection) = env::var("SSH_CONNECTION") {
        if let Some(host) = connection.split_whitespace().nth(2) {
            if host.parse::<Ipv4Addr>().is_ok() {
                hosts.push(host.to_string());
            }
        }
    }
    if let Some(output) = command_output("hostname", &["-I"]) {
        for host in output.split_whitespace() {
            if host.parse::<Ipv4Addr>().is_ok() && !hosts.iter().any(|value| value == host) {
                hosts.push(host.to_string());
            }
        }
    }
    hosts.push("127.0.0.1".into());
    hosts
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[derive(Clone)]
struct App {
    shared: Arc<Mutex<Shared>>,
    hosts: Arc<HashSet<String>>,
    installer: fn(&BootstrapOptions) -> Result<(), String>,
    diagnostics: Option<Arc<Mutex<install_events::DiagnosticLog>>>,
    completion: Arc<Notify>,
    targets: Arc<Mutex<Registry>>,
    checks: Arc<Mutex<CheckState>>,
    check_runner: fn(Arc<AtomicBool>, &mut dyn FnMut(usize, &'static str, &'static str)),
    storage: Arc<Mutex<StorageState>>,
    storage_runner:
        fn(&Input, &AtomicBool, &mut dyn FnMut(usize, &'static str, String)) -> Result<(), String>,
    loadbalancer: Arc<Mutex<LoadBalancerState>>,
    loadbalancer_runner:
        fn(&AtomicBool, &mut dyn FnMut(usize, &'static str, String)) -> Result<String, String>,
    access_reader: fn() -> Access,
}

fn real_check_runner(
    cancelled: Arc<AtomicBool>,
    publish: &mut dyn FnMut(usize, &'static str, &'static str),
) {
    checks::run_web_checks(cancelled, publish);
}

const CHECK_VALIDITY: Duration = Duration::from_secs(300);

#[derive(Default)]
struct LoadBalancerState {
    session: Option<LoadBalancerSession>,
    worker_running: bool,
}

struct LoadBalancerSession {
    id: String,
    target_id: String,
    phase: &'static str,
    checks: Vec<loadbalancer_probe::Stage>,
    completed: Option<Instant>,
    chart_version: Option<String>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct LoadBalancerReply {
    check_id: String,
    target_id: String,
    phase: &'static str,
    eligible: bool,
    checks: Vec<loadbalancer_probe::Stage>,
}

impl LoadBalancerSession {
    fn eligible(&self, target_id: &str) -> bool {
        self.target_id == target_id
            && self.phase == "complete"
            && self
                .completed
                .is_some_and(|at| at.elapsed() <= loadbalancer_probe::VALIDITY)
            && loadbalancer_probe::eligible(&self.checks)
    }

    fn reply(&self) -> LoadBalancerReply {
        LoadBalancerReply {
            check_id: self.id.clone(),
            target_id: self.target_id.clone(),
            phase: self.phase,
            eligible: self.eligible(&self.target_id),
            checks: self.checks.clone(),
        }
    }
}

async fn begin_loadbalancer(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CheckRequest>,
) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    if app.shared.lock().unwrap().phase != Phase::Ready {
        return StatusCode::CONFLICT.into_response();
    }
    if app.targets.lock().unwrap().get(&body.target_id).is_none() {
        return (StatusCode::BAD_REQUEST, "Unknown destination").into_response();
    }
    let id = match token() {
        Ok(id) => id,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let spawn = {
        let mut state = app.loadbalancer.lock().unwrap();
        if app.shared.lock().unwrap().phase != Phase::Ready {
            return StatusCode::CONFLICT.into_response();
        }
        if let Some(previous) = state.session.as_mut() {
            previous.cancelled.store(true, Ordering::Release);
            previous.phase = "cancelled";
        }
        state.session = Some(LoadBalancerSession {
            id: id.clone(),
            target_id: body.target_id,
            phase: "running",
            checks: loadbalancer_probe::stages(),
            completed: None,
            chart_version: None,
            cancelled,
        });
        let spawn = !state.worker_running;
        state.worker_running = true;
        spawn
    };
    if spawn {
        thread::spawn(move || run_loadbalancer_worker(app));
    }
    (StatusCode::ACCEPTED, Json(CheckStarted { check_id: id })).into_response()
}

fn run_loadbalancer_worker(app: App) {
    loop {
        let (id, target_id, cancelled) = {
            let mut state = app.loadbalancer.lock().unwrap();
            let Some(s) = state.session.as_ref().filter(|s| s.phase == "running") else {
                state.worker_running = false;
                return;
            };
            (s.id.clone(), s.target_id.clone(), s.cancelled.clone())
        };
        if let Some(target) = app.targets.lock().unwrap().get(&target_id) {
            let _scope = scope_target(Some(target));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (app.loadbalancer_runner)(&cancelled, &mut |index, state, message| {
                    let mut active = app.loadbalancer.lock().unwrap();
                    if let Some(s) = active.session.as_mut().filter(|s| s.id == id) {
                        if let Some(stage) = s
                            .checks
                            .get_mut(index)
                            .filter(|_| !cancelled.load(Ordering::Acquire) || index == 3)
                        {
                            stage.state = state;
                            stage.message = message;
                        }
                    }
                })
            }));
            let mut active = app.loadbalancer.lock().unwrap();
            if let Some(s) = active
                .session
                .as_mut()
                .filter(|s| s.id == id && !cancelled.load(Ordering::Acquire))
            {
                if !matches!(result, Ok(Ok(_))) || !loadbalancer_probe::eligible(&s.checks) {
                    for stage in &mut s.checks {
                        if stage.state == "pending" || stage.state == "running" {
                            stage.state = "failed";
                            stage.message = "Could not verify LoadBalancer allocation and temporary Service deletion. Retry the test; inspect the Service if deletion failed.".into();
                        }
                    }
                }
                if let Ok(Ok(version)) = result {
                    if loadbalancer_probe::eligible(&s.checks) {
                        s.chart_version = Some(version);
                    }
                }
                s.phase = "complete";
                s.completed = Some(Instant::now());
            }
            if active.session.as_ref().is_some_and(|s| s.id == id) {
                active.worker_running = false;
                return;
            }
        } else {
            let mut active = app.loadbalancer.lock().unwrap();
            if active.session.as_ref().is_some_and(|s| s.id == id) {
                active.session.as_mut().unwrap().phase = "cancelled";
                active.worker_running = false;
                return;
            }
        }
        // Replacement waits for the old temporary Service deletion before running.
    }
}

async fn get_loadbalancer(
    State(app): State<App>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let state = app.loadbalancer.lock().unwrap();
    match state.session.as_ref().filter(|s| s.id == id) {
        Some(s) => Json(s.reply()).into_response(),
        None => (StatusCode::NOT_FOUND, "Unknown LoadBalancer check session").into_response(),
    }
}

async fn cancel_loadbalancer(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CancelRequest>,
) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    let mut state = app.loadbalancer.lock().unwrap();
    let cancelled = if let Some(s) = state
        .session
        .as_mut()
        .filter(|s| s.id == body.check_id && s.phase == "running")
    {
        s.cancelled.store(true, Ordering::Release);
        s.phase = "cancelled";
        true
    } else {
        false
    };
    Json(CancelReply { cancelled }).into_response()
}

#[derive(Default)]
struct StorageState {
    session: Option<StorageSession>,
    worker_running: bool,
}

struct StorageSession {
    id: String,
    target_id: String,
    input: Input,
    phase: &'static str,
    checks: Vec<storage_probe::Stage>,
    completed: Option<Instant>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct StorageReply {
    check_id: String,
    target_id: String,
    server: String,
    path: String,
    phase: &'static str,
    eligible: bool,
    checks: Vec<storage_probe::Stage>,
}

impl StorageSession {
    fn eligible(&self, target_id: &str, input: &Input) -> bool {
        self.target_id == target_id
            && &self.input == input
            && self.phase == "complete"
            && self
                .completed
                .is_some_and(|at| at.elapsed() <= storage_probe::VALIDITY)
            && storage_probe::eligible(&self.checks)
    }
    fn reply(&self) -> StorageReply {
        StorageReply {
            check_id: self.id.clone(),
            target_id: self.target_id.clone(),
            server: self.input.server.clone(),
            path: self.input.path.clone(),
            phase: self.phase,
            eligible: self.eligible(&self.target_id, &self.input),
            checks: self.checks.clone(),
        }
    }
}

async fn begin_storage(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<storage_probe::Request>,
) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    if app.shared.lock().unwrap().phase != Phase::Ready {
        return StatusCode::CONFLICT.into_response();
    }
    if app.targets.lock().unwrap().get(&body.target_id).is_none() {
        return (StatusCode::BAD_REQUEST, "Unknown destination").into_response();
    }
    let input = match storage_probe::canonical(&body.server, &body.path) {
        Ok(input) => input,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let id = match token() {
        Ok(id) => id,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let spawn = {
        let mut state = app.storage.lock().unwrap();
        if app.shared.lock().unwrap().phase != Phase::Ready {
            return StatusCode::CONFLICT.into_response();
        }
        if let Some(previous) = state.session.as_mut() {
            previous.cancelled.store(true, Ordering::Release);
        }
        state.session = Some(StorageSession {
            id: id.clone(),
            target_id: body.target_id,
            input,
            phase: "running",
            checks: storage_probe::stages(),
            completed: None,
            cancelled,
        });
        let spawn = !state.worker_running;
        state.worker_running = true;
        spawn
    };
    if spawn {
        thread::spawn(move || run_storage_worker(app));
    }
    (StatusCode::ACCEPTED, Json(CheckStarted { check_id: id })).into_response()
}

fn run_storage_worker(app: App) {
    loop {
        let (id, target_id, input, cancelled) = {
            let mut state = app.storage.lock().unwrap();
            let Some(session) = state.session.as_ref().filter(|s| s.phase == "running") else {
                state.worker_running = false;
                return;
            };
            (
                session.id.clone(),
                session.target_id.clone(),
                session.input.clone(),
                session.cancelled.clone(),
            )
        };
        let target = app.targets.lock().unwrap().get(&target_id);
        if let Some(target) = target {
            let _scope = scope_target(Some(target));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (app.storage_runner)(&input, &cancelled, &mut |index, stage, message| {
                    let mut state = app.storage.lock().unwrap();
                    if let Some(session) = state.session.as_mut().filter(|s| s.id == id) {
                        if let Some(item) = session
                            .checks
                            .get_mut(index)
                            .filter(|_| !cancelled.load(Ordering::Acquire) || index == 4)
                        {
                            item.state = stage;
                            item.message = message;
                        }
                    }
                })
            }));
            let mut state = app.storage.lock().unwrap();
            if let Some(session) = state
                .session
                .as_mut()
                .filter(|s| s.id == id && !cancelled.load(Ordering::Acquire))
            {
                if !matches!(result, Ok(Ok(()))) || !storage_probe::eligible(&session.checks) {
                    for item in &mut session.checks {
                        if item.state == "pending" || item.state == "running" {
                            item.state = "failed";
                            item.message = "Storage could not be verified. Retry the check; inspect temporary resources if cleanup failed.".into();
                        }
                    }
                }
                session.phase = "complete";
                session.completed = Some(Instant::now());
            }
            if state.session.as_ref().is_some_and(|s| s.id == id) {
                state.worker_running = false;
                return;
            }
        } else {
            let mut state = app.storage.lock().unwrap();
            if state.session.as_ref().is_some_and(|s| s.id == id) {
                if let Some(s) = state.session.as_mut() {
                    s.phase = "cancelled";
                }
                state.worker_running = false;
                return;
            }
        }
        // The old runner has finished cleanup; only now may its replacement run.
    }
}

async fn get_storage(
    State(app): State<App>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let state = app.storage.lock().unwrap();
    match state.session.as_ref().filter(|s| s.id == id) {
        Some(s) => Json(s.reply()).into_response(),
        None => (StatusCode::NOT_FOUND, "Unknown storage check session").into_response(),
    }
}

async fn cancel_storage(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CancelRequest>,
) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    let mut state = app.storage.lock().unwrap();
    let cancelled = if let Some(s) = state
        .session
        .as_mut()
        .filter(|s| s.id == body.check_id && s.phase == "running")
    {
        s.cancelled.store(true, Ordering::Release);
        s.phase = "cancelled";
        true
    } else {
        false
    };
    Json(CancelReply { cancelled }).into_response()
}

struct CheckSession {
    id: String,
    target_id: String,
    phase: &'static str,
    checks: Vec<Check>,
    completed: Option<Instant>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct CheckState {
    session: Option<CheckSession>,
    worker_running: bool,
}

#[derive(Serialize)]
struct CheckReply {
    check_id: String,
    target_id: String,
    phase: &'static str,
    eligible: bool,
    checks: Vec<Check>,
}

impl CheckSession {
    fn reply(&self) -> CheckReply {
        CheckReply {
            check_id: self.id.clone(),
            target_id: self.target_id.clone(),
            phase: self.phase,
            eligible: self.phase == "complete"
                && self
                    .completed
                    .is_some_and(|time| time.elapsed() <= CHECK_VALIDITY)
                && checks::eligible(&self.checks),
            checks: self.checks.clone(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRequest {
    target_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelRequest {
    check_id: String,
}

#[derive(Serialize)]
struct CheckStarted {
    check_id: String,
}

#[derive(Serialize)]
struct CancelReply {
    cancelled: bool,
}

async fn begin_checks(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CheckRequest>,
) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    if app.shared.lock().unwrap().phase != Phase::Ready {
        return StatusCode::CONFLICT.into_response();
    }
    if app.targets.lock().unwrap().get(&body.target_id).is_none() {
        return (StatusCode::BAD_REQUEST, "Unknown destination").into_response();
    }
    let id = match token() {
        Ok(id) => id,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let spawn = {
        let mut active = app.checks.lock().unwrap();
        if app.shared.lock().unwrap().phase != Phase::Ready {
            return StatusCode::CONFLICT.into_response();
        }
        if let Some(previous) = active.session.as_mut() {
            previous.cancelled.store(true, Ordering::Release);
        }
        active.session = Some(CheckSession {
            id: id.clone(),
            target_id: body.target_id,
            phase: "running",
            checks: checks::initial_checks(),
            completed: None,
            cancelled: cancelled.clone(),
        });
        let spawn = !active.worker_running;
        active.worker_running = true;
        spawn
    };
    if spawn {
        thread::spawn(move || run_check_worker(app));
    }
    (StatusCode::ACCEPTED, Json(CheckStarted { check_id: id })).into_response()
}

fn run_check_worker(app: App) {
    loop {
        let (id, target_id, cancelled) = {
            let mut state = app.checks.lock().unwrap();
            let Some(session) = state
                .session
                .as_ref()
                .filter(|session| session.phase == "running")
            else {
                state.worker_running = false;
                return;
            };
            (
                session.id.clone(),
                session.target_id.clone(),
                session.cancelled.clone(),
            )
        };
        let target = app.targets.lock().unwrap().get(&target_id);
        if let Some(target) = target {
            let _scope = scope_target(Some(target));
            // A faulty runner must never leave the check pending forever.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (app.check_runner)(cancelled.clone(), &mut |index, check_state, message| {
                        let mut state = app.checks.lock().unwrap();
                        if let Some(session) = state.session.as_mut().filter(|session| {
                            session.id == id && !cancelled.load(Ordering::Acquire)
                        }) {
                            if let Some(item) = session.checks.get_mut(index) {
                                item.state = check_state;
                                item.message = message.to_string();
                            }
                        }
                    });
                }));
            let mut state = app.checks.lock().unwrap();
            if let Some(session) = state
                .session
                .as_mut()
                .filter(|session| session.id == id && !cancelled.load(Ordering::Acquire))
            {
                if result.is_err() {
                    for item in &mut session.checks {
                        if item.state == "pending" || item.state == "running" {
                            item.state = if item.severity == "required" {
                                "failed"
                            } else {
                                "warning"
                            };
                            item.message =
                                "Could not complete this check. Retry the destination checks."
                                    .into();
                        }
                    }
                }
                session.phase = "complete";
                session.completed = Some(Instant::now());
            }
            if state
                .session
                .as_ref()
                .is_some_and(|session| session.id == id)
            {
                state.worker_running = false;
                return;
            }
        } else {
            let mut state = app.checks.lock().unwrap();
            if state
                .session
                .as_ref()
                .is_some_and(|session| session.id == id)
            {
                if let Some(session) = state.session.as_mut() {
                    session.phase = "cancelled";
                }
                state.worker_running = false;
                return;
            }
        }
        // Replacements are coalesced: one worker runs only the newest selection.
    }
}

async fn get_checks(
    State(app): State<App>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let active = app.checks.lock().unwrap();
    match active.session.as_ref().filter(|session| session.id == id) {
        Some(session) => Json(session.reply()).into_response(),
        None => (StatusCode::NOT_FOUND, "Unknown check session").into_response(),
    }
}

async fn cancel_checks(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<CancelRequest>,
) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    let mut active = app.checks.lock().unwrap();
    let cancelled = if let Some(session) = active
        .session
        .as_mut()
        .filter(|session| session.id == body.check_id && session.phase == "running")
    {
        session.cancelled.store(true, Ordering::Release);
        session.phase = "cancelled";
        true
    } else {
        false
    };
    Json(CancelReply { cancelled }).into_response()
}

struct Shared {
    invitation: Option<String>,
    session: String,
    csrf: String,
    phase: Phase,
    events: Vec<String>,
    result: Option<String>,
    access: Option<Access>,
    installed_target: Option<CommandTarget>,
    log_lines: std::collections::VecDeque<String>,
    log_bytes: usize,
    log_truncated: bool,
}

impl Shared {
    fn record(&mut self, event: String) {
        self.log_bytes += event.len() + 1;
        self.log_lines.push_back(event.clone());
        while self.log_bytes > MAX_INSTALL_LOG_BYTES {
            if let Some(old) = self.log_lines.pop_front() {
                self.log_bytes -= old.len() + 1;
                self.log_truncated = true;
            }
        }
        self.events.push(event);
        if self.events.len() > MAX_EVENTS {
            self.events.remove(0);
        }
    }

    fn download(&self) -> String {
        let mut body = String::with_capacity(self.log_bytes + 512);
        body.push_str(LOG_HEADER);
        body.push_str(match self.phase {
            Phase::Ready => "Phase: ready\n",
            Phase::Running => "Phase: running\n",
            Phase::Succeeded => "Phase: succeeded\n",
            Phase::Failed => "Phase: failed\n",
        });
        if let Some(result) = &self.result {
            body.push_str("Result: ");
            body.push_str(result);
            body.push('\n');
        }
        if self.log_truncated {
            body.push_str(LOG_TRUNCATED);
        }
        for line in &self.log_lines {
            body.push_str(line);
            body.push('\n');
        }
        body
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Ready,
    Running,
    Succeeded,
    Failed,
}

#[derive(Serialize)]
struct Snapshot {
    phase: Phase,
    events: Vec<String>,
    result: Option<String>,
    access: Option<Access>,
}

#[derive(Serialize)]
struct SessionReply {
    csrf: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Redeem {
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Form {
    destination: Destination,
    server_name: String,
    storage_mode: String,
    storage_size: String,
    storage_class: String,
    nfs_server: String,
    nfs_path: String,
    access_mode: String,
    backend_port: String,
    admin_username: String,
    admin_email: String,
    admin_password: String,
    admin_confirm: String,
    grafana: bool,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Destination {
    Context { target_id: String },
    SingleNode,
}

impl Form {
    fn options(self) -> Result<BootstrapOptions, String> {
        fn required(value: String, label: &str) -> Result<String, String> {
            let value = value.trim();
            if value.is_empty() || value.len() > 255 || value.contains(['\r', '\n', '\0']) {
                Err(format!(
                    "{label} must be present, single-line, and at most 255 characters"
                ))
            } else {
                Ok(value.to_string())
            }
        }
        let cluster_mode = match self.destination {
            Destination::Context { .. } => ClusterMode::Existing,
            Destination::SingleNode => ClusterMode::SingleNode,
        };
        let storage = match self.storage_mode.as_str() {
            "managed-nfs" => {
                let mut managed = StorageOptions::default_managed(false);
                managed.size = required(self.storage_size, "storage size")?;
                let digits = managed
                    .size
                    .chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .count();
                if digits == 0
                    || managed.size.starts_with('0')
                    || !matches!(
                        &managed.size[digits..],
                        "" | "Ki" | "Mi" | "Gi" | "Ti" | "Pi" | "Ei"
                    )
                {
                    return Err("invalid storage size".into());
                }
                managed.storage_class = if self.storage_class.trim().is_empty() {
                    None
                } else {
                    let class = required(self.storage_class, "storage class")?;
                    if !class
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')
                        || !class.starts_with(|ch: char| ch.is_ascii_alphanumeric())
                        || !class.ends_with(|ch: char| ch.is_ascii_alphanumeric())
                    {
                        return Err("invalid StorageClass name".into());
                    }
                    Some(class)
                };
                StorageModeOption::ManagedNfs(managed)
            }
            "external-nfs" => {
                let input = storage_probe::canonical(&self.nfs_server, &self.nfs_path)
                    .map_err(str::to_string)?;
                StorageModeOption::ExternalNfs(ExternalNfsOptions {
                    server: input.server,
                    export_path: input.path,
                })
            }
            _ => return Err("invalid storage mode".into()),
        };
        let mut install = InstallOptions::default_for_bootstrap(false);
        install.server_name = Some(required(self.server_name, "server name")?);
        install.web_access = match self.access_mode.as_str() {
            "nodeport" => {
                let port = parse_port(self.backend_port).map_err(|_| "invalid gateway NodePort")?;
                if !(30000..=32767).contains(&port) {
                    return Err("NodePort must be between 30000 and 32767".into());
                }
                WebAccess::NodePort(port)
            }
            "loadbalancer" => WebAccess::LoadBalancer,
            "port-forward" => WebAccess::PortForward,
            _ => return Err("invalid access mode".into()),
        };
        let username = required(self.admin_username, "admin username")?;
        let email = required(self.admin_email, "admin email")?;
        if !email.contains('@') {
            return Err("invalid admin email".into());
        }
        if self.admin_password.len() < 8 || self.admin_password.len() > 1024 {
            return Err("admin password must be 8-1024 characters".into());
        }
        if self.admin_password != self.admin_confirm {
            return Err("admin passwords do not match".into());
        }
        install.admin = Some(AdminUser {
            username,
            email,
            password: self.admin_password,
        });
        Ok(BootstrapOptions {
            cluster_mode,
            install,
            storage: BootstrapStorageOptions { mode: storage },
            grafana_enabled: self.grafana,
        })
    }
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "Forbidden").into_response()
}

fn host<'a>(app: &App, headers: &'a HeaderMap) -> Option<&'a str> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    app.hosts.contains(host).then_some(host)
}

fn origin(app: &App, headers: &HeaderMap) -> bool {
    host(app, headers).is_some_and(|host| {
        headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            == Some(format!("http://{host}").as_str())
    })
}

fn authenticated(app: &App, headers: &HeaderMap) -> bool {
    if host(app, headers).is_none() {
        return false;
    }
    let guard = app.shared.lock().unwrap();
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|cookies| {
            cookies
                .split(';')
                .any(|cookie| cookie.trim() == format!("kubarr_session={}", guard.session))
        })
        && guard.invitation.is_none()
}

fn mutation(app: &App, headers: &HeaderMap) -> bool {
    authenticated(app, headers)
        && origin(app, headers)
        && headers
            .get("x-kubarr-csrf")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|value| value == app.shared.lock().unwrap().csrf)
}

async fn page(State(app): State<App>, headers: HeaderMap) -> Response {
    if host(&app, &headers).is_none() {
        return forbidden();
    }
    Html(include_str!("bootstrap_web.html")).into_response()
}

async fn redeem(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<Redeem>,
) -> Response {
    if !origin(&app, &headers) {
        return forbidden();
    }
    let mut guard = app.shared.lock().unwrap();
    if guard.invitation.as_deref() != Some(request.token.as_str()) {
        return forbidden();
    }
    guard.invitation = None;
    let csrf = guard.csrf.clone();
    let cookie = format!(
        "kubarr_session={}; HttpOnly; SameSite=Strict; Path=/",
        guard.session
    );
    let mut response = Json(SessionReply { csrf }).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    response
}

async fn session(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let csrf = app.shared.lock().unwrap().csrf.clone();
    Json(SessionReply { csrf }).into_response()
}

async fn status(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let guard = app.shared.lock().unwrap();
    Json(Snapshot {
        phase: guard.phase,
        events: guard.events.clone(),
        result: guard.result.clone(),
        access: if guard.phase == Phase::Succeeded {
            guard.access.clone()
        } else {
            None
        },
    })
    .into_response()
}

async fn install_log(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let body = app.shared.lock().unwrap().download();
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"kubarr-install.log\"",
            ),
        ],
        body,
    )
        .into_response()
}

async fn access(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let (old, target) = {
        let guard = app.shared.lock().unwrap();
        if guard.phase != Phase::Succeeded {
            return StatusCode::CONFLICT.into_response();
        }
        (guard.access.clone(), guard.installed_target.clone())
    };
    let Some(old) = old else {
        return StatusCode::CONFLICT.into_response();
    };
    if old.kind != AccessKind::Loadbalancer {
        return Json(old).into_response();
    }
    // A LoadBalancer install must retain the selected context; never fall back
    // to whatever kubeconfig the CLI environment happens to use later.
    let Some(target) = target else {
        return StatusCode::CONFLICT.into_response();
    };
    // The selected install target is frozen; never use the caller's current kubeconfig.
    let reader = app.access_reader;
    let updated = tokio::task::spawn_blocking(move || {
        let _scope = scope_target(Some(target));
        reader()
    })
    .await;
    match updated {
        Ok(updated) => {
            let mut guard = app.shared.lock().unwrap();
            // Keep a previously assigned address if a subsequent read temporarily fails.
            if updated.url.is_some() || guard.access.as_ref().is_none_or(|old| old.url.is_none()) {
                guard.access = Some(updated);
            }
            Json(guard.access.clone().unwrap()).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Serialize)]
struct Preflight {
    kubectl: bool,
    helm: bool,
    context: Option<String>,
    storage_class: Option<String>,
}

#[derive(Deserialize)]
struct PreflightQuery {
    target_id: Option<String>,
}

#[derive(Serialize)]
struct TargetReply {
    targets: Vec<Choice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    kubeconfig: String,
}

async fn list_targets(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut targets = app.targets.lock().unwrap();
        targets.discover();
        TargetReply {
            targets: targets.choices(),
        }
    })
    .await;
    match result {
        Ok(reply) => Json(reply).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn upload(State(app): State<App>, headers: HeaderMap, Json(body): Json<Upload>) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    if app.shared.lock().unwrap().phase != Phase::Ready {
        return StatusCode::CONFLICT.into_response();
    }
    if body.kubeconfig.len() > MAX_CONFIG {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Kubeconfig too large").into_response();
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut targets = app.targets.lock().unwrap();
        if app.shared.lock().unwrap().phase != Phase::Ready {
            return Err((StatusCode::CONFLICT, "Installation already started"));
        }
        targets
            .upload(&body.kubeconfig)
            .map(|targets| TargetReply { targets })
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid or unsupported kubeconfig"))
    })
    .await;
    match result {
        Ok(Ok(reply)) => Json(reply).into_response(),
        Ok(Err(err)) => err.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn preflight(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<PreflightQuery>,
) -> Response {
    if !authenticated(&app, &headers) {
        return forbidden();
    }
    if query.target_id.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            "Use /api/checks for a selected destination",
        )
            .into_response();
    }
    let info = tokio::task::spawn_blocking(move || Preflight {
        kubectl: command_exists("kubectl"),
        helm: command_exists("helm"),
        context: None,
        storage_class: None,
    })
    .await;
    match info {
        Ok(info) => Json(info).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn safe_event(message: &str) -> Option<String> {
    // Only known step headings are safe; the detail line may be user input.
    const STEPS: &[&str] = &[
        "Cluster",
        "Tools",
        "Nodes",
        "APIs",
        "Permissions",
        "Optional",
        "Storage",
        "Storage Config",
        "Fluent Bit",
        "VictoriaMetrics",
        "VictoriaLogs",
        "Grafana",
        "Database",
        "Install",
        "Done",
    ];
    if let Some((heading, _)) = message.split_once('\n') {
        if STEPS.iter().any(|name| heading == format!(">> {name}")) {
            return Some(heading.to_string());
        }
    }
    if message.starts_with("[RUN]") || message.starts_with("[PLAN]") {
        return Some("Running installation command…".into());
    }
    const SAFE_DIAGNOSTICS: &[&str] = &[
        "[FAIL] kubectl cannot reach the Kubernetes API",
        "[FAIL] no Ready nodes found",
        "[FAIL] cannot list nodes",
        "[WARN] no storage class access detected; media app storage may need manual setup",
        "[WARN] no default StorageClass detected; pass --storage-class or managed storage PVC may stay Pending",
        "[WARN] metrics API is not available; resource charts may be limited until metrics-server is installed",
        "[WARN] external NFS server did not accept a connection on port 2049; Kubernetes nodes may still reach it",
        "[WARN] could not resolve external NFS server; continuing",
        "[WARN] k3s installed, but kubectl is not using it yet",
        "[WARN] try: export KUBECONFIG=/etc/rancher/k3s/k3s.yaml",
    ];
    if SAFE_DIAGNOSTICS.contains(&message)
        || REQUIRED_API_RESOURCES
            .iter()
            .any(|req| message == format!("[FAIL] {} API is not available", req.label))
        || REQUIRED_PERMISSIONS
            .iter()
            .any(|req| message == format!("[FAIL] cannot {} {}", req.verb, req.resource))
    {
        return Some(message.to_string());
    }
    // URL host must be a literal IP (or the fixed no-address placeholder).
    if let Some(address) = message.strip_prefix("open: http://") {
        let literal = address.parse::<std::net::SocketAddr>().is_ok();
        let fallback = address
            .strip_prefix("<node-ip>:")
            .is_some_and(|port| port.parse::<u16>().is_ok_and(|port| port > 0));
        if literal || fallback {
            return Some(message.to_string());
        }
    }
    if message == "forward: kubectl port-forward -n kubarr-backend svc/kubarr-backend 8000:8000" {
        return Some(message.to_string());
    }
    if [GATEWAY_FORWARD, LB_PENDING, LB_ERROR].contains(&message) {
        return Some(message.to_string());
    }
    if let Some(address) = message.strip_prefix("open: http://") {
        if let Some((host, port)) = address.rsplit_once(':') {
            if port == "8080" && valid_hostname(host) {
                return Some(message.to_string());
            }
        }
    }
    None
}

fn safe_failure_result(error: &str) -> String {
    // Installer errors may contain file paths, command arguments, or tool text.
    // Only exact fixed messages can be returned; unknown errors stay generic.
    for known in [
        "Helm 4 is required (target: 4.3.0); check `helm version` locally",
        "required tool 'helm' was not found in PATH",
        "required tool 'kubectl' was not found in PATH",
        "required tool 'curl' was not found in PATH",
    ] {
        if error == known {
            return known.to_string();
        }
    }
    if error
        .strip_prefix("database bootstrap SQL failed (exit status ")
        .and_then(|code| code.strip_suffix(')'))
        .is_some_and(|code| code.parse::<i32>().is_ok())
    {
        return error.to_string();
    }
    "Installation failed. Check the activity log for the failed operation.".into()
}

fn execute(
    app: App,
    options: BootstrapOptions,
    installer: fn(&BootstrapOptions) -> Result<(), String>,
    uploaded: bool,
) {
    let (tx, rx) = mpsc::channel();
    install_events::set_install_log(tx);
    // External tools may echo kubeconfig credential material on failure. Do
    // not write their raw output even to the local diagnostic log for uploads.
    install_events::set_diagnostic_log(if uploaded {
        None
    } else {
        app.diagnostics.clone()
    });
    let observer = app.clone();
    let collector = thread::spawn(move || {
        for event in rx {
            let line = match event {
                install_events::InstallEvent::Access(value) => {
                    observer.shared.lock().unwrap().access = Some(value);
                    continue;
                }
                install_events::InstallEvent::Legacy(message) => safe_event(&message),
                trusted @ (install_events::InstallEvent::Stage { .. }
                | install_events::InstallEvent::Progress { .. }) => trusted.browser_line(),
            };
            if let Some(event) = line.filter(|line| line.len() <= MAX_EVENT_BYTES) {
                let mut guard = observer.shared.lock().unwrap();
                guard.record(event);
            }
        }
    });
    let result = std::panic::catch_unwind(|| installer(&options))
        .unwrap_or_else(|_| Err("installer encountered an unexpected error".into()));
    install_events::clear_install_log();
    install_events::set_diagnostic_log(None);
    let installed_target = current_target();
    let _ = collector.join();
    let mut guard = app.shared.lock().unwrap();
    match result {
        Ok(()) => {
            guard.phase = Phase::Succeeded;
            guard.installed_target = installed_target;
            guard.result =
                Some("Kubarr bootstrap completed. See the access instructions above.".into());
        }
        Err(err) => {
            guard.phase = Phase::Failed;
            guard.access = None;
            let message = safe_failure_result(&err);
            guard.result = Some(if app.diagnostics.is_some() && !uploaded {
                format!("{message} For local-only failure details, see the private diagnostic log path printed in the CLI terminal.")
            } else {
                message
            });
        }
    }
    drop(guard);
    app.completion.notify_waiters();
}

async fn start(State(app): State<App>, headers: HeaderMap, Json(form): Json<Form>) -> Response {
    if !mutation(&app, &headers) {
        return forbidden();
    }
    let (target, uploaded) = match &form.destination {
        Destination::Context { target_id } => {
            let registry = app.targets.lock().unwrap();
            match registry.get(target_id) {
                Some(target) => (Some(target), registry.uploaded(target_id)),
                None => return (StatusCode::BAD_REQUEST, "Unknown destination").into_response(),
            }
        }
        Destination::SingleNode => (None, false),
    };
    let context_id = match &form.destination {
        Destination::Context { target_id } => Some(target_id.clone()),
        Destination::SingleNode => None,
    };
    let mut options = match form.options() {
        Ok(options) => options,
        Err(err) => return (StatusCode::BAD_REQUEST, err).into_response(),
    };
    let storage = app.storage.lock().unwrap();
    if storage.worker_running
        || storage
            .session
            .as_ref()
            .is_some_and(|session| session.phase == "running")
    {
        return (
            StatusCode::CONFLICT,
            "Storage validation is still running or cleaning up",
        )
            .into_response();
    }
    if let (Some(target_id), StorageModeOption::ExternalNfs(external)) =
        (context_id.as_deref(), &options.storage.mode)
    {
        let input = Input {
            server: external.server.clone(),
            path: external.export_path.clone(),
        };
        if !storage
            .session
            .as_ref()
            .is_some_and(|session| session.eligible(target_id, &input))
        {
            return (
                StatusCode::CONFLICT,
                "Check external NFS storage again before installing",
            )
                .into_response();
        }
    }
    let loadbalancer = app.loadbalancer.lock().unwrap();
    if loadbalancer.worker_running
        || loadbalancer
            .session
            .as_ref()
            .is_some_and(|s| s.phase == "running")
    {
        return (
            StatusCode::CONFLICT,
            "LoadBalancer validation is still running or cleaning up",
        )
            .into_response();
    }
    if matches!(options.install.web_access, WebAccess::LoadBalancer) {
        let Some(target_id) = context_id.as_deref() else {
            return (StatusCode::BAD_REQUEST, "LoadBalancer requires an existing cluster with a verified address allocator. Choose an existing context or use NodePort or port-forward for new local k3s.").into_response();
        };
        if !loadbalancer
            .session
            .as_ref()
            .is_some_and(|s| s.eligible(target_id))
        {
            return (
                StatusCode::CONFLICT,
                "Check the load balancer for this destination again before installing",
            )
                .into_response();
        }
        options.install.gateway_chart_version = loadbalancer
            .session
            .as_ref()
            .and_then(|s| s.chart_version.clone());
        if options.install.gateway_chart_version.is_none() {
            return (
                StatusCode::CONFLICT,
                "Check the load balancer for this destination again before installing",
            )
                .into_response();
        }
        if !loadbalancer_probe::chart_version_still_selected(
            options.install.gateway_chart_version.as_deref().unwrap(),
        ) {
            return (StatusCode::CONFLICT, "Gateway chart changed since the load balancer check; run the test again before installing").into_response();
        }
    }
    {
        // Check and phase transition are atomic with replacement/cancellation.
        let active = app.checks.lock().unwrap();
        if let Some(target_id) = context_id.as_deref() {
            let valid = active.session.as_ref().is_some_and(|session| {
                session.target_id == target_id
                    && session.phase == "complete"
                    && session
                        .completed
                        .is_some_and(|time| time.elapsed() <= CHECK_VALIDITY)
                    && checks::eligible(&session.checks)
            });
            if !valid {
                return (
                    StatusCode::CONFLICT,
                    "Check this destination again before installing",
                )
                    .into_response();
            }
        }
        let mut guard = app.shared.lock().unwrap();
        if guard.phase != Phase::Ready {
            return (StatusCode::CONFLICT, "Installation already started").into_response();
        }
        guard.phase = Phase::Running;
        guard.events.clear();
        guard.log_lines.clear();
        guard.log_bytes = 0;
        guard.log_truncated = false;
        guard.access = None;
        guard.installed_target = None;
        drop(active);
        drop(storage);
        drop(loadbalancer);
    }
    let installer = app.installer;
    thread::spawn(move || {
        let _scope = scope_target(target);
        execute(app, options, installer, uploaded);
    });
    StatusCode::ACCEPTED.into_response()
}

fn router(app: App) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/redeem", post(redeem))
        .route("/api/session", get(session))
        .route("/api/status", get(status))
        .route("/api/access", get(access))
        .route("/api/install-log", get(install_log))
        .route("/api/preflight", get(preflight))
        .route("/api/checks", post(begin_checks))
        .route("/api/checks/cancel", post(cancel_checks))
        .route("/api/checks/{check_id}", get(get_checks))
        .route("/api/storage-checks", post(begin_storage))
        .route("/api/storage-checks/cancel", post(cancel_storage))
        .route("/api/storage-checks/{check_id}", get(get_storage))
        .route("/api/loadbalancer-checks", post(begin_loadbalancer))
        .route("/api/loadbalancer-checks/cancel", post(cancel_loadbalancer))
        .route("/api/loadbalancer-checks/{check_id}", get(get_loadbalancer))
        .route("/api/targets", get(list_targets))
        .route("/api/targets/upload", post(upload).layer(DefaultBodyLimit::max(MAX_CONFIG * 6 + 4096)))
        .route("/api/start", post(start))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(axum::middleware::map_response(|mut response: Response| async move {
            let headers = response.headers_mut();
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"));
            headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
            response
        }))
        .with_state(app)
}

async fn wait_for_completion(app: &App) {
    loop {
        // Register before checking phase to avoid losing a completion notification.
        let notified = app.completion.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if app.shared.lock().unwrap().phase != Phase::Running {
            return;
        }
        notified.await;
    }
}

async fn graceful_shutdown(app: App, signal: impl std::future::Future<Output = ()>) {
    signal.await;
    {
        let mut state = app.loadbalancer.lock().unwrap();
        if let Some(s) = state.session.as_mut().filter(|s| s.phase == "running") {
            s.cancelled.store(true, Ordering::Release);
            s.phase = "cancelled";
        }
    }
    while app.loadbalancer.lock().unwrap().worker_running {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    {
        let mut storage = app.storage.lock().unwrap();
        if let Some(session) = storage.session.as_mut().filter(|s| s.phase == "running") {
            session.cancelled.store(true, Ordering::Release);
            session.phase = "cancelled";
        }
    }
    while app.storage.lock().unwrap().worker_running {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if app.shared.lock().unwrap().phase == Phase::Running {
        eprintln!("Installation is still running; keeping the wizard available and exiting automatically after it finishes.");
        wait_for_completion(&app).await;
    }
}

fn launch_browser(
    opener: &str,
    url: &str,
    report_failure: impl FnOnce(String) + Send + 'static,
) -> Result<(), String> {
    let mut child = Command::new(opener)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("could not start browser opener: {err}"))?;
    thread::spawn(move || match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => report_failure(format!("browser opener failed ({status})")),
        Err(err) => report_failure(format!("could not wait for browser opener: {err}")),
    });
    Ok(())
}

pub fn run_web(port: u16) -> Result<(), String> {
    let mode = detect_environment();
    let bind = if mode == Environment::Ssh {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::LOCALHOST
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((bind, port)))
            .await.map_err(|err| format!("could not bind bootstrap UI: {err}"))?;
        let port = listener.local_addr().map_err(|err| err.to_string())?.port();
        let hosts = candidate_hosts(mode);
        let allowed: HashSet<String> = hosts.iter().chain(std::iter::once(&"localhost".to_string()))
            .map(|host| authority(host, port)).collect();
        let invite = token()?;
        let session = token()?;
        let csrf = token()?;
        let targets = Arc::new(Mutex::new(Registry::new()?));
        // All fallible setup above precedes creation of the persistent local
        // diagnostic file. No later setup failure leaves an orphaned log.
        let (diagnostic_path, diagnostics) = install_events::create_diagnostic_log()?;
        let app = App { shared: Arc::new(Mutex::new(Shared {
            invitation: Some(invite.clone()), session, csrf,
            phase: Phase::Ready, events: vec![], result: None,
            access: None, installed_target: None,
            log_lines: Default::default(), log_bytes: 0, log_truncated: false,
        })), hosts: Arc::new(allowed), installer: run_bootstrap_install,
        diagnostics: Some(diagnostics), completion: Arc::new(Notify::new()), targets,
         checks: Arc::new(Mutex::new(CheckState::default())),
         check_runner: real_check_runner,
         storage: Arc::new(Mutex::new(StorageState::default())),
         storage_runner: storage_probe::run,
         loadbalancer: Arc::new(Mutex::new(LoadBalancerState::default())),
         loadbalancer_runner: loadbalancer_probe::run,
         access_reader: install_access::refresh_loadbalancer };
        println!("Kubarr bootstrap wizard listening on {bind}:{port}");
        for host in &hosts {
            println!("  http://{}/#{}", authority(host, port), invite);
        }
        if mode == Environment::Ssh {
            println!("Remote wizard uses plain HTTP on all interfaces. Anyone who can observe HTTP traffic can read the admin password and session. Use a trusted network, or SSH-forward: ssh -L {port}:127.0.0.1:{port} <user>@<server> and open the printed 127.0.0.1 URL locally.");
            println!("Addresses are candidates; routing and firewall rules may prevent access.");
        } else if mode == Environment::Desktop {
            let url = format!("http://127.0.0.1:{port}/#{invite}");
            if let Err(err) = launch_browser("xdg-open", &url, |err| {
                eprintln!("{err}; open the printed URL manually.");
            }) {
                eprintln!("{err}; open the printed URL manually.");
            }
        }
        println!("Private failure diagnostics: {} (mode 0600, capped at 1 MiB). This local file may contain secrets from Helm, kubectl, or k3s; inspect it only on this host and remove it when finished.", diagnostic_path.display());
        println!("Keep this CLI running during installation; Ctrl-C exits after any running install finishes.");
        let server = axum::serve(listener, router(app.clone()));
        server.with_graceful_shutdown(graceful_shutdown(app, async {
            let _ = tokio::signal::ctrl_c().await;
        })).await.map_err(|err| err.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::{json, Value};
    use std::sync::atomic::AtomicUsize;
    #[cfg(target_os = "linux")]
    use std::sync::OnceLock;
    use std::time::Duration;
    use tower::ServiceExt;

    use crate::install_events::INSTALL_TEST_LOCK;

    fn fixture() -> App {
        App {
            shared: Arc::new(Mutex::new(Shared {
                invitation: Some("invite-secret".into()),
                session: "session-secret".into(),
                csrf: "csrf-secret".into(),
                phase: Phase::Ready,
                events: vec![],
                result: None,
                access: None,
                installed_target: None,
                log_lines: Default::default(),
                log_bytes: 0,
                log_truncated: false,
            })),
            hosts: Arc::new(["127.0.0.1:32123".to_string()].into()),
            installer: fake_success,
            diagnostics: None,
            completion: Arc::new(Notify::new()),
            targets: Arc::new(Mutex::new(Registry::empty_for_tests())),
            checks: Arc::new(Mutex::new(CheckState::default())),
            check_runner: fake_check_runner,
            storage: Arc::new(Mutex::new(StorageState::default())),
            storage_runner: fake_storage_runner,
            loadbalancer: Arc::new(Mutex::new(LoadBalancerState::default())),
            loadbalancer_runner: fake_loadbalancer_runner,
            access_reader: fake_access_reader,
        }
    }

    fn fake_access_reader() -> Access {
        Access {
            kind: AccessKind::Loadbalancer,
            url: Some("http://[2001:db8::2]:8080".into()),
            instructions: vec![],
            pending: false,
        }
    }

    fn fake_loadbalancer_runner(
        _: &AtomicBool,
        publish: &mut dyn FnMut(usize, &'static str, String),
    ) -> Result<String, String> {
        for (i, stage) in loadbalancer_probe::stages().iter().enumerate() {
            publish(i, "passed", format!("Synthetic {}", stage.id));
        }
        Ok("1.29.2+5.3".into())
    }

    async fn await_installer(app: &App) {
        tokio::time::timeout(Duration::from_secs(3), wait_for_completion(app))
            .await
            .expect("synthetic installer did not reach a terminal state");
        assert!(matches!(
            app.shared.lock().unwrap().phase,
            Phase::Succeeded | Phase::Failed
        ));
    }

    async fn checked_loadbalancer(routes: &Router, target: &str) -> String {
        let response = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/loadbalancer-checks",
                Some(json!({"target_id":target})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let data: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        let id = data["check_id"].as_str().unwrap().to_owned();
        for _ in 0..100 {
            let response = routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/loadbalancer-checks/{id}"),
                    None,
                    true,
                ))
                .await
                .unwrap();
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap())
                    .unwrap();
            if body["phase"] == "complete" {
                assert_eq!(body["eligible"], true);
                return id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("synthetic LoadBalancer check did not finish");
    }

    #[tokio::test]
    async fn loadbalancer_routes_gate_only_the_fresh_selected_context_and_reject_local_k3s() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let target = synthetic_target(&app);
        let routes = router(app.clone());
        let body = Some(json!({"target_id":target}));
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/loadbalancer-checks",
                    body.clone(),
                    false
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut without_csrf = request("POST", "/api/loadbalancer-checks", body.clone(), true);
        without_csrf.headers_mut().remove("x-kubarr-csrf");
        assert_eq!(
            routes.clone().oneshot(without_csrf).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "GET",
                    "/api/loadbalancer-checks/missing",
                    None,
                    false
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/loadbalancer-checks",
                    Some(json!({"target_id":"missing"})),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/loadbalancer-checks",
                    Some(json!({"target_id":target,"extra":true})),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut form = valid_form();
        form["access_mode"] = json!("loadbalancer");
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        form["destination"] = json!({"kind":"context","target_id":target});
        checked_target(&routes, &target).await;
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let first = checked_loadbalancer(&routes, &target).await;
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/loadbalancer-checks/cancel",
                    Some(json!({"check_id":first})),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        {
            let mut state = app.loadbalancer.lock().unwrap();
            state.session.as_mut().unwrap().completed =
                Some(Instant::now() - loadbalancer_probe::VALIDITY - Duration::from_secs(1));
        }
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        checked_loadbalancer(&routes, &target).await;
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/loadbalancer-checks/{first}"),
                    None,
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form), true))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        await_installer(&app).await;
    }

    static LB_CLEANUP_ENTERED: AtomicBool = AtomicBool::new(false);
    static LB_CLEANUP_RELEASED: AtomicBool = AtomicBool::new(false);

    fn held_loadbalancer_runner(
        _: &AtomicBool,
        publish: &mut dyn FnMut(usize, &'static str, String),
    ) -> Result<String, String> {
        publish(3, "running", "Cleaning up".into());
        LB_CLEANUP_ENTERED.store(true, Ordering::Release);
        while !LB_CLEANUP_RELEASED.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
        Err("synthetic cleanup stopped".into())
    }

    fn scoped_loadbalancer_runner(
        cancelled: &AtomicBool,
        publish: &mut dyn FnMut(usize, &'static str, String),
    ) -> Result<String, String> {
        let command = crate::util::external_command("kubectl");
        assert!(command.get_args().any(|arg| arg == "--context=destination"));
        assert!(command
            .get_envs()
            .any(|(key, path)| key == "KUBECONFIG" && path.is_some()));
        fake_loadbalancer_runner(cancelled, publish)
    }

    #[tokio::test]
    async fn loadbalancer_worker_uses_selected_private_target_and_replacement_invalidates_old_id() {
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        app.loadbalancer_runner = scoped_loadbalancer_runner;
        let target = synthetic_target(&app);
        let routes = router(app.clone());
        let first = checked_loadbalancer(&routes, &target).await;
        checked_loadbalancer(&routes, &target).await;
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/loadbalancer-checks/{first}"),
                    None,
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            app.loadbalancer
                .lock()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .target_id,
            target
        );
    }

    #[tokio::test]
    async fn cancelled_loadbalancer_cleanup_blocks_start_even_for_other_access_modes() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        LB_CLEANUP_ENTERED.store(false, Ordering::Release);
        LB_CLEANUP_RELEASED.store(false, Ordering::Release);
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        app.loadbalancer_runner = held_loadbalancer_runner;
        let target = synthetic_target(&app);
        let routes = router(app.clone());
        let response = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/loadbalancer-checks",
                Some(json!({"target_id":target})),
                true,
            ))
            .await
            .unwrap();
        let data: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        for _ in 0..100 {
            if LB_CLEANUP_ENTERED.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(LB_CLEANUP_ENTERED.load(Ordering::Acquire));
        let response = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/loadbalancer-checks/cancel",
                Some(json!({"check_id":data["check_id"]})),
                true,
            ))
            .await
            .unwrap();
        let cancel: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(cancel["cancelled"], true);
        let response = routes
            .clone()
            .oneshot(request(
                "GET",
                &format!(
                    "/api/loadbalancer-checks/{}",
                    data["check_id"].as_str().unwrap()
                ),
                None,
                true,
            ))
            .await
            .unwrap();
        let snapshot: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(snapshot["phase"], "cancelled");
        assert_eq!(snapshot["eligible"], false);
        let response = routes
            .clone()
            .oneshot(request("POST", "/api/start", Some(valid_form()), true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        LB_CLEANUP_RELEASED.store(true, Ordering::Release);
        for _ in 0..100 {
            if !app.loadbalancer.lock().unwrap().worker_running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!app.loadbalancer.lock().unwrap().worker_running);
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(valid_form()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        await_installer(&app).await;
    }

    fn fake_storage_runner(
        _: &Input,
        cancelled: &AtomicBool,
        publish: &mut dyn FnMut(usize, &'static str, String),
    ) -> Result<(), String> {
        for (index, item) in storage_probe::stages().iter().enumerate() {
            if cancelled.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            publish(index, "passed", format!("Synthetic {}", item.id));
        }
        Ok(())
    }

    async fn checked_storage(routes: &Router, target: &str, server: &str, path: &str) -> String {
        let response = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/storage-checks",
                Some(json!({"target_id":target,"server":server,"path":path})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        let id = body["check_id"].as_str().unwrap().to_owned();
        for _ in 0..100 {
            let response = routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/storage-checks/{id}"),
                    None,
                    true,
                ))
                .await
                .unwrap();
            let data: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap())
                    .unwrap();
            if data["phase"] == "complete" {
                assert_eq!(data["eligible"], true);
                assert_eq!(data["server"], server.trim());
                assert_eq!(data["path"], path.trim().trim_end_matches('/'));
                return id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("synthetic storage check did not complete");
    }

    #[tokio::test]
    async fn storage_routes_auth_canonicalization_expiry_and_start_gate() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let target = synthetic_target(&app);
        let routes = router(app.clone());
        let body = json!({"target_id":target,"server":" nfs.example ","path":" /export/ "});
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/storage-checks",
                    Some(body.clone()),
                    false
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut no_csrf = request("POST", "/api/storage-checks", Some(body.clone()), true);
        no_csrf.headers_mut().remove("x-kubarr-csrf");
        assert_eq!(
            routes.clone().oneshot(no_csrf).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request("GET", "/api/storage-checks/missing", None, false))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/storage-checks",
                    Some(json!({"target_id":"unknown","server":"nfs.example","path":"/export"})),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/storage-checks",
                    Some(
                        json!({"target_id":target,"server":"nfs.example","path":"/export/../other"})
                    ),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let checked = checked_target(&routes, &target).await;
        let mut form = valid_form();
        form["destination"] = json!({"kind":"context","target_id":target});
        form["storage_mode"] = json!("external-nfs");
        form["nfs_server"] = json!("nfs.example");
        form["nfs_path"] = json!("/export");
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let first = checked_storage(&routes, &target, " nfs.example ", " /export/ ").await;
        let snapshot = routes
            .clone()
            .oneshot(request(
                "GET",
                &format!("/api/storage-checks/{first}"),
                None,
                true,
            ))
            .await
            .unwrap();
        let snapshot: Value =
            serde_json::from_slice(&to_bytes(snapshot.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(snapshot["server"], "nfs.example");
        assert_eq!(snapshot["path"], "/export");
        form["nfs_path"] = json!("/other");
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        form["nfs_path"] = json!("/export");
        {
            let mut guard = app.storage.lock().unwrap();
            guard.session.as_mut().unwrap().completed =
                Some(Instant::now() - storage_probe::VALIDITY - Duration::from_secs(1));
        }
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let second = checked_storage(&routes, &target, "nfs.example", "/export").await;
        assert_ne!(first, second);
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/storage-checks/{first}"),
                    None,
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/storage-checks/cancel",
                    Some(json!({"check_id":first})),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.checks.lock().unwrap().session.as_ref().unwrap().id,
            checked
        );
        // A successful, canonicalized storage check permits this checked target.
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form), true))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        await_installer(&app).await;
    }

    fn slow_storage_runner(
        _: &Input,
        cancelled: &AtomicBool,
        publish: &mut dyn FnMut(usize, &'static str, String),
    ) -> Result<(), String> {
        publish(0, "running", "Waiting".into());
        for _ in 0..80 {
            if cancelled.load(Ordering::Acquire) {
                return Err("cancelled".into());
            }
            thread::sleep(Duration::from_millis(5));
        }
        fake_storage_runner(
            &Input {
                server: String::new(),
                path: String::new(),
            },
            cancelled,
            publish,
        )
    }

    static STORAGE_CLEANUP_ENTERED: AtomicBool = AtomicBool::new(false);
    static STORAGE_CLEANUP_RELEASED: AtomicBool = AtomicBool::new(false);

    fn held_cleanup_storage_runner(
        _: &Input,
        _: &AtomicBool,
        publish: &mut dyn FnMut(usize, &'static str, String),
    ) -> Result<(), String> {
        publish(4, "running", "Cleaning up".into());
        STORAGE_CLEANUP_ENTERED.store(true, Ordering::Release);
        while !STORAGE_CLEANUP_RELEASED.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(5));
        }
        publish(4, "passed", "Cleaned up".into());
        Err("Synthetic cancelled probe".into())
    }

    #[tokio::test]
    async fn start_rejects_managed_and_local_modes_until_cancelled_storage_cleanup_finishes() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        STORAGE_CLEANUP_ENTERED.store(false, Ordering::Release);
        STORAGE_CLEANUP_RELEASED.store(false, Ordering::Release);
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        app.storage_runner = held_cleanup_storage_runner;
        let target = synthetic_target(&app);
        let routes = router(app.clone());
        checked_target(&routes, &target).await;
        let response = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/storage-checks",
                Some(json!({"target_id":target,"server":"nfs.example","path":"/export"})),
                true,
            ))
            .await
            .unwrap();
        let started: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        for _ in 0..100 {
            if STORAGE_CLEANUP_ENTERED.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(STORAGE_CLEANUP_ENTERED.load(Ordering::Acquire));
        let cancelled = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/storage-checks/cancel",
                Some(json!({"check_id":started["check_id"]})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(cancelled.status(), StatusCode::OK);
        let mut managed = valid_form();
        managed["destination"] = json!({"kind":"context","target_id":target});
        for form in [managed, valid_form()] {
            let response = routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form), true))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(app.shared.lock().unwrap().phase, Phase::Ready);
        }
        STORAGE_CLEANUP_RELEASED.store(true, Ordering::Release);
        for _ in 0..100 {
            if !app.storage.lock().unwrap().worker_running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!app.storage.lock().unwrap().worker_running);
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(valid_form()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        await_installer(&app).await;
    }

    fn scoped_storage_runner(
        input: &Input,
        cancelled: &AtomicBool,
        publish: &mut dyn FnMut(usize, &'static str, String),
    ) -> Result<(), String> {
        let command = crate::util::external_command("kubectl");
        assert!(command.get_args().any(|arg| arg == "--context=destination"));
        assert!(command
            .get_envs()
            .any(|(key, path)| key == "KUBECONFIG" && path.is_some()));
        fake_storage_runner(input, cancelled, publish)
    }

    #[tokio::test]
    async fn storage_worker_uses_the_selected_private_target_snapshot() {
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        app.storage_runner = scoped_storage_runner;
        let target = synthetic_target(&app);
        let routes = router(app);
        checked_storage(&routes, &target, "nfs.example", "/export").await;
    }

    #[tokio::test]
    async fn cancellation_and_replacement_never_approve_an_old_storage_check() {
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        app.storage_runner = slow_storage_runner;
        let target = synthetic_target(&app);
        let routes = router(app.clone());
        let first = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/storage-checks",
                Some(json!({"target_id":target,"server":"nfs.example","path":"/export"})),
                true,
            ))
            .await
            .unwrap();
        let first: Value =
            serde_json::from_slice(&to_bytes(first.into_body(), 4096).await.unwrap()).unwrap();
        let old = first["check_id"].as_str().unwrap();
        let cancel = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/storage-checks/cancel",
                Some(json!({"check_id":old})),
                true,
            ))
            .await
            .unwrap();
        let cancel: Value =
            serde_json::from_slice(&to_bytes(cancel.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(cancel["cancelled"], true);
        let response = routes
            .clone()
            .oneshot(request(
                "GET",
                &format!("/api/storage-checks/{old}"),
                None,
                true,
            ))
            .await
            .unwrap();
        let response: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(response["phase"], "cancelled");
        assert_eq!(response["eligible"], false);
        let second = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/storage-checks",
                Some(json!({"target_id":target,"server":"nfs.example","path":"/different"})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/storage-checks/{old}"),
                    None,
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let id = app
            .storage
            .lock()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .id
            .clone();
        for _ in 0..120 {
            let done = app.storage.lock().unwrap().session.as_ref().unwrap().phase == "complete";
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let guard = app.storage.lock().unwrap();
        assert_eq!(guard.session.as_ref().unwrap().id, id);
        assert!(guard.session.as_ref().unwrap().eligible(
            &target,
            &Input {
                server: "nfs.example".into(),
                path: "/different".into()
            }
        ));
        assert!(!guard.session.as_ref().unwrap().eligible(
            &target,
            &Input {
                server: "nfs.example".into(),
                path: "/export".into()
            }
        ));
    }

    fn fake_check_runner(
        cancelled: Arc<AtomicBool>,
        publish: &mut dyn FnMut(usize, &'static str, &'static str),
    ) {
        for index in 0..checks::initial_checks().len() {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            publish(index, "running", "Checking…");
            publish(index, "passed", "Verified by synthetic test runner");
        }
    }

    static RUNNING_PROBES: AtomicUsize = AtomicUsize::new(0);
    static MAX_PROBES: AtomicUsize = AtomicUsize::new(0);
    static PROBE_RUNS: AtomicUsize = AtomicUsize::new(0);

    fn slow_fake_check_runner(
        cancelled: Arc<AtomicBool>,
        publish: &mut dyn FnMut(usize, &'static str, &'static str),
    ) {
        PROBE_RUNS.fetch_add(1, Ordering::SeqCst);
        let count = RUNNING_PROBES.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_PROBES.fetch_max(count, Ordering::SeqCst);
        publish(0, "running", "Checking…");
        for _ in 0..25 {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(8));
        }
        if !cancelled.load(Ordering::Acquire) {
            for index in 0..checks::initial_checks().len() {
                publish(index, "passed", "Synthetic result");
            }
        }
        RUNNING_PROBES.fetch_sub(1, Ordering::SeqCst);
    }

    #[cfg(target_os = "linux")]
    static ESCAPED_PID_PATH: OnceLock<Mutex<std::path::PathBuf>> = OnceLock::new();
    #[cfg(target_os = "linux")]
    static ESCAPE_RUNS: AtomicUsize = AtomicUsize::new(0);

    #[cfg(target_os = "linux")]
    fn escaped_fake_check_runner(
        cancelled: Arc<AtomicBool>,
        publish: &mut dyn FnMut(usize, &'static str, &'static str),
    ) {
        if ESCAPE_RUNS.fetch_add(1, Ordering::SeqCst) == 0 {
            let path = ESCAPED_PID_PATH.get().unwrap().lock().unwrap().clone();
            let script = "/usr/bin/setsid /bin/sh -c 'printf %s \"$$\" > \"$1\"; exec sleep 20' helper \"$1\" & printf 'direct child exited\\n'; exit 0";
            publish(0, "running", "Checking…");
            let _ = checks::probe(
                "/bin/sh",
                &["-c", script, "fixture", path.to_str().unwrap()],
                &cancelled,
                Instant::now() + Duration::from_millis(450),
            );
        } else {
            fake_check_runner(cancelled, publish);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn replacement_advances_when_escaped_helper_holds_old_stdout() {
        struct Escaped(std::path::PathBuf);
        impl Drop for Escaped {
            fn drop(&mut self) {
                if let Ok(pid) = std::fs::read_to_string(&self.0) {
                    if let Ok(pid) = pid.parse::<i32>() {
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                    }
                }
                let _ = std::fs::remove_file(&self.0);
            }
        }
        ESCAPE_RUNS.store(0, Ordering::SeqCst);
        let path =
            Escaped(std::env::temp_dir().join(format!("kubarr-worker-{}.pid", token().unwrap())));
        *ESCAPED_PID_PATH
            .get_or_init(|| Mutex::new(std::path::PathBuf::new()))
            .lock()
            .unwrap() = path.0.clone();
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        app.check_runner = escaped_fake_check_runner;
        let target = synthetic_target(&app);
        let routes = router(app.clone());
        let started = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/checks",
                Some(json!({"target_id":target})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::ACCEPTED);
        for _ in 0..100 {
            if path.0.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(path.0.exists());
        let start = Instant::now();
        let current = checked_target(&routes, &target).await;
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            app.checks.lock().unwrap().session.as_ref().unwrap().id,
            current
        );
        assert_eq!(ESCAPE_RUNS.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rapid_replacement_coalesces_workers_and_never_runs_checks_concurrently() {
        RUNNING_PROBES.store(0, Ordering::SeqCst);
        MAX_PROBES.store(0, Ordering::SeqCst);
        PROBE_RUNS.store(0, Ordering::SeqCst);
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        app.check_runner = slow_fake_check_runner;
        let id = synthetic_target(&app);
        let routes = router(app.clone());
        let mut ids = Vec::new();
        for number in 0..12 {
            let response = routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/checks",
                    Some(json!({"target_id":id})),
                    true,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let data: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap())
                    .unwrap();
            ids.push(data["check_id"].as_str().unwrap().to_owned());
            if number == 0 {
                for _ in 0..100 {
                    if RUNNING_PROBES.load(Ordering::SeqCst) == 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                assert_eq!(RUNNING_PROBES.load(Ordering::SeqCst), 1);
            }
        }
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/checks/{}", ids[0]),
                    None,
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        for _ in 0..200 {
            if app.checks.lock().unwrap().session.as_ref().unwrap().phase == "complete" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            app.checks.lock().unwrap().session.as_ref().unwrap().id,
            *ids.last().unwrap()
        );
        assert_eq!(
            app.checks.lock().unwrap().session.as_ref().unwrap().phase,
            "complete"
        );
        assert_eq!(PROBE_RUNS.load(Ordering::SeqCst), 2);
        assert_eq!(MAX_PROBES.load(Ordering::SeqCst), 1);
    }

    fn request(
        method: &str,
        path: &str,
        body: Option<Value>,
        authenticated: bool,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:32123")
            .header(header::ORIGIN, "http://127.0.0.1:32123");
        if authenticated {
            builder = builder
                .header(header::COOKIE, "kubarr_session=session-secret")
                .header("x-kubarr-csrf", "csrf-secret");
        }
        let body = if let Some(body) = body {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        } else {
            Body::empty()
        };
        builder.body(body).unwrap()
    }

    fn valid_form() -> Value {
        json!({"destination":{"kind":"single-node"},"server_name":"Kubarr","storage_mode":"managed-nfs",
            "storage_size":"1Ti","storage_class":"","nfs_server":"","nfs_path":"/data",
            "access_mode":"nodeport","backend_port":"30081","admin_username":"admin",
            "admin_email":"admin@example.com","admin_password":"secret-password",
            "admin_confirm":"secret-password","grafana":false})
    }

    async fn checked_target(routes: &Router, id: &str) -> String {
        let started = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/checks",
                Some(json!({"target_id":id})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::ACCEPTED);
        let reply: Value =
            serde_json::from_slice(&to_bytes(started.into_body(), 4096).await.unwrap()).unwrap();
        let check_id = reply["check_id"].as_str().unwrap().to_owned();
        for _ in 0..100 {
            let response = routes
                .clone()
                .oneshot(request(
                    "GET",
                    &format!("/api/checks/{check_id}"),
                    None,
                    true,
                ))
                .await
                .unwrap();
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap())
                    .unwrap();
            if body["phase"] == "complete" {
                assert_eq!(body["eligible"], true);
                return check_id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("synthetic check runner did not complete");
    }

    fn synthetic_target(app: &App) -> String {
        let config = "kind: Config\napiVersion: v1\nclusters:\n- name: cluster\n  cluster: {server: 'https://example.invalid'}\ncontexts:\n- name: destination\n  context: {cluster: cluster, user: account}\nusers:\n- name: account\n  user: {token: 'never-expose-this-token'}\ncurrent-context: destination\n";
        app.targets.lock().unwrap().upload(config).unwrap()[0]
            .id
            .clone()
    }

    #[tokio::test]
    async fn check_sessions_authorize_replace_cancel_and_gate_same_recent_target() {
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let id = synthetic_target(&app);
        let routes = router(app.clone());
        let body = Some(json!({"target_id":id}));
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/checks", body.clone(), false))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let mut missing_csrf = request("POST", "/api/checks", body.clone(), true);
        missing_csrf.headers_mut().remove("x-kubarr-csrf");
        assert_eq!(
            routes.clone().oneshot(missing_csrf).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request("GET", "/api/checks/missing", None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/checks",
                    Some(json!({"target_id":"unknown"})),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let first = checked_target(&routes, &id).await;
        let second = checked_target(&routes, &id).await;
        assert_ne!(first, second);
        assert_eq!(
            routes
                .clone()
                .oneshot(request("GET", &format!("/api/checks/{first}"), None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let old_cancel = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/checks/cancel",
                Some(json!({"check_id":first})),
                true,
            ))
            .await
            .unwrap();
        let old: Value =
            serde_json::from_slice(&to_bytes(old_cancel.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(old["cancelled"], false);
        assert_eq!(
            app.checks.lock().unwrap().session.as_ref().unwrap().id,
            second
        );
        let mut form = valid_form();
        form["destination"] = json!({"kind":"context","target_id":id});
        {
            let mut guard = app.checks.lock().unwrap();
            let session = guard.session.as_mut().unwrap();
            session.completed = Some(Instant::now() - CHECK_VALIDITY - Duration::from_secs(1));
        }
        let expired = routes
            .clone()
            .oneshot(request("GET", &format!("/api/checks/{second}"), None, true))
            .await
            .unwrap();
        let expired: Value =
            serde_json::from_slice(&to_bytes(expired.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(expired["eligible"], false);
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        {
            let mut guard = app.checks.lock().unwrap();
            let session = guard.session.as_mut().unwrap();
            session.completed = Some(Instant::now());
            session.checks[0].state = "failed";
        }
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let third = checked_target(&routes, &id).await;
        {
            let mut guard = app.checks.lock().unwrap();
            let session = guard.session.as_mut().unwrap();
            session.phase = "running";
            session.checks[0].state = "pending";
        }
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let cancelled = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/checks/cancel",
                Some(json!({"check_id":third})),
                true,
            ))
            .await
            .unwrap();
        let result: Value =
            serde_json::from_slice(&to_bytes(cancelled.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(result["cancelled"], true);
        let state = routes
            .clone()
            .oneshot(request("GET", &format!("/api/checks/{third}"), None, true))
            .await
            .unwrap();
        let result: Value =
            serde_json::from_slice(&to_bytes(state.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(result["phase"], "cancelled");
        assert_eq!(result["eligible"], false);
        assert!(!result.to_string().contains("never-expose-this-token"));
        assert_eq!(
            routes
                .oneshot(request("POST", "/api/start", Some(form), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn ssh_takes_precedence_and_hosts_are_port_specific() {
        assert_eq!(environment(true, true), Environment::Ssh);
        assert_eq!(environment(false, true), Environment::Desktop);
        assert_eq!(environment(false, false), Environment::Headless);
        assert_eq!(candidate_hosts(Environment::Headless), vec!["127.0.0.1"]);
        assert_eq!(authority("2001:db8::1", 3456), "[2001:db8::1]:3456");
    }

    #[test]
    fn form_validates_each_install_choice_without_executing() {
        let mut form = valid_form();
        form["destination"] = json!({"kind":"single-node"});
        form["storage_mode"] = json!("external-nfs");
        form["nfs_server"] = json!("192.0.2.5");
        form["nfs_path"] = json!("/export");
        form["access_mode"] = json!("port-forward");
        form["grafana"] = json!(true);
        let options = serde_json::from_value::<Form>(form.clone())
            .unwrap()
            .options()
            .unwrap();
        assert!(matches!(options.cluster_mode, ClusterMode::SingleNode));
        assert!(matches!(
            options.storage.mode,
            StorageModeOption::ExternalNfs(_)
        ));
        assert_eq!(options.install.web_access, WebAccess::PortForward);
        assert!(options.grafana_enabled);
        form["admin_confirm"] = json!("mismatch");
        assert!(serde_json::from_value::<Form>(form.clone())
            .unwrap()
            .options()
            .is_err());
        form["admin_confirm"] = json!("secret-password");
        form["access_mode"] = json!("nodeport");
        form["backend_port"] = json!("1234");
        assert!(serde_json::from_value::<Form>(form)
            .unwrap()
            .options()
            .is_err());
        let mut form = valid_form();
        form["backend_port"] = json!("30080");
        let nodeport = serde_json::from_value::<Form>(form.clone())
            .unwrap()
            .options()
            .unwrap();
        assert_eq!(nodeport.install.web_access, WebAccess::NodePort(30080));
        form["access_mode"] = json!("loadbalancer");
        form["backend_port"] = json!("ignored");
        assert_eq!(
            serde_json::from_value::<Form>(form.clone())
                .unwrap()
                .options()
                .unwrap()
                .install
                .web_access,
            WebAccess::LoadBalancer
        );
        form["access_mode"] = json!("invalid");
        assert!(serde_json::from_value::<Form>(form)
            .unwrap()
            .options()
            .is_err());
    }

    #[tokio::test]
    async fn invitation_session_csrf_origin_and_host_are_enforced() {
        let app = fixture();
        let router = router(app.clone());
        assert_eq!(
            router
                .clone()
                .oneshot(request("GET", "/api/status", None, false))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let invalid = request("POST", "/api/redeem", Some(json!({"token":"wrong"})), false);
        assert_eq!(
            router.clone().oneshot(invalid).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let mut bad_host = request(
            "POST",
            "/api/redeem",
            Some(json!({"token":"invite-secret"})),
            false,
        );
        bad_host.headers_mut().insert(
            header::HOST,
            HeaderValue::from_static("attacker.example:32123"),
        );
        assert_eq!(
            router.clone().oneshot(bad_host).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let redeemed = router
            .clone()
            .oneshot(request(
                "POST",
                "/api/redeem",
                Some(json!({"token":"invite-secret"})),
                false,
            ))
            .await
            .unwrap();
        assert_eq!(redeemed.status(), StatusCode::OK);
        assert!(redeemed
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("HttpOnly; SameSite=Strict"));
        assert_eq!(
            router
                .clone()
                .oneshot(request(
                    "POST",
                    "/api/redeem",
                    Some(json!({"token":"invite-secret"})),
                    false
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            router
                .clone()
                .oneshot(request("GET", "/api/status", None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let mut bad_origin = request("POST", "/api/start", Some(valid_form()), true);
        bad_origin.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("http://attacker.example"),
        );
        assert_eq!(
            router.clone().oneshot(bad_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let mut bad_csrf = request("POST", "/api/start", Some(valid_form()), true);
        bad_csrf.headers_mut().remove("x-kubarr-csrf");
        assert_eq!(
            router.clone().oneshot(bad_csrf).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(app.shared.lock().unwrap().phase, Phase::Ready);
    }

    #[tokio::test]
    async fn validation_and_request_bounds_precede_install() {
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let router = router(app.clone());
        let mut form = valid_form();
        form["admin_confirm"] = json!("wrong");
        let bad = router
            .clone()
            .oneshot(request("POST", "/api/start", Some(form), true))
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        let data = to_bytes(bad.into_body(), 1024).await.unwrap();
        assert!(!String::from_utf8_lossy(&data).contains("secret-password"));
        let oversized = request(
            "POST",
            "/api/start",
            Some(json!({"padding":"x".repeat(20_000)})),
            true,
        );
        assert_eq!(
            router.oneshot(oversized).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(app.shared.lock().unwrap().phase, Phase::Ready);
    }

    #[tokio::test]
    async fn target_routes_require_session_and_csrf_and_reject_unknown_destinations() {
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let routes = router(app.clone());
        assert_eq!(
            routes
                .clone()
                .oneshot(request("GET", "/api/targets", None, false))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let listed = routes
            .clone()
            .oneshot(request("GET", "/api/targets", None, true))
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let listed: Value =
            serde_json::from_slice(&to_bytes(listed.into_body(), 4096).await.unwrap()).unwrap();
        assert!(listed["targets"].is_array());
        let mut bad_csrf = request(
            "POST",
            "/api/targets/upload",
            Some(json!({"kubeconfig":"secret"})),
            true,
        );
        bad_csrf.headers_mut().remove("x-kubarr-csrf");
        assert_eq!(
            routes.clone().oneshot(bad_csrf).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let bad = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/targets/upload",
                Some(json!({"kubeconfig":"exec: {command: sh}"})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        assert!(
            !String::from_utf8_lossy(&to_bytes(bad.into_body(), 1024).await.unwrap())
                .contains("exec:")
        );
        assert_eq!(
            routes
                .clone()
                .oneshot(request(
                    "GET",
                    "/api/preflight?target_id=missing",
                    None,
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let preflight = routes
            .clone()
            .oneshot(request("GET", "/api/preflight", None, true))
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(preflight.into_body(), 4096).await.unwrap()).unwrap();
        assert!(body["context"].is_null());
        assert!(body["storage_class"].is_null());
        let mut form = valid_form();
        form["destination"] = json!({"kind":"context","target_id":"missing"});
        assert_eq!(
            routes
                .oneshot(request("POST", "/api/start", Some(form), true))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(app.shared.lock().unwrap().phase, Phase::Ready);
    }

    fn fake_targeted_install(options: &BootstrapOptions) -> Result<(), String> {
        if !crate::util::has_target() || !matches!(options.cluster_mode, ClusterMode::Existing) {
            return Err("target was not scoped to installer".into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn upload_returns_only_display_choices_and_selected_id_scopes_installation() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let mut app = fixture();
        app.installer = fake_targeted_install;
        app.shared.lock().unwrap().invitation = None;
        let routes = router(app.clone());
        let config = "kind: Config\napiVersion: v1\nclusters:\n- name: cluster\n  cluster: {server: 'https://example.invalid'}\ncontexts:\n- name: destination\n  context: {cluster: cluster, user: account}\nusers:\n- name: account\n  user: {token: 'never-expose-this-token'}\ncurrent-context: destination\n";
        let response = routes
            .clone()
            .oneshot(request(
                "POST",
                "/api/targets/upload",
                Some(json!({"kubeconfig":config})),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let raw = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("never-expose-this-token"));
        let body: Value = serde_json::from_slice(&raw).unwrap();
        let id = body["targets"][0]["id"].as_str().unwrap();
        assert_eq!(body["targets"][0]["context"], "destination");
        assert_eq!(body["targets"][0]["current"], true);
        let mut form = valid_form();
        form["destination"] = json!({"kind":"context","target_id":id});
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form.clone()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        checked_target(&routes, id).await;
        assert_eq!(
            routes
                .clone()
                .oneshot(request("POST", "/api/start", Some(form), true))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        for _ in 0..100 {
            if app.shared.lock().unwrap().phase == Phase::Succeeded {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(app.shared.lock().unwrap().phase, Phase::Succeeded);
        assert_eq!(
            routes
                .oneshot(request(
                    "POST",
                    "/api/targets/upload",
                    Some(json!({"kubeconfig": config})),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn http_starts_at_most_once_and_exposes_safe_terminal_status() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let router = router(app.clone());
        let started = router
            .clone()
            .oneshot(request("POST", "/api/start", Some(valid_form()), true))
            .await
            .unwrap();
        assert_eq!(started.status(), StatusCode::ACCEPTED);
        let duplicate = router
            .clone()
            .oneshot(request("POST", "/api/start", Some(valid_form()), true))
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        for _ in 0..100 {
            if app.shared.lock().unwrap().phase == Phase::Succeeded {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let result = router
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        let body = to_bytes(result.into_body(), 8192).await.unwrap();
        let snapshot: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(snapshot["phase"], "succeeded");
        assert!(!String::from_utf8_lossy(&body).contains("secret-password"));
        assert!(!String::from_utf8_lossy(&body).contains("must-never-appear"));
    }

    #[tokio::test]
    async fn http_survives_injected_install_failure() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let mut app = fixture();
        app.installer = fake_failure;
        app.shared.lock().unwrap().invitation = None;
        let router = router(app.clone());
        assert_eq!(
            router
                .clone()
                .oneshot(request("POST", "/api/start", Some(valid_form()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        for _ in 0..100 {
            if app.shared.lock().unwrap().phase == Phase::Failed {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let response = router
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(body["phase"], "failed");
        assert!(body["result"]
            .as_str()
            .unwrap()
            .contains("database bootstrap SQL failed"));
        assert!(!body.to_string().contains("secret-password"));
    }

    fn fake_success(_: &BootstrapOptions) -> Result<(), String> {
        install_events::emit(">> Storage\n   installing managed NFS");
        install_events::emit("  - admin password: must-never-appear");
        install_events::emit("     must-never-appear");
        Ok(())
    }
    fn fake_failure(_: &BootstrapOptions) -> Result<(), String> {
        install_events::emit(">> Database\n   seeding admin");
        Err("database bootstrap SQL failed (exit status 1)".into())
    }

    #[tokio::test]
    async fn injected_install_has_bounded_safe_progress_and_terminal_result() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        for (installer, expected) in [
            (
                fake_success as fn(&BootstrapOptions) -> Result<(), String>,
                Phase::Succeeded,
            ),
            (fake_failure, Phase::Failed),
        ] {
            let app = fixture();
            app.shared.lock().unwrap().invitation = None;
            let options = serde_json::from_value::<Form>(valid_form())
                .unwrap()
                .options()
                .unwrap();
            app.shared.lock().unwrap().phase = Phase::Running;
            let run = app.clone();
            thread::spawn(move || execute(run, options, installer, false))
                .join()
                .unwrap();
            let guard = app.shared.lock().unwrap();
            assert_eq!(guard.phase, expected);
            assert!(guard.events.iter().any(|line| line.contains(">>")));
            assert!(!guard.events.join(" ").contains("must-never-appear"));
            assert!(!guard.result.as_deref().unwrap().contains("secret-password"));
        }
    }

    fn fake_many_events(_: &BootstrapOptions) -> Result<(), String> {
        for _ in 0..(MAX_EVENTS + 50) {
            install_events::emit(">> Storage\n   hidden details");
        }
        Ok(())
    }

    #[test]
    fn event_history_keeps_only_the_latest_safe_messages() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let app = fixture();
        let options = serde_json::from_value::<Form>(valid_form())
            .unwrap()
            .options()
            .unwrap();
        execute(app.clone(), options, fake_many_events, false);
        let guard = app.shared.lock().unwrap();
        assert_eq!(guard.events.len(), MAX_EVENTS);
        assert_eq!(guard.events.first().unwrap(), ">> Storage");
        assert_eq!(guard.events.last().unwrap(), ">> Storage");
        assert!(!guard.events.join(" ").contains("hidden details"));
        assert_eq!(guard.log_lines.len(), MAX_EVENTS + 50);
    }

    #[tokio::test]
    async fn install_log_and_access_require_cookie_and_allowed_host() {
        let app = fixture();
        let routes = router(app.clone());
        for path in ["/api/access", "/api/install-log"] {
            assert_eq!(
                routes
                    .clone()
                    .oneshot(request("GET", path, None, false))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
            let mut wrong_host = request("GET", path, None, true);
            wrong_host
                .headers_mut()
                .insert(header::HOST, HeaderValue::from_static("attacker.test"));
            assert_eq!(
                routes.clone().oneshot(wrong_host).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        app.shared.lock().unwrap().invitation = None;
        assert_eq!(
            routes
                .clone()
                .oneshot(request("GET", "/api/access", None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let response = routes
            .oneshot(request("GET", "/api/install-log", None, true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"kubarr-install.log\""
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn curated_log_retains_older_than_status_and_excludes_secrets_on_failure() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let options = serde_json::from_value::<Form>(valid_form())
            .unwrap()
            .options()
            .unwrap();
        fn fail(_: &BootstrapOptions) -> Result<(), String> {
            install_events::emit(">> Tools\n   TOP_SECRET_password");
            install_events::emit("[RUN] kubectl --token=TOP_SECRET_password");
            install_events::emit("TOP_SECRET_password");
            for _ in 0..MAX_EVENTS + 5 {
                install_events::emit(">> Storage\n   TOP_SECRET_password");
            }
            Err("TOP_SECRET_password".into())
        }
        execute(app.clone(), options, fail, true);
        let routes = router(app);
        let status = routes
            .clone()
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        let body = to_bytes(status.into_body(), 1024 * 1024).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["access"], Value::Null);
        assert_eq!(json["events"].as_array().unwrap().len(), MAX_EVENTS);
        assert!(!String::from_utf8_lossy(&body).contains("TOP_SECRET"));
        let response = routes
            .oneshot(request("GET", "/api/install-log", None, true))
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), MAX_INSTALL_LOG_BYTES + 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("Phase: failed"));
        assert!(text.contains(">> Tools\n"));
        assert!(text.contains("Running installation command…"));
        assert!(!text.contains("TOP_SECRET"));
    }

    #[test]
    fn curated_history_caps_on_whole_utf8_lines_and_marks_truncation() {
        let initial = fixture();
        let mut shared = initial.shared.lock().unwrap().download();
        assert!(shared.contains("Phase: ready"));
        let app = fixture();
        {
            let mut state = app.shared.lock().unwrap();
            for _ in 0..2500 {
                state.record("é".repeat(510));
            }
            assert!(state.log_truncated);
            assert_eq!(state.events.len(), MAX_EVENTS);
            shared = state.download();
        }
        assert!(shared.contains(LOG_TRUNCATED));
        assert!(shared.len() <= MAX_INSTALL_LOG_BYTES + 512);
        assert!(shared.is_char_boundary(shared.len()));
    }

    #[tokio::test]
    async fn access_refresh_uses_frozen_target_only_after_success_without_mutating_events() {
        let mut app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let target = CommandTarget {
            kubeconfig: "/private/selected".into(),
            context: "selected".into(),
            _snapshot: None,
        };
        fn scoped_reader() -> Access {
            assert_eq!(current_target().unwrap().context, "selected");
            assert_eq!(
                current_target().unwrap().kubeconfig,
                std::path::PathBuf::from("/private/selected")
            );
            fake_access_reader()
        }
        app.access_reader = scoped_reader;
        {
            let mut guard = app.shared.lock().unwrap();
            guard.access = Some(Access {
                kind: AccessKind::Loadbalancer,
                url: None,
                instructions: vec!["Pending".into()],
                pending: true,
            });
            guard.installed_target = Some(target);
            guard.events = vec!["event".into()];
        }
        let routes = router(app.clone());
        assert_eq!(
            routes
                .clone()
                .oneshot(request("GET", "/api/access", None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        app.shared.lock().unwrap().phase = Phase::Succeeded;
        let response = routes
            .clone()
            .oneshot(request("GET", "/api/access", None, true))
            .await
            .unwrap();
        let json: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(json["kind"], "loadbalancer");
        assert_eq!(json["url"], "http://[2001:db8::2]:8080");
        assert_eq!(json["pending"], false);
        assert_eq!(app.shared.lock().unwrap().events, ["event"]);
        let status = routes
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        let json: Value =
            serde_json::from_slice(&to_bytes(status.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(json["access"]["url"], "http://[2001:db8::2]:8080");
        assert!(!json.to_string().contains("/private/selected"));
    }

    #[tokio::test]
    async fn installer_publishes_structured_access_only_on_success() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        fn with_access(_: &BootstrapOptions) -> Result<(), String> {
            install_events::access(install_access::resolve_access(WebAccess::PortForward));
            Ok(())
        }
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let options = serde_json::from_value::<Form>(valid_form())
            .unwrap()
            .options()
            .unwrap();
        execute(app.clone(), options, with_access, false);
        let routes = router(app);
        let status = routes
            .clone()
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        let json: Value =
            serde_json::from_slice(&to_bytes(status.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(json["access"]["kind"], "port-forward");
        assert_eq!(json["access"]["url"], "http://127.0.0.1:8080");
        let response = routes
            .oneshot(request("GET", "/api/access", None, true))
            .await
            .unwrap();
        let direct: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(direct, json["access"]);
        assert!(direct.get("access").is_none());
    }

    #[test]
    fn installer_keeps_new_local_target_after_worker_scope_ends() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        fn local_install(_: &BootstrapOptions) -> Result<(), String> {
            crate::util::set_target(CommandTarget {
                kubeconfig: "/private/new-local".into(),
                context: "new-local".into(),
                _snapshot: None,
            });
            install_events::access(install_access::resolve_access(WebAccess::PortForward));
            Ok(())
        }
        let app = fixture();
        let options = serde_json::from_value::<Form>(valid_form())
            .unwrap()
            .options()
            .unwrap();
        {
            let _scope = scope_target(None);
            execute(app.clone(), options, local_install, false);
        }
        let guard = app.shared.lock().unwrap();
        assert_eq!(
            guard.installed_target.as_ref().unwrap().context,
            "new-local"
        );
        assert_eq!(guard.access.as_ref().unwrap().kind, AccessKind::PortForward);
    }

    #[tokio::test]
    async fn one_shutdown_request_waits_for_install_and_completes_without_second_signal() {
        let app = fixture();
        {
            let mut guard = app.shared.lock().unwrap();
            guard.phase = Phase::Running;
            guard.invitation = None;
        }
        let (signal, receiver) = tokio::sync::oneshot::channel();
        let waiting = app.clone();
        let shutdown = tokio::spawn(async move {
            graceful_shutdown(waiting, async { receiver.await.unwrap() }).await
        });
        signal.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !shutdown.is_finished(),
            "shutdown must remain pending while running"
        );
        assert_eq!(
            router(app.clone())
                .oneshot(request("GET", "/api/status", None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        app.shared.lock().unwrap().phase = Phase::Failed;
        app.completion.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn opener_reaps_immediate_failure_and_reports_fallback() {
        let (tx, rx) = mpsc::channel();
        launch_browser("/bin/false", "http://127.0.0.1:1234/#test", move |err| {
            tx.send(err).unwrap();
        })
        .unwrap();
        assert!(rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .contains("browser opener failed"));
        assert!(launch_browser("/no/such/opener", "http://127.0.0.1:1234/", |_| {}).is_err());
    }

    #[test]
    fn safe_preflight_failures_are_visible_without_reflecting_arbitrary_values() {
        assert_eq!(
            safe_event("[FAIL] kubectl cannot reach the Kubernetes API").as_deref(),
            Some("[FAIL] kubectl cannot reach the Kubernetes API")
        );
        assert_eq!(
            safe_event("[FAIL] Services API is not available").as_deref(),
            Some("[FAIL] Services API is not available")
        );
        assert_eq!(
            safe_event("[FAIL] cannot create namespaces").as_deref(),
            Some("[FAIL] cannot create namespaces")
        );
        assert!(safe_event("[FAIL] secret-password").is_none());
        assert!(safe_event("[WARN] arbitrary user-provided data").is_none());
        assert_eq!(
            safe_event(">> Storage\n   private value").as_deref(),
            Some(">> Storage")
        );
        assert!(safe_event(">> SECRET_token\n   private value").is_none());
        assert!(safe_event(">> Step 3\n   private value").is_none());
        assert!(safe_event("open: http://SECRET_token:30081").is_none());
        assert!(safe_event("open: http://192.0.2.10:30081/private").is_none());
        assert!(safe_event("forward: kubectl port-forward -n kubarr-backend svc/kubarr-backend 8000:8000 SECRET_token").is_none());
        for message in [
            GATEWAY_FORWARD,
            LB_PENDING,
            LB_ERROR,
            "open: http://gateway.example.org:8080",
            "open: http://[2001:db8::a]:8080",
        ] {
            assert_eq!(safe_event(message).as_deref(), Some(message));
        }
        for message in [
            "open: http://bad/SECRET:8080",
            "open: http://evil.example:8080/SECRET",
            "open: http://bad\nSECRET:8080",
            "pending: SECRET",
        ] {
            assert!(safe_event(message).is_none());
        }
        for address in [
            "open: http://192.0.2.10:30081",
            "open: http://[2001:db8::a]:30081",
            "open: http://<node-ip>:30081",
            "forward: kubectl port-forward -n kubarr-backend svc/kubarr-backend 8000:8000",
        ] {
            assert_eq!(safe_event(address).as_deref(), Some(address));
        }
    }

    fn fake_command_failure(_: &BootstrapOptions) -> Result<(), String> {
        crate::util::try_run_or_print(
            "sh",
            &["-c", "printf 'sensitive-private-output' >&2; exit 7"],
            false,
            false,
        )
    }

    fn fake_sensitive_progress(_: &BootstrapOptions) -> Result<(), String> {
        for message in [
            ">> SECRET_node_token\n   disguised step",
            "open: http://SECRET_node_token:30081",
            "forward: kubectl port-forward -n kubarr-backend svc/kubarr-backend 8000:8000 SECRET_node_token",
            "[WARN] SECRET_node_token",
            ">> Storage\n   SECRET_node_token",
            "open: http://[2001:db8::a]:30081",
        ] {
            install_events::emit(message);
        }
        Ok(())
    }

    fn fake_typed_progress(_: &BootstrapOptions) -> Result<(), String> {
        crate::style::step("Fluent Bit", "SECRET_admin_password");
        crate::style::detail("chart", "SECRET_chart_path");
        crate::util::try_run_or_print_progress(
            "sh",
            &[
                "-c",
                "printf SECRET_tool_output >&2; exit 7",
                "--set",
                "password=SECRET_arg",
            ],
            false,
            false,
            Some(install_events::Operation::FluentBit),
        )
    }

    #[test]
    fn arbitrary_error_text_cannot_escape_via_status_result() {
        for error in [
            "logging chart missing at SECRET_chart_path",
            "failed to run helm: --set password=SECRET_password",
            "database bootstrap SQL failed (exit status 7) SECRET_sql",
        ] {
            let result = safe_failure_result(error);
            assert!(!result.contains("SECRET"));
            assert!(!result.contains("--set"));
        }
        assert_eq!(
            safe_failure_result("database bootstrap SQL failed (exit status 7)"),
            "database bootstrap SQL failed (exit status 7)"
        );
        for known in [
            "Helm 4 is required (target: 4.3.0); check `helm version` locally",
            "required tool 'kubectl' was not found in PATH",
        ] {
            assert_eq!(safe_failure_result(known), known);
            assert_ne!(safe_failure_result(&format!("{known} SECRET_token")), known);
        }
    }

    fn fake_unsupported_helm(_: &BootstrapOptions) -> Result<(), String> {
        Err("Helm 4 is required (target: 4.3.0); check `helm version` locally".into())
    }

    #[tokio::test]
    async fn status_reports_fixed_tool_error_even_without_any_progress() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let options = serde_json::from_value::<Form>(valid_form())
            .unwrap()
            .options()
            .unwrap();
        execute(app.clone(), options, fake_unsupported_helm, true);
        let response = router(app)
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["events"], json!([]));
        assert_eq!(
            body["result"],
            "Helm 4 is required (target: 4.3.0); check `helm version` locally"
        );
    }

    #[tokio::test]
    async fn typed_progress_is_detailed_but_http_never_receives_commands_or_secrets() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        for uploaded in [false, true] {
            let mut app = fixture();
            let (path, log) = install_events::create_diagnostic_log().unwrap();
            app.diagnostics = Some(log);
            app.shared.lock().unwrap().invitation = None;
            let options = serde_json::from_value::<Form>(valid_form())
                .unwrap()
                .options()
                .unwrap();
            execute(app.clone(), options, fake_typed_progress, uploaded);
            let response = router(app)
                .oneshot(request("GET", "/api/status", None, true))
                .await
                .unwrap();
            let body = String::from_utf8(
                to_bytes(response.into_body(), 65536)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert!(body.contains("[STEP] Fluent Bit"));
            assert!(body.contains("[RUN] Fluent Bit: install/upgrade Helm release fluent-bit (release namespace default; workload namespace fluent-bit)"));
            assert!(body.contains("[FAIL] Fluent Bit:"));
            assert!(body.contains("exit 7"));
            for secret in [
                "SECRET_admin_password",
                "SECRET_chart_path",
                "SECRET_tool_output",
                "SECRET_arg",
                "--set",
                "password=",
            ] {
                assert!(!body.contains(secret), "browser leaked {secret}");
            }
            let diagnostic = std::fs::read_to_string(&path).unwrap();
            assert_eq!(diagnostic.contains("SECRET_tool_output"), !uploaded);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn http_status_filters_sensitive_node_output_and_unknown_progress() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let app = fixture();
        app.shared.lock().unwrap().invitation = None;
        let options = serde_json::from_value::<Form>(valid_form())
            .unwrap()
            .options()
            .unwrap();
        execute(app.clone(), options, fake_sensitive_progress, false);
        let response = router(app)
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        let body = String::from_utf8(
            to_bytes(response.into_body(), 16384)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(!body.contains("SECRET_node_token"));
        assert!(body.contains("open: http://[2001:db8::a]:30081"));
        assert!(body.contains(">> Storage"));
    }

    #[tokio::test]
    async fn subprocess_failure_is_diagnosable_locally_without_exposing_output_over_http() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let (path, log) = install_events::create_diagnostic_log().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut app = fixture();
        app.installer = fake_command_failure;
        app.diagnostics = Some(log);
        app.shared.lock().unwrap().invitation = None;
        let router = router(app.clone());
        assert_eq!(
            router
                .clone()
                .oneshot(request("POST", "/api/start", Some(valid_form()), true))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        for _ in 0..100 {
            if app.shared.lock().unwrap().phase == Phase::Failed {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let response = router
            .oneshot(request("GET", "/api/status", None, true))
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("sensitive-private-output"));
        assert!(String::from_utf8_lossy(&body).contains("private diagnostic log"));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("sensitive-private-output"));
        std::fs::remove_file(path).unwrap();
    }
}
