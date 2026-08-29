# WoW Auction Tracker

A server-rendered web application in Rust with a work cluster built in. Prices,
raid consumables and crafting reagents from the Battle.net auction-house API;
character lookups from Raider.IO; and a job runner that spreads heavy work over
as many worker processes as you care to start.

It runs on one ordinary server. No Redis, no broker, no Kubernetes needed.

```bash
cargo run
# http://127.0.0.1:3000
```

```bash
docker compose up --build              # web + 3 workers
docker compose up -d --scale worker=8
```

---

## Two things it is built around

**Pages are fast.** Server-rendered HTML, HTMX for partial updates, CSS and JS
embedded in the binary, no JavaScript framework. Pico supplies the semantic
defaults, while a small app stylesheet handles the cluster and market views.

**Failure is visible.** Kill a worker holding a task and the task is re-run
elsewhere, the attempt is recorded, and the event log says what happened. That
is a feature of the app, not a test fixture.

---

## What works today

- Server-rendered UI (Axum + Askama + HTMX) at `/`, `/cluster`, `/nodes`,
  `/jobs`, `/jobs/{id}`, `/account`, `/wow`
- **One binary, two roles**: the web server and the workers are the same
  executable, chosen by flags, so a worker can never be a different build
- In-process workers by default; worker *processes* on this or any other
  machine via `--connect`, which is what `--scale worker=N` drives
- Anonymous workers: a worker dials in, is given an identity, and is forgotten
  when it leaves — nothing about a replica is configured in advance
- Job submission, splitting into tasks, least-loaded scheduling, progress and
  result aggregation
- Health state machine: `Healthy → Suspect → Offline` from heartbeat age
- **Failure handling**: a worker that dies mid-task has the task requeued and
  re-run elsewhere; every attempt is recorded and shown
- Roles as a set per node, changeable at runtime; deterministic leader election
- Cluster event log, in the UI and in the structured logs
- **Live updates over SSE** — the stream says only *that* something changed and
  the page refetches the affected fragments; polling remains as a fallback
- Request metrics (count, mean latency, in-flight, peak, 4xx/5xx)
- SQLite persistence for users, sessions, jobs, tasks, failures, events, cache,
  boot configuration and role assignments
- Registration / login / logout with Argon2id, session cookies, CSRF protection
- Auction-house prices with per-patch history, raid consumables and crafting
  reagents, in twelve locales for item text and a translated interface
- Raider.IO character lookup with a short-lived cache
- Failure-simulation controls (stop, start, pause heartbeat, inject failure,
  add delay)

Not implemented on purpose: Raft, distributed storage, autoscaling, Battle.net
OAuth, and horizontal scaling of the *web* tier — see "Scaling" below.

---

## Layout

```text
crates/
  cluster-core/       Job/task model, scheduler, events, worker protocol.
  cluster-local/      Coordinator, in-process worker pool, TCP transport.
  app-core/           Domain types, services, and the ports they need.
  storage/            SQLite adapters for those ports.
  app-integrations/   Blizzard and Raider.IO clients.
  app-web/            Axum routes, view models, Askama templates, assets.
  server/             Composition root; also the worker binary.
locales/              gettext catalogues, compiled in at build time.
migrations/           SQL schema.
data/                 SQLite database (gitignored).
Dockerfile            One image for both roles.
compose.yml           Web + scalable workers on one host.
```

Dependencies point inwards. `cluster-core` depends only on `serde`,
`thiserror`, `postcard` and `futures-core`; `app-core` depends on
`cluster-core`; the adapters depend on the core; `server` is the only crate
that knows every concrete implementation.

```text
                 server  (composition root)
                    │
        ┌───────────┼────────────┬──────────────┐
        ▼           ▼            ▼              ▼
     app-web    storage    app-integrations  cluster-local
        │           │            │              │
        └────► app-core ◄────────┘              │
                    │                           │
                    └────► cluster-core ◄───────┘
```

---

## Configuration

