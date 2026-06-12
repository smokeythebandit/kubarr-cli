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
            .map(|ip| ip.trim().to_string())
            .filter(|ip| !ip.is_empty())
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
