//! Materialisation as cluster work.
//!
//! CLAUDE.md §16, Phase 4: expensive calculation can use another machine
//! without giving it the database. Everything here is the same whichever way a
//! partition goes -- `cargo run` has to stay the whole story (§2), so the
//! local path is the one that was written first and stays, and the wire is a
//! transport under it rather than a second implementation beside it.
//!
//! ## The shape
//!
//! A candidate version's work is cut into **partitions**, each a bounded slice
//! of one region's price history. The partition is registered here; the task
//! that computes it carries only `(version, algorithm, partition)`. That
//! triple is §15's idempotency key and it is the entire task spec, which is
//! what keeps `TaskSpec` inside its own documented budget of "a few dozen
//! bytes" and keeps a task row small.
//!
//! ## Why the partition size is what it is
//!
//! Measured, not chosen -- and **measured against the result rather than the
//! input**, which is the correction the wire forced. The first sizing looked
//! only at what a partition costs going out: a commodity market's history
//! postcard-encodes to 913 bytes at the median and 1,520 at the worst, so 64
//! markets was ~81 KB and comfortable. What comes *back* is 4.5 times that,
//! because Phase 6 gave every window a 96-slot chart series and a histogram,
//! and a market has nine windows. At 64 markets a result is 568,469 bytes,
//! twice what a frame carries.
//!
//! The sweep, on the real archive with Phase 7's 515 real ladders attached:
//!
//! | markets | partitions | input worst | result worst |
//! |---:|---:|---:|---:|
//! | 8 | 65 | 18,775 | 73,922 |
//! | **16** | **33** | **35,310** | **145,705** |
//! | 24 | 22 | 52,998 | 218,750 |
//! | 32 | 17 | 69,287 | 290,403 |
//! | 64 | 9 | 126,264 | 568,469 |
//!
//! [`PARTITION_MARKETS`] is 16: the largest size whose *result* leaves room
//! inside `cluster_core::MAX_ARTIFACT`, at 56% of it. 24 would fit at 83%,
//! which is not room -- one more window on a market and it does not. 515 EU
//! markets are 33 tasks rather than 515, which was the other half of the
//! original argument and is still true.
//!
//! Neither bound is a law. A partition that outgrows a frame is refused by the
//! transport and requeued onto an in-process worker, so the cost of this
//! measurement going stale is throughput rather than a publication.
//!
//! ## What the coordinator keeps
//!
//! Everything that touches the database. A worker is handed an input and
//! returns rows; validation, staging and publication stay here, because §15's
//! failure contract requires an incomplete candidate to remain unreachable and
//! that is not a promise a worker can keep about itself.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use app_core::market::materialise::{self, Materialised};
use app_core::market::window::Window;
use app_core::market::{Catalog, ItemId, Ladder, MarketKey, PriceSample, Region};
use cluster_core::{NodeId, TaskSpec, TaskWork, Workload};

/// Markets in one partition. See the module docs for the sweep it comes from:
/// the largest count whose *result* fits a frame with room to spare.
pub const PARTITION_MARKETS: usize = 16;

/// One partition's input: everything needed to materialise its markets, and
/// nothing else.
///
/// Owned rather than borrowed because the point of the exercise is that this
/// can be *sent*: a worker gets the bytes and no access to what produced them.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Input {
    /// Which region's markets these are.
    ///
    /// Not read by the reduction -- the key on every market already carries it
    /// -- and kept because it is what a *sent* artifact is filed under when
    /// somebody is looking at a frame rather than at a struct.
    #[allow(dead_code)]
    pub region: Region,
    /// One entry per market: its key, its history, and the newest ladder.
    pub markets: Vec<(MarketKey, Vec<PriceSample>, Ladder)>,
    /// The windows these markets are summarised over, from the catalogue that
    /// owns them. Carried with the input because a worker has no catalogue.
    pub windows: Vec<Window>,
    /// The slice of catalogue these markets are read against.
    ///
    /// **Trimmed to the partition, and that trim is measured.** The whole
    /// catalogue postcard-encodes to 26,135 bytes -- three quarters of a 35 KB
    /// partition again -- and `materialise::commodity` reads exactly two things
    /// from it: each item's `target`, and the patch and tier dates a window's
    /// bounds come from. Keeping the partition's items plus the patches and
    /// tiers, and dropping the bonus-id maps that only per-realm gear uses,
    /// brings it to about two kilobytes.
    ///
    /// Trimmed rather than resolved into bare numbers so that the worker runs
    /// the same reduction the local path runs, against a catalogue, rather than
    /// a second code path that agrees with it today.
    pub catalog: Arc<Catalog>,
    pub now: cluster_core::Millis,
}

