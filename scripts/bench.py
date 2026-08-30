#!/usr/bin/env python3
"""Measure the read path, and say which stage moved.

Phase 0's exit gate is two sentences: the benchmark detects a regression, and
the timings identify which stage caused it. So this does not report one number
per endpoint. It reports, per endpoint and per build profile:

  * time to first byte, cold and warm, at p50/p95/p99;
  * the `Server-Timing` breakdown -- database, cache, analysis, template;
  * statements executed and rows decoded;
  * response bytes, uncompressed and over the wire.

Two profiles, because the shipped one is not the informative one. `release` is
`opt-level = "z"`: it is what runs on the server and therefore what a capacity
claim has to be made against, but part of what it measures is the compiler
optimising for size. `release-fast` is the same build at `opt-level = 3`. The
difference between the columns is how much of a number belongs to the profile
rather than to the code.

The database is a fixture from `bench-fixture.py`, never the live one: a
benchmark that writes to the archive it is measuring is measuring itself.

    python3 scripts/bench.py                        # both profiles, warm + cold
    python3 scripts/bench.py --profile release-fast
    python3 scripts/bench.py --baseline docs/bench/baseline.json

CLAUDE.md §11b: measure against the real archive, in a release build, and count
the queries. All three are this script's job.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# What a visitor actually waits for. The shell/fragment split is deliberate:
# §11b made every category page a shell plus a fragment, so a single number for
# "the consumables page" would hide which half moved.
@dataclass(frozen=True)
class Endpoint:
    name: str
    path: str
    note: str


ENDPOINTS = [
    Endpoint("gear shell", "/wow/auctions/gear", "paints immediately; one small query"),
    Endpoint("gear cards", "/partials/gear", "EU, all realms: nine cards from ~18k markets"),
    Endpoint("consumable cards", "/partials/consumables", "curated commodity list"),
    Endpoint("reagent cards", "/partials/reagents", "every reagent of the expansion: 223 cards"),
    Endpoint("enchant cards", "/partials/enchants", "generated from the catalogue"),
    Endpoint("commodity analysis", "/wow/item/237367", "one market's whole history, reduced per request"),
    Endpoint("BoE analysis", "/wow/gear/271441/hero", "one track on every realm of a region"),
]

PROFILES = {
    # name: (cargo flag, target directory)
    "debug": ([], "debug"),
    "release": (["--release"], "release"),
    "release-fast": (["--profile", "release-fast"], "release-fast"),
}

STAGES = ["db", "cache", "calc", "tpl"]


# --- running a server we own -------------------------------------------------


class Server:
    """A server process on a fixture, with timings on and collection off.

    Started by the benchmark rather than attached to, because a measurement
    has to know what the process was doing: an attached server might be
    collecting, might be serving somebody, and might not be the build being
    measured.
    """

    def __init__(self, binary: Path, database: Path, port: int, log: Path):
        self.binary = binary
        self.database = database
        self.port = port
        self.log = log
        self.process: subprocess.Popen | None = None

    def __enter__(self) -> "Server":
        environment = dict(os.environ)
        environment.update(
            {
                "APP_DATABASE": str(self.database),
                "APP_PORT": str(self.port),
                "APP_HOST": "127.0.0.1",
                # No in-process workers and no collection: the read path is
                # what is being measured, and a collector writing underneath it
                # would be measured too.
                "APP_WORKERS": "0",
                "APP_SERVER_TIMING": "true",
                "APP_LOG": "error",
                # Never the real credentials: a benchmark must not call
                # Blizzard, and without them the collector cannot start.
                "BLIZZARD_CLIENT_ID": "",
                "BLIZZARD_CLIENT_SECRET": "",
            }
        )
        self.log.parent.mkdir(parents=True, exist_ok=True)
        handle = self.log.open("wb")
        self.process = subprocess.Popen(
            [str(self.binary)],
            cwd=REPO,
            env=environment,
            stdout=handle,
            stderr=subprocess.STDOUT,
        )
        self.await_ready()
        return self

    def __exit__(self, *_) -> None:
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()

    def await_ready(self, seconds: float = 60.0) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if self.process and self.process.poll() is not None:
                raise SystemExit(
                    f"the server exited before it was ready; see {self.log}\n"
                    + self.log.read_text()[-2000:]
                )
            try:
                with socket.create_connection(("127.0.0.1", self.port), 0.25):
                    pass
            except OSError:
                time.sleep(0.1)
                continue
            if fetch(self.port, "/healthz").status == 204:
                return
        raise SystemExit(f"the server never became ready; see {self.log}")


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


# --- one request -------------------------------------------------------------


@dataclass
class Reply:
    status: int
    ttfb_ms: float
    wire_bytes: int
    body_bytes: int = 0
    stages_ms: dict = field(default_factory=dict)
    statements: int = 0
    rows: int = 0


TIMING = re.compile(r"([a-z]+);(?:dur=([0-9.]+)|desc=\"([0-9]+)\")")


def fetch(port: int, path: str, compressed: bool = True) -> Reply:
    """One request, timed by curl the way §15's baseline was taken.

    `time_starttransfer` is time to first byte, which for these responses is
    the whole of the server's work: they are rendered before they are sent.
    """
    headers = ["-H", "Accept-Encoding: gzip, br"] if compressed else ["-H", "Accept-Encoding: identity"]
    result = subprocess.run(
        [
            "curl", "-sS", "-o", "/dev/null", "-D", "-",
            *headers,
            "-w", "\n__curl__ %{time_starttransfer} %{size_download} %{http_code}\n",
            f"http://127.0.0.1:{port}{path}",
        ],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if result.returncode != 0:
        raise SystemExit(f"curl failed for {path}: {result.stderr.strip()}")

    stages: dict[str, float] = {}
    statements = rows = body = 0
    ttfb = 0.0
    wire = 0
    status = 0
    for line in result.stdout.splitlines():
        lowered = line.lower()
        if lowered.startswith("server-timing:"):
            for key, duration, count in TIMING.findall(line.split(":", 1)[1]):
                if duration:
                    stages[key] = float(duration)
                elif key == "q":
                    statements = int(count)
                elif key == "rows":
                    rows = int(count)
                elif key == "bytes":
                    body = int(count)
        elif line.startswith("__curl__"):
            _, start, size, code = line.split()
            ttfb, wire, status = float(start) * 1000.0, int(size), int(code)
    return Reply(status, ttfb, wire, body, stages, statements, rows)


def quantile(values: list[float], fraction: float) -> float:
    """Nearest-rank, which is honest about 15 samples.

    Interpolating between two of fifteen observations invents a precision the
    sample does not have. `docs/market-analysis.md` §5.1 asks for Hyndman-Fan
    R8 for *market* percentiles; this is a latency sample of a few runs and the
    simpler definition is the more truthful one here.
    """
    if not values:
        return 0.0
    ordered = sorted(values)
    rank = max(1, min(len(ordered), int(-(-fraction * len(ordered) // 1))))
    return ordered[rank - 1]


# --- measuring ---------------------------------------------------------------


def measure(port: int, endpoint: Endpoint, warm: int) -> dict:
    warmed = [fetch(port, endpoint.path) for _ in range(warm)]
    bad = [reply for reply in warmed if reply.status != 200]
    if bad:
        raise SystemExit(
            f"{endpoint.path} answered {bad[0].status}; a benchmark of an error"
            " page is not a benchmark"
        )
    uncompressed = fetch(port, endpoint.path, compressed=False)
    totals = [reply.ttfb_ms for reply in warmed]
    last = warmed[-1]
    return {
        "p50_ms": round(statistics.median(totals), 2),
        "p95_ms": round(quantile(totals, 0.95), 2),
        "p99_ms": round(quantile(totals, 0.99), 2),
        "mean_ms": round(statistics.fmean(totals), 2),
        "stages_ms": {stage: last.stages_ms.get(stage, 0.0) for stage in STAGES},
        "statements": last.statements,
        "rows": last.rows,
        "bytes_uncompressed": uncompressed.wire_bytes or last.body_bytes,
        "bytes_wire": last.wire_bytes,
        "samples": warm,
    }


def build(profile: str) -> Path:
    flags, directory = PROFILES[profile]
    print(f"  building {profile} ...", flush=True)
    subprocess.run(
        ["cargo", "build", "-p", "server", *flags],
        cwd=REPO,
        check=True,
        stdout=subprocess.DEVNULL,
    )
    return REPO / "target" / directory / "server"


def run_profile(profile: str, database: Path, warm: int, logs: Path) -> dict:
    binary = build(profile)
    port = free_port()
    results: dict[str, dict] = {}

    # Never measure against the fixture itself. The server migrates and
    # materialises on first start, and a benchmark that mutates the archive it
    # is measuring has changed the thing under it -- and would break the
    # manifest's SHA-256, which is the fixture's whole claim to being
    # reproducible. One working copy per profile, reused across the restarts
    # below so the read model is built once.
    working = REPO / "target" / "bench" / f"working-{profile}.db"
    working.parent.mkdir(parents=True, exist_ok=True)
    for leftover in (working, Path(f"{working}-wal"), Path(f"{working}-shm")):
        leftover.unlink(missing_ok=True)
    shutil.copyfile(database, working)
    database = working

    # Cold first, one fresh process per endpoint. "Cold" here means this
    # process has never answered this endpoint -- SQLite's page cache and the
    # pool are empty. It does not mean the file is out of the operating
    # system's cache, which cannot be arranged without root, and the number is
    # reported as what it is.
    for endpoint in ENDPOINTS:
        with Server(binary, database, port, logs / f"{profile}-cold.log"):
            first = fetch(port, endpoint.path)
        results[endpoint.name] = {"cold_ms": round(first.ttfb_ms, 2)}
        print(f"  {endpoint.name:<20} cold {first.ttfb_ms:8.1f} ms", flush=True)

    with Server(binary, database, port, logs / f"{profile}-warm.log"):
        for endpoint in ENDPOINTS:
            warmed = measure(port, endpoint, warm)
            results[endpoint.name].update(warmed)
            results[endpoint.name]["path"] = endpoint.path
            results[endpoint.name]["note"] = endpoint.note
            print(
                f"  {endpoint.name:<20} warm p50 {warmed['p50_ms']:8.1f}"
                f"  p95 {warmed['p95_ms']:8.1f}"
                f"  q={warmed['statements']:<4} rows={warmed['rows']:<8}"
                f" {warmed['bytes_uncompressed']:>8} B",
                flush=True,
            )
    return results


# --- reporting ---------------------------------------------------------------


def markdown(report: dict) -> str:
    profiles = list(report["profiles"])
    lines = ["| Endpoint | " + " | ".join(f"{p} p50 / p95" for p in profiles) + " |"]
    lines.append("|---|" + "---:|" * len(profiles))
    for endpoint in ENDPOINTS:
        cells = []
        for profile in profiles:
            row = report["profiles"][profile].get(endpoint.name, {})
            cells.append(f"{row.get('p50_ms', 0):.1f} / {row.get('p95_ms', 0):.1f} ms")
        lines.append(f"| {endpoint.name} | " + " | ".join(cells) + " |")
    return "\n".join(lines)


def compare(report: dict, baseline: dict, tolerance: float, floor_ms: float) -> int:
    """Fail on a regression, and name the stage that caused it.

    **Both quantiles have to move.** At the default 15 warm samples, the
    nearest-rank p95 is the fifteenth of fifteen -- it is the slowest request,
    not a percentile, and one scheduling hiccup is enough to move it a third.
    That produced a false regression on the Gear page during Phase 2: identical
    statements, identical rows, no stage changed, p50 unmoved, and a p95 twenty
    per cent higher that vanished at `--warm 60`.

    So p50 is the signal and p95 is the budget. A real slowdown moves both. The
    cost of that rule is that a change which only fattens the tail is below
    this benchmark's resolution at 15 samples; raise `--warm` to see one.
    """
    regressions = 0
    for profile, endpoints in report["profiles"].items():
        was = baseline.get("profiles", {}).get(profile, {})
        for name, now in endpoints.items():
            before = was.get(name)
            if not before:
                continue
            moved = [
                q
                for q in ("p50_ms", "p95_ms")
                if now[q] > before[q] * (1 + tolerance)
                and now[q] - before[q] >= floor_ms
            ]
            if len(moved) < 2:
                continue
            regressions += 1
            worst = max(
                STAGES,
                key=lambda s: now["stages_ms"].get(s, 0) - before["stages_ms"].get(s, 0),
            )
            print(
                f"REGRESSION {profile}/{name}:"
                f" p50 {before['p50_ms']:.1f} -> {now['p50_ms']:.1f} ms,"
                f" p95 {before['p95_ms']:.1f} -> {now['p95_ms']:.1f} ms."
                f" Largest stage change: {worst}"
                f" {before['stages_ms'].get(worst, 0):.1f} ->"
                f" {now['stages_ms'].get(worst, 0):.1f} ms;"
                f" statements {before['statements']} -> {now['statements']},"
                f" rows {before['rows']} -> {now['rows']}."
            )
    return regressions


def machine() -> dict:
    """Enough about this machine that a number can be read a year later."""
    model = ""
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    return {
        "cpu": model,
        "cores": os.cpu_count(),
        "platform": sys.platform,
    }


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--database", default="data/bench/market-realistic.db")
    parser.add_argument(
        "--profile", action="append", choices=sorted(PROFILES), default=None
    )
    parser.add_argument("--warm", type=int, default=15)
    parser.add_argument("--output", default="target/bench/report.json")
    parser.add_argument("--baseline", default=None)
    parser.add_argument(
        "--tolerance",
        type=float,
        default=0.20,
        help="fraction a p95 may grow before it is called a regression",
    )
    parser.add_argument(
        "--floor-ms",
        type=float,
        default=2.0,
        help="absolute growth below which a change is noise, not a regression",
    )
    args = parser.parse_args(argv)

    if not shutil.which("curl"):
        raise SystemExit("curl is required")
    database = Path(args.database)
    if not database.exists():
        raise SystemExit(
            f"no fixture at {database}."
            " Build one: python3 scripts/bench-fixture.py sanitize"
        )
    manifest = Path(f"{database}.manifest.json")

    profiles = args.profile or ["release", "release-fast"]
    logs = REPO / "target" / "bench" / "logs"
    report = {
        "machine": machine(),
        "warm_samples": args.warm,
        "database": str(database),
        "fixture": json.loads(manifest.read_text()) if manifest.exists() else None,
        "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "profiles": {},
    }
    for profile in profiles:
        print(f"\n{profile}")
        report["profiles"][profile] = run_profile(profile, database, args.warm, logs)

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(f"\n{markdown(report)}\n\nwritten to {output}")

    if args.baseline:
        baseline = Path(args.baseline)
        if not baseline.exists():
            raise SystemExit(f"no baseline at {baseline}")
        regressions = compare(
            report, json.loads(baseline.read_text()), args.tolerance, args.floor_ms
        )
        if regressions:
            print(f"\n{regressions} regression(s) against {baseline}")
            return 1
        print(f"\nno regression against {baseline}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
