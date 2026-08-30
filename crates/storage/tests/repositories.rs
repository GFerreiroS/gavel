//! Storage adapter tests.
//!
//! These assert the round-trip: everything written through a port comes back
//! through the port as the same domain value. They deliberately never touch a
//! SQL string, because callers never do either.

use app_core::market::analysis::{Cycle, Point, Trend};
use app_core::market::catalog::CatalogStatus;
use app_core::market::event::{EventKind, EventScope, Provenance, Validation, Visibility};
use app_core::market::materialise::{MarketState, MarketWindow, Materialised};
use app_core::market::window::Window;
use app_core::market::{MarketEvent, MarketKey};
use app_core::model::Session;
use app_core::repo::{
    CacheStore, EventRepository, JobRepository, KeyValueStore, MarketEventRepository,
    ReadModelRepository, ReleaseRepository, SessionRepository, Store, UserRepository, VersionState,
};
use cluster_core::{
    ClusterEvent, ClusterStore, EventRecord, FailureReason, JobSpec, JobState, Millis, NodeId,
    Role, RoleSet, TaskAttempt, TaskState,
};
use storage::{SqliteConfig, SqliteStore};

async fn store() -> SqliteStore {
    SqliteStore::connect(&SqliteConfig::in_memory())
        .await
        .expect("in-memory database")
}

#[tokio::test]
async fn migrations_run_on_a_fresh_database() {
    let store = store().await;
    // If the schema is missing, any of these would error rather than return
    // an empty result.
    assert!(store.jobs().recent_jobs(10).await.unwrap().is_empty());
    assert!(store.events().recent(10).await.unwrap().is_empty());
    assert_eq!(store.events().last_seq().await.unwrap(), 0);
}

#[tokio::test]
async fn users_round_trip_and_usernames_are_unique() {
    let store = store().await;
    let users = store.users();

    let created = users
        .create("Tester", "hash-goes-here", Millis(1_000))
        .await
        .unwrap();
    assert_eq!(created.username, "Tester");
    assert_eq!(created.created_at, Millis(1_000));

    let found = users.by_username("Tester").await.unwrap().unwrap();
    assert_eq!(found.user.id, created.id);
    assert_eq!(found.password_hash, "hash-goes-here");

    // The column is COLLATE NOCASE, so lookup and uniqueness ignore case.
    assert!(users.by_username("TESTER").await.unwrap().is_some());
    assert!(
        users
            .create("TESTER", "other", Millis(2_000))
            .await
            .is_err(),
        "duplicate usernames are rejected by the database, not just the service"
    );

    assert!(users.by_username("nobody").await.unwrap().is_none());
    assert_eq!(
        users.by_id(created.id).await.unwrap().unwrap().username,
        "Tester"
    );
    assert!(users.linked_accounts(created.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn sessions_round_trip_and_expire() {
    let store = store().await;
    let user = store
        .users()
        .create("sessions", "hash", Millis(0))
        .await
        .unwrap();

    let live = Session {
        id: "live-token".into(),
        user_id: user.id,
        created_at: Millis(0),
        expires_at: Millis(10_000),
    };
    let stale = Session {
        id: "stale-token".into(),
        user_id: user.id,
        created_at: Millis(0),
        expires_at: Millis(500),
    };
    store.sessions().create(&live).await.unwrap();
    store.sessions().create(&stale).await.unwrap();

    assert_eq!(
        store.sessions().get("live-token").await.unwrap().unwrap(),
        live
    );

    assert_eq!(
        store.sessions().purge_expired(Millis(1_000)).await.unwrap(),
        1
    );
    assert!(store.sessions().get("stale-token").await.unwrap().is_none());
    assert!(store.sessions().get("live-token").await.unwrap().is_some());

    store.sessions().delete("live-token").await.unwrap();
    assert!(store.sessions().get("live-token").await.unwrap().is_none());
}

#[tokio::test]
async fn jobs_tasks_and_failures_survive_a_round_trip() {
    let store = store().await;
    let jobs = store.jobs();

    let spec = JobSpec::Primes {
        upper_bound: 1_000,
        tasks: 4,
    };
    let (job, tasks) = jobs.allocate(spec, Millis(5_000)).await.unwrap();
    assert_eq!(tasks.len(), 4);
    jobs.create_job(&job, &tasks).await.unwrap();

    let loaded = jobs.job(job.id).await.unwrap().unwrap();
    assert_eq!(loaded, job, "the job comes back exactly as it went in");
    assert_eq!(jobs.tasks_for_job(job.id).await.unwrap(), tasks);

    // Ids keep advancing across jobs.
    let (second, _) = jobs.allocate(spec, Millis(6_000)).await.unwrap();
    assert!(second.id.get() > job.id.get());

    // Mutate and save.
    let mut job = job;
    job.transition_to(JobState::Running, Millis(5_100)).unwrap();
    job.tasks_completed = 2;
    jobs.save_job(&job).await.unwrap();
    assert_eq!(
        jobs.job(job.id).await.unwrap().unwrap().state,
        JobState::Running
    );

    let mut task = tasks[0].clone();
    task.assign(NodeId(7), Millis(5_200)).unwrap();
    task.start(Millis(5_300)).unwrap();
    task.complete("168 primes".into(), Millis(5_400)).unwrap();
    jobs.save_task(&task).await.unwrap();

    let reloaded = jobs.tasks_for_job(job.id).await.unwrap();
    assert_eq!(reloaded[0].state, TaskState::Completed);
    assert_eq!(reloaded[0].assigned_to, Some(NodeId(7)));
    assert_eq!(reloaded[0].attempt, 1);
    assert_eq!(reloaded[0].output.as_deref(), Some("168 primes"));

    // Failures accumulate; nothing is overwritten.
    for attempt in 1..=2u16 {
        let mut failed = tasks[1].clone();
        failed.assign(NodeId(attempt), Millis(6_000)).unwrap();
        jobs.record_failure(&TaskAttempt::new(
            &failed,
            FailureReason::NodeOffline,
            "node went offline",
            Millis(6_000 + u64::from(attempt)),
        ))
        .await
        .unwrap();
    }
    let failures = jobs.failures_for_job(job.id).await.unwrap();
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].reason, FailureReason::NodeOffline);
    assert_eq!(failures[0].detail, "node went offline");
}

#[tokio::test]
async fn unfinished_jobs_are_recoverable_after_a_restart() {
    let config = SqliteConfig::in_memory();
    let store = SqliteStore::connect(&config).await.unwrap();

    let (running, tasks) = store
        .jobs()
        .allocate(
            JobSpec::Sleep {
                total_ms: 100,
                tasks: 2,
            },
            Millis(0),
        )
        .await
        .unwrap();
    store.jobs().create_job(&running, &tasks).await.unwrap();

    let (done, done_tasks) = store
        .jobs()
        .allocate(
            JobSpec::Sleep {
                total_ms: 100,
                tasks: 1,
            },
            Millis(0),
        )
        .await
        .unwrap();
    store.jobs().create_job(&done, &done_tasks).await.unwrap();
    let mut done = done;
    done.transition_to(JobState::Running, Millis(1)).unwrap();
    done.transition_to(JobState::Completed, Millis(2)).unwrap();
    store.jobs().save_job(&done).await.unwrap();

    // A second handle to the same database is what a restart looks like.
    let reopened = SqliteStore::connect(&config).await.unwrap();
    let unfinished = reopened.jobs().unfinished_jobs().await.unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].0.id, running.id);
    assert_eq!(unfinished[0].1.len(), 2);
}

