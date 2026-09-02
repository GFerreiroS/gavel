# Adoption roadmap: Project Shatari and TSM

Status: plan and architecture decisions; **nothing in this document is
implemented**. It records what we intend to take from two external projects,
what we deliberately refuse to take, and the reasoning behind the one decision
that constrains all the others (§4).

Read this before changing collection cadence, price-sample retention, the
per-realm schema, or anything that reduces a history. It is a companion to
[market-analysis.md](market-analysis.md), which remains the source of truth for
*what the analysis means*; this document is about *where the data comes from
and how much of it we keep*.

Sections are referenced from code comments and migrations as
`docs/roadmap.md §N`, the same convention `market-analysis.md` uses.

---

## 1. Provenance and licence obligations

Three repositories by Gerard Dombroski, all **Apache-2.0**, all read in full
before this plan was written:

| Repository | What it is |
|---|---|
| [`erorus/shatari-data`](https://github.com/erorus/shatari-data) | PHP. Reads Blizzard's DB2 client files, emits static JSON (items, bonuses, auction-house categories, battle pets). Run from a dev machine, once per patch. |
| [`erorus/shatari`](https://github.com/erorus/shatari) | Node.js. A permanent process that polls the Battle.net auction API and writes ~58M binary files (~56 GB) — the collection layer behind [undermine.exchange](https://undermine.exchange). |
| [`erorus/shatari-front`](https://github.com/erorus/shatari-front) | TypeScript + Highstock. A fully static site that parses those binaries in the browser. |

Apache-2.0 is compatible with this workspace's `MIT OR Apache-2.0`. It is not,
however, a public-domain grant. **Every port carries three obligations:**

1. **Retain the copyright notice.** A `NOTICE` file at the repository root
   naming Gerard Dombroski and listing which of our files derive from which of
   theirs. This file does not exist yet; it lands with the first port, not
   before, because a NOTICE that credits nothing is noise.
2. **State the changes.** Each derived file carries a header of the form:
   ```
   //! Derived from `shatari-data/src/bonuses.php`, (c) Gerard Dombroski,
   //! Apache-2.0. Ported to Rust; <what we changed and why>.
   ```
   "Ported to Rust" alone is not a statement of changes. Say what the logic
   does differently.
3. **Include the licence text.** `LICENSES/Apache-2.0-shatari.txt`, verbatim.

Independently of the licence, the README gains a "Thanks" section naming the
three repositories and undermine.exchange. This is not a legal requirement; it
is the correct thing to do and was the first instruction given when this work
was scoped.

### 1.1 TSM

[TradeSkillMaster](https://tradeskillmaster.com) publishes pricing data as
plain CSV at `https://public-data.tradeskillmaster.com` — static, public, no
API key, no stated rate limit. See §13.

**The published documentation states no licence for the data.** It does say
that anyone *"building websites or other 3rd party tools which require more
programmatic usage"* should contact `admin@tradeskillmaster.com`. Downloading
public CSVs to compute against is one thing; publishing their figures on a
website is another. **Send that email before any TSM-derived number is visible
to the public.** This is a blocking precondition on §13.7, not a nicety.

---

## 2. What we already have

Recorded so that a future reader — human or agent — does not rebuild it. On
the statistical side this project is already **ahead of** the project it is
borrowing from. Undermine stores a minimum price and a quantity; we store:

- Hyndman-Fan R8 percentiles (p05/p25/median/p75/p95), IQR and MAD over
  equal-duration buckets, with an evidence gate that refuses to show a band
  rather than showing a weak one (`market_windows`, migration `0017`).
- Full price ladders and swept market depth — what it costs to buy *n*, not
  just what one costs (`price_ladders`, `realm_price_ladders`, migration
  `0021`).
- Cross-realm dispersion for BoE and recipes: cheapest / typical / dearest
  realm, five-number spread, `realms_listing` out of `realms_collected`
  (`market_rollup`, migrations `0016` and `0022`).
- A 168-cell hour-of-week heatmap, Spearman correlation between price and
  listed stock, drawdown and rise from running extremes, typical move between
  observations (migration `0022`).
- Listing counts and quantities per observation — *"how many auctions are
  there of each item"* is already answered, for both commodities and per-realm
  markets.
- A published/staging read model so no page ever reduces a history during a
  request (migration `0015`, and `market-analysis.md` §4).

Nothing in §4–§12 should reimplement any of the above.

---

## 3. Scope: adopted and refused

| From | Adopted | Section |
|---|---|---|
| `bonuses.php` | Bonus-id → item level, name suffix, tertiary stat | §5 |
| `items.php` | Item metadata beyond the game API (icon, vendor price, stack, BoP, expansion) | §5.3 |
| `main.js` | `If-Modified-Since`, adaptive per-realm cadence | §6 |
| `realmProcess.js` | Change detection before writing | §4 |
| `realmState.js`, `globalState.js` | The observation ledger idea | §4.3 |
| `regionState.js` | Regional arbitrage and median across realms | §9 |
| `main.js` `updateDeals` | Deal detection | §10 |
| `tokenState.js` | WoW Token price history | §11 |
| `Detail.ts` | Chart *rules* — not the library | §12 |
| TSM `region/items.csv` | `saleRate`, `soldPerDay`, `avgSalePrice` | §13 |

**Refused, deliberately:**

| Not adopting | Why |
|---|---|
| Battle pets | Out of product scope. The catalogue is curated and pets are not in it. |
| Full item universe (~30k items, all realms) | The catalogue is curated on purpose. Undermine's 58M-file architecture exists *because* they track everything; we do not, and inheriting that cost without that requirement would be backwards. |
| Public JSON API | Not planned. `shatari-front/public/api.html` documents the whole contract if this is ever revisited. |
| Free-text search and the AH category tree | The tracked item list is curated and small; browsing it needs no search engine. |
| Addon export (Oribos Exchange) | A separate product, not this one. |
| Highstock | We render SVG server-side and that stays. §12 takes the reasoning, not the 400 KB. |
| Their daily reduction (max-quantity price) | Statistically poor. See §8.2. |

---

## 4. Change-detected history

**This is the load-bearing decision of the whole plan.** §7, §8 and the shelved
items in §14 are consequences of it. Read this section before touching
`price_samples`, `realm_price_samples`, or either ladder table.

### 4.1 The measured cost of keeping everything

Measured against a real database holding exactly one collection round (four
commodity snapshots, one per region; 184 per-realm snapshots) with the active
Midnight catalogue — 515 commodity items across 4 regions, 143 per-realm items
across 184 realms, 153 distinct variants.

Reproduce with:

```sql
SELECT name, SUM(pgsize)/1048576.0 AS mib, SUM(ncell) AS cells
FROM dbstat GROUP BY name ORDER BY mib DESC;
```

Per collection round, table plus its indexes:

| | per round | per year | share |
|---|---|---|---|
| `realm_price_samples` + 3 indexes | 5.43 MiB | **46.5 GiB** | 57% |
| `realm_price_ladders` + index | 3.31 MiB | **28.3 GiB** | 35% |
| `price_ladders` | 0.65 MiB | 5.6 GiB | 7% |
| `price_samples` + 3 indexes | 0.20 MiB | 1.7 GiB | 2% |
| **total** | **9.59 MiB** | **~82 GiB/year** | |

Per year assumes 24 useful rounds per day: Blizzard publishes hourly, and the
30-minute collection interval means half the rounds collide with the primary
key and write nothing.

Two facts drive everything that follows.

**Half the bytes are indexes, not data.** 4.9 of the 9.59 MiB per round.

**A single row is not expensive; there are simply an enormous number of them.**
A `realm_price_samples` row costs 170 bytes (56 of table, 114 of index) to say
"minimum 1,240g, median 1,400g, 2 listings". At 33,366 markets × 24 hours × 365
days that is **292 million rows per year**, and the overwhelming majority say
exactly what the row before them said. The average per-realm ladder has **2.1
rungs** — two auctions that have been sitting there for days. We are not paying
to store history. We are paying to restate 292 million times that two auctions
are still listed.

### 4.2 Why the obvious fix breaks the evidence gate

The obvious fix — do not insert a row when nothing changed — is correct in
principle and, applied naively, silently breaks the statistics.

`materialise.rs:546` computes `observed_buckets: hours.len()`, counting the
distinct hours **present among the sample rows**. `materialise.rs:449` derives
`largest_gap_ms` from the gaps between consecutive sample rows. There is no
record anywhere of which snapshots were actually fetched — the sample rows
*are* the record.

Stop writing unchanged rows and a market whose price has not moved in 30 days
has one row. `observed_buckets = 1` against `expected_buckets = 720`. The
evidence gate refuses the band and the page reports *"not enough history: 1
hour of 720"*.

Exactly inverted: **the most stable markets would present as the worst
observed.** The database needs somewhere to say "I looked, and it had not
changed", and today it has none.

### 4.3 The design: observation ledger plus change rows

Two changes, together and never separately.

**An append-only ledger of what was fetched:**

```sql
CREATE TABLE collection_snapshots (
    region      TEXT    NOT NULL,
    realm_id    INTEGER NOT NULL,  -- 0 for a region-wide commodity snapshot
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (region, realm_id, observed_at)
) WITHOUT ROWID;
```

184 realms × 24 × 365 = 1.61M rows per year, roughly **32 MiB/year**. This is
the record that says *we looked*.

**Sample and ladder rows are inserted only when the state changes.**
`observed_at` changes meaning: no longer "when this was seen" but **"since when
this has been the case"**.

The two questions then separate cleanly and neither is lost:

- *Was hour H observed?* → the ledger. Exact, with no inference.
- *What was the price at hour H?* → the newest row with `observed_at <= H`.

This is what Shatari does, though its implementation scatters it:
`realmState.snapshots` and `globalState.snapshotLists` are its ledger, and
`auctionsMap.json` (`realmProcess.js`) its change detector. The idea is theirs;
only the assembly is ours.

**What counts as a change** must be defined once and in one place, because two
definitions would eventually disagree: a market has changed when any of
`min_price`, `median_price`, `listings`, or the ladder's serialised `steps`
differs from the row currently in effect. Comparing `steps` byte-for-byte is
sufficient and cheap — it is a short string, and any change to the auctions
behind it changes it.

### 4.4 Why not `valid_until`

The obvious alternative is a `valid_until` column extended on each unchanged
observation. It is worse, and the reason is worth recording so nobody proposes
it again.

It requires an `UPDATE` per market per hour whether or not anything changed.
SQLite rewrites the page. We would save disk and pay the identical number of
WAL writes — and write throughput on a single writer is precisely the
constraint this project already lists as the reason the web tier does not scale
(README, "Scaling").

The ledger is append-only. When nothing changes across a realm it writes **one**
row, not 181.

### 4.5 The duration-weighting trap

Recorded as a hard rule because it corrupts every statistic on the site without
failing a single test.

`market-analysis.md` §5.1 and migration `0017` specify percentiles over
**equal-duration buckets**. Once only changes are stored, rows are no longer
equal-duration:

> A price that held at 100g for 20 hours leaves **one** row. A price that
> flickered for 4 hours leaves **twelve**. A median taken over rows concludes
> that the market lives in the flicker. It is wrong, and it returns a number
> rather than an error.

**The rule: expand on read, never reduce over rows.** Hourly buckets are
reconstructed from the ledger plus the change rows, and handed to the engine
**exactly as they are handed to it today**. `engine.rs` is not modified.

This is what makes the change safe to verify:
`crates/app-core/tests/characterization.rs` pins today's reductions to exact
numbers. After the migration those numbers must be **unchanged**. If they move,
the expansion layer is wrong — that test is the acceptance criterion for this
entire section.

The real work here is therefore not the storage change, which is small. It is
the expansion layer, and rewriting `observed_buckets` / `largest_gap_ms` in
`materialise.rs` to read from the ledger instead of counting rows. `series.rs`
is affected for the same reason.

### 4.6 Expected saving, and how to verify it

Estimated, and **the estimate is load-bearing enough to measure before writing
the migration**:

| | now | with change detection |
|---|---|---|
| `realm_price_samples` | 46.5 GiB/yr | ~5 |
| `realm_price_ladders` | 28.3 | ~3 |
| `price_ladders` | 5.6 | ~4 |
| `price_samples` | 1.7 | ~1.3 |
| `collection_snapshots` | — | 0.03 |
| **total** | **~82 GiB/yr** | **~13 GiB/yr** |

With §7 (`variant_id`) roughly **10 GiB/year**, at which point "keep everything
permanently" stops being a decision — it is 100 GiB in ten years.

The ~90% figure on the per-realm side is reasoned, not measured: 2.1-rung
ladders and auction durations of 12–48h imply a change every several hours, not
every hour. **Verify it before committing to the design's dimensions.** Run the
collector for 48 hours and measure the true rate:

```sql
WITH changes AS (
    SELECT item_id, region, realm_id, variant, observed_at,
           min_price, median_price, listings,
           LAG(min_price)    OVER w AS prev_min,
           LAG(median_price) OVER w AS prev_median,
           LAG(listings)     OVER w AS prev_listings
    FROM realm_price_samples
    WINDOW w AS (PARTITION BY item_id, region, realm_id, variant
                 ORDER BY observed_at)
)
SELECT COUNT(*)                                        AS rows_total,
       SUM(prev_min IS NOT NULL
           AND min_price = prev_min
           AND median_price = prev_median
           AND listings = prev_listings)               AS rows_unchanged
FROM changes;
```

Expect a far lower rate on commodities — 20 rungs and hundreds of listings move
most hours — but they are 9% of the cost, so it barely matters.

If the per-realm rate comes back near 40% rather than near 90%, the design is
still right and the priorities in §14 change.

### 4.7 Consequences

Two items previously planned are **withdrawn** as direct consequences, and the
reasoning is recorded so they are not silently reinstated:

- **Lossy ladder reduction after N days** (keep nine percentile rungs instead
  of the full ladder). With change detection the ladder tables fall from ~34
  GiB/year to ~7. Discarding information to save 3 GiB/year is a bad trade.
- **Hot/cold monthly partitioning into `ATTACH`-ed archive files.** Still the
  right design at scale, and still the one that fits this project's "one
  ordinary server, no extra services" identity — but at 10 GiB/year it is not
  work for now. Revisit when the live database passes roughly 50 GiB.

---

## 5. Item bonus decoding

The highest-value port, because it closes a gap this repository has already
written down. Migration `0004` states: *"Blizzard publishes no
bonus-id-to-item-level table, so the tiers a reader sees are derived from this
at read time"*.

`shatari-data/src/bonuses.php` (263 lines) resolves exactly that, from the DB2
client files rather than the web API: `ItemBonus`, `ItemScalingConfig`,
`ItemOffsetCurve`, `ContentTuning`, `CurvePoint`. It distinguishes eight
distinct level-adjustment mechanisms — their names in the source are
`legacyAdjust`, `contentTuning`, `legacySet`, `eraCurveSet`, `itemScalingSet`,
`itemScalingSetByPlayer`, `eraAdjust`, `adjust` — resolves name suffixes, and
detects the four tertiary stats (speed, leech, avoidance, indestructible).

This turns `realm_price_samples.variant` from an opaque sorted bonus list into
a real item level and a real name. It cannot be obtained from the web API at
any price.

### 5.1 What must not be lost in the port

Migration `0004` is explicit that a patch renumbering bonus ids must cost *a
display rule, never the history*. The decoded item level is therefore a
**derived presentation fact, not a stored identity**. `variant` stays the
market's identity; the decode table maps it to a level at read time and may be
replaced wholesale when a patch lands.

### 5.2 Operational shape

This is per-patch, offline work, not a runtime dependency: DB2 files in, a
JSON table out, embedded at build time the way `catalogs.json` already is. It
does not belong in the collector.

### 5.3 Item metadata

`items.php` additionally yields icon, `vendorBuy`/`vendorSell` (with the class
price modifiers and the `ImportPrice*` tables), stack size, the bind-on-pickup
flag, expansion, and crafting quality. Take what the curated catalogue actually
needs and no more. `categories.php` (the localised AH category tree) and
`battlepets.php` are out of scope per §3.

---

## 6. Collection cadence

Three mechanisms from `shatari/src/main.js`, in ascending order of value.

**Conditional requests.** Send `If-Modified-Since` with the previous snapshot's
`Last-Modified`; a 304 costs no bytes and no parse
(`main.js:674 processConnectedRealm`). We currently poll blind every 30
minutes.

**Adaptive per-realm cadence** (`main.js:509 nextCheckTimestamp`). Realms do
not publish on the hour, and they do not publish in step with each other. The
algorithm measures the last 36 real intervals, takes a rolling 3-wide maximum,
polls 45 seconds early, and backs off to 1 / 5 / 15 / 30 minutes as a realm
becomes progressively more overdue. It is about 40 lines and it is the
difference between catching a snapshot promptly and catching it up to an hour
late.

**Change detection before writing.** Covered in §4; listed here because it
belongs to the same file in the original. §6 and §4 are not independent: doing
§6 first makes §4 cheaper to land.

---

## 7. `variant` becomes `variant_id`

`realm_price_samples` and `realm_price_ladders` are `WITHOUT ROWID` with
`variant TEXT` inside the primary key, so the full comma-separated bonus list
is stored again in **every index entry**. The measured database holds **153
distinct variants in total**.

A dictionary table and a `variant_id INTEGER` is a mechanical migration with no
information loss, worth roughly a third of the per-realm side.

**Sequence matters: this lands before §4 finishes accumulating history, and
well before §8.** Normalising 33,000 rows is trivial; normalising 300 million
is not.

---

## 8. Perpetual daily rollup

### 8.1 It is a read model, not a compression scheme

The most common misreading of Undermine's design, and it changes the decision.
Their daily table exists because reducing 43,800 hourly rows per market during
a request is exactly what `market-analysis.md` §4 forbids. `market_windows`
holds 96 fixed-resolution slots per *named* window; nothing in the schema can
serve "the whole history".

So **build it even though we delete nothing.** It is additive: raw samples kept
forever *and* one materialised row per market per day. 2,001 commodity markets
× 365 ≈ 120 MiB/year; the per-realm side is ~12M rows/year at roughly 1.2
GiB/year. Against §4's numbers this is noise.

### 8.2 Do not copy their reduction

Undermine's daily row is *"the maximum quantity observed that day, and the
price at that moment"* (`realmProcess.js updateRealmItem`). That is biased: the
price at peak stock is the price just after someone dumped inventory, not the
day's representative price.

`engine.rs` already produces something better. The daily row should carry:
open, close, low and its instant, high and its instant, mean,
p05/p25/median/p75/p95, sample count, observed buckets, and the same treatment
for quantity and listings. Roughly twenty integers, and it answers questions
theirs cannot.

**Take the shape — one perpetual row per market per day — and not the
reduction.**

---

## 9. Cross-realm arbitrage

We have the aggregate (`market_rollup.spread_*`, `realms_listing` /
`realms_collected`) and lack the breakdown. From `regionState.js` and
`Detail.ts`:

- **Arbitrage line**: per item, how many connected realms currently list it and
  the minimum price across the region. `regionState.js` stores exactly
  `{realms, min}` per item key.
- **Regional median**: the median of each realm's cheapest copy, counting only
  realms with stock right now — not realms that merely had it once.
- **Per-realm table and bar chart** (`Detail.ts:1700+`): price, quantity and
  population per realm, sortable, with connected realms collapsible, and
  zero-quantity rows always sorted last regardless of the active column.
- **Regional daily history**: quantity summed and price averaged across all
  realms per day.

Note their scope rule, which is sound and worth keeping: arbitrage is computed
only for **non-stackable** items. A commodity is already region-wide, so there
is nothing to arbitrage.

---

## 10. Deals

`main.js:336 updateDeals`, every 30 minutes per region. The logic is less naive
than it looks and should be ported as-is:

1. Non-stackable items only, per §9.
2. Median over *every* price seen for that item across realms, including realms
   where it is not currently listed.
3. Discard anything under 150 gold — noise, not opportunity.
4. `dealPrice = median`, and where **15 or more** realms currently offer it,
   `dealPrice = min(median, offered[len/3])` — the 33rd percentile of what is
   actually purchasable now.

The 15-realm gate is the part that matters: below it the percentile is a shape
in noise, which is the same reasoning as our own evidence gates
(`market-analysis.md` §5.3).

---

## 11. WoW Token

`tokenState.js` plus `/data/wow/token/index`, polled every ~20 minutes,
retaining a price history per region. Roughly forty lines, no catalogue
involvement, no interaction with §4. It is the number everyone checks.

---

## 12. Chart rules

We render SVG server-side (`crates/app-web/src/chart.rs`) and **that does not
change**. Highstock is 400 KB and would dwarf the rest of the frontend. What is
worth taking from `Detail.ts` is the judgement, all of it independent of the
library:

- **Outlier clipping on the price axis**: `max = min(observed_max, p95 × 1.1)`.
  Without it one absurd listing flattens the entire line.
- **Recompute the clip on zoom**, over the visible window rather than the whole
  series (`Detail.ts:1018 rescaleYAxes`). A chart clipped once at full extent
  is useless zoomed in.
- **Dual axis**: price as a filled area, listed quantity as a line on the right
  axis. It is *the* view of an auction house — supply and price together.
- **Heatmap colour scaled p15→p85**, not min→max, with opacity
  `pct × 0.5 + 0.1`. Extremes otherwise consume the whole ramp.
- **Bulk calculator**: enter a quantity, sweep the ladder, show total and unit
  price with the consumed rungs highlighted. We already store the ladder and
  the sweep (`depth.rs`); this is the input control on top of it.
- **Quality gates before drawing**: nothing plotted below 6 snapshots, no daily
  series below 15 days.

---

## 13. TSM integration

### 13.1 What it is

Static CSV, no key, no stated rate limit:

```
https://public-data.tradeskillmaster.com/{gameType}/{regionSlug}/commodities.csv
https://public-data.tradeskillmaster.com/{gameType}/{regionSlug}/region/items.csv
https://public-data.tradeskillmaster.com/{gameType}/{regionSlug}/realm/{realmSlug}/items.csv
```

`gameType` is `retail`; `regionSlug` is `us`/`eu`/`kr`/`tw`/`cn`. Realm and
commodity files refresh roughly **every 3 hours**; region files **daily**.
`updatedAt` is identical for every row in a file and reflects the *upstream
scan time*, not generation time. Prices are copper. `name` is enUS.

### 13.2 What it gives us that nothing else does

`region/items.csv` carries `saleRate` (0–1), `soldPerDay` and `avgSalePrice` —
**completed sales**. `market-analysis.md` §2 forbids us from claiming realised
demand, and correctly, because the Blizzard API exposes listings and not
transactions. TSM is the only source that closes that gap, and it closes it
without violating §2: the figure is measured by someone else, attributed to
them, and never inferred from our own data.

`marketValue`, `historical` and `recent` are independent valuations. Useful as
contrast; see §13.4.

### 13.3 Storage

One table per shape, kept apart from our own markets:

```
tsm_region_daily     (item_id, region, day, market_value, historical,
                      avg_sale_price, sale_rate_bp, sold_per_day, updated_at)
tsm_commodity_sample (item_id, region, observed_at, market_value, min_buyout,
                      recent, historical, updated_at)
```

`sale_rate_bp` is basis points, integer. No floating point on any value that a
page renders, consistent with migration `0003`.

Four region files daily and four commodity files every three hours. Filter to
the catalogue's item ids on parse; the files carry the whole game.

### 13.4 The display rule

Not *"never use TSM's numbers"* — **"never blend them silently"**. The columns
are not uniformly comparable and the distinction decides which source wins:

- **`minBuyout` vs our minimum**: the same measurement. One can be right and
  the other wrong, and §13.5 will say which.
- **`marketValue` vs our percentiles**: *different* measurements. We sample
  hourly; their files refresh every three hours with a longer smoothing window
  and an algorithm they do not publish. Neither is "more precise"; they answer
  differently shaped questions.
- **`saleRate` / `soldPerDay` / `avgSalePrice`**: uncontested. Use them.

Every TSM-derived figure is labelled with its source wherever it appears. If
the contrast test shows their `minBuyout` beating ours, adopt theirs **and find
out why ours was wrong** — a defect there affects everything else we compute
from the same ladder.

### 13.5 The contrast test

Pair each TSM commodity row with our nearest sample and compare. Two conditions
without which the test is meaningless:

1. **Only compare where our own price was stable across the alignment
   window.** Alignment error reaches 90 minutes; in a moving market the
   comparison measures the clock, not the price. Discard any market that moved
   between our adjacent samples.
2. **Expect a stable ratio, not equality**, except for `minBuyout`. The signal
   is not the difference, it is the *drift* of the difference: if
   `marketValue / our_median` sits at 1.08 for three months and jumps to 1.4,
   something broke, and we know which week.

Leave `realm/{slug}/items.csv` out of the test. It is keyed by `itemId` with no
bonus or item-level breakdown, so against our variant-level BoE markets it
compares different things and would produce a confident wrong answer.

### 13.6 Cadence

`saleRate` moves once a day. A daily task per region; not a poller. Commodity
files every three hours, aligned to nothing in particular — read `updatedAt`
and skip unchanged files.

### 13.7 Blocking precondition

Nothing TSM-derived becomes publicly visible before the email in §1.1 is sent
and answered. Internal computation and the contrast test are unaffected.

---

## 14. Delivery order

| # | Work | Depends on | Note |
|---|---|---|---|
| 0 | Measure the true change rate over 48h (§4.6) | — | No code. Sizes the design. |
| 1 | Bonus decoding, `bonuses.php` → Rust (§5) | — | Independent of everything else. |
| 2 | Conditional requests + adaptive cadence (§6) | — | Makes 3 cheaper to land. |
| 3 | **Observation ledger, change rows, read-side expansion (§4)** | 0, 2 | The large one. `materialise.rs`, `series.rs`. |
| 4 | `variant` → `variant_id` (§7) | — | Before the tables grow. |
| 5 | Perpetual daily rollup (§8) | 3 | Read model for long history. |
| 6 | TSM commodities + contrast test (§13.5) | — | Internal only. |
| 7 | TSM `region/items.csv` → sale rate (§13.2) | §1.1 email | Public display gated. |
| 8 | Regional arbitrage + per-realm table (§9) | — | |
| 9 | Deals (§10) | 8 | Shares the cross-realm scan. |
| 10 | WoW Token (§11) | — | An afternoon. |
| 11 | Chart rules (§12) | — | |
| — | ~~Hot/cold partitioning~~ | — | Deferred, §4.7. Revisit past ~50 GiB. |
| — | ~~Lossy ladder reduction~~ | — | Withdrawn, §4.7. |

Item 0 precedes everything because it costs two days of waiting and no code,
and it decides how much item 3 is worth.

Item 1 is deliberately first among the code items: it is independent of the
storage work, so it can proceed in parallel and cannot be invalidated by
whatever item 0 reports.

## 15. Open questions

- **Item 0's result.** If the per-realm unchanged rate lands near 40% rather
  than 90%, §4.6's numbers roughly triple and hot/cold partitioning (§4.7)
  returns to the near-term plan.
- **Commodity change rate.** Unmeasured. If commodity ladders turn out to
  change nearly every hour — likely — they become the largest remaining table
  after §4, and lossy reduction returns for that table alone.
- **TSM terms.** §1.1 is unresolved until the email is answered. Treat every
  public-facing TSM feature as blocked, not as pending.
- **Ledger granularity for commodities.** `realm_id = 0` as the sentinel for a
  region-wide snapshot mirrors `market_rollup`'s existing convention, but it is
  worth confirming against the read paths before the migration is written.
