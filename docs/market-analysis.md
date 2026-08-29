# Market analysis architecture

Status: product and architecture specification; not yet implemented.

This document is the source of truth for features that turn the Auction
Tracker into a market-analysis product. Read it before changing collection,
price history, BoE catalogues, item statistics, alerts, charts, retention, or
the archive.

## 1. Product objective

The analysis page should become a machine for answering market questions:

- Is the current price cheap relative to this market's own history?
- Is that price actionable at a useful quantity, or is it one thin listing?
- Is the market stable, volatile, liquid, or frequently out of stock?
- Where are the supply walls, and how has market depth changed?
- At what hour or weekday has it historically been cheaper?
- How did price and supply change as the expansion progressed?
- What happened around a patch, raid release, hotfix, weekly reset, or other
  recorded event?
- Is a value merely unusual, or statistically anomalous?

More data is valuable when it preserves the ability to answer one of these
questions. A decorative chart, a number with no precise definition, or a
financial-market imitation unsupported by the source data is not valuable.

## 2. What the source data can and cannot say

Blizzard exposes currently listed auctions and quantities, not completed
sales. Consequently:

- Never call listed quantity **volume**.
- Never report sell-through, realised demand, traded VWAP, or completed sales.
- Never draw OHLC candles as if the observations were transactions.
- Use **Stock**, **Listed quantity**, **Listings**, **Market depth**, and
  **Liquidity proxy** according to the exact measure.
- A correlation is an association. Do not describe it as proof that an event
  caused a price move.

An unavailable fact is rendered as unavailable. It is never estimated from an
unrelated value merely to fill a card.

## 3. One engine, several market capabilities

Statistics are not created per item, category, expansion, or patch. A common
engine analyses a generic market identified by a `MarketKey`; the catalogue
only decides which markets exist and how they are presented.

Conceptually:

```text
Commodity  -> item + region + rank
Recipe     -> item + region + connected realm
BoE        -> item + region + connected realm + track
```

The precise Rust representation may differ, but it must be typed and must not
be a string assembled differently by each caller.

Every market supplies the common facts it has:

```text
observed_at
executable price
listed quantity
listing count
price ladder, when collected
source snapshot identity
```

The executable price has market-specific semantics. For a commodity it is a
supply-weighted price resistant to a one-unit troll listing. For a BoE it may
be the cheapest copy of the selected track on the selected connected realm.
That difference belongs in snapshot normalisation, not in a forked statistics
page.

The engine has capability-specific extensions rather than pretending every
market supports every statistic:

- **All markets:** historical position, trends, robust dispersion, stock or
  listing history, data quality, seasonality.
- **Markets with quantity:** cumulative depth, quantity within a price band,
  price impact for a target quantity, and supply walls.
- **Per-realm markets:** availability across realms and cross-realm price
  dispersion.
- **BoE markets:** track and item-level structure, sockets, tertiary bonuses,
  and their price premiums.

This is one analysis system with explicit capabilities, not one oversized
result containing plausible-looking zeroes for unsupported fields.

### Current implementation gap

The repository already has pieces of this design, but they are not the target
architecture yet:

- `app-core/src/market/analysis.rs` is a pure reusable function, but it accepts
  the commodity `PriceSample` shape and calculates from the full series.
- `app-web/src/routes/item.rs` reads all history and calls that function during
  a page request.
- `app-web/src/routes/gear_stats.rs` derives BoE/recipe statistics separately.
- `app-core/src/market/stats.rs` correctly uses a supply-weighted P5 inside a
  commodity snapshot; preserve that distinction from historical percentiles.
- `app-core/src/market/alerts.rs` uses nearest-rank historical percentiles,
  whereas the target engine needs one shared R8 implementation.
- persisted samples contain price/stock summaries but not the price ladder, so
  historical depth cannot be reconstructed today.
- current `volatility_percent` is range divided by mean and is a swing, not a
  robust volatility measure.

A feature must migrate these pieces towards the common materialised engine; it
must not add another page-local calculation beside them.

## 4. Collection, calculation, and publication

Pages do not calculate statistics. They read a complete, materialised analysis
version prepared after collection.

