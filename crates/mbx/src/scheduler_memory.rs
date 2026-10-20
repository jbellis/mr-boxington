//! Windowed log-space prediction. Legacy ledger values are written separately.
use super::{MAX_LEDGER_ENTRIES, MemoryLedger};
use std::collections::BTreeMap;

pub(super) const WINDOW: usize = 16;
const MAX_RESIDUALS: usize = 256;
const MIN_RESIDUALS: usize = 50;
const MIN_POOL_KEYS: usize = 10;
/// Samples a key needs, like a key qualifying for the pooled spread, before
/// its own estimate may exceed the previous rule's charge.
const MIN_KEY_SAMPLES: usize = 4;
const DEFAULT_SIGMA_POOL: f64 = 0.5;
const NU0: usize = 4;
pub(super) const KINDS: [&str; 4] = ["compile", "link", "test-binary", "nextest-test"];
// Smallest p passing the 2026-10-18 causal 2D-P1 replay: 4.979% exceedance,
// max concurrent actual peaks 85.91 GiB (<98 GiB physical). Analysis and
// snapshot: mr-boxington-wt/replay/windowed/report.md and results.json.
pub(super) const TARGET_P: f64 = 0.95;
// One-sided Student-t p=.95, df 1..30; the duration-estimator replay table.
const T_QUANTILES: [f64; 30] = [
    6.313751514675,
    2.919985580354,
    2.353363434802,
    2.131846786327,
    2.015048373333,
    1.943180280515,
    1.89457860509,
    1.859548037531,
    1.833112932656,
    1.812461122812,
    1.795884818704,
    1.782287555649,
    1.770933395987,
    1.761310135775,
    1.753050355693,
    1.745883676276,
    1.739606726075,
    1.734063606617,
    1.729132811521,
    1.724718242921,
    1.720742902812,
    1.71714437438,
    1.713871527747,
    1.710882079909,
    1.708140761252,
    1.705617919759,
    1.703288445722,
    1.701130934266,
    1.699127026534,
    1.697260886594,
];
const NORMAL_QUANTILE: f64 = 1.6448536269514715;

#[derive(Debug, Clone, Copy)]
pub(super) struct Features {
    pub mean: f64,
    pub scale: f64,
}

#[derive(Clone, Copy)]
pub(super) struct Sample {
    pub peak: u64,
    pub features: Option<Features>,
    pub calibrate: bool,
}

impl Features {
    // A key admitted without samples has no residual, even if another
    // process records its first sample while this request runs.
    pub(super) const COLD: Self = Self {
        mean: 0.0,
        scale: 0.0,
    };
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Estimate {
    pub n: usize,
    pub window_max: u64,
    pub bound: u64,
    pub charge: u64,
    pub q: f64,
    pub features: Features,
}

pub(super) fn kind(key: &str) -> &'static str {
    if key.ends_with(" [link]") {
        "link"
    } else if key.contains(" [test") {
        "test-binary"
    } else {
        "compile"
    }
}

fn moments(values: &[u64]) -> (f64, f64) {
    let mean = values.iter().map(|p| (*p as f64).ln()).sum::<f64>() / values.len() as f64;
    let variance = if values.len() > 1 {
        values
            .iter()
            .map(|p| ((*p as f64).ln() - mean).powi(2))
            .sum::<f64>()
            / (values.len() - 1) as f64
    } else {
        0.0
    };
    (mean, variance)
}

pub(super) fn sigma_pool(ledger: &MemoryLedger, kind: &str) -> f64 {
    ledger
        .pooled_spreads
        .get(kind)
        .copied()
        .unwrap_or(DEFAULT_SIGMA_POOL)
}

fn conformal_rank(n: usize) -> usize {
    ((TARGET_P * (n + 1) as f64).ceil() as usize).min(n)
}

pub(super) fn quantile(ledger: &MemoryLedger, kind: &str, n: usize) -> f64 {
    let scores = ledger
        .memory_residuals
        .get(kind)
        .map_or(&[][..], Vec::as_slice);
    let q = history_quantile(scores, n);
    if scores.iter().filter(|r| r.is_finite()).count() < MIN_RESIDUALS {
        // The kind summary has no key variance; report its cold-key log ratio.
        q * sigma_pool(ledger, kind) * (1.0 + 1.0 / n.max(1) as f64).sqrt()
    } else {
        q
    }
}

fn history_quantile(history: &[f64], n: usize) -> f64 {
    let mut residuals: Vec<_> = history.iter().copied().filter(|r| r.is_finite()).collect();
    if residuals.len() >= MIN_RESIDUALS {
        residuals.sort_by(f64::total_cmp);
        // Finite-sample conformal rank, clamped to the available history.
        let rank = conformal_rank(residuals.len());
        residuals[rank - 1]
    } else {
        let df = NU0 + n.max(1) - 1;
        T_QUANTILES.get(df - 1).copied().unwrap_or(NORMAL_QUANTILE)
    }
}

