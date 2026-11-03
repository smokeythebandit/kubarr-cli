//! The platform database password.
//!
//! The charts used to ship `database.password: kubarr`, so every Kubarr
//! install ran the database that holds the user table, the JWT signing key
//! and the Jellyfin/SABnzbd API keys on a password anyone could read in a
//! public repository. The charts now refuse to render without a password and
//! take it from `database.existingSecret`; this module owns that Secret.
//!
//! Three rules shape the code below:
//!
//! * The password never leaves this module. It is never printed, never put in
//!   an argument list, and never carried in an error. Only the Secret's *name*
//!   travels to Helm.
//! * An existing password is never regenerated. The database owner's password
//!   is set when the data directory is initialised, so a fresh password on an
//!   existing database locks the platform out of its own data.
//! * The Secret lives in all three namespaces that read it. A Secret is not
//!   readable across namespaces, and the backend and worker charts both point
//!   `database.secretName` at `kubarr-db-app` in their own namespace.
//!
//! The database is a CloudNativePG `Cluster`, which bootstraps its owner from
//! this same Secret (`bootstrap.initdb.secret`). CNPG requires that Secret to
//! be `kubernetes.io/basic-auth` with a `username` matching the owner, so the
//! Secret has exactly one owner -- this module -- and CNPG never generates its
//! own `kubarr-db-app`.

use crate::install_events::{self, Operation};
use crate::style::{detail, ok, paint, status_label, step, warn, BLUE, CYAN, DIM};
use crate::types::{
    APP_NAMESPACE, BOOTSTRAP_RELEASE_NAMESPACE, DATABASE_CLUSTER, DATABASE_CLUSTER_RESOURCE,
    DATABASE_NAME, DATABASE_NAMESPACE, DATABASE_RELEASE, DATABASE_SERVICE, DATABASE_USER,
    DB_SECRET_NAME, WORKER_NAMESPACE, WORKER_RELEASE,
};
use crate::util::{command_output, external_command};

/// The password the charts published until this change. An install made
/// before it initialised its data directory with this value, and PostgreSQL
/// will keep honouring only that value, so recovery has to assume it when
/// nothing better survives.
const PUBLISHED_DEFAULT_PASSWORD: &str = "kubarr";

/// 48 characters out of a 64-symbol alphabet: 288 bits from the operating
/// system CSPRNG, comfortably past the 32-character floor.
const PASSWORD_LENGTH: usize = 48;

/// 256 is a whole multiple of 64, so masking a uniform byte down to six bits
/// indexes this alphabet uniformly -- no modulo bias, no rejection loop. Every
/// symbol is unreserved in a URI userinfo component, so the connection string
/// needs no percent-encoding, and none of them need YAML or shell quoting.
const PASSWORD_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Visible in the browser wizard, so it is a fixed string with nothing
/// interpolated into it. Keep it in `safe_event`'s allowlist.
pub const PUBLISHED_PASSWORD_WARNING: &str = "the platform database is running on the password that was published in the Kubarr charts; it was kept because PostgreSQL only applies a new password while initialising an empty data directory";
/// Returned when the cluster cannot be read. Fixed text with nothing
/// interpolated, so `safe_failure_result` can show it verbatim.
pub const CLUSTER_UNREADABLE: &str = "Could not read the cluster's namespaces, so the platform database password cannot be chosen safely; installation was not changed.";
pub const PUBLISHED_PASSWORD_ROTATION: &str = "to rotate it: ALTER USER kubarr on the kubarr-db primary, write the new password into the kubarr-db-app Secret in kubarr-database, kubarr-backend and kubarr-worker, then restart the backend and worker";

/// Every namespace that needs its own copy, with the Helm release that owns
/// the namespace object once its chart is installed.
const SECRET_TARGETS: &[(&str, &str)] = &[
    (DATABASE_NAMESPACE, DATABASE_RELEASE),
    (APP_NAMESPACE, "kubarr-backend"),
    (WORKER_NAMESPACE, WORKER_RELEASE),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// A Secret this CLI already wrote. Reused byte for byte.
    Existing,
    /// A pre-change install: the chart's own Secret, which carried only `uri`.
    RecoveredFromSecret,
    /// A pre-change install whose Secret is gone. The data directory was
    /// initialised with the published default, so that is the only password
    /// that still opens it.
    RecoveredDefault,
    /// A fresh install.
    Generated,
}

