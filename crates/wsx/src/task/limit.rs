//! The wait on an agent's usage limit, shared by the task host and the manager host.
//! ref: docs/tasks.md "Claude adapter"

/// After the reset: the limit lifts on the provider's clock, not this machine's.
const MARGIN_MS: u64 = 5_000;
/// No reset time known: 5 minutes, doubled for each limit in a row, at most an hour.
const FIRST_BACKOFF_MS: u64 = 5 * 60_000;
const MAX_BACKOFF_MS: u64 = 60 * 60_000;
/// A reset further ahead is not believed: the longest limit is a week.
const MAX_AHEAD_MS: u64 = 8 * 24 * 3_600_000;

/// When to resume after the `attempt`-th limit in a row (1 for the first), in unix ms.
pub fn resume_at(resets_at: Option<u64>, attempt: u32, now_ms: u64) -> u64 {
    let backoff = (FIRST_BACKOFF_MS << attempt.saturating_sub(1).min(4)).min(MAX_BACKOFF_MS);
    match resets_at.map(|secs| secs.saturating_mul(1000) + MARGIN_MS) {
        Some(at) if at > now_ms && at <= now_ms + MAX_AHEAD_MS => at,
        _ => now_ms + backoff,
    }
}

/// "session limit; resumes at 00:10", with the date when it is not within a day.
pub fn reason(until_ms: u64, now_ms: u64) -> String {
    let when = local_time(until_ms / 1000).map_or_else(
        || {
            format!(
                "in {} min",
                until_ms.saturating_sub(now_ms).div_ceil(60_000)
            )
        },
        |tm| {
            let clock = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
            match until_ms.saturating_sub(now_ms) < 20 * 3_600_000 {
                true => clock,
                false => format!("{} {} {clock}", MONTHS[tm.tm_mon as usize % 12], tm.tm_mday),
            }
        },
    );
    format!("session limit; resumes at {when}")
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `secs` (unix) in this machine's zone.
pub fn local_time(secs: u64) -> Option<libc::tm> {
    let secs = libc::time_t::try_from(secs).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for the call; localtime_r is thread-safe.
    if unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: localtime_r filled it.
    Some(unsafe { tm.assume_init() })
}