Every setting is a CLI flag backed by an environment variable, with a default.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--host` | `APP_HOST` | `127.0.0.1` | Bind address |
| `--port` | `APP_PORT` | `3000` | Bind port |
| `--database` | `APP_DATABASE` | `data/cluster.db` | SQLite file, or `:memory:` |
| `--workers` | `APP_WORKERS` | `4` | Workers inside this process |
| `--worker-listen` | `APP_WORKER_LISTEN` | off | Accept worker connections, e.g. `0.0.0.0:3001` |
| `--connect` | `APP_CONNECT` | off | Run as a worker against this coordinator |
| *(none)* | `APP_CLUSTER_TOKEN` | — | Cluster join secret. Required with `--worker-listen`; sent by `--connect` |
| `--heartbeat-ms` | `APP_HEARTBEAT_MS` | `1000` | Heartbeat interval |
| `--suspect-ms` | `APP_SUSPECT_MS` | `3000` | Silence before Suspect |
| `--offline-ms` | `APP_OFFLINE_MS` | `6000` | Silence before Offline |
| `--max-task-attempts` | `APP_MAX_ATTEMPTS` | `3` | Attempts before a task fails for good |
| `--poll-ms` | `APP_POLL_MS` | `2000` | Fallback refresh interval (SSE is primary) |
| `--gateway-min` etc. | `APP_GATEWAY_MIN` … | `1,2,2,1,1` | Role minimums |
| `--debug-controls` | `APP_DEBUG_CONTROLS` | `false` | Mount `/debug/*` (administrator only) |
| `--secure-cookies` | `APP_SECURE_COOKIES` | `false` | Requires HTTPS |
| `--log` | `APP_LOG` | `info,sqlx=warn` | Tracing filter |
| `--market-regions` | `APP_MARKET_REGIONS` | `eu,us,kr,tw` | Auction regions to collect and offer in the picker |
| `--market-interval-min` | `APP_MARKET_INTERVAL_MIN` | `30` | Commodity poll interval |
| `--market-retain-days` | `APP_MARKET_RETAIN_DAYS` | `90` | Price history kept |

Each collected region costs one commodities call per cycle (25 against
Battle.net's hourly budget of 36,000) and its own price rows. Narrow the list
if you only care about one market -- the region picker then offers only what
is collected.

Language is a separate axis from region, and site-wide -- it is picked in the
top bar, not on the market page. A visitor's language comes from `?lang=`, then
the `wow_tracker_market` cookie, then `Accept-Language`, then English.

It moves two different kinds of text:

* **Item names, effects and tooltips** come from Battle.net in all twelve
  locales. Every region's static endpoint returns all of them, so this needs no
  translator and no catalogue.
* **The interface itself** is translated from gettext PO files in `locales/`,
  compiled into static tables at build time. English and Spanish exist today;
  the language menu marks the rest as *item text only* rather than pretending.
  See [locales/README.md](locales/README.md) for the translator workflow --
  the layout is the standard one Weblate, Crowdin and Transifex all consume.

Secrets are never flags, and never logged:

* `BLIZZARD_CLIENT_ID` / `BLIZZARD_CLIENT_SECRET` -- the Game Data API client,
  used for auction prices and item tooltips. Without them the app still runs;
  it just collects no prices and shows untranslated catalog names.
* `BATTLENET_CLIENT_ID` / `BATTLENET_CLIENT_SECRET` -- the OAuth client, for
  account linking. Not wired up yet.

---

## Watching a failure

Failover is real, not narrated. To see it:

1. `cargo run` and open <http://127.0.0.1:3000/jobs>
2. Submit a `sleep` job of `30000` ms across `4` tasks
3. Open `/cluster`, find a node with a running task, press **stop**
4. Watch the event log:

```text
task-03 assigned to node-05
node-05 left
task-03 failed on node-05 (node_offline)
task-03 requeued
task-03 assigned to node-02
task-03 completed on node-02
job-01 completed
```

5. `/jobs/1` shows the failure row, and attempt `2` on the task that moved

The same sequence is asserted twice: in
`crates/cluster-local/tests/failover.rs::a_dead_worker_does_not_lose_its_task`
for in-process workers, and in
`crates/cluster-local/tests/remote.rs::a_worker_that_vanishes_mid_task_loses_no_work`
for a worker on the far side of a socket that gets yanked -- which is how a
killed process actually fails.

Or do it for real: start a coordinator and two workers, submit a job, and
`kill` one of the worker processes.

---

## Deployment

One image, two roles. The web server and the workers are the same executable
started with different flags, so a worker can never be a different build from
the thing that was tested.

```bash
docker compose up --build              # web + 3 workers
docker compose up -d --scale worker=8  # more workers, no config change
```

Or without Docker, on one host:

```bash
export APP_CLUSTER_TOKEN=$(openssl rand -hex 32)                 # same on both
server --host 0.0.0.0 --worker-listen 0.0.0.0:3001 --workers 0   # coordinator
server --connect coordinator:3001                                # each worker
```

`APP_CLUSTER_TOKEN` is what a worker presents to join, and the coordinator
**refuses to start** with `--worker-listen` and no token: without one, anything
that can open a socket to that port becomes a node of the cluster, takes work
and reports whatever result it likes. It is an environment variable, never a
flag, so it stays out of `ps` and out of shell history.

The token crosses the wire as it is, so the worker port belongs on a private
network or inside a tunnel — the same boundary as everything else here, where
TLS terminates at the reverse proxy. Do not publish 3001.

Put a reverse proxy in front for TLS. Workers need no ports, no volume and no
database: everything they need arrives over the connection they open.

### Scaling

Do not hand-roll what the deployment already provides:

| Concern | Who owns it |
|---|---|
| HTTP load balancing, TLS | reverse proxy, or a k8s Service + Ingress |
| Restarts, supervision | compose / systemd / k8s |
| Worker count | `--scale`, or a Deployment's replicas |
| **Job splitting, placement, retry, aggregation** | **this codebase** |

Workers scale horizontally today. **The web tier does not yet**, and two things
block it — both fine on one server, both mandatory before running two:

1. **SQLite** is single-writer on one filesystem. A second web replica needs
   Postgres. The storage ports make that an adapter swap, not a rewrite.
2. **The SSE bus is an in-process `tokio::broadcast`.** With two web processes,
   an event on one never reaches a browser connected to the other. Needs
   Postgres `LISTEN/NOTIFY`, Redis pub/sub, or sticky sessions.

---

## Development

```bash
cargo run                                        # start everything
cargo test --workspace                           # 168 tests
cargo fmt --all
cargo clippy --workspace --all-targets
```

Tests live next to what they cover:

- `crates/cluster-core/src/tests.rs` — roles, health, state machines, job
  splitting, schedulers, election, time
- `crates/cluster-local/tests/failover.rs` — the runtime end to end: worker
  death, retry exhaustion, heartbeat loss, runtime role changes, role
  persistence across a restart, leader failover, the live event stream
- `crates/cluster-local/tests/remote.rs` — worker *processes* over a real
  socket: anonymous registration, identity reuse, a yanked connection, and a
  restarted worker reclaiming its identity from a half-open one
- `crates/storage/tests/repositories.rs` — persistence round-trips
- `crates/app-core/tests/domain.rs` — validation, hashing, submission limits
- `crates/app-core/tests/metrics.rs` — counter accounting

---

## Live updates

The page opens one `EventSource` against `/events/stream`. The server sends only
the event *kind*; `static/live.js` (30 lines, no library) dispatches a
`cluster-changed` event on `<body>`, and the fragments listen for it:

```html
hx-trigger="cluster-changed from:body, every 20000ms"
```

The second trigger is the fallback — if the stream is unavailable the UI still
converges, just less promptly. Only the kind crosses the wire, so a busy
cluster costs one tiny frame per change rather than a rendered page per client.

Note this bus is in-process: it is why the web tier does not scale to two
replicas yet. See "Scaling".

`/api/cluster` and `/api/metrics` return the same state as JSON for scripts.

---

## Where to add things

| You want to… | Touch |
|---|---|
| Add a workload | `cluster-core/src/job.rs` (`JobSpec`/`TaskSpec`), `cluster-local/src/exec.rs` |
| Change placement | implement `cluster_core::Scheduler`, pass it to `LocalCluster::start_with` |
| Change election | implement `cluster_core::Elector`, same place |
| Add a page | `app-web/src/views.rs`, `templates/`, `app-web/src/routes/` |
| Add a provider | implement `app_core::wow::CharacterProvider` in `app-integrations` |
| Change persistence | implement the `app_core::repo` / `cluster_core::persist` ports |
| Change the worker protocol | `cluster-core/src/protocol.rs`, then both sides follow |
| Change worker behaviour | `cluster-core/src/agent.rs` — one implementation, host-tested |
| Add another transport | a peer of `cluster-local/src/remote.rs`; the coordinator does not change |
| Change what a worker process does | `crates/server/src/worker.rs` |

Nothing in the transport rows requires touching `app-web` or `app-core`. That
separation is the point: the coordinator reaches an in-process worker and a
worker on another machine the same way — by pushing a message into a channel.
