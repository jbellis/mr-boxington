//! Admission transitions, never retry polling. All failures are advisory.
use super::*;
use std::io::{BufRead, BufReader};

const FILE: &str = "decisions.jsonl";
const PREVIOUS: &str = "decisions.jsonl.1";
const LOCK: &str = "decisions.lock";
// Small transitions retain the observation log's sub-1 KiB bound. Admission
// includes bounded overtake evidence and may use conventional PIPE_BUF (4 KiB),
// including its newline, while still being issued as one O_APPEND write.
const TRANSITION_LINE_BYTES: usize = 1024;
const ADMITTED_LINE_BYTES: usize = 4096;
const MAX_OVERTAKES: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Slack {
    scope: String,
    capacity: Resources,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_per_permit: Option<u64>,
    #[serde(flatten)]
    dimensions: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Overtake {
    ticket: u64,
    name: Option<String>,
    memory_bytes: u64,
    cpu: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
    // Some(None) denotes a different, absent build; omitted means the same.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_build"
    )]
    build: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<String>,
    waited_ms: u64,
    reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    slack: Vec<Slack>,
    #[serde(skip)]
    first_arrival_ms: Option<u64>,
}

fn optional_build<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Default)]
pub(super) struct Trace {
    overtook_total: usize,
    overtook: Vec<Overtake>,
}

impl Trace {
    pub(super) fn initialize(&mut self, older: &[&Waiting], live: &[Lease], request: &Waiting) {
        self.overtook_total = older.len();
        let mut older = older.to_vec();
        older.sort_unstable_by_key(|w| w.ticket);
        self.overtook = older
            .iter()
            .take(MAX_OVERTAKES)
            .map(|older| {
                let resources = older.resources();
                Overtake {
                    ticket: older.ticket,
                    name: older.name.clone(),
                    memory_bytes: resources.memory_bytes,
                    cpu: resources.cpu,
                    domain: (older.domain != request.domain).then(|| older.domain.clone()),
                    build: (older.build.as_ref().map(|b| &b.0)
                        != request.build.as_ref().map(|b| &b.0))
                    .then(|| older.build.as_ref().map(|b| b.0.clone())),
                    priority: (older.priority != request.priority).then(|| older.priority.clone()),
                    waited_ms: 0,
                    first_arrival_ms: older.first_arrival_ms,
                    reason: older_exception(live, older, request)
                        .unwrap_or("pass")
                        .into(),
                    slack: Vec::new(),
                }
            })
            .collect();
    }

