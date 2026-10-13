use std::cell::RefCell;
use std::env;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
#[cfg(not(unix))]
use std::thread;
use std::time::{Duration, Instant};

const MAX_CAPTURE_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug)]
pub struct CommandTarget {
    pub kubeconfig: PathBuf,
    pub context: String,
    // Keep an ephemeral k3s snapshot alive for as long as any scoped command
    // or install thread can still refer to it. Registry targets own their files.
    pub _snapshot: Option<Arc<crate::targets::PrivateK3sSnapshot>>,
}

thread_local! { static TARGET: RefCell<Option<CommandTarget>> = const { RefCell::new(None) }; }

pub struct TargetScope(Option<CommandTarget>);

pub fn scope_target(target: Option<CommandTarget>) -> TargetScope {
    TargetScope(TARGET.with(|slot| slot.replace(target)))
}

impl Drop for TargetScope {
    fn drop(&mut self) {
        TARGET.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

pub fn set_target(target: CommandTarget) {
    TARGET.with(|slot| *slot.borrow_mut() = Some(target));
}

pub fn current_target() -> Option<CommandTarget> {
    TARGET.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
pub fn has_target() -> bool {
    TARGET.with(|slot| slot.borrow().is_some())
}

use crate::install_events;
use crate::install_events::{Operation, ProgressKind};
use crate::style::{status_label, BLUE, CYAN, RED};

pub struct PermissionRequirement {
    pub verb: &'static str,
    pub resource: &'static str,
    pub namespace: Option<&'static str>,
}

pub fn has_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

pub fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}

pub fn parse_port(value: String) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("invalid port '{value}'"))
}

pub fn ensure_tool(tool: &str) {
    if !command_exists(tool) {
        eprintln!(
            "{} required tool '{tool}' was not found in PATH",
            status_label("error", RED)
        );
        std::process::exit(1);
    }
    if tool == "helm" {
        let version = command_output("helm", &["version", "--template", "{{.Version}}"]);
        if version.as_deref().and_then(helm_major_version) != Some(4) {
            eprintln!(
                "{} Helm 4 is required (target: 4.3.0); detected {}",
                status_label("error", RED),
                version.as_deref().unwrap_or("an unreadable Helm version")
            );
            std::process::exit(1);
        }
    }
}

pub fn check_tool(tool: &str) -> Result<(), String> {
    if !command_exists(tool) {
        return Err(format!("required tool '{tool}' was not found in PATH"));
    }
    if tool == "helm" {
        let version = if install_events::is_active() {
            #[cfg(unix)]
            {
                let mut command = external_command("helm");
                command.args(["version", "--template", "{{.Version}}"]);
                bounded_probe_output(command, Instant::now() + Duration::from_secs(4), None)
            }
            #[cfg(not(unix))]
            {
                command_output("helm", &["version", "--template", "{{.Version}}"])
            }
        } else {
            command_output("helm", &["version", "--template", "{{.Version}}"])
        };
        if version.as_deref().and_then(helm_major_version) != Some(4) {
            return Err("Helm 4 is required (target: 4.3.0); check `helm version` locally".into());
        }
    }
    Ok(())
}

fn helm_major_version(version: &str) -> Option<u64> {
    let release = version.trim().strip_prefix('v')?.split(['-', '+']).next()?;
    let mut components = release.split('.');
    let major = components.next()?.parse().ok()?;
    components.next()?.parse::<u64>().ok()?;
    components.next()?.parse::<u64>().ok()?;
    components.next().is_none().then_some(major)
}

pub fn helm_version_is_4(version: &str) -> bool {
    helm_major_version(version) == Some(4)
}

pub fn command_exists(tool: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| dir.join(tool).is_file())
}