#[tokio::test]
async fn events_are_append_only_and_read_back_newest_first() {
    let store = store().await;
    let events = store.events();

    for seq in 1..=3u64 {
        events
            .append(&EventRecord::new(
                seq,
                Millis(seq * 100),
                ClusterEvent::NodeJoined {
                    node: NodeId(seq as u16),
                },
            ))
            .await
            .unwrap();
    }
    // Replaying an already-stored event is a no-op, not an error.
    events
        .append(&EventRecord::new(
            2,
            Millis(200),
            ClusterEvent::NodeJoined { node: NodeId(2) },
        ))
        .await
        .unwrap();

    let recent = events.recent(10).await.unwrap();
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].seq, 3, "newest first");
    assert_eq!(
        recent[0].event,
        ClusterEvent::NodeJoined { node: NodeId(3) }
    );
    assert_eq!(events.last_seq().await.unwrap(), 3);
    assert_eq!(events.recent(2).await.unwrap().len(), 2);
}

#[tokio::test]
async fn the_cache_honours_expiry() {
    let store = store().await;
    let cache = store.cache();

    cache.put("k", b"value", Millis(1_000)).await.unwrap();
    assert_eq!(
        cache.get("k", Millis(500)).await.unwrap().unwrap(),
        b"value"
    );
    assert!(
        cache.get("k", Millis(1_000)).await.unwrap().is_none(),
        "expiry is exclusive of the deadline"
    );

    // Re-putting extends the deadline.
    cache.put("k", b"fresher", Millis(5_000)).await.unwrap();
    assert_eq!(
        cache.get("k", Millis(2_000)).await.unwrap().unwrap(),
        b"fresher"
    );

    assert_eq!(cache.purge_expired(Millis(9_000)).await.unwrap(), 1);
    assert!(cache.get("k", Millis(1)).await.unwrap().is_none());
}

#[tokio::test]
async fn the_key_value_store_is_a_plain_map() {
    let store = store().await;
    let kv = store.kv();

    assert!(kv.get("absent").await.unwrap().is_none());
    kv.put("cluster/config", b"first").await.unwrap();
    assert_eq!(kv.get("cluster/config").await.unwrap().unwrap(), b"first");
    kv.put("cluster/config", b"second").await.unwrap();
    assert_eq!(kv.get("cluster/config").await.unwrap().unwrap(), b"second");
    kv.delete("cluster/config").await.unwrap();
    assert!(kv.get("cluster/config").await.unwrap().is_none());
    // Deleting something absent is not an error.
    kv.delete("cluster/config").await.unwrap();
}

#[tokio::test]
async fn node_roles_persist_and_are_replaced_in_place() {
    let config = SqliteConfig::in_memory();
    let store = SqliteStore::connect(&config).await.unwrap();
    let cluster = store.cluster_handle();

    assert!(cluster.load_node_roles().await.unwrap().is_empty());

    let initial = RoleSet::from_roles([Role::Compute, Role::Frontend]);
    cluster
        .save_node_roles(NodeId(3), initial, Millis(1_000))
        .await
        .unwrap();
    cluster
        .save_node_roles(
            NodeId(1),
            RoleSet::from_roles([Role::Gateway]),
            Millis(1_000),
        )
        .await
        .unwrap();

    let stored = cluster.load_node_roles().await.unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].0, NodeId(1), "ordered by node id");
    assert_eq!(stored[1], (NodeId(3), initial));

    // A second write for the same node updates rather than duplicating.
    let changed = RoleSet::from_roles([Role::Compute, Role::Frontend, Role::Storage]);
    cluster
        .save_node_roles(NodeId(3), changed, Millis(2_000))
        .await
        .unwrap();
    let stored = cluster.load_node_roles().await.unwrap();
    assert_eq!(stored.len(), 2, "updated in place");
    assert_eq!(stored[1], (NodeId(3), changed));

    // And it is visible to a fresh handle on the same database.
    let reopened = SqliteStore::connect(&config).await.unwrap();
    assert_eq!(
        reopened.cluster_handle().load_node_roles().await.unwrap(),
        stored
    );
}

