//! Kubeconfig inventory and private, session-owned target snapshots. Never run
//! kubectl while parsing configs: even apparently read-only config commands can
//! expose credential material or invoke authentication integrations later.
use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_yaml::{Mapping, Value};

use crate::util::CommandTarget;

pub const MAX_CONFIG: usize = 256 * 1024;
const MAX_TARGETS: usize = 64;
const MAX_UPLOADS: usize = 4;
const MAX_FILES: usize = 24;

#[derive(Clone, Debug, Serialize)]
pub struct Choice {
    pub id: String,
    pub source: String,
    pub label: String,
    pub context: String,
    pub current: bool,
}

pub struct Registry {
    dir: PathBuf,
    entries: Vec<(Choice, CommandTarget)>,
    uploaded: usize,
    discovered: bool,
}

#[derive(Debug)]
pub struct PrivateK3sSnapshot {
    dir: PathBuf,
}

impl Drop for PrivateK3sSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn private_dir() -> Result<PathBuf, String> {
    for _ in 0..4 {
        let name = random_id()?;
        let dir = env::temp_dir().join(format!("kubarr-target-{name}"));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("could not create private target directory".into()),
        }
    }
    Err("could not create private target directory".into())
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "could not generate target id".to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Registry {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            dir: private_dir()?,
            entries: vec![],
            uploaded: 0,
            discovered: false,
        })
    }

    #[cfg(test)]
    pub fn empty_for_tests() -> Self {
        let mut registry = Self::new().unwrap();
        registry.discovered = true;
        registry
    }

    pub fn choices(&self) -> Vec<Choice> {
        self.entries
            .iter()
            .map(|(choice, _)| choice.clone())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<CommandTarget> {
        self.entries
            .iter()
            .find(|(choice, _)| choice.id == id)
            .map(|(_, target)| target.clone())
    }

    pub fn uploaded(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|(choice, _)| choice.id == id && choice.source == "upload")
    }

    pub fn discover(&mut self) {
        if self.discovered {
            return;
        }
        self.discovered = true;
        let mut env_files = vec![];
        if let Some(value) = env::var_os("KUBECONFIG") {
            env_files.extend(
                env::split_paths(&value)
                    .filter(|p| !p.as_os_str().is_empty())
                    .take(MAX_FILES),
            );
        }
        // kubectl merges KUBECONFIG entries using first-wins for named items.
        self.load_files(&env_files, "env", true);
        if let Some(home) = env::var_os("HOME") {
            let kube = PathBuf::from(home).join(".kube");
            self.load_files(&[kube.join("config")], "default", false);
            if let Ok(read) = fs::read_dir(&kube) {
                let mut extra: Vec<_> = read
                    .flatten()
                    .take(MAX_FILES * 4)
                    .map(|entry| entry.path())
                    .filter(|p| {
                        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.starts_with("config-") || n.ends_with(".yaml") || n.ends_with(".yml")
                        })
                    })
                    .collect();
                extra.sort();
                extra.truncate(MAX_FILES);
                self.load_files(&extra, "local", false);
            }
        }
        self.load_files(&[PathBuf::from("/etc/rancher/k3s/k3s.yaml")], "k3s", false);
    }

    fn load_files(&mut self, files: &[PathBuf], source: &str, merge: bool) {
        let mut contexts = HashMap::new();
        let mut clusters = HashMap::new();
        let mut users = HashMap::new();
        let mut current = None;
        for path in files {
            let Ok(data) = read_limited(path) else {
                continue;
            };
            let Ok(doc) = serde_yaml::from_slice::<Value>(&data) else {
                continue;
            };
            let Some(root) = doc.as_mapping() else {
                continue;
            };
            if current.is_none() {
                current = string(root, "current-context").map(str::to_string);
            }
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("/"))
                    .join(path)
            };
            let base = absolute.parent().unwrap_or(Path::new("."));
            for (field, dest) in [
                ("contexts", &mut contexts),
                ("clusters", &mut clusters),
                ("users", &mut users),
            ] {
                if let Some(list) = get(root, field).and_then(Value::as_sequence) {
                    for item in list.iter().take(MAX_TARGETS) {
                        let Some(map) = item.as_mapping() else {
                            continue;
                        };
                        let Some(name) = string(map, "name") else {
                            continue;
                        };
                        let key = match field {
                            "contexts" => "context",
                            "clusters" => "cluster",
                            _ => "user",
                        };
                        let Some(value) = get(map, key).cloned() else {
                            continue;
                        };
                        let mut value = value;
                        if field != "contexts" {
                            absolutize_local(&mut value, base);
                        }
                        dest.entry(name.to_string()).or_insert(value);
                    }
                }
            }
            if !merge {
                let display_source = source.to_string();
                self.add_contexts(
                    &contexts,
                    &clusters,
                    &users,
                    current.as_deref(),
                    &display_source,
                    false,
                );
                contexts.clear();
                clusters.clear();
                users.clear();
                current = None;
            }
        }
        if merge {
            self.add_contexts(
                &contexts,
                &clusters,
                &users,
                current.as_deref(),
                source,
                false,
            );
        }
    }

    fn add_contexts(
        &mut self,
        contexts: &HashMap<String, Value>,
        clusters: &HashMap<String, Value>,
        users: &HashMap<String, Value>,
        current: Option<&str>,
        source: &str,
        uploaded: bool,
    ) {
        let mut names: Vec<_> = contexts.keys().collect();
        names.sort();
        for name in names {
            if self.entries.len() >= MAX_TARGETS {
                break;
            }
            let Some(context) = contexts[name].as_mapping() else {
                continue;
            };
            let (Some(cluster_name), Some(user_name)) =
                (string(context, "cluster"), string(context, "user"))
            else {
                continue;
            };
            let (Some(cluster), Some(user)) = (clusters.get(cluster_name), users.get(user_name))
            else {
                continue;
            };
            if uploaded && !safe_upload(cluster, user) {
                continue;
            }
            let mut doc = Mapping::new();
            doc.insert(Value::from("apiVersion"), Value::from("v1"));
            doc.insert(Value::from("kind"), Value::from("Config"));
            doc.insert(Value::from("current-context"), Value::from(name.as_str()));
            for (key, item_name, key_inside, value) in [
                ("contexts", name.as_str(), "context", &contexts[name]),
                ("clusters", cluster_name, "cluster", cluster),
                ("users", user_name, "user", user),
            ] {
                let mut entry = Mapping::new();
                entry.insert(Value::from("name"), Value::from(item_name));
                entry.insert(Value::from(key_inside), value.clone());
                doc.insert(
                    Value::from(key),
                    Value::Sequence(vec![Value::Mapping(entry)]),
                );
            }
            let Ok(bytes) = serde_yaml::to_string(&doc) else {
                continue;
            };
            let Ok(id) = random_id() else { continue };
            let path = self.dir.join(format!("{id}.yaml"));
            if write_private(&path, bytes.as_bytes()).is_err() {
                continue;
            }
            let display = name
                .chars()
                .filter(|ch| !ch.is_control())
                .take(100)
                .collect::<String>();
            self.entries.push((
                Choice {
                    id,
                    source: source.to_string(),
                    label: format!("{display} ({source})"),
                    context: display,
                    current: current == Some(name.as_str()),
                },
                CommandTarget {
                    kubeconfig: path,
                    context: name.to_string(),
                    _snapshot: None,
                },
            ));
        }
    }

    pub fn upload(&mut self, text: &str) -> Result<Vec<Choice>, String> {
        const INVALID: &str = "Invalid or unsupported kubeconfig";
        if text.is_empty()
            || text.len() > MAX_CONFIG
            || self.uploaded >= MAX_UPLOADS
            || self.entries.len() >= MAX_TARGETS
        {
            return Err(INVALID.into());
        }
        let doc: Value = serde_yaml::from_str(text).map_err(|_| INVALID)?;
        let root = doc.as_mapping().ok_or(INVALID)?;
        if !only_keys(
            root,
            &[
                "apiVersion",
                "kind",
                "preferences",
                "current-context",
                "clusters",
                "contexts",
                "users",
            ],
        ) || string(root, "kind") != Some("Config")
        {
            return Err(INVALID.into());
        }
        let mut sets = Vec::new();
        for (field, inner) in [
            ("contexts", "context"),
            ("clusters", "cluster"),
            ("users", "user"),
        ] {
            let list = get(root, field)
                .and_then(Value::as_sequence)
                .ok_or(INVALID)?;
            if list.len() > MAX_TARGETS {
                return Err(INVALID.into());
            }
            let mut set = HashMap::new();
            for item in list {
                let map = item.as_mapping().ok_or(INVALID)?;
                if !only_keys(map, &["name", inner]) {
                    return Err(INVALID.into());
                }
                let name = string(map, "name")
                    .filter(|n| !n.is_empty() && n.len() <= 255)
                    .ok_or(INVALID)?;
                let value = get(map, inner).ok_or(INVALID)?;
                if set.insert(name.to_string(), value.clone()).is_some() {
                    return Err(INVALID.into());
                }
            }
            sets.push(set);
        }
        let before = self.entries.len();
        let contexts = &sets[0];
        let clusters = &sets[1];
        let users = &sets[2];
        for context in contexts.values() {
            let map = context.as_mapping().ok_or(INVALID)?;
            if !only_keys(map, &["cluster", "user", "namespace"]) {
                return Err(INVALID.into());
            }
            let cluster = clusters
                .get(string(map, "cluster").ok_or(INVALID)?)
                .ok_or(INVALID)?;
            let user = users
                .get(string(map, "user").ok_or(INVALID)?)
                .ok_or(INVALID)?;
            if !safe_upload(cluster, user) {
                return Err(INVALID.into());
            }
        }
        if contexts.is_empty() || contexts.len() > MAX_TARGETS - self.entries.len() {
            return Err(INVALID.into());
        }
        self.add_contexts(
            contexts,
            clusters,
            users,
            string(root, "current-context"),
            "upload",
            true,
        );
        if self.entries.len() == before {
            return Err(INVALID.into());
        }
        self.uploaded += 1;
        Ok(self.entries[before..]
            .iter()
            .map(|(choice, _)| choice.clone())
            .collect())
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::from(key))
}
fn string<'a>(map: &'a Mapping, key: &str) -> Option<&'a str> {
    get(map, key)?.as_str()
}
fn only_keys(map: &Mapping, keys: &[&str]) -> bool {
    map.keys()
        .all(|key| key.as_str().is_some_and(|key| keys.contains(&key)))
}