/// The inputs and results of the candidate version being built.
///
/// One per process, held by the composition root, and **the coordinator's**:
/// it hands out a partition's input and takes back its rows. In this process
/// that exchange is a map lookup and an artifact never leaves memory; across a
/// socket the transport fills it from the same two calls, so nothing above it
/// can tell which way a partition went.
///
/// A `--connect` worker has none of this. Its input arrives in the assignment
/// and its result leaves in a frame, which is the whole of what it is allowed
/// to know.
#[derive(Default)]
pub struct Artifacts {
    inner: Mutex<State>,
}

impl std::fmt::Debug for Artifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (done, all) = self.done();
        write!(f, "Artifacts({done}/{all})")
    }
}

#[derive(Default)]
struct State {
    /// The candidate these partitions belong to. A result naming any other
    /// version is stale and is dropped.
    version: u64,
    algorithm: u32,
    inputs: BTreeMap<u32, Arc<Input>>,
    results: BTreeMap<u32, Vec<Materialised>>,
}

impl Artifacts {
    pub fn new() -> Artifacts {
        Artifacts::default()
    }

    /// Register a candidate's partitions, discarding whatever a previous one
    /// left. Returns how many there are.
    ///
    /// Clearing is not tidying: a partition left over from an abandoned
    /// candidate has the same number as one of this candidate's, and publishing
    /// it would be publishing a market nobody recalculated.
    pub fn begin(&self, version: u64, algorithm: u32, inputs: Vec<Input>) -> u16 {
        let mut held = self.lock();
        held.version = version;
        held.algorithm = algorithm;
        held.inputs = inputs
            .into_iter()
            .enumerate()
            .map(|(i, input)| (i as u32, Arc::new(input)))
            .collect();
        held.results.clear();
        held.inputs.len() as u16
    }

    /// What a worker needs for one partition, if it is this candidate's.
    fn registered(&self, version: u64, algorithm: u32, partition: u32) -> Option<Arc<Input>> {
        let held = self.lock();
        if held.version != version || held.algorithm != algorithm {
            return None;
        }
        held.inputs.get(&partition).cloned()
    }

    /// Record what a worker produced.
    ///
    /// **Idempotent by `(version, algorithm, partition)`**, which is §15's
    /// failure contract: a task that is retried after a worker died, or
    /// completed twice because a report was duplicated, writes the same
    /// partition twice and the second write is the same rows as the first. A
    /// result for a version that is no longer the candidate is dropped, which
    /// is the stale-result case in the same contract.
    fn finish(
        &self,
        version: u64,
        algorithm: u32,
        partition: u32,
        rows: Vec<Materialised>,
    ) -> bool {
        let input = match self.registered(version, algorithm, partition) {
            Some(input) => input,
            None => return false,
        };
        // Authentication says which worker sent the bytes, not that it did the
        // assigned work. Recompute at the coordinator boundary and require the
        // exact ordered row set. This simultaneously rejects extra, missing,
        // duplicated and cross-market keys, wrong cardinality, and plausible
        // values manufactured for a valid task/digest.
        let expected: Vec<Materialised> = input
            .markets
            .iter()
            .map(|(key, history, ladder)| {
                materialise::commodity(
                    *key,
                    history,
                    ladder,
                    &input.catalog,
                    &input.windows,
                    input.now,
                )
            })
            .collect();
        if rows != expected {
            tracing::warn!(
                version,
                algorithm,
                partition,
                expected = expected.len(),
                received = rows.len(),
                "rejecting a worker artifact that does not match its registered input"
            );
            return false;
        }
        let mut held = self.lock();
        let same_input = held
            .inputs
            .get(&partition)
            .is_some_and(|current| Arc::ptr_eq(current, &input));
        if held.version != version || held.algorithm != algorithm || !same_input {
            return false;
        }
        held.results.insert(partition, rows);
        true
    }

