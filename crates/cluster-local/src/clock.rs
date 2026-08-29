use std::time::{SystemTime, UNIX_EPOCH};

use cluster_core::{Clock, Millis};

/// System clock used by the host runtime. Tests provide deterministic
/// [`Clock`] implementations instead.
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