```text
Blizzard snapshot
       |
       v
source observation + normalised price ladder
       |
       v
common market facts identified by MarketKey
       |
       v
precomputed analysis version + time-series rollups
       |
       v
atomic publication
       |
       v
server-rendered page (read only)
```

Publication follows these rules:

1. Persist the complete source observation first.
2. Normalise it without discarding facts required by later analysis.
3. Recalculate only the affected markets and windows.
4. Validate the result and its source coverage.
5. Publish the new analysis version atomically only after every required part
   succeeds.
6. Record the calculation algorithm version and the source interval so a
   result can be reproduced or rebuilt after the algorithm improves.

Network, upstream, process, and disk failures are possible. The product
guarantee is stronger and more useful than claiming otherwise: **a failure
never publishes partial or invented analysis**. The page keeps the last valid
version with an honest `updated at` timestamp and freshness state. If no valid
version exists, it says that there is not enough data. The failed calculation
is retried and made visible to operations.

## 5. Statistical semantics

### 5.1 Historical percentiles

Historical percentiles are calculated independently for each `MarketKey` and
window. Prices from different items, regions, realms, ranks, or tracks are
never pooled to decide whether one market is cheap.

Use one documented sample-quantile definition everywhere. The intended choice
is Hyndman-Fan R8, a median-unbiased estimator with a definition independent of
the underlying distribution. Analysis, cards, alerts, tests, and archive
rebuilds must agree exactly.

Each equal-duration time bucket has equal weight in a historical percentile.
Do not weight historical time by current listed quantity: that would answer
"what price existed during high-stock periods?" rather than "where is today's
price in this market's history?"

This differs deliberately from a price percentile *inside one snapshot*. A
snapshot depth percentile is supply-weighted because it answers what a buyer
pays after consuming that share of the currently listed quantity.

### 5.2 Universal valuation bands

The bands are universal, but they are universal ranks within each market's own
distribution:

| Historical percentile | Label |
|---:|---|
| P0--P5 | Very cheap |
| above P5--P25 | Cheap |
| above P25--P75 | Typical |
| above P75--P95 | Expensive |
| above P95--P100 | Very expensive |

Use **Typical**, not **Fair**. Listed prices do not establish intrinsic or
transacted fair value.

The valuation is never shown alone. It is accompanied by:

- percentile rank;
- percentage difference from the selected-window median;
- depth or availability relevant to that market;
- window, sample coverage, freshness, and confidence state.

Liquidity and category do not secretly move the percentile boundaries. They
determine whether the result is reliable and actionable.

### 5.3 Insufficient and dependent data

Hourly market observations are serially dependent. A hundred adjacent rows do
not necessarily contain the information of a hundred independent samples, and
tail percentiles such as P5/P95 need more evidence than a median.

The engine therefore records at least:

- expected and observed buckets;
- coverage percentage and largest gap;
- source age;
- number of distinct observations or price states where useful;
- a confidence/availability state for tail estimates.

Do not emit a valuation or anomaly label when its evidence gate fails. Show
`Not enough history` and the reason instead. Exact thresholds require tests
against real archives before they are fixed; a single `min_samples` value is
not enough for every window and statistic.

### 5.4 Anomaly is not valuation

`Very cheap` means that a price occupies the lower historical tail. An
`anomaly` means that it is unusually far from the body of the distribution.
These are different statements and are displayed separately.

Use robust spread measures such as IQR and MAD. Candidate anomaly signals are
Tukey fences and a modified robust score over prices or log prices. Do not use
`(maximum - minimum) / mean` as volatility: that is a range-based swing, is
dominated by two observations, and should be named **Swing** if retained.

Useful distinct measures include:

- IQR or MAD relative to the median for level stability;
- robust changes between equal-duration observations for price volatility;
- P5--P95 historical spread for an understandable range;
- maximum drawdown/rise, clearly named;
- stock volatility, separate from price volatility.

### 5.5 Correlations

The first correlations of interest are:

- price versus listed quantity;
- price versus listing count;
- price/depth before and after a recorded event;
- realm availability versus cross-realm price dispersion;
- time from patch or raid release versus price and supply.

Prefer robust or rank-based association where the relationship is skewed or
non-linear. Always show the window, observation count, coverage, and direction.
An event-study view may compare pre-event and post-event medians, robust
spread, stock, and depth. It must use the wording `associated with` or
`observed after`, never `caused by`, unless the product later gains evidence
that supports a causal claim.