/// What the cluster says about the database, kept separate from the kubectl
/// calls that gather it so the decision can be tested without a cluster.
#[derive(Clone, Debug, Default)]
pub struct ClusterState {
    /// `kubarr-db-app`'s `password` key in `kubarr-database`.
    pub secret_password: Option<String>,
    /// `kubarr-db-app`'s `uri` key in `kubarr-database`.
    pub secret_uri: Option<String>,
    /// Whether the `kubarr-db` CloudNativePG Cluster exists in
    /// `kubarr-database`.
    ///
    /// This is the "is there already a data directory" signal. The Cluster is
    /// created by the postgresql chart and by nothing else, so if it is there
    /// the chart has run and CNPG has initialised the database with the
    /// password of the day.
    pub database_exists: bool,
}

/// Make sure the credentials Secret exists in all three namespaces.
///
/// Returns the source of the password so the caller can be tested; the
/// password itself is dropped before this function returns.
pub fn ensure_database_credentials(dry_run: bool) -> Result<CredentialSource, String> {
    step("Database", "preparing the platform database credentials");
    detail("secret", DB_SECRET_NAME);
    detail(
        "namespaces",
        &SECRET_TARGETS
            .iter()
            .map(|(namespace, _)| *namespace)
            .collect::<Vec<_>>()
            .join(", "),
    );
    if dry_run {
        plan_apply();
        return Ok(CredentialSource::Generated);
    }
    let state = read_cluster_state()?;
    let (password, source) = decide(&state)?;
    let manifest = manifest(&password, SECRET_TARGETS)?;
    drop(password);
    apply(&manifest)?;
    report(source, &state);
    Ok(source)
}

/// The password to use, and where it came from.
fn decide(state: &ClusterState) -> Result<(String, CredentialSource), String> {
    if let Some(password) = state
        .secret_password
        .as_deref()
        .filter(|password| !password.is_empty())
    {
        return Ok((password.to_string(), CredentialSource::Existing));
    }
    if let Some(password) = state.secret_uri.as_deref().and_then(password_from_uri) {
        return Ok((password, CredentialSource::RecoveredFromSecret));
    }
    if state.database_exists {
        return Ok((
            PUBLISHED_DEFAULT_PASSWORD.to_string(),
            CredentialSource::RecoveredDefault,
        ));
    }
    Ok((generate_password()?, CredentialSource::Generated))
}

/// Whether the install is carrying a password that was published in the
/// charts, and therefore has to be called out.
fn is_published(source: CredentialSource, state: &ClusterState) -> bool {
    match source {
        CredentialSource::Generated => false,
        CredentialSource::RecoveredFromSecret | CredentialSource::RecoveredDefault => true,
        // A Secret we wrote earlier, but an earlier run may have recovered the
        // published default into it.
        CredentialSource::Existing => {
            state.secret_password.as_deref() == Some(PUBLISHED_DEFAULT_PASSWORD)
        }
    }
}

fn report(source: CredentialSource, state: &ClusterState) {
    match source {
        CredentialSource::Generated => ok("generated a new platform database password"),
        CredentialSource::Existing => ok("reusing the existing platform database password"),
        CredentialSource::RecoveredFromSecret | CredentialSource::RecoveredDefault => {
            ok("kept the password the existing database was initialised with")
        }
    }
    if is_published(source, state) {
        warn(PUBLISHED_PASSWORD_WARNING);
        warn(PUBLISHED_PASSWORD_ROTATION);
    }
}

fn generate_password() -> Result<String, String> {
    let mut bytes = [0u8; PASSWORD_LENGTH];
    getrandom::fill(&mut bytes)
        .map_err(|_| "could not generate a database password".to_string())?;
    let password = bytes
        .iter()
        .map(|byte| PASSWORD_ALPHABET[usize::from(byte & 0b0011_1111)] as char)
        .collect();
    bytes.fill(0);
    Ok(password)
}