    /// Every partition's rows, or `None` if any is still missing.
    ///
    /// The completeness check is the coordinator's and it is deliberately
    /// all-or-nothing: §15's third point is that an incomplete candidate stays
    /// unreachable, and "publish what came back" is exactly how it would stop
    /// being.
    pub fn collect(&self, version: u64) -> Option<Vec<Materialised>> {
        let held = self.lock();
        if held.version != version {
            return None;
        }
        if held.results.len() != held.inputs.len() {
            return None;
        }
        Some(held.results.values().flatten().cloned().collect())
    }

    /// How many partitions have come back, for a progress line.
    pub fn done(&self) -> (usize, usize) {
        let held = self.lock();
        (held.results.len(), held.inputs.len())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock means a thread panicked while holding it. The state
        // is plain owned data with no invariant to break halfway, so the
        // recovered value is sound -- and refusing it would wedge every later
        // publication for the sake of tidiness.
        match self.inner.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The handler the composition root installs on every worker.
///
/// Pure and synchronous, like `cluster_core::workload::run_task`: it reads a
/// registered input and reduces it. No database, no network, no clock of its
/// own -- which is what makes "the same code runs in every worker" true, and
/// what will let the second half of this phase move it to another machine
/// without changing a statistic.
#[derive(Clone, Copy)]
pub struct MarketWorkload;

impl std::fmt::Debug for MarketWorkload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MarketWorkload")
    }
}

impl MarketWorkload {
    /// Stateless, and that is the property rather than an accident: everything
    /// it needs arrives in the artifact. A handler that held the coordinator's
    /// store would be one that could not be installed on another machine,
    /// which is the whole of what this phase is for.
    pub fn new() -> MarketWorkload {
        MarketWorkload
    }
}

impl Workload for MarketWorkload {
    /// Bytes in, bytes out. No database, no clock, no catalogue of its own --
    /// everything it needs came in the artifact, which is what lets this run on
    /// a machine that has none of those.
    fn run(&self, node: NodeId, spec: TaskSpec, input: &[u8]) -> Option<TaskWork> {
        let TaskSpec::Analysis { partition, .. } = spec else {
            return None;
        };
        if input.is_empty() {
            // The candidate moved on while this task was queued, so nothing was
            // handed over. Done-with-nothing rather than a failure: the task is
            // not broken, it is obsolete, and retrying it would say the same.
            return Some(TaskWork::Done {
                output: format!("partition {partition} is stale"),
            });
        }
        let input: Input = match postcard::from_bytes(input) {
            Ok(input) => input,
            Err(error) => {
                return Some(TaskWork::Done {
                    output: format!("partition {partition} could not be read: {error}"),
                });
            }
        };

        let rows: Vec<Materialised> = input
            .markets
            .iter()
            .map(|(key, history, ladder)| {
                materialise::commodity(
                    *key,
                    history,
                    ladder,
                    &input.catalog,
                    &input.windows,
                    input.now,
                )
            })
            .collect();

        let markets = rows.len();
        let artifact = postcard::to_allocvec(&rows).ok()?;
        Some(TaskWork::Produced {
            output: format!("{markets} markets in partition {partition} on {node}"),
            artifact,
        })
    }
}

/// The coordinator's side: hand out inputs, take back results.
impl cluster_core::ArtifactStore for Artifacts {
    fn input(&self, spec: TaskSpec) -> Option<Vec<u8>> {
        let TaskSpec::Analysis {
            version,
            algorithm,
            partition,
        } = spec
        else {
            return None;
        };
        let input = self.registered(version, algorithm, partition)?;
        postcard::to_allocvec(input.as_ref()).ok()
    }

    fn produced(&self, spec: TaskSpec, bytes: &[u8]) {
        let TaskSpec::Analysis {
            version,
            algorithm,
            partition,
        } = spec
        else {
            return;
        };
        match postcard::from_bytes::<Vec<Materialised>>(bytes) {
            Ok(rows) => {
                self.finish(version, algorithm, partition, rows);
            }
            // Decoded here rather than trusted: an artifact that passed its
            // integrity check can still be the wrong *shape* if two builds
            // disagree, and a partition that silently became empty would be a
            // published version with a hole in it.
            Err(error) => tracing::warn!(
                version, partition, %error,
                "a partition's result could not be decoded; it stays missing"
            ),
        }
    }
}