/// In web mode, keep installer subprocesses out of the terminal's foreground
/// process group. Flag-driven CLI commands retain ordinary Ctrl-C behavior.
pub fn external_command(program: &str) -> Command {
    let mut command = Command::new(program);
    TARGET.with(|slot| {
        if let Some(target) = slot.borrow().as_ref() {
            let tool = Path::new(program)
                .file_name()
                .and_then(|name| name.to_str());
            if tool == Some("kubectl") || tool == Some("helm") {
                command.env("KUBECONFIG", &target.kubeconfig);
                // Explicit context wins over a pre-existing current-context or Helm override.
                if tool == Some("kubectl") {
                    command.arg(format!("--context={}", target.context));
                } else {
                    command.env("HELM_KUBECONFIG", &target.kubeconfig);
                    command.env("HELM_KUBECONTEXT", &target.context);
                    command.arg("--kubeconfig").arg(&target.kubeconfig);
                    command.arg("--kube-context").arg(&target.context);
                    for key in [
                        "HELM_KUBEAPISERVER",
                        "HELM_KUBECAFILE",
                        "HELM_KUBEASUSER",
                        "HELM_KUBEASGROUPS",
                        "HELM_KUBETOKEN",
                        "HELM_KUBEINSECURE_SKIP_TLS_VERIFY",
                        "HELM_KUBETLS_SERVER_NAME",
                    ] {
                        command.env_remove(key);
                    }
                }
            }
        }
    });
    #[cfg(unix)]
    if install_events::is_active() {
        command.process_group(0);
    }
    command
}

#[cfg(test)]
fn drain_bounded(mut stream: impl Read) -> Vec<u8> {
    let mut retained = Vec::new();
    let mut buf = [0u8; 4096];
    let mut truncated = false;
    while let Ok(size) = stream.read(&mut buf) {
        if size == 0 {
            break;
        }
        let available = MAX_CAPTURE_BYTES.saturating_sub(retained.len());
        retained.extend_from_slice(&buf[..size.min(available)]);
        truncated |= size > available;
    }
    if truncated {
        const NOTE: &[u8] = b"\n[output truncated]\n";
        retained.truncate(MAX_CAPTURE_BYTES - NOTE.len());
        retained.extend_from_slice(NOTE);
    }
    retained
}