fn safe_upload(cluster: &Value, user: &Value) -> bool {
    let (Some(c), Some(u)) = (cluster.as_mapping(), user.as_mapping()) else {
        return false;
    };
    if !only_keys(
        c,
        &["server", "certificate-authority-data", "tls-server-name"],
    ) || !only_keys(u, &["token", "client-certificate-data", "client-key-data"])
    {
        return false;
    }
    let Some(server) = string(c, "server") else {
        return false;
    };
    let Some(authority) = server.strip_prefix("https://") else {
        return false;
    };
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@'])
        || authority.chars().any(char::is_whitespace)
        || server.len() > 2048
    {
        return false;
    }
    if let Some(name) = string(c, "tls-server-name") {
        if name.is_empty() || name.len() > 255 || name.contains(['\r', '\n', '\0']) {
            return false;
        }
    }
    for key in [
        "certificate-authority-data",
        "client-certificate-data",
        "client-key-data",
    ] {
        let value = if key == "certificate-authority-data" {
            get(c, key)
        } else {
            get(u, key)
        };
        if value.is_some_and(|v| v.as_str().is_none_or(str::is_empty)) {
            return false;
        }
    }
    let cert = get(u, "client-certificate-data").is_some();
    let key = get(u, "client-key-data").is_some();
    let token = get(u, "token")
        .and_then(Value::as_str)
        .is_some_and(|v| !v.is_empty());
    (cert && key && !token) || (!cert && !key && token)
}