/// The password out of a `postgresql://user:password@host:port/db` URI.
///
/// Only ever applied to the Secret the old charts rendered, which interpolated
/// the password without escaping it, so reading it back verbatim reproduces
/// exactly the password PostgreSQL was initialised with.
fn password_from_uri(uri: &str) -> Option<String> {
    let rest = uri.split_once("://")?.1;
    let authority = rest
        .split_once('/')
        .map_or(rest, |(authority, _)| authority);
    let userinfo = authority.rsplit_once('@')?.0;
    let password = userinfo.split_once(':')?.1;
    (!password.is_empty()).then(|| password.to_string())
}

/// Reading this wrong in one direction only costs a Helm value; reading it
/// wrong in the other destroys access to the data. So the reads have to be
/// trusted before their answers are: a kubectl that cannot answer at all
/// looks exactly like an empty cluster, and "empty cluster" is the one answer
/// that generates a new password. Prove kubectl can read the cluster first,
/// and fail loudly instead of guessing.
fn read_cluster_state() -> Result<ClusterState, String> {
    if command_output("kubectl", &["get", "namespace", "-o", "name"]).is_none() {
        return Err(CLUSTER_UNREADABLE.into());
    }
    Ok(ClusterState {
        secret_password: read_secret_key(DATABASE_NAMESPACE, "password"),
        secret_uri: read_secret_key(DATABASE_NAMESPACE, "uri"),
        database_exists: command_output(
            "kubectl",
            &[
                "get",
                DATABASE_CLUSTER_RESOURCE,
                DATABASE_CLUSTER,
                "-n",
                DATABASE_NAMESPACE,
                "-o",
                "jsonpath={.metadata.name}",
            ],
        )
        .is_some(),
    })
}

/// One key of `kubarr-db-app`, decoded by kubectl rather than by a base64
/// crate we would otherwise have to add. The value is returned to the caller
/// and never handed to the event log or the diagnostic file.
fn read_secret_key(namespace: &str, key: &str) -> Option<String> {
    let template =
        format!("go-template={{{{if .data.{key}}}}}{{{{.data.{key} | base64decode}}}}{{{{end}}}}");
    command_output(
        "kubectl",
        &[
            "get",
            "secret",
            DB_SECRET_NAME,
            "-n",
            namespace,
            "-o",
            &template,
        ],
    )
}

/// The namespaces and Secrets, as one declarative document.
///
/// Each namespace carries the ownership metadata Helm looks for before it
/// touches a resource it did not create, so the chart that renders the same
/// namespace later adopts this object instead of refusing to install. The
/// Secret carries `helm.sh/resource-policy: keep` because an install made
/// before this change has `kubarr-db-app` inside the release manifest: without
/// the annotation, the very next `helm upgrade` would notice the chart no
/// longer renders it and delete the credentials out from under the pods.
fn manifest(password: &str, targets: &[(&str, &str)]) -> Result<String, String> {
    // A double-quoted YAML scalar carries any printable ASCII except the two
    // characters that start an escape. A recovered password holding one of
    // those is refused rather than mangled into a Secret nothing can read.
    if !password
        .chars()
        .all(|ch| ch.is_ascii_graphic() && ch != '"' && ch != '\\')
    {
        return Err("the existing database password cannot be represented safely".into());
    }
    let uri = format!(
        "postgresql://{DATABASE_USER}:{password}@{DATABASE_SERVICE}.{DATABASE_NAMESPACE}.svc.cluster.local:5432/{DATABASE_NAME}"
    );
    let mut documents = Vec::new();
    for (namespace, release) in targets {
        documents.push(format!(
            "apiVersion: v1\n\
             kind: Namespace\n\
             metadata:\n  \
               name: {namespace}\n  \
               labels:\n    \
                 app.kubernetes.io/managed-by: Helm\n    \
                 pod-security.kubernetes.io/enforce: restricted\n    \
                 pod-security.kubernetes.io/audit: restricted\n    \
                 pod-security.kubernetes.io/warn: restricted\n  \
               annotations:\n    \
                 meta.helm.sh/release-name: {release}\n    \
                 meta.helm.sh/release-namespace: {BOOTSTRAP_RELEASE_NAMESPACE}\n"
        ));
        documents.push(format!(
            "apiVersion: v1\n\
             kind: Secret\n\
             metadata:\n  \
               name: {DB_SECRET_NAME}\n  \
               namespace: {namespace}\n  \
               annotations:\n    \
                 helm.sh/resource-policy: keep\n\
             type: kubernetes.io/basic-auth\n\
             stringData:\n  \
               username: \"{DATABASE_USER}\"\n  \
               password: \"{password}\"\n  \
               uri: \"{uri}\"\n"
        ));
    }
    Ok(documents.join("---\n"))
}