pub(super) fn estimate(ledger: &MemoryLedger, key: &str, kind: &str) -> Option<Estimate> {
    let values = ledger.memory_windows.get(key)?;
    let window_max = *values.iter().max()?;
    let n = values.len();
    let (mean, variance) = moments(values);
    let s = ((NU0 as f64 * sigma_pool(ledger, kind).powi(2) + (n - 1) as f64 * variance)
        / (NU0 + n - 1) as f64)
        .sqrt();
    let log_max = (window_max as f64).ln();
    // Own-key features encode ln(peak / admission-time window maximum).
    // Sibling features retain their independent standardized score.
    let features = Features {
        mean: log_max,
        scale: 1.0,
    };
    let enough = ledger
        .memory_residuals
        .get(kind)
        .is_some_and(|h| h.iter().filter(|r| r.is_finite()).count() >= MIN_RESIDUALS);
    let q = if enough {
        quantile(ledger, kind, n)
    } else {
        // Keep the existing Student-t bound until calibration has 50 scores.
        // Its pooled spread allows an increase even with one observation. Express
        // that bound as a log ratio, so q has the same units after warm-up.
        mean + history_quantile(&[], n) * s * (1.0 + 1.0 / n as f64).sqrt() - log_max
    };
    // Float-to-integer casts saturate at u64::MAX for an overflowing bound.
    let bound = (window_max as f64 * q.exp()).ceil() as u64;
    Some(Estimate {
        n,
        window_max,
        bound,
        charge: window_max.max(bound),
        q,
        features,
    })
}

/// Whether a kind's pooled spread comes from data rather than the default.
fn calibrated(ledger: &MemoryLedger, kind_name: &str) -> bool {
    ledger
        .memory_windows
        .iter()
        .filter(|(key, values)| kind(key) == kind_name && values.len() >= 4)
        .count()
        >= MIN_POOL_KEYS
}

/// While a kind's spread is still the default, a window's bound is several
/// times wider than this workload's measured variation, so a key's first
/// samples would be charged far more than the previous rule ever charged.
/// Until enough keys calibrate the spread, cap the charge at the previous
/// rule's, and never below the window's largest peak.
fn warm_up_charge(
    ledger: &MemoryLedger,
    kind_name: &str,
    estimate: &Estimate,
    previous: Option<u64>,
) -> u64 {
    // Per key as well as per kind: a calibrated kind can still have a wide
    // spread, and a key with one or two samples would then be charged far
    // above anything it was charged before, without receiving a new sample.
    let warming = !calibrated(ledger, kind_name) || estimate.n < MIN_KEY_SAMPLES;
    match previous {
        Some(previous) if warming => estimate.charge.min(previous.max(estimate.window_max)),
        _ => estimate.charge,
    }
}

/// The charge f47d416's rule gives a compile mode, when it has any history.
fn previous_compile_charge(ledger: &MemoryLedger, name: &str, key: &str) -> Option<u64> {
    let bare = ledger.crates.get(name).copied();
    let Some(peak) = ledger.crates.get(key).copied() else {
        return bare;
    };
    // An old writer can discard windows. Preserve its charge, including mode
    // headroom or the crate-wide fallback before three successes.
    let old_charge = if ledger.compile_samples.get(key).copied().unwrap_or(0) >= 3 {
        let padded = peak.saturating_add(peak.div_ceil(2));
        bare.map_or(padded, |bare| padded.min(bare))
    } else {
        bare.unwrap_or(peak)
    };
    Some(peak.max(old_charge))
}

/// `default` is what the previous rule charged a compile with no recorded
/// history: light crates never get a legacy entry, yet they do get windows.
fn compile_window_charge(
    ledger: &MemoryLedger,
    name: &str,
    key: &str,
    default: u64,
) -> Option<u64> {
    let estimate = estimate(ledger, key, "compile")?;
    let previous =
        previous_compile_charge(ledger, name, key).or_else(|| (default > 0).then_some(default));
    Some(warm_up_charge(ledger, "compile", &estimate, previous))
}

pub(super) fn compile_prediction(
    ledger: &MemoryLedger,
    name: &str,
    key: &str,
    default: u64,
) -> (Option<u64>, &'static str) {
    if let Some(charge) = compile_window_charge(ledger, name, key, default) {
        return (Some(charge), "window");
    }
    if ledger.crates.contains_key(key) {
        return (previous_compile_charge(ledger, name, key), "legacy");
    }
    if let Some(estimate) = sibling_estimate(ledger, name, default) {
        return (Some(estimate.charge), "sibling");
    }
    match ledger.crates.get(name).copied() {
        Some(peak) => (Some(peak), "legacy"),
        None => (None, "default"),
    }
}

