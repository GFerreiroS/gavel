#!/usr/bin/env python3
"""Measure §4.6 market-state changes without modifying the source database.

The current schema has no collection ledger.  A snapshot hour is therefore
known only when at least one sample was stored for a configured realm/region.
The tool refuses incomplete hours rather than treating them as unchanged.
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path
from urllib.parse import quote


HOUR_MS = 60 * 60 * 1000
WINDOW_HOURS = 48


class MeasurementError(Exception):
    """The database cannot produce a trustworthy measurement."""


@dataclass(frozen=True)
class MarketState:
    """The one §4.3 market state used by both market shapes."""

    min_price: int
    median_price: int
    listings: int
    steps: str | None


@dataclass(frozen=True)
class MissingSnapshot:
    market_type: str
    region: str
    realm_id: int | None
    hour_ms: int


@dataclass(frozen=True)
class MarketResult:
    compared: int
    unchanged: int
    appeared: int
    disappeared: int
    missing_ladders: int

    @property
    def changed(self) -> int:
        return self.compared - self.unchanged

    @property
    def unchanged_rate(self) -> float | None:
        if self.compared == 0:
            return None
        return self.unchanged / self.compared


@dataclass(frozen=True)
class Report:
    start_ms: int
    end_ms: int
    include_ladders: bool
    missing_snapshots: tuple[MissingSnapshot, ...]
    missing_ladders: dict[str, int]
    results: dict[str, MarketResult] | None

    @property
    def complete(self) -> bool:
        return not self.missing_snapshots and not any(self.missing_ladders.values())


def state_changed(
    previous: MarketState, current: MarketState, include_ladders: bool
) -> bool:
    """Return whether min price, median, listings, or enabled ladder steps changed."""

    if (
        previous.min_price != current.min_price
        or previous.median_price != current.median_price
        or previous.listings != current.listings
    ):
        return True
    return include_ladders and previous.steps != current.steps


def readonly_connection(path: Path) -> sqlite3.Connection:
    """Open a database through SQLite's read-only URI mode."""

    if not path.is_file():
        raise MeasurementError(f"database not found: {path}")
    uri = f"file:{quote(str(path.resolve()), safe='/')}?mode=ro"
    try:
        return sqlite3.connect(uri, uri=True)
    except sqlite3.Error as error:
        raise MeasurementError(f"could not open database read-only: {error}") from error


def require_tables(connection: sqlite3.Connection, include_ladders: bool) -> None:
    tables = {
        row[0]
        for row in connection.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table'"
        )
    }
    required = {"price_samples", "realm_price_samples", "realms"}
    if include_ladders:
        required.update({"price_ladders", "realm_price_ladders"})
    missing = sorted(required - tables)
    if missing:
        raise MeasurementError(f"database is missing required table(s): {', '.join(missing)}")


def bucket(at: int) -> int:
    return at // HOUR_MS * HOUR_MS


def window_end(connection: sqlite3.Connection, requested_end: int | None) -> int:
    if requested_end is not None:
        if requested_end <= 0 or requested_end % HOUR_MS:
            raise MeasurementError("--end-ms must be a positive UTC hour boundary in milliseconds")
        return requested_end

    latest = connection.execute(
        """
        SELECT MAX(observed_at)
        FROM (
            SELECT observed_at FROM price_samples
            UNION ALL
            SELECT observed_at FROM realm_price_samples
        )
        """
    ).fetchone()[0]
    if latest is None:
        raise MeasurementError("database contains no price observations")
    return bucket(int(latest)) + HOUR_MS


def expected_hours(start_ms: int, end_ms: int) -> range:
    return range(start_ms, end_ms, HOUR_MS)


