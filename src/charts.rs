//! Which revision of each chart an install gets.
//!
//! The newest published revision is resolved from the registry at install
//! time, so a fresh install is never left behind by whatever was current when
//! this file was last edited -- the pins below had drifted to kubarr-backend
//! 0.3.5 while 0.3.9 was published.
//!
//! Helm cannot be asked for "the newest" directly: these charts carry SemVer
//! build metadata (`5.1.5+14`), which SemVer ignores when ranking versions, so
//! an unversioned pull picks arbitrarily among them. See `registry`.
//!
//! The pins remain as the floor. If the registry cannot be reached the install
//! still runs, on a known-good revision, and says out loud that it did so --
//! an installer that fails because GitHub is slow is worse than one that is a
//! few revisions behind for an afternoon.

/// Set to `pinned` to skip the registry entirely.
///
/// Tests use it so a unit test never reaches the network: the suite has a
/// dry-run install that would otherwise make a dozen HTTPS round trips while
/// holding a shared lock, which is slow and fails without connectivity.
pub const VERSION_SOURCE_ENV: &str = "KUBARR_CHART_VERSIONS";

/// Remembered answers, so one install asks the registry once per chart rather
/// than once per call site.
fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The newest published revision, falling back to the pinned one.
///
/// The fallback is deliberately noisy: silently installing something older
/// than asked for is how the drift above went unnoticed.
pub fn resolve_version(name: &str, reference: &str) -> Result<Option<String>, String> {
    if !reference.starts_with("oci://") {
        return Ok(None);
    }
    if std::env::var(VERSION_SOURCE_ENV).as_deref() == Ok("pinned") {
        return pinned_version(name, reference).map(|v| v.map(str::to_string));
    }
    if let Ok(cache) = cache().lock() {
        if let Some(version) = cache.get(reference) {
            return Ok(Some(version.clone()));
        }
    }
    let resolved = resolve_uncached(name, reference)?;
    if let (Some(version), Ok(mut cache)) = (resolved.as_ref(), cache().lock()) {
        cache.insert(reference.to_string(), version.clone());
    }
    Ok(resolved)
}

fn resolve_uncached(name: &str, reference: &str) -> Result<Option<String>, String> {
    match crate::registry::latest_version(reference) {
        Ok(Some(version)) => Ok(Some(version)),
        Ok(None) => pinned_version(name, reference).map(|v| v.map(str::to_string)),
        Err(error) => {
            let pinned = pinned_version(name, reference)?;
            match pinned {
                Some(version) => {
                    crate::style::warn(&format!(
                        "could not read the published versions of {name} ({error}); \
                         installing the pinned {version} instead"
                    ));
                    Ok(Some(version.to_string()))
                }
                None => Err(error),
            }
        }
    }
}

pub fn pinned_version(name: &str, reference: &str) -> Result<Option<&'static str>, String> {
    if !reference.starts_with("oci://") {
        return Ok(None);
    }
    let version = match name {
        "managed-nfs" => "12.0.0+4",
        "postgresql" => "16.15.0+11",
        "fluent-bit" => "5.1.5+14",
        "victorialogs" => "1.53.0+10",
        "victoriametrics" => "1.154.1+15",
        "grafana" => "13.2.4",
        "kubarr-backend" => "0.3.5",
        "kubarr-frontend" => "0.3.5",
        "kubarr-worker" => "0.3.5",
        "openresty" => "1.29.2+5.10",
        "kubarr-cilium-identities" => "1.20.4",
        _ => return Err(format!("no pinned bootstrap chart version for '{name}'")),
    };
    Ok(Some(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_chart_overrides_are_not_forced_to_published_versions() {
        assert_eq!(
            pinned_version("fluent-bit", "/tmp/charts/fluent-bit").unwrap(),
            None
        );
        assert_eq!(
            pinned_version("openresty", "./charts/openresty").unwrap(),
            None
        );
    }

    #[test]
    fn unknown_remote_charts_cannot_fall_back_to_an_ambiguous_latest() {
        assert!(pinned_version("new-component", "oci://example.org/new-component").is_err());
    }
}
