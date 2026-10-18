use super::*;

fn pool_at(dir: &Path, capacity: u64, bytes_per_permit: u64) -> Pool {
    let mut pool = Pool::new(
        dir.to_path_buf(),
        capacity,
        bytes_per_permit,
        SchedulerPriority::Normal,
    );
    // Tests must not depend on how much memory the machine running them has
    // free; anything that wants the gate closed injects its own probe.
    pool.domain_capacity = capacity;
    pool.available_memory = || None;
    pool.pressure = false;
    pool
}

fn encode_history_map(binary: &str, records: &BTreeMap<u64, TestMeasurement>) -> Vec<u8> {
    let records: Vec<_> = records
        .iter()
        .map(|(hash, measurement)| TestHistoryRecord {
            hash: *hash,
            measurement: *measurement,
        })
        .collect();
    encode_test_history(binary, &records)
}

#[test]
fn capacity_is_enforced_and_released() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 2, 0);

    let first = pool.try_admit(1, None).unwrap().expect("first permit");
    let second = pool.try_admit(1, None).unwrap().expect("second permit");
    assert!(
        pool.try_admit(1, None).unwrap().is_none(),
        "a full pool must refuse"
    );

    drop(first);
    let third = pool.try_admit(1, None).unwrap();
    assert!(third.is_some(), "a released permit frees capacity");
    drop(second);
    drop(third);

    // Not merely unlocked: a released lease leaves no file behind. Windows
    // refuses to delete a file anyone still holds open, so a permit that only
    // unlocked its handle would leave one of these per compilation there --
    // capacity would stay correct and the directory would grow forever.
    let leases: Vec<_> = std::fs::read_dir(directory.path().join(LEASES_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(leases.is_empty(), "released leases are removed: {leases:?}");
}

#[test]
fn leases_never_collide_with_a_name_already_taken() {
    // This test's own counter: the process-wide one is advanced by every test
    // running in parallel, which could hand the stand-in's name to a lease
    // below or skip past it, and then the test would not exercise a collision.
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let directory = tempfile::tempdir().unwrap();
    let mut pool = pool_at(directory.path(), 8, 0);
    pool.lease_nonce = || NONCE.fetch_add(1, Ordering::Relaxed);
    let leases = directory.path().join(LEASES_DIR);

    let held: Vec<_> = (0..4)
        .map(|_| pool.try_admit(1, None).unwrap().expect("permit"))
        .collect();
    let names: std::collections::BTreeSet<_> = std::fs::read_dir(&leases)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 4, "every live lease has its own file");

    // A pid is unique only inside its namespace, so a name that is merely
    // "pid plus counter" collides between two containers sharing a cache --
    // silently truncating a live holder's record. Stood in for by a lease
    // somebody else holds the lock on, since that is what makes the name
    // genuinely taken: an unlocked file of the same name is a dead holder's,
    // and reclaiming it is correct.
    let taken = NONCE.load(Ordering::Relaxed);
    let squatted = leases.join(format!(
        "{}-{}-{taken}",
        std::process::id(),
        process_token()
    ));
    std::fs::write(&squatted, b"another namespace's lease").unwrap();
    let mut elsewhere = fslock::LockFile::open(&squatted).unwrap();
    assert!(
        elsewhere.try_lock().unwrap(),
        "the stand-in holds its lease"
    );

    let next = pool.try_admit(1, None).unwrap().expect("permit");
    assert_eq!(
        NONCE.load(Ordering::Relaxed),
        taken + 2,
        "the grant tried the taken name and moved on to the next"
    );
    assert!(
        leases
            .join(format!(
                "{}-{}-{}",
                std::process::id(),
                process_token(),
                taken + 1
            ))
            .exists(),
        "the successful retry uses the next nonce"
    );
    assert_eq!(
        std::fs::read(&squatted).unwrap(),
        b"another namespace's lease",
        "a name in use is left exactly as it was found"
    );
    drop(next);
    drop(held);
}

#[test]
fn weights_count_against_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 0);

    let heavy = pool.try_admit(3, None).unwrap().expect("heavy permit");
    let light = pool.try_admit(1, None).unwrap().expect("last permit");
    assert!(pool.try_admit(1, None).unwrap().is_none());
    drop(heavy);
    assert!(pool.try_admit(3, None).unwrap().is_some());
    drop(light);
}

#[test]
fn a_reservation_charges_the_larger_of_cpu_and_memory_requests() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 1024);

    let reservation = pool.reserve(Some(1), Some(3 * 1024)).unwrap();
    assert!(
        pool.try_admit(2, None).unwrap().is_none(),
        "three memory permits leave only one"
    );
    drop(reservation);
    assert!(pool.try_admit(4, None).unwrap().is_some());
}

#[test]
fn a_reservation_requires_a_requested_resource() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 1024);

    assert!(pool.reserve(None, None).is_err());
}

#[test]
fn a_memory_reservation_needs_a_memory_budget() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 0);

    assert!(pool.reserve(None, Some(1024)).is_err());
}

#[test]
fn a_waiting_normal_reservation_holds_low_priority_work_back() {
    let directory = tempfile::tempdir().unwrap();
    let normal = pool_at(directory.path(), 1, 0);
    let held = normal.try_admit(1, None).unwrap().expect("first permit");
    let waiting_dir = directory.path().to_path_buf();
    let waiter = std::thread::spawn(move || {
        let pool = pool_at(&waiting_dir, 1, 0);
        pool.reserve(Some(1), None).unwrap()
    });

    for _ in 0..20 {
        if directory.path().join(PRIORITY_WAIT_STAMP).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        directory.path().join(PRIORITY_WAIT_STAMP).exists(),
        "a normal reservation waiting for capacity announces itself"
    );
    drop(held);
    drop(waiter.join().unwrap());
}

#[test]
fn a_build_limit_counts_only_that_builds_leases() {
    let directory = tempfile::tempdir().unwrap();
    let mut other = pool_at(directory.path(), 6, 0);
    other.build = Some(("other".into(), 6));
    let elsewhere = other.try_admit(2, None).unwrap().expect("other build");

    let mut limited = pool_at(directory.path(), 6, 0);
    limited.build = Some(("limited".into(), 2));
    let own = limited
        .try_admit(2, None)
        .unwrap()
        .expect("two permits beside another build");
    assert!(
        limited.try_admit(1, None).unwrap().is_none(),
        "the build cannot exceed its own ceiling even with machine capacity free"
    );

    drop(own);
    assert!(
        limited.try_admit(3, None).unwrap().is_some(),
        "one demand heavier than the build ceiling still makes progress"
    );
    drop(elsewhere);
}

#[test]
fn a_dead_holders_lease_is_reclaimed() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 1, 0);

    // A lease file nobody holds a lock on is what a killed shim leaves behind.
    let leases = directory.path().join(LEASES_DIR);
    std::fs::create_dir_all(&leases).unwrap();
    let stale = leases.join("999999-0");
    std::fs::write(
        &stale,
        serde_json::to_vec(&Lease {
            name: None,
            mode_key: None,
            version: LEASE_VERSION,
            weight: 1,
            priority: "normal".into(),
            build_id: None,
            domain: None,
            bytes_per_permit: 0,
        })
        .unwrap(),
    )
    .unwrap();

    let permit = pool.try_admit(1, None).unwrap();
    assert!(permit.is_some(), "a stale lease must not hold capacity");
    assert!(!stale.exists(), "the stale lease is removed");
}

#[test]
fn a_demand_heavier_than_the_pool_runs_alone_rather_than_never() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 2, 0);

    // Heavier than the whole machine, and gated by a probe that reports no
    // memory at all: it still compiles, alone.
    let mut short = pool_at(directory.path(), 2, 0);
    short.available_memory = || Some(0);
    let permit = short.try_admit(10, Some(u64::MAX)).unwrap();
    assert!(permit.is_some(), "an oversized demand is admitted alone");
    // And while it runs, nothing else fits.
    assert!(pool.try_admit(1, None).unwrap().is_none());
}

#[test]
fn low_priority_leaves_the_reserve_even_on_an_idle_pool() {
    let directory = tempfile::tempdir().unwrap();
    let mut low = Pool::new(directory.path().to_path_buf(), 4, 0, SchedulerPriority::Low);
    low.available_memory = || None;
    std::fs::write(directory.path().join(PRIORITY_WAIT_STAMP), b"").unwrap();

    // An idle pool is the ordinary state between two compilations, so it is
    // no licence to stop yielding: three of the four permits are what a
    // low-priority build may take while somebody is waiting.
    let admitted = low.try_admit(3, None).unwrap();
    assert!(admitted.is_some(), "what fits beside the reserve is taken");
    let leases = std::fs::read_dir(directory.path().join(LEASES_DIR))
        .unwrap()
        .count();
    assert_eq!(leases, 1);
    drop(admitted);
}

#[test]
fn a_weight_the_reserve_would_never_admit_still_runs_alone() {
    let directory = tempfile::tempdir().unwrap();
    let mut low = Pool::new(directory.path().to_path_buf(), 4, 0, SchedulerPriority::Low);
    low.available_memory = || None;
    std::fs::write(directory.path().join(PRIORITY_WAIT_STAMP), b"").unwrap();

    // Four permits out of four never fits beside a reserve of one, and a
    // weight is already clamped to the capacity, so refusing here would not
    // be yielding -- it would be a compilation that never runs at all for as
    // long as anybody keeps waiting. It goes alone instead.
    assert!(
        low.try_admit(4, None).unwrap().is_some(),
        "a demand the reserve excludes outright runs by itself"
    );
}

#[test]
fn a_predicted_heavy_compile_does_not_wait_out_the_gate_on_an_idle_machine() {
    let directory = tempfile::tempdir().unwrap();
    let bytes_per_permit = 1024;
    let mut pool = pool_at(directory.path(), 4, bytes_per_permit);
    // Short of memory, and nothing running that could give any back.
    pool.available_memory = || Some(1);
    // Through `plan`, because that is the only weight production ever asks
    // for -- and it clamps to the capacity, so a rule written against
    // heavier-than-capacity weights would never fire for a real compilation.
    pool.record_peak(&ledger_key("enormous", true), bytes_per_permit * 1000, true)
        .unwrap();
    let (weight, predicted) = pool.plan(&Demand::new("enormous", true));
    assert_eq!(
        weight, 4,
        "a clamped weight is what admission actually sees"
    );

    assert!(
        pool.try_admit(weight, predicted).unwrap().is_some(),
        "an idle machine admits it rather than waiting out the gate deadline"
    );
}