def missing_snapshot_hours(
    connection: sqlite3.Connection, start_ms: int, end_ms: int
) -> tuple[MissingSnapshot, ...]:
    """Find every configured realm/known commodity region without a sample hour."""

    expected = set(expected_hours(start_ms, end_ms))
    missing: list[MissingSnapshot] = []

    realms = connection.execute(
        "SELECT region, realm_id FROM realms ORDER BY region, realm_id"
    ).fetchall()
    for region, realm_id in realms:
        observed = {
            bucket(int(row[0]))
            for row in connection.execute(
                """
                SELECT DISTINCT observed_at
                FROM realm_price_samples
                WHERE region = ? AND realm_id = ?
                  AND observed_at >= ? AND observed_at < ?
                """,
                (region, realm_id, start_ms, end_ms),
            )
        }
        for hour in sorted(expected - observed):
            missing.append(MissingSnapshot("per-realm", region, int(realm_id), hour))

    regions = connection.execute(
        "SELECT DISTINCT region FROM price_samples ORDER BY region"
    ).fetchall()
    for (region,) in regions:
        observed = {
            bucket(int(row[0]))
            for row in connection.execute(
                """
                SELECT DISTINCT observed_at
                FROM price_samples
                WHERE region = ? AND observed_at >= ? AND observed_at < ?
                """,
                (region, start_ms, end_ms),
            )
        }
        for hour in sorted(expected - observed):
            missing.append(MissingSnapshot("commodity", region, None, hour))

    if not realms:
        raise MeasurementError("realms table has no configured realms to measure")
    if not regions:
        raise MeasurementError("price_samples contains no commodity regions to measure")
    return tuple(missing)


def sample_rows(
    connection: sqlite3.Connection,
    market_type: str,
    start_ms: int,
    end_ms: int,
    include_ladders: bool,
) -> list[tuple[tuple[object, ...], int, MarketState]]:
    if market_type == "per-realm":
        ladder_join = """
            LEFT JOIN realm_price_ladders AS l
              ON l.item_id = p.item_id AND l.region = p.region
             AND l.realm_id = p.realm_id AND l.variant = p.variant
             AND l.observed_at = p.observed_at
        """ if include_ladders else ""
        steps = "l.steps" if include_ladders else "NULL"
        query = f"""
            SELECT p.region, p.realm_id, p.item_id, p.variant, p.observed_at,
                   p.min_price, p.median_price, p.listings, {steps}
            FROM realm_price_samples AS p
            {ladder_join}
            WHERE p.observed_at >= ? AND p.observed_at < ?
            ORDER BY p.region, p.realm_id, p.item_id, p.variant, p.observed_at
        """
        return [
            (
                (region, int(realm_id), int(item_id), variant),
                int(observed_at),
                MarketState(int(min_price), int(median_price), int(listings), steps),
            )
            for region, realm_id, item_id, variant, observed_at, min_price, median_price, listings, steps
            in connection.execute(query, (start_ms, end_ms))
        ]

    ladder_join = """
        LEFT JOIN price_ladders AS l
          ON l.item_id = p.item_id AND l.region = p.region
         AND l.observed_at = p.observed_at
    """ if include_ladders else ""
    steps = "l.steps" if include_ladders else "NULL"
    query = f"""
        SELECT p.region, p.item_id, p.observed_at,
               p.min_unit, p.median_unit, p.listings, {steps}
        FROM price_samples AS p
        {ladder_join}
        WHERE p.observed_at >= ? AND p.observed_at < ?
        ORDER BY p.region, p.item_id, p.observed_at
    """
    return [
        (
            (region, int(item_id)),
            int(observed_at),
            MarketState(int(min_price), int(median_price), int(listings), steps),
        )
        for region, item_id, observed_at, min_price, median_price, listings, steps
        in connection.execute(query, (start_ms, end_ms))
    ]


def measure_market_type(
    rows: list[tuple[tuple[object, ...], int, MarketState]],
    start_ms: int,
    end_ms: int,
    include_ladders: bool,
) -> MarketResult:
    """Compare only adjacent observed hours; gaps become explicit events."""

    by_market: dict[tuple[object, ...], dict[int, MarketState]] = {}
    missing_ladders = 0
    for key, observed_at, state in rows:
        hour = bucket(observed_at)
        states = by_market.setdefault(key, {})
        if hour in states:
            raise MeasurementError(
                "multiple observations for one market in one hour; "
                "the hourly measurement would have to choose one"
            )
        states[hour] = state
        if include_ladders and state.steps is None:
            missing_ladders += 1

    compared = unchanged = appeared = disappeared = 0
    for states in by_market.values():
        previous: MarketState | None = None
        for hour in expected_hours(start_ms, end_ms):
            current = states.get(hour)
            if previous is not None and current is not None:
                compared += 1
                if not state_changed(previous, current, include_ladders):
                    unchanged += 1
            elif previous is None and current is not None and hour != start_ms:
                appeared += 1
            elif previous is not None and current is None:
                disappeared += 1
            previous = current

    return MarketResult(compared, unchanged, appeared, disappeared, missing_ladders)


