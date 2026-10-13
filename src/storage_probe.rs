//! One-shot, selected-cluster NFSv4.1 mount and UID 1000 write verification.
//! All Kubernetes objects have fresh names and are created, never applied.
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::checks;

const IMAGE: &str = "docker.io/library/python:3.12-alpine@sha256:1b668429b3511ab407d8e00648891631b0b1a4d7e15e3ca70f38ab5b91ad4ab4";
const TIMEOUT: Duration = Duration::from_secs(120);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(35);
pub const VALIDITY: Duration = Duration::from_secs(300);

// The script accepts no user-controlled shell or Python source. All directory
// operations are relative to an O_NOFOLLOW fd on the mounted export.
const SCRIPT: &str = r#"import os, sys, signal
signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
phase = 'open'
try:
    root = os.open('/data', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
except OSError as failure:
    print('PROBE_ERROR:open:' + str(failure.errno), flush=True)
    raise
name = sys.argv[1]
made = False
folder = None
file_id = None
try:
    phase = 'mkdir'
    os.mkdir(name, 0o700, dir_fd=root)
    made = True
    folder = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root)
    directory_id = os.fstat(folder).st_ino
    phase = 'create'
    fd = os.open('probe', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=folder)
    file_id = os.fstat(fd).st_ino
    phase = 'write'
    with os.fdopen(fd, 'wb') as out:
        out.write(b'kubarr-nfs-v4.1-check\n')
        out.flush()
        os.fsync(out.fileno())
    phase = 'read'
    fd = os.open('probe', os.O_RDONLY | os.O_NOFOLLOW, dir_fd=folder)
    with os.fdopen(fd, 'rb') as inp:
        if inp.read(128) != b'kubarr-nfs-v4.1-check\n':
            raise ValueError('readback mismatch')
    print('READ_WRITE_OK', flush=True)
except OSError as failure:
    print('PROBE_ERROR:' + phase + ':' + str(failure.errno), flush=True)
    raise
except ValueError:
    print('PROBE_ERROR:' + phase + ':0', flush=True)
    raise
finally:
    try:
        if made:
            if folder is None:
                folder = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root)
                directory_id = os.fstat(folder).st_ino
            if os.stat(name, dir_fd=root, follow_symlinks=False).st_ino != directory_id:
                raise ValueError('probe directory changed')
            if file_id is not None:
                if os.stat('probe', dir_fd=folder, follow_symlinks=False).st_ino != file_id:
                    raise ValueError('probe file changed')
                os.unlink('probe', dir_fd=folder)
            os.rmdir(name, dir_fd=root)
            os.close(folder)
        os.close(root)
        print('CLEANUP_OK', flush=True)
    except OSError as failure:
        print('PROBE_ERROR:cleanup:' + str(failure.errno), flush=True)
        raise
    except ValueError:
        print('PROBE_ERROR:cleanup:0', flush=True)
        raise
"#;

fn probe_error(logs: &str) -> Option<&'static str> {
    for line in logs
        .lines()
        .filter_map(|line| line.strip_prefix("PROBE_ERROR:"))
    {
        let Some((phase, code)) = line.split_once(':') else {
            continue;
        };
        let step = match phase {
            "open" => "opening the mount",
            "mkdir" => "creating a test directory",
            "create" => "creating the exclusive test file",
            "write" => "writing or syncing the test file",
            "read" => "reading the test file",
            "cleanup" => "removing the test directory",
            _ => continue,
        };
        return Some(match (step, code) {
            (_, "13" | "1") => "NFS probe was denied filesystem access as UID/GID 1000. Check export permissions and UID mapping.",
            (_, "30") => "NFS export is read-only from this node. Check export mount and server policy.",
            (_, "28") => "NFS export ran out of space during the file check.",
            (_, "95") => "NFS export does not support an operation needed by the file check.",
            ("opening the mount", _) => "Could not open the mounted NFS directory in the test Pod.",
            ("creating a test directory", _) => "Could not create a unique test directory in the NFS export.",
            ("creating the exclusive test file", _) => "Could not create an exclusive test file in the NFS export.",
            ("writing or syncing the test file", _) => "Could not write or sync the NFS test file.",
            ("reading the test file", _) => "Could not read back the NFS test file.",
            ("removing the test directory", _) => "Could not remove the test directory from the NFS export.",
            _ => continue,
        });
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub server: String,
    pub path: String,
}

pub fn canonical(server: &str, path: &str) -> Result<Input, &'static str> {
    let server = server.trim();
    let path = path.trim();
    if server.is_empty()
        || server.len() > 255
        || !server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:-".contains(&b))
    {
        return Err("Enter a valid NFS server hostname or address.");
    }
    if path.is_empty()
        || path.len() > 255
        || !path.starts_with('/')
        || path.contains([',', '\\', '{', '}', '\r', '\n', '\0'])
    {
        return Err("Enter an absolute NFS export path without commas, backslashes, or braces.");
    }
    // A trailing slash is equivalent to the same export without it.
    let path = if path.chars().all(|ch| ch == '/') {
        "/"
    } else {
        path.trim_end_matches('/')
    };
    if path.is_empty()
        || (path != "/"
            && path[1..]
                .split('/')
                .any(|part| part == "." || part == ".." || part.is_empty()))
    {
        return Err(
            "Enter a canonical absolute NFS export path without empty or relative segments.",
        );
    }
    Ok(Input {
        server: server.to_string(),
        path: path.to_string(),
    })
}

#[derive(Clone, Serialize)]
pub struct Stage {
    pub id: &'static str,
    pub label: &'static str,
    pub severity: &'static str,
    pub state: &'static str,
    pub message: String,
}