#[tokio::test]
async fn the_cluster_handle_also_serves_jobs_and_events() {
    // It delegates; this guards against the delegation being wired to the
    // wrong repository.
    use cluster_core::{EventLog, JobStore};

    let store = store().await;
    let cluster = store.cluster_handle();

    let (job, tasks) = cluster
        .allocate(
            JobSpec::Sleep {
                total_ms: 10,
                tasks: 1,
            },
            Millis(0),
        )
        .await
        .unwrap();
    cluster.create_job(&job, &tasks).await.unwrap();
    assert_eq!(store.jobs().job(job.id).await.unwrap().unwrap(), job);

    cluster
        .append(&EventRecord::new(
            1,
            Millis(0),
            ClusterEvent::NodeJoined { node: NodeId(1) },
        ))
        .await
        .unwrap();
    assert_eq!(store.events().last_seq().await.unwrap(), 1);
}

#[tokio::test]
async fn price_history_round_trips_and_deduplicates() {
    use app_core::market::{Copper, ItemId, PriceSample, Region};
    use app_core::repo::PriceRepository;

    let store = store().await;
    let prices = store.prices();
    let item = ItemId(241324);

    let sample = |at: u64, p05: u64| PriceSample {
        item,
        region: Region::Eu,
        observed_at: Millis(at),
        min_unit_price: Copper(p05 - 10),
        p05_unit_price: Copper(p05),
        median_unit_price: Copper(p05 + 50),
        quantity: 500,
        listings: 12,
    };

    assert_eq!(
        prices.record_samples(&[sample(1_000, 900)]).await.unwrap(),
        1
    );
    // The same snapshot arriving twice must not double-count a baseline.
    assert_eq!(
        prices.record_samples(&[sample(1_000, 900)]).await.unwrap(),
        0
    );
    assert_eq!(
        prices.record_samples(&[sample(2_000, 800)]).await.unwrap(),
        1
    );

    assert_eq!(
        prices.last_observed(Region::Eu).await.unwrap(),
        Some(Millis(2_000))
    );
    // A different region is a completely separate market.
    assert_eq!(prices.last_observed(Region::Us).await.unwrap(), None);

    let history = prices.history(item, Region::Eu, Millis(0)).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].p05_unit_price, Copper(900));

    let latest = prices.latest(Region::Eu).await.unwrap();
    assert_eq!(latest.len(), 1, "one row per item: the newest");
    assert_eq!(latest[0].observed_at, Millis(2_000));

    let window = prices
        .window_stats(Region::Eu, Millis(0), None)
        .await
        .unwrap();
    assert_eq!(window.len(), 1);
    assert_eq!(window[0].low, Copper(800));
    assert_eq!(window[0].low_at, Millis(2_000), "and when it was cheapest");
    assert_eq!(window[0].high, Copper(900));
    assert_eq!(window[0].high_at, Millis(1_000));
    assert_eq!(window[0].mean, Copper(850));
    assert_eq!(window[0].samples, 2);

    assert_eq!(prices.prune_before(Millis(1_500)).await.unwrap(), 1);
    assert_eq!(
        prices
            .history(item, Region::Eu, Millis(0))
            .await
            .unwrap()
            .len(),
        1
    );
}

/// A day of snapshots becomes one row, and the archive keeps saying what the
/// day was worth. This is what lets retention stay at "keep forever" now the
/// catalogue is hundreds of items rather than twenty-six.
#[tokio::test]
async fn old_history_is_downsampled_to_one_row_a_day() {
    use app_core::market::{Copper, ItemId, PriceSample, Region};
    use app_core::repo::PriceRepository;

    const DAY: u64 = 86_400_000;
    let store = store().await;
    let prices = store.prices();
    let item = ItemId(212283);

    let sample = |at: u64, p05: u64, quantity: u64| PriceSample {
        item,
        region: Region::Eu,
        observed_at: Millis(at),
        min_unit_price: Copper(p05 - 10),
        p05_unit_price: Copper(p05),
        median_unit_price: Copper(p05 + 50),
        quantity,
        listings: 12,
    };

    // Three snapshots on day 10, one on day 11.
    prices
        .record_samples(&[
            sample(10 * DAY, 900, 400),
            sample(10 * DAY + 3_600_000, 700, 600),
            sample(10 * DAY + 7_200_000, 800, 500),
            sample(11 * DAY + 3_600_000, 1_000, 100),
        ])
        .await
        .unwrap();

    // Collapse everything before day 11.
    assert_eq!(
        prices.downsample_before(Millis(11 * DAY)).await.unwrap(),
        2,
        "two of day 10's three rows are folded away"
    );

    let history = prices.history(item, Region::Eu, Millis(0)).await.unwrap();
    assert_eq!(history.len(), 2, "one row for day 10, and day 11 untouched");

    let day = &history[0];
    assert_eq!(day.observed_at, Millis(10 * DAY), "sits on midnight");
    assert_eq!(
        day.min_unit_price,
        Copper(690),
        "the day's cheapest survives as a true minimum"
    );
    assert_eq!(day.p05_unit_price, Copper(800), "the day's average price");
    assert_eq!(day.quantity, 500, "the day's average depth");
    assert_eq!(history[1].observed_at, Millis(11 * DAY + 3_600_000));

    // Running it again finds nothing left to do.
    assert_eq!(prices.downsample_before(Millis(11 * DAY)).await.unwrap(), 0);
}