fn plan_apply() {
    // The manifest holds the password, so unlike every other planned command
    // its body is withheld even from a dry run.
    if install_events::emit("[PLAN] kubectl apply -f - (manifest withheld)") {
        return;
    }
    println!(
        "   {} kubectl apply -f - {}",
        status_label("plan", CYAN),
        paint("(manifest withheld: it carries the database password)", DIM)
    );
}

fn apply(manifest: &str) -> Result<(), String> {
    if !install_events::emit("[RUN] kubectl apply -f -") {
        println!("   {} kubectl apply -f -", status_label("run", BLUE));
    }
    let mut command = external_command("kubectl");
    command.args(["apply", "-f", "-"]);
    let status = crate::util::run_command_with_progress(
        &mut command,
        "kubectl apply",
        Some(manifest.as_bytes()),
        Some(Operation::DatabaseSecret),
    )?;
    if !status.success() {
        return Err(format!("kubectl apply failed ({status})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(password: Option<&str>, uri: Option<&str>, database_exists: bool) -> ClusterState {
        ClusterState {
            secret_password: password.map(str::to_string),
            secret_uri: uri.map(str::to_string),
            database_exists,
        }
    }

    #[test]
    fn an_existing_database_keeps_its_password_and_only_a_fresh_one_gets_a_new_password() {
        // A Secret this CLI wrote: reused byte for byte, however the rest of
        // the cluster looks. Regenerating here locks the platform out, because
        // the owner password is only set while initdb runs.
        for database_exists in [true, false] {
            let existing = state(
                Some("kept-verbatim"),
                Some("postgresql://u:other@h:5432/d"),
                database_exists,
            );
            assert_eq!(
                decide(&existing).unwrap(),
                ("kept-verbatim".to_string(), CredentialSource::Existing)
            );
            assert!(!is_published(CredentialSource::Existing, &existing));
        }

        // A pre-change install: the chart's Secret carried only `uri`.
        let legacy = state(
            None,
            Some("postgresql://kubarr:kubarr@kubarr-db-rw.kubarr-database.svc.cluster.local:5432/kubarr"),
            true,
        );
        assert_eq!(
            decide(&legacy).unwrap(),
            ("kubarr".to_string(), CredentialSource::RecoveredFromSecret)
        );
        assert!(is_published(CredentialSource::RecoveredFromSecret, &legacy));

        // A pre-change install whose Secret is gone. The data directory still
        // only answers to the password published in the charts.
        let orphaned = state(None, None, true);
        assert_eq!(
            decide(&orphaned).unwrap(),
            ("kubarr".to_string(), CredentialSource::RecoveredDefault)
        );
        assert!(is_published(CredentialSource::RecoveredDefault, &orphaned));

        // Nothing at all: a fresh install, and the only case that generates.
        let fresh = state(None, None, false);
        let (password, source) = decide(&fresh).unwrap();
        assert_eq!(source, CredentialSource::Generated);
        assert_ne!(password, PUBLISHED_DEFAULT_PASSWORD);
        assert!(!is_published(CredentialSource::Generated, &fresh));

        // An empty `password` key is not a password; fall through to recovery.
        assert_eq!(
            decide(&state(Some(""), None, true)).unwrap().1,
            CredentialSource::RecoveredDefault
        );
        // An earlier run that recovered the published default still warns.
        assert!(is_published(
            CredentialSource::Existing,
            &state(Some("kubarr"), None, true)
        ));
    }

    #[test]
    fn generated_passwords_are_long_unpredictable_and_need_no_quoting() {
        let first = generate_password().unwrap();
        let second = generate_password().unwrap();
        assert_ne!(first, second, "two calls produced the same password");
        for password in [&first, &second] {
            assert_eq!(password.chars().count(), PASSWORD_LENGTH);
            assert!(password.len() >= 32);
            assert!(
                password
                    .bytes()
                    .all(|byte| PASSWORD_ALPHABET.contains(&byte)),
                "password left the URI-safe alphabet"
            );
        }
        // Every alphabet symbol has to survive a URI userinfo component and a
        // double-quoted YAML scalar untouched.
        for byte in PASSWORD_ALPHABET {
            let ch = char::from(*byte);
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "{ch} would need escaping"
            );
        }
        // A thousand draws should cover most of a 64-symbol alphabet; a
        // generator stuck on a counter or a clock would not.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            seen.insert(generate_password().unwrap());
        }
        assert_eq!(seen.len(), 1000, "generated passwords repeated");
    }

    #[test]
    fn legacy_connection_strings_give_back_exactly_the_password_they_carried() {
        assert_eq!(
            password_from_uri(
                "postgresql://kubarr:s3cr3t@kubarr-db-rw.kubarr-database.svc.cluster.local:5432/kubarr"
            )
            .as_deref(),
            Some("s3cr3t")
        );
        // An @ inside the password: the host is after the *last* @.
        assert_eq!(
            password_from_uri("postgresql://kubarr:a@b@host:5432/kubarr").as_deref(),
            Some("a@b")
        );
        // No database path.
        assert_eq!(
            password_from_uri("postgresql://kubarr:pw@host:5432").as_deref(),
            Some("pw")
        );
        for broken in [
            "",
            "kubarr",
            "postgresql://host:5432/kubarr",
            "postgresql://kubarr@host:5432/kubarr",
            "postgresql://kubarr:@host:5432/kubarr",
        ] {
            assert_eq!(password_from_uri(broken), None, "accepted {broken:?}");
        }
    }

    #[test]
    fn the_secret_is_written_to_every_namespace_that_reads_it_and_is_safe_to_reapply() {
        let password = "generated-password-value";
        let first = manifest(password, SECRET_TARGETS).unwrap();
        let second = manifest(password, SECRET_TARGETS).unwrap();
        // `kubectl apply` of a byte-identical document is a no-op, so a
        // re-run of bootstrap neither fails nor changes the stored value.
        assert_eq!(first, second);

        for namespace in ["kubarr-database", "kubarr-backend", "kubarr-worker"] {
            assert!(
                first.contains(&format!("name: kubarr-db-app\n  namespace: {namespace}\n")),
                "no Secret for {namespace}"
            );
            assert!(
                first.contains(&format!(
                    "kind: Namespace\nmetadata:\n  name: {namespace}\n"
                )),
                "no Namespace for {namespace}"
            );
        }
        assert_eq!(first.matches("kind: Secret").count(), 3);
        assert_eq!(first.matches("kind: Namespace").count(), 3);
        assert_eq!(
            first.matches("helm.sh/resource-policy: keep").count(),
            3,
            "a chart upgrade that stops rendering the Secret would delete it"
        );
        // Helm refuses to adopt a resource it did not create unless all three
        // of these match the release that renders it.
        for release in ["postgresql", "kubarr-backend", "kubarr-worker"] {
            assert!(first.contains(&format!("meta.helm.sh/release-name: {release}\n")));
        }
        assert_eq!(
            first
                .matches("meta.helm.sh/release-namespace: default")
                .count(),
            3
        );
        assert_eq!(
            first.matches("app.kubernetes.io/managed-by: Helm").count(),
            3
        );
        assert_eq!(
            first
                .matches("pod-security.kubernetes.io/enforce: restricted")
                .count(),
            3
        );
        assert_eq!(
            first.matches(&format!("password: \"{password}\"")).count(),
            3
        );
        // CloudNativePG bootstraps the database owner from this same Secret
        // (`bootstrap.initdb.secret`), which it requires to be basic-auth
        // with a username matching the owner.
        assert_eq!(first.matches("type: kubernetes.io/basic-auth\n").count(), 3);
        assert_eq!(first.matches("type: Opaque").count(), 0);
        assert_eq!(first.matches("username: \"kubarr\"\n").count(), 3);
        assert_eq!(
            first
                .matches(&format!(
                    "uri: \"postgresql://kubarr:{password}@kubarr-db-rw.kubarr-database.svc.cluster.local:5432/kubarr\""
                ))
                .count(),
            3
        );
        // Rendered as YAML, not as shell words: nothing here is a kubectl
        // argument.
        for document in first.split("---\n") {
            assert!(document.starts_with("apiVersion: v1\n"));
        }
    }

    #[test]
    fn a_recovered_password_that_cannot_be_quoted_fails_without_echoing_it() {
        for hostile in ["pass\"word", "pass\nword", "pass\\word", "wachtwoord-é"] {
            let Err(error) = manifest(hostile, SECRET_TARGETS) else {
                // Only the characters that survive YAML double quoting are
                // accepted; the rest must be refused rather than escaped
                // wrongly into a Secret nothing can read.
                assert!(
                    !hostile.contains('"') && !hostile.contains('\\') && !hostile.contains('\n'),
                    "accepted {hostile:?}"
                );
                continue;
            };
            assert!(!error.contains(hostile), "the error echoed the password");
        }
    }
}