    pub(super) fn slack(
        &mut self,
        ticket: u64,
        scope: &str,
        capacity: Resources,
        dimension: &str,
        value: u64,
    ) {
        let bytes_per_permit = scope
            .starts_with("legacy:")
            .then_some(capacity.memory_bytes);
        let capacity = if bytes_per_permit.is_some() {
            Resources {
                cpu: capacity.cpu,
                memory_bytes: 0,
            }
        } else {
            capacity
        };
        if let Some(waiter) = self.overtook.iter_mut().find(|w| w.ticket == ticket) {
            if waiter.reason != "pass" {
                return;
            }
            if let Some(slack) = waiter.slack.iter_mut().find(|s| {
                s.scope == scope && s.capacity == capacity && s.bytes_per_permit == bytes_per_permit
            }) {
                slack.dimensions.insert(dimension.into(), value);
            } else {
                waiter.slack.push(Slack {
                    scope: scope.into(),
                    capacity,
                    bytes_per_permit,
                    dimensions: BTreeMap::from([(dimension.into(), value)]),
                });
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FreeScope {
    scope: String,
    // A zero memory capacity means unlimited; free memory is then null.
    capacity: Resources,
    cpu: u64,
    memory_bytes: Option<u64>,
}

impl FreeScope {
    fn new(scope: &str, capacity: Resources, used: Resources) -> Self {
        Self {
            scope: scope.into(),
            capacity,
            cpu: capacity.cpu.saturating_sub(used.cpu),
            memory_bytes: (capacity.memory_bytes != 0)
                .then(|| capacity.memory_bytes.saturating_sub(used.memory_bytes)),
        }
    }
}

/// The remaining queue-record fields accompany a reclaimed holder's identity
/// and effective resources, so its original ceilings are not lost on removal.
#[derive(Debug, Serialize, Deserialize)]
struct QueueLimits {
    weight: u64,
    bytes_per_permit: u64,
    domain_capacity: u64,
    global_capacity: Option<u64>,
    cpu_cap: Option<u64>,
    mem_cap: Option<u64>,
    domain_memory_cap: Option<u64>,
    domain_cpu_cap: Option<u64>,
    build_capacity: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Event {
    v: u8,
    ts_ms: u64,
    mbx_version: String,
    pid: Option<u32>,
    event: String,
    name: Option<String>,
    mode_key: Option<String>,
    cpu: u64,
    memory_bytes: u64,
    domain: String,
    build: Option<String>,
    priority: String,
    ticket: u64,
    first_arrival_ms: Option<u64>,
    waited_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queue_limits: Option<QueueLimits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    free: Vec<FreeScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overtook_total: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overtook_logged: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overtook: Option<Vec<Overtake>>,
}

impl Event {
    fn new(pool: &Pool, kind: &str, record: &Waiting) -> Self {
        let now = (pool.clock)();
        let resources = record.resources();
        Self {
            v: 1,
            ts_ms: now,
            mbx_version: env!("CARGO_PKG_VERSION").into(),
            pid: record.pid,
            event: kind.into(),
            name: record
                .name
                .clone()
                .or_else(|| record.pid.is_some().then(|| "mbx reserve".into())),
            mode_key: record
                .mode_key
                .clone()
                .or_else(|| record.pid.is_some().then(|| "reservation".into())),
            cpu: resources.cpu,
            memory_bytes: resources.memory_bytes,
            domain: record.domain.clone(),
            build: record.build.as_ref().map(|b| b.0.clone()),
            priority: record.priority.clone(),
            ticket: record.ticket,
            first_arrival_ms: record.first_arrival_ms,
            waited_ms: record
                .first_arrival_ms
                .map_or(0, |first| now.saturating_sub(first)),
            reason: None,
            queue_limits: (kind == "abandoned").then_some(QueueLimits {
                weight: record.weight,
                bytes_per_permit: record.bytes_per_permit,
                domain_capacity: record.domain_capacity,
                global_capacity: record.global_capacity,
                cpu_cap: record.cpu_cap,
                mem_cap: record.mem_cap,
                domain_memory_cap: record.domain_memory_cap,
                domain_cpu_cap: record.domain_cpu_cap,
                build_capacity: record.build.as_ref().map(|b| b.1),
            }),
            rule: None,
            free: Vec::new(),
            overtook_total: None,
            overtook_logged: None,
            overtook: None,
        }
    }
}

impl Pool {
    pub(super) fn dequeue(&self, queued: &mut Option<QueueEntry>, reason: &str) {
        if let Some(entry) = queued.take() {
            let mut event = Event::new(self, "dequeued", &entry.record);
            event.reason = Some(reason.into());
            self.write_decision(event);
        }
    }

    pub(super) fn decision_event(&self, kind: &str, record: &Waiting, age_ms: Option<u64>) {
        if !self.events {
            return;
        }
        let mut event = Event::new(self, kind, record);
        if kind == "abandoned" {
            // Age of the reclaimed record, rather than total request age.
            event.waited_ms = age_ms.unwrap_or(0);
        }
        self.write_decision(event);
    }

    pub(super) fn admitted_event(
        &self,
        request: &Waiting,
        live: &[Lease],
        decision: AdmissionDecision,
        mut trace: Trace,
    ) {
        let mut event = Event::new(self, "admitted", request);
        event.rule = Some(
            match decision {
                AdmissionDecision::Ordered => "ordered",
                AdmissionDecision::Passing => "passing",
                AdmissionDecision::Waiting => unreachable!("only granted permits are logged"),
            }
            .into(),
        );
        event.free.push(FreeScope::new(
            "global",
            request.global_budget(Resources {
                cpu: self.capacity,
                memory_bytes: self.memory_capacity,
            }),
            scope_used(live.iter()),
        ));
        event.free.push(FreeScope::new(
            "domain",
            request.domain_budget(),
            domain_used(live, &request.domain),
        ));
        if let Some((id, capacity)) = &request.build {
            event.free.push(FreeScope::new(
                "build",
                Resources {
                    cpu: *capacity,
                    memory_bytes: 0,
                },
                scope_used(live.iter().filter(|l| l.build_id.as_ref() == Some(id))),
            ));
        }
        for waiter in &mut trace.overtook {
            waiter.waited_ms = waiter
                .first_arrival_ms
                .map_or(0, |first| event.ts_ms.saturating_sub(first));
        }
        event.overtook_total = Some(trace.overtook_total);
        event.overtook = Some(trace.overtook);
        self.write_decision(event);
    }

    fn write_decision(&self, event: Event) {
        if self.events
            && let Err(error) = append(&self.dir, event)
        {
            debug!("scheduler admission decision was not recorded: {error:#}");
        }
    }
}

fn compact_optional(value: &mut Option<String>) -> Result<()> {
    if let Some(value) = value {
        accuracy::compact_to(value, 64)?;
    }
    Ok(())
}

fn encode(mut event: Event) -> Result<Vec<u8>> {
    for value in [
        &mut event.mbx_version,
        &mut event.domain,
        &mut event.priority,
    ] {
        accuracy::compact_to(value, 64)?;
    }
    for value in [&mut event.name, &mut event.mode_key, &mut event.build] {
        compact_optional(value)?;
    }
    if let Some(overtook) = &mut event.overtook {
        overtook.truncate(MAX_OVERTAKES);
        for waiter in overtook.iter_mut() {
            for value in [&mut waiter.name, &mut waiter.domain, &mut waiter.priority] {
                compact_optional(value)?;
            }
            if let Some(build) = &mut waiter.build {
                compact_optional(build)?;
            }
        }
    }
    let limit = if event.event == "admitted" {
        ADMITTED_LINE_BYTES
    } else {
        TRANSITION_LINE_BYTES - 1
    };
    loop {
        if let Some(overtook) = &event.overtook {
            event.overtook_logged = Some(overtook.len());
        }
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        if line.len() <= limit {
            return Ok(line);
        }
        if let Some(overtook) = &mut event.overtook
            && !overtook.is_empty()
        {
            overtook.pop();
            continue;
        }
        // Metadata is bounded independently. This is a defensive fallback for
        // future additions: retain the decision and totals even with long IDs.
        for value in [&mut event.name, &mut event.mode_key, &mut event.build]
            .into_iter()
            .flatten()
        {
            accuracy::compact_to(value, 24)?;
        }
        accuracy::compact_to(&mut event.domain, 24)?;
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        eyre::ensure!(
            line.len() <= limit,
            "scheduler decision metadata exceeds line bound"
        );
        return Ok(line);
    }
}

fn append(dir: &Path, event: Event) -> Result<()> {
    let line = encode(event)?;
    accuracy::append_line(dir, FILE, PREVIOUS, LOCK, &line)
}

#[derive(Debug, Default, Serialize)]
pub(super) struct Summary {
    admitted_by_rule: BTreeMap<String, u64>,
    admissions_overtaking_by_reason: BTreeMap<String, u64>,
    admissions_overtaking: u64,
    overtook_entries_omitted: usize,
    dequeued_by_reason: BTreeMap<String, u64>,
    largest_overtakes: Vec<LargestOvertake>,
    abandoned_long_waits: Vec<Abandoned>,
}

#[derive(Debug, Serialize)]
struct LargestOvertake {
    ts_ms: u64,
    admitted_name: Option<String>,
    admitted_ticket: u64,
    #[serde(flatten)]
    waiter: Overtake,
}

#[derive(Debug, Serialize)]
struct Abandoned {
    ts_ms: u64,
    name: Option<String>,
    memory_bytes: u64,
    age_ms: u64,
}

fn counts_text(counts: &BTreeMap<String, u64>) -> String {
    if counts.is_empty() {
        "none".into()
    } else {
        counts
            .iter()
            .map(|(key, count)| format!("{key}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Summary {
    pub(super) fn read(dir: &Path) -> Self {
        let mut summary = Self::default();
        for filename in [PREVIOUS, FILE] {
            let Ok(file) = File::open(dir.join(filename)) else {
                continue;
            };
            for line in BufReader::new(file).split(b'\n').flatten() {
                let Ok(event) = serde_json::from_slice::<Event>(&line) else {
                    continue;
                };
                if event.v != 1 {
                    continue;
                }
                match event.event.as_str() {
                    "admitted" => {
                        *summary
                            .admitted_by_rule
                            .entry(event.rule.unwrap_or_else(|| "unknown".into()))
                            .or_default() += 1;
                        if event.overtook_total.unwrap_or(0) > 0 {
                            summary.admissions_overtaking += 1;
                        }
                        let entries = event.overtook.unwrap_or_default();
                        summary.overtook_entries_omitted += event
                            .overtook_total
                            .unwrap_or(0)
                            .saturating_sub(entries.len());
                        let reasons: std::collections::BTreeSet<_> =
                            entries.iter().map(|w| w.reason.clone()).collect();
                        for reason in reasons {
                            *summary
                                .admissions_overtaking_by_reason
                                .entry(reason)
                                .or_default() += 1;
                        }
                        for mut waiter in entries {
                            // Expand inherited identities in JSON statistics.
                            if waiter.domain.is_none() {
                                waiter.domain = Some(event.domain.clone());
                            }
                            if waiter.build.is_none() {
                                waiter.build = Some(event.build.clone());
                            }
                            if waiter.priority.is_none() {
                                waiter.priority = Some(event.priority.clone());
                            }
                            summary.largest_overtakes.push(LargestOvertake {
                                ts_ms: event.ts_ms,
                                admitted_name: event.name.clone(),
                                admitted_ticket: event.ticket,
                                waiter,
                            });
                        }
                    }
                    "dequeued" => {
                        *summary
                            .dequeued_by_reason
                            .entry(event.reason.unwrap_or_else(|| "unknown".into()))
                            .or_default() += 1
                    }
                    "abandoned" if event.waited_ms > 15 * 60 * 1000 => {
                        summary.abandoned_long_waits.push(Abandoned {
                            ts_ms: event.ts_ms,
                            name: event.name,
                            memory_bytes: event.memory_bytes,
                            age_ms: event.waited_ms,
                        })
                    }
                    _ => {}
                }
            }
        }
        summary
            .largest_overtakes
            .sort_by_key(|entry| std::cmp::Reverse(entry.waiter.waited_ms));
        summary
    }

    pub(super) fn text(&self) -> String {
        let mut lines = vec![
            "Admission decisions:".into(),
            format!(
                "  Admitted by rule: {}",
                counts_text(&self.admitted_by_rule)
            ),
            format!(
                "  Admissions overtaking older waiters: {}",
                self.admissions_overtaking
            ),
            format!(
                "  Admissions overtaking by reason (recorded entries): {}",
                counts_text(&self.admissions_overtaking_by_reason)
            ),
            format!(
                "  Dequeued by reason: {}",
                counts_text(&self.dequeued_by_reason)
            ),
        ];
        if self.overtook_entries_omitted > 0 {
            lines.push(format!("  {} overtake entries omitted by log limits; reason counts and rankings may be incomplete.", self.overtook_entries_omitted));
        }
        for entry in self.largest_overtakes.iter().take(5) {
            lines.push(format!(
                "  Overtook {} (ticket {}, {}, waited {}ms): {}",
                entry.waiter.name.as_deref().unwrap_or("unknown"),
                entry.waiter.ticket,
                crate::stats::size(entry.waiter.memory_bytes),
                entry.waiter.waited_ms,
                entry.waiter.reason
            ));
        }
        for entry in &self.abandoned_long_waits {
            lines.push(format!(
                "  Abandoned {}: {}, age {}ms",
                entry.name.as_deref().unwrap_or("unknown"),
                crate::stats::size(entry.memory_bytes),
                entry.age_ms
            ));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(dir: &Path, capacity: u64) -> Pool {
        let mut pool = Pool::new(
            dir.into(),
            capacity,
            MEMORY_QUANTUM,
            SchedulerPriority::Normal,
        );
        pool.domain_capacity = capacity;
        pool.legacy_domain_capacity = capacity;
        pool.domain_memory_capacity = capacity * MEMORY_QUANTUM;
        pool.available_memory = || None;
        pool.pressure = false;
        pool.clock = || 1_000;
        pool
    }

    fn record(ticket: u64, cpu: u64) -> Waiting {
        serde_json::from_value(serde_json::json!({
            "pid": 123, "first_arrival_ms": 100, "name": "crate", "mode_key": "crate [compile mode]",
            "ticket": ticket, "weight": cpu, "cpu": cpu, "memory_bytes": cpu * MEMORY_QUANTUM,
            "priority": "normal", "domain": "host", "domain_capacity": 120,
            "domain_cpu_cap": 120, "domain_memory_cap": 120 * MEMORY_QUANTUM,
            "cpu_cap": 120, "mem_cap": 120 * MEMORY_QUANTUM,
            "bytes_per_permit": MEMORY_QUANTUM, "build": ["build", 120]
        })).unwrap()
    }

    fn lease(waiter: &Waiting) -> Lease {
        Lease {
            version: LEASE_VERSION,
            name: waiter.name.clone(),
            mode_key: waiter.mode_key.clone(),
            weight: waiter.weight,
            cpu: waiter.cpu,
            memory_bytes: waiter.memory_bytes,
            priority: waiter.priority.clone(),
            build_id: waiter.build.as_ref().map(|b| b.0.clone()),
            domain: Some(waiter.domain.clone()),
            bytes_per_permit: waiter.bytes_per_permit,
        }
    }

    fn events(dir: &Path) -> Vec<Event> {
        std::fs::read_to_string(dir.join(FILE))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }

    #[test]
    fn event_schemas_and_escaped_line_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 120);
        let mut waiter = record(u64::MAX, 1);
        let long = "\n🦀".repeat(500);
        waiter.name = Some(long.clone());
        waiter.mode_key = Some(long.clone());
        waiter.domain = long.clone();
        waiter.priority = long.clone();
        waiter.build = Some((long, u64::MAX));
        waiter.cpu = Some(u64::MAX);
        waiter.memory_bytes = Some(u64::MAX);
        waiter.weight = u64::MAX;
        waiter.bytes_per_permit = u64::MAX;
        waiter.domain_capacity = u64::MAX;
        waiter.global_capacity = Some(u64::MAX);
        waiter.cpu_cap = Some(u64::MAX);
        waiter.mem_cap = Some(u64::MAX);
        waiter.domain_cpu_cap = Some(u64::MAX);
        waiter.domain_memory_cap = Some(u64::MAX);

        for kind in ["enqueued", "dequeued", "abandoned", "admitted"] {
            let mut event = Event::new(&pool, kind, &waiter);
            if kind == "dequeued" {
                event.reason = Some("available-memory".into());
            }
            if kind == "admitted" {
                event.rule = Some("ordered".into());
                event.overtook_total = Some(0);
                event.overtook = Some(Vec::new());
            }
            let bytes = encode(event).unwrap();
            assert!(
                bytes.len() < TRANSITION_LINE_BYTES,
                "{kind}: {}",
                bytes.len()
            );
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            for key in [
                "v",
                "ts_ms",
                "mbx_version",
                "pid",
                "name",
                "mode_key",
                "cpu",
                "memory_bytes",
                "domain",
                "build",
                "priority",
                "ticket",
                "first_arrival_ms",
                "waited_ms",
            ] {
                assert!(!value[key].is_null(), "{kind} missing {key}");
            }
            assert_eq!(value["v"], 1);
            assert_eq!(value["pid"], 123);
            assert_eq!(value["event"], kind);
            if kind == "abandoned" {
                assert_eq!(value["queue_limits"]["build_capacity"], u64::MAX);
            }
            assert_eq!(value["waited_ms"], 900);
            serde_json::from_slice::<Event>(&bytes).unwrap();
        }
    }

    #[test]
    fn older_exceptions_share_admission_predicates() {
        let request = record(10, 1);
        let mut older = record(1, 119);
        let mut occupied = record(0, 2);
        for reason in ["exception:priority", "exception:build", "exception:domain"] {
            older.priority = "normal".into();
            older.domain = request.domain.clone();
            older.build = request.build.clone();
            occupied.build = request.build.clone();
            occupied.domain = request.domain.clone();
            match reason {
                "exception:priority" => older.priority = "low".into(),
                "exception:build" => {
                    older.build = Some(("other".into(), 2));
                    occupied.build = older.build.clone();
                }
                _ => {
                    older.domain = "other".into();
                    older.build = None;
                    occupied.build = None;
                    older.domain_cpu_cap = Some(2);
                    occupied.domain = older.domain.clone();
                }
            }
            let trace = std::cell::RefCell::new(Trace::default());
            assert_eq!(
                admission_decision_traced(
                    120,
                    &[lease(&occupied)],
                    [&older].into_iter(),
                    &request,
                    Some(&trace)
                ),
                AdmissionDecision::Ordered
            );
            let trace = trace.into_inner();
            assert_eq!(trace.overtook.len(), 1);
            assert_eq!(trace.overtook[0].reason, reason);
            assert!(trace.overtook[0].slack.is_empty());
        }
    }

    #[test]
    fn passing_reports_actual_prefix_slack_in_all_dimensions_and_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 120);
        let mut request = record(10, 12);
        request.memory_bytes = Some(8 * MEMORY_QUANTUM);
        let older = record(1, 54);
        let live = [lease(&record(0, 54)), lease(&record(0, 54))];
        let trace = std::cell::RefCell::new(Trace::default());
        let decision =
            admission_decision_traced(120, &live, [&older].into_iter(), &request, Some(&trace));
        assert_eq!(decision, AdmissionDecision::Passing);
        let trace = trace.into_inner();
        let waiter = &trace.overtook[0];
        assert_eq!(waiter.reason, "pass");
        for scope in ["global", "domain"] {
            let slack = waiter.slack.iter().find(|s| s.scope == scope).unwrap();
            assert_eq!(slack.dimensions["cpu"], 12);
            assert_eq!(slack.dimensions["memory_bytes"], 10 * MEMORY_QUANTUM + 2);
        }
        assert_eq!(
            waiter
                .slack
                .iter()
                .find(|s| s.scope == "build")
                .unwrap()
                .dimensions["cpu"],
            12
        );
        pool.admitted_event(&request, &live, decision, trace);
        let logged = events(dir.path()).pop().unwrap();
        assert_eq!(logged.free[0].cpu, 12);
        assert_eq!(logged.free[0].memory_bytes, Some(12 * MEMORY_QUANTUM));
        assert_eq!(logged.overtook.as_ref().unwrap()[0].waited_ms, 900);
        let json = serde_json::to_value(logged).unwrap();
        assert!(json["overtook"][0].get("domain").is_none());
        assert!(json["overtook"][0].get("build").is_none());
        assert!(json["overtook"][0].get("priority").is_none());
        let mut legacy = older.clone();
        legacy.cpu = None;
        legacy.memory_bytes = None;
        let trace = std::cell::RefCell::new(Trace::default());
        assert_eq!(
            admission_decision_traced(120, &live, [&legacy].into_iter(), &request, Some(&trace)),
            AdmissionDecision::Passing
        );
        let trace = trace.into_inner();
        for scope in ["legacy:global", "legacy:domain", "legacy:build"] {
            let slack = trace.overtook[0]
                .slack
                .iter()
                .find(|s| s.scope == scope)
                .unwrap();
            assert_eq!(slack.dimensions["weight"], 12);
            assert_eq!(slack.bytes_per_permit, Some(MEMORY_QUANTUM));
        }
    }

    #[test]
    fn admission_truncation_keeps_oldest_entries_and_full_total() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 120);
        let request = record(100, 1);
        let mut older: Vec<_> = (0..40).rev().map(|t| record(t, 1)).collect();
        older.sort_by_key(|w| w.ticket);
        let refs: Vec<_> = older.iter().collect();
        let mut trace = Trace::default();
        trace.initialize(&refs, &[], &request);
        for waiter in &mut trace.overtook {
            waiter.name = Some("\n🦀".repeat(500));
            waiter.domain = Some("other-domain".repeat(100));
            waiter.build = Some(Some("other-build".repeat(100)));
            waiter.priority = Some("other-priority".repeat(100));
        }
        pool.admitted_event(&request, &[], AdmissionDecision::Passing, trace);
        let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(bytes.len() <= ADMITTED_LINE_BYTES);
        let logged: Event = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(logged.overtook_total, Some(40));
        let entries = logged.overtook.unwrap();
        assert!(!entries.is_empty());
        assert!(
            entries.len() < MAX_OVERTAKES,
            "size must also limit the entry count"
        );
        assert_eq!(logged.overtook_logged, Some(entries.len()));
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.ticket, i as u64);
            assert!(serde_json::to_vec(&entry.name).unwrap().len() <= 64);
        }
        assert!(Summary::read(dir.path()).text().contains("omitted"));
        let mut trace = Trace::default();
        trace.initialize(&refs, &[], &request);
        pool.admitted_event(&request, &[], AdmissionDecision::Passing, trace);
        let event = events(dir.path()).pop().unwrap();
        assert_eq!(event.overtook_logged, Some(16));
        assert_eq!(event.overtook_total, Some(40));
    }

    #[test]
    fn oversized_first_overtake_keeps_the_decision_with_totals_only() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 120);
        let request = record(100, 1);
        let older = record(1, 1);
        let mut trace = Trace::default();
        trace.initialize(&[&older], &[], &request);
        for i in 0..100 {
            trace.slack(
                older.ticket,
                "global",
                Resources {
                    cpu: i,
                    memory_bytes: i,
                },
                "cpu",
                1,
            );
        }
        pool.admitted_event(&request, &[], AdmissionDecision::Passing, trace);
        let event = events(dir.path()).pop().unwrap();
        assert_eq!(event.overtook_total, Some(1));
        assert_eq!(event.overtook_logged, Some(0));
        assert!(event.overtook.unwrap().is_empty());
    }