#[test]
fn the_reserve_never_swallows_the_whole_pool() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(PRIORITY_WAIT_STAMP), b"").unwrap();

    // A quarter of one permit rounds up to one, which would leave a
    // single-permit machine reserving everything -- and then the only way a
    // low-priority build could ever run would be the oversized-demand escape,
    // which hands it the whole machine at once. Yielding must not invert.
    for capacity in 1..=8 {
        let mut low = Pool::new(
            directory.path().to_path_buf(),
            capacity,
            0,
            SchedulerPriority::Low,
        );
        low.available_memory = || None;
        assert!(
            low.reserved() < capacity,
            "capacity {capacity} reserved all of itself"
        );
        let permit = low.try_admit(1, None).unwrap();
        assert!(
            permit.is_some(),
            "low priority makes progress at capacity {capacity}"
        );
    }
}

#[test]
fn the_memory_gate_defers_predicted_heavy_work() {
    let directory = tempfile::tempdir().unwrap();
    let mut pool = pool_at(directory.path(), 8, 0);
    pool.available_memory = || Some(1024);

    let running = pool.try_admit(1, None).unwrap().expect("first permit");
    assert!(
        pool.try_admit(2, Some(2048)).unwrap().is_none(),
        "a predicted-heavy job defers while memory is short"
    );
    assert!(
        pool.try_admit(2, Some(512)).unwrap().is_some(),
        "a prediction that fits is admitted"
    );
    assert!(
        pool.try_admit(2, None).unwrap().is_some(),
        "an unpredicted job is not gated"
    );
    drop(running);
}

#[test]
fn low_priority_leaves_the_reserve_while_a_normal_build_waits() {
    let directory = tempfile::tempdir().unwrap();
    let normal = pool_at(directory.path(), 4, 0);
    let mut low = Pool::new(directory.path().to_path_buf(), 4, 0, SchedulerPriority::Low);
    low.available_memory = || None;

    let held = normal.try_admit(2, None).unwrap().expect("two permits");
    std::fs::write(directory.path().join(PRIORITY_WAIT_STAMP), b"").unwrap();

    // capacity 4 minus a reserve of 1: the two held plus one more would fit,
    // but low priority must leave the reserve for whoever stamped.
    assert!(low.try_admit(2, None).unwrap().is_none());
    let admitted = low.try_admit(1, None).unwrap();
    assert!(admitted.is_some(), "low priority still uses spare capacity");
    // The normal build takes the reserved permit low priority left alone.
    assert!(normal.try_admit(1, None).unwrap().is_some());
    drop(held);
    drop(admitted);
}

#[test]
fn plans_weigh_history_links_and_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let bytes_per_permit = 1024;
    let pool = pool_at(directory.path(), 8, bytes_per_permit);
    pool.record_peak("heavy", 3500, false).unwrap();
    pool.record_peak("enormous", 1024 * 1024, false).unwrap();

    let (weight, predicted) = pool.plan(&Demand::new("unseen", false));
    assert_eq!((weight, predicted), (1, None));

    let (weight, predicted) = pool.plan(&Demand::new("unseen", true));
    assert_eq!(
        (weight, predicted),
        (LINK_WEIGHT, Some(LINK_WEIGHT * bytes_per_permit)),
        "an unmeasured link starts at the link floor"
    );

    let (weight, predicted) = pool.plan(&Demand::new("heavy", false));
    assert_eq!((weight, predicted), (4, Some(3500)));

    // "heavy" was measured compiling to metadata, and a link of that same
    // crate must not inherit it: `cargo check` and `cargo build` name one
    // crate for two very different workloads, and the metadata-only number
    // is the smaller. Inheriting it retires the link floor with a
    // measurement that was never a link's -- over-admission with a
    // plausible number attached. An unmeasured link keeps the floor.
    let (weight, predicted) = pool.plan(&Demand::new("heavy", true));
    assert_eq!(
        (weight, predicted),
        (LINK_WEIGHT, Some(LINK_WEIGHT * bytes_per_permit)),
        "a metadata-only measurement never speaks for the link beside it"
    );

    // A link measured *as a link* is weighted by what it used, whichever
    // side of the floor that falls on -- above it,
    pool.record_peak(&ledger_key("heavy", true), 9000, true)
        .unwrap();
    let (weight, predicted) = pool.plan(&Demand::new("heavy", true));
    assert_eq!(
        (weight, predicted),
        (8, Some(9000)),
        "a link's own history above the floor wins"
    );
    // and below it, which is the floor being retired rather than raised.
    pool.record_peak(&ledger_key("light-link", true), 700, true)
        .unwrap();
    let (weight, predicted) = pool.plan(&Demand::new("light-link", true));
    assert_eq!(
        (weight, predicted),
        (1, Some(700)),
        "a link's own history below the floor also wins"
    );
    // The two entries are genuinely separate, not one overwriting the other.
    assert_eq!(
        pool.plan(&Demand::new("heavy", false)),
        (4, Some(3500)),
        "the metadata entry survives the link entry beside it"
    );

    let (weight, _) = pool.plan(&Demand::new("enormous", false));
    assert_eq!(weight, 8, "weights clamp to the capacity");

    // Without a memory budget there is no ledger and no prediction; only the
    // static link floor remains.
    let unweighted = pool_at(directory.path(), 8, 0);
    assert_eq!(unweighted.plan(&Demand::new("heavy", false)), (1, None));
    assert_eq!(
        unweighted.plan(&Demand::new("heavy", true)),
        (LINK_WEIGHT, None)
    );
}

/// A test binary is charged for its threads as well as its memory, and never
/// shares a ledger entry with the crate compiled under the same name.
#[test]
fn test_binaries_weigh_their_threads_and_their_own_history() {
    let directory = tempfile::tempdir().unwrap();
    let bytes_per_permit = 1000;
    let pool = pool_at(directory.path(), 8, bytes_per_permit);

    assert_eq!(
        pool.plan(&Demand::test("suite", None)),
        (4, None),
        "an unstated thread count takes half the pool"
    );
    assert_eq!(pool.plan(&Demand::test("suite", Some(3))), (3, None));
    assert_eq!(
        pool.plan(&Demand::test("suite", Some(64))),
        (8, None),
        "threads clamp to the capacity"
    );

    // The compile of `suite` measured light and the run measured heavy; each
    // is planned from its own entry.
    pool.record_peak("suite", 1500, false).unwrap();
    pool.record_peak("suite [test, 1 threads]", 6500, false)
        .unwrap();
    assert_eq!(pool.plan(&Demand::new("suite", false)), (2, Some(1500)));
    assert_eq!(
        pool.plan(&Demand::test("suite", Some(1))),
        (7, Some(6500)),
        "memory outweighs a single thread"
    );

    let odd = pool_at(directory.path(), 5, 0);
    assert_eq!(
        odd.plan(&Demand::test("suite", None)),
        (3, None),
        "half an odd pool rounds up"
    );
    assert_eq!(
        pool_at(directory.path(), 1, 0).plan(&Demand::test("suite", Some(0))),
        (1, None),
        "a demand never weighs nothing"
    );
}

/// A test binary's measured core count replaces the guess, is kept per thread
/// setting, and is never lowered by a reading taken on a busier machine.
#[test]
fn test_binaries_are_weighed_by_the_cores_they_were_measured_using() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 0);

    pool.record_cpu("suite [test]", 2).unwrap();
    assert_eq!(
        pool.plan(&Demand::test("suite", None)),
        (2, None),
        "a measurement replaces half the pool, even without a memory budget"
    );
    assert_eq!(
        pool.plan(&Demand::test("suite", Some(6))),
        (6, None),
        "a stated thread count has its own history"
    );
    assert_eq!(
        pool.plan(&Demand::new("suite", false)),
        (1, None),
        "compilations are not measured for cores"
    );

    pool.record_cpu("suite [test]", 1).unwrap();
    assert_eq!(pool.plan(&Demand::test("suite", None)), (2, None));
    pool.record_cpu("suite [test]", 12).unwrap();
    assert_eq!(
        pool.plan(&Demand::test("suite", None)),
        (8, None),
        "a higher reading raises it, clamped to the capacity"
    );
}

#[test]
fn average_cores_round_to_the_nearest_core() {
    let seconds = Duration::from_secs_f64;
    assert_eq!(average_cores(seconds(0.01), seconds(2.0)), 1);
    assert_eq!(average_cores(seconds(2.8), seconds(2.0)), 1);
    assert_eq!(average_cores(seconds(3.2), seconds(2.0)), 2);
    assert_eq!(average_cores(seconds(15.0), seconds(2.0)), 8);
    assert_eq!(average_cores(seconds(1.0), Duration::ZERO), 1);
}

/// Every test binary is its own crate name, so a cold `cargo test --no-run`
/// never finds a per-crate entry for the link in front of it. What the machine
/// has learned about links in general is what it has to go on instead.
#[test]
fn an_unmeasured_link_is_weighed_by_what_this_machine_s_links_have_cost() {
    let directory = tempfile::tempdir().unwrap();
    let bytes_per_permit = 1000;
    let pool = pool_at(directory.path(), 8, bytes_per_permit);

    // Below the sample floor, one measurement is an anecdote: the static
    // weight still stands.
    pool.record_peak(&ledger_key("first", true), 5000, true)
        .unwrap();
    pool.record_peak(&ledger_key("second", true), 4000, true)
        .unwrap();
    assert_eq!(
        pool.plan(&Demand::new("unseen", true)),
        (LINK_WEIGHT, Some(LINK_WEIGHT * bytes_per_permit)),
        "two measurements are not yet history"
    );

    // The third makes it history, and the heaviest of them is the guess.
    pool.record_peak(&ledger_key("third", true), 3000, true)
        .unwrap();
    assert_eq!(
        pool.plan(&Demand::new("unseen", true)),
        (5, Some(5000)),
        "an unmeasured link is weighed by the heaviest link this machine made"
    );

    // A link this machine has actually measured still speaks for itself.
    pool.record_peak(&ledger_key("known", true), 1500, true)
        .unwrap();
    assert_eq!(
        pool.plan(&Demand::new("known", true)),
        (2, Some(1500)),
        "a crate's own history outranks the machine's"
    );

    // And nothing about this reaches a compilation that never links.
    assert_eq!(pool.plan(&Demand::new("unseen", false)), (1, None));
}

