//! Scratch: what does the archive curve cost, and what does it lose?
//!
//! Reads `item_id|levels|total|steps` on stdin -- a `sqlite3` dump, never a
//! `SqliteStore::connect`, which would migrate the database it was pointed at.
use app_core::market::Ladder;

fn main() {
    let mut ladders = Vec::new();
    for line in std::io::stdin().lines().map_while(Result::ok) {
        let mut f = line.split('|');
        let (_id, _levels, _total, steps) = (f.next(), f.next(), f.next(), f.next());
        if let Some(steps) = steps {
            ladders.push(Ladder::decode(steps));
        }
    }
    println!("ladders: {}", ladders.len());

    let mut exact_bytes = 0usize;
    let mut curve_bytes = 0usize;
    let mut within5 = Vec::new();
    let mut within20 = Vec::new();
    let mut p50 = Vec::new();
    let mut p25 = Vec::new();
    for ladder in &ladders {
        exact_bytes += ladder.encode().len();
        // 12 cumulative counts + cheapest + total + levels, as text.
        let curve = ladder.compact();
        curve_bytes += curve
            .cumulative
            .iter()
            .map(|u| u.to_string().len() + 1)
            .sum::<usize>()
            + curve.cheapest.get().to_string().len()
            + curve.total.to_string().len()
            + 4;

        let rel = |exact: Option<u64>, got: Option<u64>| match (exact, got) {
            (Some(a), Some(b)) if a > 0 => Some(((b as f64 - a as f64) / a as f64 * 100.0).abs()),
            (Some(0), Some(0)) | (None, None) => Some(0.0),
            _ => None,
        };
        if let Some(e) = rel(ladder.quantity_within(5), curve.quantity_within(5)) {
            within5.push(e);
        }
        if let Some(e) = rel(ladder.quantity_within(20), curve.quantity_within(20)) {
            within20.push(e);
        }
        let money =
            |a: Option<app_core::market::Copper>, b: Option<app_core::market::Copper>| match (a, b)
            {
                (Some(a), Some(b)) if a.get() > 0 => {
                    Some((b.get() as f64 - a.get() as f64) / a.get() as f64 * 100.0)
                }
                _ => None,
            };
        if let Some(e) = money(ladder.supply_percentile(50), curve.supply_percentile(50)) {
            p50.push(e.abs());
        }
        if let Some(e) = money(ladder.supply_percentile(25), curve.supply_percentile(25)) {
            p25.push(e.abs());
        }
    }

    let show = |name: &str, mut xs: Vec<f64>| {
        if xs.is_empty() {
            println!("  {name:22} no comparable markets");
            return;
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| xs[((xs.len() - 1) as f64 * p) as usize];
        println!(
            "  {name:22} n={:<4} p50 {:>6.2}%  p95 {:>6.2}%  max {:>7.2}%  exact {:>5.1}%",
            xs.len(),
            q(0.5),
            q(0.95),
            xs[xs.len() - 1],
            xs.iter().filter(|e| **e == 0.0).count() as f64 / xs.len() as f64 * 100.0
        );
    };
    println!(
        "bytes: exact {exact_bytes} -> curve {curve_bytes}  ({:.1}x smaller)",
        exact_bytes as f64 / curve_bytes as f64
    );
    println!("error against the exact ladder:");
    show("units within 5%", within5);
    show("units within 20%", within20);
    show("supply p25 (price)", p25);
    show("supply p50 (price)", p50);
}