#[cfg(unix)]
fn set_nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn drain_available(stream: &mut impl Read, retained: &mut Vec<u8>, truncated: &mut bool) -> bool {
    let mut buf = [0u8; 4096];
    // Yield back to the supervisor even when a child writes continuously.
    for _ in 0..16 {
        match stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(size) => {
                let available = MAX_CAPTURE_BYTES.saturating_sub(retained.len());
                retained.extend_from_slice(&buf[..size.min(available)]);
                *truncated |= size > available;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
    false
}

#[cfg(unix)]
fn finish_capture(retained: &mut Vec<u8>, truncated: bool) {
    if truncated {
        const NOTE: &[u8] = b"\n[output truncated]\n";
        retained.truncate(MAX_CAPTURE_BYTES - NOTE.len());
        retained.extend_from_slice(NOTE);
    }
}

/// The parent exit, not pipe EOF, terminates supervision. A detached helper
/// can inherit stdout/stderr; we close our pipe ends after a short final drain.
#[cfg(unix)]
fn supervise_web_child(
    child: &mut Child,
    label: &str,
    input: Option<&[u8]>,
    operation: Option<Operation>,
    began: Instant,
    interval: Duration,
    deadline: Option<Instant>,
) -> (
    Result<std::process::ExitStatus, String>,
    Vec<u8>,
    Vec<u8>,
    Result<(), String>,
) {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut stdin = child.stdin.take();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    let mut written = 0;
    let mut write_result = Ok(());
    let mut setup_error = None;
    for fd in [
        stdout.as_ref().map(AsRawFd::as_raw_fd),
        stderr.as_ref().map(AsRawFd::as_raw_fd),
        stdin.as_ref().map(AsRawFd::as_raw_fd),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(err) = set_nonblocking(fd) {
            setup_error = Some(err);
            break;
        }
    }
    if let Some(err) = setup_error {
        // A child whose pipes could not be supervised must be reaped; do not
        // leave an unsupervised install process behind.
        let _ = child.kill();
        let _ = child.wait();
        return (
            Err(format!("cannot supervise {label}: {err}")),
            output,
            errors,
            write_result,
        );
    }
    if input.is_none_or(|bytes| bytes.is_empty()) {
        stdin = None;
    }
    let mut next_heartbeat = interval;
    let mut exited = None;
    let mut after_exit = None;
    loop {
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exited = Some(Ok(status));
                    after_exit = Some(Instant::now());
                }
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    exited = Some(Err(format!("failed to wait for {label}: {err}")));
                    after_exit = Some(Instant::now());
                }
                Ok(None) => {}
            }
        }
        if exited.is_none() && deadline.is_some_and(|at| Instant::now() >= at) {
            let _ = child.kill();
            let _ = child.wait();
            exited = Some(Err(format!("{label} timed out")));
            after_exit = Some(Instant::now());
        }
        if let Some(ref mut pipe) = stdout {
            if drain_available(pipe, &mut output, &mut stdout_truncated) {
                stdout = None;
            }
        }
        if let Some(ref mut pipe) = stderr {
            if drain_available(pipe, &mut errors, &mut stderr_truncated) {
                stderr = None;
            }
        }
        if exited.is_none() {
            if let (Some(pipe), Some(bytes)) = (stdin.as_mut(), input) {
                match pipe.write(&bytes[written..]) {
                    Ok(0) => {}
                    Ok(size) => {
                        written += size;
                        if written == bytes.len() {
                            stdin = None;
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::Interrupted
                            || err.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(err) => {
                        write_result = Err(format!("failed to write {label} input: {err}"));
                        stdin = None;
                    }
                }
            }
            if began.elapsed() >= next_heartbeat {
                if let Some(operation) = operation {
                    install_events::progress(operation, ProgressKind::Wait, began.elapsed());
                }
                next_heartbeat = began.elapsed() + interval;
            }
        } else {
            stdin = None;
            if input.is_some_and(|bytes| written < bytes.len()) && write_result.is_ok() {
                write_result = Err(format!(
                    "failed to write {label} input: process exited before input was sent"
                ));
            }
            if (stdout.is_none() && stderr.is_none())
                || after_exit.is_some_and(|at: Instant| at.elapsed() >= Duration::from_millis(200))
            {
                break;
            }
        }
        let mut fds = Vec::with_capacity(3);
        if let Some(pipe) = &stdout {
            fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(pipe) = &stderr {
            fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if exited.is_none() {
            if let Some(pipe) = &stdin {
                fds.push(libc::pollfd {
                    fd: pipe.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                });
            }
        }
        // Poll only as an interruptible sleep: always recheck the real parent.
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 25);
        }
    }
    finish_capture(&mut output, stdout_truncated);
    finish_capture(&mut errors, stderr_truncated);
    (
        exited.expect("parent exit checked"),
        output,
        errors,
        write_result,
    )
}

/// Only use for probes with a fixed, non-sensitive response. No probe output
/// is emitted to the browser or written to the private failure log.
#[cfg(unix)]
pub fn bounded_probe_output(
    mut command: Command,
    deadline: Instant,
    operation: Option<Operation>,
) -> Option<String> {
    if Instant::now() >= deadline {
        return None;
    }
    let began = Instant::now();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let (status, stdout, _, _) = supervise_web_child(
        &mut child,
        "schema probe",
        None,
        operation,
        began,
        Duration::from_secs(10),
        Some(deadline),
    );
    status
        .ok()
        .filter(|status| status.success())
        .and_then(|_| String::from_utf8(stdout).ok())
}

/// Web subprocesses drain both pipes concurrently, keeping bounded failure
/// diagnostics in the private local file, never in events or HTTP responses.
pub fn run_command_with_progress(
    command: &mut Command,
    label: &str,
    input: Option<&[u8]>,
    operation: Option<Operation>,
) -> Result<std::process::ExitStatus, String> {
    run_command_with_interval(command, label, input, operation, Duration::from_secs(10))
}

