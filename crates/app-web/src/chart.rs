//! Charts, rendered server-side as inline SVG.
//!
//! No charting library: a JS bundle would dwarf the rest of the frontend, and
//! the data is already reduced by the time it reaches here, so drawing it is a
//! few dozen lines of arithmetic.
//!
//! Colours come from the validated two-slot categorical palette (blue, then
//! orange, assigned in fixed order and never cycled) and are emitted as CSS
//! custom properties so the light and dark steps swap in one place. Hover is
//! native `<title>`: a real tooltip with no script.

use std::fmt::Write;

use app_core::market::{Copper, Cycle, Point};
use cluster_core::Millis;

/// The categorical palette, in slot order: blue, then orange.
///
/// One source of truth, in Rust, because two things draw from it -- the line
/// strokes inside the SVG and the legend swatches outside it. When the palette
/// lived in CSS the legend and the chart could disagree silently, and did:
/// rewriting the stylesheet left the swatches with no colour at all while the
/// lines kept theirs.
pub const SERIES_COLOURS: [&str; 2] = ["#3b82f6", "#f59e0b"];

/// Series identity is fixed by slot, never by rank order in the data.
const SERIES: [&str; 2] = ["var(--series-1)", "var(--series-2)"];

/// The colour for a series slot, saturating rather than wrapping: a third
/// series reuses the last colour instead of silently pairing with the first.
pub fn series_colour(slot: usize) -> &'static str {
    SERIES_COLOURS[slot.min(SERIES_COLOURS.len() - 1)]
}

const W: f64 = 760.0;
const H: f64 = 260.0;
const PAD_L: f64 = 64.0;
const PAD_R: f64 = 14.0;
const PAD_T: f64 = 14.0;
const PAD_B: f64 = 30.0;

/// What the y values mean. Without this the stock chart formatted unit counts
/// through the money formatter and rendered "9650 units" as "0g".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Gold,
    Count,
}

impl Unit {
    /// Axis-tick text. Precision adapts to the magnitude, so a 3-gold item
    /// does not get an axis reading `0g, 0g, 1g`.
    fn tick(self, value: u64) -> String {
        match self {
            Unit::Gold => {
                let gold = value as f64 / 10_000.0;
                if value == 0 {
                    "0".to_string()
                } else if gold >= 1_000.0 {
                    format!("{:.0}k g", gold / 1_000.0)
                } else if gold >= 10.0 {
                    format!("{gold:.0}g")
                } else if gold >= 1.0 {
                    format!("{gold:.1}g")
                } else {
                    format!("{}s", value / 100)
                }
            }
            Unit::Count => {
                if value >= 1_000_000 {
                    format!("{:.1}M", value as f64 / 1_000_000.0)
                } else if value >= 1_000 {
                    format!("{:.0}k", value as f64 / 1_000.0)
                } else {
                    value.to_string()
                }
            }
        }
    }