/// The window is what links cost *lately*, so it forgets in order and keeps
/// recording even when the crate's own entry has nothing to learn.
#[test]
fn the_link_window_is_bounded_and_records_every_link() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 1000);

    for index in 0..MAX_LINK_SAMPLES + 4 {
        pool.record_peak(&ledger_key("crate", true), 1000 + index as u64, true)
            .unwrap();
    }
    let ledger = read_ledger(&pool.ledger_path());
    assert_eq!(ledger.link_peaks.len(), MAX_LINK_SAMPLES);
    assert_eq!(
        ledger.link_peaks.first().copied(),
        Some(1000 + 4),
        "the oldest measurements are the ones dropped"
    );

    // A measurement no higher than the crate's recorded peak leaves the entry
    // alone, but the window still saw a link happen.
    let before = read_ledger(&pool.ledger_path()).link_peaks.len();
    pool.record_peak(&ledger_key("crate", true), 1, true)
        .unwrap();
    let after = read_ledger(&pool.ledger_path());
    assert_eq!(after.link_peaks.len(), before);
    assert_eq!(after.link_peaks.last().copied(), Some(1));
    assert_eq!(
        after.crates.get(&ledger_key("crate", true)).copied(),
        Some(1000 + MAX_LINK_SAMPLES as u64 + 3),
        "a lower measurement never shrinks the recorded peak"
    );
}

/// A repeat compilation that teaches the ledger nothing writes nothing: the
/// file is rewritten under a lock, and every crate in a build would otherwise
/// pay for it on every build after the first.
#[test]
fn a_measurement_that_changes_nothing_leaves_the_ledger_alone() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 1000);
    pool.record_peak("crate", 4000, false).unwrap();

    let written = || {
        std::fs::metadata(pool.ledger_path())
            .and_then(|metadata| metadata.modified())
            .unwrap()
    };
    let before = written();
    // Coarse filesystem timestamps would hide a rewrite that happened within
    // the same tick, so the check is on the bytes as well.
    let bytes = std::fs::read(pool.ledger_path()).unwrap();
    pool.record_peak("crate", 1000, false).unwrap();
    assert_eq!(std::fs::read(pool.ledger_path()).unwrap(), bytes);
    assert_eq!(written().duration_since(before).unwrap().as_nanos(), 0);

    // A link, though, always has a window entry to add.
    pool.record_peak(&ledger_key("crate", true), 1, true)
        .unwrap();
    assert_eq!(
        read_ledger(&pool.ledger_path()).link_peaks,
        vec![1],
        "a link's measurement lands in the window even when it teaches the entry nothing"
    );
}

#[test]
fn the_ledger_only_remembers_what_matters_and_never_shrinks_a_peak() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 1000);

    // Under one permit's worth of memory: not worth a ledger entry.
    assert_eq!(ledger_peak(900, false, false, true, 1000), None);
    // Over it: recorded as measured.
    assert_eq!(ledger_peak(1500, false, false, true, 1000), Some(1500));
    // A link that succeeded is recorded even light, because that is what
    // retires its floor.
    assert_eq!(ledger_peak(900, false, true, true, 1000), Some(900));

    pool.record_peak("crate", 1500, false).unwrap();
    pool.record_peak("crate", 1200, false).unwrap();
    assert_eq!(
        read_ledger(&pool.ledger_path()).crates.get("crate"),
        Some(&1500),
        "a lower later measurement never shrinks the recorded peak"
    );

    let garbage = pool.ledger_path();
    std::fs::write(&garbage, b"not json").unwrap();
    assert!(read_ledger(&garbage).crates.is_empty());
    pool.record_peak("crate", 1500, false).unwrap();
    assert_eq!(read_ledger(&garbage).crates.get("crate"), Some(&1500));
}

#[test]
fn an_oom_kill_escalates_past_what_was_measured() {
    // The killer stopped it at 1500; the record says it needs more than that.
    assert_eq!(ledger_peak(1500, true, false, false, 1000), Some(3000));
    // Even a tiny measurement escalates past one permit, so the weight rises.
    assert_eq!(ledger_peak(10, true, false, false, 1000), Some(1001));
    // A killed link escalates too, rather than recording what it reached.
    assert_eq!(ledger_peak(1500, true, true, false, 1000), Some(3000));
}

#[test]
fn a_link_that_never_reached_the_linker_cannot_cheapen_the_ones_that_do() {
    // Whether an invocation links is its crate type, not its outcome: a
    // binary that dies in type checking is a "link" that never ran one. Its
    // small peak must not retire the floor -- machine-wide, for every later
    // build sharing the cache -- on the strength of a compilation that never
    // did the expensive thing.
    assert_eq!(
        ledger_peak(900, false, true, false, 1000),
        None,
        "a failed link records nothing it could later be admitted on"
    );
    // The failure can still say something is heavy, which is the direction
    // that keeps a link that died reaching for memory from repeating itself.
    assert_eq!(ledger_peak(4000, false, true, false, 1000), Some(4000));
    // And the successful link of the same shape still retires its floor.
    assert_eq!(ledger_peak(900, false, true, true, 1000), Some(900));
}

#[test]
fn the_ledger_drops_its_smallest_entries_at_the_cap() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 1);

    for index in 0..MAX_LEDGER_ENTRIES + 10 {
        pool.record_peak(&format!("crate-{index}"), 100 + index as u64, false)
            .unwrap();
    }
    let ledger = read_ledger(&pool.ledger_path());
    assert_eq!(ledger.crates.len(), MAX_LEDGER_ENTRIES);
    assert!(
        !ledger.crates.contains_key("crate-0"),
        "the smallest entries go first"
    );
    let largest = format!("crate-{}", MAX_LEDGER_ENTRIES + 9);
    assert!(ledger.crates.contains_key(&largest));
}

#[cfg(target_os = "linux")]
#[test]
fn a_flight_leaves_its_prediction_for_the_next_build() {
    let directory = tempfile::tempdir().unwrap();

    let first = flight_at(directory.path(), "rustc", "abc123").unwrap();
    assert!(!first.waited(), "an empty pool has nothing to wait on");
    assert_eq!(first.inherited(), None, "nothing was left before");
    first.leave("the prediction");
    drop(first);

    let second = flight_at(directory.path(), "rustc", "abc123").unwrap();
    assert!(!second.waited(), "the first flight released its lock");
    assert_eq!(
        second.inherited(),
        Some("the prediction"),
        "what a flight leaves outlives it"
    );
}

#[test]
fn a_waiter_wakes_to_what_its_holder_left() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().to_path_buf();

    let holder = flight_at(&dir, "rustc", "abc123").unwrap();
    let waiter = std::thread::spawn(move || flight_at(&dir, "rustc", "abc123").unwrap());
    // Long enough for the thread to block on the lock; a race the other way
    // only weakens the `waited` assertion into a flake-free pass.
    std::thread::sleep(std::time::Duration::from_millis(100));
    holder.leave("freshly published");
    drop(holder);

    let waiter = waiter.join().unwrap();
    assert!(waiter.waited(), "the second flight blocked on the first");
    assert_eq!(waiter.inherited(), Some("freshly published"));
}

#[test]
fn a_record_that_does_not_describe_this_flight_is_not_inherited() {
    let directory = tempfile::tempdir().unwrap();

    let holder = flight_at(directory.path(), "rustc", "abc123").unwrap();
    holder.leave("a rustc prediction");
    drop(holder);

    // Same file name could never collide across these, but the record itself
    // must also say what it is: a version bump or a corrupt write reads as
    // nothing rather than as a prediction.
    let other = flight_at(directory.path(), "cc", "abc123").unwrap();
    assert_eq!(other.inherited(), None);
    drop(other);

    let path = directory
        .path()
        .join(INFLIGHT_DIR)
        .join("rustc-abc123.prediction");
    std::fs::write(&path, b"not json").unwrap();
    let corrupt = flight_at(directory.path(), "rustc", "abc123").unwrap();
    assert_eq!(corrupt.inherited(), None, "garbage reads as nothing");
}

#[test]
fn the_sweep_keeps_fresh_and_held_flights_and_drops_the_rest() {
    let cache = tempfile::tempdir().unwrap();
    let scheduler = cache.path().join(SCHEDULER_DIR);
    let inflight = scheduler.join(INFLIGHT_DIR);

    let old = flight_at(&scheduler, "rustc", "old").unwrap();
    old.leave("stale");
    drop(old);
    let fresh = flight_at(&scheduler, "rustc", "fresh").unwrap();
    fresh.leave("current");
    drop(fresh);
    let held = flight_at(&scheduler, "rustc", "held").unwrap();

    let backdate = |name: &str| {
        let ancient = std::time::SystemTime::now() - FLIGHT_MAX_AGE - Duration::from_secs(60);
        for path in [
            inflight.join(name),
            inflight.join(format!("{name}.prediction")),
        ] {
            if path.exists() {
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(ancient)
                    .unwrap();
            }
        }
    };
    backdate("rustc-old");
    backdate("rustc-held");

    prune_flights(cache.path());

    assert!(!inflight.join("rustc-old").exists(), "stale flights go");
    assert!(!inflight.join("rustc-old.prediction").exists());
    assert!(inflight.join("rustc-fresh").exists(), "fresh flights stay");
    assert!(inflight.join("rustc-fresh.prediction").exists());
    assert!(
        inflight.join("rustc-held").exists(),
        "a lock that cannot be taken is a live compilation, whatever its age"
    );
    drop(held);
}

#[test]
fn an_oversized_payload_is_not_left_behind() {
    let directory = tempfile::tempdir().unwrap();

    let holder = flight_at(directory.path(), "rustc", "abc123").unwrap();
    holder.leave(&"x".repeat(MAX_FLIGHT_PAYLOAD + 1));
    drop(holder);

    let next = flight_at(directory.path(), "rustc", "abc123").unwrap();
    assert_eq!(next.inherited(), None);
}