fn run_command_with_interval(
    command: &mut Command,
    label: &str,
    input: Option<&[u8]>,
    operation: Option<Operation>,
    interval: Duration,
) -> Result<std::process::ExitStatus, String> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    if install_events::is_active() {
        let began = Instant::now();
        if let Some(operation) = operation {
            install_events::progress(operation, ProgressKind::Start, Duration::ZERO);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                if let Some(operation) = operation {
                    install_events::progress(
                        operation,
                        ProgressKind::Failure(None),
                        began.elapsed(),
                    );
                }
                format!("failed to run {label}: {err}")
            })?;
        #[cfg(unix)]
        let (status, stdout, stderr, write_result) =
            supervise_web_child(&mut child, label, input, operation, began, interval, None);
        #[cfg(not(unix))]
        let (status, stdout, stderr, write_result) = {
            let stdout = child.stdout.take().ok_or("child stdout unavailable")?;
            let stderr = child.stderr.take().ok_or("child stderr unavailable")?;
            let stdout_reader = thread::spawn(move || drain_bounded(stdout));
            let stderr_reader = thread::spawn(move || drain_bounded(stderr));
            let write_result = if let Some(bytes) = input {
                child
                    .stdin
                    .take()
                    .ok_or("child stdin unavailable")?
                    .write_all(bytes)
                    .map_err(|err| format!("failed to write {label} input: {err}"))
            } else {
                Ok(())
            };
            let mut next_heartbeat = interval;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Err(err) => break Err(format!("failed to wait for {label}: {err}")),
                    Ok(None) => {
                        if began.elapsed() >= next_heartbeat {
                            if let Some(operation) = operation {
                                install_events::progress(
                                    operation,
                                    ProgressKind::Wait,
                                    began.elapsed(),
                                );
                            }
                            next_heartbeat += interval;
                        }
                        thread::sleep(Duration::from_millis(100).min(interval));
                    }
                }
            };
            (
                status,
                stdout_reader.join().unwrap_or_default(),
                stderr_reader.join().unwrap_or_default(),
                write_result,
            )
        };
        if status.as_ref().is_ok_and(|status| !status.success()) || write_result.is_err() {
            install_events::record_command_failure(label, &stdout, &stderr);
        }
        if let Some(operation) = operation {
            let kind = match &status {
                Ok(status) if status.success() && write_result.is_ok() => {
                    ProgressKind::Success(status.code())
                }
                Ok(status) => ProgressKind::Failure(status.code()),
                Err(_) => ProgressKind::Failure(None),
            };
            install_events::progress(operation, kind, began.elapsed());
        }
        write_result?;
        status
    } else if let Some(bytes) = input {
        let mut child = command
            .spawn()
            .map_err(|err| format!("failed to run {label}: {err}"))?;
        child
            .stdin
            .take()
            .ok_or("child stdin unavailable")?
            .write_all(bytes)
            .map_err(|err| format!("failed to write {label} input: {err}"))?;
        child
            .wait()
            .map_err(|err| format!("failed to wait for {label}: {err}"))
    } else {
        command
            .status()
            .map_err(|err| format!("failed to run {label}: {err}"))
    }
}

