//! Small display helpers, kept out of the templates so the templates stay
//! declarative and the formatting stays testable.

use app_core::locale::Locale;

pub(crate) fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = value as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.0} {}", UNITS[unit])
}

/// A duration at the coarsest unit that still says something.
///
/// It stopped at minutes until Phase 5, which was right while its only caller
/// was a job's runtime -- a job that runs for an hour is news, and "73m" is
/// the number somebody wants. Then a market card started printing how old its
/// price was, and a snapshot from last night rendered as **"992m 35s ago"**.
/// That is a number a reader has to do arithmetic on to understand, which is
/// the opposite of what a freshness line is for.
///
/// Two significant units, never three: "16h 32m", not "16h 32m 35s".
pub(crate) fn duration_ms(ms: u64) -> String {
    const SECOND: u64 = 1_000;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if ms < SECOND {
        format!("{ms} ms")
    } else if ms < MINUTE {
        format!("{:.1} s", ms as f64 / SECOND as f64)
    } else if ms < HOUR {
        format!("{}m {}s", ms / MINUTE, (ms % MINUTE) / SECOND)
    } else if ms < DAY {
        format!("{}h {}m", ms / HOUR, (ms % HOUR) / MINUTE)
    } else {
        format!("{}d {}h", ms / DAY, (ms % DAY) / HOUR)
    }
}

/// "hace 3m 20s" / "3m 20s ago" -- the wording is translated, the number is
/// not. Takes the locale because the sentence wraps the value: several
/// languages put "ago" in front, so the two cannot be concatenated in the
/// template.
pub(crate) fn ago(locale: Locale, ms: u64) -> String {
    if ms < 1_500 {
        return crate::i18n::translate(locale, "just now").to_string();
    }
    crate::i18n::translate(locale, "{} ago").replacen("{}", &duration_ms(ms), 1)
}

pub(crate) fn optional_f32(value: Option<f32>) -> String {
    match value {
        Some(v) => format!("{v:.1}"),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every step, and the one that was wrong.
    ///
    /// `992m 35s` is what a card printed for a price collected last night
    /// before this function learned about hours -- a number a reader has to
    /// divide by sixty to understand.
    #[test]
    fn a_duration_uses_the_coarsest_unit_that_says_something() {
        assert_eq!(duration_ms(0), "0 ms");
        assert_eq!(duration_ms(999), "999 ms");
        assert_eq!(duration_ms(1_500), "1.5 s");
        assert_eq!(duration_ms(90_000), "1m 30s");
        // The one from the card.
        assert_eq!(duration_ms(59_555_000), "16h 32m");
        assert_eq!(duration_ms(3_600_000), "1h 0m");
        assert_eq!(duration_ms(90_000_000), "1d 1h");
    }

    /// Two units, never three: the third is noise at every scale it appears.
    #[test]
    fn a_duration_never_shows_a_third_unit() {
        for ms in [1_500u64, 90_000, 59_555_000, 90_000_000] {
            let rendered = duration_ms(ms);
            assert!(
                rendered.matches(char::is_alphabetic).count() <= 3,
                "{rendered} has more than two units"
            );
        }
    }
}