#[test]
fn session_environment_states_off_explicitly() {
    let config = crate::config::Config::for_test(Path::new("/cache"));
    assert_eq!(
        session_environment_with_jobs(&config, None),
        vec![(SCHED_DIR_ENV.to_string(), String::new())]
    );

    let mut config = crate::config::Config::for_test(Path::new("/cache"));
    config.scheduler.enabled = true;
    config.scheduler.cpus = 8;
    config.scheduler.reserve_cpus = 2;
    config.scheduler.memory_bytes = Some(8000);
    let environment = session_environment_with_jobs(&config, None);
    let value = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap()
    };
    assert_eq!(
        value(SCHED_DIR_ENV),
        Path::new("/cache").join("scheduler").to_str().unwrap()
    );
    assert_eq!(value(SCHED_SLOTS_ENV), "6");
    assert_eq!(value(SCHED_SLOT_BYTES_ENV), "1333");
    assert_eq!(value(SCHED_PRIORITY_ENV), "normal");
    assert!(!value(SCHED_BUILD_ID_ENV).is_empty());
    assert_eq!(value(SCHED_BUILD_SLOTS_ENV), "6");

    let environment = session_environment_with_jobs(&config, Some(3));
    let value = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap()
    };
    assert_eq!(value(SCHED_SLOTS_ENV), "6", "the machine pool stays global");
    assert_eq!(value(SCHED_SLOT_BYTES_ENV), "1333");
    assert_eq!(value(SCHED_BUILD_SLOTS_ENV), "3");
}

#[test]
fn pressure_blocks_unknown_demands_but_keeps_idle_progress() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CLOCK: AtomicU64 = AtomicU64::new(1_000);
    let dir = tempfile::tempdir().unwrap();
    let mut pool = pool_at(dir.path(), 4, 100);
    pool.pressure = true;
    pool.clock = || CLOCK.load(Ordering::SeqCst);
    pool.pressure_probe = || crate::pressure::Reading {
        total: Some(100),
        available: Some(0),
        ..Default::default()
    };
    let first = pool.try_admit(1, None).unwrap().unwrap();
    CLOCK.store(1_500, Ordering::SeqCst);
    assert!(pool.try_admit(1, None).unwrap().is_none());
    // Even a demand with no history, or an expired prediction gate, waits.
    CLOCK.store(2_000, Ordering::SeqCst);
    assert!(pool.try_admit(1, None).unwrap().is_none());
    drop(first);
    let first = pool.try_admit(1, None).unwrap().unwrap();
    pool.pressure = false;
    let second = pool.try_admit(1, None).unwrap().unwrap();
    drop((first, second));
}

#[test]
fn recovery_spaces_admissions_and_probe_failure_fails_open() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CLOCK: AtomicU64 = AtomicU64::new(1_000);
    let dir = tempfile::tempdir().unwrap();
    let mut pool = pool_at(dir.path(), 8, 100);
    pool.pressure = true;
    pool.clock = || CLOCK.load(Ordering::SeqCst);
    pool.pressure_probe = || crate::pressure::Reading {
        total: Some(100),
        available: Some(0),
        ..Default::default()
    };
    let first = pool.try_admit(1, None).unwrap().unwrap();
    CLOCK.store(1_500, Ordering::SeqCst);
    assert!(pool.try_admit(1, None).unwrap().is_none());
    pool.pressure_probe = || crate::pressure::Reading {
        total: Some(100),
        available: Some(50),
        ..Default::default()
    };
    for now in (2_000..7_000).step_by(500) {
        CLOCK.store(now, Ordering::SeqCst);
        assert!(pool.try_admit(1, None).unwrap().is_none());
    }
    CLOCK.store(7_000, Ordering::SeqCst);
    let second = pool.try_admit(1, None).unwrap().unwrap();
    assert!(pool.try_admit(1, None).unwrap().is_none());
    CLOCK.store(7_500, Ordering::SeqCst);
    let third = pool.try_admit(1, None).unwrap().unwrap();
    pool.pressure_probe = Default::default;
    CLOCK.store(8_000, Ordering::SeqCst);
    assert!(pool.try_admit(1, None).unwrap().is_some());
    drop((first, second, third));
}

#[test]
fn containers_sharing_a_pool_are_gated_by_their_own_pressure() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CLOCK: AtomicU64 = AtomicU64::new(1_000);
    fn container(counter: u64) -> crate::pressure::Reading {
        crate::pressure::Reading {
            total: Some(100),
            available: Some(50),
            // The same pathname names a different cgroup in each container.
            stalls: [("/sys/fs/cgroup/memory.pressure".into(), counter)].into(),
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let container_pool = |domain, probe| {
        let mut pool = pool_at(directory.path(), 64, 100);
        pool.pressure = true;
        pool.pressure_domain = domain;
        pool.pressure_probe = probe;
        pool.clock = || CLOCK.load(Ordering::SeqCst);
        pool
    };
    // Three idle containers whose stall counters never move, as in the
    // reported build, plus one that is genuinely short of memory.
    let idle = [
        container_pool("one", || container(21_764_622)),
        container_pool("two", || container(200_000_000)),
        container_pool("three", || container(393_795_795)),
    ];
    let short = container_pool("short", || crate::pressure::Reading {
        total: Some(100),
        available: Some(0),
        ..Default::default()
    });
    let held = short
        .try_admit(1, None)
        .unwrap()
        .expect("an idle pool admits");
    CLOCK.store(1_500, Ordering::SeqCst);
    assert!(short.try_admit(1, None).unwrap().is_none());
    for (step, now) in (2_000..60_000).step_by(500).enumerate() {
        CLOCK.store(now, Ordering::SeqCst);
        let permit = idle[step % 3].try_admit(1, None).unwrap();
        assert!(
            permit.is_some(),
            "another container's pressure gated this one"
        );
        assert!(short.try_admit(1, None).unwrap().is_none());
    }
    drop(held);
}

#[test]
fn an_oversized_idle_grant_spaces_the_next_pools_admission() {
    let directory = tempfile::tempdir().unwrap();
    let mut low = pool_at(directory.path(), 4, 100);
    low.pressure = true;
    low.priority = SchedulerPriority::Low;
    low.clock = || 7_000;
    std::fs::write(directory.path().join(PRIORITY_WAIT_STAMP), b"").unwrap();
    // Establish a recovered state without granting a compilation in the ramp.
    let state = crate::pressure::domain_state(directory.path(), low.pressure_domain);
    crate::pressure::sample(&state, 1_000, || crate::pressure::Reading {
        total: Some(100),
        available: Some(0),
        ..Default::default()
    })
    .unwrap();
    crate::pressure::sample(&state, 1_500, || crate::pressure::Reading {
        total: Some(100),
        available: Some(0),
        ..Default::default()
    })
    .unwrap();
    for now in (2_000..=7_000).step_by(500) {
        crate::pressure::sample(&state, now, || crate::pressure::Reading {
            total: Some(100),
            available: Some(50),
            ..Default::default()
        })
        .unwrap();
    }
    let first = low.try_admit(4, None).unwrap().unwrap();
    let mut wider = pool_at(directory.path(), 8, 100);
    wider.pressure = true;
    wider.clock = || 7_000;
    assert!(wider.try_admit(1, None).unwrap().is_none());
    drop(first);
}

#[test]
fn frozen_work_has_admission_priority_until_its_heartbeat_expires() {
    let directory = tempfile::tempdir().unwrap();
    let mut pool = pool_at(directory.path(), 4, 100);
    pool.pressure = true;
    pool.pressure_probe = Default::default;
    pool.clock = || 1_000;
    let first = pool.try_admit(1, None).unwrap().unwrap();
    let pending = directory.path().join("suspended");
    std::fs::create_dir(&pending).unwrap();
    std::fs::write(pending.join("generation"), "1000").unwrap();
    assert!(pool.try_admit(1, None).unwrap().is_none());
    // A dead supervisor cannot hold admissions closed indefinitely.
    pool.clock = || 4_000;
    assert!(pool.try_admit(1, None).unwrap().is_some());
    drop(first);
}

#[test]
fn link_profile_separates_release_from_dev_links() {
    let args = |flags: &[&str]| -> Vec<std::ffi::OsString> {
        flags.iter().map(std::ffi::OsString::from).collect()
    };
    let release = args(&["-C", "opt-level=3", "-C", "lto", "-C", "codegen-units=1"]);
    let dev = args(&["-C", "debuginfo=0", "-C", "opt-level=0"]);
    let release_demand = Demand::new("fallow", true).with_link_profile(&release);
    let dev_demand = Demand::new("fallow", true).with_link_profile(&dev);
    assert_ne!(
        ledger_key(&release_demand.name, true),
        ledger_key(&dev_demand.name, true)
    );

    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 1024);
    pool.record_peak(&ledger_key(&release_demand.name, true), 10_000_000, true)
        .unwrap();
    let (_, predicted) = pool.plan(&dev_demand);
    assert_ne!(
        predicted,
        Some(10_000_000),
        "a release peak must not charge a dev link"
    );
    let (_, predicted) = pool.plan(&release_demand);
    assert_eq!(predicted, Some(10_000_000));

    // Spelled `-Clto=fat` or as separate arguments, the profile is the same;
    // non-links are untouched.
    assert_eq!(
        link_profile(&args(&["-Clto=fat", "-Copt-level=3", "-Ccodegen-units=1"])),
        link_profile(&release)
    );
    assert_eq!(
        Demand::new("lib", false).with_link_profile(&release).name,
        "lib"
    );
}

#[test]
fn link_profile_reads_every_spelling_and_keeps_windows_apart() {
    let args = |flags: &[&str]| -> Vec<std::ffi::OsString> {
        flags.iter().map(std::ffi::OsString::from).collect()
    };
    assert_eq!(
        link_profile(&args(&["-O"])),
        link_profile(&args(&["-C", "opt-level=3"]))
    );
    assert_eq!(
        link_profile(&args(&["--codegen=lto", "--codegen", "codegen-units=1"])),
        link_profile(&args(&["-Clto", "-Ccodegen-units=1"]))
    );
    assert_ne!(
        link_profile(&args(&[])),
        link_profile(&args(&["-C", "lto=off"])),
        "an absent lto setting is not an explicit off"
    );

    // Three release links must not make a never-measured dev link
    // inherit a release-sized prediction.
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 1024);
    let release = Demand::new("bin", true).with_link_profile(&args(&["-Clto", "-Copt-level=3"]));
    for _ in 0..3 {
        pool.record_link_peak(
            &ledger_key(&release.name, true),
            1_000_000,
            true,
            release.profile.as_deref(),
        )
        .unwrap();
    }
    let dev = Demand::new("bin", true).with_link_profile(&args(&["-Copt-level=0"]));
    let (_, predicted) = pool.plan(&dev);
    assert_eq!(predicted, Some(LINK_WEIGHT * 1024));
    let (_, predicted) = pool.plan(&release);
    assert_eq!(predicted, Some(1_000_000));
}

