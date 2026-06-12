use std::process::{Command, Stdio};

use crate::style::{fail, ok, step};
use crate::util::{command_exists, has_help};

pub fn doctor(args: Vec<String>) {
    if has_help(&args) {
        println!(
            "Check tools needed by Kubarr.\n\nUSAGE:\n    kubarr doctor [--cluster]\n\nOPTIONS:\n    --cluster    Also check kubectl access to the current cluster"
        );
        return;
    }

    let check_cluster = args.iter().any(|arg| arg == "--cluster");
    let mut failed = false;

    step("Doctor", "checking local development tools");
    for tool in ["kubectl", "helm", "cargo", "node", "pnpm"] {
        if command_exists(tool) {
            ok(&format!("{tool} found"));
        } else {
            fail(&format!("{tool} missing"));
            failed = true;
        }
    }

    if check_cluster {
        step("Cluster", "checking Kubernetes API access");
        let status = Command::new("kubectl")
            .args(["cluster-info"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok_and(|status| status.success()) {
            ok("kubectl cluster access");
        } else {
            fail("kubectl cluster access");
            failed = true;
        }
    }

    if failed {
        std::process::exit(1);
    }
}
