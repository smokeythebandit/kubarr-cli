//! Bootstrap chart revisions are explicit: SemVer build metadata (+revision)
//! does not determine precedence, so an unversioned OCI pull can pick an old fix.

pub fn pinned_version(name: &str, reference: &str) -> Result<Option<&'static str>, String> {
    if !reference.starts_with("oci://") {
        return Ok(None);
    }
    let version = match name {
        "managed-nfs" => "12.0.0+3",
        "postgresql" => "16.15.0+9",
        "fluent-bit" => "5.1.5+10",
        "victorialogs" => "1.53.0+9",
        "victoriametrics" => "1.154.1+11",
        "grafana" => "13.2.4",
        "kubarr-backend" => "0.3.4",
        "kubarr-frontend" => "0.3.4",
        "kubarr-worker" => "0.3.4",
        "openresty" => "1.29.2+5.6",
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
