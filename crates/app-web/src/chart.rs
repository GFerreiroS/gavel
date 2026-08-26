//! Charts, rendered server-side as inline SVG.
//!
//! No charting library. Partly because a JS bundle would dwarf the rest of the
//! frontend, and partly because the eventual target serves pages from flash --
//! but mostly because the data is already reduced by the time it reaches here,
//! so drawing it is a few dozen lines of arithmetic.
//!
//! Colours come from the validated two-slot categorical palette (blue, then
//! orange, assigned in fixed order and never cycled) and are emitted as CSS
//! custom properties so the light and dark steps swap in one place. Hover is
//! native `<title>`: a real tooltip with no script.

use std::fmt::Write;

use app_core::market::{Copper, Cycle, Point};
use cluster_core::Millis;

/// Series identity is fixed by slot, never by rank order in the data.
pub const SERIES: [&str; 2] = ["var(--series-1)", "var(--series-2)"];

const W: f64 = 760.0;
const H: f64 = 240.0;
const PAD_L: f64 = 66.0;
const PAD_R: f64 = 12.0;
const PAD_T: f64 = 12.0;
const PAD_B: f64 = 26.0;

fn gold(c: Copper) -> String {
    let g = c.get() / 10_000;
    if g >= 1_000 {
        format!("{}.{}k g", g / 1_000, (g % 1_000) / 100)
    } else {
        format!("{g}g")
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One plotted line.
pub struct Series<'a> {
    pub label: &'a str,
    pub points: &'a [Point],
    /// Index into [`SERIES`].
    pub slot: usize,
}

/// Price over time. Up to two series; a legend is always drawn for two.
pub fn line_chart(series: &[Series<'_>], empty_note: &str) -> String {
    let all: Vec<&Point> = series.iter().flat_map(|s| s.points.iter()).collect();
    if all.len() < 2 {
        return placeholder(empty_note);
    }

    let (min_t, max_t) = (
        all.iter().map(|p| p.at.get()).min().unwrap(),
        all.iter().map(|p| p.at.get()).max().unwrap(),
    );
    let max_p = all.iter().map(|p| p.price.get()).max().unwrap().max(1);
    // Baseline at zero: a truncated y-axis exaggerates every wiggle, and price
    // is a magnitude where zero is meaningful.
    let (min_p, span_t) = (0u64, (max_t - min_t).max(1));
    let span_p = (max_p - min_p).max(1);

    let x = |t: u64| PAD_L + (t - min_t) as f64 / span_t as f64 * (W - PAD_L - PAD_R);
    let y = |p: u64| H - PAD_B - (p - min_p) as f64 / span_p as f64 * (H - PAD_T - PAD_B);

    let mut svg = String::with_capacity(8 * 1024);
    let _ = write!(
        svg,
        r#"<svg class="chart" viewBox="0 0 {W} {H}" role="img" preserveAspectRatio="none">"#
    );

    // Recessive gridlines and y labels.
    for step in 0..=4 {
        let value = min_p + span_p * step / 4;
        let gy = y(value);
        let _ = write!(
            svg,
            r#"<line class="grid" x1="{PAD_L}" y1="{gy:.1}" x2="{:.1}" y2="{gy:.1}"/>"#,
            W - PAD_R
        );
        let _ = write!(
            svg,
            r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
            PAD_L - 6.0,
            gy + 3.5,
            gold(Copper(value))
        );
    }

    for s in series {
        if s.points.len() < 2 {
            continue;
        }
        let colour = SERIES[s.slot.min(SERIES.len() - 1)];
        let path: String = s
            .points
            .iter()
            .enumerate()
            .map(|(i, p)| {
                format!(
                    "{}{:.1} {:.1}",
                    if i == 0 { "M" } else { "L" },
                    x(p.at.get()),
                    y(p.price.get())
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let _ = write!(
            svg,
            r#"<path d="{path}" fill="none" stroke="{colour}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>"#
        );
    }

    // Hover strips: one transparent column per sample, carrying a native
    // tooltip. Cheaper than a script and works with the keyboard off.
    if let Some(first) = series.first() {
        let step = (W - PAD_L - PAD_R) / first.points.len().max(1) as f64;
        for (i, p) in first.points.iter().enumerate() {
            let cx = x(p.at.get());
            let mut tip = format!("{}\n", p.at.to_utc_string());
            for s in series {
                if let Some(sp) = s.points.get(i) {
                    let _ = writeln!(tip, "{}: {}", s.label, sp.price);
                }
            }
            let _ = write!(tip, "stock: {}", p.quantity);
            let _ = write!(
                svg,
                r#"<rect class="hit" x="{:.1}" y="{PAD_T}" width="{:.1}" height="{:.1}"><title>{}</title></rect>"#,
                cx - step / 2.0,
                step.max(2.0),
                H - PAD_T - PAD_B,
                escape(&tip)
            );
        }
    }

    let _ = write!(
        svg,
        r#"<text class="axis" x="{PAD_L}" y="{:.1}">{}</text>"#,
        H - 8.0,
        Millis(min_t).to_date_string()
    );
    let _ = write!(
        svg,
        r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
        W - PAD_R,
        H - 8.0,
        Millis(max_t).to_date_string()
    );
    svg.push_str("</svg>");
    svg
}

/// Average price per repeating bucket -- hour of day, day of week.
///
/// One series, so no legend: the heading names it.
pub fn bar_chart(cycles: &[Cycle], labels: &dyn Fn(u8) -> String, empty_note: &str) -> String {
    let populated: Vec<&Cycle> = cycles.iter().filter(|c| c.samples > 0).collect();
    if populated.len() < 2 {
        return placeholder(empty_note);
    }

    let max = populated
        .iter()
        .map(|c| c.mean.get())
        .max()
        .unwrap_or(1)
        .max(1);
    let cheapest = populated.iter().map(|c| c.mean.get()).min().unwrap_or(0);
    let slot = (W - PAD_L - PAD_R) / cycles.len() as f64;
    // A 2px surface gap between adjacent bars.
    let bar_w = (slot - 2.0).max(1.0);

    let mut svg = String::with_capacity(4 * 1024);
    let _ = write!(
        svg,
        r#"<svg class="chart" viewBox="0 0 {W} {H}" role="img" preserveAspectRatio="none">"#
    );
    for step in 0..=3 {
        let value = max * step / 3;
        let gy = H - PAD_B - (value as f64 / max as f64) * (H - PAD_T - PAD_B);
        let _ = write!(
            svg,
            r#"<line class="grid" x1="{PAD_L}" y1="{gy:.1}" x2="{:.1}" y2="{gy:.1}"/>"#,
            W - PAD_R
        );
        let _ = write!(
            svg,
            r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
            PAD_L - 6.0,
            gy + 3.5,
            gold(Copper(value))
        );
    }

    for (i, cycle) in cycles.iter().enumerate() {
        if cycle.samples == 0 {
            continue;
        }
        let h = (cycle.mean.get() as f64 / max as f64) * (H - PAD_T - PAD_B);
        let bx = PAD_L + i as f64 * slot + 1.0;
        let by = H - PAD_B - h;
        // The cheapest bucket is the answer to "when do I buy", so it is
        // marked rather than left for the reader to find.
        let cls = if cycle.mean.get() == cheapest {
            "bar best"
        } else {
            "bar"
        };
        let _ = write!(
            svg,
            r#"<rect class="{cls}" x="{bx:.1}" y="{by:.1}" width="{bar_w:.1}" height="{h:.1}" rx="3"><title>{}: {} ({} samples)</title></rect>"#,
            escape(&labels(cycle.bucket)),
            cycle.mean,
            cycle.samples
        );
        // Label every third slot on the 24-hour chart, all of them on 7.
        if cycles.len() <= 8 || i % 3 == 0 {
            let _ = write!(
                svg,
                r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="middle">{}</text>"#,
                bx + bar_w / 2.0,
                H - 8.0,
                escape(&labels(cycle.bucket))
            );
        }
    }
    svg.push_str("</svg>");
    svg
}

fn placeholder(note: &str) -> String {
    format!(r#"<p class="chart-empty muted">{}</p>"#, escape(note))
}