#[tokio::test]
async fn alerts_round_trip_and_support_the_cooldown() {
    use app_core::market::{Alert, AlertSeverity, Copper, ItemId, Region};
    use app_core::repo::PriceRepository;

    let store = store().await;
    let prices = store.prices();
    let item = ItemId(241321);

    assert_eq!(prices.last_alert_at(item, Region::Eu).await.unwrap(), None);

    let alert = Alert {
        item,
        region: Region::Eu,
        severity: AlertSeverity::VeryLow,
        observed_at: Millis(5_000),
        current: Copper(400),
        baseline: Copper(1_000),
        threshold: Copper(600),
        discount_percent: 60,
        quantity: 900,
    };
    prices.record_alert(&alert).await.unwrap();

    assert_eq!(
        prices.last_alert_at(item, Region::Eu).await.unwrap(),
        Some(Millis(5_000))
    );
    assert_eq!(prices.last_alert_at(item, Region::Us).await.unwrap(), None);

    let recent = prices.recent_alerts(10).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0], alert, "the alert survives the round trip intact");
}

/// An in-memory database must outlive the recycling of its pool.
///
/// A `mode=memory` SQLite database exists only while some connection to it is
/// open. sqlx recycles connections at `max_lifetime` -- 30 minutes by default
/// -- and when the last one went, the whole schema went with it: the next
/// query failed with "no such table", half an hour into a run that had
/// reported a clean start.
///
/// The lifetime is squeezed to milliseconds here so the failure takes a second
/// rather than half an hour.
#[tokio::test]
async fn an_in_memory_database_survives_connection_recycling() {
    let mut config = SqliteConfig::in_memory();
    config.max_lifetime_ms = Some(50);
    let store = SqliteStore::connect(&config).await.expect("connect");

    store
        .kv()
        .put("canary", b"present")
        .await
        .expect("write before recycling");

    // Comfortably longer than the lifetime above, so every connection the pool
    // started with has been retired and replaced at least once.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    assert_eq!(
        store
            .kv()
            .get("canary")
            .await
            .expect("read after recycling"),
        Some(b"present".to_vec()),
        "the schema and its rows outlive the connections that created them"
    );
}

// --- the "latest per market" query ------------------------------------------
//
// `latest` and `latest_in_region` are the two queries a price page waits on,
// and they are written in a SQLite-specific form -- `MAX(observed_at)` with
// the other columns bare -- because the portable `ROW_NUMBER() OVER (PARTITION
// BY ...)` was four and a half times slower on the real archive.
//
// That form is only correct because SQLite promises the bare columns come from
// the row that produced the max. This holds the fast query against the
// portable one on data shaped like the real thing: several observations per
// market, arriving out of order, across realms and variants and regions.

use app_core::market::{Copper, ItemId, RealmId, RealmSample, Region};
use app_core::repo::RealmPriceRepository;
use sqlx::Row;

fn realm_sample(
    item: u32,
    region: Region,
    realm: u32,
    variant: &str,
    at: u64,
    min: u64,
) -> RealmSample {
    RealmSample {
        item: ItemId(item),
        region,
        realm: RealmId(realm),
        variant: variant.to_string(),
        observed_at: Millis(at),
        min_price: Copper(min),
        median_price: Copper(min * 2),
        max_price: Copper(min * 3),
        listings: 7,
    }
}

/// What the query used to say, spelled portably, straight against the pool.
async fn window_function_latest(
    store: &SqliteStore,
    region: Region,
    realm: Option<RealmId>,
) -> Vec<(u32, u32, String, u64, u64)> {
    let mut sql = String::from(
        "SELECT item_id, realm_id, variant, observed_at, min_price FROM (
           SELECT *, ROW_NUMBER() OVER (
                       PARTITION BY item_id, realm_id, variant
                       ORDER BY observed_at DESC) AS rn
             FROM realm_price_samples WHERE region = ?",
    );
    if realm.is_some() {
        sql.push_str(" AND realm_id = ?");
    }
    sql.push_str(") WHERE rn = 1");

    let mut query = sqlx::query(&sql).bind(region.as_str());
    if let Some(realm) = realm {
        query = query.bind(realm.get() as i64);
    }
    let rows = query.fetch_all(store.pool()).await.expect("window query");

    let mut out: Vec<(u32, u32, String, u64, u64)> = rows
        .iter()
        .map(|r| {
            (
                r.get::<i64, _>("item_id") as u32,
                r.get::<i64, _>("realm_id") as u32,
                r.get::<String, _>("variant"),
                r.get::<i64, _>("observed_at") as u64,
                r.get::<i64, _>("min_price") as u64,
            )
        })
        .collect();
    out.sort();
    out
}

