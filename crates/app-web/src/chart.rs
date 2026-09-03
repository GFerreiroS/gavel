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

use app_core::market::correlate::Heatmap;
use app_core::market::depth::Ladder;
use app_core::market::engine::Spark;
use app_core::market::series::{ChartPoint, ChartSeries, Histogram};
use app_core::market::{Buckets, Copper, Gates, Insufficient, Point};
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
// The right-hand quantity axis needs the same room as the price axis. Keeping
// the shared charts inside one geometry is what makes their plot areas line
// up, instead of leaving a second scale hanging off the edge.
const PAD_R: f64 = 64.0;
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

/// Escape for SVG, which the templates render with `|safe` because this
/// module builds the markup itself.
///
/// Quotes as well as the three characters element text needs. Everything
/// interpolated here today lands in element content -- a `<title>`, a `<text>`
/// -- where `&<>` would be enough, and one of those `<title>`s carries a
/// series label, which on the gear pages is a realm name that came from
/// Blizzard. The day somebody puts one of these in an `x="…"` instead, the
/// quotes are the difference between a chart and an injection, and finding
/// that out then is worse than the two extra replacements now.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
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
    /// Whether the tooltip should describe the points' stock dimension.
    pub show_stock: bool,
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
    // The price band. The fill is the same hue as the median line at low
    // opacity, so the two read as one statement about one market rather than
    // as two series -- which is what they are.
    "svg.chart .band-fill{fill:var(--series-1);fill-opacity:.16;stroke:none}",
    "svg.chart .band-median{fill:none;stroke:var(--series-1);stroke-width:2;",
    "stroke-linejoin:round;stroke-linecap:round}",
    // The raw observation is thin and faint: it is what the median is a
    // summary *of*, and drawn at equal weight it would compete with it.
    "svg.chart .band-raw{fill:none;stroke:var(--series-1);stroke-opacity:.45;",
    "stroke-width:1;stroke-linejoin:round}",
    // The price is an area, while supply is a line on its own right-hand axis.
    // Hue plus the labelled axes make the two scales readable without relying
    // on a reader to infer them from the shapes alone.
    "svg.chart .price-area{fill:var(--series-1);fill-opacity:.16;stroke:none}",
    "svg.chart .stock-line{fill:none;stroke:var(--series-2);stroke-width:2;",
    "stroke-linejoin:round;stroke-linecap:round}",
    "svg.chart .axis-price{fill:var(--series-1)}",
    "svg.chart .axis-stock{fill:var(--series-2)}",
    // Today, as a rule across the plot. Dashed, because it is a reference
    // line rather than a measurement over time.
    "svg.chart .band-now{stroke:var(--series-2);stroke-width:1.5;",
    "stroke-dasharray:4 3}",
    // The weekly grid. One hue at varying opacity rather than a rainbow: the
    // ordering stays legible without a legend, and it survives being
    // colour-blind. A hole is an outline, so it cannot be mistaken for the
    // pale end of the scale.
    "svg.chart .heat-cell{fill:var(--series-1);stroke:none}",
    "svg.chart .heat-gap{fill:none;stroke:var(--chart-line);stroke-width:.5;",
    "stroke-dasharray:2 2}",
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
            if unit == Unit::Gold && first.show_stock {
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

/// What a chart says when it has nothing to draw.
///
/// A sentence rather than an empty box: §2's rule that unavailable data is
/// rendered unavailable, and a chart is where an empty box is most easily read
/// as a market at zero.
fn placeholder(note: &str) -> String {
    format!(r#"<p class="chart-empty muted">{}</p>"#, escape(note))
}

// --- the analysis page's price band --------------------------------------

/// The minimum evidence before a chart is allowed to claim a shape.
const CHART_GATES: Gates = Gates {
    median: 6,
    tails: 6,
    coverage: 0,
};

fn chart_admission(series: &ChartSeries) -> Result<(), Insufficient> {
    // This uses the same gate and refusal vocabulary as every valuation: a
    // chart with four samples is not a weaker chart, it is no chart at all.
    // Coverage is intentionally absent here. The stored series records gaps
    // visibly, and §12's chart rule sets a six-snapshot threshold rather than
    // a second, unrelated coverage threshold.
    CHART_GATES.admit(series.observed() as u32, None)
}

fn refused_chart(empty_note: &str, reason: Insufficient) -> String {
    match reason {
        Insufficient::NotEnoughHistory { have, need } => {
            placeholder(&format!("{empty_note} ({have} of {need} snapshots)"))
        }
        Insufficient::TooManyGaps { coverage, need } => placeholder(&format!(
            "{empty_note} ({coverage}% coverage; need {need}%)"
        )),
    }
}

/// `min(observed max, P95 × 1.1)`, using the shared R8 percentile estimator.
///
/// A lone absurd listing must remain available in its tooltip, but cannot be
/// allowed to flatten every ordinary observation into one straight line.
fn clipped_price_max(observed: &[&ChartPoint]) -> u64 {
    let buckets = Buckets::from_observations(observed.iter().map(|point| (point.at, point.price)));
    let observed_max = observed
        .iter()
        .map(|point| point.price.get())
        .max()
        .expect("the caller checks evidence before clipping");
    let p95 = buckets
        .quantile(0.95)
        .expect("the caller checks evidence before clipping")
        .get();
    observed_max.min(p95.saturating_mul(11).div_ceil(10))
}

/// §12: no daily series below this many calendar days.
///
/// A chart over two days is not a trend; it is two data points that happen to
/// occupy adjacent pixels. The reader has no way to tell a three-day spike
/// from a three-day market, and drawing the band implies that a pattern
/// exists to read. Fifteen days is the minimum before the band makes a claim
/// worth reading.
const CHART_MIN_DAYS: u64 = 15;
const MILLIS_PER_DAY: u64 = 86_400_000;

fn right_y_axis(svg: &mut String, lo: f64, hi: f64, step: f64, unit: Unit) {
    let plot_h = H - PAD_T - PAD_B;
    let mut value = lo;
    while value <= hi + step / 2.0 {
        let gy = H - PAD_B - (value - lo) / (hi - lo) * plot_h;
        let _ = write!(
            svg,
            r#"<text class="axis axis-stock" x="{:.1}" y="{:.1}">{}</text>"#,
            W - PAD_R + 8.0,
            gy + 3.5,
            escape(&unit.tick(value.max(0.0) as u64))
        );
        value += step;
    }
}

/// Price and listed stock over time, with the price inside a rolling P25--P75
/// band and gaps left as gaps.
///
/// `docs/market-analysis.md` §6 asks for exactly this rather than a line
/// through every observation, and the reason is what the two marks are for. A
/// raw line answers "what was the price at 03:00 on Tuesday", which is a
/// question about one hour. The band answers "what has this been worth, and
/// how tightly" -- and the two together are what make a spike *legible as a
/// spike* instead of as the market having moved.
///
/// **A gap is drawn as a gap.** A slot nothing was collected in breaks the
/// line and the band; §15's rule that unavailable data is never invented
/// applies most sharply to a chart, which will happily draw a confident
/// straight line across a week nobody looked at.
pub fn band_chart(series: &ChartSeries, current: Option<Copper>, empty_note: &str) -> String {
    let observed: Vec<&ChartPoint> = series.points.iter().filter(|p| p.observed).collect();
    if let Err(reason) = chart_admission(series) {
        return refused_chart(empty_note, reason);
    }

    // §12: no daily series below 15 days. This is a separate gate from the
    // snapshot count: six snapshots in two days is thin in a different way
    // from six snapshots spread over three weeks -- the band would be claiming
    // a trend across a window that is almost entirely extrapolation.
    let span_days = series.until.get().saturating_sub(series.from.get()) / MILLIS_PER_DAY;
    if span_days < CHART_MIN_DAYS {
        return placeholder(&format!(
            "{empty_note} ({span_days} of {CHART_MIN_DAYS} days)"
        ));
    }

    let (min_t, max_t) = (
        series.from.get(),
        series.until.get().max(series.from.get() + 1),
    );
    let raw_min = observed
        .iter()
        .map(|p| p.price.get().min(p.p25.get()))
        .min()
        .expect("not empty");
    let raw_max = clipped_price_max(&observed);

    // Same rule as `line_chart`: a price chart may sit off zero, and the axis
    // is labelled so the truncation is visible.
    let floor = if raw_min < (raw_max as f64 * 0.25) as u64 {
        0
    } else {
        raw_min
    };
    let (lo, hi, step) = nice_axis(floor, raw_max, 4);
    let stock_max = observed
        .iter()
        .map(|point| point.quantity)
        .max()
        .expect("not empty");
    let (stock_lo, stock_hi, stock_step) = nice_axis(0, stock_max, 4);
    let span_t = (max_t - min_t).max(1);
    let x = |t: u64| PAD_L + (t.saturating_sub(min_t)) as f64 / span_t as f64 * (W - PAD_L - PAD_R);
    let y = |p: u64| H - PAD_B - (p.min(hi as u64) as f64 - lo) / (hi - lo) * (H - PAD_T - PAD_B);
    let stock_y = |quantity: u64| {
        H - PAD_B - (quantity as f64 - stock_lo) / (stock_hi - stock_lo) * (H - PAD_T - PAD_B)
    };

    let mut svg = String::with_capacity(16 * 1024);
    open_svg(&mut svg);
    y_axis(&mut svg, lo, hi, step, unit_gold());
    right_y_axis(&mut svg, stock_lo, stock_hi, stock_step, Unit::Count);
    let _ = write!(
        svg,
        r#"<text class="axis axis-price" x="{PAD_L}" y="12">Price · gold</text><text class="axis axis-stock" x="{:.1}" y="12" text-anchor="end">Stock · units listed</text>"#,
        W - PAD_R
    );

    // One band and one line per *run* of observed slots. A run is what a break
    // in the data leaves behind, and drawing runs rather than the whole series
    // is what makes the break visible instead of bridged.
    for run in runs(&series.points) {
        if run.len() < 2 {
            continue;
        }
        // The band is a closed shape: P75 forward, P25 back.
        let mut band = String::with_capacity(run.len() * 24);
        for (i, p) in run.iter().enumerate() {
            let _ = write!(
                band,
                "{}{:.1} {:.1}",
                if i == 0 { "M" } else { "L" },
                x(p.at.get()),
                y(p.p75.get())
            );
            band.push(' ');
        }
        for p in run.iter().rev() {
            let _ = write!(band, "L{:.1} {:.1} ", x(p.at.get()), y(p.p25.get()));
        }
        band.push('Z');
        let _ = write!(svg, r#"<path class="band-fill" d="{}"/>"#, band.trim());

        let line = |pick: &dyn Fn(&ChartPoint) -> u64| -> String {
            run.iter()
                .enumerate()
                .map(|(i, p)| {
                    format!(
                        "{}{:.1} {:.1}",
                        if i == 0 { "M" } else { "L" },
                        x(p.at.get()),
                        y(pick(p))
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        // The observation under the median, so the median reads as the summary
        // of it rather than as a second series competing with it.
        let _ = write!(
            svg,
            r#"<path class="band-raw" d="{}"/>"#,
            line(&|p| p.price.get())
        );
        let _ = write!(
            svg,
            r#"<path class="band-median" d="{}"/>"#,
            line(&|p| p.median.get())
        );

        // The price area's baseline is the actual axis floor, not zero: a
        // price axis may begin above zero without turning the shape into a
        // false zero-priced observation.
        let mut area = line(&|p| p.price.get());
        let _ = write!(
            area,
            " L{:.1} {:.1} L{:.1} {:.1} Z",
            x(run.last().expect("run is not empty").at.get()),
            H - PAD_B,
            x(run[0].at.get()),
            H - PAD_B
        );
        let _ = write!(svg, r#"<path class="price-area" d="{}"/>"#, area);

        let stock = run
            .iter()
            .enumerate()
            .map(|(i, point)| {
                format!(
                    "{}{:.1} {:.1}",
                    if i == 0 { "M" } else { "L" },
                    x(point.at.get()),
                    stock_y(point.quantity)
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let _ = write!(svg, r#"<path class="stock-line" d="{stock}"/>"#);
    }

    // Where the market is now, as a rule across the plot. §6 asks for the
    // current value on this panel: without it the reader has to find the right
    // edge of a line that may end in a gap.
    if let Some(price) = current
        && price.get() >= lo as u64
        && price.get() <= hi as u64
    {
        let cy = y(price.get());
        let _ = write!(
            svg,
            r#"<line class="band-now" x1="{PAD_L}" y1="{cy:.1}" x2="{:.1}" y2="{cy:.1}"><title>{}</title></line>"#,
            W - PAD_R,
            escape(&format!("now: {}", unit_gold().value(price.get())))
        );
    }

    // Hover strips, as on every other chart: a native tooltip, no script. A
    // gap gets one too, and says so -- "nothing collected" is the answer the
    // reader came for when they see a break.
    let step_x = (W - PAD_L - PAD_R) / series.points.len().max(1) as f64;
    for point in &series.points {
        let cx = x(point.at.get());
        let tip = if point.observed {
            format!(
                "{}\nprice: {}\nmedian: {}\nP25-P75: {} - {}\nstock: {} in {} auctions",
                point.at.to_utc_string(),
                unit_gold().value(point.price.get()),
                unit_gold().value(point.median.get()),
                unit_gold().value(point.p25.get()),
                unit_gold().value(point.p75.get()),
                point.quantity,
                point.listings,
            )
        } else {
            format!("{}\nnothing collected", point.at.to_utc_string())
        };
        let _ = write!(
            svg,
            r#"<rect class="hit" x="{:.1}" y="{PAD_T}" width="{:.1}" height="{:.1}"><title>{}</title></rect>"#,
            cx - step_x / 2.0,
            step_x.max(2.0),
            H - PAD_T - PAD_B,
            escape(&tip)
        );
    }

    time_axis(&mut svg, min_t, max_t);
    svg.push_str("</svg>");
    svg
}

/// Contiguous runs of observed slots.
///
/// The unit a band and a line are drawn in: a break in the data has to be a
/// break in the ink, or the chart asserts something nobody measured.
fn runs(points: &[ChartPoint]) -> Vec<Vec<&ChartPoint>> {
    let mut out: Vec<Vec<&ChartPoint>> = Vec::new();
    let mut run: Vec<&ChartPoint> = Vec::new();
    for point in points {
        if point.observed {
            run.push(point);
        } else if !run.is_empty() {
            out.push(std::mem::take(&mut run));
        }
    }
    if !run.is_empty() {
        out.push(run);
    }
    out
}

fn unit_gold() -> Unit {
    Unit::Gold
}

fn time_axis(svg: &mut String, min_t: u64, max_t: u64) {
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
}

// --- the distribution ----------------------------------------------------

/// How often this market traded at each price, with today's price marked.
///
/// §5.4's panel, and the reason it exists beside the valuation band rather
/// than instead of it: a band says *where* today sits, and this says *what it
/// sits in*. `Cheap, P12` in a market whose prices span 20% is a different
/// proposition from `Cheap, P12` in one that has ranged over a factor of ten,
/// and no single percentile can carry that difference.
///
/// Bars count **hours**, not observations: the same equal-duration buckets
/// every other historical statistic here is computed over.
pub fn histogram_chart(histogram: &Histogram, current: Option<Copper>, empty_note: &str) -> String {
    if histogram.is_empty() {
        return placeholder(empty_note);
    }
    let tallest = histogram.tallest().max(1);
    let bins = histogram.bins.len().max(1);
    let plot_w = W - PAD_L - PAD_R;
    let plot_h = H - PAD_T - PAD_B;
    let width = plot_w / bins as f64;
    let here = current.and_then(|price| histogram.bin_of(price));

    let mut svg = String::with_capacity(6 * 1024);
    open_svg(&mut svg);

    // Bars are anchored to zero, which for a bar chart is not negotiable --
    // the length *is* the count.
    let (lo, hi) = (histogram.lo.get(), histogram.hi.get());
    let span = hi.saturating_sub(lo);
    for (index, count) in histogram.bins.iter().enumerate() {
        let height = *count as f64 / tallest as f64 * plot_h;
        let bx = PAD_L + index as f64 * width;
        let price = lo + span * index as u64 / (bins.max(2) - 1) as u64;
        let class = if here == Some(index) {
            "bar best"
        } else {
            "bar"
        };
        let _ = write!(
            svg,
            r#"<rect class="{class}" x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}"><title>{}</title></rect>"#,
            bx + 1.0,
            H - PAD_B - height,
            (width - 2.0).max(1.0),
            height,
            escape(&format!(
                "around {}\n{count} hours",
                Unit::Gold.value(price)
            ))
        );
    }

    // The axis is prices, so it is labelled with prices: the two ends and the
    // middle, which is as many as fit without overlapping.
    for (fraction, anchor) in [(0.0, "start"), (0.5, "middle"), (1.0, "end")] {
        let price = lo + (span as f64 * fraction) as u64;
        let _ = write!(
            svg,
            r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="{anchor}">{}</text>"#,
            PAD_L + plot_w * fraction,
            H - 9.0,
            escape(&Unit::Gold.tick(price))
        );
    }
    svg.push_str("</svg>");
    svg
}

// --- the weekly heatmap --------------------------------------------------

/// Median price by hour of the week: seven rows of twenty-four.
///
/// Replaces the two separate cycle bar charts, and the reason is that they
/// could not answer the question together. "Cheapest at 04:00" and "cheapest
/// on Tuesday" do not compose into "cheapest at 04:00 on Tuesday" -- the whole
/// point of a weekly rhythm is that the hour and the day interact, because a
/// reset happens at one hour on one day.
///
/// **A hole is drawn as a hole**, not as the cheapest colour. That is the one
/// failure mode this panel has: a reader looking at a mostly-empty grid reads
/// the gaps as cheapness, which is why `Heatmap::is_usable` refuses to draw
/// one at all below its gate.
pub fn heatmap_chart(map: &Heatmap, weekdays: &[String], empty_note: &str) -> String {
    if !map.is_usable() {
        return placeholder(empty_note);
    }
    let Some((lo, hi)) = map.range() else {
        return placeholder(empty_note);
    };

    // Its own geometry: seven rows is a short chart, and the row labels need
    // more room on the left than a numeric axis does.
    const LABEL_W: f64 = 34.0;
    const ROW_H: f64 = 22.0;
    let top = 16.0;
    let height = top + 7.0 * ROW_H + 20.0;
    let cell_w = (W - LABEL_W - PAD_R) / 24.0;
    let span = hi.get().saturating_sub(lo.get()).max(1);

    let mut svg = String::with_capacity(24 * 1024);
    let _ = write!(
        svg,
        r#"<svg class="chart heat" viewBox="0 0 {W} {height:.0}" role="img">"#
    );
    svg.push_str(CHART_STYLE);

    for hour in (0..24).step_by(4) {
        let _ = write!(
            svg,
            r#"<text class="axis" x="{:.1}" y="12" text-anchor="middle">{hour:02}</text>"#,
            LABEL_W + (hour as f64 + 0.5) * cell_w
        );
    }

    for (day, label) in weekdays.iter().enumerate().take(7) {
        let y = top + day as f64 * ROW_H;
        let _ = write!(
            svg,
            r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
            LABEL_W - 6.0,
            y + ROW_H / 2.0 + 3.5,
            escape(label)
        );
        for hour in 0..24 {
            let x = LABEL_W + hour as f64 * cell_w;
            match map.cells.get(day * 24 + hour).copied().flatten() {
                Some(price) => {
                    // Cheap is dark, dear is pale: one hue at varying opacity
                    // rather than a rainbow, so the ordering is legible
                    // without a legend and survives being colour-blind.
                    let share = (price.get() - lo.get()) as f64 / span as f64;
                    let opacity = 0.88 - share * 0.72;
                    let _ = write!(
                        svg,
                        r#"<rect class="heat-cell" x="{x:.1}" y="{y:.1}" width="{:.1}" height="{:.1}" opacity="{opacity:.2}"><title>{}</title></rect>"#,
                        cell_w - 1.0,
                        ROW_H - 1.0,
                        escape(&format!(
                            "{label} {hour:02}:00 UTC\n{}",
                            Unit::Gold.value(price.get())
                        ))
                    );
                }
                // A hole: outlined, empty, and hoverable so it can say what it
                // is. Drawing nothing at all would leave the page background
                // showing, which reads as one more shade of the scale.
                None => {
                    let _ = write!(
                        svg,
                        r#"<rect class="heat-gap" x="{x:.1}" y="{y:.1}" width="{:.1}" height="{:.1}"><title>{}</title></rect>"#,
                        cell_w - 1.0,
                        ROW_H - 1.0,
                        escape(&format!("{label} {hour:02}:00 UTC\nnothing collected"))
                    );
                }
            }
        }
    }
    svg.push_str("</svg>");
    svg
}

// --- the depth curve -----------------------------------------------------

/// The supply curve: price along the bottom, cumulative units up the side.
///
/// A step chart rather than a line, because supply *is* a step function --
/// there are twenty units at 100 and none at 101, and drawing a slope between
/// them would invent an offer nobody made. It is the same rule the price
/// chart's gaps follow, applied to the other axis.
///
/// Reading it: how far right you have to go to get the height you need is what
/// buying that many costs. A curve that climbs steeply near the left is a deep
/// market; one that is flat until it jumps is a thin market with a wall on it.
pub fn depth_chart(ladder: &Ladder, target: u64, empty_note: &str) -> String {
    if ladder.levels() < 2 {
        return placeholder(empty_note);
    }
    let total = ladder.total();
    let (lo, hi) = match (ladder.cheapest(), ladder.dearest()) {
        (Some(lo), Some(hi)) if hi > lo => (lo.get(), hi.get()),
        _ => return placeholder(empty_note),
    };

    let span = (hi - lo).max(1);
    let x =
        |price: u64| PAD_L + (price.saturating_sub(lo)) as f64 / span as f64 * (W - PAD_L - PAD_R);
    let y = |units: u64| H - PAD_B - units as f64 / total.max(1) as f64 * (H - PAD_T - PAD_B);

    let mut svg = String::with_capacity(8 * 1024);
    open_svg(&mut svg);

    // Horizontal gridlines in units, so the height can be read off.
    let (glo, ghi, step) = nice_axis(0, total, 4);
    let mut value = glo;
    while value <= ghi + step / 2.0 {
        let gy = y(value as u64);
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
            escape(&Unit::Count.tick(value.max(0.0) as u64))
        );
        value += step;
    }

    // The staircase, and the area under it.
    let mut path = format!("M{:.1} {:.1}", x(lo), y(0));
    let mut last = 0u64;
    for step_ in &ladder.steps {
        // Across at the height we were, then up to the new one: a step, not a
        // slope. The horizontal segment is the range of prices at which that
        // much supply is available, which is a real fact about the market.
        let _ = write!(path, " L{:.1} {:.1}", x(step_.price.get()), y(last));
        let _ = write!(
            path,
            " L{:.1} {:.1}",
            x(step_.price.get()),
            y(step_.cumulative)
        );
        last = step_.cumulative;
    }
    let _ = write!(
        svg,
        r#"<path class="band-fill" d="{path} L{:.1} {:.1} L{:.1} {:.1} Z"/>"#,
        x(hi),
        y(0),
        x(lo),
        y(0)
    );
    let _ = write!(svg, r#"<path class="band-median" d="{path}"/>"#);

    // The target quantity as a rule across it: where that line meets the
    // staircase is what the order costs.
    if target > 0 && target <= total {
        let ty = y(target);
        let _ = write!(
            svg,
            r#"<line class="band-now" x1="{PAD_L}" y1="{ty:.1}" x2="{:.1}" y2="{ty:.1}"><title>{}</title></line>"#,
            W - PAD_R,
            escape(&format!("{target} units"))
        );
    }

    // Hover strips per rung.
    for (index, rung) in ladder.steps.iter().enumerate() {
        let left = if index == 0 {
            x(lo)
        } else {
            x(ladder.steps[index - 1].price.get())
        };
        let right = x(rung.price.get());
        let _ = write!(
            svg,
            r#"<rect class="hit" x="{:.1}" y="{PAD_T}" width="{:.1}" height="{:.1}"><title>{}</title></rect>"#,
            left,
            (right - left).max(2.0),
            H - PAD_T - PAD_B,
            escape(&format!(
                "{}\n{} units at this price\n{} units at or below it",
                Unit::Gold.value(rung.price.get()),
                rung.quantity,
                rung.cumulative
            ))
        );
    }

    // The axis is prices.
    for (fraction, anchor) in [(0.0, "start"), (1.0, "end")] {
        let price = lo + (span as f64 * fraction) as u64;
        let _ = write!(
            svg,
            r#"<text class="axis" x="{:.1}" y="{:.1}" text-anchor="{anchor}">{}</text>"#,
            PAD_L + (W - PAD_L - PAD_R) * fraction,
            H - 9.0,
            escape(&Unit::Gold.tick(price))
        );
    }
    svg.push_str("</svg>");
    svg
}

// --- sparkline -----------------------------------------------------------

/// The sparkline's own box. Small, and wide rather than tall: it sits inside a
/// card between the price and the figures, where a reader wants the shape of
/// the last fortnight and not a chart to read values off. There are no axes
/// for that reason -- the numbers it would label are printed underneath it.
const SPARK_W: f64 = 100.0;
const SPARK_H: f64 = 24.0;

/// One market's shape over the reader's comparison window.
///
/// Takes [`Spark`]'s equal-duration slots, so the horizontal really is time
/// and a gap really is a gap: the line breaks at a slot nothing was observed
/// in rather than being drawn straight through it, which would invent the very
/// data §15 says is never invented.
///
/// Returns an empty string for a market with no shape to draw. The caller
/// renders nothing at all rather than an empty box, because a card grid needs
/// every card the same height and an empty box is not a smaller thing than a
/// line -- it is the same height saying less.
pub fn sparkline(spark: &Spark, label: &str) -> String {
    if spark.is_empty() {
        return String::new();
    }
    let values: Vec<u64> = spark.slots.iter().flatten().map(|p| p.get()).collect();
    let (lo, hi) = (
        *values.iter().min().expect("not empty"),
        *values.iter().max().expect("not empty"),
    );
    // A flat market draws down the middle rather than dividing by zero. It is
    // also the honest picture: nothing moved.
    let span = (hi - lo) as f64;
    let slots = spark.slots.len().max(2);
    // Rounded to whole viewBox units, which costs nothing and is most of the
    // markup. The box is 100 by 24 and the line renders about 24 pixels tall,
    // so a unit is a pixel: `73.3,17.6` and `73,18` draw the same line, and
    // the first is nearly twice the bytes across several hundred cards.
    let x = |index: usize| (index as f64 / (slots - 1) as f64 * SPARK_W).round() as i32;
    let y = |price: u64| {
        if span == 0.0 {
            (SPARK_H / 2.0).round() as i32
        } else {
            // A 1px inset top and bottom so a peak's stroke is not clipped by
            // the viewBox.
            (SPARK_H - 1.0 - (price - lo) as f64 / span * (SPARK_H - 2.0)).round() as i32
        }
    };

    let mut svg = String::with_capacity(320);
    // No inline `<style>` and no per-element colour: the stroke is painted by
    // `.spark-line` in the stylesheet. `SERIES_COLOURS` is still the one
    // source of truth -- the comment on it records what happened when the
    // palette lived in two places at once -- and `the_stylesheet_paints_the
    // _sparkline_the_series_colour` is what now keeps the two in step. A test
    // catches that drift once; a custom property on every SVG would pay for it
    // on every card of every page, for ever.
    let _ = write!(
        svg,
        r#"<svg class="spark" viewBox="0 0 {SPARK_W:.0} {SPARK_H:.0}" role="img" aria-label="{}" preserveAspectRatio="none">"#,
        escape(label)
    );

    // One polyline per run of observed slots. A break in the data is a break
    // in the line, which is the whole reason the slots are optional.
    let mut run: Vec<String> = Vec::new();
    let flush = |svg: &mut String, run: &mut Vec<String>| {
        match run.len() {
            0 => {}
            // A single observed slot between two gaps is a dot: a polyline of
            // one point draws nothing at all, and "nothing" is the wrong
            // picture for an observation that happened.
            1 => {
                let point = run[0].clone();
                let (px, py) = point.split_once(',').expect("written as x,y");
                let _ = write!(
                    svg,
                    r#"<circle class="spark-dot" cx="{px}" cy="{py}" r="1"/>"#
                );
            }
            _ => {
                let _ = write!(
                    svg,
                    r#"<polyline class="spark-line" points="{}"/>"#,
                    run.join(" ")
                );
            }
        }
        run.clear();
    };
    for (index, slot) in spark.slots.iter().enumerate() {
        match slot {
            Some(price) => run.push(format!("{},{}", x(index), y(price.get()))),
            None => flush(&mut svg, &mut run),
        }
    }
    flush(&mut svg, &mut run);

    // The latest observed slot, marked. A sparkline's rightmost point is the
    // one a reader is actually standing on, and on a line this short it is
    // otherwise indistinguishable from the rest of the stroke.
    if let Some((index, price)) = spark
        .slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|p| (i, p)))
        .next_back()
    {
        let _ = write!(
            svg,
            r#"<circle class="spark-now" cx="{}" cy="{}" r="1.8"/>"#,
            x(index),
            y(price.get())
        );
    }

    svg.push_str("</svg>");
    svg
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
                show_stock: true,
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

    #[test]
    fn a_price_series_without_stock_omits_the_stock_tooltip() {
        let pts = points(&[100, 200]);
        let svg = line_chart(
            &[Series {
                label: "WoW Token",
                points: &pts,
                slot: 0,
                show_stock: false,
            }],
            Unit::Gold,
            "empty",
        );
        assert!(!svg.contains("stock:"));
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
                show_stock: true,
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

    /// The card sparkline is painted by the stylesheet, so the stylesheet has
    /// to agree with the palette.
    ///
    /// The sparkline carries no `<style>` of its own and no per-element
    /// colour: a category page draws several hundred of them, and paying
    /// twenty-four bytes a card to restate a constant is the kind of thing
    /// Phase 3 spent its time removing. That leaves the value written down in
    /// two places, which is exactly the arrangement `SERIES_COLOURS` exists to
    /// prevent -- so this is the test that makes it safe: drift fails here
    /// rather than showing up as a line nobody can see.
    #[test]
    fn the_stylesheet_paints_the_sparkline_the_series_colour() {
        let stylesheet = include_str!("../static/style.css");
        let expected = series_colour(0);
        let painted: Vec<&str> = stylesheet
            .lines()
            .filter(|line| line.contains("stroke:") || line.contains("fill:"))
            .filter(|line| line.contains('#'))
            .collect();
        assert!(
            !painted.is_empty(),
            "no literal colour in the stylesheet: has .spark-line moved?"
        );
        for rule in [
            "stroke: #3b82f6;",
            ".spark-dot, .spark-now { fill: #3b82f6;",
        ] {
            assert!(
                stylesheet.contains(rule),
                "the stylesheet no longer contains `{rule}`; \
                 keep it equal to SERIES_COLOURS[0] ({expected})"
            );
        }
        assert_eq!(
            expected, "#3b82f6",
            "the palette moved: update .spark-line and .spark-dot in style.css to match"
        );
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
                show_stock: true,
            }],
            Unit::Gold,
            "not enough yet",
        );
        assert!(svg.contains("not enough yet"));
        assert!(!svg.contains("<svg"));
    }

    /// Row labels come from outside this module, so they are escaped.
    ///
    /// The heatmap's are translated weekday names, which are catalogue text
    /// rather than user input -- but the rule is that everything written
    /// through `|safe` is escaped on the way out, and a label that was safe
    /// today is a label somebody sources differently tomorrow.
    #[test]
    fn markup_from_labels_is_escaped() {
        let map = app_core::market::Heatmap::of(
            (0..(24 * 8u64)).map(|h| (Millis(h * 3_600_000), Copper(1_000 + h))),
        );
        let labels: Vec<String> = (0..7).map(|_| "<script>x</script>".to_string()).collect();
        let svg = heatmap_chart(&map, &labels, "empty");
        assert!(svg.contains("<svg"), "the grid was drawn at all");
        assert!(!svg.contains("<script>"), "label was not escaped");
        assert!(svg.contains("&lt;script&gt;"));
    }

    /// Everything this module writes goes to the page through `|safe`, so this
    /// escape is the only thing standing between upstream text -- a series
    /// label on the gear pages is a realm name Blizzard supplied -- and the
    /// markup. Quotes included, so that moving one of these into an attribute
    /// later is a layout change rather than an injection.
    #[test]
    fn a_label_can_break_out_of_neither_an_element_nor_an_attribute() {
        let escaped = escape(r#"<script>alert(1)</script> " ' &"#);
        for ch in ['<', '>', '"', '\''] {
            assert!(!escaped.contains(ch), "{ch:?} survived: {escaped}");
        }
        assert!(escaped.contains("&lt;script&gt;"), "{escaped}");
        assert!(escaped.contains("&quot;"), "{escaped}");
        assert!(escaped.contains("&#x27;"), "{escaped}");
        assert!(escaped.contains("&amp;"), "{escaped}");
    }

    /// A bar chart is anchored to zero, because the length *is* the value.
    ///
    /// The distribution histogram is the app's bar chart since the two cycle
    /// charts became a heatmap; the property it inherited is this one, and it
    /// is not negotiable for a bar.
    #[test]
    fn bars_are_anchored_to_the_baseline() {
        let buckets = app_core::market::engine::Buckets::from_observations(
            (0..80u64).map(|i| (Millis(i * 3_600_000), Copper(1_000 + i * 25))),
        );
        let histogram = Histogram::of(&buckets).expect("a populated market");
        let svg = histogram_chart(&histogram, Some(Copper(1_050)), "empty");

        // Every bar's foot is on the same line, which is the baseline.
        let feet: Vec<f64> = regex_free_heights(&svg);
        assert!(!feet.is_empty(), "bars were drawn");
        for foot in &feet {
            assert!(
                (foot - feet[0]).abs() < 0.05,
                "a bar does not start at the baseline: {feet:?}"
            );
        }
        assert!(
            svg.contains("class=\"bar best\""),
            "the current price's bin is marked"
        );
    }

    /// The y + height of every `rect.bar`, which is where its foot sits.
    fn regex_free_heights(svg: &str) -> Vec<f64> {
        let mut feet = Vec::new();
        for chunk in svg.split("<rect class=\"bar").skip(1) {
            let field = |name: &str| -> Option<f64> {
                let start = chunk.find(&format!("{name}=\""))? + name.len() + 2;
                let end = start + chunk[start..].find('"')?;
                chunk[start..end].parse().ok()
            };
            if let (Some(y), Some(h)) = (field("y"), field("height")) {
                feet.push(y + h);
            }
        }
        feet
    }

    /// Clipping arithmetic: an isolated absurd listing must not flatten the
    /// axis, but it must remain available in a tooltip.
    ///
    /// §12: `max = min(observed_max, P95 × 1.1)`. When the vast majority of
    /// observations are at a normal price and one is 100× the normal price,
    /// the clipped maximum should be near the normal price, not the outlier.
    ///
    /// Uses 40 normal values + 1 outlier so that, with the Type-8 estimator,
    /// P95 lands solidly within the normal range (index 39/41) rather than
    /// blending into the outlier.
    #[test]
    fn outlier_is_clipped_to_p95_times_1_1() {
        // 40 normal prices, then one extreme outlier (100×).
        let normal_price = 1_000_000u64; // 100 g
        let outlier_price = 100_000_000u64; // 10 000 g
        let points: Vec<ChartPoint> = (0..40u64)
            .map(|i| ChartPoint {
                at: Millis(i * 3_600_000),
                price: Copper(normal_price),
                observed: true,
                ..ChartPoint::default()
            })
            .chain(std::iter::once(ChartPoint {
                at: Millis(40 * 3_600_000),
                price: Copper(outlier_price),
                observed: true,
                ..ChartPoint::default()
            }))
            .collect();
        let observed_refs: Vec<&ChartPoint> = points.iter().collect();
        let clipped = clipped_price_max(&observed_refs);

        // With n=41 and the type-8 estimator, P95 lands at position 39.6
        // (1-indexed), which falls between the 39th and 40th values — both
        // `normal_price` — so P95 = normal_price exactly.
        // Clipped max = min(outlier, normal × 1.1) = normal × 1.1.
        let expected_max = (normal_price as f64 * 1.1) as u64;
        assert!(
            clipped <= expected_max + 1,
            "clipped max {clipped} should be ≤ p95×1.1 ≈ {expected_max}"
        );
        assert!(
            clipped >= normal_price,
            "clipped max {clipped} dropped below the normal price"
        );
        assert!(
            clipped < outlier_price,
            "outlier {outlier_price} was not clipped; axis max is {clipped}"
        );
    }

    /// When the same value is the both the observed max AND the p95, the clip
    /// must equal observed_max (i.e. nothing is clipped). This guards against
    /// a regression where the formula over-clips a uniform series.
    #[test]
    fn uniform_series_is_not_clipped() {
        let price = 500_000u64;
        let points: Vec<ChartPoint> = (0..10u64)
            .map(|i| ChartPoint {
                at: Millis(i * 3_600_000),
                price: Copper(price),
                observed: true,
                ..ChartPoint::default()
            })
            .collect();
        let observed_refs: Vec<&ChartPoint> = points.iter().collect();
        let clipped = clipped_price_max(&observed_refs);
        // p95 == price, so p95 × 1.1 > price: clip should NOT reduce
        // below the actual max.
        assert!(
            clipped >= price,
            "uniform series was clipped: max={clipped} but all prices={price}"
        );
    }

    /// §12 snapshot gate: fewer than 6 observed snapshots must be refused.
    ///
    /// The result must say why it refused (mention the count and the
    /// threshold) so the reader is not left looking at an empty panel with
    /// no explanation.
    #[test]
    fn band_chart_refuses_fewer_than_6_snapshots() {
        // 20-day window, but only 4 observed slots.
        let from = Millis(0);
        let until = Millis(20 * MILLIS_PER_DAY);
        let span = until.get() - from.get();
        // 4 evenly spaced observed points + the rest as gaps.
        let mut points = vec![
            ChartPoint {
                ..ChartPoint::default()
            };
            96
        ];
        for i in 0..4usize {
            let slot = i * 24; // spread across the 96 slots
            points[slot] = ChartPoint {
                at: Millis(from.get() + (slot as u64 * span) / 96),
                price: Copper(100_000),
                median: Copper(100_000),
                p25: Copper(80_000),
                p75: Copper(120_000),
                quantity: 50,
                listings: 5,
                observed: true,
            };
        }
        let series = ChartSeries {
            from,
            until,
            points,
        };

        let out = band_chart(&series, None, "no data yet");

        assert!(
            !out.contains("<svg"),
            "a 4-snapshot series must not draw an SVG"
        );
        assert!(
            out.contains("no data yet"),
            "the refusal must echo the caller's note"
        );
        // The refusal must say how many it had so the reader can see why.
        assert!(
            out.contains('4') || out.contains("4 of"),
            "refusal should mention the snapshot count: {out}"
        );
    }

    /// §12 daily-series gate: a window shorter than 15 days must be refused
    /// even when it has enough snapshots.
    #[test]
    fn band_chart_refuses_fewer_than_15_days() {
        // 8 snapshots (> 6), but only 7 days.
        let from = Millis(0);
        let until = Millis(7 * MILLIS_PER_DAY);
        let span = until.get() - from.get();
        let mut points = vec![
            ChartPoint {
                ..ChartPoint::default()
            };
            96
        ];
        // Place 8 observed slots across the series.
        for i in 0..8usize {
            let slot = i * 11; // 8 × 11 < 96
            points[slot] = ChartPoint {
                at: Millis(from.get() + (slot as u64 * span) / 96),
                price: Copper(100_000),
                median: Copper(100_000),
                p25: Copper(80_000),
                p75: Copper(120_000),
                quantity: 50,
                listings: 5,
                observed: true,
            };
        }
        let series = ChartSeries {
            from,
            until,
            points,
        };

        let out = band_chart(&series, None, "too short");

        assert!(
            !out.contains("<svg"),
            "a 7-day series must not draw an SVG even with enough snapshots"
        );
        assert!(
            out.contains("too short"),
            "the refusal must echo the caller's note"
        );
        // The refusal must mention the day count so the reader knows why.
        assert!(
            out.contains('7') || out.contains("7 of"),
            "refusal should mention the span in days: {out}"
        );
    }

    /// Both gates passed: a series spanning 20 days with 10 snapshots must
    /// produce an SVG.
    #[test]
    fn band_chart_draws_when_both_gates_pass() {
        let from = Millis(0);
        let until = Millis(20 * MILLIS_PER_DAY);
        let span = until.get() - from.get();
        let mut points = vec![
            ChartPoint {
                ..ChartPoint::default()
            };
            96
        ];
        // Place 10 evenly spaced observed slots.
        for i in 0..10usize {
            let slot = i * 9; // 10 × 9 = 90 < 96
            points[slot] = ChartPoint {
                at: Millis(from.get() + (slot as u64 * span) / 96),
                price: Copper(100_000),
                median: Copper(100_000),
                p25: Copper(80_000),
                p75: Copper(120_000),
                quantity: 50,
                listings: 5,
                observed: true,
            };
        }
        let series = ChartSeries {
            from,
            until,
            points,
        };

        let out = band_chart(&series, None, "no data");

        assert!(
            out.contains("<svg"),
            "a 20-day, 10-snapshot series should render an SVG"
        );
        // Both axis labels must be present.
        assert!(
            out.contains("axis-price") && out.contains("axis-stock"),
            "dual-axis labels must appear in the SVG"
        );
    }
}