pub fn stages() -> Vec<Stage> {
    [
        ("node", "Select a Ready Linux node"),
        ("resources", "Create temporary NFSv4.1 volume"),
        ("mount", "Mount NFS on the selected node"),
        ("write", "Read and write as UID/GID 1000"),
        ("cleanup", "Remove temporary test resources"),
    ]
    .into_iter()
    .map(|(id, label)| Stage {
        id,
        label,
        severity: "required",
        state: "pending",
        message: String::new(),
    })
    .collect()
}

pub fn eligible(stages: &[Stage]) -> bool {
    stages.len() == 5 && stages.iter().all(|item| item.state == "passed")
}

fn random() -> Result<String, &'static str> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "Could not create a unique storage check name")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct Names {
    ns: String,
    pv: String,
    pvc: String,
    pod: String,
    folder: String,
    marker: String,
}

const RESOURCE_COUNT: usize = 4;
type CreatedUids = [Option<String>; RESOURCE_COUNT];

fn identity(value: &Value, names: &Names, kind: &str, name: &str, ns: Option<&str>) -> bool {
    value.pointer("/kind").and_then(Value::as_str) == Some(kind)
        && value.pointer("/metadata/name").and_then(Value::as_str) == Some(name)
        && (ns.is_none() || value.pointer("/metadata/namespace").and_then(Value::as_str) == ns)
        && value
            .pointer("/metadata/labels/app.kubernetes.io~1managed-by")
            .and_then(Value::as_str)
            == Some("kubarr-cli")
        && value
            .pointer("/metadata/labels/kubarr.io~1storage-check")
            .and_then(Value::as_str)
            == Some(names.marker.as_str())
}

fn uid(value: &Value) -> Option<&str> {
    value
        .pointer("/metadata/uid")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

fn created_uid(value: &Value, names: &Names, manifest: &Value) -> Option<String> {
    let kind = manifest["kind"].as_str()?;
    let name = manifest.pointer("/metadata/name")?.as_str()?;
    let ns = manifest
        .pointer("/metadata/namespace")
        .and_then(Value::as_str);
    identity(value, names, kind, name, ns)
        .then(|| uid(value).map(str::to_owned))
        .flatten()
}

// A timed-out create may have succeeded on the API after its client exited.
// Only adopt our exact, randomly named, tagged resource with a freshly read UID
// and the original critical spec. Failure to prove identity leaves it alone.
fn adoptable(value: &Value, names: &Names, manifest: &Value) -> bool {
    if created_uid(value, names, manifest).is_none()
        || value
            .pointer("/metadata/creationTimestamp")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return false;
    }
    let paths: &[&str] = match manifest["kind"].as_str() {
        Some("Namespace") => &[],
        Some("PersistentVolume") => &[
            "/spec/nfs",
            "/spec/mountOptions",
            "/spec/persistentVolumeReclaimPolicy",
            "/spec/storageClassName",
        ],
        Some("PersistentVolumeClaim") => &[
            "/spec/volumeName",
            "/spec/storageClassName",
            "/spec/accessModes",
        ],
        Some("Pod") => &[
            "/spec/nodeName",
            "/spec/automountServiceAccountToken",
            "/spec/securityContext",
            "/spec/containers/0/image",
            "/spec/containers/0/command",
            "/spec/volumes/0/persistentVolumeClaim",
        ],
        _ => return false,
    };
    paths.iter().all(|path| {
        manifest.pointer(path).is_some() && value.pointer(path) == manifest.pointer(path)
    })
}

impl Names {
    fn new() -> Result<Self, &'static str> {
        let id = random()?;
        Ok(Self {
            ns: format!("kubarr-nfs-check-{id}"),
            pv: format!("kubarr-nfs-check-{id}"),
            pvc: "check".into(),
            pod: "check".into(),
            folder: format!(".kubarr-check-{id}"),
            marker: id,
        })
    }
    fn label(&self) -> Value {
        json!({"app.kubernetes.io/managed-by":"kubarr-cli","kubarr.io/storage-check":self.marker})
    }
    fn manifests(&self, input: &Input, node: &str) -> [Value; 4] {
        let labels = self.label();
        let mut namespace_labels = labels.clone();
        namespace_labels["pod-security.kubernetes.io/enforce"] = json!("restricted");
        [
            json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":self.ns,"labels":namespace_labels}}),
            json!({"apiVersion":"v1","kind":"PersistentVolume","metadata":{"name":self.pv,"labels":labels},"spec":{
                "capacity":{"storage":"1Mi"},"accessModes":["ReadWriteMany"],"persistentVolumeReclaimPolicy":"Retain",
                "storageClassName":"","mountOptions":["vers=4.1"],"nfs":{"server":input.server,"path":input.path}}}),
            json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":self.pvc,"namespace":self.ns,"labels":labels},"spec":{
                "accessModes":["ReadWriteMany"],"volumeName":self.pv,"storageClassName":"","resources":{"requests":{"storage":"1Mi"}}}}),
            json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":self.pod,"namespace":self.ns,"labels":labels},"spec":{
                "restartPolicy":"Never","automountServiceAccountToken":false,"activeDeadlineSeconds":70,
                "nodeName":node,"securityContext":{"runAsNonRoot":true,"runAsUser":1000,"runAsGroup":1000,"seccompProfile":{"type":"RuntimeDefault"}},
                "containers":[{"name":"check","image":IMAGE,"imagePullPolicy":"IfNotPresent","command":["python3","-c",SCRIPT,self.folder],
                    "resources":{"requests":{"cpu":"10m","memory":"32Mi"},"limits":{"cpu":"200m","memory":"128Mi"}},
                    "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}},
                    "volumeMounts":[{"name":"nfs","mountPath":"/data"}]}],
                "volumes":[{"name":"nfs","persistentVolumeClaim":{"claimName":self.pvc}}]}}),
        ]
    }
}