fn absolutize_local(value: &mut Value, base: &Path) {
    let Some(map) = value.as_mapping_mut() else {
        return;
    };
    for field in [
        "certificate-authority",
        "client-certificate",
        "client-key",
        "tokenFile",
    ] {
        if let Some(path) = map.get_mut(Value::from(field)) {
            if let Some(name) = path.as_str() {
                let joined = base.join(name);
                // Preserve relative references with respect to their originating config.
                if let Ok(full) = joined.canonicalize() {
                    *path = Value::from(full.to_string_lossy().to_string());
                } else if base.is_absolute() {
                    *path = Value::from(joined.to_string_lossy().to_string());
                }
            }
        }
    }
    // A trusted local exec plugin with a relative path is resolved by kubectl
    // against its original kubeconfig directory. Moving the config to our
    // private snapshot must not silently change which program will execute.
    if let Some(exec) = map
        .get_mut(Value::from("exec"))
        .and_then(Value::as_mapping_mut)
    {
        if let Some(command) = exec.get_mut(Value::from("command")) {
            if let Some(name) = command.as_str() {
                let file = Path::new(name);
                if !file.is_absolute() && name.contains('/') {
                    let joined: PathBuf = base.join(file).components().collect();
                    let full = joined.canonicalize().unwrap_or(joined);
                    *command = Value::from(full.to_string_lossy().to_string());
                }
            }
        }
    }
}

