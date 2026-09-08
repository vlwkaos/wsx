// One-time bounded startup check against the latest published GitHub release.

use std::time::Duration;

const CURRENT: &str = env!("CARGO_PKG_VERSION");
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/vlwkaos/wsx/releases/latest";
const MAX_RESPONSE_BYTES: &str = "65536";

/// Returns the latest version string if it is newer than the running binary.
pub fn fetch_latest_version() -> Option<String> {
    retry_once(fetch_latest_version_once, || {
        std::thread::sleep(Duration::from_millis(250))
    })
    .ok()
    .flatten()
}

fn fetch_latest_version_once() -> Result<Option<String>, ()> {
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
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    let latest = parse_latest_version(&out.stdout).ok_or(())?;
    Ok(is_newer(&latest, CURRENT).then_some(latest))
}

fn retry_once<T, E>(
    mut operation: impl FnMut() -> Result<T, E>,
    backoff: impl FnOnce(),
) -> Result<T, E> {
    match operation() {
        Ok(value) => Ok(value),
        Err(_) => {
            backoff();
            operation()
        }
    }
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
    use super::{is_newer, parse_latest_version, retry_once};

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
    fn update_lookup_retries_one_failure_but_not_a_definitive_result() {
        let mut attempts = 0;
        let mut backed_off = false;
        let result = retry_once(
            || {
                attempts += 1;
                (attempts == 2)
                    .then_some(Some("0.22.3".to_string()))
                    .ok_or(())
            },
            || backed_off = true,
        );
        assert_eq!(result.unwrap().as_deref(), Some("0.22.3"));
        assert_eq!(attempts, 2);
        assert!(backed_off);

        let mut attempts = 0;
        let current = retry_once(
            || {
                attempts += 1;
                Ok::<_, ()>(None::<String>)
            },
            || panic!("a definitive current result must not retry"),
        );
        assert_eq!(current.unwrap(), None);
        assert_eq!(attempts, 1);
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
