# Measuring the read path

Phase 0 of the market-analysis roadmap (CLAUDE.md §16) exists so that a later
change can prove it improved the real product. Everything in this directory is
that proof's apparatus: the fixtures a measurement is taken against, the
numbers as they stood, and the query plans the planner chose.

Its exit gate has two halves, and both are testable:

1. the benchmark detects a regression;
2. the timings say which stage caused it.

## The two fixtures

`scripts/bench-fixture.py` builds both. Neither is committed -- one is derived
from a live archive, the other is 100 MB of generated rows -- but both are
reproducible from the script, and each writes a manifest beside it recording
its SHA-256, migration version, row counts, distinct markets and exact size.

```bash
# The authoritative one: real prices, real realms, nobody's account.
python3 scripts/bench-fixture.py sanitize \
    --source data/cluster.db --output data/bench/market-realistic.db

# The deterministic one: the same shape from a seed. Same bytes every time.
python3 scripts/bench-fixture.py synthetic \
    --output target/bench/market-synthetic.db --seed 20260830
```

| Fixture | What it is for |
|---|---|
| Real, sanitised | The official p50/p95/p99, and any index or query decision |
| Synthetic, deterministic | Tests, statement counts, query plans, structural regressions |

The synthetic one is **not** a latency measurement. Its distributions are
imitated and a CI machine is not a reference machine; what it is good for is
that a query plan or an N+1 shows up in it exactly as it would in the real one.

The sanitiser builds a **new** database from `migrations/` and copies a
whitelist into it. It does not copy everything and then delete the private
tables: a deleted row's bytes stay in the file's free list and in the WAL, and
a fixture that has to be trusted cannot be one whose privacy depends on nobody
running `strings` over it. Every table has to be named in the script as copied
or as left empty, so a new migration cannot quietly add one that travels.

## The benchmark

```bash
python3 scripts/bench.py                                    # release + release-fast
python3 scripts/bench.py --baseline docs/bench/baseline.json  # fail on a regression
python3 scripts/bench.py --profile debug --warm 5            # quick loop
```

It starts its own server on the fixture, with `--server-timing` on, no
in-process workers and no Blizzard credentials, so nothing is collecting while
the read path is measured. Per endpoint it records time to first byte cold and
warm at p50/p95/p99, the `Server-Timing` breakdown, statements executed, rows
decoded, and response bytes both uncompressed and over the wire.

**Cold** means a freshly started process: SQLite's page cache and the
connection pool are empty. It does not mean the file is out of the operating
system's cache, which cannot be arranged without root.

### Two release profiles, and why

`release` is `opt-level = "z"`. It is what ships, so it is what a capacity
claim has to be made against -- but part of what it measures is the compiler
optimising for size rather than the read path being slow. `release-fast`
(`Cargo.toml`, `[profile.release-fast]`) is the same build at `opt-level = 3`.
The gap between the two columns is how much of a number belongs to the profile.

Debug is kept in the baseline for one reason: it is what `cargo run` gives you,
and knowing it is three to four times the release number stops a local
observation being mistaken for a production one.

## Server-Timing

`--server-timing` / `APP_SERVER_TIMING=true`, **off by default**. Per-stage
timings, statement counts and row counts say how the deployment is doing, which
§7 keeps on the operations side of the app; a visitor is owed the page, not the
shape of the read path behind it.

```text
server-timing: db;dur=292.736, cache;dur=8.689, calc;dur=34.560, tpl;dur=0.648,
               q;desc="4", rows;desc="19654", total;dur=413.922, bytes;desc="53751"
```

| Metric | Is |
|---|---|
| `db` | time inside the database driver |
| `cache` | time inside the cache port, which is also database time |
| `calc` | reducing observations into statistics or into a page's read model |
| `tpl` | rendering templates |
| `q`, `rows` | statements executed and rows decoded |
| `bytes` | the response body before compression |
| `total` | the whole request, measured by the outermost middleware |

The stages are **not** a partition of the total. A stage may contain another --
a cache read is a database read -- so the sum can exceed `total`. The
alternative is subtracting nested time and reporting a number that matches no
clock anyone can check.

`calc` is the one to watch: Phase 2's exit gate is that no handler reduces a
history during a request, so it is a number that has to reach zero rather than
one that has to get smaller.

Two mechanisms sit behind those numbers, for a reason worth knowing. `cache`,
`calc` and `tpl` are measured by guards inside the request. `db`, `q` and
`rows` come from a process-wide counter that the request takes the difference
across, because the SQLite driver runs every statement on a thread of its own
and cannot be asked whose request it was serving. Served sequentially -- how
the benchmark asks -- that difference is exactly the request's own work. Under
concurrent traffic it is the process's database work while the request ran,
which is a weaker claim and is the one being made.

## Query plans

`docs/bench/query-plans.md`, regenerated by `scripts/query-plans.py`.
Re-record it whenever an index, a query or the statistics change.

```bash
python3 scripts/query-plans.py            # write it
python3 scripts/query-plans.py --check    # fail if it is out of date
```

Each recorded query carries a fragment of itself that must still be present in
the storage adapter, so a query that is edited and not re-recorded fails the
script instead of quietly documenting a plan nobody runs.

## Characterization

`crates/app-core/tests/characterization.rs` pins what the pure reductions
answer today, to exact numbers, over a generated deterministic dataset. Several
of those numbers are things `docs/market-analysis.md` says will change --
`volatility_percent` is a range-based swing, the alert percentile is
nearest-rank rather than Hyndman-Fan R8. Pinning them is the point: when the
definitions are replaced, the diff to that file is the list of what actually
changed for a reader, separated from what merely moved.

## Re-recording the baseline

Replace `docs/bench/baseline.json` only deliberately, and say in the commit
message what moved and why. It is the thing a regression is measured against;
a baseline quietly refreshed is a regression quietly accepted.
