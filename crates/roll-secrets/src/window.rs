//! Rotation windows (docs/secrets.md, "Rotation windows"): when a due scheduled rotation may run,
//! as AWS Secrets Manager's schedule window. `sun 02:00-04:00`, `mon,thu 22:00-01:00` (past
//! midnight: it ends the next day), `daily 03:00-05:00`; times are UTC, an hour at least.

const MINUTE: i64 = 60_000;
const DAY: i64 = 24 * 60 * MINUTE;
const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// Bit `n` set: the window opens on that weekday (0 is Monday).
    days: u8,
    /// Minutes after midnight UTC.
    start: i64,
    end: i64,
}

fn minutes(hhmm: &str) -> Option<i64> {
    let (h, m) = hhmm.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60 && hhmm.len() == 5).then_some(h * 60 + m)
}

/// The weekday (0 is Monday) of the day starting at unix day `day`: 1970-01-01 was a Thursday.
fn weekday(day: i64) -> u32 {
    (day + 3).rem_euclid(7) as u32
}

impl Window {
    pub fn parse(s: &str) -> Result<Window, String> {
        let bad = || format!("rotate_window is like \"sun 02:00-04:00\" or \"daily 03:00-05:00\" (UTC), not {s:?}");
        let s = s.trim().to_ascii_lowercase();
        let (days, times) = s.split_once(' ').ok_or_else(bad)?;
        let days = if days == "daily" {
            0x7f
        } else {
            days.split(',').try_fold(0u8, |acc, d| DAYS.iter().position(|n| *n == d).map(|i| acc | 1 << i)).ok_or_else(bad)?
        };
        let (start, end) = times.trim().split_once('-').ok_or_else(bad)?;
        let (start, end) = (minutes(start.trim()).ok_or_else(bad)?, minutes(end.trim()).ok_or_else(bad)?);
        if (end - start).rem_euclid(24 * 60) < 60 {
            return Err(format!("rotate_window {s:?} is open for less than an hour"));
        }
        Ok(Window { days, start, end })
    }

    fn opens_on(&self, day: i64) -> bool {
        self.days & 1 << weekday(day) != 0
    }

    /// Whether the window is open at `at` (unix ms).
    pub fn open_at(&self, at: i64) -> bool {
        let (day, m) = (at.div_euclid(DAY), at.rem_euclid(DAY) / MINUTE);
        if self.start < self.end {
            self.opens_on(day) && (self.start..self.end).contains(&m)
        } else {
            (self.opens_on(day) && m >= self.start) || (self.opens_on(day - 1) && m < self.end)
        }
    }

    /// The first moment at or after `from` (unix ms) the window is open.
    pub fn next_open(&self, from: i64) -> i64 {
        if self.open_at(from) {
            return from;
        }
        let today = from.div_euclid(DAY);
        (0..=7).map(|d| today + d).filter(|d| self.opens_on(*d)).map(|d| d * DAY + self.start * MINUTE).find(|t| *t >= from).unwrap_or(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_opens_on_its_days_and_crosses_midnight() {
        // 2026-11-15 was a Sunday.
        let sunday = 20772 * DAY;
        assert_eq!(weekday(20772), 6);
        let w = Window::parse("Sun 02:00-04:00").unwrap();
        assert!(w.open_at(sunday + 150 * MINUTE) && !w.open_at(sunday + 240 * MINUTE) && !w.open_at(sunday + DAY + 150 * MINUTE));
        // Due on Monday: the next Sunday's start.
        assert_eq!(w.next_open(sunday + DAY), sunday + 7 * DAY + 120 * MINUTE);
        let late = Window::parse("mon,thu 22:00-01:00").unwrap();
        assert!(late.open_at(sunday + DAY + 23 * 60 * MINUTE) && late.open_at(sunday + 2 * DAY + 30 * MINUTE), "Monday night into Tuesday");
        assert!(!late.open_at(sunday + 30 * MINUTE), "Sunday 00:30 follows a Saturday, not a Monday");
        assert_eq!(Window::parse("daily 03:00-05:00").unwrap().next_open(sunday + 6 * 60 * MINUTE), sunday + DAY + 180 * MINUTE);
        for bad in ["sun", "sun 2:00-04:00", "someday 02:00-04:00", "sun 02:00-02:30", "sun 25:00-02:00"] {
            assert!(Window::parse(bad).is_err(), "{bad}");
        }
    }
}