/// Cut a region's history into partitions of [`PARTITION_MARKETS`] markets.
///
/// Grouping is a walk: the rows arrive ordered by item, which is the same
/// property the in-process materialiser relies on.
pub fn partition(
    region: Region,
    history: &[PriceSample],
    ladders: &BTreeMap<ItemId, Ladder>,
    owner: impl Fn(ItemId) -> Arc<Catalog>,
    windows: impl Fn(&Catalog) -> Vec<Window>,
    now: cluster_core::Millis,
) -> Vec<Input> {
    let no_ladder = Ladder::default();
    let mut out: Vec<Input> = Vec::new();
    let mut current: Vec<(MarketKey, Vec<PriceSample>, Ladder)> = Vec::new();
    let mut catalog: Option<Arc<Catalog>> = None;

    for group in grouped(history) {
        let owner = owner(group[0].item);
        // A partition holds one catalogue's markets: the input carries the
        // catalogue, and mixing two would mean carrying both.
        let same = catalog.as_ref().is_some_and(|held| held.id == owner.id);
        if (!same && !current.is_empty()) || current.len() >= PARTITION_MARKETS {
            let held = catalog.take().expect("a partition has a catalogue");
            out.push(finish(
                region,
                &held,
                std::mem::take(&mut current),
                &windows,
                now,
            ));
        }
        let key = owner.market_of(&group[0]);
        let ladder = ladders.get(&key.item()).unwrap_or(&no_ladder).clone();
        current.push((key, group.to_vec(), ladder));
        catalog = Some(owner);
    }
    if let Some(held) = catalog {
        out.push(finish(region, &held, current, &windows, now));
    }
    out
}

/// Seal one partition, carrying only the catalogue it needs.
fn finish(
    region: Region,
    catalog: &Catalog,
    markets: Vec<(MarketKey, Vec<PriceSample>, Ladder)>,
    windows: &impl Fn(&Catalog) -> Vec<Window>,
    now: cluster_core::Millis,
) -> Input {
    let items: Vec<ItemId> = markets.iter().map(|(key, _, _)| key.item()).collect();
    Input {
        region,
        windows: windows(catalog),
        markets,
        catalog: Arc::new(trim(catalog, &items)),
        now,
    }
}

/// The slice of a catalogue a commodity partition is read against.
///
/// `materialise::commodity` reads exactly two things from a catalogue: each
/// item's `target`, and the patch and tier dates a window's bounds come from.
/// So a partition needs its own items, every patch and every tier -- and none
/// of the bonus-id maps, which only per-realm gear consults.
///
/// Measured: 26,135 bytes whole, about two thousand trimmed. On a partition of
/// 16 markets that is the difference between doubling it and rounding it.
fn trim(catalog: &Catalog, items: &[ItemId]) -> Catalog {
    let mut cut = catalog.clone();
    cut.items
        .retain(|entry| entry.item_ids().any(|id| items.contains(&id)));
    // Gear's bonus-id vocabulary. A commodity market has no track and no
    // upgrade level, so carrying these would be carrying the catalogue's
    // bulkiest maps to be ignored.
    cut.item_levels.clear();
    cut.tracks.clear();
    cut.modifiers.clear();
    // Notes are administrator-only research (§8). A worker has no business
    // holding them, and they are the one field here that could carry something
    // unpublished onto another machine.
    cut.notes.clear();
    cut
}

