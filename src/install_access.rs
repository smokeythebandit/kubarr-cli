use std::process::Command;

use crate::style::{paint, step, BOLD, GREEN};
use crate::types::InstallOptions;

pub fn print_access_hint(options: &InstallOptions) {
    if options.dry_run {
        return;
    }
    step("Done", "Kubarr was installed successfully");

    if let Some(port) = options.backend_node_port {
        let node_ip = Command::new("kubectl")
            .args([
                "get",
                "nodes",
                "-o",
                "jsonpath={.items[0].status.addresses[?(@.type==\"InternalIP\")].address}",
            ])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok()
                } else {
                    None
                }
            })
            .and_then(|ip| select_node_ip(&ip))
            .unwrap_or_else(|| "<node-ip>".to_string());
        println!(
            "    {} {}",
            paint("open:", GREEN),
            paint(&format!("http://{node_ip}:{port}"), BOLD)
        );
    } else {
        println!(
            "    {} kubectl port-forward -n {} svc/{}-backend 8000:8000",
            paint("forward:", GREEN),
            options.namespace,
            options.release
        );
    }
}

fn select_node_ip(value: &str) -> Option<String> {
    let addresses: Vec<&str> = value.split_whitespace().collect();
    addresses
        .iter()
        .copied()
        .find(|address| address.parse::<std::net::Ipv4Addr>().is_ok())
        .or_else(|| addresses.first().copied())
        .map(str::to_string)
}