#[test]
fn nextest_history_only_raises_measurements() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 100);
    let binary = "project/package::integration";
    pool.record_test_history(binary, "test_a", Some(20), Some(2))
        .unwrap();
    pool.record_test_history(binary, "test_a", Some(10), Some(1))
        .unwrap();
    let path = test_history_path(&directory.path().join(TEST_HISTORY_DIR), binary);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        TEST_HISTORY_HEADER_LEN + TEST_HISTORY_RECORD_LEN
    );

    pool.record_test_history(binary, "test_a", Some(25), Some(3))
        .unwrap();
    let records = read_test_history_records(&path, binary);
    assert_eq!(
        records
            .iter()
            .find(|record| record.hash == test_history_hash("test_a"))
            .map(|record| record.measurement),
        Some(TestMeasurement {
            peak: Some(25),
            cores: Some(3),
        })
    );
}

#[test]
fn nextest_history_binary_search_finds_first_last_and_missing_records() {
    let directory = tempfile::tempdir().unwrap();
    for count in [1, 2, 512] {
        let binary = format!("project/package-{count}");
        let names: Vec<_> = (0..count).map(|index| format!("test_{index}")).collect();
        let mut records = BTreeMap::new();
        for (index, name) in names.iter().enumerate() {
            records.insert(
                test_history_hash(name),
                TestMeasurement {
                    peak: Some(index as u64 + 10),
                    cores: Some(index as u64 % 8 + 1),
                },
            );
        }
        assert_eq!(records.len(), count);
        let path = test_history_path(directory.path(), &binary);
        std::fs::write(&path, encode_history_map(&binary, &records)).unwrap();

        let mut ordered: Vec<_> = names
            .iter()
            .map(|name| (test_history_hash(name), name.as_str()))
            .collect();
        ordered.sort_unstable_by_key(|(hash, _)| *hash);
        for (_, name) in [ordered[0], ordered[count - 1]] {
            let lookup = lookup_test_history(&path, &binary, name);
            assert_eq!(lookup.test, Some(records[&test_history_hash(name)]));
            assert_eq!(lookup.header.count, count as u64);
        }
        let missing = "not-in-this-file";
        assert!(!records.contains_key(&test_history_hash(missing)));
        assert_eq!(
            lookup_test_history(&path, &binary, missing).test,
            None,
            "missing hash in {count}-record file"
        );
    }
}

#[test]
fn nextest_plan_uses_header_maxima_for_a_new_test() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 100);
    let binary = "project/bin";
    pool.record_test_history(binary, "heavy", Some(450), Some(3))
        .unwrap();
    pool.record_test_history(binary, "small", Some(100), Some(1))
        .unwrap();

    let new_test = Demand::nextest(binary, "new");
    assert_eq!(pool.plan(&new_test), (5, Some(450)));
    assert_eq!(
        new_test.per_test.as_ref().unwrap().known.get(),
        Some(TestMeasurement::default()),
        "a missing test keeps its own measurement unknown"
    );
}

#[test]
fn nextest_history_rejects_bad_headers_and_ignores_partial_tail() {
    let directory = tempfile::tempdir().unwrap();
    let binary = "binary-a";
    let path = test_history_path(directory.path(), binary);
    let records = BTreeMap::from([
        (
            test_history_hash("first"),
            TestMeasurement {
                peak: Some(12),
                cores: Some(2),
            },
        ),
        (
            test_history_hash("last"),
            TestMeasurement {
                peak: Some(42),
                cores: Some(4),
            },
        ),
    ]);
    let bytes = encode_history_map(binary, &records);

    std::fs::write(&path, []).unwrap();
    assert_eq!(lookup_test_history(&path, binary, "first").header.count, 0);
    std::fs::write(&path, &bytes[..17]).unwrap();
    assert_eq!(lookup_test_history(&path, binary, "first").header.count, 0);

    let mut bad_magic = bytes.clone();
    bad_magic[0] ^= 1;
    std::fs::write(&path, bad_magic).unwrap();
    assert_eq!(lookup_test_history(&path, binary, "first").header.count, 0);

    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        lookup_test_history(&path, "binary-b", "first").header.count,
        0
    );

    let mut partial_tail = bytes;
    partial_tail.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7]);
    std::fs::write(&path, partial_tail).unwrap();
    let lookup = lookup_test_history(&path, binary, "last");
    assert_eq!(lookup.header.count, 2);
    assert_eq!(lookup.test, Some(records[&test_history_hash("last")]));
    assert_eq!(read_test_history_records(&path, binary).len(), 2);

    let pool = pool_at(directory.path(), 8, 100);
    let collision_path = test_history_path(&pool.dir.join(TEST_HISTORY_DIR), binary);
    std::fs::create_dir_all(collision_path.parent().unwrap()).unwrap();
    std::fs::write(&collision_path, encode_history_map("binary-b", &records)).unwrap();
    pool.record_test_history(binary, "replacement", Some(7), None)
        .unwrap();
    let rewritten = read_test_history_records(&collision_path, binary);
    assert_eq!(rewritten.len(), 1);
    assert_eq!(rewritten[0].hash, test_history_hash("replacement"));
    assert_eq!(
        read_test_history_header(&File::open(collision_path).unwrap(), binary)
            .unwrap()
            .maximum
            .peak,
        Some(7)
    );
}

#[test]
fn nextest_history_cap_drops_the_smallest_peak_first() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 100);
    let binary = "binary-with-many-tests";
    let path = test_history_path(&pool.dir.join(TEST_HISTORY_DIR), binary);
    let records: BTreeMap<_, _> = (1..=MAX_TEST_HISTORY_ENTRIES as u64 + 1)
        .map(|hash| {
            (
                hash,
                TestMeasurement {
                    peak: Some(hash),
                    cores: Some(1),
                },
            )
        })
        .collect();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, encode_history_map(binary, &records)).unwrap();

    let new_test = "new-test";
    let new_hash = test_history_hash(new_test);
    assert!(!records.contains_key(&new_hash));
    pool.record_test_history(
        binary,
        new_test,
        Some(MAX_TEST_HISTORY_ENTRIES as u64 + 10),
        None,
    )
    .unwrap();
    let capped = read_test_history_records(&path, binary);
    assert_eq!(capped.len(), MAX_TEST_HISTORY_ENTRIES);
    assert!(
        !capped.iter().any(|record| record.hash == 1),
        "the smallest peak should be dropped"
    );
    assert!(capped.iter().any(|record| record.hash == new_hash));
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        TEST_HISTORY_HEADER_LEN + TEST_HISTORY_RECORD_LEN * MAX_TEST_HISTORY_ENTRIES as u64
    );
}

#[test]
fn nextest_history_cap_breaks_equal_peak_ties_by_cores() {
    let mut records = vec![
        TestHistoryRecord {
            hash: 1,
            measurement: TestMeasurement {
                peak: Some(10),
                cores: Some(1),
            },
        },
        TestHistoryRecord {
            hash: 2,
            measurement: TestMeasurement {
                peak: Some(10),
                cores: Some(2),
            },
        },
        TestHistoryRecord {
            hash: 3,
            measurement: TestMeasurement {
                peak: Some(20),
                cores: Some(1),
            },
        },
    ];
    assert!(enforce_test_history_cap(&mut records, 2));
    assert!(!records.iter().any(|record| record.hash == 1));
    assert!(records.iter().any(|record| record.hash == 2));
    assert!(records.iter().any(|record| record.hash == 3));
}

#[test]
fn nextest_plan_uses_test_then_sibling_history_and_cold_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 8, 100);
    let measured = Demand::nextest("project/bin", "known");
    pool.record_test_history("project/bin", "known", Some(350), Some(3))
        .unwrap();
    assert_eq!(pool.plan(&measured), (4, Some(350)));

    let sibling = Demand::nextest("project/bin", "new");
    assert_eq!(pool.plan(&sibling), (4, Some(350)));

    let cold = Demand::nextest("project/cold", "first");
    assert_eq!(pool.plan(&cold), (2, Some(200)));
    let no_memory = pool_at(directory.path(), 8, 0);
    assert_eq!(no_memory.plan(&cold), (1, None));
}

#[test]
fn nextest_plan_clamps_to_capacity_and_ignores_suite_ledger_history() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 100);
    let nextest = Demand::nextest("project/bin", "case");
    assert_eq!(pool.plan(&nextest), (2, Some(200)));

    pool.record_test_history("project/bin", "case", Some(900), Some(12))
        .unwrap();
    assert!(read_ledger(&pool.ledger_path()).crates.is_empty());
    assert!(read_ledger(&pool.ledger_path()).cpus.is_empty());
    assert_eq!(pool.plan(&nextest), (4, Some(900)));

    pool.record_peak(&nextest.name, 9_000, false).unwrap();
    pool.record_cpu(&nextest.name, 9).unwrap();
    assert_eq!(pool.plan(&nextest), (4, Some(900)));

    let suite = Demand::test("project/bin", None);
    pool.record_peak(&suite.name, 100, false).unwrap();
    pool.record_cpu(&suite.name, 3).unwrap();
    assert_eq!(pool.plan(&suite), (3, Some(100)));
    let history = read_test_history_records(
        &test_history_path(&pool.dir.join(TEST_HISTORY_DIR), "project/bin"),
        "project/bin",
    );
    assert_eq!(history.len(), 1);
    assert!(
        history
            .iter()
            .any(|record| record.hash == test_history_hash("case"))
    );
}

#[test]
fn nextest_plan_clamps_heavy_measurements_to_pool_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 100);
    pool.record_test_history("bin", "heavy", Some(900), Some(12))
        .unwrap();
    assert_eq!(pool.plan(&Demand::nextest("bin", "heavy")), (4, Some(900)));
}