    fn value(self, value: u64) -> String {
        match self {
            Unit::Gold => Copper(value).to_string(),
            Unit::Count => format!("{value} units"),
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A "nice" axis step: 1, 2, 2.5 or 5 times a power of ten.
///
/// Ticks used to be plain fractions of the maximum, which produced axes like
/// `0, 154, 308, 462, 617`. Rounded steps are the difference between an axis
/// you can read a value off and one you can only look at.
fn nice_step(rough: f64) -> f64 {
    if rough <= 0.0 {
        return 1.0;
    }
    let magnitude = 10f64.powf(rough.log10().floor());
    let frac = rough / magnitude;
    let nice = if frac <= 1.0 {
        1.0
    } else if frac <= 2.0 {
        2.0
    } else if frac <= 2.5 {
        2.5
    } else if frac <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// Axis bounds snapped outwards to whole steps.
fn nice_axis(min: u64, max: u64, target_ticks: usize) -> (f64, f64, f64) {
    let span = (max.saturating_sub(min)).max(1) as f64;
    let step = nice_step(span / target_ticks.max(1) as f64);
    let lo = (min as f64 / step).floor() * step;
    let hi = (max as f64 / step).ceil() * step;
    // A flat series would otherwise collapse to a zero-height plot.
    if (hi - lo).abs() < f64::EPSILON {
        (lo, lo + step, step)
    } else {
        (lo, hi, step)
    }
}

/// One plotted line.
pub struct Series<'a> {
    pub label: &'a str,
    pub points: &'a [Point],
    /// Index into [`SERIES`].
    pub slot: usize,
}

/// The chart's own stylesheet, emitted inside every chart.
///
/// These rules used to live in the app stylesheet. That made a chart depend on
/// a file that never mentions charts, and the dependency was invisible: a
/// rewrite of `style.css` dropped `.chart` and every graph in the app broke at
/// once -- unsized, unlabelled, with the invisible hover targets rendering as
/// solid grey slabs. Nothing failed to compile and no test noticed.
///
/// Carrying the styling here makes a chart one self-contained artefact. Adding
/// a new one cannot depend on remembering to add CSS somewhere else, and no
/// edit to the stylesheet can take the axes away.
///
/// Every colour falls back to a literal, so a chart still reads even if the
/// Pico variables are renamed. The block repeats once per chart, which gzip
/// reduces to almost nothing -- a cheaper price than the coupling it removes.
const CHART_STYLE: &str = concat!(
    "<style>",
    "svg.chart{width:100%;height:auto;display:block;",
    // The two-slot categorical palette, assigned by slot and never cycled.
    // Kept in step with SERIES_COLOURS by `the_legend_and_the_lines_agree`;
    // `concat!` takes literals only, so the values cannot be interpolated.
    "--series-1:#3b82f6;--series-2:#f59e0b;",
    "--chart-line:var(--pico-muted-border-color,#4a5568);",
    "--chart-text:var(--pico-muted-color,#8b93a7)}",
    "svg.chart .grid{stroke:var(--chart-line);stroke-width:1}",
    "svg.chart .axis{fill:var(--chart-text);font-size:11px;font-family:inherit}",
    "svg.chart .bar{fill:var(--series-1)}",
    "svg.chart .bar.best{fill:var(--series-2)}",
    "svg.chart path.bar{stroke:none}",
    // Invisible until hovered: the wide rects exist to give a pointer
    // somewhere to land for the native <title> tooltip.
    "svg.chart .hit{fill:transparent}",
    "svg.chart .hit:hover{fill:var(--chart-line);fill-opacity:.35}",
    "</style>",
);

fn open_svg(svg: &mut String) {
    // No `preserveAspectRatio="none"`. With it, the viewBox stretched to the
    // container width while the height stayed fixed, so every glyph and stroke
    // was distorted horizontally. Uniform scaling plus the `height: auto` in
    // CHART_STYLE keeps the geometry honest.
    let _ = write!(
        svg,
        r#"<svg class="chart" viewBox="0 0 {W} {H}" role="img">"#
    );
    svg.push_str(CHART_STYLE);
}

fn y_axis(svg: &mut String, lo: f64, hi: f64, step: f64, unit: Unit) {
    let plot_h = H - PAD_T - PAD_B;
    let mut value = lo;
    while value <= hi + step / 2.0 {
        let gy = H - PAD_B - (value - lo) / (hi - lo) * plot_h;
        let _ = write!(
            svg,
            r#"<line class="grid" x1="{PAD_L}" y1="{gy:.1}" x2="{:.1}" y2="{gy:.1}"/>"#,
            W - PAD_R
        );
        let _ = write!(
            svg,
            r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
            PAD_L - 8.0,
            gy + 3.5,
            escape(&unit.tick(value.max(0.0) as u64))
        );
        value += step;
    }
}

/// Price (or stock) over time. Up to two series.
pub fn line_chart(series: &[Series<'_>], unit: Unit, empty_note: &str) -> String {
    let all: Vec<&Point> = series.iter().flat_map(|s| s.points.iter()).collect();
    if all.len() < 2 {
        return placeholder(empty_note);
    }

    let (min_t, max_t) = (
        all.iter().map(|p| p.at.get()).min().unwrap(),
        all.iter().map(|p| p.at.get()).max().unwrap(),
    );
    let raw_min = all.iter().map(|p| p.price.get()).min().unwrap();
    let raw_max = all.iter().map(|p| p.price.get()).max().unwrap();

    // A line chart may sit off zero -- bars may not. Forcing zero here would
    // squash a series that moves between 550 and 600 into a flat smear at the
    // top of the plot. The axis is labelled, so the truncation is visible.
    // Zero is still included when the data comes near it.
    let floor = if raw_min < (raw_max as f64 * 0.25) as u64 {
        0
    } else {
        raw_min
    };
    let (lo, hi, step) = nice_axis(floor, raw_max, 4);

    let span_t = (max_t - min_t).max(1);
    let x = |t: u64| PAD_L + (t - min_t) as f64 / span_t as f64 * (W - PAD_L - PAD_R);
    let y = |p: u64| H - PAD_B - (p as f64 - lo) / (hi - lo) * (H - PAD_T - PAD_B);

    let mut svg = String::with_capacity(8 * 1024);
    open_svg(&mut svg);
    y_axis(&mut svg, lo, hi, step, unit);

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
    // tooltip. Cheaper than a script and works without one.
    if let Some(first) = series.first() {
        let step_x = (W - PAD_L - PAD_R) / first.points.len().max(1) as f64;
        for (i, p) in first.points.iter().enumerate() {
            let cx = x(p.at.get());
            let mut tip = format!("{}\n", p.at.to_utc_string());
            for s in series {
                if let Some(sp) = s.points.get(i) {
                    let _ = writeln!(tip, "{}: {}", s.label, unit.value(sp.price.get()));
                }
            }
            if unit == Unit::Gold {
                let _ = write!(tip, "stock: {}", p.quantity);
            }
            let _ = write!(
                svg,
                r#"<rect class="hit" x="{:.1}" y="{PAD_T}" width="{:.1}" height="{:.1}"><title>{}</title></rect>"#,
                cx - step_x / 2.0,
                step_x.max(2.0),
                H - PAD_T - PAD_B,
                escape(tip.trim_end())
            );
        }
    }

    let _ = write!(
        svg,
        r#"<text class="axis" x="{PAD_L}" y="{:.1}">{}</text>"#,
        H - 9.0,
        Millis(min_t).to_date_string()
    );
    let _ = write!(
        svg,
        r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
        W - PAD_R,
        H - 9.0,
        Millis(max_t).to_date_string()
    );
    svg.push_str("</svg>");
    svg
}

/// Average price per repeating bucket -- hour of day, day of week.
///
/// One series, so no legend: the heading names it. Bars are anchored to zero,
/// which for a bar chart is not negotiable -- the length *is* the value.
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
    let (lo, hi, step) = nice_axis(0, max, 3);

    let slot = (W - PAD_L - PAD_R) / cycles.len() as f64;
    // A 2px surface gap between adjacent bars, not a border around them.
    let bar_w = (slot - 2.0).max(1.0);

    let mut svg = String::with_capacity(4 * 1024);
    open_svg(&mut svg);
    y_axis(&mut svg, lo, hi, step, Unit::Gold);

    let plot_h = H - PAD_T - PAD_B;
    for (i, cycle) in cycles.iter().enumerate() {
        if cycle.samples == 0 {
            continue;
        }
        let h = (cycle.mean.get() as f64 - lo) / (hi - lo) * plot_h;
        let bx = PAD_L + i as f64 * slot + 1.0;
        let by = H - PAD_B - h;
        // The cheapest bucket answers "when do I buy", so it is marked rather
        // than left for the reader to find.
        let cls = if cycle.mean.get() == cheapest {
            "bar best"
        } else {
            "bar"
        };
        // Rounded data-end only: the baseline end stays square, so the bar
        // reads as anchored rather than floating.
        let r = 3f64.min(bar_w / 2.0).min(h);
        let path = format!(
            "M{bx:.1} {:.1} L{bx:.1} {:.1} Q{bx:.1} {by:.1} {:.1} {by:.1} L{:.1} {by:.1} Q{:.1} {by:.1} {:.1} {:.1} L{:.1} {:.1} Z",
            H - PAD_B,
            by + r,
            bx + r,
            bx + bar_w - r,
            bx + bar_w,
            bx + bar_w,
            by + r,
            bx + bar_w,
            H - PAD_B
        );
        let _ = write!(
            svg,
            r#"<path class="{cls}" d="{path}"><title>{}: {} ({} samples)</title></path>"#,
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
                H - 9.0,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_steps_are_round_numbers() {
        // The old axis was plain fractions of the maximum, which produced
        // ticks like 0, 154, 308, 462, 617.
        for rough in [0.9, 1.0, 1.7, 2.4, 3.0, 7.0, 13.0, 260.0, 4_321.0] {
            let step = nice_step(rough);
            let mantissa = step / 10f64.powf(step.log10().floor());
            assert!(
                [1.0, 2.0, 2.5, 5.0]
                    .iter()
                    .any(|m| (m - mantissa).abs() < 1e-9),
                "step {step} for {rough} is not a nice number (mantissa {mantissa})"
            );
            assert!(
                step >= rough / 10.0,
                "step {step} far too small for {rough}"
            );
        }
    }

    #[test]
    fn axis_bounds_enclose_the_data_on_whole_steps() {
        let (lo, hi, step) = nice_axis(120, 880, 4);
        assert!(lo <= 120.0 && hi >= 880.0, "bounds {lo}..{hi} exclude data");
        assert!((lo / step).fract().abs() < 1e-9, "lo is not on a step");
        assert!((hi / step).fract().abs() < 1e-9, "hi is not on a step");
    }

    #[test]
    fn a_flat_series_still_gets_a_usable_axis() {
        // Every sample identical: without a guard the plot height is zero and
        // every division blows up.
        let (lo, hi, step) = nice_axis(500, 500, 4);
        assert!(hi > lo, "a flat series collapsed the axis");
        assert!(step > 0.0);
    }

    #[test]
    fn gold_ticks_keep_precision_on_cheap_items() {
        // A 3-gold item used to render an axis of 0g, 0g, 1g, 2g, 3g.
        assert_eq!(Unit::Gold.tick(0), "0");
        assert_eq!(Unit::Gold.tick(4_500), "45s");
        assert_eq!(Unit::Gold.tick(35_000), "3.5g");
        assert_eq!(Unit::Gold.tick(6_170_000), "617g");
        assert_eq!(Unit::Gold.tick(23_000_000), "2k g");
    }

    #[test]
    fn counts_are_not_formatted_as_money() {
        // The stock chart passed unit counts through the money formatter, so
        // 9650 listed units rendered as "0g".
        assert_eq!(Unit::Count.tick(650), "650");
        assert_eq!(Unit::Count.tick(9_650), "10k");
        assert_eq!(Unit::Count.tick(2_400_000), "2.4M");
        assert_ne!(Unit::Count.tick(9_650), Unit::Gold.tick(9_650));
    }

    fn points(prices: &[u64]) -> Vec<Point> {
        prices
            .iter()
            .enumerate()
            .map(|(i, p)| Point {
                at: Millis(i as u64 * 3_600_000),
                price: Copper(*p),
                quantity: 100,
            })
            .collect()
    }

    #[test]
    fn a_line_chart_scales_uniformly() {
        let pts = points(&[100, 200, 150]);
        let svg = line_chart(
            &[Series {
                label: "p",
                points: &pts,
                slot: 0,
            }],
            Unit::Gold,
            "empty",
        );
        assert!(
            !svg.contains("preserveAspectRatio"),
            "non-uniform scaling distorts every glyph"
        );
        assert!(svg.contains("viewBox=\"0 0 760 260\""));
        assert!(svg.contains("<path"), "the series was not drawn");
        assert!(svg.contains("<title>"), "hover tooltips are missing");
    }

    /// A chart must not depend on the app stylesheet.
    ///
    /// This is the regression that broke every graph at once: the rules lived
    /// in `style.css`, a rewrite of that file dropped them, and nothing failed
    /// to build. The charts rendered unsized, unlabelled, with the invisible
    /// hover targets showing as solid slabs.
    #[test]
    fn a_chart_carries_everything_it_needs_to_render() {
        let svg = line_chart(
            &[Series {
                label: "Rank 1",
                slot: 0,
                points: &points(&[100, 200, 150, 300]),
            }],
            Unit::Gold,
            "no data",
        );

        assert!(svg.contains("<style>"), "the chart brings its own styling");
        assert!(
            svg.contains("width:100%") && svg.contains("height:auto"),
            "an SVG with only a viewBox and no sizing overflows its container"
        );
        for hook in [".grid", ".axis", ".hit", ".bar"] {
            assert!(
                svg.contains(&format!("svg.chart {hook}")),
                "{hook} is styled by the chart itself, not by a stylesheet \
                 that has never heard of charts"
            );
        }
        assert!(
            svg.contains("--pico-muted-border-color,"),
            "colours follow the theme but fall back to a literal"
        );
    }

    /// The legend draws from `SERIES_COLOURS`; the lines draw from the
    /// variables in `CHART_STYLE`. They have to be the same colours, and only
    /// this test says so -- `concat!` cannot interpolate the constants.
    #[test]
    fn the_legend_and_the_lines_agree() {
        for (slot, colour) in SERIES_COLOURS.iter().enumerate() {
            assert!(
                CHART_STYLE.contains(colour),
                "slot {slot} is {colour} in the legend but not in the chart"
            );
        }
    }

    /// Every series colour has to resolve, or the line is drawn in nothing.
    #[test]
    fn series_colours_are_defined_by_the_chart() {
        for (slot, series) in SERIES.iter().enumerate() {
            let name = series.trim_start_matches("var(").trim_end_matches(')');
            assert!(
                CHART_STYLE.contains(&format!("{name}:")),
                "series slot {slot} uses {name}, which nothing defines"
            );
        }
    }

    #[test]
    fn a_short_series_renders_a_note_not_a_broken_chart() {
        let pts = points(&[100]);
        let svg = line_chart(
            &[Series {
                label: "p",
                points: &pts,
                slot: 0,
            }],
            Unit::Gold,
            "not enough yet",
        );
        assert!(svg.contains("not enough yet"));
        assert!(!svg.contains("<svg"));
    }

    #[test]
    fn markup_from_labels_is_escaped() {
        let cycles: Vec<Cycle> = (0..3)
            .map(|b| Cycle {
                bucket: b,
                mean: Copper(1_000 * (b as u64 + 1)),
                samples: 5,
            })
            .collect();
        let svg = bar_chart(&cycles, &|_| "<script>x</script>".into(), "empty");
        assert!(!svg.contains("<script>"), "label was not escaped");
        assert!(svg.contains("&lt;script&gt;"));
    }

    #[test]
    fn bars_are_anchored_to_the_baseline() {
        let cycles: Vec<Cycle> = (0..4)
            .map(|b| Cycle {
                bucket: b,
                mean: Copper(10_000 * (b as u64 + 1)),
                samples: 5,
            })
            .collect();
        let svg = bar_chart(&cycles, &|b| format!("{b}"), "empty");
        // A bar chart must start at zero: the length is the value.
        assert!(svg.contains(r#"<text class="axis" x="56.0""#));
        assert!(
            svg.contains("class=\"bar best\""),
            "cheapest is highlighted"
        );
        assert_eq!(svg.matches("<path class=").count(), 4);
    }
}
