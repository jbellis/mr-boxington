//! Windowed log-space prediction. Legacy ledger values are written separately.
use super::{MAX_LEDGER_ENTRIES, MemoryLedger};
use std::collections::BTreeMap;

pub(super) const WINDOW: usize = 16;
const MAX_RESIDUALS: usize = 256;
const MIN_RESIDUALS: usize = 50;
const MIN_POOL_KEYS: usize = 10;
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
    // A planned cold key has no residual. Preserve that fact even if another
    // process records its first sample while this request waits in the queue.
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
    let mut residuals: Vec<_> = ledger
        .memory_residuals
        .get(kind)
        .into_iter()
        .flatten()
        .copied()
        .filter(|r| r.is_finite())
        .collect();
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
    let features = Features {
        mean,
        scale: s * (1.0 + 1.0 / n as f64).sqrt(),
    };
    let q = quantile(ledger, kind, n);
    // Float-to-integer casts saturate at u64::MAX for an overflowing bound.
    let bound = (mean + q * features.scale).exp().ceil() as u64;
    Some(Estimate {
        n,
        window_max,
        bound,
        charge: window_max.max(bound),
        q,
        features,
    })
}

pub(super) fn compile_prediction(
    ledger: &MemoryLedger,
    name: &str,
    key: &str,
) -> (Option<u64>, &'static str) {
    if let Some(estimate) = estimate(ledger, key, "compile") {
        return (Some(estimate.charge), "window");
    }
    if let Some(peak) = ledger.crates.get(key).copied() {
        // An old writer can discard windows. Preserve its charge, including
        // mode headroom or the crate-wide fallback before three successes.
        let bare = ledger.crates.get(name).copied();
        let old_charge = if ledger.compile_samples.get(key).copied().unwrap_or(0) >= 3 {
            let padded = peak.saturating_add(peak.div_ceil(2));
            bare.map_or(padded, |bare| padded.min(bare))
        } else {
            bare.unwrap_or(peak)
        };
        return (Some(peak.max(old_charge)), "legacy");
    }
    let prefix = format!("{name} [compile ");
    if let Some(charge) = ledger
        .memory_windows
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .filter_map(|key| estimate(ledger, key, "compile").map(|e| e.charge))
        .max()
    {
        return (Some(charge), "sibling");
    }
    match ledger.crates.get(name).copied() {
        Some(peak) => (Some(peak), "legacy"),
        None => (None, "default"),
    }
}

/// Shared by admission and stats, including old-writer and eviction fallbacks.
pub(super) fn link_prediction(
    ledger: &MemoryLedger,
    key: &str,
    profile: Option<&str>,
    floor: u64,
) -> (Option<u64>, &'static str) {
    if let Some(estimate) = estimate(ledger, key, "link") {
        return (Some(estimate.charge), "window");
    }
    if let Some(peak) = ledger.crates.get(key).copied() {
        return (Some(peak), "legacy");
    }
    let window = match profile {
        Some(profile) => ledger
            .profile_link_peaks
            .get(profile)
            .map_or(&[][..], Vec::as_slice),
        None => ledger.link_peaks.as_slice(),
    };
    match window.iter().copied().max() {
        Some(peak) => (Some(peak), "link-window"),
        None => (Some(floor), "link-floor"),
    }
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
        residuals.retain(|r| r.is_finite());
        residuals.drain(..residuals.len().saturating_sub(MAX_RESIDUALS));
    }
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
            let sigma = if values.len() < MIN_POOL_KEYS {
                DEFAULT_SIGMA_POOL
            } else {
                values.sort_by(f64::total_cmp);
                let mid = values.len() / 2;
                if values.len().is_multiple_of(2) {
                    (values[mid - 1] + values[mid]) / 2.0
                } else {
                    values[mid]
                }
            };
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
            history.drain(..history.len().saturating_sub(MAX_RESIDUALS));
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
        assert!((e.features.scale - expected).abs() < 1e-12);
    }
    #[test]
    fn t_fallback_nearest_rank_calibration_and_window_floor() {
        let mut ledger = MemoryLedger::default();
        ledger.memory_windows.insert("key".into(), vec![100]);
        assert_eq!(quantile(&ledger, "compile", 1), T_QUANTILES[3]);
        assert_eq!(quantile(&ledger, "compile", 40), NORMAL_QUANTILE);
        ledger
            .memory_residuals
            .insert("compile".into(), vec![-10.0; 49]);
        assert_eq!(quantile(&ledger, "compile", 1), T_QUANTILES[3]);
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
        assert_eq!(quantile(&ledger, "link", 1), T_QUANTILES[3]);
    }
    #[test]
    fn finite_sample_rank_matches_review_scenario_and_clamps_to_history() {
        let mut ledger = MemoryLedger::default();
        ledger.memory_windows.insert("key".into(), vec![100; 16]);
        ledger
            .memory_residuals
            .insert("compile".into(), (0..50).map(|i| i as f64 / 10.0).collect());
        assert_eq!(quantile(&ledger, "compile", 16), 4.8);
        assert_eq!(estimate(&ledger, "key", "compile").unwrap().charge, 312);
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
        assert_eq!(ledger.memory_residuals["compile"].len(), 256);
        assert_eq!(ledger.memory_residuals["compile"], vec![0.0; 256]);
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
        assert_eq!(ledger.memory_residuals["compile"], vec![0.0; 256]);
    }
    #[test]
    fn new_maps_are_bounded_and_cold_hierarchy_uses_sibling_charges() {
        let mut ledger = MemoryLedger::default();
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile new]"),
            (None, "default")
        );
        ledger.crates.insert("crate".into(), 99999);
        assert_eq!(
            compile_prediction(&ledger, "crate", "crate [compile new]"),
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
            compile_prediction(&ledger, "crate", "crate [compile new]"),
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
            compile_prediction(&ledger, "crate", "crate [compile new]"),
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
