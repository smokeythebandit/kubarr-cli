use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::install_events;
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

fn helm_major_version(version: &str) -> Option<u64> {
    let release = version.trim().strip_prefix('v')?.split(['-', '+']).next()?;
    let mut components = release.split('.');
    let major = components.next()?.parse().ok()?;
    components.next()?.parse::<u64>().ok()?;
    components.next()?.parse::<u64>().ok()?;
    components.next().is_none().then_some(major)
}

pub fn command_exists(tool: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| dir.join(tool).is_file())
}

pub fn command_success(command: &str, args: &[&str]) -> bool {
    Command::new(command)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
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

pub fn run_or_print(command: &str, args: &[&str], dry_run: bool, allow_failure: bool) {
    if dry_run {
        if install_events::emit(format!("[PLAN] {} {}", command, args.join(" "))) {
            return;
        }
        println!(
            "   {} {} {}",
            status_label("plan", CYAN),
            command,
            args.join(" ")
        );
        return;
    }
    if install_events::emit(format!("[RUN] {} {}", command, args.join(" "))) {
        let output = Command::new(command)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to run {command}: {e}"));
        emit_command_output(&output.stdout);
        emit_command_output(&output.stderr);
        if !output.status.success() && !allow_failure {
            std::process::exit(output.status.code().unwrap_or(1));
        }
        return;
    }
    println!(
        "   {} {} {}",
        status_label("run", BLUE),
        command,
        args.join(" ")
    );
    let output = Command::new(command)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {command}: {e}"));
    print_command_output(&output.stdout);
    print_command_output(&output.stderr);
    if !output.status.success() && !allow_failure {
        std::process::exit(output.status.code().unwrap_or(1));
    }
}

fn emit_command_output(output: &[u8]) {
    let text = String::from_utf8_lossy(output);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        install_events::emit(format!("     {line}"));
    }
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
}
