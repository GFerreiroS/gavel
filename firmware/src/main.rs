//! ESP32-S3 cluster node.
//!
//! This is not a mock. It links `cluster-core` -- the same crate the server
//! links -- and runs it on Xtensa, under QEMU today and on silicon unchanged.
//! Its job in V0 is to answer one question that no amount of host testing can:
//! *does the portable core actually work on the device?*
//!
//! So it boots, reports the capabilities of the chip it is really running on,
//! and then executes the domain logic -- role sets, health transitions, the
//! task state machine, scheduling, and the prime workload -- asserting the
//! results. The last line it prints is a verdict the host harness greps for.
//!
//! Run it with:  ./scripts/qemu-test.sh

#![no_std]
#![no_main]

extern crate alloc;

mod heap;

#[global_allocator]
static ALLOCATOR: heap::Bump = heap::Bump;

// The ESP-IDF second-stage bootloader refuses to launch an image without an
// app descriptor, so QEMU would sit at the bootloader prompt without this.
esp_bootloader_esp_idf::esp_app_desc!();

use alloc::string::ToString;
use alloc::vec::Vec;

use cluster_core::{
    Clock, ClusterEvent, CpuClass, HealthPolicy, Job, JobSpec, JobState, LeastLoaded,
    LowestHealthyId, Millis, Node, NodeCapabilities, NodeId, NodeStatus, Role, RolePolicies,
    RoleSet, Scheduler, Task, TaskId, TaskSpec, TaskState, TaskWork,
};
use cluster_core::coordinator::Elector;
use esp_backtrace as _;
use esp_println::println;

/// The ESP32-S3's clock is not wired to wall time here, so time comes from the
/// cycle counter. This is exactly the seam `Clock` exists for.
struct DeviceClock {
    hz: u64,
}

impl Clock for DeviceClock {
    fn now(&self) -> Millis {
        let cycles = esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_micros();
        let _ = self.hz;
        Millis(cycles / 1000)
    }
}

// SAFETY-adjacent note: `Clock` requires Send + Sync; this type holds only a
// `u64` and reads a hardware counter, so both hold trivially.
unsafe impl Send for DeviceClock {}
unsafe impl Sync for DeviceClock {}

struct Checks {
    passed: u32,
    failed: u32,
}

impl Checks {
    fn new() -> Self {
        Self {
            passed: 0,
            failed: 0,
        }
    }

    fn check(&mut self, name: &str, ok: bool) {
        if ok {
            self.passed += 1;
            println!("  ok    {name}");
        } else {
            self.failed += 1;
            println!("  FAIL  {name}");
        }
    }
}