/// Set a file's modification time. Opened for writing, because Windows
/// refuses to change the times of a file opened read-only.
fn backdate(path: &Path, time: std::time::SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(time))
        .unwrap();
}

#[test]
fn nextest_history_pruning_removes_old_regular_files() {
    use std::time::{Duration, SystemTime};

    let directory = tempfile::tempdir().unwrap();
    let scheduler = directory.path().join(SCHEDULER_DIR);
    let history_dir = scheduler.join(TEST_HISTORY_DIR);
    std::fs::create_dir_all(&history_dir).unwrap();
    let stale = history_dir.join("stale.hist");
    let legacy = history_dir.join("legacy.jsonl");
    let recent = history_dir.join("recent.hist");
    std::fs::write(&stale, b"old\n").unwrap();
    std::fs::write(&legacy, b"old\n").unwrap();
    std::fs::write(&recent, b"new\n").unwrap();
    let old = SystemTime::now() - FLIGHT_MAX_AGE - Duration::from_secs(1);
    backdate(&stale, old);
    backdate(&legacy, old);

    // A writer holding the history lock keeps even stale history for now.
    let mut held = fslock::LockFile::open(&scheduler.join(TEST_HISTORY_LOCK)).unwrap();
    held.lock().unwrap();
    prune_test_history(directory.path());
    assert!(stale.exists());
    assert!(legacy.exists());
    drop(held);

    prune_test_history(directory.path());
    assert!(!stale.exists());
    assert!(!legacy.exists());
    assert!(recent.exists());
    // The lock is shared by every writer and is never removed.
    assert!(scheduler.join(TEST_HISTORY_LOCK).exists());
}

#[test]
fn planning_keeps_history_a_stable_suite_still_uses_from_looking_unused() {
    use std::time::{Duration, SystemTime};

    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 100);
    pool.record_test_history("bin", "case", Some(50), Some(1))
        .unwrap();
    let path = test_history_path(&pool.dir.join(TEST_HISTORY_DIR), "bin");
    let age = |path: &Path| {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap_or_default()
    };
    let old = SystemTime::now() - FLIGHT_MAX_AGE + Duration::from_secs(60);
    backdate(&path, old);

    pool.plan(&Demand::nextest("bin", "case"));
    assert!(age(&path) < TEST_HISTORY_REFRESH);
    prune_test_history(directory.path());
    assert!(path.exists());
}

#[test]
fn a_test_that_raises_nothing_records_without_the_history_lock() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 100);
    pool.record_test_history("bin", "case", Some(50), Some(2))
        .unwrap();
    let demand = Demand::nextest("bin", "case");
    pool.plan(&demand);
    let identity = demand.per_test.as_ref().unwrap();

    // Held elsewhere for the whole call: a recording that needed it would
    // block here instead of returning.
    let mut held = fslock::LockFile::open(&pool.dir.join(TEST_HISTORY_LOCK)).unwrap();
    held.lock().unwrap();
    pool.record_test_measurement(identity, Some(40), Some(2))
        .unwrap();
    pool.record_test_measurement(identity, None, None).unwrap();
    drop(held);

    pool.record_test_measurement(identity, Some(80), None)
        .unwrap();
    let history = read_test_history_records(
        &test_history_path(&pool.dir.join(TEST_HISTORY_DIR), "bin"),
        "bin",
    );
    assert_eq!(
        history
            .iter()
            .find(|record| record.hash == test_history_hash("case"))
            .map(|record| record.measurement),
        Some(TestMeasurement {
            peak: Some(80),
            cores: Some(2)
        })
    );
}

// Regression: S40's eight-permit analysis compile waited over an hour while
// newer small compiles repeatedly refilled the shared pool.
#[test]
fn a_heavy_waiter_drains_the_pool_before_new_small_compilations() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 0);
    let first = pool.try_admit(2, None).unwrap().unwrap();
    let second = pool.try_admit(1, None).unwrap().unwrap();
    let mut heavy = None;
    assert!(
        pool.try_admit_queued(4, None, &mut heavy)
            .unwrap()
            .is_none()
    );
    for _ in 0..10 {
        assert!(pool.try_admit(1, None).unwrap().is_none());
    }
    drop(first);
    assert!(pool.try_admit(1, None).unwrap().is_none());
    assert!(
        pool.try_admit_queued(4, None, &mut heavy)
            .unwrap()
            .is_none()
    );
    drop(second);
    let admitted = pool.try_admit_queued(4, None, &mut heavy).unwrap().unwrap();
    assert!(heavy.is_none());
    drop(admitted);
    assert!(pool.try_admit(1, None).unwrap().is_some());
}

#[test]
fn cancelled_and_crashed_waiters_release_their_queue_positions() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 4, 0);
    let held = pool.try_admit(3, None).unwrap().unwrap();
    let mut queued = None;
    assert!(
        pool.try_admit_queued(2, None, &mut queued)
            .unwrap()
            .is_none()
    );
    assert!(pool.try_admit(1, None).unwrap().is_none());
    drop(queued.take());
    assert!(pool.try_admit(1, None).unwrap().is_some());
    assert!(
        pool.try_admit_queued(2, None, &mut queued)
            .unwrap()
            .is_none()
    );
    // Leave the file behind with its lock released, as after process death.
    let mut crashed = queued.take().unwrap();
    drop(crashed._guard.lock.take());
    assert!(pool.try_admit(1, None).unwrap().is_some());
    drop(held);
}

#[test]
fn container_limits_cover_separate_builds_without_blocking_other_containers() {
    let directory = tempfile::tempdir().unwrap();
    let mut a = pool_at(directory.path(), 8, 0);
    a.pressure_domain = "container-a";
    a.domain_capacity = 3;
    a.build = Some(("build-a1".into(), 8));
    let mut a2 = pool_at(directory.path(), 8, 0);
    a2.pressure_domain = "container-a";
    a2.domain_capacity = 3;
    a2.build = Some(("build-a2".into(), 8));
    let mut b = pool_at(directory.path(), 8, 0);
    b.pressure_domain = "container-b";
    b.domain_capacity = 3;
    let held = a.try_admit(2, None).unwrap().unwrap();
    let mut queued = None;
    assert!(a2.try_admit_queued(3, None, &mut queued).unwrap().is_none());
    // Do not refill a's last slot; let its oldest request run. b can progress.
    assert!(a.try_admit(1, None).unwrap().is_none());
    let other = b.try_admit(3, None).unwrap().unwrap();
    assert!(b.try_admit(1, None).unwrap().is_none());
    drop(held);
    assert!(a2.try_admit_queued(3, None, &mut queued).unwrap().is_some());
    drop(other);
}

#[test]
fn a_build_ceiling_does_not_reserve_the_host_for_that_build() {
    let directory = tempfile::tempdir().unwrap();
    let mut a = pool_at(directory.path(), 8, 0);
    a.build = Some(("a".into(), 3));
    let b = pool_at(directory.path(), 8, 0);
    let held = a.try_admit(2, None).unwrap().unwrap();
    let mut queued = None;
    assert!(a.try_admit_queued(3, None, &mut queued).unwrap().is_none());
    assert!(a.try_admit(1, None).unwrap().is_none());
    assert!(b.try_admit(3, None).unwrap().is_some());
    drop(held);
    assert!(a.try_admit_queued(3, None, &mut queued).unwrap().is_some());
}

#[test]
fn an_available_memory_gate_releases_the_waiters_place() {
    let directory = tempfile::tempdir().unwrap();
    let mut pool = pool_at(directory.path(), 4, 1024);
    let held = pool.try_admit(3, None).unwrap().unwrap();
    let mut queued = None;
    assert!(
        pool.try_admit_queued(2, None, &mut queued)
            .unwrap()
            .is_none()
    );
    pool.available_memory = || Some(0);
    assert!(
        pool.try_admit_queued(2, Some(2048), &mut queued)
            .unwrap()
            .is_none()
    );
    assert!(queued.is_none());
    assert!(pool.try_admit(1, None).unwrap().is_some());
    drop(held);
}

#[test]
fn a_budget_change_preserves_the_memory_and_cpu_charges_of_live_leases() {
    let directory = tempfile::tempdir().unwrap();
    let old = pool_at(directory.path(), 4, 1024);
    let new = pool_at(directory.path(), 16, 4096);
    let large_unit = new.try_admit(1, None).unwrap().unwrap();
    // One new permit consumes all four old permits, not just one.
    assert!(old.try_admit(1, None).unwrap().is_none());
    drop(large_unit);
    let cores = old.try_admit(4, None).unwrap().unwrap();
    // Conversely, a four-core reservation cannot shrink to one core merely
    // because the memory budget per permit grew.
    assert!(new.try_admit(13, None).unwrap().is_none());
    drop(cores);
    assert!(new.try_admit(13, None).unwrap().is_some());
}