// A private, short-lived manifest file avoids blocking on a piped stdin if a
// kubectl authentication plugin does not consume it. Never expose raw output.
fn kubectl(
    args: &[&str],
    manifest: Option<&Value>,
    cancelled: &AtomicBool,
    end: Instant,
) -> Option<checks::ProbeResult> {
    #[cfg(test)]
    if let Some(result) =
        MOCK.with(|mock| mock.borrow().as_ref().map(|runner| runner(args, manifest)))
    {
        return result;
    }
    if let Some(manifest) = manifest {
        let name = random().ok()?;
        let path = std::env::temp_dir().join(format!("kubarr-nfs-manifest-{name}.json"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).ok()?;
        let result = (|| {
            file.write_all(serde_json::to_string(manifest).ok()?.as_bytes())
                .ok()?;
            drop(file);
            let filename = path.to_str()?;
            let mut command = if args.is_empty() {
                vec!["create", "-o", "json"]
            } else {
                args.to_vec()
            };
            command.extend(["-f", filename]);
            checks::probe("kubectl", &command, cancelled, end)
        })();
        let _ = fs::remove_file(path);
        result
    } else {
        checks::probe("kubectl", args, cancelled, end)
    }
}

#[cfg(test)]
thread_local! {
    static MOCK: std::cell::RefCell<Option<Box<dyn Fn(&[&str], Option<&Value>) -> Option<checks::ProbeResult>>>> = std::cell::RefCell::new(None);
}

fn get(
    kind: &str,
    name: &str,
    ns: Option<&str>,
    cancelled: &AtomicBool,
    end: Instant,
) -> Option<Value> {
    let mut args = vec!["get", kind, name];
    if let Some(ns) = ns {
        args.extend(["-n", ns]);
    }
    args.extend(["-o", "json"]);
    let out = kubectl(&args, None, cancelled, end)?;
    out.success
        .then(|| serde_json::from_str(&out.output).ok())
        .flatten()
}

fn ready_node(cancelled: &AtomicBool, end: Instant) -> Option<String> {
    let out = kubectl(&["get", "nodes", "-o", "json"], None, cancelled, end)?;
    let data: Value = serde_json::from_str(&out.output).ok()?;
    data.get("items")?.as_array()?.iter().find_map(|node| {
        (node
            .pointer("/metadata/labels/kubernetes.io~1os")?
            .as_str()?
            == "linux"
            && node.pointer("/spec/unschedulable") != Some(&json!(true))
            && node
                .pointer("/status/conditions")?
                .as_array()?
                .iter()
                .any(|condition| condition["type"] == "Ready" && condition["status"] == "True"))
        .then(|| node.pointer("/metadata/name")?.as_str().map(str::to_string))
        .flatten()
    })
}

fn remove(
    kind: &str,
    name: &str,
    ns: Option<&str>,
    names: &Names,
    expected: Option<&str>,
    end: Instant,
) -> bool {
    // The DELETE itself carries the UID precondition. A label check before an
    // ordinary name-based delete cannot prevent a replacement race.
    let stop = AtomicBool::new(false);
    let resource = match kind {
        "Pod" => "pod",
        "PersistentVolumeClaim" => "pvc",
        "PersistentVolume" => "pv",
        "Namespace" => "namespace",
        _ => return false,
    };
    let mut query = vec!["get", resource, name, "--ignore-not-found", "-o", "json"];
    if let Some(ns) = ns {
        query.extend(["-n", ns]);
    }
    let Some(found) = kubectl(&query, None, &stop, end).filter(|out| out.success) else {
        return false;
    };
    // A create request can time out before the API actually accepts it.
    // A successful, empty lookup confirms there is nothing to remove.
    if found.output.trim().is_empty() {
        return expected.is_none();
    }
    let Ok(value) = serde_json::from_str::<Value>(&found.output) else {
        return false;
    };
    if !identity(&value, names, kind, name, ns) || uid(&value) != expected {
        return false;
    }
    let uri = match (kind, ns) {
        ("Pod", Some(namespace)) => format!("/api/v1/namespaces/{namespace}/pods/{name}"),
        ("PersistentVolumeClaim", Some(namespace)) => {
            format!("/api/v1/namespaces/{namespace}/persistentvolumeclaims/{name}")
        }
        ("Namespace", None) => format!("/api/v1/namespaces/{name}"),
        ("PersistentVolume", None) => format!("/api/v1/persistentvolumes/{name}"),
        _ => return false,
    };
    let options =
        json!({"apiVersion":"v1","kind":"DeleteOptions","preconditions":{"uid":expected.unwrap()}});
    if !kubectl(&["delete", "--raw", &uri], Some(&options), &stop, end)
        .is_some_and(|out| out.success)
    {
        return false;
    }
    while Instant::now() < end {
        match kubectl(&query, None, &stop, end) {
            Some(out) if out.success && out.output.trim().is_empty() => return true,
            Some(out) if out.success => {
                // A replacement with the same name cannot satisfy our UID.
                if !serde_json::from_str::<Value>(&out.output)
                    .is_ok_and(|v| identity(&v, names, kind, name, ns) && uid(&v) == expected)
                {
                    return false;
                }
            }
            _ => return false,
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

fn cleanup(names: &Names, attempted: usize, uids: &CreatedUids, end: Instant) -> bool {
    // If creation timed out, the server may have created the object anyway.
    // Inspect every attempted name; never guess that a failed create is absent.
    let mut ok = true;
    if attempted >= 4 {
        ok = remove(
            "Pod",
            &names.pod,
            Some(&names.ns),
            names,
            uids[3].as_deref(),
            end,
        ) && ok;
    }
    if attempted >= 3 {
        ok = remove(
            "PersistentVolumeClaim",
            &names.pvc,
            Some(&names.ns),
            names,
            uids[2].as_deref(),
            end,
        ) && ok;
    }
    // A changed/unknown dependent is a reason to leave the namespace for
    // inspection rather than cascade-delete someone else's replacement.
    if attempted >= 1 && ok {
        let stop = AtomicBool::new(false);
        let empty = kubectl(
            &["get", "pods,pvc", "-n", &names.ns, "-o", "json"],
            None,
            &stop,
            end,
        )
        .filter(|out| out.success)
        .and_then(|out| serde_json::from_str::<Value>(&out.output).ok())
        .and_then(|list| list.get("items")?.as_array().map(Vec::is_empty))
        .unwrap_or(false);
        // Do not cascade-delete an unexpectedly present Pod or PVC.
        ok = empty && ok;
    }
    if attempted >= 1 && ok {
        ok = remove("Namespace", &names.ns, None, names, uids[0].as_deref(), end) && ok;
    }
    if attempted >= 2 {
        ok = remove(
            "PersistentVolume",
            &names.pv,
            None,
            names,
            uids[1].as_deref(),
            end,
        ) && ok;
    }
    ok
}

// On an unexpected unwind, still attempt to remove every object whose create
// request was sent. Ordinary error and cancellation paths perform and report
// the same cleanup explicitly below.
struct CleanupGuard<'a> {
    names: &'a Names,
    attempted: usize,
    uids: CreatedUids,
    armed: bool,
}

impl Drop for CleanupGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if !cleanup(
                self.names,
                self.attempted,
                &self.uids,
                Instant::now() + CLEANUP_TIMEOUT,
            ) {
                eprintln!("External NFS check interrupted; cleanup unconfirmed for namespace {}, PV {}, possible export directory {}.",
                    self.names.ns, self.names.pv, self.names.folder);
            }
        }
    }
}

