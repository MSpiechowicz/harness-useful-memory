//! The cron expressions of a routine's `{"type": "cron"}` schedule, as the desktop's routine
//! format defines them (`docs/routines-format.md`, Cron): five fields (minute, hour, day of the
//! month, month, day of the week) separated by exactly one space, each a comma-separated list of
//! items. An item is `*`, a value or a range `a-b`, and a step `/n` may follow `*` or a range
//! (not a single value). Values are decimal numbers of at most two digits; months also take `jan`
//! to `dec`, days of the week `sun` to `sat` (any case), and the day of the week runs 0 to 7, 0
//! and 7 both Sunday (Sunday ending a range that starts after Sunday counts as 7: `fri-sun` is
//! Friday to Sunday, `sun-sun` is Sunday only). A step runs from 1 to the field's highest value.
//!
//! [`check`] only decides validity, purely: the grammar, and that the expression can match at
//! all, checked against the calendar rather than today's date (and so without a time zone, which
//! this server does not know). The time fields always hold some minute, every month holds every
//! day of the week, and every date falls on every day of the week in some year. So an expression
//! can only never match when its day of the month is the one restricted day field (it does not
//! begin with `*`, while the day of the week does: the two then combine with AND) and none of its
//! days exists in any of its months, February counting 29 days: `0 0 31 2 *` and
//! `0 0 31 4,6,9,11 *` never match, `0 0 29 2 *` matches in leap years. When both day fields
//! begin with something other than `*`, a day matching either one matches (OR).

/// The month names, January first.
const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// The day of the week names, Sunday (0) first.
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// The most days each month has, January first (February in a leap year).
const MONTH_DAYS: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// One field of the expression: its name in messages, its smallest and highest value, and the
/// names standing for the values from the smallest on.
struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
}

const MINUTE: Field = Field {
    name: "minute",
    min: 0,
    max: 59,
    names: &[],
};

const HOUR: Field = Field {
    name: "hour",
    min: 0,
    max: 23,
    names: &[],
};

const DAY: Field = Field {
    name: "day of the month",
    min: 1,
    max: 31,
    names: &[],
};

const MONTH: Field = Field {
    name: "month",
    min: 1,
    max: 12,
    names: &MONTHS,
};

const WEEKDAY: Field = Field {
    name: "day of the week",
    min: 0,
    max: 7,
    names: &WEEKDAYS,
};

/// The values a field matches, one bit per value.
type Values = u64;

/// Ok when `expression` is a valid five-field cron expression that can match (see the module
/// docs); the reason otherwise.
pub fn check(expression: &str) -> Result<(), String> {
    let fields: Vec<&str> = expression.split(' ').collect();

    let [minute, hour, day, month, weekday] = fields[..] else {
        return Err(
            "a cron expression is five fields separated by single spaces: minute hour day-of-month month \
             day-of-week"
                .to_owned(),
        );
    };

    values(minute, &MINUTE)?;
    values(hour, &HOUR)?;
    let days = values(day, &DAY)?;
    let months = values(month, &MONTH)?;
    values(weekday, &WEEKDAY)?;

    let only_days = !day.starts_with('*') && weekday.starts_with('*');

    if only_days && !day_exists(days, months) {
        return Err("the cron expression never runs: none of its days exists in its months".to_owned());
    }

    Ok(())
}

/// True when some day of `days` exists in some month of `months` (February counting 29 days).
fn day_exists(days: Values, months: Values) -> bool {
    MONTH_DAYS.iter().zip(1..).any(|(length, month)| {
        let in_month = months & (1 << month) != 0;
        in_month && (1..=*length).any(|day| days & (1 << day) != 0)
    })
}

/// The values of one field, at least one.
fn values(text: &str, field: &Field) -> Result<Values, String> {
    let mut matched: Values = 0;

    for item in text.split(',') {
        matched |= item_values(item, field).map_err(|reason| format!("cron {}: {reason}", field.name))?;
    }

    Ok(matched)
}