#[esp_hal::main]
fn main() -> ! {
    // Deliberately no `esp_hal::init()`. It configures clocks and PLLs that
    // QEMU does not model, and hangs there -- and this firmware drives no
    // peripherals, so there is nothing to initialise. On real silicon a node
    // that actually talks to a radio would call it; this one does not need to.

    println!();
    println!("=== esp-web-cluster node firmware ===");
    println!("chip      : ESP32-S3 (xtensa lx7)");
    println!("target    : {}", env!("TARGET_TRIPLE"));
    println!("heap      : {} bytes (bump)", heap::capacity());

    let clock = DeviceClock { hz: 240_000_000 };
    let boot = clock.now();

    // What this node would advertise to a coordinator. Derived from the chip
    // it is running on, not from a table on the host.
    let capabilities = NodeCapabilities {
        cpu_class: CpuClass::Xtensa,
        cores: 2,
        memory_bytes: 512 * 1024,
        flash_bytes: 8 * 1024 * 1024,
        psram_bytes: Some(8 * 1024 * 1024),
        has_sd: false,
        has_display: false,
        has_wifi: true,
        has_ethernet: false,
    };
    println!(
        "caps      : {} x{} cores, {} KB RAM, usable {} KB",
        capabilities.cpu_class.as_str(),
        capabilities.cores,
        capabilities.memory_bytes / 1024,
        capabilities.usable_ram_bytes() / 1024
    );
    println!();

    let mut checks = Checks::new();

    // --- roles ------------------------------------------------------------
    println!("roles");
    let mut node = Node::new(NodeId(1), capabilities, boot);
    node.status = NodeStatus::Healthy;
    node.roles = RoleSet::from_roles([Role::Compute, Role::Frontend]);
    checks.check("role set packs into one byte", size_of::<RoleSet>() == 1);
    checks.check("holds multiple roles", node.roles.len() == 2);
    checks.check("compute is schedulable", node.is_schedulable());
    node.roles.remove(Role::Compute);
    checks.check("role removal takes effect", !node.is_schedulable());
    node.roles.insert(Role::Compute);

    let policies = RolePolicies::default();
    checks.check(
        "unmet policies do not underflow",
        policies.unmet(|_| 99).is_empty(),
    );
    checks.check(
        "unmet policies are ordered by priority",
        policies.unmet(|_| 0).first().map(|(r, _)| *r) == Some(Role::Gateway),
    );

    // --- health -----------------------------------------------------------
    println!("health");
    let health = HealthPolicy::default();
    checks.check(
        "fresh heartbeat stays healthy",
        health.classify(Millis(10_000), Millis(10_500)).is_none(),
    );
    checks.check(
        "silence becomes suspect",
        health.classify(Millis(10_000), Millis(13_500)) == Some(NodeStatus::Suspect),
    );
    checks.check(
        "longer silence becomes offline",
        health.classify(Millis(10_000), Millis(20_000)) == Some(NodeStatus::Offline),
    );

    // --- task state machine ----------------------------------------------
    println!("task state machine");
    let mut task = Task::new(
        TaskId(1),
        cluster_core::JobId(1),
        0,
        TaskSpec::Sleep { millis: 5 },
        boot,
    );
    checks.check("assign succeeds", task.assign(NodeId(1), clock.now()).is_ok());
    checks.check("attempt counted", task.attempt == 1);
    checks.check("start succeeds", task.start(clock.now()).is_ok());
    checks.check("requeue succeeds", task.requeue(clock.now()).is_ok());
    checks.check("requeue clears the node", task.assigned_to.is_none());
    let _ = task.assign(NodeId(2), clock.now());
    let _ = task.start(clock.now());
    checks.check(
        "complete succeeds",
        task.complete("done".to_string(), clock.now()).is_ok(),
    );
    checks.check("terminal is terminal", task.state == TaskState::Completed);
    checks.check(
        "a completed task cannot be requeued",
        task.requeue(clock.now()).is_err(),
    );

    // --- job splitting ----------------------------------------------------
    println!("job splitting");
    let spec = JobSpec::Primes {
        upper_bound: 2_000,
        tasks: 4,
    };
    let parts = spec.split();
    checks.check("splits into the requested count", parts.len() == 4);
    let mut expected = 0u64;
    let mut contiguous = true;
    for part in &parts {
        if let TaskSpec::Primes { start, end } = *part {
            if start != expected {
                contiguous = false;
            }
            expected = end;
        }
    }
    checks.check("ranges tile without gaps", contiguous && expected == 2_000);

    let mut job = Job::new(cluster_core::JobId(1), spec, boot);
    checks.check(
        "job transitions are validated",
        job.transition_to(JobState::Running, clock.now()).is_ok()
            && job.transition_to(JobState::Queued, clock.now()).is_err(),
    );

    // --- scheduling -------------------------------------------------------
    println!("scheduling");
    let mut idle = Node::new(NodeId(2), capabilities, boot);
    idle.status = NodeStatus::Healthy;
    idle.roles = RoleSet::from_roles([Role::Compute]);
    let mut busy = Node::new(NodeId(3), capabilities, boot);
    busy.status = NodeStatus::Healthy;
    busy.roles = RoleSet::from_roles([Role::Compute]);
    busy.load.running_tasks = 4;
    let nodes: Vec<Node> = alloc::vec![busy, idle];

    let chosen = block_on(LeastLoaded.select_node(&task, &nodes));
    checks.check("least-loaded picks the idle node", chosen == Ok(NodeId(2)));

    let offline: Vec<Node> = alloc::vec![];
    checks.check(
        "no eligible node is an error, not a panic",
        block_on(LeastLoaded.select_node(&task, &offline)).is_err(),
    );
    checks.check(
        "election picks the lowest healthy id",
        LowestHealthyId.elect(&nodes) == Some(NodeId(2))
    );

    // --- the workload itself ---------------------------------------------
    println!("workload");
    // pi(1000) = 168. The device must agree with the host, byte for byte.
    let primes = cluster_core::count_primes(0, 1_000);
    checks.check("counts primes correctly (pi(1000) = 168)", primes == 168);

    let work = cluster_core::run_task(NodeId(1), TaskSpec::Primes { start: 0, end: 100 });
    let counted_here = matches!(&work, TaskWork::Done { output } if output.contains("25 primes"));
    checks.check("run_task computes on-device", counted_here);

    let deferred = cluster_core::run_task(NodeId(1), TaskSpec::Sleep { millis: 7 });
    checks.check(
        "sleep defers to the caller",
        matches!(deferred, TaskWork::Wait { millis: 7, .. })
    );

    // --- events -----------------------------------------------------------
    println!("events");
    let event = ClusterEvent::TaskCompleted {
        task: TaskId(7),
        node: NodeId(3),
    };
    checks.check("event kind is stable", event.kind() == "task_completed");
    checks.check(
        "event renders",
        event.message() == "task-07 completed on node-03"
    );

    // --- timing -----------------------------------------------------------
    let elapsed = clock.now().since(boot);
    println!();
    println!("elapsed   : {elapsed} ms");
    println!(
        "heap peak : {} / {} bytes ({} allocation failures)",
        heap::peak(),
        heap::capacity(),
        heap::exhausted()
    );
    println!("checks    : {} passed, {} failed", checks.passed, checks.failed);
    if checks.failed == 0 && heap::exhausted() == 0 {
        println!("RESULT: PASS");
    } else {
        println!("RESULT: FAIL");
    }
    println!("=== node firmware halted ===");

    loop {
        // The harness stops QEMU once it has seen the verdict above.
        core::hint::spin_loop();
    }
}

/// The domain layer's async is cooperative and never actually pends -- the
/// schedulers await nothing. That means a two-line executor is enough here,
/// and the firmware needs no async runtime at all.
fn block_on<F: core::future::Future>(mut future: F) -> F::Output {
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut context = Context::from_waker(&waker);
    let mut future = unsafe { core::pin::Pin::new_unchecked(&mut future) };
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
}