/// A new mode is another draw from the crate's mode maxima, rather than
/// another run of whichever sibling happens to have the largest charge.
pub(super) fn sibling_estimate(
    ledger: &MemoryLedger,
    name: &str,
    default: u64,
) -> Option<Estimate> {
    let prefix = format!("{name} [compile ");
    let mut maxima = Vec::new();
    let mut floor = 0;
    for (key, window) in &ledger.memory_windows {
        if key.starts_with(&prefix)
            && let Some(maximum) = window.iter().max()
            && let Some(charge) = compile_window_charge(ledger, name, key, default)
        {
            maxima.push(*maximum);
            floor = floor.max(charge);
        }
    }
    let window_max = *maxima.iter().max()?;
    let n = maxima.len();
    let (mean, variance) = moments(&maxima);
    let spread = ((NU0 as f64 * ledger.sibling_spread.powi(2) + (n - 1) as f64 * variance)
        / (NU0 + n - 1) as f64)
        .sqrt();
    let features = Features {
        mean,
        scale: spread * (1.0 + 1.0 / n as f64).sqrt(),
    };
    let q = sibling_quantile(ledger, n);
    let bound = (mean + q * features.scale).exp().ceil() as u64;
    let charge = floor.max(bound);
    // A mode without samples is never charged more than the previous rule
    // charged it: the crate-wide maximum, or one share when the crate has
    // none. This cap wins over the sibling floor.
    let cap = ledger
        .crates
        .get(name)
        .copied()
        .or_else(|| (default > 0).then_some(default));
    let charge = cap.map_or(charge, |cap| charge.min(cap));
    Some(Estimate {
        n,
        window_max,
        bound,
        charge,
        q,
        features,
    })
}

pub(super) fn sibling_quantile(ledger: &MemoryLedger, n: usize) -> f64 {
    history_quantile(&ledger.sibling_residuals, n)
}

pub(super) fn default_sibling_spread() -> f64 {
    DEFAULT_SIGMA_POOL
}

