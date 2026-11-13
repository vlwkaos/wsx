//! The wait on an agent's usage limit, shared by the task host and the manager host.
//! ref: docs/tasks.md "Claude adapter" (Usage limit), "Agents"

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
    known_reset(resets_at, now_ms).unwrap_or(now_ms + backoff)
}

/// The provider's reset `resets_at` (unix seconds) as the time to resume, when it is
/// believable; without one the host backs off, and the backoffs in a row add up to one limit.
pub fn known_reset(resets_at: Option<u64>, now_ms: u64) -> Option<u64> {
    resets_at
        .map(|secs| secs.saturating_mul(1000) + MARGIN_MS)
        .filter(|at| *at > now_ms && *at <= now_ms + MAX_AHEAD_MS)
}

/// The words a sign-in wait's reason opens with.
pub const SIGN_IN_WORDS: &str = "sign-in expired";

/// "sign-in expired; tries again at 00:10": the wait for a failed sign-in's retry.
pub fn sign_in_reason(until_ms: u64, now_ms: u64) -> String {
    format!(
        "{SIGN_IN_WORDS}; tries again at {}",
        wsx_core::task::status::resume_time(until_ms, now_ms)
    )
}

/// The reason of a wait on `account` now: its failed sign-in's retry (`sign_in_reason`),
/// else the window and reset its limit record names (`status::limit_reason`).
pub fn wait_reason(adapter: &str, account: &str, until_ms: u64, now_ms: u64) -> String {
    use wsx_core::task::{status, store};
    match store::sign_in_failed_until(account, now_ms) {
        Some(retry_ms) => sign_in_reason(retry_ms, now_ms),
        None => status::limit_reason(
            adapter,
            store::account_limit(account, now_ms)
                .and_then(|limit| limit.window)
                .as_deref(),
            until_ms,
            now_ms,
        ),
    }
}
