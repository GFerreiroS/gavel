#!/usr/bin/env python3
"""Record what SQLite decides to do with the read path's queries.

CLAUDE.md §11b: "Index for the window, not for the filter [...] Check with
`EXPLAIN QUERY PLAN`." Phase 0 asks for those plans to be written down, so that
a later change to an index, a query or the statistics can be compared against
what the planner used to choose rather than against somebody's memory of it.

The queries live in `crates/storage/src/sqlite/`. They are repeated here
because `EXPLAIN QUERY PLAN` needs the text, and each one carries a fragment
that must still be present in its source file -- so a query that is edited and
not re-recorded fails this script instead of quietly documenting a plan nobody
runs any more.

    python3 scripts/query-plans.py                       # write docs/bench/query-plans.md
    python3 scripts/query-plans.py --check               # fail if it is out of date
"""

from __future__ import annotations

import argparse
import sqlite3
import subprocess
import sys
import textwrap
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUTPUT = REPO / "docs" / "bench" / "query-plans.md"
SYNTHETIC_FIXTURE = Path("target/bench/market-synthetic.db")


@dataclass(frozen=True)
class Query:
    name: str
    source: str
    #: A distinctive line of the query as it appears in `source`. If it is gone,
    #: the query moved on and this record is stale.
    anchor: str
    sql: str
    params: tuple
    why: str


REGION = "eu"
ITEM = 237367
REALM_ITEM = 271441
REALM = 1403
SINCE = 0

QUERIES = [
    Query(
        name="commodity latest, one region",
        source="crates/storage/src/sqlite/prices.rs",
        anchor="JOIN (SELECT item_id, MAX(observed_at) AS newest",
        sql="""
            SELECT s.* FROM price_samples s
             JOIN (SELECT item_id, MAX(observed_at) AS newest
                     FROM price_samples WHERE region = ?
                    GROUP BY item_id) latest
               ON s.item_id = latest.item_id AND s.observed_at = latest.newest
             WHERE s.region = ?
             ORDER BY s.item_id
        """,
        params=(REGION, REGION),
        why="every commodity category page: one row per market, newest first",
    ),
    Query(
        name="commodity window statistics",
        source="crates/storage/src/sqlite/prices.rs",
        anchor="MAX(p05_unit) AS high",
        sql="""
            SELECT item_id,
                   MAX(p05_unit) AS high,
                   observed_at   AS high_at,
                   AVG(p05_unit) AS mean,
                   COUNT(*)      AS samples
              FROM price_samples
             WHERE region = ? AND observed_at >= ? AND observed_at < ?
             GROUP BY item_id
             ORDER BY item_id
        """,
        params=(REGION, SINCE, 2_000_000_000_000),
        why="the card's comparison window, and the all-time extremes beside it",
    ),
    Query(
        name="commodity history, one market",
        source="crates/storage/src/sqlite/prices.rs",
        anchor="WHERE item_id = ? AND region = ? AND observed_at >= ?",
        sql="""
            SELECT * FROM price_samples
             WHERE item_id = ? AND region = ? AND observed_at >= ?
             ORDER BY observed_at
        """,
        params=(ITEM, REGION, SINCE),
        why="the analysis page's full-history reduction, which Phase 2 removes",
    ),
    Query(
        name="per-realm latest, whole region",
        source="crates/storage/src/sqlite/realm_prices.rs",
        anchor="SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,",
        sql="""
            SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
                   MAX(samples.observed_at) AS observed_at, samples.min_price,
                   samples.median_price, samples.max_price, samples.listings
              FROM realm_price_samples AS samples
              JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
             WHERE samples.region = ?
             GROUP BY samples.item_id, samples.realm_id, samples.variant_id
        """,
        params=(REGION,),
        why="the gear and recipe pages: 18k markets rebuilt to draw nine cards",
    ),
    Query(
        name="per-realm latest, one realm",
        source="crates/storage/src/sqlite/realm_prices.rs",
        anchor="const BY_MARKET",
        sql="""
            SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
                   MAX(samples.observed_at) AS observed_at, samples.min_price,
                   samples.median_price, samples.max_price, samples.listings
              FROM realm_price_samples AS samples
              JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
             WHERE samples.region = ? AND samples.realm_id = ?
             GROUP BY samples.item_id, samples.realm_id, samples.variant_id
        """,
        params=(REGION, REALM),
        why="the same pages once a realm is chosen",
    ),
    Query(
        name="per-realm history, one item across a region",
        source="crates/storage/src/sqlite/realm_prices.rs",
        anchor="WHERE samples.item_id = ? AND samples.region = ?",
        sql="""
            SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
                   samples.observed_at, samples.min_price, samples.median_price,
                   samples.max_price, samples.listings
              FROM realm_price_samples AS samples
              JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
             WHERE samples.item_id = ? AND samples.region = ? AND samples.observed_at >= ?
             ORDER BY samples.observed_at
        """,
        params=(REALM_ITEM, REGION, SINCE),
        why="the BoE analysis page: one track on every realm of a region",
    ),
    Query(
        name="per-realm history, one item on one realm",
        source="crates/storage/src/sqlite/realm_prices.rs",
        anchor="AND samples.realm_id = ? AND samples.observed_at >= ?",
        sql="""
            SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
                    samples.observed_at, samples.min_price, samples.median_price,
                    samples.max_price, samples.listings
               FROM realm_price_samples AS samples
               JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
              WHERE samples.item_id = ? AND samples.region = ?
                    AND samples.realm_id = ? AND samples.observed_at >= ?
              ORDER BY samples.observed_at
        """,
        params=(REALM_ITEM, REGION, REALM, SINCE),
        why="the single-realm full history view",
    ),
    Query(
        name="per-realm window, whole region",
        source="crates/storage/src/sqlite/realm_prices.rs",
        anchor="WHERE samples.region = ? AND samples.observed_at >= ?",
        sql="""
            SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
                    samples.observed_at, samples.min_price, samples.median_price,
                    samples.max_price, samples.listings
               FROM realm_price_samples AS samples
               JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
              WHERE samples.region = ? AND samples.observed_at >= ?
              ORDER BY samples.item_id, samples.realm_id, samples.variant_id, samples.observed_at
        """,
        params=(REGION, SINCE),
        why="the background materialiser reading a window of history",
    ),
    Query(
        name="tooltips for a whole category",
        source="crates/storage/src/sqlite/cache.rs",
        anchor="SELECT key, value FROM cache WHERE key IN ({placeholders}) AND expires_at > ?",
        sql="""
            SELECT key, value FROM cache
             WHERE key IN (?, ?, ?) AND expires_at > ?
        """,
        params=(
            "item-tooltip:v3:en_US:237367",
            "item-tooltip:v3:en_US:237369",
            "item-tooltip:v3:en_US:237370",
            0,
        ),
        why="`get_many`, which replaced 1316 single reads per page (§11b)",
    ),
]