pub fn command_success(command: &str, args: &[&str]) -> bool {
    external_command(command)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = external_command(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn kubectl_cluster_access() -> bool {
    command_success("kubectl", &["cluster-info"])
}

pub fn kubectl_server_git_version(output: &str) -> Option<String> {
    let version: serde_json::Value = serde_json::from_str(output).ok()?;
    version
        .get("serverVersion")?
        .get("gitVersion")?
        .as_str()
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

pub fn ready_node_count() -> Option<usize> {
    command_output("kubectl", &["get", "nodes", "--no-headers"]).map(|nodes| {
        nodes
            .lines()
            .filter(|line| line.split_whitespace().nth(1) == Some("Ready"))
            .count()
    })
}

pub fn api_resource_available(api_group: &str, resource: &str) -> bool {
    command_output(
        "kubectl",
        &["api-resources", "--api-group", api_group, "--no-headers"],
    )
    .is_some_and(|resources| {
        resources
            .lines()
            .any(|line| line.split_whitespace().next() == Some(resource))
    })
}

pub fn can_i(permission: &PermissionRequirement) -> bool {
    let mut args = vec!["auth", "can-i", permission.verb, permission.resource];
    if let Some(namespace) = permission.namespace {
        args.extend(["-n", namespace]);
    }
    command_success("kubectl", &args)
}

pub fn default_storage_class() -> Option<String> {
    command_output(
        "kubectl",
        &[
            "get", "storageclass", "-o",
            "jsonpath={range .items[?(@.metadata.annotations.storageclass\\.kubernetes\\.io/is-default-class==\"true\")]}{.metadata.name}{\"\\n\"}{end}",
        ],
    ).and_then(|output| output.lines().next().map(str::to_string))
}

pub fn chart_ref(chart_name: &str, default_ref: &str) -> String {
    env::var("KUBARR_CHARTS_DIR")
        .ok()
        .map(|dir| Path::new(&dir).join(chart_name).display().to_string())
        .unwrap_or_else(|| default_ref.to_string())
}

#[cfg(test)]
pub fn try_run_or_print(
    command: &str,
    args: &[&str],
    dry_run: bool,
    allow_failure: bool,
) -> Result<(), String> {
    try_run_or_print_progress(command, args, dry_run, allow_failure, None)
}

pub fn try_run_or_print_progress(
    command: &str,
    args: &[&str],
    dry_run: bool,
    allow_failure: bool,
    operation: Option<Operation>,
) -> Result<(), String> {
    if dry_run {
        if install_events::emit(format!("[PLAN] {} {}", command, args.join(" "))) {
            return Ok(());
        }
        println!(
            "   {} {} {}",
            status_label("plan", CYAN),
            command,
            args.join(" ")
        );
        return Ok(());
    }
    if install_events::is_active() {
        let status = run_command_with_progress(
            external_command(command).args(args),
            command,
            None,
            operation,
        )?;
        if !status.success() && !allow_failure {
            return Err(format!("{command} failed (exit status {status})"));
        }
        return Ok(());
    }
    println!(
        "   {} {} {}",
        status_label("run", BLUE),
        command,
        args.join(" ")
    );
    let output = external_command(command)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {command}: {e}"))?;
    print_command_output(&output.stdout);
    print_command_output(&output.stderr);
    if !output.status.success() && !allow_failure {
        return Err(format!("{command} failed (exit status {})", output.status));
    }
    Ok(())
}

fn print_command_output(output: &[u8]) {
    let text = String::from_utf8_lossy(output);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        println!("     {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn target_scope_applies_to_real_child_processes_without_changing_parent_environment() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "kubarr-command-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let script = "#!/bin/sh\nprintf '%s\\n' \"$KUBECONFIG\" \"$HELM_KUBECONTEXT\" \"$HELM_KUBEAPISERVER\" \"$@\"";
        let kubectl = path.join("kubectl");
        let helm = path.join("helm");
        for executable in [&kubectl, &helm] {
            std::fs::write(executable, script).unwrap();
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = path.join("secret.yaml");
        std::fs::write(&config, "test").unwrap();
        {
            let _scope = scope_target(Some(CommandTarget {
                kubeconfig: config.clone(),
                context: "selected".into(),
                _snapshot: None,
            }));
            let out = external_command(kubectl.to_str().unwrap())
                .arg("get")
                .output()
                .unwrap();
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(text.contains(config.to_str().unwrap()));
            assert!(text.contains("--context=selected"));
            let mut command = external_command(helm.to_str().unwrap());
            assert!(command
                .get_envs()
                .any(|(name, value)| name == "HELM_KUBEAPISERVER" && value.is_none()));
            let out = command.arg("upgrade").output().unwrap();
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(text.contains("selected"));
            assert!(text.contains(config.to_str().unwrap()));
            assert!(text.contains("--kube-context"));
            assert!(text.contains("--kubeconfig"));
        }
        assert!(!has_target());
        let out = external_command(kubectl.to_str().unwrap())
            .arg("get")
            .output()
            .unwrap();
        assert!(!String::from_utf8(out.stdout)
            .unwrap()
            .contains("--context=selected"));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn helm_version_parses_stable_prerelease_and_build_metadata() {
        for version in [
            "v4.3.0",
            "v4.0.0-rc.1",
            "v4.3.0+gabc123",
            " v4.3.0-rc.1+gabc123\n",
        ] {
            assert_eq!(helm_major_version(version), Some(4));
        }
    }

    #[test]
    fn helm_version_rejects_other_majors_and_malformed_output() {
        for version in ["v3.19.0+gabc123", "v5.0.0", "v40.3.0"] {
            assert_ne!(helm_major_version(version), Some(4));
        }
        for version in ["", "4.3.0", "v4", "v4.3", "v4.x.0", "v4.3.0.1", "not helm"] {
            assert_eq!(helm_major_version(version), None);
        }
    }

    #[test]
    fn kubectl_server_version_uses_server_instead_of_client_version() {
        let output = r#"{
            "clientVersion": {"gitVersion": "v1.36.4"},
            "serverVersion": {"gitVersion": "v1.35.8"}
        }"#;

        assert_eq!(
            kubectl_server_git_version(output).as_deref(),
            Some("v1.35.8")
        );
    }

    #[test]
    fn kubectl_server_version_rejects_missing_or_malformed_output() {
        for output in [
            r#"{"clientVersion":{"gitVersion":"v1.36.4"}}"#,
            r#"{"serverVersion":{}}"#,
            r#"{"serverVersion":{"gitVersion":42}}"#,
            r#"{"serverVersion":{"gitVersion":""}}"#,
            "not json",
        ] {
            assert_eq!(kubectl_server_git_version(output), None);
        }
    }

    #[test]
    fn command_failure_returns_error_without_exiting_installer() {
        let _lock = install_events::INSTALL_TEST_LOCK.lock().unwrap();
        let result = try_run_or_print("sh", &["-c", "exit 7"], false, false);
        assert!(result.unwrap_err().contains("exit status: 7"));
        assert!(try_run_or_print("sh", &["-c", "exit 7"], false, true).is_ok());
    }

    #[test]
    fn command_capture_drains_and_bounds_large_outputs() {
        let result = drain_bounded(std::io::Cursor::new(vec![b'x'; MAX_CAPTURE_BYTES * 3]));
        assert_eq!(result.len(), MAX_CAPTURE_BYTES);
        assert!(result.ends_with(b"[output truncated]\n"));
    }

    #[test]
    fn synthetic_wait_and_exit_are_reported_without_shell_args_or_output() {
        let _lock = install_events::INSTALL_TEST_LOCK.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        install_events::set_install_log(tx);
        let mut command = external_command("sh");
        command.args([
            "-c",
            "sleep 0.12; printf SECRET_stdout; printf SECRET_stderr >&2; exit 9",
            "--set",
            "SECRET_password",
        ]);
        let status = run_command_with_interval(
            &mut command,
            "sh",
            None,
            Some(Operation::FluentBit),
            Duration::from_millis(20),
        )
        .unwrap();
        install_events::clear_install_log();
        assert_eq!(status.code(), Some(9));
        let lines: Vec<_> = rx
            .into_iter()
            .filter_map(|event| event.browser_line())
            .collect();
        assert!(lines.first().unwrap().contains("[RUN] Fluent Bit:"));
        assert!(lines
            .iter()
            .any(|line| line.contains("[WAIT] Fluent Bit:") && line.contains("elapsed 0s")));
        assert!(lines.last().unwrap().contains("[FAIL] Fluent Bit:"));
        assert!(lines.last().unwrap().contains("exit 9"));
        let body = lines.join("\n");
        for secret in ["SECRET_stdout", "SECRET_stderr", "SECRET_password", "--set"] {
            assert!(!body.contains(secret));
        }
    }

    #[test]
    fn synthetic_success_reports_completion_after_child_exit() {
        let _lock = install_events::INSTALL_TEST_LOCK.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        install_events::set_install_log(tx);
        let mut command = external_command("sh");
        command.args(["-c", "sleep 0.04; exit 0"]);
        let status = run_command_with_interval(
            &mut command,
            "sh",
            None,
            Some(Operation::Gateway),
            Duration::from_millis(10),
        )
        .unwrap();
        install_events::clear_install_log();
        assert!(status.success());
        let lines: Vec<_> = rx
            .into_iter()
            .filter_map(|event| event.browser_line())
            .collect();
        assert!(lines.iter().any(|line| line.contains("[WAIT] Gateway:")));
        assert!(lines.last().unwrap().contains("[OK] Gateway:"));
        assert!(lines.last().unwrap().contains("exit 0"));
        assert!(!lines.iter().any(|line| line.contains("[FAIL]")));
    }

    #[cfg(unix)]
    #[test]
    fn detached_helper_inheriting_pipes_cannot_delay_real_parent_completion() {
        let _lock = install_events::INSTALL_TEST_LOCK.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (path, log) = install_events::create_diagnostic_log().unwrap();
        install_events::set_diagnostic_log(Some(log));
        install_events::set_install_log(tx);
        let began = Instant::now();
        let (finished, outcome) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut command = external_command("sh");
            command.args(["-c", "setsid sleep 3 & printf '%s\\n' \"$!\"; exit 7"]);
            finished
                .send(run_command_with_interval(
                    &mut command,
                    "sh",
                    None,
                    Some(Operation::Gateway),
                    Duration::from_millis(20),
                ))
                .unwrap();
        });
        let status = outcome
            .recv_timeout(Duration::from_secs(2))
            .expect("parent exit waited for a detached helper")
            .unwrap();
        worker.join().unwrap();
        let elapsed = began.elapsed();
        install_events::clear_install_log();
        install_events::set_diagnostic_log(None);
        let diagnostic = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        let pid: i32 = diagnostic
            .split("stdout:\n")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        assert_eq!(status.code(), Some(7));
        assert!(
            elapsed < Duration::from_secs(2),
            "parent exit was held by inherited pipes: {elapsed:?}"
        );
        let lines: Vec<_> = rx
            .into_iter()
            .filter_map(|event| event.browser_line())
            .collect();
        assert!(lines.last().unwrap().contains("[FAIL] Gateway:"));
        assert!(lines.last().unwrap().contains("exit 7"));
        assert!(!lines.join(" ").contains(&pid.to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn large_piped_input_and_two_full_output_streams_do_not_block_progress() {
        let _lock = install_events::INSTALL_TEST_LOCK.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        install_events::set_install_log(tx);
        let mut command = external_command("sh");
        command.args(["-c", "dd if=/dev/zero bs=4096 count=40 2>/dev/null; dd if=/dev/zero bs=4096 count=40 >&2 2>/dev/null; cat >/dev/null"]);
        let status = run_command_with_interval(
            &mut command,
            "sh",
            Some(&vec![b'X'; 256 * 1024]),
            Some(Operation::StorageSecret),
            Duration::from_millis(20),
        )
        .unwrap();
        install_events::clear_install_log();
        assert!(status.success());
        assert!(rx
            .into_iter()
            .filter_map(|event| event.browser_line())
            .last()
            .unwrap()
            .contains("[OK] Storage:"));
    }

    #[cfg(unix)]
    #[test]
    fn supervised_child_retains_only_bounded_private_failure_output() {
        let _lock = install_events::INSTALL_TEST_LOCK.lock().unwrap();
        let (path, log) = install_events::create_diagnostic_log().unwrap();
        install_events::set_diagnostic_log(Some(log));
        let (tx, rx) = std::sync::mpsc::channel();
        install_events::set_install_log(tx);
        let mut command = external_command("sh");
        command.args(["-c", "dd if=/dev/zero bs=4096 count=40 2>/dev/null; dd if=/dev/zero bs=4096 count=40 >&2 2>/dev/null; exit 8"]);
        let status = run_command_with_interval(
            &mut command,
            "sh",
            None,
            Some(Operation::StorageSecret),
            Duration::from_millis(20),
        )
        .unwrap();
        install_events::clear_install_log();
        install_events::set_diagnostic_log(None);
        let diagnostic = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(status.code(), Some(8));
        assert!(diagnostic.len() <= 2 * MAX_CAPTURE_BYTES + 100);
        assert_eq!(
            diagnostic
                .windows(b"[output truncated]".len())
                .filter(|slice| *slice == b"[output truncated]")
                .count(),
            2
        );
        assert!(rx
            .into_iter()
            .filter_map(|event| event.browser_line())
            .last()
            .unwrap()
            .contains("[FAIL] Storage:"));
    }
}
