//! Storage adapter tests.
//!
//! These assert the round-trip: everything written through a port comes back
//! through the port as the same domain value. They deliberately never touch a
//! SQL string, because callers never do either.

use app_core::model::Session;
use app_core::repo::{
    CacheStore, EventRepository, JobRepository, KeyValueStore, SessionRepository, Store,
    UserRepository,
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
