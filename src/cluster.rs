use std::path::Path;

use crate::style::{ok, status_label, step, BLUE, CYAN};
use crate::types::{BootstrapOptions, ClusterMode};
use crate::util::{check_tool, command_success, external_command, set_target};

const K3S_CONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
const INSTALL_COMMAND: &str = "curl -sfL https://get.k3s.io | sh -s - --write-kubeconfig-mode 600";

pub fn setup_cluster_if_needed(options: &BootstrapOptions) -> Result<(), String> {
    match options.cluster_mode {
        ClusterMode::Existing => {
            if options.install.dry_run {
                step("Cluster", "using existing Kubernetes context");
            }
            Ok(())
        }
        ClusterMode::SingleNode => setup_single_node_cluster(options.install.dry_run),
    }
}

fn setup_single_node_cluster(dry_run: bool) -> Result<(), String> {
    setup_single_node_cluster_at(
        Path::new(K3S_CONFIG),
        dry_run,
        || command_success("kubectl", &["cluster-info"]),
        || {
            check_tool("curl")?;
            println!(
                "   {} sh -c '{}'",
                status_label("run", BLUE),
                INSTALL_COMMAND
            );
            let mut command = external_command("sh");
            command.args(["-c", INSTALL_COMMAND]);
            let status = crate::util::run_command_with_progress(
                &mut command,
                "k3s installer",
                None,
                Some(crate::install_events::Operation::K3s),
            )?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("k3s installer failed ({status})"))
            }
        },
    )
}

/// An existing but invalid k3s kubeconfig is an error, not permission to run an
/// installer or try another context. A missing config means a genuinely fresh
/// k3s install. Read/snapshot again *after* installation, never before the new
/// config exists.
fn setup_single_node_cluster_at(
    path: &Path,
    dry_run: bool,
    mut reachable: impl FnMut() -> bool,
    install: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    step("Cluster", "setting up single-node Kubernetes with k3s");
    if dry_run {
        println!(
            "   {} sh -c '{}'",
            status_label("plan", CYAN),
            INSTALL_COMMAND
        );
        println!(
            "   {} use local k3s kubeconfig at {K3S_CONFIG}",
            status_label("plan", CYAN)
        );
        return Ok(());
    }

    let present = match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err("cannot inspect local k3s kubeconfig".into()),
    };
    if present {
        set_target(crate::targets::k3s_target_at(path)?);
        if reachable() {
            ok("local k3s API is already reachable; skipping k3s installation");
            return Ok(());
        }
    }

    install()?;
    set_target(crate::targets::k3s_target_at(path)?);
    if !reachable() {
        return Err("k3s installed, but its local Kubernetes API is unreachable".into());
    }
    ok("using local k3s Kubernetes API");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::{external_command, has_target, scope_target};
    use std::cell::Cell;
    use std::fs;

    const LOCAL: &str = "kind: Config\napiVersion: v1\ncurrent-context: local\nclusters:\n- name: local\n  cluster: {server: 'https://127.0.0.1:6443'}\ncontexts:\n- name: local\n  context: {cluster: local, user: local}\nusers:\n- name: local\n  user: {token: local-token}\n";

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kubarr-k3s-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn fake_kubectl(dir: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("kubectl");
        fs::write(
            &program,
            "#!/bin/sh\ntest -f \"$KUBECONFIG\" && printf '%s\\n' \"$KUBECONFIG\" \"$1\"\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    #[cfg(unix)]
    #[test]
    fn existing_k3s_ignores_unrelated_context_and_pins_command_target() {
        let _scope = scope_target(None);
        let dir = temp_dir();
        let config = dir.join("k3s.yaml");
        let unrelated = dir.join("unrelated.yaml");
        fs::write(&config, LOCAL).unwrap();
        fs::write(&unrelated, LOCAL.replace("127.0.0.1", "remote.invalid")).unwrap();
        let fake = fake_kubectl(&dir);
        let calls = Cell::new(0);
        setup_single_node_cluster_at(
            &config,
            false,
            || {
                calls.set(calls.get() + 1);
                let output = external_command(fake.to_str().unwrap())
                    .arg("cluster-info")
                    .output()
                    .unwrap();
                let text = String::from_utf8(output.stdout).unwrap();
                assert!(output.status.success());
                assert!(!text.contains(unrelated.to_str().unwrap()));
                assert!(text.contains("--context=local"));
                true
            },
            || panic!("reachable local k3s must not reinstall"),
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(has_target());
        fs::write(&config, LOCAL.replace("127.0.0.1", "remote.invalid")).unwrap();
        let output = external_command(fake.to_str().unwrap()).output().unwrap();
        assert!(
            output.status.success(),
            "later commands must use the frozen snapshot"
        );
        drop(_scope);
        assert!(!has_target());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fresh_k3s_snapshots_only_after_installer_and_never_checks_unrelated_context() {
        let _scope = scope_target(None);
        let dir = temp_dir();
        let config = dir.join("k3s.yaml");
        let fake = fake_kubectl(&dir);
        let calls = Cell::new(0);
        setup_single_node_cluster_at(
            &config,
            false,
            || {
                calls.set(calls.get() + 1);
                assert!(has_target());
                external_command(fake.to_str().unwrap())
                    .output()
                    .unwrap()
                    .status
                    .success()
            },
            || {
                assert!(
                    !has_target(),
                    "must not use ambient KUBECONFIG while k3s is absent"
                );
                fs::write(&config, LOCAL).unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(has_target());
        drop(_scope);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_existing_k3s_fails_closed_without_running_installer_or_checks() {
        let _scope = scope_target(None);
        let dir = temp_dir();
        let config = dir.join("k3s.yaml");
        fs::write(&config, LOCAL.replace("127.0.0.1", "remote.invalid")).unwrap();
        assert!(setup_single_node_cluster_at(
            &config,
            false,
            || panic!("unexpected check"),
            || panic!("unexpected installer")
        )
        .is_err());
        assert!(!has_target());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fresh_or_repaired_k3s_repointed_to_remote_fails_before_install_checks() {
        let _scope = scope_target(None);
        let dir = temp_dir();
        let config = dir.join("k3s.yaml");
        let remote = LOCAL.replace("127.0.0.1", "remote.invalid");
        assert!(setup_single_node_cluster_at(
            &config,
            false,
            || panic!("must not check unrelated cluster"),
            || {
                fs::write(&config, &remote).unwrap();
                Ok(())
            }
        )
        .is_err());
        assert!(!has_target());

        fs::write(&config, LOCAL).unwrap();
        let checks = Cell::new(0);
        assert!(setup_single_node_cluster_at(
            &config,
            false,
            || {
                checks.set(checks.get() + 1);
                false
            },
            || {
                fs::write(&config, &remote).unwrap();
                Ok(())
            }
        )
        .is_err());
        assert_eq!(
            checks.get(),
            1,
            "remote replacement must not reach a subsequent check"
        );
        drop(_scope);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dry_run_skips_file_access_commands_and_installation() {
        let _scope = scope_target(None);
        let fake_path = Path::new("/path/which/is/not/read/in-dry-run");
        assert!(setup_single_node_cluster_at(
            fake_path,
            true,
            || panic!("unexpected check"),
            || panic!("unexpected installer")
        )
        .is_ok());
        assert!(!has_target());
    }
}
