// One-time bounded startup check against the latest published GitHub release.

const CURRENT: &str = env!("CARGO_PKG_VERSION");
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/vlwkaos/wsx/releases/latest";
const MAX_RESPONSE_BYTES: &str = "65536";

/// Returns the latest version string if it is newer than the running binary.
pub fn fetch_latest_version() -> Option<String> {
    let out = std::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "5",
            "--max-filesize",
            MAX_RESPONSE_BYTES,
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            "-A",
            "wsx-update-check",
            LATEST_RELEASE_URL,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let latest = parse_latest_version(&out.stdout)?;
    is_newer(&latest, CURRENT).then_some(latest)
}

fn parse_latest_version(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    parse_version(version)?;
    Some(version.to_string())
}

fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_newer, parse_latest_version};

    #[test]
    fn latest_release_parser_accepts_prefixed_stable_tags() {
        assert_eq!(
            parse_latest_version(br#"{"tag_name":"v0.21.0"}"#).as_deref(),
            Some("0.21.0")
        );
        assert_eq!(
            parse_latest_version(br#"{"tag_name":"0.21.0"}"#).as_deref(),
            Some("0.21.0")
        );
    }

    #[test]
    fn latest_release_parser_rejects_missing_or_nonstable_tags() {
        assert_eq!(parse_latest_version(br#"{}"#), None);
        assert_eq!(
            parse_latest_version(br#"{"tag_name":"v0.21.0-beta.1"}"#),
            None
        );
        assert_eq!(parse_latest_version(b"not json"), None);
    }

    #[test]
    fn newer_version_comparison_is_strict_and_numeric() {
        assert!(is_newer("0.20.1", "0.20.0"));
        assert!(is_newer("0.21.0", "0.20.9"));
        assert!(!is_newer("0.20.0", "0.20.0"));
        assert!(!is_newer("0.19.9", "0.20.0"));
        assert!(!is_newer("invalid", "0.20.0"));
    }
}