#[test]
fn queued_domain_limits_use_the_waiters_original_permit_units() {
    let directory = tempfile::tempdir().unwrap();
    let mut old = pool_at(directory.path(), 4, 1024);
    old.pressure_domain = "a";
    let mut new = pool_at(directory.path(), 16, 4096);
    new.pressure_domain = "a";
    let held = new.try_admit(1, None).unwrap().unwrap();
    let mut queued = None;
    assert!(
        old.try_admit_queued(2, None, &mut queued)
            .unwrap()
            .is_none()
    );
    assert!(new.try_admit(1, None).unwrap().is_none());
    // The old request is waiting for container a, not for the host pool.
    new.pressure_domain = "b";
    assert!(new.try_admit(1, None).unwrap().is_some());
    drop(held);
    assert!(
        old.try_admit_queued(2, None, &mut queued)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_waiter_removed_after_its_lock_check_has_left_the_queue() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("released");
    assert!(read_waiter(&path).unwrap().is_none());
    std::fs::write(&path, b"{").unwrap();
    assert!(
        read_waiter(&path).is_err(),
        "a record that is present but unreadable must not be skipped"
    );
}

const GIB: u64 = 1 << 30;

#[test]
fn a_cpu_only_local_ceiling_ignores_memory() {
    assert_eq!(local_ceiling(16, 0, 4, Some(GIB)), 4);
    assert_eq!(local_ceiling(16, 0, 32, Some(GIB)), 16);
}

#[test]
fn the_local_ceiling_is_the_smallest_of_pool_cpus_and_memory() {
    // 85% of 20GiB holds four 4GiB permits.
    assert_eq!(local_ceiling(16, 4 * GIB, 8, Some(20 * GIB)), 4);
    assert_eq!(local_ceiling(16, GIB, 2, Some(64 * GIB)), 2);
    assert_eq!(local_ceiling(3, GIB, 8, Some(64 * GIB)), 3);
    // Unknown memory leaves the pool and CPU limits in place.
    assert_eq!(local_ceiling(16, GIB, 8, None), 8);
}

#[test]
fn a_container_smaller_than_one_memory_permit_still_admits_one_compilation() {
    assert_eq!(local_ceiling(16, 8 * GIB, 8, Some(4 * GIB)), 1);
}

fn compile_demand(name: &str, arguments: &[&str]) -> Demand {
    Demand::new(name, false).with_link_profile(
        &arguments
            .iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>(),
    )
}

#[test]
fn compile_mode_keys_cover_effective_rustc_settings() {
    let mode = |args: &[&str]| compile_demand("crate", args).compile_mode.unwrap();
    assert_eq!(
        mode(&[]),
        "crate [compile emit=link crate-type= opt0 lto-default cgu- debug0 incremental-off]"
    );
    assert_eq!(
        mode(&["--emit=dep-info,metadata"]),
        "crate [compile emit=metadata crate-type= opt0 lto-default cgu- debug0 incremental-off]"
    );
    assert!(
        mode(&["--emit", "metadata=out,dep-info=dep"])
            .contains("compile emit=metadata crate-type=")
    );
    assert!(
        mode(&["--emit=metadata,llvm-ir"]).contains("compile emit=llvm-ir,metadata crate-type=")
    );
    assert_eq!(
        mode(&[
            "--emit=metadata",
            "--emit=link",
            "--emit",
            "metadata",
            "--test",
            "-O",
            "-Copt-level=1",
            "--codegen=opt-level=2",
            "-C",
            "opt-level=0",
            "-Clto=fat",
            "--codegen",
            "lto=off",
            "-Ccodegen-units=16",
            "-Ccodegen-units=1",
            "-g",
            "-Cdebuginfo=line-tables-only",
            "-Cincremental=first",
            "--codegen=incremental=",
            "-Zthreads=8",
            "-Z",
            "threads=2"
        ]),
        "crate [compile emit=link,metadata crate-type= test opt0 lto-off cgu1 debugline-tables-only incremental-off threads=2]"
    );
    assert!(mode(&["-C", "incremental=second"]).contains("incremental-on"));
    for args in [
        vec!["--emit=link"],
        vec!["--test"],
        vec!["-Copt-level=3"],
        vec!["-Clto"],
        vec!["-Ccodegen-units=2"],
        vec!["-g"],
        vec!["-Cincremental=dir"],
        vec!["-Zthreads=4"],
    ] {
        let baseline = if args == ["--emit=link"] {
            mode(&["--emit=metadata"])
        } else {
            mode(&[])
        };
        assert_ne!(mode(&args), baseline, "{args:?}");
    }
}

#[test]
fn compile_keys_do_not_collide_with_existing_workloads() {
    let demand = compile_demand("crate", &["--test", "--emit=metadata"]);
    let key = demand.compile_mode.unwrap();
    let link = Demand::new("crate", true).with_link_profile(&[]);
    for old in [
        ledger_key("crate", false),
        ledger_key("crate", true),
        ledger_key(&link.name, true),
        Demand::test("crate", None).name,
        Demand::test("crate", Some(2)).name,
    ] {
        assert_ne!(key, old);
    }
    assert!(link.compile_mode.is_none());
    assert!(Demand::test("crate", None).compile_mode.is_none());
}

#[test]
fn compile_modes_fall_back_until_three_successful_samples() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let demand = compile_demand("heavy", &["--emit=metadata"]);
    pool.record_peak("heavy", 1000, false).unwrap();
    assert_eq!(pool.plan(&demand), (10, Some(1000)));
    for count in 1..=4 {
        pool.record_compile(&demand, 81, false, true).unwrap();
        let expected = if count < 3 {
            (10, Some(1000))
        } else {
            (2, Some(122))
        };
        assert_eq!(pool.plan(&demand), expected);
        let ledger = read_ledger(&pool.ledger_path());
        assert_eq!(
            ledger.compile_samples[demand.compile_mode.as_ref().unwrap()],
            count.min(3)
        );
        assert_eq!(ledger.crates["heavy"], 1000);
    }
}

#[test]
fn compile_headroom_is_capped_and_handles_a_missing_bare_peak() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let demand = compile_demand("heavy", &[]);
    for _ in 0..3 {
        pool.record_compile(&demand, 401, false, true).unwrap();
    }
    assert_eq!(pool.plan(&demand), (5, Some(401)));
    let key = demand.compile_mode.as_ref().unwrap();
    let mut ledger = read_ledger(&pool.ledger_path());
    ledger.crates.remove("heavy");
    std::fs::write(pool.ledger_path(), serde_json::to_vec(&ledger).unwrap()).unwrap();
    assert_eq!(pool.plan(&demand), (7, Some(602)));
    ledger.crates.insert(key.clone(), u64::MAX);
    assert_eq!(
        compile_prediction(&ledger, "heavy", key),
        (Some(u64::MAX), true)
    );
}

#[test]
fn recreating_a_missing_fallback_covers_all_retained_modes() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let old = compile_demand("heavy", &["--emit=metadata"]);
    for _ in 0..3 {
        pool.record_compile(&old, 1000, false, true).unwrap();
    }
    let mut ledger = read_ledger(&pool.ledger_path());
    ledger.crates.remove("heavy");
    std::fs::write(pool.ledger_path(), serde_json::to_vec(&ledger).unwrap()).unwrap();
    let new = compile_demand("heavy", &[]);
    pool.record_compile(&new, 200, false, true).unwrap();
    assert_eq!(read_ledger(&pool.ledger_path()).crates["heavy"], 1000);
    assert_eq!(pool.plan(&old), (10, Some(1000)));
}

#[test]
fn compile_failures_and_oom_raise_peaks_without_successful_samples() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let demand = compile_demand("heavy", &[]);
    let key = demand.compile_mode.as_ref().unwrap();
    pool.record_compile(&demand, 200, false, false).unwrap();
    let ledger = read_ledger(&pool.ledger_path());
    assert_eq!(ledger.crates["heavy"], 200);
    assert_eq!(ledger.crates[key], 200);
    assert!(!ledger.compile_samples.contains_key(key));
    pool.record_compile(&demand, 301, true, false).unwrap();
    let ledger = read_ledger(&pool.ledger_path());
    assert_eq!(ledger.crates["heavy"], 602);
    assert_eq!(ledger.crates[key], 602);
    assert!(!ledger.compile_samples.contains_key(key));
    pool.record_compile(&demand, 1, true, false).unwrap();
    assert_eq!(read_ledger(&pool.ledger_path()).crates[key], 602);
    assert_eq!(pool.plan(&demand), (7, Some(602)));
}

#[test]
fn light_crates_stay_unrecorded_and_cpu_only_compiles_do_not_learn() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let demand = compile_demand("light", &[]);
    for _ in 0..3 {
        pool.record_compile(&demand, 99, false, true).unwrap();
    }
    assert!(read_ledger(&pool.ledger_path()).crates.is_empty());
    assert!(!directory.path().join(LEDGER_LOCK).exists());
    assert_eq!(pool.plan(&demand), (1, None));
    let cpu = pool_at(directory.path(), 100, 0);
    cpu.record_compile(&demand, 999, false, true).unwrap();
    assert!(read_ledger(&pool.ledger_path()).crates.is_empty());
}