fn read_limited(path: &Path) -> Result<Vec<u8>, ()> {
    if !path.is_file() || fs::metadata(path).map_err(|_| ())?.len() > MAX_CONFIG as u64 {
        return Err(());
    }
    let mut reader = std::io::Read::take(
        fs::File::open(path).map_err(|_| ())?,
        (MAX_CONFIG + 1) as u64,
    );
    let mut bytes = vec![];
    std::io::Read::read_to_end(&mut reader, &mut bytes).map_err(|_| ())?;
    if bytes.len() > MAX_CONFIG {
        Err(())
    } else {
        Ok(bytes)
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|_| ())?;
    if file.write_all(bytes).is_err() {
        let _ = fs::remove_file(path);
        return Err(());
    }
    Ok(())
}

pub fn k3s_target_at(path: &Path) -> Result<CommandTarget, String> {
    // Caller distinguishes an absent config (fresh install) from any present
    // but invalid config (fail closed). Never fall back to the user's context.
    let bytes = read_limited(&path).map_err(|_| "k3s kubeconfig is unavailable")?;
    let doc: Value = serde_yaml::from_slice(&bytes).map_err(|_| "k3s kubeconfig is invalid")?;
    let root = doc.as_mapping().ok_or("k3s kubeconfig is invalid")?;
    let context = string(root, "current-context")
        .filter(|v| !v.is_empty())
        .ok_or("k3s kubeconfig has no context")?;
    let selected = entry(root, "contexts", "context", context).ok_or("k3s context is invalid")?;
    let selected_map = selected.as_mapping().ok_or("k3s context is invalid")?;
    let cluster_name = string(selected_map, "cluster")
        .ok_or("k3s context is invalid")?
        .to_string();
    let user_name = string(selected_map, "user")
        .ok_or("k3s context is invalid")?
        .to_string();
    let mut cluster =
        entry(root, "clusters", "cluster", &cluster_name).ok_or("k3s cluster is invalid")?;
    let mut user = entry(root, "users", "user", &user_name).ok_or("k3s credentials are invalid")?;
    let cluster_map = cluster.as_mapping().ok_or("k3s cluster is invalid")?;
    let server = string(cluster_map, "server").ok_or("k3s cluster is invalid")?;
    // Literal loopback only. DNS names (including localhost) and interface
    // addresses are deliberately excluded: resolution and routing may change.
    let local = server
        .strip_prefix("https://")
        .and_then(|authority| {
            if authority.starts_with('[') {
                let (host, port) = authority.split_once("]:")?;
                (host == "[::1" && valid_api_port(port)).then_some(())
            } else {
                let (host, port) = authority.split_once(':')?;
                (host == "127.0.0.1" && valid_api_port(port)).then_some(())
            }
        })
        .is_some();
    if !local
        || !only_keys(
            cluster_map,
            &[
                "server",
                "certificate-authority-data",
                "certificate-authority",
                "tls-server-name",
            ],
        )
    {
        return Err("k3s kubeconfig must point to a local loopback API".into());
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|_| "k3s kubeconfig is unavailable")?
            .join(path)
    };
    let base = absolute.parent().ok_or("k3s kubeconfig is invalid")?;
    absolutize_local(&mut cluster, base);
    absolutize_local(&mut user, base);
    let mut snapshot = Mapping::new();
    snapshot.insert(Value::from("apiVersion"), Value::from("v1"));
    snapshot.insert(Value::from("kind"), Value::from("Config"));
    snapshot.insert(Value::from("current-context"), Value::from(context));
    for (field, name, key, value) in [
        ("contexts", context, "context", selected),
        ("clusters", cluster_name.as_str(), "cluster", cluster),
        ("users", user_name.as_str(), "user", user),
    ] {
        let mut entry = Mapping::new();
        entry.insert(Value::from("name"), Value::from(name));
        entry.insert(Value::from(key), value);
        snapshot.insert(
            Value::from(field),
            Value::Sequence(vec![Value::Mapping(entry)]),
        );
    }
    let content = serde_yaml::to_string(&snapshot).map_err(|_| "k3s kubeconfig is invalid")?;
    if content.len() > MAX_CONFIG {
        return Err("k3s kubeconfig is too large".into());
    }
    let dir = Arc::new(PrivateK3sSnapshot {
        dir: private_dir()?,
    });
    let private_path = dir.dir.join("k3s.yaml");
    write_private(&private_path, content.as_bytes())
        .map_err(|_| "could not write private k3s kubeconfig")?;
    Ok(CommandTarget {
        kubeconfig: private_path,
        context: context.to_string(),
        _snapshot: Some(dir),
    })
}

