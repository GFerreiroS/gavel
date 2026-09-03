#!/usr/bin/env python3
"""Build the archives Phase 0 measures against.

CLAUDE.md §11b: "Measure against the real archive, not a fixture. On an empty
one all of these are instant." That rule and a live database with accounts in
it pull in opposite directions, so there are two fixtures and they have
different jobs:

  sanitize   One authoritative local archive, built from the real one. Real
             prices, real realms, real distributions -- and no person in it.
             This is what p50/p95/p99 and an index decision are allowed to be
             argued from. It is not committed; this script is.

  synthetic  One deterministic archive of the same *shape*, from a seed. This
             is what tests, query counts, `EXPLAIN QUERY PLAN` and CI use. It
             is emphatically not a latency measurement: the distributions are
             imitated and a CI machine is not a reference machine.

The sanitiser copies a whitelist into a database built from `migrations/`.
It does not copy everything and delete the private tables afterwards, which
would be the obvious way round and the wrong one: a deleted row's bytes stay
in the file's free list, and in the WAL, and a fixture that has to be trusted
cannot be one whose privacy depends on nobody running strings over it.

The whitelist is also why a new migration cannot quietly leak a new table.
Every table in the destination has to be named here as copied or as left
empty; one that is neither stops the run until a human decides which it is.

    python3 scripts/bench-fixture.py sanitize \
        --source data/cluster.db --output data/bench/market-realistic.db
    python3 scripts/bench-fixture.py synthetic \
        --output target/bench/market-synthetic.db --seed 20260830
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import sqlite3
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
MIGRATIONS = REPO / "migrations"

SCRIPT_VERSION = 1

# Stamped into `_sqlx_migrations` so a rebuild is byte-identical.
INSTALLED_ON = "2026-08-30 00:00:00"

# --- the whitelist -----------------------------------------------------------
#
# Market data, and nothing that belongs to a person.

COPIED = {
    # The commodity archive: region-wide prices, one row per item/region/hour.
    "price_samples",
    # Commodity price ladders are source observations, not a derived page model.
    "price_ladders",
    # The per-realm archive: gear and recipes, one row per market per snapshot.
    "realm_price_samples",
    # Per-realm ladders are the source observations beside those summaries.
    "realm_price_ladders",
    # The storage-only dictionary that resolves each per-realm variant id.
    # It is market source data: without it the copied per-realm rows lose their
    # bonus-list identities.
    "market_variants",
    # Which connected realms exist, what they are called, and what they join.
    # Not personal, and the realm picker is unusable without it.
    "realms",
    # Which categories an administrator has turned on. A switch, not a person.
    "collection_settings",
}

# Not copied. Two different reasons live here, and both are worth stating.
#
# Most of these must not travel: they belong to a person or to a deployment.
# The rest are *derived* -- a read model built by some earlier algorithm
# version -- and copying those would be worse than useless, because a benchmark
# measuring a stale read model is measuring the wrong thing. The server
# rebuilds them from the observations on first start, which is the same path a
# real deployment takes.
#
# Named so that adding a table to a migration is a decision here rather than an
# omission. The reason matters more than the name: it is what the next person
# reads when they wonder whether their new table belongs above or below.
EMPTIED = {
    "users": "accounts, and Argon2 hashes of passwords",
    "sessions": "live sign-ins",
    "linked_accounts": "which Battle.net account belongs to whom",
    "user_watches": "what a named person follows",
    "price_alerts": "an alert is raised for a watch, so it names a market a person asked about",
    "jobs": "operations, not market data",
    "tasks": "operations",
    "task_failures": "operations",
    "cluster_events": "operations",
    "sequences": "operations",
    "node_roles": "operations",
    "admins": "who administers this instance",
    "admin_bootstrap": "whether this deployment consumed its one-shot administrator bootstrap",
    "kv": "boot configuration; a deployment's settings, not the market",
    "_sqlx_migrations": "written by the migration run itself, not copied",
    "catalog_releases": "which catalogue this deployment activated; the seed rebuilds it",
    "market_events": "re-derived from the catalogue at every start",
    "analysis_versions": "derived: this deployment's calculation history",
    "market_current": "derived: rebuilt from the observations on first start",
    "market_windows": "derived: rebuilt from the observations on first start",
    "market_rollup": "derived: rebuilt from the per-realm observations on first start",
}

# The tooltip cache is copied, but only the item tooltips: a category page's
# first cost is reading them (§11b), so a fixture without them measures a page
# nobody has. Everything else in that table is an upstream response cached for
# whoever asked for it.
CACHE_TABLE = "cache"
CACHE_PREFIX = "item-tooltip:"


# --- building a destination from the repository's migrations -----------------


@dataclass(frozen=True)
class Migration:
    version: int
    description: str
    sql: str

    @property
    def checksum(self) -> bytes:
        # What SQLx stores and re-checks at every start: SHA-384 over the
        # migration's bytes. Get it wrong and the server refuses to open the
        # fixture, which is a good failure but a confusing one.
        return hashlib.sha384(self.sql.encode()).digest()


def migrations() -> list[Migration]:
    found = []
    for path in sorted(MIGRATIONS.glob("*.sql")):
        version, _, rest = path.name.partition("_")
        if not rest.endswith(".sql"):
            continue
        found.append(
            Migration(
                version=int(version),
                description=rest[: -len(".sql")].replace("_", " "),
                sql=path.read_text(),
            )
        )
    if not found:
        raise SystemExit(f"no migrations found in {MIGRATIONS}")
    return found


def build_schema(db: sqlite3.Connection) -> int:
    """Apply every migration, and record them the way SQLx would."""
    db.executescript(
        """
        CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        );
        """
    )
    latest = 0
    for migration in migrations():
        db.executescript(migration.sql)
        db.execute(
            "INSERT INTO _sqlx_migrations"
            " (version, description, installed_on, success, checksum, execution_time)"
            " VALUES (?, ?, ?, 1, ?, 0)",
            (
                migration.version,
                migration.description,
                # Fixed rather than `CURRENT_TIMESTAMP`: the synthetic fixture
                # promises the same bytes for the same seed, and a clock in the
                # file would break that on the first rebuild. SQLx reads this
                # column back but never checks it.
                INSTALLED_ON,
                migration.checksum,
            ),
        )
        latest = migration.version
    db.commit()
    return latest


def tables(db: sqlite3.Connection) -> set[str]:
    rows = db.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table'"
        " AND name NOT LIKE 'sqlite_%'"
    ).fetchall()
    return {row[0] for row in rows}


def count_rows(db: sqlite3.Connection, table: str) -> int:
    if table not in tables(db):
        return 0
    return db.execute(f"SELECT count(*) FROM {table}").fetchone()[0]


def check_whitelist(db: sqlite3.Connection) -> None:
    known = COPIED | set(EMPTIED) | {CACHE_TABLE}
    unknown = sorted(tables(db) - known)
    if unknown:
        raise SystemExit(
            "bench-fixture does not know what to do with: "
            + ", ".join(unknown)
            + "\nAdd each to COPIED (source observations, safe to publish in a"
            " fixture) or to EMPTIED (with the reason: whose it is, or that it"
            " is derived and will be rebuilt)."
            "\nRefusing to guess: guessing is how a fixture starts carrying"
            " somebody's session."
        )


# --- sanitize ----------------------------------------------------------------


def consistent_backup(source: Path, into: Path) -> None:
    """A copy taken through SQLite's backup API rather than `cp`.

    The live database has a WAL beside it and a collector writing into it. A
    file copy of the three files is a copy of a moment that never existed;
    this is the moment SQLite says it was.
    """
    # 0600 before a byte of it exists, not after.
    handle = os.open(into, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
    os.close(handle)
    src = sqlite3.connect(f"file:{source}?mode=ro", uri=True)
    dst = sqlite3.connect(into)
    try:
        src.backup(dst)
    finally:
        dst.close()
        src.close()


def copy_table(db: sqlite3.Connection, table: str, where: str = "") -> int:
    db.execute(f"INSERT INTO main.{table} SELECT * FROM src.{table} {where}")
    return db.execute(f"SELECT count(*) FROM main.{table}").fetchone()[0]


def sanitize(args: argparse.Namespace) -> None:
    source = Path(args.source)
    output = Path(args.output)
    if not source.exists():
        raise SystemExit(f"no such database: {source}")
    output.parent.mkdir(parents=True, exist_ok=True)
    for leftover in (output, Path(f"{output}-wal"), Path(f"{output}-shm")):
        leftover.unlink(missing_ok=True)

    started = time.monotonic()
    with tempfile.TemporaryDirectory(dir=output.parent) as tmp:
        snapshot = Path(tmp) / "snapshot.db"
        consistent_backup(source, snapshot)

        db = sqlite3.connect(output)
        db.execute("PRAGMA foreign_keys = ON")
        version = build_schema(db)
        check_whitelist(db)
        # What the migrations themselves put there -- `sequences` is seeded by
        # one. The assertion further down is that copying added nothing to any
        # of these, not that they are empty; a migration's own rows are the
        # schema, not the source database's data.
        baseline = {table: count_rows(db, table) for table in EMPTIED}

        db.execute("ATTACH DATABASE ? AS src", (str(snapshot),))
        present = {
            row[0]
            for row in db.execute(
                "SELECT name FROM src.sqlite_master WHERE type = 'table'"
            )
        }

        counts: dict[str, int] = {}
        for table in sorted(COPIED):
            counts[table] = copy_table(db, table) if table in present else 0
        # Item tooltips only. A category page reads one per card, and a
        # fixture without them measures a page nobody is served.
        if CACHE_TABLE in present:
            db.execute(
                "INSERT INTO main.cache SELECT * FROM src.cache WHERE key LIKE ?",
                (CACHE_PREFIX + "%",),
            )
            counts[CACHE_TABLE] = db.execute(
                "SELECT count(*) FROM main.cache"
            ).fetchone()[0]
        db.commit()

        markets = market_counts(db)
        observed = db.execute(
            "SELECT max(observed_at) FROM ("
            "  SELECT max(observed_at) AS observed_at FROM price_samples"
            "  UNION ALL SELECT max(observed_at) FROM realm_price_samples)"
        ).fetchone()[0]

        db.execute("DETACH DATABASE src")

        # Prove the whitelist held rather than trusting that it did.
        for table in sorted(EMPTIED):
            after = count_rows(db, table)
            if after != baseline[table]:
                raise SystemExit(
                    f"{table} gained {after - baseline[table]} rows and must"
                    f" gain none ({EMPTIED[table]})"
                )

        verify(db)
        db.close()

    if Path(f"{output}-wal").exists() or Path(f"{output}-shm").exists():
        raise SystemExit(
            "a -wal or -shm file survived the VACUUM; the fixture is not one file"
        )

    manifest = write_manifest(
        output,
        kind="sanitized",
        migration_version=version,
        rows=counts,
        markets=markets,
        latest_observation_ms=observed,
        extra={"source": str(source)},
    )
    elapsed = time.monotonic() - started
    report(output, manifest, elapsed)


def verify(db: sqlite3.Connection) -> None:
    """The four checks that say the file is sound before anyone measures it."""
    broken = db.execute("PRAGMA foreign_key_check").fetchall()
    if broken:
        raise SystemExit(f"foreign_key_check found {len(broken)} broken references")
    integrity = db.execute("PRAGMA integrity_check").fetchone()[0]
    if integrity != "ok":
        raise SystemExit(f"integrity_check: {integrity}")
    db.execute("ANALYZE")
    db.execute("PRAGMA optimize")
    db.commit()
    # Outside a transaction, and last: it is what turns the free list -- where
    # a deleted row's bytes would still be -- back into unallocated file.
    db.isolation_level = None
    db.execute("VACUUM")


def market_counts(db: sqlite3.Connection) -> dict[str, int]:
    commodity = db.execute(
        "SELECT count(*) FROM (SELECT DISTINCT item_id, region FROM price_samples)"
    ).fetchone()[0]
    realm = db.execute(
        "SELECT count(*) FROM ("
        " SELECT DISTINCT item_id, region, realm_id, variant_id FROM realm_price_samples)"
    ).fetchone()[0]
    return {"commodity": commodity, "realm_variant": realm, "total": commodity + realm}


def write_manifest(
    output: Path,
    *,
    kind: str,
    migration_version: int,
    rows: dict[str, int],
    markets: dict[str, int],
    latest_observation_ms: int | None,
    extra: dict | None = None,
) -> dict:
    digest = hashlib.sha256()
    with output.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    manifest = {
        "kind": kind,
        "script_version": SCRIPT_VERSION,
        "sha256": digest.hexdigest(),
        "bytes": output.stat().st_size,
        "migration_version": migration_version,
        "rows": rows,
        "markets": markets,
        "latest_observation_ms": latest_observation_ms,
        "built_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    manifest.update(extra or {})
    path = output.with_suffix(output.suffix + ".manifest.json")
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return manifest


def report(output: Path, manifest: dict, elapsed: float) -> None:
    print(f"{output}  ({manifest['bytes']:,} bytes, {elapsed:.1f}s)")
    print(f"  sha256   {manifest['sha256']}")
    print(f"  schema   migration {manifest['migration_version']}")
    for table, count in sorted(manifest["rows"].items()):
        print(f"  {table:<22} {count:>10,}")
    for name, count in sorted(manifest["markets"].items()):
        print(f"  markets/{name:<14} {count:>10,}")
    print(f"  manifest {output}.manifest.json")


# --- synthetic ---------------------------------------------------------------
#
# The numbers below are the archive measured on 2026-08-30 and recorded in
# CLAUDE.md §15. The point is not to be that archive; it is to have its shape,
# so that a query plan, a statement count or an N+1 shows up here too.

REGIONS = ["eu", "us", "kr", "tw"]
COMMODITY_ITEMS = {"eu": 515, "us": 515, "kr": 509, "tw": 501}
REALM_ITEMS = {"eu": 143, "us": 143, "kr": 142, "tw": 141}
REALM_COUNT = {"eu": 92, "us": 84, "kr": 4, "tw": 4}
HOURS = 36
# One per upgrade track, plus the empty variant a recipe has (CLAUDE.md §8).
VARIANTS = [
    "",
    "6652,10844,12833,13332,13662",
    "6652,10844,12834,13333,13662",
    "6652,10844,12835,13333,13662,13696",
    "6652,10844,12841,13334,13662",
]
LOCALES = {"eu": "enGB", "us": "enUS", "kr": "koKR", "tw": "zhTW"}
# A tooltip miss fetches every language at once and caches them all (see
# `ItemDetails::lookup`), so the cache holds one entry per item per locale.
CACHE_LOCALES = [
    "en_US", "en_GB", "es_ES", "es_MX", "de_DE", "fr_FR",
    "it_IT", "pt_BR", "ru_RU", "ko_KR", "zh_TW", "zh_CN",
]
# More items than there are markets, because the real cache holds tooltips for
# items a catalogue has since stopped tracking. 12 x 1540 is the 18,478 entries
# §15 measured.
CACHE_ITEMS = 1540
# A realm/variant market is observed on about 10 of the 36 hours: markets are
# not all collected every cycle, and that is what a coverage gate has to see.
REALM_HOUR_RATE = (0.12, 0.55)


def synthetic(args: argparse.Namespace) -> None:
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    for leftover in (output, Path(f"{output}-wal"), Path(f"{output}-shm")):
        leftover.unlink(missing_ok=True)

    started = time.monotonic()
    rng = random.Random(args.seed)
    db = sqlite3.connect(output)
    db.execute("PRAGMA journal_mode = MEMORY")
    db.execute("PRAGMA synchronous = OFF")
    version = build_schema(db)
    check_whitelist(db)

    # A round hour, so a window boundary in a test is a round number too.
    now = (args.now // 3_600_000) * 3_600_000
    counts = {
        "realms": seed_realms(db, rng),
        "market_variants": seed_market_variants(db),
        "price_samples": seed_commodities(db, rng, now),
        "realm_price_samples": seed_realm_prices(db, rng, now),
        "cache": seed_tooltips(db, rng, now),
    }
    db.commit()
    verify(db)
    db.close()

    manifest = write_manifest(
        output,
        kind="synthetic",
        migration_version=version,
        rows=counts,
        markets=market_counts(sqlite3.connect(output)),
        latest_observation_ms=now,
        extra={"seed": args.seed, "hours": HOURS},
    )
    report(output, manifest, time.monotonic() - started)


def seed_realms(db: sqlite3.Connection, rng: random.Random) -> int:
    rows = []
    for region in REGIONS:
        for index in range(REALM_COUNT[region]):
            realm_id = 1000 + index
            # Some houses join several realms, which is what makes the picker
            # list more entries than there are auction houses (§7).
            joined = 1 if index % 3 else rng.randint(2, 4)
            members = [f"{region.upper()}-Realm-{realm_id}-{n}" for n in range(joined)]
            rows.append(
                (
                    realm_id,
                    region,
                    ", ".join(members),
                    1,
                    LOCALES[region],
                    json.dumps(members),
                )
            )
    db.executemany(
        "INSERT INTO realms (realm_id, region, name, enabled, locale, members)"
        " VALUES (?, ?, ?, ?, ?, ?)",
        rows,
    )
    return len(rows)


def price_walk(rng: random.Random, hours: int) -> list[int]:
    """A price series with a level, drift, noise and the occasional spike.

    Not a model of anything. It exists so that a percentile, an IQR and an
    anomaly gate have something with tails to be calculated over, rather than
    a straight line on which every statistic is the same number.
    """
    level = rng.choice([rng.randint(5_000, 80_000), rng.randint(100_000, 4_000_000)])
    drift = rng.uniform(-0.004, 0.004)
    series = []
    for hour in range(hours):
        level = max(100, int(level * (1 + drift + rng.gauss(0, 0.02))))
        if rng.random() < 0.02:
            level = int(level * rng.uniform(1.8, 4.0))  # a spike with a tail
        series.append(level)
    return series


def seed_commodities(db: sqlite3.Connection, rng: random.Random, now: int) -> int:
    rows = []
    for region in REGIONS:
        for index in range(COMMODITY_ITEMS[region]):
            item = 200_000 + index
            # Not every market is observed every hour: gaps are what the
            # coverage and freshness statistics have to survive.
            observed = [
                hour for hour in range(HOURS) if rng.random() > 0.12
            ]
            prices = price_walk(rng, HOURS)
            for hour in observed:
                minimum = prices[hour]
                p05 = int(minimum * rng.uniform(1.0, 1.08))
                median = int(p05 * rng.uniform(1.0, 1.35))
                # A market that is listed but empty happens, and a card has to
                # say so rather than show a zero.
                quantity = 0 if rng.random() < 0.03 else rng.randint(1, 40_000)
                rows.append(
                    (
                        item,
                        region,
                        now - (HOURS - 1 - hour) * 3_600_000,
                        minimum,
                        p05,
                        median,
                        quantity,
                        rng.randint(1, 900),
                    )
                )
    db.executemany(
        "INSERT INTO price_samples (item_id, region, observed_at, min_unit,"
        " p05_unit, median_unit, quantity, listings) VALUES (?,?,?,?,?,?,?,?)",
        rows,
    )
    return len(rows)


def seed_realm_prices(db: sqlite3.Connection, rng: random.Random, now: int) -> int:
    variant_ids = {
        variant: variant_id
        for variant_id, variant in db.execute(
            "SELECT variant_id, variant FROM market_variants"
        )
    }
    rows = []
    for region in REGIONS:
        realms = [1000 + index for index in range(REALM_COUNT[region])]
        for index in range(REALM_ITEMS[region]):
            item = 300_000 + index
            # A recipe has one version of itself; a BoE has one row per track.
            variants = [""] if index % 2 else VARIANTS[1:]
            for realm in realms:
                # Two separate things, and conflating them was wrong: how many
                # of an item's variants a realm lists at all, and how often
                # that market is caught by a snapshot. A thin realm with two
                # observations is the case a coverage gate has to catch.
                density = rng.choice([0.15, 0.5, 0.95])
                hour_rate = rng.uniform(*REALM_HOUR_RATE)
                for variant in variants:
                    if rng.random() > density:
                        continue
                    prices = price_walk(rng, HOURS)
                    for hour in range(HOURS):
                        if rng.random() > hour_rate:
                            continue
                        minimum = prices[hour]
                        median = int(minimum * rng.uniform(1.0, 1.6))
                        rows.append(
                            (
                                item,
                                region,
                                realm,
                                variant_ids[variant],
                                now - (HOURS - 1 - hour) * 3_600_000,
                                minimum,
                                median,
                                rng.randint(1, 12),
                                int(median * rng.uniform(1.0, 2.2)),
                            )
                        )
    db.executemany(
        "INSERT INTO realm_price_samples (item_id, region, realm_id, variant_id,"
        " observed_at, min_price, median_price, listings, max_price)"
        " VALUES (?,?,?,?,?,?,?,?,?)",
        rows,
    )
    return len(rows)


def seed_market_variants(db: sqlite3.Connection) -> int:
    db.executemany(
        "INSERT INTO market_variants (variant) VALUES (?)",
        ((variant,) for variant in VARIANTS),
    )
    return len(VARIANTS)


def seed_tooltips(db: sqlite3.Connection, rng: random.Random, now: int) -> int:
    """Cached tooltips, in the shape and roughly the size the real ones are.

    §15 records 18,478 entries holding 9,851,153 value bytes: about 533 bytes
    each. A fixture whose tooltips are 40 bytes measures a cache read nobody
    performs, and reading the tooltips was the single largest cost §11b found
    on a category page.
    """
    rows = []
    ttl = now + 7 * 24 * 3_600_000
    for locale in CACHE_LOCALES:
        for offset in range(CACHE_ITEMS):
            # The tracked items first, then tooltips for items no catalogue
            # follows any more -- which is what the real table looks like a
            # patch after it was filled.
            item = 200_000 + offset if offset < 1_000 else 300_000 + offset - 1_000
            body = {
                "item": item,
                "name": f"Item {item}",
                "quality": rng.choice(["Common", "Rare", "Epic"]),
                "icon": f"https://render.worldofwarcraft.com/icons/56/{item}.jpg",
                "text": "x" * rng.randint(300, 700),
            }
            rows.append((f"item-tooltip:v3:{locale}:{item}", json.dumps(body).encode(), ttl))
    db.executemany("INSERT INTO cache (key, value, expires_at) VALUES (?, ?, ?)", rows)
    return len(rows)


# --- entry point -------------------------------------------------------------


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    clean = sub.add_parser(
        "sanitize", help="build the authoritative fixture from a real archive"
    )
    clean.add_argument("--source", default="data/cluster.db")
    clean.add_argument("--output", default="data/bench/market-realistic.db")
    clean.set_defaults(run=sanitize)

    made = sub.add_parser(
        "synthetic", help="build the deterministic fixture tests and CI use"
    )
    made.add_argument("--output", default="target/bench/market-synthetic.db")
    made.add_argument("--seed", type=int, default=20260830)
    made.add_argument(
        "--now",
        type=int,
        default=1_788_048_000_000,
        help="epoch milliseconds the newest observation lands on",
    )
    made.set_defaults(run=synthetic)

    args = parser.parse_args(argv)
    args.run(args)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