fn as_tuples(samples: &[RealmSample]) -> Vec<(u32, u32, String, u64, u64)> {
    let mut out: Vec<(u32, u32, String, u64, u64)> = samples
        .iter()
        .map(|s| {
            (
                s.item.get(),
                s.realm.get(),
                s.variant.clone(),
                s.observed_at.get(),
                s.min_price.get(),
            )
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn latest_matches_the_window_function() {
    let store = store().await;

    // Deliberately out of insertion order, so "the newest" cannot be "the last
    // one written". Two variants and two realms per item, two regions, and one
    // market observed only once.
    let mut samples = Vec::new();
    for item in [100u32, 200] {
        for realm in [1u32, 2] {
            for variant in ["", "13332"] {
                for (at, min) in [(300u64, 50u64), (100, 90), (200, 70)] {
                    samples.push(realm_sample(item, Region::Eu, realm, variant, at, min));
                }
            }
        }
    }
    samples.push(realm_sample(300, Region::Eu, 1, "", 42, 999));
    samples.push(realm_sample(100, Region::Us, 1, "", 500, 11));

    let written = store
        .realm_prices()
        .record_samples(&samples)
        .await
        .expect("record");
    assert_eq!(written as usize, samples.len());

    for region in [Region::Eu, Region::Us] {
        assert_eq!(
            as_tuples(&store.realm_prices().latest_in_region(region).await.unwrap()),
            window_function_latest(&store, region, None).await,
            "latest_in_region disagrees with the window function for {region}"
        );
    }

    for realm in [RealmId(1), RealmId(2)] {
        assert_eq!(
            as_tuples(
                &store
                    .realm_prices()
                    .latest(Region::Eu, realm)
                    .await
                    .unwrap()
            ),
            window_function_latest(&store, Region::Eu, Some(realm)).await,
            "latest disagrees with the window function for realm {realm:?}"
        );
    }
}

/// The property the whole formulation rests on: the row that comes back is the
/// one that produced the max, not some other row from the group.
#[tokio::test]
async fn latest_returns_the_newest_rows_own_prices() {
    let store = store().await;
    store
        .realm_prices()
        .record_samples(&[
            realm_sample(1, Region::Eu, 1, "", 100, 900),
            realm_sample(1, Region::Eu, 1, "", 300, 100),
            realm_sample(1, Region::Eu, 1, "", 200, 500),
        ])
        .await
        .expect("record");

    let latest = store
        .realm_prices()
        .latest_in_region(Region::Eu)
        .await
        .unwrap();
    assert_eq!(latest.len(), 1, "one market, one row");
    assert_eq!(latest[0].observed_at, Millis(300));
    assert_eq!(
        latest[0].min_price,
        Copper(100),
        "the newest row's own price"
    );
    assert_eq!(
        latest[0].max_price,
        Copper(300),
        "and its own other columns"
    );
}

/// A variant is part of what makes a market: two upgrade tracks of one item on
/// one realm are two prices, not one.
#[tokio::test]
async fn a_variant_is_its_own_market() {
    let store = store().await;
    store
        .realm_prices()
        .record_samples(&[
            realm_sample(1, Region::Eu, 1, "13332", 100, 10),
            realm_sample(1, Region::Eu, 1, "13334", 100, 90),
        ])
        .await
        .expect("record");

    let latest = store
        .realm_prices()
        .latest_in_region(Region::Eu)
        .await
        .unwrap();
    assert_eq!(latest.len(), 2);
}

// --- watchlists -------------------------------------------------------------

use app_core::repo::WatchRepository;

async fn a_user(store: &SqliteStore, name: &str) -> app_core::model::UserId {
    store
        .users()
        .create(name, "hash", Millis(0))
        .await
        .expect("create user")
        .id
}

#[tokio::test]
async fn a_watchlist_is_one_persons_and_nobody_elses() {
    let store = store().await;
    let alice = a_user(&store, "alice").await;
    let bob = a_user(&store, "bob").await;

    let watches = store.watches();
    watches
        .watch(alice, ItemId(1), Region::Eu, Millis(10))
        .await
        .unwrap();
    watches
        .watch(bob, ItemId(2), Region::Eu, Millis(20))
        .await
        .unwrap();

    let hers: Vec<u32> = watches
        .watches(alice)
        .await
        .unwrap()
        .iter()
        .map(|w| w.item.get())
        .collect();
    let his: Vec<u32> = watches
        .watches(bob)
        .await
        .unwrap()
        .iter()
        .map(|w| w.item.get())
        .collect();

    assert_eq!(hers, vec![1]);
    assert_eq!(his, vec![2], "one person's list is not another's");
}

/// EU and US are separate markets. Following an item on one says nothing
/// about the other, and a price on one is not something a player on the other
/// can act on.
#[tokio::test]
async fn a_region_is_part_of_what_is_followed() {
    let store = store().await;
    let alice = a_user(&store, "alice").await;
    store
        .watches()
        .watch(alice, ItemId(1), Region::Eu, Millis(10))
        .await
        .unwrap();

    let watched: Vec<(u32, Region)> = store
        .watches()
        .watches(alice)
        .await
        .unwrap()
        .iter()
        .map(|w| (w.item.get(), w.region))
        .collect();
    assert_eq!(watched, vec![(1, Region::Eu)]);

    store
        .watches()
        .watch(alice, ItemId(1), Region::Us, Millis(20))
        .await
        .unwrap();
    assert_eq!(store.watches().watches(alice).await.unwrap().len(), 2);
}

/// The control is a toggle, and a double-click is not a fault.
#[tokio::test]
async fn following_and_unfollowing_are_both_idempotent() {
    let store = store().await;
    let alice = a_user(&store, "alice").await;
    let watches = store.watches();

    for at in [10, 20, 30] {
        watches
            .watch(alice, ItemId(1), Region::Eu, Millis(at))
            .await
            .unwrap();
    }
    let held = watches.watches(alice).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(
        held[0].added_at,
        Millis(10),
        "clicking again must not reshuffle the list"
    );

    for _ in 0..3 {
        watches.unwatch(alice, ItemId(1), Region::Eu).await.unwrap();
    }
    assert!(watches.watches(alice).await.unwrap().is_empty());
    // And unfollowing something never followed is not an error either.
    watches
        .unwatch(alice, ItemId(99), Region::Eu)
        .await
        .unwrap();
}

/// A watchlist must not outlive the account it belongs to.
#[tokio::test]
async fn deleting_a_user_takes_their_watchlist_with_it() {
    let store = store().await;
    let alice = a_user(&store, "alice").await;
    store
        .watches()
        .watch(alice, ItemId(1), Region::Eu, Millis(10))
        .await
        .unwrap();

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(alice)
        .execute(store.pool())
        .await
        .expect("delete user");

    assert!(
        store.watches().watches(alice).await.unwrap().is_empty(),
        "the foreign key must cascade"
    );
}

/// The page asks for one day. Older alerts stay in the table -- they are the
/// history's account of itself -- but must not reach the reader.
#[tokio::test]
async fn alerts_since_is_a_window_not_the_whole_table() {
    use app_core::market::{Alert, AlertSeverity};
    use app_core::repo::PriceRepository;

    let store = store().await;
    let alert = |at: u64| Alert {
        item: ItemId(1),
        region: Region::Eu,
        severity: AlertSeverity::Low,
        observed_at: Millis(at),
        current: Copper(10),
        baseline: Copper(100),
        threshold: Copper(50),
        discount_percent: 90,
        quantity: 500,
    };
    let day = 24 * 60 * 60 * 1000u64;
    for at in [0, day, 2 * day] {
        store
            .prices()
            .record_alert(&alert(at))
            .await
            .expect("record alert");
    }

    let since = Millis(2 * day - 1);
    let today = store.prices().alerts_since(since, 100).await.unwrap();
    assert_eq!(today.len(), 1, "only the one inside the window");
    assert_eq!(today[0].observed_at, Millis(2 * day));

    // And the limit is a limit.
    assert_eq!(
        store
            .prices()
            .alerts_since(Millis(0), 2)
            .await
            .unwrap()
            .len(),
        2
    );
}

// --- catalogue releases ------------------------------------------------------

fn draft(id: &str) -> (String, CatalogStatus) {
    (id.to_string(), CatalogStatus::DraftPtr)
}

/// §8: "Activating the new tier and archiving the old one is one transaction,
/// so there is never zero or two unintentionally active BoE tiers."
#[tokio::test]
async fn activating_a_tier_archives_the_one_it_replaces() {
    let store = store().await;
    let releases = store.releases();
    let now = Millis(1_767_225_600_000);

    let seeded = releases
        .seed(
            &[("old".to_string(), CatalogStatus::Active), draft("new")],
            now,
        )
        .await
        .expect("seed");
    assert_eq!(seeded, 2);

    let later = Millis(now.get() + 1000);
    let done = releases.activate("new", later).await.expect("activate");
    assert_eq!(done.activated, "new");
    assert_eq!(
        done.archived.as_deref(),
        Some("old"),
        "the tier it replaced is archived by the same call"
    );

    let states = releases.releases().await.unwrap();
    let active: Vec<&str> = states
        .iter()
        .filter(|r| r.state.is_active())
        .map(|r| r.catalog.as_str())
        .collect();
    assert_eq!(active, ["new"], "exactly one active, never two");

    let old = states.iter().find(|r| r.catalog == "old").unwrap();
    assert_eq!(old.state, CatalogStatus::Archived);
    assert_eq!(old.archived_at, Some(later));
    assert_eq!(
        old.activated_at,
        Some(now),
        "an archived tier still says when it was the current one"
    );

    let new = states.iter().find(|r| r.catalog == "new").unwrap();
    assert_eq!(new.activated_at, Some(later));
    assert_eq!(new.archived_at, None);
}

/// The button that does this is one a person may press twice, and the second
/// press must not archive the catalogue the first one activated.
#[tokio::test]
async fn activating_the_active_catalogue_changes_nothing() {
    let store = store().await;
    let releases = store.releases();
    let now = Millis(1_000);
    releases
        .seed(&[("only".to_string(), CatalogStatus::Active)], now)
        .await
        .unwrap();

    let done = releases.activate("only", Millis(2_000)).await.unwrap();
    assert_eq!(done.archived, None);

    let states = releases.releases().await.unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].state, CatalogStatus::Active);
    assert_eq!(
        states[0].activated_at,
        Some(now),
        "it did not become active again; it never stopped"
    );
}

/// An upgrade must not silently undo an activation. The binary's opinion is
/// the seed and nothing more.
#[tokio::test]
async fn seeding_never_overwrites_a_state_somebody_set() {
    let store = store().await;
    let releases = store.releases();
    releases
        .seed(
            &[("a".to_string(), CatalogStatus::Active), draft("b")],
            Millis(1),
        )
        .await
        .unwrap();
    releases.activate("b", Millis(2)).await.unwrap();

    // The next release ships with the same defaults it always had, plus one
    // new catalogue.
    let added = releases
        .seed(
            &[
                ("a".to_string(), CatalogStatus::Active),
                draft("b"),
                draft("c"),
            ],
            Millis(3),
        )
        .await
        .unwrap();
    assert_eq!(added, 1, "only the catalogue the database had never seen");

    let states = releases.releases().await.unwrap();
    let by_id = |id: &str| states.iter().find(|r| r.catalog == id).unwrap().state;
    assert_eq!(by_id("a"), CatalogStatus::Archived);
    assert_eq!(by_id("b"), CatalogStatus::Active, "the activation survived");
    assert_eq!(by_id("c"), CatalogStatus::DraftPtr);
}

/// Activating something this instance has no catalogue for would leave a state
/// with no content behind it.
#[tokio::test]
async fn an_unknown_catalogue_cannot_be_activated() {
    let store = store().await;
    assert!(matches!(
        store.releases().activate("nothing", Millis(1)).await,
        Err(app_core::error::RepoError::NotFound)
    ));
}

/// The database, not a template and not the transaction's good behaviour, is
/// what makes two active catalogues impossible. This writes past the port to
/// prove the constraint is really there.
#[tokio::test]
async fn the_schema_refuses_a_second_active_catalogue() {
    let store = store().await;
    store
        .releases()
        .seed(
            &[("a".to_string(), CatalogStatus::Active), draft("b")],
            Millis(1),
        )
        .await
        .unwrap();

    let forced = sqlx::query("UPDATE catalog_releases SET state = 'active' WHERE catalog_id = 'b'")
        .execute(store.pool())
        .await;
    assert!(
        forced.is_err(),
        "the partial unique index has to refuse this, or 'exactly one active' is only a habit"
    );

    // Zero active, on the other hand, is a legal state: an expansion that has
    // ended while its successor is still on the PTR.
    sqlx::query("UPDATE catalog_releases SET state = 'archived' WHERE catalog_id = 'a'")
        .execute(store.pool())
        .await
        .expect("nothing active is allowed");
}

// --- the game's timeline -----------------------------------------------------

fn event(id: &str, at: u64, validation: Validation, visibility: Visibility) -> MarketEvent {
    MarketEvent {
        id: id.to_string(),
        kind: EventKind::Annotation,
        title: format!("Event {id}"),
        notes: Some("a note".into()),
        starts_at: Millis(at),
        ends_at: None,
        scope: EventScope {
            regions: vec![Region::Eu],
            patch: Some("12.1".into()),
            ..EventScope::default()
        },
        provenance: Provenance::Administrator,
        validation,
        visibility,
    }
}

#[tokio::test]
async fn an_event_round_trips_with_its_whole_scope() {
    let store = store().await;
    let mut original = event("a", 1_000, Validation::Validated, Visibility::Public);
    original.scope.market = Some(MarketKey::commodity(Region::Eu, ItemId(212_265), 3));
    original.scope.tier = Some("venomous-abyss".into());
    original.ends_at = Some(Millis(2_000));

    assert_eq!(
        store
            .market_events()
            .record(&[original.clone()])
            .await
            .unwrap(),
        1
    );
    let back = store
        .market_events()
        .between(Millis(0), Millis(10_000), false)
        .await
        .unwrap();
    assert_eq!(back, vec![original]);
}

/// The catalogue's events are re-derived at every start, so recording them
/// twice must not produce two copies of a patch release.
#[tokio::test]
async fn recording_the_same_event_twice_writes_it_once() {
    let store = store().await;
    let one = event("a", 1_000, Validation::Validated, Visibility::Public);

    assert_eq!(
        store
            .market_events()
            .record(std::slice::from_ref(&one))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .market_events()
            .record(std::slice::from_ref(&one))
            .await
            .unwrap(),
        0,
        "the second start adds nothing"
    );

    // And an edit made after the first seed survives the second, the same way
    // a release state somebody set survives an upgrade.
    let mut edited = one.clone();
    edited.title = "Corrected by hand".into();
    assert_eq!(store.market_events().record(&[edited]).await.unwrap(), 0);
    let back = store
        .market_events()
        .between(Millis(0), Millis(10_000), false)
        .await
        .unwrap();
    assert_eq!(back[0].title, one.title, "a seed never overwrites");
}

/// An internal note must not leak, and neither must one nobody has checked.
#[tokio::test]
async fn a_visitor_sees_only_checked_public_events() {
    let store = store().await;
    store
        .market_events()
        .record(&[
            event("public", 1_000, Validation::Validated, Visibility::Public),
            event(
                "unchecked",
                2_000,
                Validation::Unvalidated,
                Visibility::Public,
            ),
            event("rejected", 3_000, Validation::Rejected, Visibility::Public),
            event(
                "internal",
                4_000,
                Validation::Validated,
                Visibility::Internal,
            ),
        ])
        .await
        .unwrap();

    let everything = store
        .market_events()
        .between(Millis(0), Millis(10_000), false)
        .await
        .unwrap();
    assert_eq!(everything.len(), 4);

    let visible = store
        .market_events()
        .between(Millis(0), Millis(10_000), true)
        .await
        .unwrap();
    let ids: Vec<&str> = visible.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["public"]);
}

