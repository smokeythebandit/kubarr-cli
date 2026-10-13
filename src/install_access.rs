use crate::install_events;
use crate::style::{paint, step, GREEN};
use crate::types::{InstallOptions, WebAccess};
use crate::util::external_command;
use serde::Serialize;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const GATEWAY_SERVICE: &str = "kubarr-gateway";
pub const GATEWAY_NAMESPACE: &str = "openresty";
pub const GATEWAY_PORT: u16 = 8080;
pub const LB_PENDING: &str = "pending: gateway LoadBalancer address is not assigned yet; kubectl get svc -n openresty kubarr-gateway --watch";
pub const LB_ERROR: &str = "pending: could not read gateway LoadBalancer address; kubectl get svc -n openresty kubarr-gateway --watch";
pub const GATEWAY_FORWARD: &str =
    "forward: kubectl port-forward -n openresty svc/kubarr-gateway 8080:8080";

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AccessKind {
    Nodeport,
    PortForward,
    Loadbalancer,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Access {
    pub kind: AccessKind,
    pub url: Option<String>,
    pub instructions: Vec<String>,
    pub pending: bool,
}

fn probe(command: std::process::Command) -> Option<String> {
    #[cfg(unix)]
    {
        crate::util::bounded_probe_output(command, Instant::now() + Duration::from_secs(4), None)
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        None
    }
}

fn url_hint(hint: &str) -> Option<String> {
    hint.strip_prefix("open: http://")
        .filter(|address| {
            address.parse::<std::net::SocketAddr>().is_ok()
                || address
                    .rsplit_once(':')
                    .is_some_and(|(host, port)| port == "8080" && valid_hostname(host))
        })
        .map(|address| format!("http://{address}"))
}

pub fn resolve_access(access: WebAccess) -> Access {
    match access {
        WebAccess::NodePort(port) => {
            let url = node_ip().map(|ip| format!("http://{ip}:{port}"));
            Access { kind: AccessKind::Nodeport, pending: url.is_none(),
                instructions: if url.is_none() { vec!["Find a reachable cluster node IP and use the selected gateway NodePort.".into()] } else { vec![] }, url }
        }
        WebAccess::PortForward => Access { kind: AccessKind::PortForward,
            url: Some(format!("http://127.0.0.1:{GATEWAY_PORT}")), pending: false,
            instructions: vec![
                "Use the kubeconfig selected for this installation. Run on the machine where you will open the dashboard:".into(),
                GATEWAY_FORWARD.strip_prefix("forward: ").unwrap().into(),
                "The dashboard link works only after forwarding starts on that machine. If using a remote browser, forward the dashboard port to your browser's machine first.".into(),
            ] },
        WebAccess::LoadBalancer => refresh_loadbalancer(),
    }
}

pub fn refresh_loadbalancer() -> Access {
    let mut command = external_command("kubectl");
    command.args([
        "--request-timeout=3s",
        "get",
        "svc",
        "-n",
        GATEWAY_NAMESPACE,
        GATEWAY_SERVICE,
        "-o",
        "json",
    ]);
    let hint = probe(command).and_then(|json| lb_access_hint(&json));
    let url = hint.as_deref().and_then(url_hint);
    Access {
        kind: AccessKind::Loadbalancer,
        pending: url.is_none(),
        url,
        instructions: if hint.is_none() {
            vec!["Gateway LoadBalancer address is pending. Using the kubeconfig selected for this installation, check: kubectl get svc -n openresty kubarr-gateway --watch".into()]
        } else {
            vec![]
        },
    }
}

fn publish(message: String) {
    if !install_events::emit(message.clone()) {
        println!("   {}", paint(&message, GREEN));
    }
}

pub fn print_access_hint(options: &InstallOptions) {
    if options.dry_run {
        return;
    }
    step("Done", "Kubarr was installed successfully");

    let mode = options.web_access;
    let access = resolve_access(mode);
    install_events::access(access.clone());

    match access.kind {
        AccessKind::Nodeport => publish(format!(
            "open: {}",
            access.url.unwrap_or_else(|| format!(
                "http://<node-ip>:{}",
                match mode {
                    WebAccess::NodePort(port) => port,
                    _ => unreachable!(),
                }
            ))
        )),
        AccessKind::PortForward => {
            publish(GATEWAY_FORWARD.into());
            publish(format!("open: http://127.0.0.1:{GATEWAY_PORT}"));
        }
        AccessKind::Loadbalancer => publish(
            access
                .url
                .map(|url| format!("open: {url}"))
                .unwrap_or_else(|| LB_PENDING.into()),
        ),
    }
}

fn node_ip() -> Option<String> {
    let mut command = external_command("kubectl");
    command.args([
        "--request-timeout=3s",
        "get",
        "nodes",
        "-o",
        "jsonpath={.items[0].status.addresses[?(@.type==\"InternalIP\")].address}",
    ]);
    probe(command).and_then(|ip| select_node_ip(&ip))
}

/// Return only a validated endpoint; never reflect arbitrary kubectl output.
pub fn lb_access_hint(json: &str) -> Option<String> {
    let service: serde_json::Value = serde_json::from_str(json).ok()?;
    let ingress = service
        .pointer("/status/loadBalancer/ingress")?
        .as_array()?;
    for entry in ingress {
        if let Some(ip) = entry.get("ip").and_then(|v| v.as_str()) {
            if let Ok(ip) = ip.parse::<IpAddr>() {
                return Some(format!(
                    "open: http://{}:{GATEWAY_PORT}",
                    match ip {
                        IpAddr::V4(ip) => ip.to_string(),
                        IpAddr::V6(ip) => format!("[{ip}]"),
                    }
                ));
            }
        }
        if let Some(name) = entry.get("hostname").and_then(|v| v.as_str()) {
            if valid_hostname(name) {
                return Some(format!("open: http://{name}:{GATEWAY_PORT}"));
            }
        }
    }
    None
}

pub fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.starts_with(|c: char| c.is_ascii_alphanumeric())
                && label.ends_with(|c: char| c.is_ascii_alphanumeric())
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

fn select_node_ip(value: &str) -> Option<String> {
    let addresses: Vec<IpAddr> = value
        .split_whitespace()
        .filter_map(|address| address.parse::<IpAddr>().ok())
        .collect();
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| addresses.iter().find(|address| address.is_ipv6()))
        .map(|address| match address {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        })
}