#[test]
fn old_ledgers_drop_compile_trust_but_keep_the_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let demand = compile_demand("heavy", &[]);
    let key = demand.compile_mode.as_ref().unwrap();
    std::fs::write(
        pool.ledger_path(),
        serde_json::to_vec(&serde_json::json!({
            "version": LEDGER_VERSION, "crates": {"heavy": 1000, key: 200}, "link_peaks": []
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(pool.plan(&demand), (10, Some(1000)));
}

#[test]
fn evicted_compile_peaks_cannot_reuse_old_sample_counts() {
    let mut ledger = MemoryLedger::default();
    let key =
        "heavy [compile emit=metadata crate-type= opt0 lto-default cgu- debug0 incremental-off]";
    ledger.crates.insert("heavy".into(), 1000);
    ledger.crates.insert(key.into(), 200);
    ledger.compile_samples.insert(key.into(), 3);
    for n in 0..MAX_LEDGER_ENTRIES {
        ledger.crates.insert(format!("other{n}"), 2000);
    }
    trim_compile_ledger(&mut ledger);
    assert!(!ledger.crates.contains_key(key));
    assert!(!ledger.compile_samples.contains_key(key));
    assert!(!ledger.crates.contains_key("heavy"));
    ledger.crates.insert("heavy".into(), 1000);
    ledger.crates.insert(key.into(), 1);
    assert_eq!(
        compile_prediction(&ledger, "heavy", key),
        (Some(1000), false)
    );
}

#[test]
fn scheduler_stats_read_old_records_without_locking_or_reclaiming_them() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let demand = compile_demand("heavy", &["--emit=metadata"]);
    pool.record_peak("heavy", 1000, false).unwrap();
    for _ in 0..3 {
        pool.record_compile(&demand, 81, false, true).unwrap();
    }
    let leases = directory.path().join(LEASES_DIR);
    let waiters = directory.path().join(WAITERS_DIR);
    std::fs::create_dir_all(&leases).unwrap();
    std::fs::create_dir_all(&waiters).unwrap();
    let old_lease = br#"{"version":1,"weight":2,"priority":"normal"}"#;
    let old_waiter = br#"{"ticket":1,"weight":3,"priority":"normal","domain":"old-domain","domain_capacity":10,"bytes_per_permit":100,"build":null}"#;
    assert!(
        serde_json::from_slice::<Lease>(old_lease)
            .unwrap()
            .name
            .is_none()
    );
    assert!(
        serde_json::from_slice::<Waiting>(old_waiter)
            .unwrap()
            .name
            .is_none()
    );
    std::fs::write(leases.join("stale"), old_lease).unwrap();
    std::fs::write(waiters.join("stale"), old_waiter).unwrap();
    let held = pool
        .try_admit_named(2, None, &mut None, Some(&demand))
        .unwrap()
        .unwrap();
    // Admission reclaimed the deliberately stale records; restore them before
    // reading stats to prove the read-only path leaves them in place.
    std::fs::write(leases.join("stale"), old_lease).unwrap();
    std::fs::write(waiters.join("stale"), old_waiter).unwrap();
    let mut queued = None;
    assert!(
        pool.try_admit_named(100, None, &mut queued, Some(&demand))
            .unwrap()
            .is_none()
    );
    // Queue admission also reclaims stale records. Restore once more so the
    // stats reader sees both old and current records of each kind.
    std::fs::write(leases.join("stale"), old_lease).unwrap();
    std::fs::write(waiters.join("stale"), old_waiter).unwrap();
    let snapshot = scheduler_snapshot(directory.path(), 100, 100, true);
    let text = snapshot.text();
    assert!(
        text.contains("samples=3, charge=122 B / 2 permits (mode)"),
        "{text}"
    );
    assert!(text.contains("lease unknown: mode=unknown, build=unknown, domain=unknown"));
    assert!(text.contains("waiter unknown: mode=unknown, build=unknown, domain=old-domain"));
    assert!(text.contains("lease heavy: mode=heavy [compile emit=metadata crate-type="));
    assert!(text.contains("waiter heavy: mode=heavy [compile emit=metadata crate-type="));
    assert!(text.contains("age="));
    assert!(leases.join("stale").exists());
    assert!(waiters.join("stale").exists());
    let mut lock = fslock::LockFile::open(&held.path).unwrap();
    assert!(
        !lock.try_lock().unwrap(),
        "stats must leave held locks alone"
    );
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["entries"][1]["source"], "mode");
    assert_eq!(json["activity"].as_array().unwrap().len(), 4);
    let cpu = scheduler_snapshot(directory.path(), 100, 0, true);
    assert_eq!(cpu.entries[1].charge_permits, None);
    assert_eq!(cpu.entries[1].charge_bytes, None);
    let mut ledger = read_ledger(&pool.ledger_path());
    ledger.compile_samples.clear();
    std::fs::write(pool.ledger_path(), serde_json::to_vec(&ledger).unwrap()).unwrap();
    let fallback = scheduler_snapshot(directory.path(), 100, 100, true);
    assert!(
        fallback
            .text()
            .contains("charge=1000 B / 10 permits (crate-wide fallback)")
    );
}

#[test]
fn scheduler_stats_defer_link_and_test_charges_to_admission() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 10, 100);
    pool.record_peak("crate [link]", 200, true).unwrap();
    pool.record_peak("crate [test]", 200, false).unwrap();
    pool.record_peak("crate [test, 3 threads]", 200, false)
        .unwrap();
    pool.record_cpu("crate [test]", 4).unwrap();
    for unit in [0, 100] {
        let report = scheduler_snapshot(directory.path(), 10, unit, true);
        for entry in &report.entries {
            assert_eq!(entry.charge_bytes, None, "{}", entry.key);
            assert_eq!(entry.charge_permits, None, "{}", entry.key);
            assert_eq!(entry.recorded_peak_bytes, 200);
        }
        assert!(
            report
                .text()
                .contains("peak=200 B, samples=0, charge=see admission")
        );
        assert!(!report.text().contains("see admission (see admission)"));
    }
}

#[test]
fn compile_emit_sets_combine_normalize_and_distinguish_codegen_outputs() {
    let mode = |args: &[&str]| compile_demand("crate", args).compile_mode.unwrap();
    assert_eq!(mode(&[]), mode(&["--emit=link"]));
    assert_eq!(
        mode(&["--emit=metadata"]),
        mode(&["--emit", "dep-info=dep,metadata=one", "--emit=metadata=two"])
    );
    assert_eq!(
        mode(&["--emit=link", "--emit", "metadata"]),
        mode(&["--emit=metadata,link,link"])
    );
    assert!(mode(&["--emit=link", "--emit=metadata"]).contains("emit=link,metadata "));
    assert_ne!(
        mode(&["--emit=link", "--emit=metadata"]),
        mode(&["--emit=metadata"])
    );
    let mut keys = std::collections::BTreeSet::new();
    for emit in [
        "metadata", "link", "mir", "obj", "asm", "llvm-ir", "llvm-bc", "dep-info",
    ] {
        assert!(keys.insert(mode(&[&format!("--emit={emit}")])), "{emit}");
    }
    assert!(mode(&["--emit=dep-info"]).contains("emit= crate-type="));
}

#[test]
fn compile_modes_include_all_crate_types_and_every_target() {
    let mode = |args: &[&str]| compile_demand("crate", args).compile_mode.unwrap();
    let types = mode(&["--crate-type=rlib,bin", "--crate-type", "rlib,cdylib"]);
    assert!(types.contains("crate-type=bin,cdylib,rlib "));
    assert_eq!(types, mode(&["--crate-type", "cdylib,bin,rlib"]));
    assert_ne!(mode(&["--crate-type=rlib"]), mode(&["--crate-type=bin"]));
    let repeated = mode(&["--target", "first", "--target=wasm32-unknown-unknown"]);
    assert_ne!(repeated, mode(&["--target=wasm32-unknown-unknown"]));
    assert_ne!(repeated, mode(&["--target=first"]));
    assert!(repeated.contains("targets=[\"first\", \"wasm32-unknown-unknown\"]"));
    assert!(
        mode(&["--target=wasm32-unknown-unknown"]).contains("target=\"wasm32-unknown-unknown\"")
    );
    assert_ne!(mode(&[]), mode(&["--target=wasm32-unknown-unknown"]));
    assert_ne!(
        mode(&["--target=host test"]),
        mode(&["--target=host", "--test"])
    );
}

#[test]
fn materially_different_compiles_cannot_reuse_a_trained_light_mode() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    pool.record_peak("crate", 1000, false).unwrap();
    let metadata = compile_demand("crate", &["--crate-type=rlib", "--emit=metadata"]);
    let mir = compile_demand("crate", &["--crate-type=rlib", "--emit=mir"]);
    for _ in 0..3 {
        pool.record_compile(&metadata, 100, false, true).unwrap();
        pool.record_compile(&mir, 100, false, true).unwrap();
    }
    assert_eq!(pool.plan(&metadata), (2, Some(150)));
    for args in [
        vec!["--crate-type=rlib", "--emit=link", "--emit=metadata"],
        vec!["--crate-type=rlib", "--emit=link"],
        vec![
            "--crate-type=bin",
            "--emit=metadata",
            "--target=wasm32-unknown-unknown",
        ],
        vec![
            "--crate-type=rlib",
            "--emit=metadata",
            "--target=wasm32-unknown-unknown",
        ],
        vec!["--crate-type=bin", "--emit=metadata"],
    ] {
        assert_eq!(
            pool.plan(&compile_demand("crate", &args)),
            (10, Some(1000)),
            "{args:?}"
        );
    }
}

#[test]
fn wrapper_observations_raise_peaks_but_never_train_a_light_mode() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    pool.record_peak("crate", 1000, false).unwrap();
    let args = vec![std::ffi::OsString::from("--emit=metadata")];
    let wrapped = Demand::new("crate", false).with_compiler_execution(&args, None, true, false);
    let direct = Demand::new("crate", false).with_compiler_execution(&args, None, false, false);
    let key = direct.compile_mode.as_ref().unwrap();
    assert_eq!(wrapped.compile_mode, direct.compile_mode);
    for _ in 0..3 {
        pool.record_compile(&wrapped, 81, false, true).unwrap();
    }
    let ledger = read_ledger(&pool.ledger_path());
    assert_eq!(ledger.crates[key], 81);
    assert!(!ledger.compile_samples.contains_key(key));
    assert_eq!(pool.plan(&direct), (10, Some(1000)));
    pool.record_compile(&direct, 81, false, true).unwrap();
    pool.record_compile(&wrapped, 1200, false, true).unwrap();
    let ledger = read_ledger(&pool.ledger_path());
    assert_eq!(ledger.compile_samples[key], 1);
    assert_eq!(ledger.crates[key], 1200);
    assert_eq!(ledger.crates["crate"], 1200);
    pool.record_compile(&wrapped, 800, true, false).unwrap();
    let ledger = read_ledger(&pool.ledger_path());
    assert_eq!(ledger.compile_samples[key], 1);
    assert_eq!(ledger.crates[key], 1600);
    assert_eq!(ledger.crates["crate"], 1600);
}

#[test]
fn compile_keys_do_not_reuse_legacy_classification_samples() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let mut ledger = MemoryLedger::default();
    let old = "crate [compile metadata opt0 lto-default cgu- debug0 incremental-off]";
    ledger.crates.insert("crate".into(), 1000);
    ledger.crates.insert(old.into(), 100);
    ledger.compile_samples.insert(old.into(), 3);
    std::fs::write(pool.ledger_path(), serde_json::to_vec(&ledger).unwrap()).unwrap();
    assert_eq!(
        pool.plan(&compile_demand("crate", &["--emit=metadata"])),
        (10, Some(1000))
    );
}

#[test]
fn scheduler_stats_compile_charges_match_admission_predictions() {
    let directory = tempfile::tempdir().unwrap();
    let pool = pool_at(directory.path(), 100, 100);
    let demand = compile_demand("crate", &["--emit=metadata"]);
    pool.record_peak("crate", 1000, false).unwrap();
    for _ in 0..4 {
        let snapshot = scheduler_snapshot(directory.path(), 100, 100, true);
        let planned = pool.plan(&demand);
        for entry in &snapshot.entries {
            if entry.key == demand.compile_mode.as_ref().unwrap().as_str() {
                assert_eq!(
                    (entry.charge_permits, entry.charge_bytes),
                    (Some(planned.0), planned.1)
                );
            } else if entry.key == "crate" {
                assert_eq!(entry.charge_bytes, Some(1000));
                assert_eq!(entry.charge_permits, Some(10));
            }
        }
        pool.record_compile(&demand, 81, false, true).unwrap();
    }
}