/// The values of one item: `*`, `a` or `a-b`, the first and the last with an optional `/step`.
fn item_values(item: &str, field: &Field) -> Result<Values, String> {
    if item.is_empty() {
        return Err("an item is empty".to_owned());
    }

    let (base, step) = match item.split_once('/') {
        Some((base, step)) => (base, Some(step)),
        None => (item, None),
    };

    let (first, last) = if base == "*" {
        (field.min, field.max)
    } else if let Some((first, last)) = base.split_once('-') {
        let first = value(first, field)?;
        (first, range_end(first, last, field)?)
    } else if step.is_some() {
        return Err(format!("{item:?} has a step after a single value"));
    } else {
        let only = value(base, field)?;
        (only, only)
    };

    if first > last {
        return Err(format!("{item:?} is a range that ends before it starts"));
    }

    let step = match step {
        None => 1,
        Some(step) => match number(step) {
            Some(step) if (1..=field.max).contains(&step) => step,
            _ => {
                return Err(format!(
                    "{item:?} has a step that is not a number from 1 to {}",
                    field.max
                ));
            }
        },
    };

    Ok((first..=last)
        .step_by(usize::try_from(step).unwrap_or(1))
        .fold(0, |matched, value| matched | (1 << value)))
}

/// A number from the field's smallest to its highest value, or one of its names.
fn value(text: &str, field: &Field) -> Result<u32, String> {
    let lower = text.to_ascii_lowercase();

    if let Some(index) = field.names.iter().position(|name| *name == lower) {
        // The names stand for the values from the smallest on (`jan` is 1, `sun` is 0).
        return Ok(field.min + u32::try_from(index).unwrap_or(0));
    }

    match number(text) {
        Some(found) if (field.min..=field.max).contains(&found) => Ok(found),
        _ => Err(format!("{text:?} is not a value from {} to {}", field.min, field.max)),
    }
}

/// The end of a range: [`value`], except that Sunday (`sun` or `0`) ends a day of the week range
/// that starts after Sunday as 7, so `fri-sun` runs Friday to Sunday and `sun-sun` is Sunday only.
fn range_end(start: u32, text: &str, field: &Field) -> Result<u32, String> {
    let end = value(text, field)?;

    if field.name == WEEKDAY.name && start > 0 && end == 0 {
        return Ok(7);
    }

    Ok(end)
}

/// One or two decimal digits as a number.
fn number(text: &str) -> Option<u32> {
    let digits = (1..=2).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit());
    digits.then(|| text.parse().ok())?
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn valid_expressions_pass() {
        for good in [
            "* * * * *",
            "0 9 * * 1-5",
            "*/15 * * * *",
            "0 0 1 * *",
            "30 8 * * MON,wed,Fri",
            "0 12 * JAN-mar sun",
            "0 0 29 2 *",
            "0 0 31 2 1",
            "0 0 31 2 fri-sun,mon",
            "0 0 * * 7",
            "0 0 * * sun-sun",
            "0 0 * * 0-0",
            "0 0 * * fri-0",
            "0-30/10 1-23/2 1,15 */3 0-7",
            "59 23 31 12 6",
            "0 0 30,31 1-12 *",
        ] {
            assert_eq!(check(good), Ok(()), "{good}");
        }
    }

    #[test]
    fn invalid_expressions_fail() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            " * * * * *",
            "* * * * * ",
            "*  * * * *",
            "0\t9 * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "*/60 * * * *",
            "5/10 * * * *",
            "005 * * * *",
            "5-1 * * * *",
            "* * * * fri-mon",
            "jan * * * *",
            "* * * * mon-",
            ", * * * *",
            "1,,2 * * * *",
            "-1 * * * *",
            "* * * foo *",
            "* * * * sunday",
            "* * ? * *",
            "* * L * *",
            "* * * * 5#2",
            "@daily",
            "0 0 30 2 *",
            "0 0 31 4,6,9,11 *",
            "0 0 31 2 *",
            "0 0 31 2 */2",
        ] {
            assert!(check(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names_stand_for_their_numbers() {
        assert_eq!(value("jan", &MONTH), Ok(1));
        assert_eq!(value("DEC", &MONTH), Ok(12));
        assert_eq!(value("sun", &WEEKDAY), Ok(0));
        assert_eq!(value("Sat", &WEEKDAY), Ok(6));
        assert!(value("jan", &WEEKDAY).is_err());
    }
}