/// "What was going on then" wants both the instants inside the window and the
/// intervals that were still running through it.
#[tokio::test]
async fn a_window_holds_what_happened_and_what_was_still_happening() {
    let store = store().await;
    let mut running = event("running", 1_000, Validation::Validated, Visibility::Public);
    running.ends_at = Some(Millis(9_000));
    let inside = event("inside", 5_000, Validation::Validated, Visibility::Public);
    let after = event("after", 20_000, Validation::Validated, Visibility::Public);
    let mut ended_before = event("before", 100, Validation::Validated, Visibility::Public);
    ended_before.ends_at = Some(Millis(200));

    store
        .market_events()
        .record(&[running, inside, after, ended_before])
        .await
        .unwrap();

    let window = store
        .market_events()
        .between(Millis(4_000), Millis(6_000), false)
        .await
        .unwrap();
    let ids: Vec<&str> = window.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        ["running", "inside"],
        "oldest first: the one still going on, and the one that started inside"
    );
}

// --- the read model, and the version that publishes it -----------------------

fn materialised(item: u32, price: u64, samples: u32) -> Materialised {
    let key = MarketKey::commodity(Region::Eu, ItemId(item), 1);
    Materialised {
        state: MarketState {
            key,
            observed_at: Some(Millis(1_000)),
            price: Copper(price),
            min_price: Copper(price),
            median_price: Copper(price),
            quantity: 100,
            listings: 3,
            first_seen: Some(Millis(0)),
            samples,
            mean: Copper(price),
            median: Copper(price),
            low: Copper(price),
            low_at: Millis(0),
            high: Copper(price),
            high_at: Millis(1_000),
            volatility_percent: 0,
            day: Trend::UNKNOWN,
            week: Trend::UNKNOWN,
            month: Trend::UNKNOWN,
            by_hour: vec![Cycle {
                bucket: 3,
                mean: Copper(price),
                samples: 1,
            }],
            by_weekday: Vec::new(),
            best_hour: Some(3),
            best_weekday: None,
            series: vec![Point {
                at: Millis(1_000),
                price: Copper(price),
                quantity: 100,
            }],
        },
        windows: vec![MarketWindow {
            key,
            window: Window::Days(7),
            low: Copper(price),
            low_at: Millis(0),
            high: Copper(price),
            high_at: Millis(1_000),
            mean: Copper(price),
            median: Copper(price),
            samples,
            first_at: Millis(0),
            last_at: Millis(1_000),
            expected_buckets: Some(168),
            observed_buckets: samples,
            largest_gap_ms: 3_600_000,
        }],
    }
}

