// Compatibility reference copied from fe73826. Keep independent of the vector policy.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Lease {
    version: u8,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mode_key: Option<String>,
    weight: u64,
    priority: String,
    #[serde(default)]
    build_id: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    bytes_per_permit: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Waiting {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    mode_key: Option<String>,
    ticket: u64,
    weight: u64,
    priority: String,
    domain: String,
    domain_capacity: u64,
    #[serde(default)]
    global_capacity: Option<u64>,
    bytes_per_permit: u64,
    build: Option<(String, u64)>,
}

fn fits(used: u64, weight: u64, capacity: u64) -> bool {
    used == 0 || used.saturating_add(weight) <= capacity
}

// Sessions keep their original permit units across a configuration change.
// Never charge a larger memory permit as one of a smaller pool's permits.
// Keep the original weight as a CPU floor: reservations and tests may have
// requested that many cores, even when their memory fits fewer new permits.
fn lease_weight(lease: &Lease, bytes_per_permit: u64) -> u64 {
    weight_in_units(lease.weight, lease.bytes_per_permit, bytes_per_permit)
}

fn weight_in_units(weight: u64, original_unit: u64, unit: u64) -> u64 {
    if unit == 0 {
        weight
    } else {
        weight.max(weight.saturating_mul(original_unit).div_ceil(unit))
    }
}

/// Protect every prefix of the older queue against every possible completion
/// order. Earlier waiters eventually become leases too, so their weights join
/// the subset sums after checking each prefix.
fn can_pass_waiters(capacity: u64, live: &[u64], older: &[u64], weight: u64) -> bool {
    if older.is_empty() {
        return true;
    }
    // Bound the complete bitset, including future leases from older waiters.
    const MAX_BITS: u64 = 1 << 16;
    let Some(total) = live.iter().chain(older).try_fold(0_u64, |sum, w| {
        sum.checked_add(*w).filter(|sum| *sum < MAX_BITS)
    }) else {
        return false;
    };
    let used: u64 = live.iter().sum();
    let Some(free) = capacity.checked_sub(used) else {
        return false;
    };
    if weight > free {
        return false;
    }
    let mut sums = vec![0_u64; (total as usize + 1).div_ceil(64)];
    sums[0] = 1;
    for w in live {
        add_subset_weight(&mut sums, *w as usize);
    }
    let mut prefix = 0;
    for h in older {
        prefix += h;
        let slack = if prefix <= free {
            free - prefix
        } else {
            let threshold = (prefix - free) as usize;
            let first_word = threshold / 64;
            let Some(sum) = (first_word..sums.len()).find_map(|index| {
                let bits = if index == first_word {
                    sums[index] & (u64::MAX << (threshold % 64))
                } else {
                    sums[index]
                };
                (bits != 0).then(|| index * 64 + bits.trailing_zeros() as usize)
            }) else {
                return false;
            };
            (sum - threshold) as u64
        };
        if weight > slack {
            return false;
        }
        add_subset_weight(&mut sums, *h as usize);
    }
    true
}

fn add_subset_weight(sums: &mut [u64], weight: usize) {
    let words = weight / 64;
    let shift = weight % 64;
    for index in (words..sums.len()).rev() {
        let mut shifted = sums[index - words] << shift;
        if shift != 0 && index > words {
            shifted |= sums[index - words - 1] >> (64 - shift);
        }
        sums[index] |= shifted;
    }
}

fn domain_used(live: &[Lease], domain: &str, bytes_per_permit: u64) -> u64 {
    live.iter()
        // Old wrappers do not identify their domain. Charge them locally too
        // until they finish, rather than over-admitting during an upgrade.
        .filter(|lease| lease.domain.as_deref().is_none_or(|id| id == domain))
        .map(|lease| lease_weight(lease, bytes_per_permit))
        .sum()
}

