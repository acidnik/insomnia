//! Quiet hours: a daily window in local wall-clock time during which the
//! daemon goes on running checks and updating state, but holds notifications
//! back. What happens to the held-back messages is in `Engine::flush_deferred`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::config::QuietTimeConfig;

/// A daily window `from` (inclusive) → `to` (exclusive) as minutes since local
/// midnight. `from < to` sits inside one day (`02:00` → `09:00`), `from > to`
/// crosses midnight (`23:00` → `07:00`) — the direction is inferred, there is
/// no separate switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietWindow {
    from: u32,
    to: u32,
}

impl QuietWindow {
    /// `None` config means no quiet hours at all; a malformed one is an error
    /// and not a warning: a typo here would silently wake the user up all night
    pub fn parse(cfg: Option<&QuietTimeConfig>) -> Result<Option<QuietWindow>> {
        let Some(cfg) = cfg else { return Ok(None) };
        let from = parse_hhmm(&cfg.from).context("quiet_time.from")?;
        let to = parse_hhmm(&cfg.to).context("quiet_time.to")?;
        if from == to {
            bail!(
                "quiet_time.from and quiet_time.to are both {} — that would silence alerts \
                 for the whole day; remove the [quiet_time] section to disable quiet hours",
                cfg.from
            );
        }
        Ok(Some(QuietWindow { from, to }))
    }

    /// is the window open at this minute of the local day?
    pub fn active_at(&self, minute: u32) -> bool {
        if self.from < self.to {
            minute >= self.from && minute < self.to
        } else {
            // crosses midnight: 23:00 → 07:00 is open at 23:30 and at 00:30
            minute >= self.from || minute < self.to
        }
    }

    /// `23:00→07:00`, for logs and the summary header
    pub fn label(&self) -> String {
        format!("{}→{}", fmt_hhmm(self.from), fmt_hhmm(self.to))
    }
}

fn parse_hhmm(s: &str) -> Result<u32> {
    let s = s.trim();
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("expected HH:MM, got '{s}'"))?;
    let h: u32 = h.trim().parse().map_err(|_| anyhow!("bad hour in '{s}'"))?;
    let m: u32 = m.trim().parse().map_err(|_| anyhow!("bad minute in '{s}'"))?;
    if h > 23 || m > 59 {
        bail!("'{s}' is not a valid time of day");
    }
    Ok(h * 60 + m)
}

fn fmt_hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// minutes since local midnight. Goes through the C library so that the TZ
/// database and DST are handled exactly like everywhere else on the machine
/// (the project has no date library, and libc is already a dependency).
pub fn local_minute() -> u32 {
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as libc::time_t,
        Err(_) => return 0,
    };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r fills tm for any time_t and is thread-safe; a null
    // return (out-of-range time) leaves the zeroed struct alone
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return 0;
    }
    (tm.tm_hour as u32) * 60 + tm.tm_min as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(from: &str, to: &str) -> QuietWindow {
        QuietWindow::parse(Some(&QuietTimeConfig {
            from: from.to_string(),
            to: to.to_string(),
        }))
        .unwrap()
        .unwrap()
    }

    fn minute(h: u32, m: u32) -> u32 {
        h * 60 + m
    }

    #[test]
    fn window_crossing_midnight() {
        let w = window("23:00", "07:00");
        assert_eq!(w.label(), "23:00→07:00");
        assert!(!w.active_at(minute(22, 59)));
        assert!(w.active_at(minute(23, 0)), "start is inclusive");
        assert!(w.active_at(minute(23, 30)));
        assert!(w.active_at(minute(0, 0)), "the tail is after midnight");
        assert!(w.active_at(minute(6, 59)));
        assert!(!w.active_at(minute(7, 0)), "end is exclusive");
        assert!(!w.active_at(minute(12, 0)));
    }

    #[test]
    fn window_inside_one_day() {
        let w = window("02:00", "09:00");
        assert_eq!(w.label(), "02:00→09:00");
        assert!(!w.active_at(minute(1, 59)));
        assert!(w.active_at(minute(2, 0)));
        assert!(w.active_at(minute(8, 59)));
        assert!(!w.active_at(minute(9, 0)));
        assert!(!w.active_at(minute(23, 0)));
    }

    #[test]
    fn accepts_hour_without_leading_zero() {
        let w = window("7:05", "9:00");
        assert_eq!(w.label(), "07:05→09:00");
        assert!(w.active_at(minute(8, 0)));
    }

    #[test]
    fn missing_section_means_no_quiet_hours() {
        assert!(QuietWindow::parse(None).unwrap().is_none());
    }

    #[test]
    fn rejects_nonsense_windows() {
        let bad = |from: &str, to: &str| {
            QuietWindow::parse(Some(&QuietTimeConfig {
                from: from.to_string(),
                to: to.to_string(),
            }))
            .is_err()
        };
        assert!(bad("07:00", "07:00"), "equal ends are ambiguous, not all-day");
        assert!(bad("24:00", "07:00"));
        assert!(bad("23:70", "07:00"));
        assert!(bad("7", "07:00"), "no colon");
        assert!(bad("", "07:00"));
    }

    #[test]
    fn local_minute_is_a_time_of_day() {
        assert!(local_minute() < 24 * 60);
    }
}
