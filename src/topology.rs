//! What the cluster looks like at install time: how many nodes the platform
//! can spread across, and where PostgreSQL's node-local volumes come from.
//!
//! The node count is the value every HA chart reads as `kubarr.ha.nodeCount`.
//! The worker keeps it current afterwards (it adds a two-minute Ready debounce
//! against reboots); at install time there is nothing to debounce, so a node
//! counts as soon as it is Ready.

use serde_json::Value;

use crate::types::LOCAL_PATH_STORAGE_CLASS;
use crate::util::command_output;

/// Nodes a replica can be scheduled on: Ready, not cordoned, and carrying no
/// taint that keeps ordinary pods off (`NoSchedule` / `NoExecute`).
pub fn schedulable_node_count_from_json(nodes_json: &str) -> Result<u32, String> {
    let nodes: Value = serde_json::from_str(nodes_json)
        .map_err(|err| format!("could not read the cluster's nodes: {err}"))?;
    let items = nodes
        .get("items")
        .and_then(Value::as_array)
        .ok_or("could not read the cluster's nodes: no node list")?;
    let count = items.iter().filter(|node| is_schedulable(node)).count();
    u32::try_from(count).map_err(|_| "the cluster reported too many nodes".to_string())
}

fn is_schedulable(node: &Value) -> bool {
    let ready = node
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|condition| condition["type"] == "Ready" && condition["status"] == "True");
    let cordoned = node.pointer("/spec/unschedulable") == Some(&Value::Bool(true));
    let repelled = node
        .pointer("/spec/taints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|taint| taint["effect"] == "NoSchedule" || taint["effect"] == "NoExecute");
    ready && !cordoned && !repelled
}

/// The node count to install with. Never below one: a cluster whose every
/// node is tainted still runs a single-replica install the way it always did.
pub fn detect_node_count(dry_run: bool) -> Result<u32, String> {
    if dry_run {
        return Ok(1);
    }
    let nodes = command_output("kubectl", &["get", "nodes", "-o", "json"])
        .ok_or("could not list the cluster's nodes to size the installation")?;
    Ok(schedulable_node_count_from_json(&nodes)?.max(1))
}

/// Whether PostgreSQL needs the bundled local-path provisioner: there is no
/// default StorageClass, or the default one is NFS-backed. A database on NFS is
/// exactly what moving to CloudNativePG gets away from.
pub fn needs_local_path(storage_classes_json: &str) -> bool {
    default_storage_class(storage_classes_json).is_none_or(|class| is_nfs(&class.provisioner))
}

struct DefaultClass {
    name: String,
    provisioner: String,
}

