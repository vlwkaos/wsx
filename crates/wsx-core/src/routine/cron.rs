//! Five-field cron (minute hour day-of-month month day-of-week) on the local clock: `*`,
//! numbers, `a-b`, lists and `/step`; day of week 0-7, 0 and 7 are Sunday. When both day
//! fields are restricted, a day matching either runs, as in Vixie cron.

use super::RoutineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub minute: u8,
    pub hour: u8,
    pub day_of_month: u8,
    pub month: u8,
    pub day_of_week: u8,
}

impl LocalTime {
    pub fn at(epoch: i64) -> Self {
        let timestamp = epoch as libc::time_t;
        let mut out = std::mem::MaybeUninit::<libc::tm>::zeroed();
        // ^ localtime_r fills the whole tm it is given.
        let out = unsafe {
            libc::localtime_r(&timestamp, out.as_mut_ptr());
            out.assume_init()
        };
        Self {
            minute: out.tm_min as u8,
            hour: out.tm_hour as u8,
            day_of_month: out.tm_mday as u8,
            month: (out.tm_mon + 1) as u8,
            day_of_week: out.tm_wday as u8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    allowed: Vec<bool>,
    restricted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Field,
    hour: Field,
    day_of_month: Field,
    month: Field,
    day_of_week: Field,
}

impl CronSchedule {
    pub fn parse(input: &str) -> Result<Self, RoutineError> {
        let parts: Vec<_> = input.split_whitespace().collect();
        let [minute, hour, day_of_month, month, day_of_week] = parts[..] else {
            return Err(invalid("cron must contain exactly five fields"));
        };
        let mut day_of_week = parse_field(day_of_week, 0, 7)?;
        if day_of_week.allowed[7] {
            day_of_week.allowed[0] = true;
        }
        Ok(Self {
            minute: parse_field(minute, 0, 59)?,
            hour: parse_field(hour, 0, 23)?,
            day_of_month: parse_field(day_of_month, 1, 31)?,
            month: parse_field(month, 1, 12)?,
            day_of_week,
        })
    }

    pub fn matches(&self, t: LocalTime) -> bool {
        self.minute.allowed[t.minute as usize]
            && self.hour.allowed[t.hour as usize]
            && self.date_matches(t)
    }

    fn date_matches(&self, t: LocalTime) -> bool {
        let dom = self.day_of_month.allowed[t.day_of_month as usize];
        let dow = self.day_of_week.allowed[t.day_of_week as usize];
        let day = match self.day_of_month.restricted && self.day_of_week.restricted {
            true => dom || dow,
            false => dom && dow,
        };
        self.month.allowed[t.month as usize] && day
    }

    /// The first matching minute strictly after `epoch`, as epoch seconds.
    pub fn next_run_after(&self, epoch: i64) -> Option<i64> {
        let mut candidate = (epoch / 60 + 1) * 60;
        // ^ Eight years cover the longest gap a numeric schedule has (Feb 29 over a century).
        let deadline = candidate + 8 * 366 * 24 * 3600;
        while candidate < deadline {
            let local = LocalTime::at(candidate);
            if self.matches(local) {
                return Some(candidate);
            }
            // ^ Skip the rest of an hour that cannot match, else to the next allowed minute.
            let minute = usize::from(local.minute);
            let skip = match self.date_matches(local) && self.hour.allowed[local.hour as usize] {
                true => (minute + 1..60)
                    .find(|m| self.minute.allowed[*m])
                    .unwrap_or(60),
                false => 60,
            } - minute;
            candidate += skip as i64 * 60;
        }
        None
    }
}

fn invalid(message: impl Into<String>) -> RoutineError {
    RoutineError::validation(message)
}

fn parse_field(text: &str, min: u8, max: u8) -> Result<Field, RoutineError> {
    let mut allowed = vec![false; max as usize + 1];
    let mut restricted = true;
    for item in text.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => {
                let step = step
                    .parse::<u8>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| invalid(format!("invalid step in '{item}'")))?;
                (base, Some(step))
            }
            None => (item, None),
        };
        let (start, end) = if base == "*" {
            restricted = false;
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            (number(a, min, max)?, number(b, min, max)?)
        } else {
            let value = number(base, min, max)?;
            (value, if step.is_some() { max } else { value })
        };
        if start > end {
            return Err(invalid(format!("descending range '{base}'")));
        }
        for value in (start..=end).step_by(step.unwrap_or(1) as usize) {
            allowed[value as usize] = true;
        }
    }
    Ok(Field {
        allowed,
        restricted,
    })
}

fn number(text: &str, min: u8, max: u8) -> Result<u8, RoutineError> {
    text.parse::<u8>()
        .ok()
        .filter(|value| (min..=max).contains(value))
        .ok_or_else(|| {
            invalid(format!(
                "invalid cron number '{text}', expected {min}-{max}"
            ))
        })
}