fn build_fits(
    live: &[Lease],
    build: &Option<(String, u64)>,
    weight: u64,
    bytes_per_permit: u64,
) -> bool {
    build.as_ref().is_none_or(|(id, capacity)| {
        fits(
            live.iter()
                .filter(|lease| lease.build_id.as_ref() == Some(id))
                .map(|lease| lease_weight(lease, bytes_per_permit))
                .sum(),
            weight,
            *capacity,
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AdmissionDecision {
    Ordered,
    Passing,
    Waiting,
}

/// Pure admission policy, shared by the pool and the event simulations. Keep
/// today's scope exceptions as a fast path; only a refusal needs the slack
/// calculation across every scope the passer shares with an older waiter.
pub(super) fn admission_decision<'a>(
    capacity: u64,
    live: &[Lease],
    waiting: impl Iterator<Item = &'a Waiting>,
    request: &Waiting,
) -> AdmissionDecision {
    let used = live
        .iter()
        .map(|lease| lease_weight(lease, request.bytes_per_permit))
        .sum();
    if !build_fits(
        live,
        &request.build,
        request.weight,
        request.bytes_per_permit,
    ) || !fits(
        domain_used(live, &request.domain, request.bytes_per_permit),
        request.weight,
        request.domain_capacity,
    ) || !fits(used, request.weight, capacity)
    {
        return AdmissionDecision::Waiting;
    }
    let mut older: Vec<_> = waiting
        .filter(|older| {
            older.ticket < request.ticket
                && !(older.priority == "low" && request.priority == "normal")
        })
        .collect();
    older.sort_unstable_by_key(|older| older.ticket);
    let ordered_blocked = older.iter().any(|older| {
        if !build_fits(live, &older.build, older.weight, older.bytes_per_permit) {
            return older.build.as_ref().map(|(id, _)| id)
                == request.build.as_ref().map(|(id, _)| id);
        }
        if !fits(
            domain_used(live, &older.domain, older.bytes_per_permit),
            older.weight,
            older.domain_capacity,
        ) {
            return older.domain == request.domain;
        }
        true
    });
    if !ordered_blocked {
        return AdmissionDecision::Ordered;
    }
    let scope_allows = |capacity, unit, build: Option<&str>, domain: Option<&str>| {
        let weights: Vec<_> = live
            .iter()
            .filter(|lease| {
                build.is_none_or(|id| lease.build_id.as_deref() == Some(id))
                    && domain.is_none_or(|id| {
                        lease
                            .domain
                            .as_deref()
                            .is_none_or(|lease_id| lease_id == id)
                    })
            })
            .map(|lease| lease_weight(lease, unit))
            .collect();
        // Include all older waiters in this physical scope, even if a
        // different ceiling currently blocks them. Their weights can later
        // become live leases and must participate in the prefix calculation.
        let heads: Vec<_> = older
            .iter()
            .filter(|waiter| {
                build.is_none_or(|id| waiter.build.as_ref().map(|(id, _)| id.as_str()) == Some(id))
                    && domain.is_none_or(|id| waiter.domain == id)
            })
            .map(|waiter| weight_in_units(waiter.weight, waiter.bytes_per_permit, unit))
            .collect();
        can_pass_waiters(
            capacity,
            &weights,
            &heads,
            weight_in_units(request.weight, request.bytes_per_permit, unit),
        )
    };
    // Every scope may retain older permit units/limits, so protect each
    // distinct recorded pair, starting with the request's global budget.
    if !scope_allows(capacity, request.bytes_per_permit, None, None) {
        return AdmissionDecision::Waiting;
    }
    let mut checked_global =
        std::collections::BTreeSet::from([(capacity, request.bytes_per_permit)]);
    let mut checked = std::collections::BTreeSet::new();
    for waiter in &older {
        // Older mbx records lack the global ceiling; retain the request's
        // capacity as the fallback until those waiters leave the queue.
        let waiter_capacity = waiter.global_capacity.unwrap_or(capacity);
        if checked_global.insert((waiter_capacity, waiter.bytes_per_permit))
            && !scope_allows(waiter_capacity, waiter.bytes_per_permit, None, None)
        {
            return AdmissionDecision::Waiting;
        }
        if waiter.domain == request.domain
            && checked.insert((false, waiter.domain_capacity, waiter.bytes_per_permit))
            && !scope_allows(
                waiter.domain_capacity,
                waiter.bytes_per_permit,
                None,
                Some(&waiter.domain),
            )
        {
            return AdmissionDecision::Waiting;
        }
        if let (Some((id, ceiling)), Some((request_id, _))) = (&waiter.build, &request.build)
            && id == request_id
            && checked.insert((true, *ceiling, waiter.bytes_per_permit))
            && !scope_allows(*ceiling, waiter.bytes_per_permit, Some(id), None)
        {
            return AdmissionDecision::Waiting;
        }
    }
    AdmissionDecision::Passing
}

pub(super) fn local_ceiling(
    capacity: u64,
    bytes_per_permit: u64,
    cpus: u64,
    memory: Option<u64>,
) -> u64 {
    let memory = if bytes_per_permit == 0 {
        capacity
    } else {
        memory.map_or(capacity, |bytes| {
            // The budget admission enforces. Permit rounding puts it fewer
            // than `capacity` bytes below the configured value.
            let budget = capacity.saturating_mul(bytes_per_permit);
            if budget > bytes {
                (bytes / 100 * 85) / bytes_per_permit
            } else {
                capacity
            }
        })
    };
    capacity.min(cpus).min(memory).max(1)
}

pub(super) fn prediction(pool: &Pool, demand: &Demand) -> (u64, Option<u64>, &'static str) {
    if let Some(identity) = &demand.per_test {
        let path = test_history_path(&pool.dir.join(TEST_HISTORY_DIR), &identity.binary);
        let history = lookup_test_history(&path, &identity.binary, &identity.test);
        if history.header.count > 0 {
            refresh_test_history(&path);
        }
        identity.known.set(Some(history.test.unwrap_or_default()));
        let measurement = history
            .test
            .or_else(|| (history.header.count > 0).then_some(history.header.maximum));
        let cores = measurement
            .and_then(|measurement| measurement.cores)
            .unwrap_or(1)
            .max(1);
        if pool.bytes_per_permit == 0 {
            return (cores.min(pool.capacity), None, "unmeasured");
        }
        let known_peak = measurement.and_then(|measurement| measurement.peak);
        let predicted = known_peak
            .unwrap_or_else(|| UNMEASURED_TEST_WEIGHT.saturating_mul(pool.bytes_per_permit));
        let weight = predicted
            .div_ceil(pool.bytes_per_permit)
            .max(cores)
            .clamp(1, pool.capacity);
        return (
            weight,
            Some(predicted),
            if known_peak.is_some() {
                "test-history"
            } else {
                "unmeasured"
            },
        );
    }
    // What a test binary was measured using beats what it said or what
    // its harness would default to: libtest starts a thread per CPU
    // whether or not the tests keep them busy.
    let learned = demand
        .learns_cpu
        .then(|| {
            read_ledger(&pool.ledger_path())
                .cpus
                .get(&demand.name)
                .copied()
        })
        .flatten();
    let threads = learned
        .or(demand.threads)
        .unwrap_or_else(|| pool.capacity.div_ceil(2))
        .max(1);
    let link_floor = demand.links.then_some(LINK_WEIGHT);
    if pool.bytes_per_permit == 0 {
        return (
            link_floor.unwrap_or(1).max(threads).min(pool.capacity),
            None,
            "unmeasured",
        );
    }
    let ledger = read_ledger(&pool.ledger_path());
    let recorded = ledger
        .crates
        .get(&ledger_key(&demand.name, demand.links))
        .copied();
    let mut link_source = "link-floor";
    let link_bytes = link_floor.map(|weight| {
        // What this machine's links have actually cost beats a constant
        // chosen before anyone had seen one. Every test binary is its own
        // crate name, so a cold `cargo test --no-run` never gets to reuse
        // the measurement of the link it just made -- the per-crate entry
        // is always missing for the one in front of it -- and this is the
        // history it can use instead. The heaviest of the window rather
        // than its average, because the cost of guessing low is an
        // out-of-memory kill and the cost of guessing high is a wait.
        //
        // A link with a known profile consults only that profile's
        // window, so a release LTO link never speaks for a dev link.
        let window = match &demand.profile {
            Some(profile) => ledger
                .profile_link_peaks
                .get(profile)
                .map_or(&[][..], Vec::as_slice),
            None => ledger.link_peaks.as_slice(),
        };
        let measured = (window.len() >= MIN_LINK_SAMPLES)
            .then(|| window.iter().copied().max())
            .flatten();
        if measured.is_some() {
            link_source = "link-window";
        }
        measured.unwrap_or_else(|| weight.saturating_mul(pool.bytes_per_permit))
    });
    // History replaces the floor rather than merely raising it. The floor
    // is a guess for links the pool has never seen; once one has been
    // measured, charging it the guess anyway would hold the tail of every
    // build -- the link-heavy stretch -- to half the concurrency its real
    // memory justifies. The ledger never lowers a recorded peak and an
    // OOM kill escalates past it, so trusting the measurement stays safe
    // in the direction that matters.
    //
    // Only a *link's* own history may retire the link floor, which is why
    // the two are separate ledger entries. One crate is compiled both
    // ways in a single build -- `cargo check` emits metadata where `cargo
    // build` links -- and the metadata-only measurement is the smaller of
    // the two by far. Keyed by name alone it would retire the floor for
    // the link beside it, which is over-admission with a plausible number
    // attached to it.
    let (estimate, source) = if let Some(key) = &demand.compile_mode {
        let (value, trusted) = legacy_compile_prediction(&ledger, &demand.name, key);
        (
            value,
            if trusted {
                "mode"
            } else if value.is_some() {
                "crate-wide"
            } else {
                "unmeasured"
            },
        )
    } else {
        (
            recorded,
            if recorded.is_none() {
                "unmeasured"
            } else if demand.learns_cpu {
                "test-history"
            } else if demand.links {
                "link-history"
            } else {
                "crate-wide"
            },
        )
    };
    let (predicted, source) = match estimate {
        Some(value) => (Some(value), source),
        None if link_bytes.is_some() => (link_bytes, link_source),
        None => (None, source),
    };
    // Memory and threads are separate costs of the same process; it takes
    // whichever share of the pool is larger.
    let weight = predicted
        .map_or(1, |bytes| bytes.div_ceil(pool.bytes_per_permit))
        .max(threads)
        .clamp(1, pool.capacity);
    (weight, predicted, source)
}

// State writer copied from f47d416, before calibrated windows.
pub(super) fn record_compile(
        pool: &super::Pool,
        demand: &Demand,
        measured: u64,
        oom: bool,
        succeeded: bool,
    ) -> Result<()> {
        let Some(key) = demand.compile_mode.as_ref() else {
            return Ok(());
        };
        if pool.bytes_per_permit == 0 {
            return Ok(());
        }
        let succeeded = succeeded && demand.count_compile_samples;
        let peak = ledger_peak(measured, oom, false, succeeded, pool.bytes_per_permit);
        // Ordinary light crates need no ledger lock. A concurrent first heavy
        // sample may be missed here; that only delays trusting the light mode.
        if peak.is_none()
            && !read_ledger(&pool.ledger_path())
                .crates
                .contains_key(&demand.name)
        {
            return Ok(());
        }
        std::fs::create_dir_all(&pool.dir)?;
        let mut lock = fslock::LockFile::open(&pool.dir.join(LEDGER_LOCK))?;
        lock.lock()?;
        let mut ledger = read_ledger(&pool.ledger_path());
        let mode_peak = peak.unwrap_or(measured);
        let bare_known = ledger.crates.get(&demand.name).copied();
        let mode_known = ledger.crates.get(key).copied();
        let count = ledger.compile_samples.get(key).copied().unwrap_or(0);
        if bare_known.is_some_and(|bare| peak.is_none_or(|peak| peak <= bare))
            && mode_known.is_some_and(|known| known >= mode_peak)
            && (!succeeded || oom || count >= 3)
        {
            return Ok(());
        }
        if let Some(peak) = peak {
            // A mode may survive without its bare entry (e.g. an old writer's
            // eviction). Recreating the fallback must cover every retained
            // mode, rather than cap a known mode below its measured maximum.
            let retained = if bare_known.is_none() {
                let prefix = format!("{} [compile ", demand.name);
                ledger
                    .crates
                    .iter()
                    .filter(|(name, _)| name.starts_with(&prefix))
                    .map(|(_, peak)| *peak)
                    .max()
                    .unwrap_or(0)
            } else {
                0
            };
            let bare = ledger.crates.entry(demand.name.clone()).or_default();
            *bare = (*bare).max(peak).max(retained);
        }
        if !ledger.crates.contains_key(&demand.name) {
            return Ok(());
        }
        // Even a light successful mode must learn when its crate is heavy.
        let mode = ledger.crates.entry(key.clone()).or_default();
        *mode = (*mode).max(mode_peak);
        if succeeded && !oom {
            let count = ledger.compile_samples.entry(key.clone()).or_default();
            *count = count.saturating_add(1).min(3);
        }
        ledger.version = LEDGER_VERSION;
        trim_compile_ledger(&mut ledger);
        let mut contents = serde_json::to_vec(&ledger)?;
        contents.push(b'\n');
        crate::util::write_advisory(&pool.ledger_path(), &contents)
    }


// Link writer copied from f47d416.
pub(super) fn record_link_peak(
        pool: &super::Pool,
        name: &str,
        peak: u64,
        links: bool,
        profile: Option<&str>,
    ) -> Result<()> {
        std::fs::create_dir_all(&pool.dir)
            .wrap_err_with(|| format!("failed to create {}", pool.dir.display()))?;
        let mut lock = fslock::LockFile::open(&pool.dir.join(LEDGER_LOCK))?;
        lock.lock()?;
        let path = pool.ledger_path();
        let mut ledger = read_ledger(&path);
        ledger.version = LEDGER_VERSION;
        // The window records every link, including one whose per-crate entry
        // is already higher: it says what links cost around here lately, which
        // a measurement no worse than the last one still answers.
        if links {
            let window = match profile {
                Some(profile) => ledger
                    .profile_link_peaks
                    .entry(profile.to_string())
                    .or_default(),
                None => &mut ledger.link_peaks,
            };
            window.push(peak);
            let surplus = window.len().saturating_sub(MAX_LINK_SAMPLES);
            window.drain(..surplus);
            while ledger.profile_link_peaks.len() > MAX_LEDGER_ENTRIES {
                ledger.profile_link_peaks.pop_first();
            }
        }
        let known = ledger.crates.get(name).is_some_and(|known| *known >= peak);
        if known && !links {
            // Nothing moved: the entry already stands higher and there is no
            // window entry to add. Rewriting the file identically would cost
            // every repeat compilation a write under this lock.
            return Ok(());
        }
        if !known {
            ledger.crates.insert(name.to_string(), peak);
        }
        trim_compile_ledger(&mut ledger);
        let mut contents = serde_json::to_vec(&ledger)?;
        contents.push(b'\n');
        // A heuristic every link refreshes: a lost write costs one sample.
        crate::util::write_advisory(&path, &contents)
    }