fn pooled_median(values: &mut [f64]) -> f64 {
    if values.len() < MIN_POOL_KEYS {
        return DEFAULT_SIGMA_POOL;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn sanitize_residuals(history: &mut Vec<f64>) {
    history.retain(|r| r.is_finite());
    history.drain(..history.len().saturating_sub(MAX_RESIDUALS));
}

/// Sibling calibration is separate from the requested mode's own window.
pub(super) fn record_sibling(
    ledger: &mut MemoryLedger,
    peak: u64,
    features: Option<Features>,
    calibrate: bool,
) {
    if calibrate
        && peak > 0
        && let Some(features) = features
        && features.scale > 0.0
    {
        let residual = ((peak as f64).ln() - features.mean) / features.scale;
        if residual.is_finite() {
            ledger.sibling_residuals.push(residual);
            sanitize_residuals(&mut ledger.sibling_residuals);
        }
    }
}

/// Shared by admission and stats, including old-writer and eviction fallbacks.
pub(super) fn link_prediction(
    ledger: &MemoryLedger,
    key: &str,
    profile: Option<&str>,
    floor: u64,
) -> (Option<u64>, &'static str) {
    let window = match profile {
        Some(profile) => ledger
            .profile_link_peaks
            .get(profile)
            .map_or(&[][..], Vec::as_slice),
        None => ledger.link_peaks.as_slice(),
    };
    let fallback = match ledger.crates.get(key).copied() {
        Some(peak) => (peak, "legacy"),
        None => match window.iter().copied().max() {
            Some(peak) => (peak, "link-window"),
            None => (floor, "link-floor"),
        },
    };
    if let Some(estimate) = estimate(ledger, key, "link") {
        return (
            Some(warm_up_charge(ledger, "link", &estimate, Some(fallback.0))),
            "window",
        );
    }
    (Some(fallback.0), fallback.1)
}

fn trim(ledger: &mut MemoryLedger) {
    while ledger.memory_windows.len() > MAX_LEDGER_ENTRIES {
        let smallest = ledger
            .memory_windows
            .iter()
            .min_by_key(|(_, peaks)| peaks.iter().max().copied().unwrap_or(0))
            .map(|(key, _)| key.clone())
            .expect("overfull windows");
        ledger.memory_windows.remove(&smallest);
    }
}

/// Sanitize only the new fields. Old writers must see unchanged legacy fields.
pub(super) fn prepare(ledger: &mut MemoryLedger) {
    for window in ledger.memory_windows.values_mut() {
        window.retain(|p| *p > 0);
        window.drain(..window.len().saturating_sub(WINDOW));
    }
    ledger.memory_windows.retain(|key, w| {
        // Native cc:<filename> keys collide across libraries; their history
        // must remain a never-lowered legacy maximum, not a rolling window.
        (key.contains(" [compile ") || key.ends_with(" [link]")) && !w.is_empty()
    });
    trim(ledger);
    ledger
        .memory_residuals
        .retain(|kind, _| KINDS.contains(&kind.as_str()));
    for residuals in ledger.memory_residuals.values_mut() {
        sanitize_residuals(residuals);
    }
    sanitize_residuals(&mut ledger.sibling_residuals);
    let mut modes: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for (key, window) in &ledger.memory_windows {
        if let Some((name, _)) = key.split_once(" [compile ") {
            modes
                .entry(name)
                .or_default()
                .push(*window.iter().max().expect("nonempty window"));
        }
    }
    let mut between: Vec<_> = modes
        .values()
        .filter(|v| v.len() >= 2)
        .map(|v| moments(v).1.sqrt())
        .collect();
    ledger.sibling_spread = pooled_median(&mut between);
    let mut spreads: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for (key, values) in &ledger.memory_windows {
        if values.len() >= 4 {
            spreads
                .entry(kind(key))
                .or_default()
                .push(moments(values).1.sqrt());
        }
    }
    ledger.pooled_spreads = KINDS
        .into_iter()
        .map(|kind| {
            let values = spreads.entry(kind).or_default();
            let sigma = pooled_median(values);
            (kind.to_string(), sigma)
        })
        .collect();
}

/// Called under the ledger lock, before appending the new peak. Only successful
/// unwrapped runs calibrate; an OOM still appends its doubled peak as a floor.
pub(super) fn record(
    ledger: &mut MemoryLedger,
    key: &str,
    kind: &str,
    peak: u64,
    features: Option<Features>,
    calibrate: bool,
) {
    if peak == 0 {
        return;
    }
    if calibrate
        && let Some(features) = features
        && features.scale > 0.0
    {
        let residual = ((peak as f64).ln() - features.mean) / features.scale;
        if residual.is_finite() {
            let history = ledger.memory_residuals.entry(kind.to_string()).or_default();
            history.push(residual);
            sanitize_residuals(history);
        }
    }
    let window = ledger.memory_windows.entry(key.to_string()).or_default();
    window.push(peak);
    window.drain(..window.len().saturating_sub(WINDOW));
    trim(ledger);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_ratio_score_and_charge_ignore_key_spread() {
        let mut ledger = MemoryLedger::default();
        ledger.memory_windows.insert("narrow".into(), vec![100; 16]);
        ledger.memory_windows.insert("wide".into(), vec![10, 100]);
        ledger
            .memory_residuals
            .insert("compile".into(), vec![1.5_f64.ln(); 50]);
        for key in ["narrow", "wide"] {
            let e = estimate(&ledger, key, "compile").unwrap();
            assert_eq!(e.charge, 150);
            record(&mut ledger, key, "compile", 150, Some(e.features), true);
            assert!(
                (ledger.memory_residuals["compile"].last().unwrap() - 1.5_f64.ln()).abs() < 1e-12
            );
        }
    }

    #[test]
    fn repeated_key_scores_keep_the_latest_256_without_a_key_cap() {
        let mut ledger = MemoryLedger::default();
        let features = Features {
            mean: 100.0_f64.ln(),
            scale: 1.0,
        };
        for peak in 101..=400 {
            record(&mut ledger, "repeat", "compile", peak, Some(features), true);
        }
        let history = &ledger.memory_residuals["compile"];
        assert_eq!(history.len(), MAX_RESIDUALS);
        assert!((history[0] - (145.0_f64 / 100.0).ln()).abs() < 1e-12);
        assert!((history[MAX_RESIDUALS - 1] - 4.0_f64.ln()).abs() < 1e-12);
        let encoded = serde_json::to_value(&ledger).unwrap();
        assert_eq!(
            encoded["memory_log_ratios"]["compile"]
                .as_array()
                .unwrap()
                .len(),
            MAX_RESIDUALS
        );
        assert!(encoded["memory_log_ratios"]["compile"][0].is_number());
    }

    #[test]
    fn old_ledger_scores_are_ignored_without_losing_windows() {
        let mut ledger: MemoryLedger = serde_json::from_str(
            r#"{
            "version":1,"crates":{"crate":100},"link_peaks":[],
            "memory_windows":{"crate [compile a]":[100]},
            "memory_residuals":{"compile":[7.0,8.0]},
            "sibling_residuals":[2.0]
        }"#,
        )
        .unwrap();
        prepare(&mut ledger);
        assert!(ledger.memory_residuals.is_empty());
        assert_eq!(ledger.memory_windows["crate [compile a]"], vec![100]);
        assert_eq!(ledger.crates["crate"], 100);
        assert_eq!(ledger.sibling_residuals, vec![2.0]);
        assert_eq!(
            estimate(&ledger, "crate [compile a]", "compile")
                .unwrap()
                .charge,
            452
        );
    }

    fn siblings(values: &[u64]) -> MemoryLedger {
        let mut ledger = MemoryLedger::default();
        for (i, value) in values.iter().enumerate() {
            ledger
                .memory_windows
                .insert(format!("crate [compile {i}]"), vec![*value; 4]);
        }
        ledger
    }

    #[test]
    fn sibling_fixed_vectors_and_shrinkage() {
        for (values, expected) in [
            (vec![100], 452),
            (vec![100, 200], 484),
            (vec![100, 200, 300, 400], 704),
        ] {
            let mut ledger = siblings(&values);
            let e = sibling_estimate(&ledger, "crate", 0).unwrap();
            assert_eq!(e.bound, expected);
            assert_eq!(e.n, values.len());
            assert!((e.features.mean - moments(&values).0).abs() < 1e-12);
            ledger.sibling_spread = 0.25;
            let shrunk = sibling_estimate(&ledger, "crate", 0).unwrap();
            let k = values.len();
            let variance = moments(&values).1;
            let expected_scale =
                ((4.0 * 0.25_f64.powi(2) + (k - 1) as f64 * variance) / (4 + k - 1) as f64).sqrt()
                    * (1.0 + 1.0 / k as f64).sqrt();
            assert!((shrunk.features.scale - expected_scale).abs() < 1e-12);
            assert!(shrunk.features.scale < e.features.scale);
        }
    }

    #[test]
    fn sibling_charge_without_crate_entry_is_capped_at_default() {
        let ledger = siblings(&[50]);
        let estimate = sibling_estimate(&ledger, "crate", 200).unwrap();
        assert!(estimate.bound > 200);
        assert_eq!(estimate.charge, 200);
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile new]", 200),
            (Some(200), "sibling")
        );
    }

    #[test]
    fn sibling_quantile_switches_independently_and_charge_has_floor_then_cap() {
        let mut ledger = siblings(&[100, 200]);
        ledger
            .memory_residuals
            .insert("compile".into(), vec![99.0; 50]);
        ledger.sibling_residuals = vec![9.0; 49];
        assert_eq!(sibling_quantile(&ledger, 2), T_QUANTILES[4]);
        ledger.sibling_residuals = (0..50).map(|x| x as f64).collect();
        assert_eq!(sibling_quantile(&ledger, 2), 48.0);
        ledger.sibling_residuals = vec![-10.0; 50];
        ledger
            .memory_residuals
            .insert("compile".into(), vec![-10.0; 50]);
        assert_eq!(sibling_estimate(&ledger, "crate", 0).unwrap().charge, 200);
        ledger.crates.insert("crate".into(), 150);
        // Cap takes precedence even when a sibling's own peak exceeds it.
        assert_eq!(sibling_estimate(&ledger, "crate", 0).unwrap().charge, 150);
        ledger.sibling_residuals.clear();
        ledger.crates.insert("crate".into(), 300);
        assert_eq!(sibling_estimate(&ledger, "crate", 0).unwrap().charge, 300);
        ledger.crates.remove("crate");
        let estimate = sibling_estimate(&ledger, "crate", 0).unwrap();
        assert!(estimate.bound > 300);
        assert_eq!(estimate.charge, estimate.bound);
    }

    #[test]
    fn sibling_live_heavier_mode_is_covered() {
        let gib = (1_u64 << 30) as f64;
        let mut ledger = siblings(&[(0.10 * gib) as u64, (0.36 * gib) as u64]);
        ledger
            .memory_residuals
            .insert("compile".into(), vec![0.0; 50]);
        ledger.crates.insert("crate".into(), gib as u64);
        let e = sibling_estimate(&ledger, "crate", 0).unwrap();
        assert!(e.charge as f64 > 0.52 * gib);
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile opt3]", 0),
            (Some(e.charge), "sibling")
        );
    }

    #[test]
    fn sibling_residuals_roll_and_do_not_touch_own_histories() {
        let mut ledger = MemoryLedger::default();
        let features = Features {
            mean: 100.0_f64.ln(),
            scale: 1.0,
        };
        for _ in 0..MAX_RESIDUALS + 20 {
            record_sibling(&mut ledger, 100, Some(features), true);
        }
        assert_eq!(ledger.sibling_residuals, vec![0.0; MAX_RESIDUALS]);
        record_sibling(&mut ledger, 200, Some(features), false);
        record_sibling(&mut ledger, 200, None, true);
        record_sibling(&mut ledger, 200, Some(Features::COLD), true);
        record_sibling(&mut ledger, 0, Some(features), true);
        assert_eq!(ledger.sibling_residuals, vec![0.0; MAX_RESIDUALS]);
        assert!(ledger.memory_windows.is_empty());
        assert!(ledger.memory_residuals.is_empty());
    }

    #[test]
    fn sibling_pool_prepare_eviction_and_serde_preserve_new_history() {
        let mut ledger = MemoryLedger::default();
        for i in 0..MIN_POOL_KEYS {
            ledger
                .memory_windows
                .insert(format!("crate{i} [compile a]"), vec![100]);
            ledger
                .memory_windows
                .insert(format!("crate{i} [compile b]"), vec![200]);
        }
        ledger.sibling_residuals = vec![1.0; MAX_RESIDUALS + 20];
        ledger.sibling_residuals.push(f64::NAN);
        prepare(&mut ledger);
        let expected = 2.0_f64.ln() / 2.0_f64.sqrt();
        assert!((ledger.sibling_spread - expected).abs() < 1e-12);
        assert_eq!(ledger.sibling_residuals, vec![1.0; MAX_RESIDUALS]);
        let encoded = serde_json::to_vec(&ledger).unwrap();
        let mut decoded: MemoryLedger = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.sibling_spread, ledger.sibling_spread);
        prepare(&mut decoded);
        assert_eq!(decoded.sibling_spread, ledger.sibling_spread);
        assert_eq!(decoded.sibling_residuals, ledger.sibling_residuals);
        // Eviction does not erase global calibration. The derived pool must
        // still reflect only the surviving crates, falling back below ten.
        for i in 0..MAX_LEDGER_ENTRIES {
            record(
                &mut decoded,
                &format!("heavy{i} [compile a]"),
                "compile",
                1000,
                None,
                true,
            );
        }
        assert_eq!(decoded.sibling_residuals, ledger.sibling_residuals);
        assert_eq!(decoded.sibling_spread, ledger.sibling_spread);
        prepare(&mut decoded);
        assert_eq!(decoded.sibling_spread, DEFAULT_SIGMA_POOL);
        let mut old: MemoryLedger =
            serde_json::from_str(r#"{"version":1,"crates":{},"link_peaks":[]}"#).unwrap();
        assert_eq!(old.sibling_spread, DEFAULT_SIGMA_POOL);
        assert!(old.sibling_residuals.is_empty());
        prepare(&mut old);
        assert_eq!(old.sibling_spread, DEFAULT_SIGMA_POOL);
    }

    fn calibrate(ledger: &mut MemoryLedger, kind_name: &str) {
        for index in 0..MIN_POOL_KEYS {
            let key = match kind_name {
                "link" => format!("calibration-{index} [link]"),
                "test-binary" => format!("calibration-{index} [test]"),
                _ => format!("calibration-{index} [compile mode]"),
            };
            ledger.memory_windows.insert(key, vec![100; 4]);
        }
    }

    #[test]
    fn warmup_compile_cap_uses_previous_charge_and_respects_window_max() {
        let key = "crate [compile mode]";
        let mut ledger = MemoryLedger::default();
        ledger.crates.insert("crate".into(), 200);
        ledger.memory_windows.insert(key.into(), vec![100]);
        let raw = estimate(&ledger, key, "compile").unwrap().charge;
        assert!(raw > 200);
        assert_eq!(compile_window_charge(&ledger, "crate", key, 0), Some(200));

        ledger.crates.insert("crate".into(), 200);
        ledger.memory_windows.insert(key.into(), vec![500]);
        let raw = estimate(&ledger, key, "compile").unwrap().charge;
        assert!(raw > 500);
        assert_eq!(compile_window_charge(&ledger, "crate", key, 0), Some(500));
    }

    #[test]
    fn calibrated_compile_kind_uses_raw_estimate() {
        let key = "crate [compile mode]";
        let mut ledger = MemoryLedger::default();
        ledger.crates.insert("crate".into(), 200);
        ledger.memory_windows.insert(key.into(), vec![100; 4]);
        calibrate(&mut ledger, "compile");
        let raw = estimate(&ledger, key, "compile").unwrap().charge;
        assert!(raw > 200);
        assert_eq!(compile_window_charge(&ledger, "crate", key, 0), Some(raw));
    }

    #[test]
    fn link_warmup_uses_legacy_profile_and_floor_fallbacks() {
        let key = "crate [link]";
        let mut legacy = MemoryLedger::default();
        legacy.crates.insert(key.into(), 200);
        legacy.memory_windows.insert(key.into(), vec![100]);
        let raw = estimate(&legacy, key, "link").unwrap().charge;
        assert!(raw > 200);
        assert_eq!(
            link_prediction(&legacy, key, None, 50),
            (Some(200), "window")
        );

        let mut profile = MemoryLedger::default();
        profile
            .profile_link_peaks
            .insert("release".into(), vec![200]);
        profile.memory_windows.insert(key.into(), vec![100]);
        assert_eq!(
            link_prediction(&profile, key, Some("release"), 50),
            (Some(200), "window")
        );

        let mut floor = MemoryLedger::default();
        floor.memory_windows.insert(key.into(), vec![5000]);
        let raw = estimate(&floor, key, "link").unwrap().charge;
        assert!(raw > 1000);
        assert_eq!(
            link_prediction(&floor, key, None, 1000),
            (Some(5000), "window"),
            "the charge cannot fall below the largest observed window peak"
        );
    }

    #[test]
    fn compile_window_without_any_previous_rule_value_or_default_is_not_capped() {
        // With no default (CPU-only scheduling) there is nothing to cap at; with
        // a default, see warm_up_caps_a_key_without_history_at_the_default_charge.
        let key = "crate [compile mode]";
        let mut ledger = MemoryLedger::default();
        ledger.memory_windows.insert(key.into(), vec![100]);
        let raw = estimate(&ledger, key, "compile").unwrap().charge;
        assert_eq!(compile_window_charge(&ledger, "crate", key, 0), Some(raw));
    }
    #[test]
    fn fixed_vectors_and_shrinkage() {
        for (n, expected) in [(1, 452), (2, 484), (3, 583), (16, 2533)] {
            let mut ledger = MemoryLedger::default();
            ledger
                .memory_windows
                .insert("key".into(), (1..=n).map(|x| x * 100).collect());
            let estimate = estimate(&ledger, "key", "compile").unwrap();
            assert_eq!(estimate.charge, expected, "n={n}");
            assert_eq!(estimate.n, n as usize);
        }
        let mut ledger = MemoryLedger::default();
        ledger.memory_windows.insert("key".into(), vec![100, 200]);
        ledger.pooled_spreads.insert("compile".into(), 0.25);
        let e = estimate(&ledger, "key", "compile").unwrap();
        let key_variance = 2.0_f64.ln().powi(2) / 2.0;
        let expected = ((4.0 * 0.25_f64.powi(2) + key_variance) / 5.0).sqrt() * 1.5_f64.sqrt();
        assert!(
            (e.q - (moments(&[100, 200]).0 + T_QUANTILES[4] * expected - 200.0_f64.ln())).abs()
                < 1e-12
        );
    }
    #[test]
    fn t_fallback_nearest_rank_calibration_and_window_floor() {
        let mut ledger = MemoryLedger::default();
        ledger.memory_windows.insert("key".into(), vec![100]);
        assert_eq!(
            quantile(&ledger, "compile", 1),
            T_QUANTILES[3] * 0.5 * 2.0_f64.sqrt()
        );
        assert_eq!(
            quantile(&ledger, "compile", 40),
            NORMAL_QUANTILE * 0.5 * 1.025_f64.sqrt()
        );
        ledger
            .memory_residuals
            .insert("compile".into(), vec![-10.0; 49]);
        assert_eq!(
            quantile(&ledger, "compile", 1),
            T_QUANTILES[3] * 0.5 * 2.0_f64.sqrt()
        );
        ledger
            .memory_residuals
            .insert("compile".into(), (0..50).map(|i| i as f64).collect());
        assert_eq!(quantile(&ledger, "compile", 1), 48.0);
        ledger
            .memory_residuals
            .insert("compile".into(), vec![-10.0; 50]);
        let e = estimate(&ledger, "key", "compile").unwrap();
        assert!(e.bound < 100);
        assert_eq!(e.charge, 100);
        assert_eq!(
            quantile(&ledger, "link", 1),
            T_QUANTILES[3] * 0.5 * 2.0_f64.sqrt()
        );
    }
    #[test]
    fn finite_sample_rank_matches_review_scenario_and_clamps_to_history() {
        let mut ledger = MemoryLedger::default();
        ledger.memory_windows.insert("key".into(), vec![100; 16]);
        ledger
            .memory_residuals
            .insert("compile".into(), (0..50).map(|i| i as f64 / 10.0).collect());
        assert_eq!(quantile(&ledger, "compile", 16), 4.8);
        assert_eq!(
            estimate(&ledger, "key", "compile").unwrap().charge,
            (100.0 * 4.8_f64.exp()).ceil() as u64
        );
        ledger
            .memory_residuals
            .insert("compile".into(), (0..256).map(|i| i as f64).collect());
        assert_eq!(quantile(&ledger, "compile", 16), 244.0); // rank 245
        assert_eq!(conformal_rank(50), 49);
        assert_eq!(conformal_rank(256), 245);
        assert_eq!(conformal_rank(1), 1); // Clamp to the available history.
    }
    #[test]
    fn pooled_spread_qualifies_ten_keys_of_each_kind_independently() {
        let mut ledger = MemoryLedger::default();
        for i in 0..9 {
            ledger
                .memory_windows
                .insert(format!("crate{i} [compile mode]"), vec![100, 200, 400, 800]);
        }
        ledger
            .memory_windows
            .insert("short".into(), vec![100, 200, 400]);
        prepare(&mut ledger);
        assert_eq!(sigma_pool(&ledger, "compile"), 0.5);
        ledger
            .memory_windows
            .insert("tenth [compile mode]".into(), vec![100, 200, 400, 800]);
        prepare(&mut ledger);
        assert!((sigma_pool(&ledger, "compile") - 0.8948491622597644).abs() < 1e-12);
        assert_eq!(sigma_pool(&ledger, "link"), 0.5);
    }
    #[test]
    fn histories_roll_estimates_fall_and_zero_scale_skips_residuals() {
        let mut ledger = MemoryLedger::default();
        record(&mut ledger, "key", "compile", 10000, None, true);
        let high = estimate(&ledger, "key", "compile").unwrap().charge;
        for _ in 0..16 {
            let features = estimate(&ledger, "key", "compile").unwrap().features;
            record(&mut ledger, "key", "compile", 100, Some(features), true);
        }
        assert_eq!(ledger.memory_windows["key"], vec![100; 16]);
        assert!(estimate(&ledger, "key", "compile").unwrap().charge < high);
        for _ in 0..300 {
            record(
                &mut ledger,
                "key",
                "compile",
                100,
                Some(Features {
                    mean: 100.0_f64.ln(),
                    scale: 1.0,
                }),
                true,
            );
        }
        assert_eq!(ledger.memory_residuals["compile"].len(), MAX_RESIDUALS);
        assert!(ledger.memory_residuals["compile"].iter().all(|r| *r == 0.0));
        record(
            &mut ledger,
            "key",
            "compile",
            100,
            Some(Features {
                mean: 0.0,
                scale: 0.0,
            }),
            true,
        );
        assert!(ledger.memory_residuals["compile"].iter().all(|r| *r == 0.0));
    }
    #[test]
    fn a_calibrated_wide_spread_does_not_uncap_a_key_with_few_samples() {
        // Ten link keys with four samples each and a wide spread, as in review.
        let mut ledger = MemoryLedger::default();
        for i in 0..MIN_POOL_KEYS {
            ledger
                .memory_windows
                .insert(format!("calibrating{i} [link]"), vec![100, 145, 213, 320]);
        }
        let key = "heavy [link]";
        ledger.memory_windows.insert(key.into(), vec![21_000]);
        ledger.crates.insert(key.into(), 21_000);
        prepare(&mut ledger);
        assert!(calibrated(&ledger, "link"));
        assert!(sigma_pool(&ledger, "link") > 0.45);
        let raw = estimate(&ledger, key, "link").unwrap().charge;
        assert!(
            raw > 90_000,
            "the wide spread makes the raw bound large: {raw}"
        );
        // One sample: still held to the previous rule's charge.
        assert_eq!(
            link_prediction(&ledger, key, None, 1000),
            (Some(21_000), "window")
        );
        // With four samples of its own, the key's estimate applies uncapped.
        ledger
            .memory_windows
            .insert(key.into(), vec![21_000, 21_000, 21_000, 21_000]);
        prepare(&mut ledger);
        let own = estimate(&ledger, key, "link").unwrap().charge;
        assert_eq!(
            link_prediction(&ledger, key, None, 1000),
            (Some(own), "window")
        );
    }

    #[test]
    fn warm_up_caps_a_key_without_history_at_the_default_charge() {
        // A light crate never gets a legacy entry, but it does get a window.
        // The previous rule charged it the default; warm-up must not exceed that.
        let mut ledger = MemoryLedger::default();
        let key = "light [compile mode]";
        record(&mut ledger, key, "compile", 100, None, true);
        let raw = estimate(&ledger, key, "compile").unwrap().charge;
        assert!(
            raw > 200,
            "the default spread makes the raw bound wide: {raw}"
        );
        assert_eq!(
            compile_prediction(&ledger, "light", key, 200),
            (Some(200), "window")
        );
        // Never below the window's largest peak.
        assert_eq!(
            compile_prediction(&ledger, "light", key, 50),
            (Some(100), "window")
        );
        // No default (CPU-only scheduling): nothing to cap at.
        assert_eq!(
            compile_prediction(&ledger, "light", key, 0),
            (Some(raw), "window")
        );
    }

    #[test]
    fn new_maps_are_bounded_and_cold_hierarchy_uses_sibling_charges() {
        let mut ledger = MemoryLedger::default();
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile new]", 0),
            (None, "default")
        );
        ledger.crates.insert("crate".into(), 99999);
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile new]", 0),
            (Some(99999), "legacy")
        );
        record(
            &mut ledger,
            "crate [compile old]",
            "compile",
            100,
            None,
            true,
        );
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile new]", 0),
            (Some(452), "sibling")
        );
        record(
            &mut ledger,
            "crate [compile other]",
            "compile",
            200,
            None,
            true,
        );
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile new]", 0),
            (Some(904), "sibling")
        );
        for i in 0..MAX_LEDGER_ENTRIES {
            record(
                &mut ledger,
                &format!("other{i}"),
                "compile",
                1000,
                None,
                true,
            );
        }
        assert_eq!(ledger.memory_windows.len(), MAX_LEDGER_ENTRIES);
        assert!(!ledger.memory_windows.contains_key("crate [compile old]"));
        let mut oversized = MemoryLedger::default();
        oversized.memory_windows.insert("key".into(), vec![0; 100]);
        oversized
            .memory_residuals
            .insert("unknown".into(), vec![0.0; 300]);
        prepare(&mut oversized);
        assert!(oversized.memory_windows.is_empty());
        assert!(oversized.memory_residuals.is_empty());
    }
}