/// Everything a market carries has to come back the way it went in, JSON
/// columns included: the chart is drawn from them.
#[tokio::test]
async fn a_materialised_market_round_trips() {
    let store = store().await;
    let model = store.read_model();
    let version = model.begin(1, Millis(10)).await.unwrap();
    let original = materialised(10, 5_000, 42);

    model
        .stage(version, std::slice::from_ref(&original))
        .await
        .unwrap();
    model
        .publish(version, (Some(Millis(0)), Some(Millis(1_000))), Millis(20))
        .await
        .unwrap();

    let back = model.market(original.state.key).await.unwrap().unwrap();
    assert_eq!(back, original.state);

    let windows = model.windows_of(original.state.key).await.unwrap();
    assert_eq!(windows, original.windows);
}

/// The guarantee, in one test: while a version is being built, nothing about
/// it is reachable, and when it lands every page moves at once.
#[tokio::test]
async fn a_candidate_is_unreachable_until_it_is_published() {
    let store = store().await;
    let model = store.read_model();

    let first = model.begin(1, Millis(10)).await.unwrap();
    model
        .stage(first, &[materialised(10, 100, 1), materialised(11, 200, 1)])
        .await
        .unwrap();

    assert!(
        model.commodities(Region::Eu).await.unwrap().is_empty(),
        "a staged candidate is not a published version"
    );
    assert!(model.published().await.unwrap().is_none());

    model
        .publish(first, (None, None), Millis(20))
        .await
        .unwrap();
    let published = model.commodities(Region::Eu).await.unwrap();
    assert_eq!(published.len(), 2);
    assert_eq!(published[0].price, Copper(100));

    // A second candidate, staged but not published, changes nothing anybody
    // can see.
    let second = model.begin(1, Millis(30)).await.unwrap();
    model
        .stage(second, &[materialised(10, 999, 5)])
        .await
        .unwrap();
    assert_eq!(
        model.commodities(Region::Eu).await.unwrap()[0].price,
        Copper(100),
        "the previous version is still what is served"
    );

    model
        .publish(second, (None, None), Millis(40))
        .await
        .unwrap();
    let now = model.commodities(Region::Eu).await.unwrap();
    assert_eq!(now[0].price, Copper(999), "and now it is the new one");
    assert_eq!(
        now[1].price,
        Copper(200),
        "a market the new version did not recalculate keeps what it had"
    );
    assert_eq!(now.len(), 2);
}