## 6. Analysis windows and time resolution

Precompute common windows rather than rebuilding arbitrary slices during a
request. The initial set is:

- 24 hours;
- 7 days;
- 30 days;
- current patch;
- current raid tier where applicable;
- expansion to date;
- archived patch/tier lifetime;
- archived expansion lifetime.

The purpose of long-term retention is to see how markets fluctuate as an
expansion advances and to relate those movements to events. Retention must
therefore preserve, at a tested resolution:

- executable price and historical distribution;
- listed quantity and listing count;
- depth shape or a documented depth curve representation;
- data gaps and observation time;
- patch/tier/event boundaries.

Do not choose a daily rollup merely because it is smaller if it destroys the
hourly/weekly seasonality or event interval a planned analysis needs. Do not
keep billions of redundant auction rows merely to claim that nothing was
discarded. Benchmark the real archive and preserve the smallest representation
that can reproduce the promised statistics.

Likely policies, subject to measurement, are:

- Keep sparse BoE/recipe ladders exactly for much longer, potentially forever.
- Aggregate auctions at the same price into one price level immediately; an
  auction id is not analysis history.
- Keep exact recent commodity ladders for interactive depth analysis.
- Preserve older commodity depth as a compact, versioned curve with enough
  cumulative-quantity breakpoints to reproduce price impact, distribution,
  and wall statistics.
- Keep long-term price/stock time-series resolution high enough for the event
  and seasonality features above. Compaction changes representation and
  resolution; it never silently deletes the existence of a market or event.

The exact hot-window length, curve encoding, and archive resolution remain an
engineering decision that must be made from storage and query benchmarks on
the real database. Changing them later requires a migration and a documented
statement of which analyses remain reproducible.

## 7. Historical market depth

A normalised snapshot ladder is sorted by price and groups equal prices:

```text
price | listed quantity at price | cumulative quantity
```

From it the engine can precompute:

- price after consuming a target quantity;
- price at P1/P5/P10/P25/P50/P75/P90/P95/P99 of listed supply;
- quantity available within +1%, +5%, +10%, and +25% of the executable price;
- significant supply walls and the price jump after each;
- curve area/slope as explicitly named liquidity proxies;
- changes in those measures through time and around events.

Target quantities may differ by product use: one BoE, a raid night's potions,
or a large reagent purchase. The analysis engine accepts a target profile from
catalogue/domain metadata; it does not hard-code one quantity per page.

## 8. Expansion, patch, raid tier, and BoE lifecycle

Commodity markets normally continue across patches while the same item
remains relevant. Patch boundaries annotate and segment one continuous
history.

BoE catalogues follow raid tiers. New tiers introduce a new active catalogue;
the former active BoE tier stops collecting automatically and becomes a
read-only archive. Activating the new tier and archiving the old one is one
transaction, so there is never zero or two unintentionally active BoE tiers.

Catalogue/release states are:

```text
draft_ptr -> active -> archived
```

- **draft_ptr:** administrator-only metadata. It contains expansion, patch,
  raid/tier, candidate item ids, track mappings, notes, and validation state.
  It is not collected, has no public market page, and lists no prices.
- **active:** public and collected. For BoEs there is one intended active tier.
- **archived:** public, frozen, and never collected again. Its last valid
  materialised analysis remains browsable and can be rebuilt from retained
  observations if the algorithm changes.

An administrator explicitly activates a PTR catalogue after reviewing it.
Activation, rather than an unattended calendar date, triggers automatic
archiving because PTR and release schedules can change.

Adding a live patch/tier is a data operation. It may require PTR research and
catalogue review, but it does not require new statistics, routes, templates, or
calculation code.

The public archive hierarchy is:

```text
Expansion
└── Patch
    └── Raid / tier
        └── market and item analysis
```

Patch and raid/tier are stored separately even when the current content maps
one-to-one; that relationship must not be baked into keys.

## 9. Event timeline

Correlating market movement with the expansion requires explicit, timestamped
events rather than labels inferred later from a chart.

An event carries:

- stable id and type;
- title and optional notes/source;
- start time and optional end time in UTC;
- applicable region(s);
- expansion, patch, raid/tier, category, item, or market scope as appropriate;
- provenance: shipped catalogue, administrator entry, or deterministic
  calendar rule;
- visibility and validation state.

Candidate event types include patch release, raid opening, season start,
weekly reset, hotfix/balance change, profession change, holiday, and a manual
market annotation. Region reset times are region-scoped events, not one global
timestamp.

PTR notes remain administrator-only until deliberately promoted. Public event
annotations must not leak private operational notes or unconfirmed catalogue
data.

## 10. Analysis output

The full analysis page may grow, but every panel must retain a question:

1. Price history with rolling median, P25--P75 band, current value, and event
   markers.
2. Historical percentile/distribution with valuation and anomaly separated.
3. Current market depth and target-quantity price impact.
4. Historical stock, listings, and depth metrics.
5. Hour-by-weekday buying-time heatmap with evidence coverage.
6. Robust volatility, swing, drawdown, and stability measures.
7. Price-versus-stock association.
8. Event studies across patch, raid, and selected annotations.
9. BoE cross-realm availability and price dispersion.
10. Data quality and freshness, always visible enough to interpret the rest.

The item card is a summary, not a second analysis page. Keep the Blizzard icon,
name/kind/rank, current price, selected-window change, sparkline, median, stock
or listings, valuation percentile, and one **View analysis** action. Remove
redundant Low/Avg/High prose once the sparkline and analysis page communicate
it better. Shared card geometry and row alignment remain mandatory.

## 11. Correctness and performance acceptance criteria

A market-analysis feature is not complete unless:

- pages perform no full-history statistical reduction;
- publication is atomic and a partial version is unreachable;
- every metric has one definition used by cards, pages, alerts, and tests;
- money remains integer copper at rest and across calculation boundaries;
- unsupported or under-evidenced measures render unavailable;
- freshness, window, and source coverage are available to the view model;
- no listing measure is called sales or volume;
- event correlations avoid causal wording;
- archive navigation continues to work after collection stops;
- activating a BoE tier automatically archives its predecessor;
- `draft_ptr` data and controls are administrator-only and show no prices;
- adding a patch/tier changes catalogue data, not analysis code;
- query count and latency are measured against the real archive;
- retained data can reproduce every promised archived statistic at its
  documented resolution.

## 12. Delivery order

Build this as vertical slices rather than one statistical rewrite:

1. Release/catalogue lifecycle and event model.
2. Typed `MarketKey`, common observations, calculation versioning, and atomic
   materialisation.
3. Universal percentiles, quality gates, robust spread, and the card sparkline.
4. Price bands, distribution, and stock history on the analysis page.
5. Current and historical depth storage plus price-impact analysis.
6. Event annotations and pre/post-event comparisons.
7. Heatmaps, robust volatility, correlations, and market-specific extensions.
8. Archive browsing by expansion, patch, and raid/tier.

Each slice must improve a real page, carry tests for its statistical meaning,
and leave the next patch/tier as a data update rather than a code fork.

## 13. Statistical and market-data references

- [NIST: Percentiles](https://itl.nist.gov/div898/handbook/prc/section2/prc262.htm)
  describes percentile ranks and compares common sample-quantile definitions.
- [Hyndman and Fan: Sample quantiles in statistical packages](https://robjhyndman.com/publications/quantiles/)
  compares nine definitions and recommends the median-unbiased R8 estimator.
- [NIST: Measures of scale](https://itl.nist.gov/div898/handbook/eda/section3/eda356.htm)
  explains why IQR and MAD are more stable than range or standard deviation in
  distributions with extreme tails.
- [NIST: Outlier detection with IQR fences](https://www.itl.nist.gov/div898/handbook/prc/section1/prc16.htm)
  distinguishes central spread from mild and extreme outliers.
- [Heidelberger and Lewis: Quantile Estimation in Dependent Sequences](https://doi.org/10.1287/opre.32.1.185)
  explains why positive serial dependence makes extreme quantiles require more
  evidence than the nominal row count suggests.
- [Bank for International Settlements: Market liquidity](https://www.bis.org/publ/bppdf/bispap02.pdf)
  distinguishes resting orders and book depth from executed order flow.