#[cfg(test)]
mod tests {
    use super::{
        lb_access_hint, resolve_access, select_node_ip, url_hint, valid_hostname, AccessKind,
    };
    use crate::types::WebAccess;

    #[test]
    fn structured_access_has_no_placeholder_link_or_untrusted_url() {
        for hint in [
            "open: http://<node-ip>:30080",
            "open: http://user@host:8080",
            "open: http://bad/path:8080",
            "open: https://host:8080",
            "open: http://host:8080/secret",
        ] {
            assert_eq!(url_hint(hint), None);
        }
        for hint in [
            "open: http://[2001:db8::2]:8080",
            "open: http://example.org:8080",
            "open: http://192.0.2.2:30080",
        ] {
            assert!(url_hint(hint).is_some());
        }
        let forward = resolve_access(WebAccess::PortForward);
        assert_eq!(forward.kind, AccessKind::PortForward);
        assert_eq!(forward.url.as_deref(), Some("http://127.0.0.1:8080"));
        assert!(forward
            .instructions
            .iter()
            .any(|s| s.contains("svc/kubarr-gateway 8080:8080")));
        assert!(forward
            .instructions
            .iter()
            .any(|s| s.contains("browser's machine")));
    }

    #[test]
    fn lb_ingress_only_accepts_safe_ip_or_hostname() {
        for (entry, expected) in [
            (r#"{"ip":"192.0.2.20"}"#, "open: http://192.0.2.20:8080"),
            (r#"{"ip":"2001:db8::2"}"#, "open: http://[2001:db8::2]:8080"),
            (
                r#"{"hostname":"gateway.example.org"}"#,
                "open: http://gateway.example.org:8080",
            ),
        ] {
            assert_eq!(
                lb_access_hint(&format!(
                    r#"{{"status":{{"loadBalancer":{{"ingress":[{entry}]}}}}}}"#
                ))
                .as_deref(),
                Some(expected)
            );
        }
        for payload in [
            "{}",
            r#"{"status":{"loadBalancer":{"ingress":[]}}}"#,
            "not json",
            r#"{"status":{"loadBalancer":{"ingress":[{"ip":"192.0.2.1/private","hostname":"x\nSECRET"}]}}}"#,
        ] {
            assert_eq!(lb_access_hint(payload), None);
        }
        for hostname in [
            "x/secret",
            "-bad.example",
            "bad..example",
            "x:8080",
            "x\nSECRET",
        ] {
            assert!(!valid_hostname(hostname));
        }
    }

    #[test]
    fn node_output_only_yields_literal_ip_addresses_and_brackets_ipv6() {
        assert_eq!(select_node_ip("192.0.2.10"), Some("192.0.2.10".into()));
        assert_eq!(select_node_ip("2001:db8::a"), Some("[2001:db8::a]".into()));
        assert_eq!(
            select_node_ip("2001:db8::a 192.0.2.10"),
            Some("192.0.2.10".into())
        );
        assert_eq!(
            select_node_ip("SECRET_token 2001:db8::a"),
            Some("[2001:db8::a]".into())
        );
        for payload in [
            "SECRET_token",
            "192.0.2.10/SECRET",
            "192.0.2.10@attacker",
            "[2001:db8::a]",
            "http://192.0.2.10",
            "127.0.0.1:443",
        ] {
            assert_eq!(select_node_ip(payload), None, "accepted {payload}");
        }
    }
}
