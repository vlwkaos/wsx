//! Small text helpers of the task code: a line shortened, a date in words.

const FIRST_LINE_CHARS: usize = 160;

/// The first line of `text` with words, cut to `FIRST_LINE_CHARS` characters.
pub fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    match line.chars().count() > FIRST_LINE_CHARS {
        true => format!(
            "{}…",
            line.chars().take(FIRST_LINE_CHARS - 1).collect::<String>()
        ),
        false => line.to_string(),
    }
}

/// The date (UTC) of a unix time in ms, `YYYY-MM-DD`.
pub fn day_of(unix_ms: u64) -> String {
    let days = (unix_ms / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let of_era = days - era * 146_097;
    let of_year = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_day = of_era - (365 * of_year + of_year / 4 - of_year / 100);
    let shifted = (5 * of_day + 2) / 153;
    let day = of_day - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = of_year + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