def analyse(
    database: Path, end_ms: int | None = None, include_ladders: bool = True
) -> Report:
    """Produce a report or a coverage failure, without modifying ``database``."""

    connection = readonly_connection(database)
    try:
        require_tables(connection, include_ladders)
        end = window_end(connection, end_ms)
        start = end - WINDOW_HOURS * HOUR_MS
        missing = missing_snapshot_hours(connection, start, end)

        rows = {
            market_type: sample_rows(
                connection, market_type, start, end, include_ladders
            )
            for market_type in ("per-realm", "commodity")
        }
        provisional = {
            market_type: measure_market_type(
                market_rows, start, end, include_ladders
            )
            for market_type, market_rows in rows.items()
        }
        missing_ladders = {
            market_type: result.missing_ladders
            for market_type, result in provisional.items()
        }
        if missing or any(missing_ladders.values()):
            return Report(start, end, include_ladders, missing, missing_ladders, None)
        return Report(start, end, include_ladders, missing, missing_ladders, provisional)
    finally:
        connection.close()


def utc(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def report_json(report: Report) -> str:
    payload = {
        "window": {
            "start_ms": report.start_ms,
            "end_ms": report.end_ms,
            "start_utc": utc(report.start_ms),
            "end_utc": utc(report.end_ms),
            "hours": WINDOW_HOURS,
        },
        "include_ladders": report.include_ladders,
        "complete": report.complete,
        "missing_snapshot_hours": [asdict(missing) for missing in report.missing_snapshots],
        "missing_ladders": report.missing_ladders,
        "markets": (
            None
            if report.results is None
            else {
                market_type: {
                    **asdict(result),
                    "changed": result.changed,
                    "unchanged_rate": result.unchanged_rate,
                }
                for market_type, result in report.results.items()
            }
        ),
    }
    return json.dumps(payload, indent=2, sort_keys=True)


def print_human(report: Report) -> None:
    print(
        f"Window: {utc(report.start_ms)} through {utc(report.end_ms)} "
        f"(exclusive, {WINDOW_HOURS} hours)"
    )
    print(f"Ladder steps included: {'yes' if report.include_ladders else 'no'}")
    print()
    print("Coverage")
    print("type       missing snapshot hours  missing ladder rows")
    for market_type in ("per-realm", "commodity"):
        missing_hours = sum(
            missing.market_type == market_type for missing in report.missing_snapshots
        )
        print(
            f"{market_type:<10} {missing_hours:>22}  "
            f"{report.missing_ladders[market_type]:>19}"
        )

    if not report.complete:
        print("\nCoverage is incomplete; no change rate was calculated.")
        for missing in report.missing_snapshots:
            realm = "-" if missing.realm_id is None else str(missing.realm_id)
            print(
                f"missing snapshot hour: {missing.market_type} "
                f"region={missing.region} realm_id={realm} hour={utc(missing.hour_ms)}"
            )
        return

    assert report.results is not None
    print("\nChange rate")
    print("type       compared  unchanged  changed  unchanged rate  appeared  disappeared")
    for market_type in ("per-realm", "commodity"):
        result = report.results[market_type]
        rate = f"{result.unchanged_rate * 100:.2f}%"
        print(
            f"{market_type:<10} {result.compared:>8}  {result.unchanged:>9}  "
            f"{result.changed:>7}  {rate:>14}  {result.appeared:>8}  "
            f"{result.disappeared:>11}"
        )


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Measure §4.6 change rates from a SQLite database without writing to it."
    )
    parser.add_argument("database", type=Path, help="SQLite database to open read-only")
    parser.add_argument(
        "--end-ms",
        type=int,
        help="exclusive UTC hour boundary in Unix milliseconds; defaults to latest sample hour",
    )
    parser.add_argument(
        "--include-ladders",
        dest="include_ladders",
        action="store_true",
        default=True,
        help="include ladder steps in the change predicate (default)",
    )
    parser.add_argument(
        "--no-include-ladders",
        dest="include_ladders",
        action="store_false",
        help="compare only min price, median price, and listings",
    )
    parser.add_argument("--json", action="store_true", help="emit JSON instead of the human table")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_arguments(sys.argv[1:] if argv is None else argv)
    try:
        report = analyse(args.database, args.end_ms, args.include_ladders)
    except MeasurementError as error:
        print(f"change-rate: {error}", file=sys.stderr)
        return 2

    if args.json:
        print(report_json(report))
    else:
        print_human(report)
    return 0 if report.complete else 1


if __name__ == "__main__":
    raise SystemExit(main())
