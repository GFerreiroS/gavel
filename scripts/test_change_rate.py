#!/usr/bin/env python3
"""Tests for the read-only §4.6 change-rate harness."""

from __future__ import annotations

import importlib.util
import contextlib
import io
import json
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("change-rate.py")
SPEC = importlib.util.spec_from_file_location("change_rate", SCRIPT)
assert SPEC and SPEC.loader
change_rate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = change_rate
SPEC.loader.exec_module(change_rate)


class ChangeRateTests(unittest.TestCase):
    def database(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        directory = tempfile.TemporaryDirectory()
        path = Path(directory.name) / "market.db"
        connection = sqlite3.connect(path)
        connection.executescript(
            """
            CREATE TABLE realms (region TEXT NOT NULL, realm_id INTEGER NOT NULL);
            CREATE TABLE realm_price_samples (
                item_id INTEGER, region TEXT, realm_id INTEGER, variant TEXT,
                observed_at INTEGER, min_price INTEGER, median_price INTEGER,
                listings INTEGER
            );
            CREATE TABLE realm_price_ladders (
                item_id INTEGER, region TEXT, realm_id INTEGER, variant TEXT,
                observed_at INTEGER, steps TEXT
            );
            CREATE TABLE price_samples (
                item_id INTEGER, region TEXT, observed_at INTEGER,
                min_unit INTEGER, p05_unit INTEGER, median_unit INTEGER,
                quantity INTEGER, listings INTEGER
            );
            CREATE TABLE price_ladders (
                item_id INTEGER, region TEXT, observed_at INTEGER, steps TEXT
            );
            """
        )
        connection.execute("INSERT INTO realms VALUES ('eu', 1)")
        for hour in range(change_rate.WINDOW_HOURS):
            observed_at = hour * change_rate.HOUR_MS
            steps = "100:1" if hour < 2 else "100:2"
            connection.execute(
                "INSERT INTO realm_price_samples VALUES (1, 'eu', 1, '', ?, 100, 100, 1)",
                (observed_at,),
            )
            connection.execute(
                "INSERT INTO realm_price_ladders VALUES (1, 'eu', 1, '', ?, ?)",
                (observed_at, steps),
            )
            connection.execute(
                "INSERT INTO price_samples VALUES (2, 'eu', ?, 200, 200, 200, 5, 1)",
                (observed_at,),
            )
            connection.execute(
                "INSERT INTO price_ladders VALUES (2, 'eu', ?, '200:5')",
                (observed_at,),
            )
        connection.commit()
        connection.close()
        return directory, path

    def test_ladder_change_counts_only_when_enabled(self) -> None:
        directory, path = self.database()
        self.addCleanup(directory.cleanup)

        with_ladders = change_rate.analyse(path)

        self.assertTrue(with_ladders.complete)
        self.assertEqual(with_ladders.results["per-realm"].compared, 47)
        self.assertEqual(with_ladders.results["per-realm"].unchanged, 46)
        self.assertEqual(with_ladders.results["commodity"].unchanged, 47)

        connection = sqlite3.connect(path)
        connection.execute("DROP TABLE realm_price_ladders")
        connection.execute("DROP TABLE price_ladders")
        connection.commit()
        connection.close()

        without_ladders = change_rate.analyse(path, include_ladders=False)
        self.assertEqual(without_ladders.results["per-realm"].unchanged, 47)

    def test_market_appearances_and_disappearances_are_explicit(self) -> None:
        directory, path = self.database()
        self.addCleanup(directory.cleanup)
        connection = sqlite3.connect(path)
        for hour in range(10, 31):
            observed_at = hour * change_rate.HOUR_MS
            connection.execute(
                "INSERT INTO realm_price_samples VALUES (3, 'eu', 1, '', ?, 300, 300, 1)",
                (observed_at,),
            )
            connection.execute(
                "INSERT INTO realm_price_ladders VALUES (3, 'eu', 1, '', ?, '300:1')",
                (observed_at,),
            )
        connection.commit()
        connection.close()

        report = change_rate.analyse(path)
        result = report.results["per-realm"]
        self.assertEqual(result.appeared, 1)
        self.assertEqual(result.disappeared, 1)
        self.assertEqual(result.compared, 67)
        self.assertEqual(result.unchanged, 66)

    def test_missing_snapshot_hour_blocks_the_rate_and_lists_the_gap(self) -> None:
        directory, path = self.database()
        self.addCleanup(directory.cleanup)
        connection = sqlite3.connect(path)
        observed_at = 17 * change_rate.HOUR_MS
        connection.execute(
            "DELETE FROM realm_price_samples WHERE item_id = 1 AND observed_at = ?",
            (observed_at,),
        )
        connection.execute(
            "DELETE FROM realm_price_ladders WHERE item_id = 1 AND observed_at = ?",
            (observed_at,),
        )
        connection.commit()
        connection.close()

        report = change_rate.analyse(path)
        self.assertFalse(report.complete)
        self.assertIsNone(report.results)
        self.assertEqual(
            report.missing_snapshots,
            (
                change_rate.MissingSnapshot(
                    "per-realm", "eu", 1, 17 * change_rate.HOUR_MS
                ),
            ),
        )
        payload = json.loads(change_rate.report_json(report))
        self.assertFalse(payload["complete"])
        self.assertIsNone(payload["markets"])

        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            exit_code = change_rate.main([str(path)])
        self.assertEqual(exit_code, 1)
        self.assertIn("Coverage is incomplete; no change rate was calculated.", output.getvalue())

    def test_json_cli_output_contains_separate_market_results(self) -> None:
        directory, path = self.database()
        self.addCleanup(directory.cleanup)

        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            exit_code = change_rate.main([str(path), "--json"])
        payload = json.loads(output.getvalue())

        self.assertEqual(exit_code, 0)
        self.assertTrue(payload["complete"])
        self.assertEqual(payload["markets"]["per-realm"]["compared"], 47)
        self.assertEqual(payload["markets"]["commodity"]["compared"], 47)

    def test_cli_rejects_a_missing_database(self) -> None:
        missing = Path(tempfile.gettempdir()) / "gavel-change-rate-missing.db"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), str(missing)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("database not found", result.stderr)

    def test_database_connection_is_read_only(self) -> None:
        directory, path = self.database()
        self.addCleanup(directory.cleanup)

        connection = change_rate.readonly_connection(path)
        self.addCleanup(connection.close)
        with self.assertRaises(sqlite3.OperationalError):
            connection.execute("INSERT INTO realms VALUES ('us', 2)")


if __name__ == "__main__":
    unittest.main()
