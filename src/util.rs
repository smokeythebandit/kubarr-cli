use std::env;
use std::process::{Command, Stdio};

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

pub fn run_or_print(command: &str, args: &[&str], dry_run: bool, allow_failure: bool) {
    if dry_run {
        println!(
            "    {} {} {}",
            status_label("plan", CYAN),
            command,
            args.join(" ")
        );
        return;
    }
    println!(
        "    {} {} {}",
        status_label("run", BLUE),
        command,
        args.join(" ")
    );
    let status = Command::new(command)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {command}: {e}"));
    if !status.success() && !allow_failure {
        std::process::exit(status.code().unwrap_or(1));
    }
}
