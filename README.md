# ESP Web Cluster — V0

A web application whose runtime is modelled as a cluster from day one, so that
the cluster can later become actual ESP32 hardware instead of Tokio tasks.

V0 runs entirely on a development PC. There is no ESP32, no QEMU, no Docker,
no Redis, no broker. One command starts everything.

```bash
cargo run
# http://127.0.0.1:3000
```

```bash
cargo run -- --nodes 8 --port 3000
```

---

## What works today

- Server-rendered UI (Axum + Askama + HTMX) at `/`, `/cluster`, `/nodes`,
  `/jobs`, `/jobs/{id}`, `/account`, `/wow`
- Eight simulated nodes with mixed ESP32 capability profiles, registering,
  heartbeating, advertising capabilities and taking work
- Roles as a set per node, changeable at runtime without changing node identity
- Deterministic leader election, tracked separately from the gateway role
- Job submission, splitting into tasks, least-loaded scheduling across workers,
  progress and result aggregation
- Health state machine: `Healthy → Suspect → Offline` from heartbeat age
- **Failure handling**: a worker that dies mid-task has the task requeued and
  re-run from the beginning elsewhere; every attempt is recorded and shown
- Cluster event log, in the UI and in the structured logs
- **Live updates over SSE** — one stream tells the page *that* something
  changed and it refetches only the affected fragments; slow polling remains as
  a fallback
- Request metrics (count, mean latency, in-flight, peak, 4xx/5xx) alongside the
  cluster's own queue-depth and load signals
- SQLite persistence for users, sessions, jobs, tasks, failures, events, cache,
  boot configuration and **role assignments** — a role changed at runtime
  survives a restart while node identity does not change
- Registration / login / logout with Argon2id, session cookies, CSRF protection
- Raider.IO character lookup with a short-lived cache (verified against the
  live API)
- Failure-simulation controls (stop, start, pause heartbeat, inject failure,
  add delay)

Not implemented on purpose: ESP32 firmware, QEMU, ESP-NOW, Raft, distributed
storage, real autoscaling, Battle.net OAuth. See CLAUDE.md §40.

---

## Layout

```text
crates/
  cluster-core/       no_std + alloc. The portable model and the ports.
  cluster-local/      PC runtime: one Tokio task per node, one supervisor.
  app-core/           Domain types, services, and the ports they need.
  storage/            SQLite adapters for those ports.
  app-integrations/   Raider.IO adapter; Battle.net config placeholder.
  app-web/            Axum routes, view models, Askama templates, assets.
  server/             Composition root: config, tracing, wiring, serving.
firmware/             ESP32-S3 node firmware. Own toolchain, own target.
scripts/              QEMU fetch/run/test helpers.
migrations/           SQL schema.
data/                 SQLite database (gitignored).
tools/                Espressif QEMU (fetched, gitignored).
```

Dependencies point inwards. `cluster-core` depends on nothing but `serde` and
`thiserror`; `app-core` depends on `cluster-core`; the adapters depend on the
core; `server` is the only crate that knows every concrete implementation.

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

## The ESP32 constraint

The long-term target is a cluster of microcontrollers with a few hundred KB of
RAM each. Three rules follow from that, and they shape the code:

**1. `cluster-core` is `no_std` and must stay that way.** It is the crate that
will eventually be cross-compiled for xtensa/riscv32. It contains ids, roles,
capabilities, jobs, tasks, events, scheduling policy, election policy and the
persistence/cluster ports — and no I/O, no threads, no runtime, no clock.
Verified by:

```bash
./check-portable.sh
```

which does three things, each catching what the one before it cannot:

1. `--no-default-features` on the host — proves it does not **use** std
2. cross-compiles for device targets — proves it can **be built** for one
3. boots it under QEMU — proves it **runs correctly** on one

Level 1 alone is not enough, and that is not hypothetical: it passed happily
while the crate could not build for `riscv32imc` at all, because `RoundRobin`
held an `AtomicUsize` and **the ESP32-C3 has no atomic instructions**. It is
now stateless.

| Target | Chips | Atomics |
|---|---|---|
| `xtensa-esp32s3-none-elf` | **ESP32-S3 — the target** | yes (needs `espup`) |
| `riscv32imc-unknown-none-elf` | ESP32-C3, C2 | no |
| `riscv32imac-unknown-none-elf` | ESP32-C6, H2 | yes |

The S3 is what this is built for; the others are checked so the core does not
quietly acquire an S3-only dependency. `cluster-core`'s entire dependency tree
is 21 crates.

**2. No boxing on the hot paths.** Every port is declared as
`fn f(&self) -> impl Future<Output = T> + Send`, not `async fn` and not
`#[async_trait]`. That keeps calls allocation-free and the traits usable from an
embedded executor such as embassy. Implementations still write plain `async fn`.
The web layer takes one type parameter (`E: Ports`) rather than a bag of
`Box<dyn …>`.

**3. Message passing, not shared memory.** All cluster state lives inside one
supervisor task and is reached only by sending it a message. There is no mutex
in the runtime. A node knows its own id, its capabilities and its mailbox —
exactly what an ESP32 will know.

Smaller consequences: `RoleSet` is a one-byte bitmask; ids are `u16`/`u64`, not
UUIDs; timestamps are `u64` milliseconds with the date formatting written by
hand rather than pulling in `chrono`; CSS and JS are embedded in the binary so
the asset budget is visible at build time.