/// Split a region's history into one slice per market.
fn grouped(history: &[PriceSample]) -> impl Iterator<Item = &[PriceSample]> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= history.len() {
            return None;
        }
        let item = history[start].item;
        let mut end = start;
        while end < history.len() && history[end].item == item {
            end += 1;
        }
        let group = &history[start..end];
        start = end;
        Some(group)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::market::{CatalogSet, Copper, Region};
    use cluster_core::{Millis, SupervisorMessage, WireTaskSpec};

    const AT: Millis = Millis(1_767_225_600_000);
    const HOUR: u64 = 60 * 60 * 1000;

    fn catalog() -> Arc<Catalog> {
        Arc::new(
            CatalogSet::embedded()
                .shipped_active()
                .expect("an active catalogue")
                .clone(),
        )
    }

    /// A history over the catalogue's own commodity ids, so the keys and ranks
    /// are the real ones rather than a shape invented for the test.
    fn history(markets: usize, samples: usize) -> Vec<PriceSample> {
        let catalog = catalog();
        let ids: Vec<ItemId> = catalog.tracked_ids().into_iter().take(markets).collect();
        let mut out = Vec::new();
        for (m, item) in ids.iter().enumerate() {
            for s in 0..samples {
                out.push(PriceSample {
                    item: *item,
                    region: Region::Eu,
                    observed_at: Millis(AT.get() + (s as u64) * HOUR),
                    min_unit_price: Copper(1_000 + (m * 7 + s * 13) as u64),
                    p05_unit_price: Copper(1_100 + (m * 7 + s * 13) as u64),
                    median_unit_price: Copper(1_500 + (m * 11 + s * 3) as u64),
                    quantity: 100 + (s as u64),
                    listings: 4 + (s as u32),
                });
            }
        }
        out
    }

    fn cut(history: &[PriceSample]) -> Vec<Input> {
        cut_with(history, &BTreeMap::new())
    }

    fn cut_with(history: &[PriceSample], ladders: &BTreeMap<ItemId, Ladder>) -> Vec<Input> {
        let catalog = catalog();
        let windows = Window::all_for(&catalog);
        partition(
            Region::Eu,
            history,
            ladders,
            |_| catalog.clone(),
            |_| windows.clone(),
            AT,
        )
    }

    /// A ladder of the shape the fattest market on the real archive has: 322
    /// rungs, which is the maximum across the 515 real ladders Phase 7
    /// collected. Used where the point is what a partition *weighs*.
    fn fat_ladder() -> Ladder {
        let listings: Vec<app_core::market::Listing> = (0..322u64)
            .map(|i| app_core::market::Listing {
                item: ItemId(1),
                unit_price: Copper(1_000 + i * 37),
                quantity: 40 + i * 3,
            })
            .collect();
        Ladder::of(&listings)
    }

    /// Run every registered partition **the way a worker does**: fetch the
    /// input as bytes, compute, hand the result back as bytes.
    ///
    /// Through the two ports rather than into the store directly, so the
    /// serialisation both ends of the wire depend on is exercised by every
    /// test below rather than by one of them.
    fn run_all(artifacts: &Arc<Artifacts>, version: u64, partitions: u16) {
        use cluster_core::ArtifactStore;
        let workload = MarketWorkload::new();
        for partition in 0..partitions as u32 {
            let spec = TaskSpec::Analysis {
                version,
                algorithm: 7,
                partition,
            };
            let input = artifacts.input(spec).expect("a registered partition");
            match workload.run(NodeId(1), spec, &input) {
                Some(TaskWork::Produced { artifact, .. }) => artifacts.produced(spec, &artifact),
                other => panic!("expected an artifact, got {other:?}"),
            }
        }
    }

    /// **Phase 4's exit gate, first leg.** The same fixture must produce
    /// field-equivalent analysis whether one process reduced it or the work was
    /// cut into partitions and handed out.
    ///
    /// Asserted against `materialise::commodity` called directly, not against a
    /// previous run of the same path -- otherwise it would only prove the
    /// partitioning is deterministic, which is a much weaker claim than that it
    /// changed nothing.
    #[test]
    fn partitioned_work_is_the_same_work() {
        let history = history(200, 6);
        let catalog = catalog();
        let windows = Window::all_for(&catalog);

        let direct: Vec<Materialised> = grouped(&history)
            .map(|group| {
                let key = catalog.market_of(&group[0]);
                materialise::commodity(key, group, &Ladder::default(), &catalog, &windows, AT)
            })
            .collect();

        let artifacts = Arc::new(Artifacts::new());
        let inputs = cut(&history);
        assert!(inputs.len() > 1, "200 markets is more than one partition");
        let partitions = artifacts.begin(9, 7, inputs);
        run_all(&artifacts, 9, partitions);
        let distributed = artifacts.collect(9).expect("every partition came back");

        assert_eq!(distributed.len(), direct.len());
        for (a, b) in distributed.iter().zip(direct.iter()) {
            assert_eq!(a.state, b.state, "market {:?}", a.state.key);
            assert_eq!(a.windows, b.windows, "market {:?}", a.state.key);
        }
    }

    /// **Phase 4's exit gate, second leg.** The same fixture produces the same
    /// rows whether the partition stayed in this process or went through the
    /// wire's own encoder, frame and decoder.
    ///
    /// Through `encode_frame` and `decode_frame` rather than beside them: the
    /// claim is about what a *worker* computes from what a *coordinator* sent,
    /// and postcard is where a type that round-trips wrongly would show up.
    /// What this does not carry is the socket, which
    /// `cluster-local/tests/remote.rs` holds -- between them the path is
    /// covered end to end.
    #[test]
    fn a_partition_that_crossed_the_wire_is_the_same_partition() {
        // The algorithm `run_all` files its work under.
        const ALGO: u32 = 7;
        let history = history(40, 12);
        let ladders: BTreeMap<ItemId, Ladder> = catalog()
            .tracked_ids()
            .into_iter()
            .take(40)
            .map(|id| (id, fat_ladder()))
            .collect();

        let here = Arc::new(Artifacts::new());
        let partitions = here.begin(4, ALGO, cut_with(&history, &ladders));
        run_all(&here, 4, partitions);
        let local = here.collect(4).expect("every partition came back");

        // The same candidate again, with every byte put through the wire.
        let there = Arc::new(Artifacts::new());
        assert_eq!(
            there.begin(4, ALGO, cut_with(&history, &ladders)),
            partitions
        );
        let worker = MarketWorkload::new();
        for partition in 0..partitions as u32 {
            let spec = TaskSpec::Analysis {
                version: 4,
                algorithm: ALGO,
                partition,
            };

            // Coordinator -> worker: fetch, frame, send.
            let input = cluster_core::ArtifactStore::input(there.as_ref(), spec)
                .expect("a registered partition");
            let assignment = SupervisorMessage::Assign {
                task: cluster_core::TaskId(partition as u64),
                spec: WireTaskSpec::of(spec, Some(input)).expect("shippable"),
            };
            let mut frame = Vec::new();
            cluster_core::encode_frame(&assignment, &mut frame).expect("encode");
            let len =
                cluster_core::frame_len(frame[..cluster_core::LENGTH_PREFIX].try_into().unwrap())
                    .expect("length");
            assert_eq!(len, frame.len() - cluster_core::LENGTH_PREFIX);
            let SupervisorMessage::Assign { spec: wire, .. } =
                cluster_core::decode_frame::<SupervisorMessage>(
                    &frame[cluster_core::LENGTH_PREFIX..],
                )
                .expect("decode")
            else {
                panic!("an assignment decodes as one");
            };

            // The worker: nothing but the bytes it was handed.
            let produced = worker
                .run(
                    NodeId(1),
                    TaskSpec::from(&wire),
                    wire.input().expect("intact"),
                )
                .expect("the handler ran it");
            let TaskWork::Produced { artifact, .. } = produced else {
                panic!("expected an artifact, got {produced:?}");
            };

            // Worker -> coordinator: frame, send, verify, take.
            let mut frame = Vec::new();
            cluster_core::encode_frame(
                &cluster_core::NodeMessage::TaskProduced {
                    task: cluster_core::TaskId(partition as u64),
                    artifact: cluster_core::Artifact::new(artifact),
                },
                &mut frame,
            )
            .expect("a result of the measured size fits a frame");
            let cluster_core::NodeMessage::TaskProduced { artifact, .. } =
                cluster_core::decode_frame::<cluster_core::NodeMessage>(
                    &frame[cluster_core::LENGTH_PREFIX..],
                )
                .expect("decode")
            else {
                panic!("a result decodes as one");
            };
            cluster_core::ArtifactStore::produced(
                there.as_ref(),
                spec,
                artifact.verify().expect("the digest survived the journey"),
            );
        }

        let remote = there.collect(4).expect("every partition came back");
        assert_eq!(
            local, remote,
            "the wire changed nothing about what was materialised"
        );
    }

    /// The partition size is a measurement, and this is what stops it going
    /// stale silently: both frames a partition costs must fit inside
    /// `cluster_core::MAX_ARTIFACT`, with the worst ladder the real archive
    /// has attached.
    ///
    /// A window added to the analysis is what would break it, and the sweep in
    /// the module docs is what to redo when it does.
    #[test]
    fn a_partition_fits_a_frame_in_both_directions() {
        // 76 hourly observations is the depth of the real archive; the ladder
        // is its fattest market.
        let history = history(PARTITION_MARKETS, 76);
        let ladders: BTreeMap<ItemId, Ladder> = catalog()
            .tracked_ids()
            .into_iter()
            .take(PARTITION_MARKETS)
            .map(|id| (id, fat_ladder()))
            .collect();
        let inputs = cut_with(&history, &ladders);
        assert_eq!(inputs.len(), 1, "that is one partition's worth");

        let encoded = postcard::to_allocvec(&inputs[0]).expect("encode");
        assert!(
            encoded.len() <= cluster_core::MAX_ARTIFACT,
            "a partition's input is {} bytes",
            encoded.len()
        );

        let rows: Vec<Materialised> = inputs[0]
            .markets
            .iter()
            .map(|(key, history, ladder)| {
                materialise::commodity(
                    *key,
                    history,
                    ladder,
                    &inputs[0].catalog,
                    &inputs[0].windows,
                    AT,
                )
            })
            .collect();
        let result = postcard::to_allocvec(&rows).expect("encode");
        assert!(
            result.len() <= cluster_core::MAX_ARTIFACT,
            "a partition's result is {} bytes, and it is the bigger half",
            result.len()
        );
    }

    /// A partition is sized by measured payload. 16 markets each, so 200 is
    /// thirteen partitions and the last one is short rather than dropped.
    #[test]
    fn partitions_are_the_measured_size_and_nothing_is_lost() {
        let history = history(200, 3);
        let inputs = cut(&history);
        assert_eq!(inputs.len(), 200_usize.div_ceil(PARTITION_MARKETS));
        assert_eq!(inputs.iter().map(|i| i.markets.len()).sum::<usize>(), 200);
        assert!(inputs.iter().all(|i| i.markets.len() <= PARTITION_MARKETS));
    }

    /// §15's failure contract: a retried or duplicated result writes the same
    /// partition twice, and the second write says the same thing as the first.
    #[test]
    fn a_partition_run_twice_lands_the_same_rows() {
        let history = history(70, 4);
        let artifacts = Arc::new(Artifacts::new());
        let partitions = artifacts.begin(3, 7, cut(&history));
        run_all(&artifacts, 3, partitions);
        let once = artifacts.collect(3).expect("complete");

        // A worker that died after reporting, and the requeued attempt.
        run_all(&artifacts, 3, partitions);
        let twice = artifacts.collect(3).expect("still complete");
        assert_eq!(once.len(), twice.len());
        assert_eq!(once, twice);
    }

    #[test]
    fn semantically_forged_worker_artifacts_are_rejected_even_with_valid_bytes() {
        use cluster_core::ArtifactStore;

        let history = history(4, 4);
        let inputs = cut(&history);
        let input = &inputs[0];
        let expected: Vec<Materialised> = input
            .markets
            .iter()
            .map(|(key, history, ladder)| {
                materialise::commodity(
                    *key,
                    history,
                    ladder,
                    &input.catalog,
                    &input.windows,
                    input.now,
                )
            })
            .collect();

        let mut nonexistent_key = expected.clone();
        nonexistent_key[0].state.key = MarketKey::commodity(Region::Eu, ItemId(4_000_000), 1);
        let mut other_market = expected.clone();
        other_market[0].state.key =
            MarketKey::commodity(Region::Us, other_market[0].state.key.item(), 1);
        let mut duplicate_key = expected.clone();
        duplicate_key[1] = duplicate_key[0].clone();
        let mut reordered = expected.clone();
        reordered.swap(0, 1);
        let foreign_history = self::history(4, 5);
        let foreign_inputs = cut(&foreign_history);
        let foreign_content: Vec<Materialised> = foreign_inputs[0]
            .markets
            .iter()
            .map(|(key, history, ladder)| {
                materialise::commodity(
                    *key,
                    history,
                    ladder,
                    &input.catalog,
                    &input.windows,
                    input.now,
                )
            })
            .collect();
        let cases = [
            (
                "missing/cardinality",
                expected[..expected.len() - 1].to_vec(),
            ),
            ("extra/cardinality", {
                let mut rows = expected.clone();
                rows.push(expected[0].clone());
                rows
            }),
            ("nonexistent key", nonexistent_key),
            ("foreign region/market", other_market),
            ("duplicate key", duplicate_key),
            ("reordered partition", reordered),
            (
                "valid task id with another input's content",
                foreign_content,
            ),
        ];

        for (case, forged) in cases {
            let artifacts = Artifacts::new();
            artifacts.begin(8, 7, cut(&history));
            let artifact = cluster_core::Artifact::new(postcard::to_allocvec(&forged).unwrap());
            assert!(
                artifact.verify().is_some(),
                "the manipulated {case} has a valid digest"
            );
            artifacts.produced(
                TaskSpec::Analysis {
                    version: 8,
                    algorithm: 7,
                    partition: 0,
                },
                &artifact.bytes,
            );
            assert_eq!(artifacts.done().0, 0, "forged {case} must stay missing");
        }

        let artifacts = Artifacts::new();
        artifacts.begin(8, 7, cut(&history));
        let bytes = postcard::to_allocvec(&expected).unwrap();
        artifacts.produced(
            TaskSpec::Analysis {
                version: 8,
                algorithm: 7,
                partition: 99,
            },
            &bytes,
        );
        assert_eq!(artifacts.done().0, 0, "foreign partition must be rejected");
    }

    /// A result for a candidate that has been abandoned is dropped rather than
    /// mixed into the one being built. Same contract, the stale-version leg.
    #[test]
    fn a_result_for_an_obsolete_version_is_refused() {
        let history = history(10, 3);
        let artifacts = Arc::new(Artifacts::new());
        artifacts.begin(1, 7, cut(&history));

        // A straggler from the previous candidate reports late.
        let workload = MarketWorkload::new();
        let stale = TaskSpec::Analysis {
            version: 0,
            algorithm: 7,
            partition: 0,
        };
        // The coordinator has nothing to hand over for it, which is how a
        // stale assignment stops before it is sent rather than after it is
        // computed.
        assert!(
            cluster_core::ArtifactStore::input(artifacts.as_ref(), stale).is_none(),
            "an obsolete partition has no input to send"
        );
        // And a worker handed nothing says so rather than computing from it.
        match workload
            .run(NodeId(1), stale, &[])
            .expect("the handler answers")
        {
            TaskWork::Done { output } => assert!(output.contains("stale"), "{output}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(artifacts.done(), (0, 1), "nothing was recorded for it");

        // And a *result* filed against the wrong algorithm is refused too.
        assert!(!artifacts.finish(1, 999, 0, Vec::new()));
        assert_eq!(artifacts.done(), (0, 1));
    }

    /// §15, point three: an incomplete candidate stays unreachable. Collecting
    /// is all-or-nothing, so "publish what came back" is not a thing the
    /// coordinator can do by accident.
    #[test]
    fn an_incomplete_candidate_yields_nothing() {
        let history = history(150, 3);
        let artifacts = Arc::new(Artifacts::new());
        let partitions = artifacts.begin(5, 7, cut(&history));
        assert!(partitions >= 3);

        use cluster_core::ArtifactStore;
        let workload = MarketWorkload::new();
        for partition in 0..partitions as u32 - 1 {
            let spec = TaskSpec::Analysis {
                version: 5,
                algorithm: 7,
                partition,
            };
            let input = artifacts.input(spec).expect("registered");
            if let Some(TaskWork::Produced { artifact, .. }) = workload.run(NodeId(1), spec, &input)
            {
                artifacts.produced(spec, &artifact);
            }
        }
        assert!(
            artifacts.collect(5).is_none(),
            "one partition short is not a version"
        );

        run_all(&artifacts, 5, partitions);
        assert!(artifacts.collect(5).is_some());
    }

    /// Opening a candidate clears the last one's results. A partition left
    /// over has the same number as one of this candidate's, and publishing it
    /// would publish a market nobody recalculated.
    #[test]
    fn a_new_candidate_does_not_inherit_the_last_ones_results() {
        let history = history(70, 3);
        let artifacts = Arc::new(Artifacts::new());
        let partitions = artifacts.begin(1, 7, cut(&history));
        run_all(&artifacts, 1, partitions);
        assert!(artifacts.collect(1).is_some());

        artifacts.begin(2, 7, cut(&history));
        assert!(artifacts.collect(2).is_none(), "nothing carried over");
        assert_eq!(artifacts.done().0, 0);
    }

    /// A spec the handler does not recognise is `None`, so the runtime reports
    /// a failure rather than a success with an empty result.
    #[test]
    fn the_handler_declines_what_is_not_its_work() {
        let workload = MarketWorkload::new();
        assert!(
            workload
                .run(NodeId(1), TaskSpec::Primes { start: 0, end: 10 }, &[])
                .is_none()
        );
    }
}