def check_anchors() -> list[str]:
    missing = []
    for query in QUERIES:
        source = (REPO / query.source).read_text()
        if query.anchor not in source:
            missing.append(f"{query.name}: {query.source} no longer contains {query.anchor!r}")
    return missing


def plan(db: sqlite3.Connection, query: Query) -> list[str]:
    rows = db.execute(
        "EXPLAIN QUERY PLAN " + query.sql.strip(), query.params
    ).fetchall()
    # Rows are (id, parent, notused, detail); indent by depth so a subquery
    # reads as one.
    depth: dict[int, int] = {0: 0}
    lines = []
    for node, parent, _unused, detail in rows:
        depth[node] = depth.get(parent, 0) + 1
        lines.append("  " * (depth[node] - 1) + detail)
    return lines


def render(database: Path, db: sqlite3.Connection) -> str:
    statistics = db.execute(
        "SELECT count(*) FROM sqlite_master WHERE name = 'sqlite_stat1'"
    ).fetchone()[0]
    out = [
        "# Query plans",
        "",
        "Recorded by `scripts/query-plans.py`. Regenerate it whenever an index,",
        "a query or the statistics change, and say in the commit message what",
        "moved -- CLAUDE.md §11b's rule is to check the plan, and a plan nobody",
        "wrote down is a plan nobody can compare against.",
        "",
        f"Fixture: `{database}`",
        "The deterministic synthetic fixture is generated on demand for this"
        " check; query-plan shape is reproducible, while latency remains a"
        " real-archive measurement.",
        f"`sqlite_stat1` present: **{'yes' if statistics else 'no'}**"
        " -- the planner guesses without it, and guessed four times slower on"
        " every category page.",
        "",
        "Two phrases are worth grepping for. `USE TEMP B-TREE FOR ORDER BY`",
        "means the index did not deliver the order the query asked for.",
        "`SCAN` without `USING INDEX` means the whole table was read.",
        "",
    ]
    for query in QUERIES:
        out.append(f"## {query.name}")
        out.append("")
        out.append(f"{query.why}.  ")
        out.append(f"`{query.source}`")
        out.append("")
        out.append("```sql")
        out.extend(textwrap.dedent(query.sql).strip().splitlines())
        out.append("```")
        out.append("")
        out.append("```text")
        out.extend(plan(db, query))
        out.append("```")
        out.append("")
    return "\n".join(out)


def ensure_synthetic_fixture(database: Path) -> None:
    """Build CI's deterministic query-plan fixture when it is absent.

    The real, sanitised archive is the authority for timing. It is deliberately
    not committed, so using it as the default made `--check` depend on whoever
    happened to have one locally. Query plans need stable schema and statistics,
    which the seeded synthetic fixture supplies reproducibly.
    """
    if database != SYNTHETIC_FIXTURE or database.exists():
        return
    subprocess.run(
        [
            sys.executable,
            str(REPO / "scripts" / "bench-fixture.py"),
            "synthetic",
            "--output",
            str(database),
        ],
        cwd=REPO,
        check=True,
    )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--database", default=str(SYNTHETIC_FIXTURE))
    parser.add_argument("--output", default=str(OUTPUT))
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if the recorded plans are not what the fixture produces now",
    )
    args = parser.parse_args(argv)

    stale = check_anchors()
    if stale:
        print("the recorded queries no longer match the storage adapter:")
        for line in stale:
            print(f"  {line}")
        return 2

    database = Path(args.database)
    ensure_synthetic_fixture(database)
    if not database.exists():
        raise SystemExit(
            f"no fixture at {database}."
            " Build the deterministic default: python3 scripts/bench-fixture.py"
            " synthetic --output target/bench/market-synthetic.db"
            "; or pass a real sanitised archive with --database."
        )
    db = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    text = render(database, db)

    output = Path(args.output)
    if args.check:
        if not output.exists() or output.read_text() != text:
            print(f"{output} is out of date; run scripts/query-plans.py")
            return 1
        print(f"{output} is current")
        return 0
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text)
    print(f"written to {output}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