`Argon2` is the deliberate exception — correct on a PC, far too memory-hungry
for a node — which is why it sits behind `PasswordHasher` rather than being
called directly.

---

## Configuration

Every setting is a CLI flag backed by an environment variable, with a default.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--host` | `ESP_HOST` | `127.0.0.1` | Bind address |
| `--port` | `ESP_PORT` | `3000` | Bind port |
| `--database` | `ESP_DATABASE` | `data/cluster.db` | SQLite file, or `:memory:` |
| `--nodes` | `ESP_NODES` | `8` | Simulated nodes |
| `--heartbeat-ms` | `ESP_HEARTBEAT_MS` | `1000` | Heartbeat interval |
| `--suspect-ms` | `ESP_SUSPECT_MS` | `3000` | Silence before Suspect |
| `--offline-ms` | `ESP_OFFLINE_MS` | `6000` | Silence before Offline |
| `--max-task-attempts` | `ESP_MAX_ATTEMPTS` | `3` | Attempts before a task fails for good |
| `--poll-ms` | `ESP_POLL_MS` | `2000` | Fallback refresh interval (SSE is primary) |
| `--gateway-min` etc. | `ESP_GATEWAY_MIN` … | `1,2,2,1,1` | Role minimums |
| `--debug-controls` | `ESP_DEBUG_CONTROLS` | `true` | Mount `/debug/*` |
| `--secure-cookies` | `ESP_SECURE_COOKIES` | `false` | Requires HTTPS |
| `--log` | `ESP_LOG` | `info,sqlx=warn` | Tracing filter |

Secrets are never flags. Battle.net credentials are read from
`BATTLENET_CLIENT_ID` / `BATTLENET_CLIENT_SECRET` by the adapter that needs
them, and never logged.

---

## Watching a failure

The point of V0 is that failover is real, not narrated. To see it:

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

The same sequence is asserted in
`crates/cluster-local/tests/failover.rs::a_dead_worker_does_not_lose_its_task`.

---

## Development

```bash
cargo run                                        # start everything
cargo test --workspace                           # 56 tests
cargo fmt --all
cargo clippy --workspace --all-targets
./check-portable.sh                              # host + cross-compile + QEMU
./scripts/qemu-test.sh                           # just the device tests
```

Tests live next to what they cover:

- `crates/cluster-core/src/tests.rs` — roles, health, state machines, job
  splitting, schedulers, election, time
- `crates/cluster-local/tests/failover.rs` — the runtime end to end: worker
  death, retry exhaustion, heartbeat loss, runtime role changes, role
  persistence across a restart, leader failover, the live event stream
- `crates/storage/tests/repositories.rs` — persistence round-trips
- `crates/app-core/tests/domain.rs` — validation, hashing, submission limits
- `crates/app-core/tests/metrics.rs` — counter accounting

---

## Running on an emulated ESP32-S3

`firmware/` is a real ESP32-S3 binary. It links `cluster-core` — the same crate
the server links, not a copy — and runs the domain logic on Xtensa: role sets,
health transitions, the task state machine, job splitting, scheduling, election,
and the prime workload, asserting each result. QEMU today, silicon unchanged.

```bash
./scripts/fetch-qemu.sh     # Espressif's QEMU fork; stock QEMU has no ESP machines
./scripts/qemu-test.sh      # build for S3, boot, check the verdict
```

```text
=== esp-web-cluster node firmware ===
chip      : ESP32-S3 (xtensa lx7)
target    : xtensa-esp32s3-none-elf
heap      : 32768 bytes (bump)
caps      : xtensa x2 cores, 512 KB RAM, usable 8704 KB
...
  ok    counts primes correctly (pi(1000) = 168)
  ok    run_task computes on-device
heap peak : 505 / 32768 bytes (0 allocation failures)
checks    : 28 passed, 0 failed
RESULT: PASS
```

The whole run takes under a second, and the harness exits non-zero when a check
fails — verified by breaking one on purpose.

**What it measures that host tests cannot:**

| | |
|---|---|
| Flash footprint | 57 KB text, 2 KB data |
| Heap needed by the domain layer | **505 bytes** peak |
| `pi(1000)` on device | 168 — identical to the host |

That 505-byte figure is the number worth watching. It is what says the domain
layer could plausibly share a node with a network stack, and it will move the
moment someone adds a `String` to a hot path.

**Two things to know if you extend the firmware.** It does not call
`esp_hal::init()` — that configures clocks and PLLs QEMU does not model, and
hangs there; a node driving real peripherals would need it. And the allocator is
a bump allocator that never reclaims, which fits a binary that boots, checks and
halts, but not a node serving tasks indefinitely.

Requires `espup` for the Xtensa toolchain (it is not in upstream rustc):

```bash
cargo install espup && espup install
```

---

## Live updates

The page opens one `EventSource` against `/events/stream`. The server sends only
the event *kind*; `static/live.js` (30 lines, no library) dispatches a
`cluster-changed` event on `<body>`, and the fragments listen for it:

```html
hx-trigger="cluster-changed from:body, every 20000ms"
```

The second trigger is the fallback — if the stream is unavailable the UI still
converges, just less promptly. Only the kind crosses the wire because the thing
serving this eventually has a radio, not a datacentre NIC.

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
| Run on real nodes | implement `cluster_core::ClusterControl` beside `cluster-local` |
| Test on a device | add checks to `firmware/src/main.rs`, run `scripts/qemu-test.sh` |

Nothing in that last row requires touching `app-web` or `app-core`. That is the
whole point of V0.
