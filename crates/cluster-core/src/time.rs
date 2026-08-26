//! Time, behind a trait.
//!
//! `std::time` does not exist on the ESP target in the shape we want, and
//! `chrono` is far too heavy, so the core model only ever speaks in
//! "milliseconds since the Unix epoch" and gets them from a [`Clock`].

use core::fmt;

use alloc::string::String;
use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Millis(pub u64);

impl Millis {
    pub const ZERO: Millis = Millis(0);

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Saturating elapsed time between two instants.
    pub const fn since(self, earlier: Millis) -> u64 {
        self.0.saturating_sub(earlier.0)
    }

    pub const fn plus_ms(self, ms: u64) -> Millis {
        Millis(self.0.saturating_add(ms))
    }

    /// `YYYY-MM-DD HH:MM:SS` in UTC, without pulling in a date library.
    pub fn to_utc_string(self) -> String {
        use alloc::format;
        let (y, mo, d, h, mi, s) = self.to_utc_parts();
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    }

    /// `HH:MM:SS` in UTC -- what the event log shows.
    pub fn to_clock_string(self) -> String {
        use alloc::format;
        let (_, _, _, h, mi, s) = self.to_utc_parts();
        format!("{h:02}:{mi:02}:{s:02}")
    }

    /// Midnight UTC on a calendar date. The inverse of [`Millis::to_utc_parts`],
    /// so config files can carry `2026-08-18` instead of an epoch integer.
    pub fn from_utc_date(year: i64, month: u32, day: u32) -> Millis {
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = if month > 2 { month - 3 } else { month + 9 } as i64;
        let doy = (153 * mp + 2) / 5 + day as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Millis((days.max(0) as u64) * 86_400_000)
    }

    /// `YYYY-MM-DD`, for config files and axis labels.
    pub fn to_date_string(self) -> String {
        use alloc::format;
        let (y, mo, d, _, _, _) = self.to_utc_parts();
        format!("{y:04}-{mo:02}-{d:02}")
    }

    /// Civil-from-days (Howard Hinnant's algorithm), valid for 1970-01-01 on.
    fn to_utc_parts(self) -> (i64, u32, u32, u32, u32, u32) {
        let secs = (self.0 / 1000) as i64;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);

        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = if m <= 2 { y + 1 } else { y };

        (
            y,
            m,
            d,
            (rem / 3600) as u32,
            ((rem % 3600) / 60) as u32,
            (rem % 60) as u32,
        )
    }
}

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The only way core code is allowed to learn the time.
///
/// PC implementation reads `SystemTime`; a future ESP implementation will read
/// the RTC or an SNTP-disciplined counter.
pub trait Clock: Send + Sync {
    fn now(&self) -> Millis;
}

impl<T: Clock + ?Sized> Clock for &T {
    fn now(&self) -> Millis {
        (**self).now()
    }
}
