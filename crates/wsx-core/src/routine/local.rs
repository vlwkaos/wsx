//! This machine's local clock for routines: the zone's time of a unix second, and the trigger
//! text a routine shows and takes back.

use super::Trigger;

/// A routine's trigger as `wsx routine list`, the ledger and the routine editor show it: the
/// cron or `event:<kind>`.
pub fn trigger_text(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron(cron) => cron.clone(),
        Trigger::Event { kind } => format!("event:{kind}"),
    }
}

/// The trigger `trigger_text` shows, read back.
pub fn parse_trigger(text: &str) -> Trigger {
    let text = text.trim();
    match text.strip_prefix("event:") {
        Some(kind) => Trigger::Event { kind: kind.into() },
        None => Trigger::Cron(text.into()),
    }
}

/// `YYYY-MM-DD HH:MM`, or `HH:MM` (its next time after `now_secs`), in this machine's zone, as
/// an epoch minute.
pub fn local_minute(text: &str, now_secs: i64) -> Result<i64, String> {
    let wrong = || format!("time must be HH:MM or YYYY-MM-DD HH:MM: {text}");
    let number = |part: &str| part.parse::<i32>().map_err(|_| wrong());
    let text = text.trim();
    let (date, clock) = match text.split_once(' ') {
        Some((date, clock)) => (Some(date), clock),
        None => (None, text),
    };
    let (hour, minute) = clock.split_once(':').ok_or_else(wrong)?;
    let (hour, minute) = (number(hour)?, number(minute)?);
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return Err(wrong());
    }
    let mut tm = local_tm(now_secs).ok_or_else(wrong)?;
    if let Some(date) = date {
        let parts: Vec<&str> = date.split('-').collect();
        let [year, month, day] = parts[..] else {
            return Err(wrong());
        };
        (tm.tm_year, tm.tm_mon, tm.tm_mday) =
            (number(year)? - 1900, number(month)? - 1, number(day)?);
    }
    (tm.tm_hour, tm.tm_min, tm.tm_sec, tm.tm_isdst) = (hour, minute, 0, -1);
    let day = (tm.tm_year, tm.tm_mon, tm.tm_mday);
    // SAFETY: tm is a valid, filled struct.
    let mut secs = unsafe { libc::mktime(&mut tm) };
    // ^ mktime moves a day that does not exist (02-31) into the next month: refuse it.
    if secs == -1 || (tm.tm_year, tm.tm_mon, tm.tm_mday) != day {
        return Err(wrong());
    }
    if date.is_none() && secs <= now_secs {
        secs += 24 * 3600;
    }
    Ok(secs / 60)
}

/// `secs` (unix) in this machine's zone; none when the zone cannot tell.
pub fn local_tm(secs: i64) -> Option<libc::tm> {
    let secs = libc::time_t::try_from(secs).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for the call; localtime_r is thread-safe.
    if unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: localtime_r filled it.
    Some(unsafe { tm.assume_init() })
}

/// `2026-10-28 03:00`: `secs` (unix) in this machine's zone.
pub fn local_stamp(secs: i64) -> Option<String> {
    let tm = local_tm(secs)?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}