    #[test]
    fn queue_transitions_and_gate_requeues_preserve_first_arrival() {
        for reason in ["available-memory", "pressure", "resumes-pending", "error"] {
            let dir = tempfile::tempdir().unwrap();
            let mut pool = pool(dir.path(), 4);
            let held = pool.try_admit(3, None).unwrap().unwrap();
            let mut queued = None;
            let resources = Resources {
                cpu: 2,
                memory_bytes: 2 * MEMORY_QUANTUM,
            };
            for _ in 0..3 {
                assert!(
                    pool.try_admit_named_at(resources, 2, None, &mut queued, None, 100)
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(
                events(dir.path())
                    .iter()
                    .filter(|e| e.event == "enqueued")
                    .count(),
                1
            );
            match reason {
                "available-memory" => pool.available_memory = || Some(0),
                "pressure" => {
                    pool.pressure = true;
                    pool.pressure_probe = || crate::pressure::Reading {
                        total: Some(100),
                        available: Some(0),
                        ..Default::default()
                    };
                    // Pressure requires two bad samples, at least one interval apart.
                    crate::pressure::sample(
                        &crate::pressure::domain_state(&pool.dir, pool.pressure_domain),
                        500,
                        pool.pressure_probe,
                    )
                    .unwrap();
                }
                "resumes-pending" => {
                    pool.pressure = true;
                    let suspended = dir.path().join("suspended");
                    std::fs::create_dir_all(&suspended).unwrap();
                    std::fs::write(suspended.join("fixture"), "1000").unwrap();
                }
                _ => {
                    std::fs::write(dir.path().join(WAITERS_DIR).join("malformed"), b"bad json")
                        .unwrap();
                }
            }
            // Keep an invalid waiter live so scan_waiters propagates the error.
            let mut bad_lock = if reason == "error" {
                let mut lock =
                    fslock::LockFile::open(&dir.path().join(WAITERS_DIR).join("malformed"))
                        .unwrap();
                lock.lock().unwrap();
                Some(lock)
            } else {
                None
            };
            let outcome = pool.try_admit_named_at(
                resources,
                2,
                Some(2 * MEMORY_QUANTUM),
                &mut queued,
                None,
                100,
            );
            assert_eq!(outcome.is_err(), reason == "error");
            assert!(queued.is_none(), "{reason} must dequeue");
            let logged = events(dir.path());
            let dequeued = logged.last().unwrap();
            assert_eq!(dequeued.event, "dequeued");
            assert_eq!(dequeued.reason.as_deref(), Some(reason));
            assert_eq!(dequeued.waited_ms, 900);
            let retry = pool.try_admit_named_at(
                resources,
                2,
                Some(2 * MEMORY_QUANTUM),
                &mut queued,
                None,
                100,
            );
            assert_eq!(retry.is_err(), reason == "error");
            assert_eq!(
                events(dir.path()).len(),
                logged.len(),
                "a retry without a queue entry must not log another dequeue"
            );

            if let Some(lock) = &mut bad_lock {
                lock.unlock().unwrap();
            }
            drop(bad_lock);
            if reason == "error" {
                std::fs::remove_file(dir.path().join(WAITERS_DIR).join("malformed")).unwrap();
            }
            pool.pressure = false;
            pool.available_memory = || None;
            assert!(
                pool.try_admit_named_at(resources, 2, None, &mut queued, None, 100)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(queued.as_ref().unwrap().record.first_arrival_ms, Some(100));
            drop(held);
            assert!(
                pool.try_admit_named_at(resources, 2, None, &mut queued, None, 100)
                    .unwrap()
                    .is_some()
            );
            let event = events(dir.path()).pop().unwrap();
            assert_eq!(event.event, "admitted");
            assert_eq!(event.waited_ms, 900);
        }
    }

    #[test]
    fn abandoned_scan_uses_mtime_and_legacy_live_waiters_inherit_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut pool = pool(dir.path(), 4);
        pool.clock = || 2_000_000;
        let waiters = dir.path().join(WAITERS_DIR);
        std::fs::create_dir_all(&waiters).unwrap();
        let mut record = record(1, 2);
        record.first_arrival_ms = None;
        record.pid = None;
        let path = waiters.join("dead");
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_millis(1000))
            .unwrap();
        let mut lock = fslock::LockFile::open(&path).unwrap();
        lock.lock().unwrap();
        let live = scan_waiters_logged(&waiters, Some(&pool)).unwrap();
        assert_eq!(live[0].first_arrival_ms, Some(1000));
        assert!(events(dir.path()).is_empty());
        drop(lock);
        // fslock explicitly truncates on unlock; process death instead leaves
        // the record intact. Restore that kernel-death fixture before scanning.
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_millis(1000))
            .unwrap();
        assert!(
            scan_waiters_logged(&waiters, Some(&pool))
                .unwrap()
                .is_empty()
        );
        let events = events(dir.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "abandoned");
        assert_eq!(events[0].ticket, 1);
        assert_eq!(events[0].waited_ms, 1_999_000);
        assert!(!path.exists());
        let summary = Summary::read(dir.path());
        assert_eq!(summary.abandoned_long_waits[0].age_ms, 1_999_000);
    }

