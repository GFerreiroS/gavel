//! Small display helpers, kept out of the templates so the templates stay
//! declarative and the formatting stays testable.

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

pub(crate) fn duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

pub(crate) fn ago(ms: u64) -> String {
    if ms < 1_500 {
        "just now".to_string()
    } else {
        format!("{} ago", duration_ms(ms))
    }
}

pub(crate) fn optional_f32(value: Option<f32>) -> String {
    match value {
        Some(v) => format!("{v:.1}"),
        None => "—".to_string(),
    }
}