fn valid_api_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|p| p > 0)
}

fn entry(root: &Mapping, list: &str, field: &str, name: &str) -> Option<Value> {
    let entries = get(root, list)?.as_sequence()?;
    let mut match_value = None;
    for item in entries {
        let item = item.as_mapping()?;
        if string(item, "name") == Some(name) {
            if match_value.is_some() {
                return None;
            }
            match_value = Some(get(item, field)?.clone());
        }
    }
    match_value
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "kind: Config\napiVersion: v1\ncurrent-context: one\nclusters:\n- name: c\n  cluster:\n    server: https://example.invalid\n    certificate-authority-data: Y2E=\ncontexts:\n- name: one\n  context: {cluster: c, user: u}\nusers:\n- name: u\n  user: {token: private-token}\n";

    #[test]
    fn uploads_create_private_snapshots_and_never_return_credentials() {
        let mut registry = Registry::new().unwrap();
        let dir = registry.dir.clone();
        let result = registry.upload(VALID).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, "upload");
        assert_eq!(result[0].context, "one");
        assert!(registry.get(&result[0].id).is_some());
        assert!(registry.get("unknown").is_none());
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("private-token"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&registry.get(&result[0].id).unwrap().kubeconfig)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(registry);
        assert!(!dir.exists());
    }

    #[test]
    fn untrusted_upload_rejects_plugins_file_paths_proxy_and_insecure_connections() {
        for (old, new) in [
            ("token: private-token", "exec: {command: sh}"),
            ("token: private-token", "auth-provider: {name: gcp}"),
            ("token: private-token", "tokenFile: /etc/passwd"),
            ("token: private-token", "client-key: /etc/passwd"),
            (
                "certificate-authority-data: Y2E=",
                "certificate-authority: /etc/passwd",
            ),
            (
                "certificate-authority-data: Y2E=",
                "proxy-url: https://example.invalid",
            ),
            (
                "certificate-authority-data: Y2E=",
                "insecure-skip-tls-verify: true",
            ),
            ("https://example.invalid", "http://example.invalid"),
        ] {
            let mut registry = Registry::new().unwrap();
            assert_eq!(
                registry.upload(&VALID.replace(old, new)).unwrap_err(),
                "Invalid or unsupported kubeconfig"
            );
            assert!(registry.choices().is_empty());
        }
    }

    #[test]
    fn bounds_and_duplicate_names_fail_closed() {
        let mut registry = Registry::new().unwrap();
        assert!(registry.upload(&"x".repeat(MAX_CONFIG + 1)).is_err());
        assert!(registry
            .upload(&VALID.replace(
                "- name: one",
                "- name: one\n  context: {cluster: c, user: u}\n- name: one"
            ))
            .is_err());
        for _ in 0..MAX_UPLOADS {
            registry.upload(VALID).unwrap();
        }
        assert!(registry.upload(VALID).is_err());
    }

    #[test]
    fn ordered_local_merge_keeps_first_named_values_and_resolves_relative_files() {
        let mut registry = Registry::new().unwrap();
        let first = registry.dir.join("first.yaml");
        let second = registry.dir.join("second.yaml");
        fs::write(
            &first,
            VALID.replace(
                "certificate-authority-data: Y2E=",
                "certificate-authority: ca.pem",
            ),
        )
        .unwrap();
        fs::write(
            &second,
            VALID.replace("https://example.invalid", "https://wrong.invalid"),
        )
        .unwrap();
        registry.load_files(&[first, second], "env", true);
        let chosen = registry.get(&registry.choices()[0].id).unwrap();
        let snapshot = fs::read_to_string(chosen.kubeconfig).unwrap();
        assert!(snapshot.contains("https://example.invalid"));
        assert!(!snapshot.contains("https://wrong.invalid"));
        assert!(snapshot.contains(&registry.dir.join("ca.pem").to_string_lossy().to_string()));
        assert!(registry.choices()[0].current);
    }

    fn local_k3s() -> String {
        VALID.replace("https://example.invalid", "https://127.0.0.1:6443")
    }

    #[test]
    fn k3s_snapshot_pins_local_endpoint_and_cleans_up_after_last_clone() {
        let registry = Registry::new().unwrap();
        let path = registry.dir.join("k3s.yaml");
        fs::write(&path, local_k3s()).unwrap();
        let target = k3s_target_at(&path).unwrap();
        let directory = target.kubeconfig.parent().unwrap().to_path_buf();
        assert_ne!(target.kubeconfig, path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&target.kubeconfig)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let copy = target.clone();
        fs::write(
            &path,
            local_k3s().replace("127.0.0.1:6443", "remote.invalid:6443"),
        )
        .unwrap();
        assert!(fs::read_to_string(&target.kubeconfig)
            .unwrap()
            .contains("127.0.0.1:6443"));
        assert!(k3s_target_at(&path).is_err());
        drop(target);
        assert!(directory.exists());
        drop(copy);
        assert!(!directory.exists());
    }

    #[test]
    fn k3s_endpoint_must_be_literal_loopback_without_proxy_or_insecure_tls() {
        let registry = Registry::new().unwrap();
        let path = registry.dir.join("k3s.yaml");
        for address in [
            "https://localhost:6443",
            "https://192.168.1.2:6443",
            "https://remote.invalid:6443",
            "https://127.0.0.1:6443@remote.invalid",
            "http://127.0.0.1:6443",
            "https://127.0.0.1:0",
        ] {
            fs::write(
                &path,
                local_k3s().replace("https://127.0.0.1:6443", address),
            )
            .unwrap();
            assert!(k3s_target_at(&path).is_err(), "accepted {address}");
        }
        for key in [
            "proxy-url: https://remote.invalid",
            "insecure-skip-tls-verify: true",
        ] {
            fs::write(
                &path,
                local_k3s().replace(
                    "server: https://127.0.0.1:6443",
                    &format!("server: https://127.0.0.1:6443\n    {key}"),
                ),
            )
            .unwrap();
            assert!(k3s_target_at(&path).is_err());
        }
        fs::write(&path, local_k3s().replace("127.0.0.1:6443", "[::1]:6443")).unwrap();
        assert!(k3s_target_at(&path).is_ok());
    }

    #[test]
    fn local_relative_exec_plugin_uses_original_config_dir_bare_name_uses_path() {
        let mut registry = Registry::new().unwrap();
        let first = registry.dir.join("local.yaml");
        fs::write(
            &first,
            VALID.replace("token: private-token", "exec: {command: ./plugins/login}"),
        )
        .unwrap();
        registry.load_files(&[first], "local", false);
        let selected = registry.get(&registry.choices()[0].id).unwrap();
        let snapshot = fs::read_to_string(selected.kubeconfig).unwrap();
        assert!(snapshot.contains(
            &registry
                .dir
                .join("plugins/login")
                .to_string_lossy()
                .to_string()
        ));

        let second = registry.dir.join("bare.yaml");
        fs::write(
            &second,
            VALID.replace("token: private-token", "exec: {command: kubelogin}"),
        )
        .unwrap();
        registry.load_files(&[second], "local", false);
        let selected = registry.get(&registry.choices()[1].id).unwrap();
        let snapshot = fs::read_to_string(selected.kubeconfig).unwrap();
        assert!(snapshot.contains("command: kubelogin"));
    }
}
