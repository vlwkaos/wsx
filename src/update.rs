// One-time startup check against crates.io for a newer version.
// Uses curl (available on macOS/Linux) — no extra dependencies.

const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Returns the latest version string if it's newer than the running binary, else None.
pub fn fetch_latest_version() -> Option<String> {
    let url = format!("https://crates.io/api/v1/crates/{}", CRATE_NAME);
    let out = std::process::Command::new("curl")
        .args(["-s", "--max-time", "5", "-A", "wsx-update-check", &url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let latest = parse_newest_version(&body)?;
    if is_newer(&latest, CURRENT) {
        Some(latest)
    } else {
        None
    }
}

fn parse_newest_version(body: &str) -> Option<String> {
    // JSON contains `"newest_version":"x.y.z"`
    let key = "\"newest_version\":\"";
    let start = body.find(key)? + key.len();
    let end = body[start..].find('"')? + start;
    Some(body[start..end].to_string())
}

/// Returns true if `candidate` is strictly greater than `current` (semver x.y.z).
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<(u32, u32, u32)> {
        let mut p = v.splitn(3, '.');
        Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?))
    }
    match (parse(candidate), parse(current)) {
        (Some(c), Some(r)) => c > r,
        _ => false,
    }
}
