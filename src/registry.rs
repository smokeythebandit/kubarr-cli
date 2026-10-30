//! Finding the newest published revision of a chart.
//!
//! Helm cannot do this for us. These charts carry SemVer build metadata
//! (`5.1.5+14`), and SemVer says build metadata is ignored when comparing
//! versions -- so every `5.1.5+*` ranks equal and an unversioned pull picks one
//! arbitrarily. Measured against the live registry: Helm resolved `fluent-bit`
//! to `5.1.5+10` when `+14` existed, and `victoriametrics` to `1.154.1+10`
//! when `+16` existed. Six revisions of fixes, skipped silently.
//!
//! So the tags are listed and ordered here instead, with the build number read
//! as a number rather than as text.

use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// A chart version split into the parts that decide which is newer.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
struct Version {
    core: (u64, u64, u64),
    /// The `+N` revision, which may itself be dotted (`+5.11`). Each part is
    /// ordered as a number: as text, "+5.2" sorts after "+5.11", and "+9"
    /// after "+14", either of which picks an older chart than published.
    build: Vec<u64>,
}

/// Parse a chart version, in either the SemVer or the OCI tag spelling.
///
/// An OCI tag cannot contain `+`, so the registry stores `5.1.5+14` as
/// `5.1.5_14`. Both forms arrive here.
fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim();
    let (core_text, build_text) = match text.split_once(['+', '_']) {
        Some((core, build)) => (core, Some(build)),
        None => (text, None),
    };
    let mut parts = core_text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    // A non-numeric build suffix is not a revision we can order, so the whole
    // tag is skipped rather than guessed at.
    let build = match build_text {
        Some(text) => text
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()?,
        None => Vec::new(),
    };
    Some(Version {
        core: (major, minor, patch),
        build,
    })
}

/// Split `oci://host/path/name` into the host and the repository path.
fn split_reference(reference: &str) -> Option<(String, String)> {
    let rest = reference.strip_prefix("oci://")?;
    let (host, repository) = rest.split_once('/')?;
    if host.is_empty() || repository.is_empty() {
        return None;
    }
    Some((host.to_string(), repository.to_string()))
}

/// The newest published version of a chart, in SemVer spelling.
///
/// Returns `Ok(None)` for a reference that is not an OCI chart, matching how
/// a local path is left alone.
pub fn latest_version(reference: &str) -> Result<Option<String>, String> {
    let Some((host, repository)) = split_reference(reference) else {
        return Ok(None);
    };
    let tags = list_tags(&host, &repository)?;
    let newest = tags
        .iter()
        .filter_map(|tag| parse_version(tag).map(|version| (version, tag)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, tag)| tag.replace('_', "+"));

    match newest {
        Some(version) => Ok(Some(version)),
        None => Err(format!(
            "no usable version tags published for {repository} on {host}"
        )),
    }
}

fn list_tags(host: &str, repository: &str) -> Result<Vec<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|error| format!("could not build an HTTP client: {error}"))?;

    // Even a public registry hands out a pull token first.
    let token_url =
        format!("https://{host}/token?scope=repository:{repository}:pull&service={host}");
    let token = client
        .get(&token_url)
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json::<serde_json::Value>())
        .map_err(|error| format!("could not get a pull token from {host}: {error}"))?
        .get("token")
        .and_then(|token| token.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{host} returned no pull token"))?;

    let list_url = format!("https://{host}/v2/{repository}/tags/list?n=1000");
    let body = client
        .get(&list_url)
        .bearer_auth(token)
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json::<serde_json::Value>())
        .map_err(|error| format!("could not list tags for {repository}: {error}"))?;

    let tags = body
        .get("tags")
        .and_then(|tags| tags.as_array())
        .ok_or_else(|| format!("{repository} returned no tag list"))?
        .iter()
        .filter_map(|tag| tag.as_str().map(str::to_string))
        .collect();
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_revision_is_ordered_as_a_number_not_as_text() {
        // The whole reason this module exists. Sorted as text, "+9" lands
        // after "+14" and the installer picks a five-revision-old chart.
        let nine = parse_version("5.1.5+9").expect("parses");
        let fourteen = parse_version("5.1.5+14").expect("parses");
        assert!(fourteen > nine);
    }

    #[test]
    fn the_oci_tag_spelling_parses_the_same_as_the_semver_one() {
        // A tag cannot contain '+', so the registry stores it as '_'.
        assert_eq!(parse_version("1.154.1_16"), parse_version("1.154.1+16"));
    }

    #[test]
    fn the_core_version_outranks_the_build_revision() {
        let old_core_high_build = parse_version("5.1.5+99").expect("parses");
        let new_core_no_build = parse_version("5.2.0").expect("parses");
        assert!(new_core_no_build > old_core_high_build);
    }

    #[test]
    fn patch_numbers_compare_numerically_too() {
        let nine = parse_version("0.3.9").expect("parses");
        let ten = parse_version("0.3.10").expect("parses");
        assert!(ten > nine);
    }

    #[test]
    fn a_version_with_no_build_metadata_sorts_before_any_revision_of_it() {
        let plain = parse_version("13.2.4").expect("parses");
        assert!(plain.build.is_empty());
        assert!(parse_version("13.2.4+1").expect("parses") > plain);
    }

    #[test]
    fn a_dotted_build_revision_compares_part_by_part_as_numbers() {
        // openresty publishes 1.29.2+5.11. As text "+5.2" sorts after "+5.11",
        // and an earlier version of this parser rejected dotted builds
        // outright and silently picked 1.29.2+5 instead.
        let five_two = parse_version("1.29.2+5.2").expect("parses");
        let five_eleven = parse_version("1.29.2+5.11").expect("parses");
        assert!(five_eleven > five_two);

        // A bare revision precedes its own dotted children.
        assert!(
            parse_version("1.29.2+5.1").expect("parses")
                > parse_version("1.29.2+5").expect("parses")
        );
    }

    #[test]
    fn every_published_openresty_tag_shape_parses() {
        for tag in ["1.29.2_5.1", "1.29.2_5.6", "1.29.2_5.10", "1.29.2_5.11"] {
            assert!(parse_version(tag).is_some(), "{tag}");
        }
        let newest = ["1.29.2_5.1", "1.29.2_5.11", "1.29.2_5.2", "1.29.2_5.10"]
            .iter()
            .filter_map(|t| parse_version(t).map(|v| (v, *t)))
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|(_, t)| t);
        assert_eq!(newest, Some("1.29.2_5.11"));
    }

    #[test]
    fn tags_that_are_not_versions_are_skipped_rather_than_guessed_at() {
        assert!(parse_version("latest").is_none());
        assert!(parse_version("sha256-abc").is_none());
        assert!(parse_version("1.2").is_none());
        assert!(parse_version("1.2.3.4").is_none());
        // A build suffix that is not a number cannot be ordered.
        assert!(parse_version("1.2.3+rc1").is_none());
    }

    #[test]
    fn a_local_chart_path_is_left_alone() {
        assert_eq!(latest_version("/tmp/charts/fluent-bit"), Ok(None));
        assert_eq!(latest_version("./charts/openresty"), Ok(None));
    }

    #[test]
    fn a_reference_splits_into_host_and_repository() {
        assert_eq!(
            split_reference("oci://ghcr.io/smokeythebandit/kubarr-charts/fluent-bit"),
            Some((
                "ghcr.io".to_string(),
                "smokeythebandit/kubarr-charts/fluent-bit".to_string()
            ))
        );
        assert_eq!(split_reference("oci://ghcr.io"), None);
    }
}