/// The cluster's default StorageClass. With several defaults Kubernetes picks
/// any non-NFS one is as good as another for us, so prefer one of those.
fn default_storage_class(storage_classes_json: &str) -> Option<DefaultClass> {
    let classes: Value = serde_json::from_str(storage_classes_json).ok()?;
    let defaults: Vec<DefaultClass> = classes
        .get("items")?
        .as_array()?
        .iter()
        .filter(|class| {
            [
                "storageclass.kubernetes.io/is-default-class",
                "storageclass.beta.kubernetes.io/is-default-class",
            ]
            .iter()
            .any(|key| class["metadata"]["annotations"][key] == "true")
        })
        .filter_map(|class| {
            Some(DefaultClass {
                name: class["metadata"]["name"].as_str()?.to_string(),
                provisioner: class["provisioner"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect();
    let mut defaults = defaults.into_iter();
    let first = defaults.next()?;
    if !is_nfs(&first.provisioner) {
        return Some(first);
    }
    defaults
        .find(|class| !is_nfs(&class.provisioner))
        .or(Some(first))
}

fn is_nfs(provisioner: &str) -> bool {
    matches!(
        provisioner,
        "kubarr.dev/nfs-subdir"
            | "nfs.csi.k8s.io"
            | "cluster.local/nfs-subdir-external-provisioner"
    ) || provisioner.to_ascii_lowercase().contains("nfs")
}

/// Where PostgreSQL's volumes come from.
#[derive(Debug, PartialEq, Eq)]
pub struct DatabaseStorage {
    /// Named explicitly even when it is the cluster default, so a re-run
    /// renders the same Cluster spec after local-path has become the default.
    pub storage_class: String,
    pub install_local_path: bool,
    /// Only when the cluster had no default at all: an NFS default someone
    /// chose is left alone.
    pub make_local_path_default: bool,
}

pub fn plan_database_storage(storage_classes_json: &str) -> DatabaseStorage {
    let default = default_storage_class(storage_classes_json);
    match default {
        Some(class) if !needs_local_path(storage_classes_json) => DatabaseStorage {
            storage_class: class.name,
            install_local_path: false,
            make_local_path_default: false,
        },
        other => DatabaseStorage {
            storage_class: LOCAL_PATH_STORAGE_CLASS.to_string(),
            install_local_path: true,
            make_local_path_default: other.is_none(),
        },
    }
}

/// A dry run reads nothing from the cluster, so it plans the fullest path: a
/// cluster with no default StorageClass.
pub fn detect_database_storage(dry_run: bool) -> Result<DatabaseStorage, String> {
    if dry_run {
        return Ok(plan_database_storage(r#"{"items":[]}"#));
    }
    let classes = command_output("kubectl", &["get", "storageclass", "-o", "json"])
        .ok_or("could not list the cluster's StorageClasses to place the database")?;
    Ok(plan_database_storage(&classes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, ready: &str, unschedulable: bool, taints: Value) -> Value {
        json!({
            "metadata": {"name": name},
            "spec": {"unschedulable": unschedulable, "taints": taints},
            "status": {"conditions": [
                {"type": "MemoryPressure", "status": "False"},
                {"type": "Ready", "status": ready}
            ]}
        })
    }

    fn count(nodes: Vec<Value>) -> u32 {
        schedulable_node_count_from_json(&json!({"items": nodes}).to_string()).unwrap()
    }

    #[test]
    fn cordoned_and_noschedule_tainted_nodes_do_not_count() {
        assert_eq!(
            count(vec![
                node("a", "True", false, json!([])),
                node("b", "True", true, json!([])),
                node(
                    "c",
                    "True",
                    false,
                    json!([{"key": "node-role.kubernetes.io/control-plane", "effect": "NoSchedule"}])
                ),
            ]),
            1
        );
    }

    #[test]
    fn ready_untainted_nodes_all_count() {
        let nodes = (0..3)
            .map(|i| node(&format!("n{i}"), "True", false, json!(null)))
            .collect();
        assert_eq!(count(nodes), 3);
    }

    #[test]
    fn notready_and_noexecute_nodes_do_not_count_but_prefer_noschedule_does() {
        assert_eq!(
            count(vec![
                node("down", "False", false, json!([])),
                node("unknown", "Unknown", false, json!([])),
                node(
                    "evicting",
                    "True",
                    false,
                    json!([{"key": "x", "effect": "NoExecute"}])
                ),
                node(
                    "soft",
                    "True",
                    false,
                    json!([{"key": "x", "effect": "PreferNoSchedule"}])
                ),
            ]),
            1
        );
        // No conditions at all is not Ready.
        assert_eq!(count(vec![json!({"metadata": {"name": "bare"}})]), 0);
    }

    #[test]
    fn unreadable_node_lists_are_errors_not_zero() {
        assert!(schedulable_node_count_from_json("not json").is_err());
        assert!(schedulable_node_count_from_json("{}").is_err());
    }

    #[test]
    fn a_dry_run_installs_for_one_node() {
        assert_eq!(detect_node_count(true).unwrap(), 1);
    }

    fn classes(entries: &[(&str, &str, bool)]) -> String {
        let items: Vec<Value> = entries
            .iter()
            .map(|(name, provisioner, default)| {
                json!({
                    "metadata": {
                        "name": name,
                        "annotations": if *default {
                            json!({"storageclass.kubernetes.io/is-default-class": "true"})
                        } else {
                            json!({})
                        }
                    },
                    "provisioner": provisioner
                })
            })
            .collect();
        json!({ "items": items }).to_string()
    }

    #[test]
    fn local_path_is_needed_without_a_default_or_with_an_nfs_default() {
        assert!(needs_local_path(&classes(&[])));
        assert!(needs_local_path(&classes(&[(
            "fast",
            "rancher.io/local-path",
            false
        )])));
        assert!(!needs_local_path(&classes(&[(
            "local-path",
            "rancher.io/local-path",
            true
        )])));
        for nfs in [
            "kubarr.dev/nfs-subdir",
            "nfs.csi.k8s.io",
            "cluster.local/nfs-subdir-external-provisioner",
            "example.com/NFS-client",
        ] {
            assert!(
                needs_local_path(&classes(&[("shared", nfs, true)])),
                "{nfs}"
            );
        }
        assert!(!needs_local_path(&classes(&[(
            "ceph",
            "rbd.csi.ceph.com",
            true
        )])));
        // The beta annotation still marks a default on older clusters.
        let beta = json!({"items": [{
            "metadata": {"name": "old", "annotations": {"storageclass.beta.kubernetes.io/is-default-class": "true"}},
            "provisioner": "rancher.io/local-path"
        }]});
        assert!(!needs_local_path(&beta.to_string()));
    }

    #[test]
    fn the_database_uses_the_default_class_by_name_or_installs_local_path() {
        assert_eq!(
            plan_database_storage(&classes(&[("longhorn", "driver.longhorn.io", true)])),
            DatabaseStorage {
                storage_class: "longhorn".into(),
                install_local_path: false,
                make_local_path_default: false,
            }
        );
        // No default at all: install it and make it the default.
        assert_eq!(
            plan_database_storage(&classes(&[])),
            DatabaseStorage {
                storage_class: "local-path".into(),
                install_local_path: true,
                make_local_path_default: true,
            }
        );
        // Someone chose an NFS default: install local-path beside it, but
        // leave their default alone.
        assert_eq!(
            plan_database_storage(&classes(&[("nfs", "kubarr.dev/nfs-subdir", true)])),
            DatabaseStorage {
                storage_class: "local-path".into(),
                install_local_path: true,
                make_local_path_default: false,
            }
        );
        // A re-run after the first one made local-path the default renders
        // the same class name.
        assert_eq!(
            plan_database_storage(&classes(&[("local-path", "rancher.io/local-path", true)]))
                .storage_class,
            "local-path"
        );
    }
}
