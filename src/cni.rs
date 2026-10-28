//! Enables Hubble flow metrics on an existing Cilium install.
//!
//! The network page's internet/LAN/cluster split reads Hubble's metrics out of
//! VictoriaMetrics. Hubble ships with Cilium but exports nothing by default, so
//! this step turns the exporter on where Cilium is already running.
//!
//! It deliberately does not install a CNI. The single-node path installs k3s
//! with its own flannel, and replacing a working CNI is not something an
//! application installer should do quietly.
use crate::install_events::Operation;
use crate::style::{detail, ok, step, warn};
use crate::types::BootstrapOptions;
use crate::util::{check_tool, command_output, try_run_or_print_progress};

const CILIUM_RELEASE: &str = "cilium";
const CILIUM_NAMESPACE: &str = "kube-system";

/// Metric specifications passed to Cilium.
///
/// `destinationContext=namespace|identity` is the whole trick: Hubble uses the
/// first non-empty context, so cluster destinations report a bare namespace
/// while external ones report identity labels — `reserved:world` for the
/// internet and `cidr:...` for the LAN ranges.
const FLOW_METRIC: &str =
    "flow:sourceContext=namespace;destinationContext=namespace|identity;labelsContext=traffic_direction";
const DROP_METRIC: &str = "drop:sourceContext=namespace;destinationContext=namespace|identity";

#[derive(Debug, PartialEq, Eq)]
pub enum CniState {
    Cilium,
    OtherCni,
    Unknown,
}

/// Inspect `helm list -A -o json` output for a Cilium release.
pub fn cni_state_from_releases(json: &str) -> CniState {
    let Ok(releases) = serde_json::from_str::<serde_json::Value>(json) else {
        // Unreadable output is not evidence that no CNI exists.
        return CniState::Unknown;
    };
    let Some(releases) = releases.as_array() else {
        return CniState::Unknown;
    };
    if releases.iter().any(|release| {
        release["chart"]
            .as_str()
            .unwrap_or_default()
            .starts_with("cilium-")
    }) {
        return CniState::Cilium;
    }
    CniState::OtherCni
}

/// Helm arguments that turn the Hubble exporter on and leave everything else be.
pub fn hubble_upgrade_args(release: &str, namespace: &str) -> Vec<String> {
    vec![
        "upgrade".to_string(),
        release.to_string(),
        "cilium/cilium".to_string(),
        "-n".to_string(),
        namespace.to_string(),
        // This upgrades a running CNI. Dropping the operator's existing values
        // would be a connectivity outage, and re-pinning the chart version
        // could change the data path, so neither is done here.
        "--reuse-values".to_string(),
        "--set".to_string(),
        format!("hubble.metrics.enabled={{{FLOW_METRIC},{DROP_METRIC}}}"),
        "--set".to_string(),
        "podAnnotations.prometheus\\.io/scrape=true".to_string(),
        "--set".to_string(),
        "podAnnotations.prometheus\\.io/port=9965".to_string(),
        "--set".to_string(),
        "podAnnotations.prometheus\\.io/path=/metrics".to_string(),
        "--timeout".to_string(),
        crate::util::HELM_TIMEOUT.to_string(),
    ]
}

const IDENTITIES_CHART: &str = "oci://ghcr.io/smokeythebandit/charts/kubarr-cilium-identities";
const IDENTITIES_RELEASE: &str = "kubarr-cilium-identities";

/// Install the chart whose policy mints CIDR identities for the LAN ranges.
///
/// Without those identities Cilium reports every external destination as
/// `reserved:world`, and LAN traffic is indistinguishable from internet
/// traffic. The policy inside selects no Pod, so it changes no endpoint's
/// enforcement; see the chart for why that matters.
pub fn install_cidr_identities(options: &BootstrapOptions) -> Result<(), String> {
    step(
        "Network identities",
        "installing LAN CIDR identities with Helm",
    );
    let chart = crate::util::chart_ref("kubarr-cilium-identities", IDENTITIES_CHART);
    let version = crate::charts::pinned_version("kubarr-cilium-identities", &chart)?;

    let mut args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        IDENTITIES_RELEASE.to_string(),
        chart,
        "-n".to_string(),
        CILIUM_NAMESPACE.to_string(),
        "--timeout".to_string(),
        crate::util::HELM_TIMEOUT.to_string(),
    ];
    if let Some(version) = version {
        args.extend(["--version".to_string(), version.to_string()]);
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    try_run_or_print_progress(
        "helm",
        &refs,
        options.install.dry_run,
        false,
        Some(Operation::CidrIdentities),
    )
}

/// Turn on Hubble metrics if this cluster runs Cilium.
pub fn ensure_hubble_metrics(options: &BootstrapOptions) -> Result<(), String> {
    step("Network flows", "enabling Hubble metrics where Cilium runs");
    if options.install.dry_run {
        detail(
            "plan",
            "enable Hubble metrics on an existing Cilium release",
        );
        return Ok(());
    }
    check_tool("helm")?;

    let releases = command_output("helm", &["list", "-A", "-o", "json"]).unwrap_or_default();
    match cni_state_from_releases(&releases) {
        CniState::Cilium => {
            install_cidr_identities(options)?;
            let args = hubble_upgrade_args(CILIUM_RELEASE, CILIUM_NAMESPACE);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            try_run_or_print_progress("helm", &refs, false, false, Some(Operation::Hubble))
        }
        CniState::OtherCni => {
            // Not an error: everything else still works, the network page just
            // cannot break traffic down by destination.
            warn("no Cilium release found; network flow classes will be unavailable");
            Ok(())
        }
        CniState::Unknown => {
            warn("could not read Helm releases; leaving the CNI untouched");
            Ok(())
        }
    }
    .map(|outcome| {
        ok("Hubble metrics requested");
        outcome
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_cilium_release() {
        let releases = r#"[{"name":"cilium","namespace":"kube-system","chart":"cilium-1.20.4","status":"deployed"}]"#;
        assert_eq!(cni_state_from_releases(releases), CniState::Cilium);
    }

    #[test]
    fn a_cluster_without_cilium_is_left_alone() {
        assert_eq!(cni_state_from_releases("[]"), CniState::OtherCni);
        let calico = r#"[{"name":"calico","chart":"tigera-operator-3.29.0"}]"#;
        assert_eq!(cni_state_from_releases(calico), CniState::OtherCni);
    }

    #[test]
    fn unreadable_output_is_not_mistaken_for_an_absent_cni() {
        assert_eq!(cni_state_from_releases("not json"), CniState::Unknown);
        assert_eq!(cni_state_from_releases(""), CniState::Unknown);
    }

    #[test]
    fn the_upgrade_keeps_existing_values_and_pins_no_version() {
        let args = hubble_upgrade_args("cilium", "kube-system");

        assert!(args.contains(&"--reuse-values".to_string()));
        assert!(args
            .iter()
            .any(|arg| arg.contains("hubble.metrics.enabled")));
        // The context chain is what separates LAN from internet.
        assert!(args
            .iter()
            .any(|arg| arg.contains("destinationContext=namespace|identity")));
        // Without the scrape annotations VictoriaMetrics never collects it.
        assert!(args
            .iter()
            .any(|arg| arg.contains("prometheus\\.io/scrape")));
        assert!(args.iter().any(|arg| arg.contains("9965")));
        // Never re-pin the chart version of a running CNI.
        assert!(!args.iter().any(|arg| arg == "--version"));
    }
}