    #[test]
    fn events_gate_disables_all_transition_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut pool = pool(dir.path(), 4);
        pool.events = false;
        let held = pool.try_admit(3, None).unwrap().unwrap();
        let mut queued = None;
        assert!(
            pool.try_admit_queued(2, None, &mut queued)
                .unwrap()
                .is_none()
        );
        pool.available_memory = || Some(0);
        assert!(
            pool.try_admit_queued(2, Some(2 * MEMORY_QUANTUM), &mut queued)
                .unwrap()
                .is_none()
        );
        let waiter = record(1, 1);
        pool.decision_event("abandoned", &waiter, Some(100));
        assert!(!dir.path().join(FILE).exists());
        drop(held);
    }

    #[test]
    fn rotation_uses_try_lock_and_replaces_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 120);
        let path = dir.path().join(FILE);
        File::create(&path)
            .unwrap()
            .set_len(8 * 1024 * 1024 + 1)
            .unwrap();
        std::fs::write(dir.path().join(PREVIOUS), b"old").unwrap();
        let mut lock = fslock::LockFile::open(&dir.path().join(LOCK)).unwrap();
        lock.lock().unwrap();
        append(dir.path(), Event::new(&pool, "enqueued", &record(1, 1))).unwrap();
        assert_eq!(std::fs::read(dir.path().join(PREVIOUS)).unwrap(), b"old");
        drop(lock);
        append(dir.path(), Event::new(&pool, "enqueued", &record(1, 1))).unwrap();
        assert!(std::fs::metadata(dir.path().join(PREVIOUS)).unwrap().len() > 8 * 1024 * 1024);
        assert!(std::fs::metadata(path).unwrap().len() < 1024);
    }

    #[test]
    fn stats_fixture_reads_both_generations_and_keeps_all_ranked_entries_in_json() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 120);
        for rule in ["ordered", "passing"] {
            let request = record(100, 1);
            let mut trace = Trace::default();
            let mut older: Vec<_> = (1..=7).map(|i| record(i, 1)).collect();
            for (i, waiter) in older.iter_mut().enumerate() {
                waiter.first_arrival_ms = Some((i * 100) as u64);
                if i == 0 || rule == "ordered" {
                    waiter.priority = "low".into();
                }
            }
            trace.initialize(&older.iter().collect::<Vec<_>>(), &[], &request);
            let decision = if rule == "ordered" {
                AdmissionDecision::Ordered
            } else {
                AdmissionDecision::Passing
            };
            pool.admitted_event(&request, &[], decision, trace);
            if rule == "ordered" {
                std::fs::rename(dir.path().join(FILE), dir.path().join(PREVIOUS)).unwrap();
            }
        }
        let mut dequeued = Event::new(&pool, "dequeued", &record(1, 1));
        dequeued.reason = Some("pressure".into());
        append(dir.path(), dequeued).unwrap();
        for age in [900_000, 900_001] {
            let mut abandoned = Event::new(&pool, "abandoned", &record(1, 1));
            abandoned.waited_ms = age;
            append(dir.path(), abandoned).unwrap();
        }
        use std::io::Write;
        let mut file = File::options()
            .append(true)
            .open(dir.path().join(FILE))
            .unwrap();
        file.write_all(b"malformed\n\xff\n").unwrap();
        let summary = Summary::read(dir.path());
        assert_eq!(summary.admitted_by_rule["ordered"], 1);
        assert_eq!(summary.admitted_by_rule["passing"], 1);
        assert_eq!(summary.admissions_overtaking_by_reason["pass"], 1);
        assert_eq!(
            summary.admissions_overtaking_by_reason["exception:priority"],
            2
        );
        assert_eq!(summary.dequeued_by_reason["pressure"], 1);
        assert_eq!(summary.abandoned_long_waits.len(), 1);
        assert_eq!(summary.largest_overtakes.len(), 14);
        let text = summary.text();
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("  Overtook "))
                .count(),
            5
        );
        assert!(text.contains("Abandoned crate:"));
        let json = serde_json::to_value(summary).unwrap();
        assert_eq!(json["largest_overtakes"].as_array().unwrap().len(), 14);
        assert_eq!(json["largest_overtakes"][0]["waited_ms"], 1000);
    }
}