/// The failure contract: a candidate that dies leaves the published version
/// exactly where it was, and says so where operations can see it.
#[tokio::test]
async fn abandoning_a_candidate_leaves_the_published_version_alone() {
    let store = store().await;
    let model = store.read_model();

    let good = model.begin(1, Millis(10)).await.unwrap();
    model
        .stage(good, &[materialised(10, 100, 1)])
        .await
        .unwrap();
    model.publish(good, (None, None), Millis(20)).await.unwrap();

    let doomed = model.begin(1, Millis(30)).await.unwrap();
    model
        .stage(doomed, &[materialised(10, 999, 9)])
        .await
        .unwrap();
    model.abandon(doomed, "eu: upstream said no").await.unwrap();

    assert_eq!(
        model.commodities(Region::Eu).await.unwrap()[0].price,
        Copper(100)
    );
    assert_eq!(model.published().await.unwrap().unwrap().version, good);

    // The failure is visible rather than merely absent (CLAUDE.md §14.7).
    let versions = model.versions(10).await.unwrap();
    let failed = versions.iter().find(|v| v.version == doomed).unwrap();
    assert_eq!(failed.state, VersionState::Failed);
    assert_eq!(failed.note.as_deref(), Some("eu: upstream said no"));
}

/// A materialiser killed halfway leaves staging rows nobody will publish. The
/// next run must not mistake them for its own -- that would publish half of
/// somebody else's work as if it were whole.
#[tokio::test]
async fn a_new_candidate_clears_what_a_dead_one_left() {
    let store = store().await;
    let model = store.read_model();

    let dead = model.begin(1, Millis(10)).await.unwrap();
    model
        .stage(dead, &[materialised(10, 100, 1)])
        .await
        .unwrap();
    // No publish, no abandon: the process died.

    let fresh = model.begin(1, Millis(20)).await.unwrap();
    assert_ne!(fresh, dead);

    let versions = model.versions(10).await.unwrap();
    let old = versions.iter().find(|v| v.version == dead).unwrap();
    assert_eq!(old.state, VersionState::Failed);
    assert!(old.note.as_deref().unwrap().contains("abandoned"));

    // Publishing the fresh candidate, which staged nothing, publishes nothing.
    model
        .publish(fresh, (None, None), Millis(30))
        .await
        .unwrap();
    assert!(model.commodities(Region::Eu).await.unwrap().is_empty());
}

/// Publishing something that is not a live candidate is a mistake worth
/// refusing rather than absorbing.
#[tokio::test]
async fn only_a_live_candidate_can_be_published() {
    let store = store().await;
    let model = store.read_model();
    assert!(matches!(
        model.publish(999, (None, None), Millis(1)).await,
        Err(app_core::error::RepoError::NotFound)
    ));

    let version = model.begin(1, Millis(10)).await.unwrap();
    model
        .publish(version, (None, None), Millis(20))
        .await
        .unwrap();
    assert!(matches!(
        model.publish(version, (None, None), Millis(30)).await,
        Err(app_core::error::RepoError::Conflict(_))
    ));
}
