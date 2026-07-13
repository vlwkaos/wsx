use super::RoutineError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub minute: u8,
    pub hour: u8,
    pub day_of_month: u8,
    pub month: u8,
    pub day_of_week: u8,
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
        if parts.len() != 5 {
            return Err(invalid("cron must contain exactly five fields"));
        }
        Ok(Self {
            minute: parse_field(parts[0], 0, 59, false)?,
            hour: parse_field(parts[1], 0, 23, false)?,
            day_of_month: parse_field(parts[2], 1, 31, false)?,
            month: parse_field(parts[3], 1, 12, false)?,
            day_of_week: parse_field(parts[4], 0, 7, true)?,
        })
    }

    pub fn matches(&self, t: LocalTime) -> bool {
        let basic = self.minute.allowed[t.minute as usize]
            && self.hour.allowed[t.hour as usize]
            && self.month.allowed[t.month as usize];
        let dom = self.day_of_month.allowed[t.day_of_month as usize];
        let dow = self.day_of_week.allowed[t.day_of_week as usize];
        let day = if self.day_of_month.restricted && self.day_of_week.restricted {
            dom || dow
        } else {
            dom && dow
        };
        basic && day
    }
}

fn invalid(message: impl Into<String>) -> RoutineError {
    RoutineError::Validation(message.into())
}

fn parse_field(text: &str, min: u8, max: u8, sunday: bool) -> Result<Field, RoutineError> {
    if text.is_empty() {
        return Err(invalid("empty cron field"));
    }
    let mut allowed = vec![false; max as usize + 1];
    let mut wildcard = false;
    for item in text.split(',') {
        if item.is_empty() {
            return Err(invalid(format!("empty cron list item in '{text}'")));
        }
        let (base, step, stepped) = match item.split_once('/') {
            Some((base, step)) if !base.is_empty() && !step.is_empty() && !step.contains('/') => {
                let step = step
                    .parse::<u8>()
                    .map_err(|_| invalid(format!("invalid step in '{item}'")))?;
                if step == 0 {
                    return Err(invalid("cron step must be positive"));
                }
                (base, step, true)
            }
            Some(_) => return Err(invalid(format!("invalid stepped field '{item}'"))),
            None => (item, 1, false),
        };
        let (start, end) = if base == "*" {
            wildcard = true;
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            if b.contains('-') {
                return Err(invalid(format!("invalid range '{base}'")));
            }
            (number(a, min, max)?, number(b, min, max)?)
        } else {
            let value = number(base, min, max)?;
            (value, if stepped { max } else { value })
        };
        if start > end {
            return Err(invalid(format!("descending range '{base}'")));
        }
        for value in (start..=end).step_by(step as usize) {
            allowed[value as usize] = true;
        }
    }
    if sunday && allowed[7] {
        allowed[0] = true;
        allowed[7] = false;
    }
    Ok(Field {
        allowed,
        restricted: !wildcard,
    })
}

fn number(text: &str, min: u8, max: u8) -> Result<u8, RoutineError> {
    let value = text
        .parse::<u8>()
        .map_err(|_| invalid(format!("invalid cron number '{text}'")))?;
    if !(min..=max).contains(&value) {
        return Err(invalid(format!(
            "cron number {value} outside {min}..={max}"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(minute: u8, hour: u8, dom: u8, month: u8, dow: u8) -> LocalTime {
        LocalTime {
            minute,
            hour,
            day_of_month: dom,
            month,
            day_of_week: dow,
        }
    }

    #[test]
    fn supports_wildcard_lists_ranges_steps_and_sunday_seven() {
        let cron = CronSchedule::parse("*/15 1,3 10-12/2 * 7").unwrap();
        assert!(cron.matches(time(30, 3, 10, 6, 0)));
        assert!(!cron.matches(time(31, 3, 10, 6, 0)));
        let numeric_step = CronSchedule::parse("5/20 * * * *").unwrap();
        assert!(numeric_step.matches(time(25, 0, 1, 1, 1)));
    }

    #[test]
    fn restricted_dom_and_dow_use_vixie_or() {
        let cron = CronSchedule::parse("0 0 1 * 2").unwrap();
        assert!(cron.matches(time(0, 0, 9, 1, 2)));
        assert!(cron.matches(time(0, 0, 1, 1, 4)));
        assert!(!cron.matches(time(0, 0, 9, 1, 4)));
    }

    #[test]
    fn rejects_invalid_boundaries_and_steps() {
        for value in [
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "1,,2 * * * *",
            "4-2 * * * *",
        ] {
            assert!(CronSchedule::parse(value).is_err(), "{value}");
        }
    }
}
