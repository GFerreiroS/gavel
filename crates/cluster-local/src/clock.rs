use std::time::{SystemTime, UNIX_EPOCH};

use cluster_core::{Clock, Millis};

/// Host clock. The ESP implementation will read the RTC / SNTP-disciplined
/// counter instead, which is exactly why callers only ever see [`Clock`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        Millis(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        )
    }
}