/// Exercises the real code path against a stand-in `kubectl`: the password is
/// generated, written and reused without the test ever being told what it is,
/// which is the only way to prove the rest of the installer never sees it
/// either.
#[cfg(all(test, unix))]
mod cluster_tests {
    use super::*;
    use crate::install_events::{InstallEvent, INSTALL_TEST_LOCK};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    struct FakeKubectl {
        root: std::path::PathBuf,
        previous_path: Option<std::ffi::OsString>,
    }

    impl FakeKubectl {
        /// Answers the three reads `read_cluster_state` makes and records
        /// every argument list and every applied manifest. Once a manifest has
        /// been applied it starts reporting the Secret's `password` key, the
        /// way a cluster would on the next bootstrap run.
        fn install() -> Self {
            Self::install_answering(true)
        }

        /// A kubectl that cannot answer -- no cluster, no permission, a
        /// platform where the probe helper is unavailable. Indistinguishable
        /// from an empty cluster unless the code checks.
        fn blind() -> Self {
            Self::install_answering(false)
        }

        fn install_answering(answers: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "kubarr-db-credentials-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let dir = root.display();
            let script = format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" >> {dir}/argv\n\
                 case \"$1 $2\" in\n\
                 'get secret')\n\
                 case \"$*\" in *.data.password*) [ -f {dir}/password ] && cat {dir}/password ;; esac\n\
                 exit 0 ;;\n\
                 'get namespace') echo namespace/default; exit 0 ;;\n\
                 'get cluster.postgresql.cnpg.io') exit 1 ;;\n\
                 'apply -f')\n\
                 cat > {dir}/latest\n\
                 cat {dir}/latest >> {dir}/applied\n\
                 sed -n 's/^  password: \"\\(.*\\)\"$/\\1/p' {dir}/latest | head -1 | tr -d '\\n' > {dir}/password\n\
                 exit 0 ;;\n\
                 esac\n\
                 exit 1\n"
            );
            let script = if answers {
                script
            } else {
                format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> {dir}/argv\nexit 1\n")
            };
            let path = root.join("kubectl");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let previous_path = std::env::var_os("PATH");
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    root.display(),
                    previous_path.clone().unwrap_or_default().to_string_lossy()
                ),
            );
            Self {
                root,
                previous_path,
            }
        }

        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
        }
    }

    impl Drop for FakeKubectl {
        fn drop(&mut self) {
            match self.previous_path.take() {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_password_is_generated_once_reused_on_a_rerun_and_never_leaves_the_cluster() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let fake = FakeKubectl::install();
        let (tx, rx) = mpsc::channel();
        install_events::set_install_log(tx);

        assert_eq!(
            ensure_database_credentials(false).unwrap(),
            CredentialSource::Generated,
            "an empty cluster must get a new password"
        );
        // A second bootstrap run finds the Secret and must keep it: PostgreSQL
        // only sets the owner password while initialising an empty data
        // directory, so a new one here would lock the platform out.
        assert_eq!(
            ensure_database_credentials(false).unwrap(),
            CredentialSource::Existing,
            "a re-run regenerated the password"
        );

        install_events::clear_install_log();
        let events: Vec<String> = rx
            .into_iter()
            .filter_map(|event| match event {
                InstallEvent::Legacy(line) => Some(line),
                other => other.browser_line(),
            })
            .collect();

        let applied = fake.read("applied");
        let passwords: Vec<&str> = applied
            .lines()
            .filter_map(|line| line.trim().strip_prefix("password: \""))
            .filter_map(|line| line.strip_suffix('"'))
            .collect();
        // Three namespaces, twice.
        assert_eq!(passwords.len(), 6, "applied manifests: {applied}");
        let password = passwords[0];
        assert_eq!(password.len(), PASSWORD_LENGTH);
        assert!(
            passwords.iter().all(|value| *value == password),
            "the re-run wrote a different password"
        );
        assert_eq!(applied.matches("kind: Secret").count(), 6);
        assert_eq!(applied.matches("kind: Namespace").count(), 6);

        // The manifest is the only place the password exists. It reached
        // kubectl on stdin, so it is in no argument list...
        let argv = fake.read("argv");
        assert!(!argv.contains(password), "the password reached an argv");
        assert!(!argv.contains("--from-literal"), "argv: {argv}");
        assert!(argv.contains("apply\n-f\n-\n"), "argv: {argv}");
        // An existing database is the CloudNativePG Cluster, not the
        // StatefulSet the old chart rendered.
        assert!(
            argv.contains("get\ncluster.postgresql.cnpg.io\nkubarr-db\n"),
            "argv: {argv}"
        );
        assert!(!argv.contains("statefulset"), "argv: {argv}");
        // ...and in no progress line the browser or the terminal can see.
        for event in &events {
            assert!(!event.contains(password), "a progress event leaked it");
        }
        assert!(events
            .iter()
            .any(|event| event.contains("Database: apply credentials Secret")));
        // And the browser filter lets the step through without the detail.
        assert!(events.iter().any(|event| event.contains(">> Database")
            || event.contains("[STEP] Database")
            || event.contains("[OK] Database")));
    }

    #[test]
    fn a_cluster_that_cannot_be_read_stops_instead_of_overwriting_a_live_password() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let fake = FakeKubectl::blind();
        let (tx, rx) = mpsc::channel();
        install_events::set_install_log(tx);
        let error = ensure_database_credentials(false).unwrap_err();
        install_events::clear_install_log();
        drop(rx);
        // "No Secret and no database Cluster" is what an unreachable cluster looks
        // like, and it is the one answer that generates. Generating here would
        // hand PostgreSQL a password it will never accept, because it only
        // sets the owner password while initialising an empty data directory.
        assert_eq!(error, CLUSTER_UNREADABLE);
        assert_eq!(
            crate::bootstrap_web::safe_failure_result_for_test(&error),
            CLUSTER_UNREADABLE,
            "the browser was shown a generic failure instead of the cause"
        );
        assert_eq!(fake.read("applied"), "", "wrote a Secret blind");
        assert!(!fake.read("argv").contains("apply"), "applied blind");
    }

    #[test]
    fn a_dry_run_touches_no_cluster_and_withholds_the_manifest() {
        let _lock = INSTALL_TEST_LOCK.lock().unwrap();
        let fake = FakeKubectl::install();
        let (tx, rx) = mpsc::channel();
        install_events::set_install_log(tx);
        ensure_database_credentials(true).unwrap();
        install_events::clear_install_log();
        let events: Vec<String> = rx
            .into_iter()
            .filter_map(|event| match event {
                InstallEvent::Legacy(line) => Some(line),
                other => other.browser_line(),
            })
            .collect();
        assert_eq!(fake.read("argv"), "", "a dry run ran kubectl");
        assert_eq!(fake.read("applied"), "");
        let plan: Vec<&String> = events
            .iter()
            .filter(|event| event.starts_with("[PLAN]"))
            .collect();
        assert_eq!(plan.len(), 1, "events: {events:?}");
        assert!(plan[0].contains("withheld"), "{}", plan[0]);
        assert!(!plan[0].contains("stringData"));
    }
}