fn publish_stage(
    publish: &mut dyn FnMut(usize, &'static str, String),
    index: usize,
    state: &'static str,
    message: impl Into<String>,
) {
    publish(index, state, message.into());
}

/// A completed check requires both script-level removal and Kubernetes cleanup.
/// Curated errors only: never forward kubectl output, Pod logs or NFS content.
pub fn run(
    input: &Input,
    cancelled: &AtomicBool,
    publish: &mut dyn FnMut(usize, &'static str, String),
) -> Result<(), String> {
    let names = Names::new().map_err(str::to_string)?;
    let end = Instant::now() + TIMEOUT;
    let mut guard = CleanupGuard {
        names: &names,
        attempted: 0,
        uids: std::array::from_fn(|_| None),
        armed: true,
    };
    let mut uncertain_create = false;
    let mut file_confirmed = false;
    publish_stage(publish, 0, "running", "Selecting a Ready Linux node…");
    let node = ready_node(cancelled, end);
    let chosen_node = node.clone().unwrap_or_default();
    if node.is_none() {
        publish_stage(
            publish,
            0,
            "failed",
            "No Ready Linux node was available or node discovery failed.",
        );
    }
    let result = if let Some(node) = node {
        publish_stage(
            publish,
            0,
            "passed",
            format!("Testing on node {node}. This does not test other nodes."),
        );
        (|| {
            let manifests = names.manifests(input, &node);
            publish_stage(
                publish,
                1,
                "running",
                "Creating isolated NFSv4.1 test resources…",
            );
            for (index, manifest) in manifests.iter().enumerate() {
                guard.attempted = index + 1;
                let created = kubectl(&[], Some(manifest), cancelled, end);
                uncertain_create |= created.is_none();
                if let Some(out) = created.as_ref().filter(|out| out.success) {
                    guard.uids[index] = serde_json::from_str::<Value>(&out.output)
                        .ok()
                        .and_then(|value| created_uid(&value, &names, manifest));
                }
                if !created.is_some_and(|out| out.success) || guard.uids[index].is_none() {
                    publish_stage(publish, 1, "failed", "Could not create the temporary namespace, volume or Pod. Check RBAC, admission and quota.");
                    return Err("Could not create temporary storage check resources".to_string());
                }
                if index == 2 {
                    publish_stage(publish, 1, "passed", "Temporary volume and claim created.");
                    publish_stage(
                        publish,
                        2,
                        "running",
                        "Waiting for the node to mount the NFSv4.1 export…",
                    );
                }
            }
            loop {
                if cancelled.load(Ordering::Acquire) || Instant::now() >= end {
                    publish_stage(publish, 2, "failed", "Mount check cancelled or timed out. Check NFSv4.1 exports, node connectivity and mount events.");
                    return Err("NFS mount check cancelled or timed out".into());
                }
                let Some(pod) = get("pod", &names.pod, Some(&names.ns), cancelled, end) else {
                    publish_stage(
                        publish,
                        2,
                        "failed",
                        "Could not read the temporary Pod status.",
                    );
                    return Err("Could not read NFS check Pod".into());
                };
                if !identity(&pod, &names, "Pod", &names.pod, Some(&names.ns))
                    || uid(&pod) != guard.uids[3].as_deref()
                {
                    publish_stage(
                        publish,
                        2,
                        "failed",
                        "The temporary Pod identity changed. Leave it for inspection.",
                    );
                    return Err("Temporary NFS Pod identity changed".into());
                }
                let phase = pod.pointer("/status/phase").and_then(Value::as_str);
                let terminated = pod.pointer("/status/containerStatuses/0/state/terminated");
                // Kubernetes may show an exited container with exitCode 0 while
                // Pod phase is still Running. Do not fail an otherwise healthy
                // NFS test before the Pod controller marks it Succeeded.
                if phase == Some("Succeeded")
                    || terminated
                        .and_then(|status| status.get("exitCode"))
                        .and_then(Value::as_i64)
                        .is_some_and(|code| code != 0)
                {
                    publish_stage(publish, 2, "passed", format!("NFS mounted on node {node}."));
                    publish_stage(
                        publish,
                        3,
                        "running",
                        "Confirming UID/GID 1000 write and readback…",
                    );
                    let logs = kubectl(
                        &["logs", &names.pod, "-n", &names.ns, "--limit-bytes=4096"],
                        None,
                        cancelled,
                        end,
                    );
                    let reason = match logs.as_ref() {
                        None => "Could not retrieve the test Pod logs before its deadline.",
                        Some(out) if !out.success => "The test Pod logs could not be read. Check namespace Pod log permission.",
                        Some(out) => probe_error(&out.output).unwrap_or_else(|| {
                            match (
                                out.output.lines().any(|v| v == "READ_WRITE_OK"),
                                out.output.lines().any(|v| v == "CLEANUP_OK"),
                                pod.pointer("/status/phase").and_then(Value::as_str),
                            ) {
                                (true, false, _) => "NFS read/write succeeded, but test directory removal was not confirmed.",
                                (false, true, _) => "The test directory was removed, but NFS read/write was not confirmed.",
                                (false, false, Some("Failed")) => "The test Pod exited unsuccessfully before confirming the NFS file test.",
                                (false, false, _) => "The test Pod produced no verified NFS file-test markers.",
                                _ => "The file-test markers appeared, but the Pod did not finish successfully.",
                            }
                        }),
                    };
                    let success = pod.pointer("/status/phase").and_then(Value::as_str)
                        == Some("Succeeded")
                        && logs.is_some_and(|out| {
                            out.success
                                && out.output.lines().any(|v| v == "READ_WRITE_OK")
                                && out.output.lines().any(|v| v == "CLEANUP_OK")
                        });
                    file_confirmed = success;
                    publish_stage(
                        publish,
                        3,
                        if success { "passed" } else { "failed" },
                        if success {
                            "Exclusive file written, read back, and removed as UID/GID 1000."
                                .to_string()
                        } else {
                            format!("{reason} Possible leftover {}.", names.folder)
                        },
                    );
                    return if success {
                        Ok(())
                    } else {
                        Err("NFS read/write or file cleanup failed".into())
                    };
                }
                if pod.pointer("/status/phase").and_then(Value::as_str) == Some("Failed") {
                    publish_stage(publish, 2, "failed", "Test Pod failed. Check node NFSv4.1 mount events, image availability and export permissions.");
                    return Err("NFS mount or test Pod failed".into());
                }
                if pod
                    .pointer("/status/containerStatuses/0/state/waiting/reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| {
                        matches!(
                            reason,
                            "ImagePullBackOff" | "ErrImagePull" | "CreateContainerConfigError"
                        )
                    })
                {
                    publish_stage(
                        publish,
                        2,
                        "failed",
                        "Test image could not start. Check image access and Pod admission.",
                    );
                    return Err("NFS check image could not start".into());
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })()
    } else {
        Err("Could not select a Ready Linux node".into())
    };
    publish_stage(
        publish,
        4,
        "running",
        "Removing temporary Kubernetes resources…",
    );
    // A create that timed out may have succeeded after its client disappeared.
    // Adopt only an exact, tagged object with an observed UID; never delete a
    // same-name object simply because it carries a familiar label.
    let manifests = names.manifests(input, &chosen_node);
    for index in 0..guard.attempted {
        if guard.uids[index].is_none() {
            let (kind, name, ns) = match index {
                0 => ("namespace", names.ns.as_str(), None),
                1 => ("pv", names.pv.as_str(), None),
                2 => ("pvc", names.pvc.as_str(), Some(names.ns.as_str())),
                _ => ("pod", names.pod.as_str(), Some(names.ns.as_str())),
            };
            let stop = AtomicBool::new(false);
            if let Some(value) = get(
                kind,
                name,
                ns,
                &stop,
                Instant::now() + Duration::from_secs(4),
            ) {
                if adoptable(&value, &names, &manifests[index]) {
                    guard.uids[index] = uid(&value).map(str::to_owned);
                }
            }
        }
    }
    if guard.attempted >= 4 && guard.uids[3].is_some() && !file_confirmed {
        // A cancelled request cannot fetch logs with its cancelled token.
        // Observe a terminal Pod once more before removing it; only the fixed
        // script marker can prove the export directory was removed.
        let stop = AtomicBool::new(false);
        let cutoff = Instant::now() + Duration::from_secs(4);
        if let Some(pod) = get("pod", &names.pod, Some(&names.ns), &stop, cutoff) {
            if identity(&pod, &names, "Pod", &names.pod, Some(&names.ns))
                && uid(&pod) == guard.uids[3].as_deref()
                && matches!(
                    pod.pointer("/status/phase").and_then(Value::as_str),
                    Some("Succeeded" | "Failed")
                )
            {
                file_confirmed = kubectl(
                    &["logs", &names.pod, "-n", &names.ns, "--limit-bytes=4096"],
                    None,
                    &stop,
                    cutoff,
                )
                .is_some_and(|out| {
                    out.success && out.output.lines().any(|line| line == "CLEANUP_OK")
                });
            }
        }
    }
    let cleaned = cleanup(
        &names,
        guard.attempted,
        &guard.uids,
        Instant::now() + CLEANUP_TIMEOUT,
    );
    guard.armed = false;
    if !cleaned || uncertain_create || (guard.attempted >= 4 && !file_confirmed) {
        let message = format!("Could not confirm file or Kubernetes cleanup (or a create timed out and may arrive late). Check namespace {}, PV {}, and possible export test directory {}.", names.ns, names.pv, names.folder);
        eprintln!("{message}");
        publish_stage(publish, 4, "failed", message.clone());
        return Err(message);
    }
    publish_stage(
        publish,
        4,
        "passed",
        "Temporary Kubernetes resources removed; export test directory removal confirmed when a Pod was created.",
    );
    result
}

pub fn verify_after_k3s(input: &Input) -> Result<(), String> {
    let mut stages = stages();
    let result = run(input, &AtomicBool::new(false), &mut |i, state, message| {
        stages[i].state = state;
        stages[i].message = message;
    });
    result.and_then(|()| {
        if eligible(&stages) {
            Ok(())
        } else {
            Err("External NFS check did not complete".into())
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub target_id: String,
    pub server: String,
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    thread_local! {
        static LAG_POD_PHASE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn fake_run(
        fail_create: Option<&'static str>,
        fail_delete: bool,
        failed_pod: bool,
        replace_on_delete: Option<&'static str>,
        timeout_create: Option<(&'static str, bool)>,
        cancel_after_pod_create: bool,
    ) -> (Result<(), String>, Vec<Stage>, Vec<String>) {
        let objects = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let saved = objects.clone();
        let recorded = calls.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let lag_pod_phase = LAG_POD_PHASE.with(|flag| flag.get());
        let pod_reads = std::sync::atomic::AtomicUsize::new(0);
        MOCK.with(|mock| *mock.borrow_mut() = Some(Box::new(move |args, manifest| {
            let mut objects = saved.lock().unwrap();
            let success = |output: String| Some(checks::ProbeResult { success: true, output });
            let failure = || Some(checks::ProbeResult { success: false, output: "secret-error-must-not-escape".into() });
            if let Some(manifest) = manifest {
                if args.first() == Some(&"delete") {
                    let uri = args.get(2).copied().unwrap_or("");
                    let kind = if uri.contains("/pods/") { "Pod" }
                        else if uri.contains("/persistentvolumeclaims/") { "PersistentVolumeClaim" }
                        else if uri.contains("/persistentvolumes/") { "PersistentVolume" }
                        else if uri.contains("/namespaces/") { "Namespace" }
                        else { return failure(); };
                    recorded.lock().unwrap().push(format!("delete {kind}"));
                    if fail_delete && kind == "PersistentVolume" { return failure(); }
                    if replace_on_delete == Some(kind) {
                        objects.get_mut(kind).unwrap()["metadata"]["uid"] = json!(format!("replacement-{kind}"));
                    }
                    let expected = manifest["preconditions"]["uid"].as_str().unwrap();
                    if objects[kind]["metadata"]["uid"] != expected { return failure(); }
                    objects.remove(kind);
                    return success(String::new());
                }
                let kind = manifest["kind"].as_str().unwrap();
                recorded.lock().unwrap().push(format!("create {kind}"));
                if fail_create == Some(kind) { return failure(); }
                let mut value = manifest.clone();
                value["metadata"]["uid"] = json!(format!("original-{kind}"));
                value["metadata"]["creationTimestamp"] = json!("2026-10-12T00:00:00Z");
                if timeout_create == Some((kind, true)) {
                    value["spec"]["nodeName"] = json!("unrelated-node");
                }
                objects.insert(kind.into(), value);
                if cancel_after_pod_create && kind == "Pod" {
                    signal.store(true, Ordering::Release);
                }
                if timeout_create.is_some_and(|(timeout, _)| timeout == kind) { return None; }
                return success(objects[kind].to_string());
            }
            let op = args.first().copied().unwrap_or("");
            if args == ["get", "nodes", "-o", "json"] {
                return success(json!({"items":[{"metadata":{"name":"synthetic-node","labels":{"kubernetes.io/os":"linux"}},
                    "status":{"conditions":[{"type":"Ready","status":"True"}]}}]}).to_string());
            }
            if op == "logs" {
                return success(if failed_pod { String::new() } else { "READ_WRITE_OK\nCLEANUP_OK\n".into() });
            }
            if op == "get" && args.get(1) == Some(&"pods,pvc") {
                return success(json!({"items": objects.iter().filter(|(kind, _)| ["Pod", "PersistentVolumeClaim"].contains(&kind.as_str())).map(|(_, value)| value).collect::<Vec<_>>()}).to_string());
            }
            let kind = match args.get(1).copied() {
                Some("pod") => "Pod", Some("pvc") => "PersistentVolumeClaim",
                Some("namespace") => "Namespace", Some("pv") => "PersistentVolume", _ => return failure(),
            };
            if op == "get" {
                if let Some(mut value) = objects.get(kind).cloned() {
                    if kind == "Pod" {
                        value["status"] = if failed_pod { json!({"phase":"Failed"}) }
                            else if lag_pod_phase && pod_reads.fetch_add(1, Ordering::AcqRel) == 0 {
                                json!({"phase":"Running","containerStatuses":[{"state":{"terminated":{"exitCode":0}}}]})
                            } else { json!({"phase":"Succeeded"}) };
                    }
                    return success(value.to_string());
                }
                return if args.contains(&"--ignore-not-found") { success(String::new()) } else { failure() };
            }
            failure()
        })));
        let mut stages = stages();
        let result = run(
            &canonical("192.0.2.11", "/export").unwrap(),
            &cancelled,
            &mut |i, state, message| {
                stages[i].state = state;
                stages[i].message = message;
            },
        );
        MOCK.with(|mock| *mock.borrow_mut() = None);
        let history = calls.lock().unwrap().clone();
        (result, stages, history)
    }

    #[test]
    fn synthetic_kubectl_success_and_failed_create_cleanup_are_ordered() {
        let (result, stages, history) = fake_run(None, false, false, None, None, false);
        assert!(
            result.is_ok(),
            "{result:?} {history:?} {:?}",
            stages
                .iter()
                .map(|s| (s.id, s.state, s.message.as_str()))
                .collect::<Vec<_>>()
        );
        assert!(eligible(&stages));
        assert_eq!(
            history,
            [
                "create Namespace",
                "create PersistentVolume",
                "create PersistentVolumeClaim",
                "create Pod",
                "delete Pod",
                "delete PersistentVolumeClaim",
                "delete Namespace",
                "delete PersistentVolume"
            ]
        );

        let (result, stages, history) = fake_run(
            Some("PersistentVolumeClaim"),
            false,
            false,
            None,
            None,
            false,
        );
        assert!(result.is_err());
        assert!(!eligible(&stages));
        assert_eq!(
            history,
            [
                "create Namespace",
                "create PersistentVolume",
                "create PersistentVolumeClaim",
                "delete Namespace",
                "delete PersistentVolume"
            ]
        );
        assert!(!stages.iter().any(|s| s.message.contains("secret-error")));

        let (result, stages, _) = fake_run(None, true, false, None, None, false);
        assert!(result
            .unwrap_err()
            .contains("Could not confirm file or Kubernetes cleanup"));
        assert_eq!(stages[4].state, "failed");
        let (result, stages, _) = fake_run(None, false, true, None, None, false);
        assert!(result.is_err());
        assert_eq!(stages[2].state, "failed");
        assert_eq!(stages[4].state, "failed");

        let (result, stages, history) = fake_run(None, false, false, Some("Pod"), None, false);
        assert!(result.is_err());
        assert_eq!(stages[4].state, "failed");
        assert!(history.contains(&"delete Pod".to_string()));
        assert!(
            !history.contains(&"delete Namespace".to_string()),
            "replacement must never be cascade deleted"
        );

        let (result, stages, history) =
            fake_run(None, false, false, Some("Namespace"), None, false);
        assert!(result.is_err());
        assert_eq!(stages[4].state, "failed");
        assert!(
            history.contains(&"delete Namespace".to_string()),
            "UID-preconditioned DELETE was attempted"
        );

        let (result, stages, history) =
            fake_run(None, false, false, None, Some(("Pod", false)), false);
        assert!(
            result.is_err(),
            "a timed-out creation cannot pass the check"
        );
        assert_eq!(stages[4].state, "failed");
        assert!(
            history.contains(&"delete Pod".to_string()),
            "matching run-owned Pod was adopted with its observed UID"
        );

        let (result, _, history) = fake_run(None, false, false, None, Some(("Pod", true)), false);
        assert!(result.is_err());
        assert!(
            !history.contains(&"delete Pod".to_string()),
            "changed Pod spec cannot be adopted"
        );
        assert!(!history.contains(&"delete Namespace".to_string()));

        let (result, stages, _) = fake_run(None, false, false, None, None, true);
        assert!(result.is_err(), "cancellation never qualifies as success");
        assert_eq!(
            stages[4].state, "passed",
            "post-cancellation Pod logs confirmed its file cleanup"
        );
        let (result, stages, _) = fake_run(None, false, true, None, None, true);
        assert!(result.is_err());
        assert_eq!(
            stages[4].state, "failed",
            "missing file cleanup marker must fail cleanup"
        );

        LAG_POD_PHASE.with(|flag| flag.set(true));
        let (result, stages, _) = fake_run(None, false, false, None, None, false);
        LAG_POD_PHASE.with(|flag| flag.set(false));
        assert!(
            result.is_ok(),
            "a terminated container precedes a Succeeded Pod: {result:?}"
        );
        assert!(eligible(&stages));
    }

    #[test]
    fn input_normalizes_once_and_rejects_path_traversal() {
        assert_eq!(
            canonical(" 192.168.1.11 ", " /mnt/bertha_hdd/kubarr-data/ ").unwrap(),
            Input {
                server: "192.168.1.11".into(),
                path: "/mnt/bertha_hdd/kubarr-data".into()
            }
        );
        assert_eq!(canonical("nfs.example", "///").unwrap().path, "/");
        for path in [
            "/data//child",
            "/data/../other",
            "/data/./file",
            "relative",
            "/data\nother",
            "/data,other",
            "/data\\other",
        ] {
            assert!(canonical("nfs.example", path).is_err(), "{path:?}");
        }
        for server in ["", "-nfs;rm", "nfs server", "nfs/server", "nfs\nserver"] {
            assert!(canonical(server, "/data").is_err(), "{server:?}");
        }
    }

    #[test]
    fn resources_are_unique_static_restricted_and_only_reference_exact_export() {
        let names = Names::new().unwrap();
        let next = Names::new().unwrap();
        assert_ne!(names.ns, next.ns);
        let input = canonical("192.168.1.11", "/mnt/bertha_hdd/kubarr-data").unwrap();
        let [ns, pv, pvc, pod] = names.manifests(&input, "talos-node");
        assert_eq!(ns["kind"], "Namespace");
        assert_eq!(
            ns.pointer("/metadata/labels/pod-security.kubernetes.io~1enforce"),
            Some(&json!("restricted"))
        );
        assert_eq!(pv.pointer("/spec/nfs/server"), Some(&json!(input.server)));
        assert_eq!(pv.pointer("/spec/nfs/path"), Some(&json!(input.path)));
        assert_eq!(pv.pointer("/spec/mountOptions/0"), Some(&json!("vers=4.1")));
        assert_eq!(
            pv.pointer("/spec/persistentVolumeReclaimPolicy"),
            Some(&json!("Retain"))
        );
        assert_eq!(pvc.pointer("/spec/storageClassName"), Some(&json!("")));
        assert_eq!(pvc.pointer("/spec/volumeName"), Some(&json!(names.pv)));
        assert_eq!(pod.pointer("/spec/nodeName"), Some(&json!("talos-node")));
        assert_eq!(
            pod.pointer("/spec/automountServiceAccountToken"),
            Some(&json!(false))
        );
        assert_eq!(
            pod.pointer("/spec/securityContext/runAsUser"),
            Some(&json!(1000))
        );
        assert_eq!(
            pod.pointer("/spec/securityContext/runAsGroup"),
            Some(&json!(1000))
        );
        assert!(pod.pointer("/spec/securityContext/fsGroup").is_none());
        assert_eq!(
            pod.pointer("/spec/securityContext/seccompProfile/type"),
            Some(&json!("RuntimeDefault"))
        );
        assert_eq!(
            pod.pointer("/spec/containers/0/securityContext/capabilities/drop/0"),
            Some(&json!("ALL"))
        );
        assert_eq!(
            pod.pointer("/spec/containers/0/securityContext/allowPrivilegeEscalation"),
            Some(&json!(false))
        );
        assert!(pod.pointer("/spec/volumes/0/nfs").is_none());
        assert!(pod.pointer("/spec/volumes/0/hostPath").is_none());
        assert!(SCRIPT.contains("os.O_EXCL | os.O_NOFOLLOW"));
        assert!(SCRIPT.contains("os.rmdir(name, dir_fd=root)"));
        assert!(!SCRIPT.contains("rmtree"));
        let mut observed = ns.clone();
        observed["metadata"]["uid"] = json!("synthetic-uid");
        observed["metadata"]["creationTimestamp"] = json!("2026-10-12T00:00:00Z");
        assert!(identity(&observed, &names, "Namespace", &names.ns, None));
        assert_eq!(
            created_uid(&observed, &names, &ns).as_deref(),
            Some("synthetic-uid")
        );
        assert!(adoptable(&observed, &names, &ns));
        observed["metadata"]["name"] = json!("other-namespace");
        assert!(!adoptable(&observed, &names, &ns));
        observed["metadata"]["name"] = json!(names.ns);
        observed["metadata"]["labels"]["kubarr.io/storage-check"] = json!("another-run");
        assert!(!identity(&observed, &names, "Namespace", &names.ns, None));
    }

    #[test]
    fn only_five_passed_stages_are_eligible() {
        let mut checks = stages();
        assert!(!eligible(&checks));
        for stage in &mut checks {
            stage.state = "passed";
        }
        assert!(eligible(&checks));
        checks[4].state = "failed";
        assert!(!eligible(&checks));
    }

    #[test]
    fn probe_failure_reason_is_curated_and_does_not_reflect_logs() {
        assert_eq!(
            probe_error("PROBE_ERROR:write:13 secret-token"),
            Some("Could not write or sync the NFS test file.")
        );
        assert_eq!(probe_error("PROBE_ERROR:write:13"), Some("NFS probe was denied filesystem access as UID/GID 1000. Check export permissions and UID mapping."));
        assert_eq!(
            probe_error("PROBE_ERROR:cleanup:95"),
            Some("NFS export does not support an operation needed by the file check.")
        );
        assert_eq!(probe_error("PROBE_ERROR:unknown:13"), None);
        assert_eq!(
            probe_error("PROBE_ERROR:read:0"),
            Some("Could not read back the NFS test file.")
        );
    }

    #[test]
    fn python_probe_touches_only_its_unique_directory_in_local_fixture() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("kubarr-script-test-{}", random().unwrap()));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("existing.txt"), b"preserve this").unwrap();
        let dir = ".kubarr-check-synthetic";
        let script = SCRIPT.replace("'/data'", &format!("{:?}", root.to_str().unwrap()));
        let result = Command::new("python3")
            .args(["-c", &script, dir])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("CLEANUP_OK"));
        assert!(!root.join(dir).exists());
        assert_eq!(
            fs::read(root.join("existing.txt")).unwrap(),
            b"preserve this"
        );
        let result = Command::new("python3")
            .args(["-c", &script, "existing.txt"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(
            fs::read(root.join("existing.txt")).unwrap(),
            b"preserve this"
        );
        let failed_script =
            script.replace("os.fsync(out.fileno())", "raise OSError(95, 'simulated')");
        let result = Command::new("python3")
            .args(["-c", &failed_script, dir])
            .output()
            .unwrap();
        assert!(!result.status.success());
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("PROBE_ERROR:write:95"));
        assert!(output.contains("CLEANUP_OK"));
        assert!(!root.join(dir).exists());
        let mismatch = script.replace(
            "if inp.read(128) != b'kubarr-nfs-v4.1-check\\n':",
            "if True:",
        );
        let result = Command::new("python3")
            .args(["-c", &mismatch, dir])
            .output()
            .unwrap();
        assert!(!result.status.success());
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains("PROBE_ERROR:read:0"));
        assert!(output.contains("CLEANUP_OK"));
        assert!(!root.join(dir).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
