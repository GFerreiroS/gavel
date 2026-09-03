//! Storage adapter tests.
//!
//! These assert the round-trip: everything written through a port comes back
//! through the port as the same domain value. They deliberately never touch a
//! SQL string, because callers never do either.

use app_core::market::analysis::{Cycle, Point, Trend};
use app_core::market::catalog::CatalogStatus;
use app_core::market::event::{EventKind, EventScope, Provenance, Validation, Visibility};
use app_core::market::materialise::{
    LevelStat, MarketRollup, MarketState, MarketWindow, Materialised, ModifierStat, Scope,
};
use app_core::market::window::Window;
use app_core::market::{ItemKind, MarketEvent, MarketKey, Track};
use app_core::model::Session;
use app_core::repo::{
    CacheStore, EventRepository, JobRepository, KeyValueStore, MarketEventRepository,
    PriceRepository, ReadModelRepository, ReleaseRepository, SessionRepository, Store,
    UserRepository, VersionState,
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
    assert!(
        !created.is_admin,
        "public registration never bootstraps admin"
    );

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

/// Off by default, set, cleared -- and never spilling onto `by_id`'s `User`,
/// which is what pages render.
#[tokio::test]
async fn a_discord_webhook_round_trips_and_stays_off_the_user_view() {
    let store = store().await;
    let users = store.users();
    let alice = users.create("alice", "hash", Millis(0)).await.unwrap().id;

    assert_eq!(users.discord_webhook(alice).await.unwrap(), None);

    let url = "https://discord.com/api/webhooks/1/secret";
    users.set_discord_webhook(alice, Some(url)).await.unwrap();
    assert_eq!(
        users.discord_webhook(alice).await.unwrap().as_deref(),
        Some(url)
    );
    assert!(
        !format!("{:?}", users.by_id(alice).await.unwrap().unwrap()).contains("discord"),
        "the webhook must not ride along on the type a page renders"
    );

    users.set_discord_webhook(alice, None).await.unwrap();
    assert_eq!(users.discord_webhook(alice).await.unwrap(), None);
}

#[tokio::test]
async fn administrator_bootstrap_is_explicit_and_atomic() {
    let store = store().await;
    let users = store.users();
    let ordinary = users.create("first", "hash", Millis(1)).await.unwrap();
    assert!(!ordinary.is_admin, "creation order grants no privilege");

    let (left, right) = tokio::join!(
        users.bootstrap_admin("operator-a", "hash-a", Millis(2)),
        users.bootstrap_admin("operator-b", "hash-b", Millis(2)),
    );
    let created = usize::from(left.unwrap().is_some()) + usize::from(right.unwrap().is_some());
    assert_eq!(created, 1, "concurrent bootstrap creates exactly one admin");

    assert!(
        users
            .bootstrap_admin("operator-c", "hash-c", Millis(3))
            .await
            .unwrap()
            .is_none(),
        "bootstrap is one-shot once an administrator exists"
    );

    let admin = users
        .by_username("operator-a")
        .await
        .unwrap()
        .or(users.by_username("operator-b").await.unwrap())
        .unwrap()
        .user;
    assert!(users.delete(admin.id).await.unwrap());
    assert!(
        users
            .bootstrap_admin("operator-d", "hash-d", Millis(4))
            .await
            .unwrap()
            .is_none(),
        "deleting personal data must not reopen bootstrap"
    );
}

#[tokio::test]
async fn operational_retention_removes_only_old_terminal_history() {
    use cluster_core::{EventLog, JobStore};

    let store = store().await;
    let jobs = store.jobs();
    let make = |id: u64, created: u64, state: JobState| {
        let spec = JobSpec::Sleep {
            total_ms: 1,
            tasks: 1,
        };
        let mut job = cluster_core::Job::new(cluster_core::JobId(id), spec, Millis(created));
        let task = cluster_core::Task::new(
            cluster_core::TaskId(id),
            job.id,
            0,
            spec.split()[0],
            Millis(created),
        );
        job.state = state;
        if state.is_terminal() {
            job.finished_at = Some(Millis(created));
        }
        (job, task)
    };
    let (old_done, old_task) = make(1, 10, JobState::Completed);
    let (old_live, live_task) = make(2, 10, JobState::Queued);
    let (new_done, new_task) = make(3, 30, JobState::Completed);
    for (job, task) in [
        (&old_done, &old_task),
        (&old_live, &live_task),
        (&new_done, &new_task),
    ] {
        jobs.create_job(job, std::slice::from_ref(task))
            .await
            .unwrap();
    }

    assert_eq!(jobs.prune_terminal_before(Millis(20)).await.unwrap(), 1);
    assert!(jobs.job(old_done.id).await.unwrap().is_none());
    assert!(jobs.job(old_live.id).await.unwrap().is_some());
    assert!(jobs.job(new_done.id).await.unwrap().is_some());

    store
        .events()
        .append(&EventRecord::new(
            1,
            Millis(10),
            ClusterEvent::NodeJoined { node: NodeId(1) },
        ))
        .await
        .unwrap();
    store
        .events()
        .append(&EventRecord::new(
            2,
            Millis(30),
            ClusterEvent::NodeJoined { node: NodeId(2) },
        ))
        .await
        .unwrap();
    assert_eq!(store.events().prune_before(Millis(20)).await.unwrap(), 1);
    assert_eq!(store.events().recent(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn deleting_an_account_cascades_owned_data() {
    let store = store().await;
    let users = store.users();
    let user = users.create("eraseme", "hash", Millis(1)).await.unwrap();
    store
        .sessions()
        .create(&Session {
            id: "owned-session".into(),
            user_id: user.id,
            created_at: Millis(1),
            expires_at: Millis(10),
        })
        .await
        .unwrap();
    assert!(users.delete(user.id).await.unwrap());
    assert!(users.by_id(user.id).await.unwrap().is_none());
    assert!(
        store
            .sessions()
            .get("owned-session")
            .await
            .unwrap()
            .is_none()
    );
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
           SELECT samples.item_id, samples.realm_id, variants.variant,
                  samples.observed_at, samples.min_price,
                  ROW_NUMBER() OVER (
                      PARTITION BY samples.item_id, samples.realm_id, samples.variant_id
                      ORDER BY samples.observed_at DESC) AS rn
             FROM realm_price_samples AS samples
             JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
            WHERE samples.region = ?",
    );
    if realm.is_some() {
        sql.push_str(" AND samples.realm_id = ?");
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

/// Migration 0026 must preserve every bonus-list identity exactly. This starts
/// from the pre-0026 source-table shapes rather than from a fresh database,
/// which would only prove the new schema can be created.
#[tokio::test]
async fn variant_dictionary_migration_is_lossless() {
    use sqlx::sqlite::SqlitePoolOptions;

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct Sample {
        item_id: i64,
        region: String,
        realm_id: i64,
        variant: String,
        observed_at: i64,
        min_price: i64,
        median_price: i64,
        max_price: i64,
        listings: i64,
    }

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("old-shape database");
    sqlx::raw_sql(
        "CREATE TABLE realm_price_samples (
            item_id INTEGER NOT NULL, region TEXT NOT NULL, realm_id INTEGER NOT NULL,
            variant TEXT NOT NULL, observed_at INTEGER NOT NULL, min_price INTEGER NOT NULL,
            median_price INTEGER NOT NULL, max_price INTEGER NOT NULL DEFAULT 0,
            listings INTEGER NOT NULL,
            PRIMARY KEY (item_id, region, realm_id, variant, observed_at)
        ) WITHOUT ROWID;
        CREATE TABLE realm_price_ladders (
            item_id INTEGER NOT NULL, region TEXT NOT NULL, realm_id INTEGER NOT NULL,
            variant TEXT NOT NULL, observed_at INTEGER NOT NULL, levels INTEGER NOT NULL,
            total INTEGER NOT NULL, steps TEXT NOT NULL,
            PRIMARY KEY (item_id, region, realm_id, variant, observed_at)
        ) WITHOUT ROWID;",
    )
    .execute(&pool)
    .await
    .expect("old tables");

    sqlx::query(
        "INSERT INTO realm_price_samples
           (item_id, region, realm_id, variant, observed_at,
            min_price, median_price, max_price, listings)
         VALUES (10, 'eu', 1403, '12833,13333', 1000, 100, 125, 150, 2),
                (10, 'eu', 1403, '',            2000, 200, 250, 300, 3),
                (11, 'us', 60,   '12833,13333', 3000, 400, 450, 500, 4)",
    )
    .execute(&pool)
    .await
    .expect("old samples");
    sqlx::query(
        "INSERT INTO realm_price_ladders
           (item_id, region, realm_id, variant, observed_at, levels, total, steps)
         VALUES (10, 'eu', 1403, '12833,13333', 1000, 2, 2, '100:1,150:1'),
                (10, 'eu', 1403, 'socket,13333', 2000, 1, 1, '250:1')",
    )
    .execute(&pool)
    .await
    .expect("old ladders");

    sqlx::raw_sql(include_str!("../../../migrations/0026_variant_ids.sql"))
        .execute(&pool)
        .await
        .expect("migration 0026");

    let samples: Vec<Sample> = sqlx::query_as(
        "SELECT samples.item_id, samples.region, samples.realm_id, variants.variant,
                samples.observed_at, samples.min_price, samples.median_price,
                samples.max_price, samples.listings
           FROM realm_price_samples AS samples
           JOIN market_variants AS variants ON variants.variant_id = samples.variant_id
          ORDER BY samples.item_id, samples.region, samples.realm_id,
                   variants.variant, samples.observed_at",
    )
    .fetch_all(&pool)
    .await
    .expect("migrated samples");
    assert_eq!(
        samples,
        vec![
            Sample {
                item_id: 10,
                region: "eu".into(),
                realm_id: 1403,
                variant: "".into(),
                observed_at: 2000,
                min_price: 200,
                median_price: 250,
                max_price: 300,
                listings: 3,
            },
            Sample {
                item_id: 10,
                region: "eu".into(),
                realm_id: 1403,
                variant: "12833,13333".into(),
                observed_at: 1000,
                min_price: 100,
                median_price: 125,
                max_price: 150,
                listings: 2,
            },
            Sample {
                item_id: 11,
                region: "us".into(),
                realm_id: 60,
                variant: "12833,13333".into(),
                observed_at: 3000,
                min_price: 400,
                median_price: 450,
                max_price: 500,
                listings: 4,
            },
        ]
    );

    #[derive(Debug, PartialEq, sqlx::FromRow)]
    struct LadderRow {
        item_id: i64,
        region: String,
        realm_id: i64,
        variant: String,
        observed_at: i64,
        levels: i64,
        total: i64,
        steps: String,
    }

    let ladders: Vec<LadderRow> = sqlx::query_as(
        "SELECT ladders.item_id, ladders.region, ladders.realm_id, variants.variant,
                ladders.observed_at, ladders.levels, ladders.total, ladders.steps
           FROM realm_price_ladders AS ladders
           JOIN market_variants AS variants ON variants.variant_id = ladders.variant_id
          ORDER BY ladders.item_id, ladders.region, ladders.realm_id,
                   variants.variant, ladders.observed_at",
    )
    .fetch_all(&pool)
    .await
    .expect("migrated ladders");
    assert_eq!(
        ladders,
        vec![
            LadderRow {
                item_id: 10,
                region: "eu".into(),
                realm_id: 1403,
                variant: "12833,13333".into(),
                observed_at: 1000,
                levels: 2,
                total: 2,
                steps: "100:1,150:1".into(),
            },
            LadderRow {
                item_id: 10,
                region: "eu".into(),
                realm_id: 1403,
                variant: "socket,13333".into(),
                observed_at: 2000,
                levels: 1,
                total: 1,
                steps: "250:1".into(),
            },
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT dflt_value FROM pragma_table_info('realm_price_samples')
              WHERE name = 'max_price'",
        )
        .fetch_one(&pool)
        .await
        .expect("max_price schema"),
        Some("0".into()),
        "migration 0005's compatibility default survives the table rebuild"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM market_variants")
            .fetch_one(&pool)
            .await
            .unwrap(),
        3,
        "the dictionary is the union of sample and ladder variants"
    );
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

/// The direction raising an alert actually needs: not "what does Alice
/// follow" but "who follows this market" -- and only that market, not a
/// namesake item in another region or a different item in the same one.
#[tokio::test]
async fn watchers_of_a_market_are_found_and_nobody_elses_are() {
    let store = store().await;
    let alice = a_user(&store, "alice").await;
    let bob = a_user(&store, "bob").await;
    let watches = store.watches();

    watches
        .watch(alice, ItemId(1), Region::Eu, Millis(10))
        .await
        .unwrap();
    watches
        .watch(bob, ItemId(1), Region::Eu, Millis(20))
        .await
        .unwrap();
    // A namesake in another region, and a different item in the same
    // region: neither belongs in the answer for (1, Eu).
    watches
        .watch(alice, ItemId(1), Region::Us, Millis(30))
        .await
        .unwrap();
    watches
        .watch(bob, ItemId(2), Region::Eu, Millis(40))
        .await
        .unwrap();

    let mut watchers = watches.watchers(ItemId(1), Region::Eu).await.unwrap();
    watchers.sort();
    assert_eq!(watchers, vec![alice, bob]);

    assert!(
        watches
            .watchers(ItemId(99), Region::Eu)
            .await
            .unwrap()
            .is_empty(),
        "nobody watches an item nobody followed"
    );
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

/// A release that adds a catalogue shipping as `active` must not stop the
/// instance from starting.
///
/// Found by running a rollover rather than by reading the code: the second
/// active row hits the partial unique index, `seed` returns the conflict, and
/// startup fails on it. Refusing to boot is the harshest failure there is and
/// the wrong one here -- nothing is broken, a person has simply not chosen
/// yet. So the newcomer waits at `/admin` as a draft.
#[tokio::test]
async fn a_newcomer_shipping_as_active_waits_instead_of_breaking_startup() {
    let store = store().await;
    let releases = store.releases();

    releases
        .seed(&[("s2".to_string(), CatalogStatus::Active)], Millis(1_000))
        .await
        .expect("the first seed, on an empty database");

    // The next release ships season 3, and ships it active.
    let seeded = releases
        .seed(
            &[
                ("s2".to_string(), CatalogStatus::Active),
                ("s3".to_string(), CatalogStatus::Active),
            ],
            Millis(2_000),
        )
        .await
        .expect("the second seed must not fail");
    assert_eq!(seeded, 1, "only the newcomer was written");

    let states = releases.releases().await.unwrap();
    let state = |id: &str| {
        states
            .iter()
            .find(|r| r.catalog == id)
            .map(|r| r.state)
            .unwrap()
    };
    assert_eq!(state("s2"), CatalogStatus::Active, "still collecting");
    assert_eq!(
        state("s3"),
        CatalogStatus::DraftPtr,
        "the newcomer waits for somebody to activate it"
    );

    // And it activates normally from there, archiving the one it replaces.
    let done = releases.activate("s3", Millis(3_000)).await.unwrap();
    assert_eq!(done.archived.as_deref(), Some("s2"));
}

/// Two active catalogues in one seed is the same hazard on an empty database.
/// A test asserts the shipped file never does it; this is what makes that
/// test's failure a reviewable state rather than an instance that will not
/// start.
#[tokio::test]
async fn two_catalogues_shipping_as_active_seed_one_of_them() {
    let store = store().await;
    let releases = store.releases();
    releases
        .seed(
            &[
                ("a".to_string(), CatalogStatus::Active),
                ("b".to_string(), CatalogStatus::Active),
            ],
            Millis(1_000),
        )
        .await
        .expect("seeding must not fail");

    let states = releases.releases().await.unwrap();
    let active: Vec<&str> = states
        .iter()
        .filter(|r| r.state.is_active())
        .map(|r| r.catalog.as_str())
        .collect();
    assert_eq!(active, ["a"], "the first wins; the rest wait");
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

    // A dense ladder with a wall in it, and the summary swept from it rather
    // than written by hand -- so the round trip covers a real encoding, walls
    // and optional percentiles included, instead of a `None`.
    let rung = |over: u64, quantity: u64| app_core::market::Listing {
        item: ItemId(item),
        unit_price: Copper(price + over),
        quantity,
    };
    let ladder = app_core::market::Ladder::of(&[
        rung(0, 5),
        rung(10, 10),
        rung(20, 20),
        rung(30, 40),
        rung(40, 80),
    ]);
    let depth = app_core::market::Depth::of(&ladder, app_core::market::Target(20));

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
            ladder,
            depth,
            // A heatmap with holes in it, a correlation with its pair count,
            // and a stability figure: the round trip has to keep a `None`
            // apart from a zero on each, because "not enough evidence" and
            // "no movement" are opposite readings of the same column.
            heatmap: app_core::market::Heatmap::of(
                (0..(24 * 4u64)).map(|h| (Millis(h * 3_600_000), Copper(price + h))),
            ),
            stock_association: Some(app_core::market::Association {
                rho_percent: -42,
                pairs: 96,
            }),
            swings: app_core::market::Swings {
                drawdown_percent: 17,
                rise_percent: 33,
            },
            stability: Some(app_core::market::Stability {
                typical_move_percent: 4,
                changes: 95,
            }),
        },
        windows: vec![MarketWindow {
            key,
            window: Window::Days(7),
            low: Copper(price),
            low_at: Millis(0),
            high: Copper(price),
            high_at: Millis(1_000),
            mean: Copper(price),
            distribution: app_core::market::engine::Distribution {
                p05: Copper(price),
                p25: Copper(price),
                median: Copper(price),
                p75: Copper(price),
                p95: Copper(price),
                iqr: Copper(0),
                mad: Copper(0),
                buckets: samples,
            },
            position: app_core::market::engine::Position {
                rank: Some(50),
                valuation: Some(app_core::market::engine::Valuation::Typical),
                insufficient: None,
                from_median_percent: Some(0),
                anomaly: app_core::market::engine::Anomaly::Ordinary,
            },
            swing: app_core::market::engine::Swing(0),
            // With a gap in the middle, deliberately: the sparkline is stored
            // as a string rather than JSON, and a `None` between two values is
            // the case that encoding has to survive a round trip through
            // SQLite. `assert_eq!(windows, original.windows)` below covers it.
            spark: app_core::market::engine::Spark {
                slots: vec![Some(Copper(price)), None, Some(Copper(price))],
            },
            // A gap in the middle here too, for the same reason: the series
            // encodes an unobserved slot as an empty record, and a `None`
            // between two values is the case that has to survive SQLite.
            series: app_core::market::series::ChartSeries {
                from: Millis(0),
                until: Millis(1_000),
                points: vec![
                    app_core::market::series::ChartPoint {
                        at: Millis(0),
                        price: Copper(price),
                        median: Copper(price),
                        p25: Copper(price),
                        p75: Copper(price),
                        quantity: 5,
                        listings: 2,
                        observed: true,
                    },
                    // 333, not 500: a slot's instant is derived from the
                    // span and its index rather than stored, which is what
                    // keeps ninety-six timestamps out of the column. Writing
                    // an arbitrary one here would be testing a field the
                    // encoding does not carry.
                    app_core::market::series::ChartPoint {
                        at: Millis(333),
                        ..Default::default()
                    },
                    app_core::market::series::ChartPoint {
                        at: Millis(666),
                        price: Copper(price),
                        median: Copper(price),
                        p25: Copper(price),
                        p75: Copper(price),
                        quantity: 7,
                        listings: 3,
                        observed: true,
                    },
                ],
            },
            histogram: Some(app_core::market::series::Histogram {
                lo: Copper(price),
                hi: Copper(price * 2),
                bins: vec![1; app_core::market::series::BINS],
            }),
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

/// A per-realm roll-up, with every column Phase 5 added filled in.
///
/// Deliberately not `MarketRollup::empty` plus a price: the bug this test was
/// written after was a column list and a placeholder list that had drifted
/// apart, and only a row that binds *every* column can catch that.
fn rollup(item: u32, price: u64) -> MarketRollup {
    MarketRollup {
        depth: None,
        ladder: Default::default(),
        region: Region::Eu,
        item: ItemId(item),
        kind: ItemKind::Boe,
        track: Some(Track::Hero),
        scope: Scope::Realm(RealmId(1403)),
        window: Window::Days(7),
        observed_at: Some(Millis(1_000)),
        snapshots: 12,
        realms_listing: 3,
        cheapest_now: Some(Copper(price)),
        cheapest_realm: Some(RealmId(1403)),
        dearest_realm_now: Some(Copper(price * 2)),
        dearest_realm: Some(RealmId(1404)),
        median_realm_now: Some(Copper(price + price / 2)),
        highest_now: Some(Copper(price * 3)),
        cheapest_ever: Some(Copper(price / 2)),
        highest_ever: Some(Copper(price * 4)),
        listings_now: 7,
        listings_seen: 91,
        level_range: "279-285".to_string(),
        levels: vec![LevelStat {
            item_level: 285,
            upgrade: "Hero 4/6".to_string(),
            cheapest: Copper(price),
            highest: Copper(price * 2),
            listings: 4,
            realms: 1,
        }],
        modifiers: vec![ModifierStat {
            name: "Leech".to_string(),
            now: 2,
            seen: 9,
        }],
        series: vec![Point {
            at: Millis(1_000),
            price: Copper(price),
            quantity: 7,
        }],
        distribution: Some(app_core::market::engine::Distribution {
            p05: Copper(price / 2),
            p25: Copper(price - 10),
            median: Copper(price),
            p75: Copper(price + 10),
            p95: Copper(price * 2),
            iqr: Copper(20),
            mad: Copper(8),
            buckets: 96,
        }),
        position: Some(app_core::market::engine::Position {
            rank: Some(4),
            valuation: Some(app_core::market::engine::Valuation::VeryCheap),
            insufficient: None,
            from_median_percent: Some(-31),
            anomaly: app_core::market::engine::Anomaly::Mild,
        }),
        swing: app_core::market::engine::Swing(140),
        realms_collected: 5,
        // Three realms listing out of five collected, and the spread across
        // them: the availability fraction and the dispersion Phase 8 adds.
        realm_spread: Some(app_core::market::engine::Distribution {
            p05: Copper(price),
            p25: Copper(price + 5),
            median: Copper(price + 20),
            p75: Copper(price + 60),
            p95: Copper(price * 2),
            iqr: Copper(55),
            mad: Copper(20),
            buckets: 3,
        }),
    }
}

/// A roll-up comes back the way it went in, band and all.
///
/// This path had no test at all until Phase 5 added sixteen columns to it and
/// the insert went out by two placeholders -- which every existing test passed
/// straight through, because none of them staged a roll-up. The failure only
/// appeared when the server was run against the real archive: `39 values for
/// 41 columns`, once, in a log line, on a code path a page reads from.
#[tokio::test]
async fn a_rolled_up_market_round_trips() {
    let store = store().await;
    let model = store.read_model();
    let version = model.begin(2, Millis(10)).await.unwrap();
    let original = rollup(212_265, 5_000);

    model
        .stage_rollups(version, std::slice::from_ref(&original))
        .await
        .unwrap();
    model
        .publish(version, (Some(Millis(0)), Some(Millis(1_000))), Millis(20))
        .await
        .unwrap();

    let back = model
        .rollup(
            original.region,
            original.item,
            original.track,
            original.scope,
        )
        .await
        .unwrap()
        .expect("the roll-up is published");
    assert_eq!(back, original);
}

/// An item detail needs both the evidence-bearing regional row and each realm
/// behind it. Keeping them in one indexed read is also the input shape Deals
/// will need later; neither caller needs to reduce the archive.
#[tokio::test]
async fn item_rollups_return_regional_evidence_and_every_realm() {
    let store = store().await;
    let model = store.read_model();
    let version = model.begin(2, Millis(10)).await.unwrap();

    let mut regional = rollup(212_265, 5_000);
    regional.scope = Scope::Region;
    regional.realms_listing = 2;
    regional.realms_collected = 3;

    let first = rollup(212_265, 5_000);
    let mut second = rollup(212_265, 7_000);
    second.scope = Scope::Realm(RealmId(1404));
    let unrelated = rollup(212_266, 9_000);

    model
        .stage_rollups(
            version,
            &[regional.clone(), first.clone(), second.clone(), unrelated],
        )
        .await
        .unwrap();
    model
        .publish(version, (Some(Millis(0)), Some(Millis(1_000))), Millis(20))
        .await
        .unwrap();

    let rows = model
        .item_rollups(Region::Eu, ItemId(212_265))
        .await
        .unwrap();
    assert_eq!(rows, vec![regional, first, second]);
    assert_eq!(rows[0].scope, Scope::Region);
    assert_eq!(rows[0].realms_listing, 2);
    assert_eq!(rows[0].realms_collected, 3);

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN
         SELECT * FROM market_rollup
         WHERE state = 'published' AND region = ? AND item_id = ?
         ORDER BY track, realm_id",
    )
    .bind(Region::Eu.as_str())
    .bind(212_265_i64)
    .fetch_all(store.pool())
    .await
    .expect("query plan");
    let detail: Vec<String> = plan.into_iter().map(|row| row.get(3)).collect();
    assert!(
        detail
            .iter()
            .any(|step| step.contains("SEARCH market_rollup USING PRIMARY KEY")),
        "expected the (region, item_id, track, realm_id, state) primary key: {detail:?}"
    );
}

/// Deals needs the same regional evidence plus every purchasable realm, but
/// for every item at once. It remains a published read-model range, never an
/// archive reduction during a request.
#[tokio::test]
async fn deal_rollups_return_every_items_regional_and_realm_rows() {
    let store = store().await;
    let model = store.read_model();
    let version = model.begin(2, Millis(10)).await.unwrap();

    let mut first_regional = rollup(212_265, 5_000);
    first_regional.scope = Scope::Region;
    let first_realm = rollup(212_265, 5_000);
    let mut second_regional = rollup(212_266, 9_000);
    second_regional.scope = Scope::Region;
    let mut second_realm = rollup(212_266, 9_000);
    second_realm.scope = Scope::Realm(RealmId(1404));

    model
        .stage_rollups(
            version,
            &[
                first_regional.clone(),
                first_realm.clone(),
                second_regional.clone(),
                second_realm.clone(),
            ],
        )
        .await
        .unwrap();
    model
        .publish(version, (Some(Millis(0)), Some(Millis(1_000))), Millis(20))
        .await
        .unwrap();

    assert_eq!(
        model.deal_rollups(Region::Eu).await.unwrap(),
        vec![first_regional, first_realm, second_regional, second_realm]
    );

    let plan = sqlx::query(
        "EXPLAIN QUERY PLAN
         SELECT * FROM market_rollup
         WHERE state = 'published' AND region = ?
         ORDER BY item_id, track, realm_id",
    )
    .bind(Region::Eu.as_str())
    .fetch_all(store.pool())
    .await
    .expect("query plan");
    let detail: Vec<String> = plan.into_iter().map(|row| row.get(3)).collect();
    assert!(
        detail
            .iter()
            .any(|step| step.contains("SEARCH market_rollup USING PRIMARY KEY")),
        "expected the (region, item_id, track, realm_id, state) primary key: {detail:?}"
    );
}

/// The refusal survives the round trip too, with its two numbers.
///
/// A band and a reason are stored in different columns, and a reason is the
/// thing a card prints *instead* of a band -- so a reason that came back as
/// `None` would be a card going quiet, which is the failure §5.3 names.
#[tokio::test]
async fn a_rollup_without_enough_history_keeps_its_reason() {
    let store = store().await;
    let model = store.read_model();
    let version = model.begin(2, Millis(10)).await.unwrap();

    let mut original = rollup(212_266, 900);
    original.position = Some(app_core::market::engine::Position {
        rank: Some(50),
        valuation: None,
        insufficient: Some(app_core::market::engine::Insufficient::TooManyGaps {
            coverage: 12,
            need: 25,
        }),
        from_median_percent: Some(0),
        anomaly: app_core::market::engine::Anomaly::Ordinary,
    });

    model
        .stage_rollups(version, std::slice::from_ref(&original))
        .await
        .unwrap();
    model
        .publish(version, (None, None), Millis(20))
        .await
        .unwrap();

    let back = model
        .rollup(
            original.region,
            original.item,
            original.track,
            original.scope,
        )
        .await
        .unwrap()
        .expect("the roll-up is published");
    assert_eq!(back.position, original.position);
}

// --- market depth (Phase 7) --------------------------------------------------

/// Ladders round-trip, and re-recording an instant is a no-op.
///
/// The second half is the one that matters: a retried collection must not
/// double a market's supply, and it must not replace a stored ladder with a
/// differently-parsed copy of itself.
#[tokio::test]
async fn a_commodity_ladder_round_trips_and_a_retry_changes_nothing() {
    let store = store().await;
    let prices = store.prices();

    let ladder = app_core::market::Ladder::of(&[
        app_core::market::Listing {
            item: ItemId(212_265),
            unit_price: Copper(100),
            quantity: 20,
        },
        app_core::market::Listing {
            item: ItemId(212_265),
            unit_price: Copper(150),
            quantity: 300,
        },
    ]);
    let rows = &[(ItemId(212_265), ladder.clone())];

    assert_eq!(
        prices
            .record_ladders(Region::Eu, Millis(1_000), rows)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        prices
            .record_ladders(Region::Eu, Millis(1_000), rows)
            .await
            .unwrap(),
        0,
        "the same instant twice is one ladder"
    );

    let back = prices.latest_ladders(Region::Eu).await.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].0, ItemId(212_265));
    assert_eq!(back[0].1, Millis(1_000));
    assert_eq!(back[0].2, ladder);
    assert_eq!(back[0].2.total(), 320);
}

/// The newest ladder wins, and the hot window drops the rest.
#[tokio::test]
async fn ladders_leave_the_hot_window_without_taking_the_newest_with_them() {
    let store = store().await;
    let prices = store.prices();
    let rung = |price: u64, quantity: u64| app_core::market::Listing {
        item: ItemId(1),
        unit_price: Copper(price),
        quantity,
    };

    for (at, price) in [(1_000u64, 100u64), (2_000, 200), (3_000, 300)] {
        prices
            .record_ladders(
                Region::Eu,
                Millis(at),
                &[(ItemId(1), app_core::market::Ladder::of(&[rung(price, 5)]))],
            )
            .await
            .unwrap();
    }

    let newest = prices.latest_ladders(Region::Eu).await.unwrap();
    assert_eq!(newest.len(), 1, "one row per market, the newest");
    assert_eq!(newest[0].1, Millis(3_000));

    assert_eq!(
        prices.prune_ladders_before(Millis(2_500)).await.unwrap(),
        2,
        "the two that left the window"
    );
    let after = prices.latest_ladders(Region::Eu).await.unwrap();
    assert_eq!(after[0].1, Millis(3_000), "and the newest is still here");
}

/// The sparse half: per realm, per variant, and read one item at a time.
///
/// A region-wide sweep of these would be 35,720 markets to answer a question
/// about one BoE, which is why the port asks for an item.
#[tokio::test]
async fn a_realm_ladder_is_stored_per_variant() {
    let store = store().await;
    let prices = store.realm_prices();
    let one = |price: u64| {
        app_core::market::Ladder::of(&[app_core::market::Listing {
            item: ItemId(271_441),
            unit_price: Copper(price),
            quantity: 1,
        }])
    };

    let rows = vec![
        (ItemId(271_441), "12833,13333".to_string(), one(25_000)),
        (ItemId(271_441), "12843,13334".to_string(), one(300_000)),
    ];
    assert_eq!(
        prices
            .record_ladders(Region::Eu, RealmId(1403), Millis(1_000), &rows)
            .await
            .unwrap(),
        2,
        "two variants are two markets"
    );

    let back = prices
        .latest_ladders_for(Region::Eu, ItemId(271_441))
        .await
        .unwrap();
    assert_eq!(back.len(), 2);
    assert!(back.iter().all(|(realm, ..)| *realm == RealmId(1403)));
    assert!(back.iter().all(|(_, _, at, _)| *at == Millis(1_000)));
    let cheap = back
        .iter()
        .find(|(_, variant, _, _)| variant == "12833,13333")
        .expect("the variant we stored");
    assert_eq!(cheap.3.cheapest(), Some(Copper(25_000)));
    assert!(
        cheap.3.is_sparse(),
        "one auction is not a distribution, and the metrics say so"
    );

    assert_eq!(prices.prune_ladders_before(Millis(2_000)).await.unwrap(), 2);
    assert!(
        prices
            .latest_ladders_for(Region::Eu, ItemId(271_441))
            .await
            .unwrap()
            .is_empty()
    );
}

/// The administrator's view sees what a visitor's must not.
///
/// Two halves of one guarantee: `recent` is the one read meant to see the
/// unchecked and the internal, and `between(.., public_only = true)` must
/// never return either.
#[tokio::test]
async fn an_unchecked_event_reaches_the_administrator_and_nobody_else() {
    let store = store().await;
    let events = store.market_events();

    let note = MarketEvent {
        id: "note:1".into(),
        kind: EventKind::Annotation,
        title: "Herbalism nerfed".into(),
        notes: None,
        starts_at: Millis(10_000),
        ends_at: None,
        scope: EventScope::default(),
        provenance: Provenance::Administrator,
        validation: Validation::Unvalidated,
        visibility: Visibility::Internal,
    };
    events.record(std::slice::from_ref(&note)).await.unwrap();

    assert_eq!(
        events.recent(10).await.unwrap().len(),
        1,
        "the reviewer sees it"
    );
    assert!(
        events
            .between(Millis(0), Millis(20_000), true)
            .await
            .unwrap()
            .is_empty(),
        "and a visitor does not"
    );

    // Published: validated *and* public, in one call, because they are one
    // decision and the halves in the wrong order would allow the state that
    // must never exist.
    assert!(
        events
            .review("note:1", Validation::Validated, Visibility::Public)
            .await
            .unwrap()
    );
    let public = events
        .between(Millis(0), Millis(20_000), true)
        .await
        .unwrap();
    assert_eq!(public.len(), 1);
    assert!(public[0].is_public());

    // Retracted, and gone from the public read again.
    events
        .review("note:1", Validation::Unvalidated, Visibility::Internal)
        .await
        .unwrap();
    assert!(
        events
            .between(Millis(0), Millis(20_000), true)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Only an administrator's own note can be forgotten.
///
/// A catalogue event is re-derived from `catalogs.json` at every start, so
/// deleting one would delete it until the next restart put it back -- a button
/// that appears not to work. The filter is in the statement rather than in a
/// check before it, so there is no path around it.
#[tokio::test]
async fn a_catalogue_event_cannot_be_forgotten() {
    let store = store().await;
    let events = store.market_events();

    let shipped = MarketEvent {
        id: "patch:midnight:12.1".into(),
        kind: EventKind::PatchRelease,
        title: "12.1 The Curse".into(),
        notes: None,
        starts_at: Millis(1_000),
        ends_at: None,
        scope: EventScope::default(),
        provenance: Provenance::Catalogue,
        validation: Validation::Validated,
        visibility: Visibility::Public,
    };
    let typed = MarketEvent {
        id: "note:2".into(),
        provenance: Provenance::Administrator,
        ..shipped.clone()
    };
    events.record(&[shipped, typed]).await.unwrap();

    assert!(
        !events.forget("patch:midnight:12.1").await.unwrap(),
        "the catalogue's own event stays"
    );
    assert!(events.forget("note:2").await.unwrap(), "the typed one goes");
    assert_eq!(events.recent(10).await.unwrap().len(), 1);
}
