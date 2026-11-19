//! Sleep stages estimated from the ring's 2.5-minute epochs: heart rate, HRV, breathing rate and
//! movement, read the way sleep research describes each stage, then kept to the order stages
//! follow through a night.
//!
//! Sources (docs/SLEEP.md):
//! - Deep sleep has the night's lowest and steadiest heart rate and the most regular breathing;
//!   REM a faster, less steady heart rate, lower HRV and irregular breathing; both come with
//!   stillness (Penzel et al. 2016, Cabiddu et al. 2012).
//! - Deep sleep falls mostly in the first third of the night and REM grows towards morning; the
//!   first REM comes about 80 to 100 minutes after sleep onset (Carskadon and Dement).
//! - Healthy adults spend about 20 % of sleep in deep sleep and 19 % in REM, with no significant
//!   change between 18 and 80 years under current scoring rules (Boulos et al. 2019). The fixed
//!   settings below put a typical night near those shares; no night is held to them.
//!
//! Each night is judged against itself (signs relative to the surrounding 45 minutes) and against
//! the wearer's usual nights (the previous two weeks), so a night that never settles shows less
//! deep sleep instead of being rescaled into an ordinary one.

use crate::store::{NightEpoch, Store, RINGCONN};
use anyhow::Result;
use chrono::{Local, NaiveDate, TimeZone};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Awake,
    Light,
    Deep,
    Rem,
}

impl Stage {
    const ALL: [Stage; 4] = [Stage::Awake, Stage::Light, Stage::Deep, Stage::Rem];

    fn index(self) -> usize {
        self as usize
    }
}

/// Epochs either side for the local baseline: 45 minutes.
const LOCAL: usize = 18;
/// Epochs either side over which steadiness is measured: 12.5 minutes in all.
const SPREAD: usize = 2;
/// Weight of each sign, in spreads of the night.
const GAIN: f64 = 4.0;
/// Weight of the whole night against the wearer's usual night.
const USUAL: f64 = 0.25;
/// Chance of staying in a stage from one epoch to the next.
const STAY: f64 = 0.92;
/// Deep sleep's lead over light sleep at sleep onset, and how far it falls by the end.
const DEEP_EARLY: f64 = 0.5;
const DEEP_FALL: f64 = 5.0;
/// REM against light sleep at sleep onset, and how far it rises by the end.
const REM_BASE: f64 = -2.5;
const REM_RISE: f64 = 1.0;
/// Minutes after sleep onset before any REM.
const FIRST_REM: f64 = 70.0;
/// Values from earlier nights needed before they count as the wearer's usual.
const ENOUGH: usize = 200;
/// Signs are clipped to this many spreads.
const CLIP: f64 = 3.0;
const EPOCH_MINUTES: f64 = 2.5;

/// The wearer's usual sleeping heart rate, HRV and steadiness, from earlier nights: (median,
/// spread) for each.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usual {
    hr: Option<(f64, f64)>,
    hrv: Option<(f64, f64)>,
    hr_steadiness: Option<(f64, f64)>,
    rr_steadiness: Option<(f64, f64)>,
}

impl Usual {
    pub fn from_nights(nights: &[&[NightEpoch]]) -> Self {
        let collect = |f: &dyn Fn(&NightEpoch) -> Option<f64>| nights.iter().flat_map(|n| n.iter().filter_map(f)).collect::<Vec<_>>();
        let unsteady = |f: fn(&NightEpoch) -> Option<f64>| {
            nights.iter().flat_map(|n| instability(&n.iter().map(f).collect::<Vec<_>>())).flatten().collect::<Vec<_>>()
        };
        let summary = |v: Vec<f64>| (v.len() >= ENOUGH).then(|| Some((median(&v)?, spread(&v)?))).flatten();
        Self {
            hr: summary(collect(&|e| e.hr)),
            hrv: summary(collect(&|e| e.hrv)),
            hr_steadiness: summary(unsteady(|e| e.hr)),
            rr_steadiness: summary(unsteady(|e| e.rr)),
        }
    }

    /// Median sleeping heart rate in bpm and HRV in ms.
    pub fn heart_rate(&self) -> Option<f64> {
        self.hr.map(|h| h.0)
    }

    pub fn hrv(&self) -> Option<f64> {
        self.hrv.map(|h| h.0)
    }
}

fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    Some(if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 })
}

/// A robust spread: the interquartile range over 1.35, which equals the standard deviation for
/// normally distributed values.
fn spread(v: &[f64]) -> Option<f64> {
    if v.len() < 8 {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    Some(((s[3 * s.len() / 4] - s[s.len() / 4]) / 1.35).max(1e-6))
}

fn present(v: &[Option<f64>]) -> Vec<f64> {
    v.iter().flatten().copied().collect()
}

/// Mean absolute change between consecutive epochs within `SPREAD` either side.
fn instability(v: &[Option<f64>]) -> Vec<Option<f64>> {
    let n = v.len();
    (0..n)
        .map(|i| {
            let d: Vec<f64> =
                (i.saturating_sub(SPREAD).max(1)..n.min(i + SPREAD + 1)).filter_map(|j| Some((v[j]? - v[j - 1]?).abs())).collect();
            (!d.is_empty()).then(|| d.iter().sum::<f64>() / d.len() as f64)
        })
        .collect()
}

#[derive(Debug, Default, Clone, Copy)]
struct Signs {
    hr: Option<f64>,
    hrv: Option<f64>,
    /// Heart rate and breathing unsteadiness within the night.
    hr_unsteady: Option<f64>,
    rr_unsteady: Option<f64>,
    /// The same against the wearer's usual nights.
    hr_unsteady_usual: Option<f64>,
    rr_unsteady_usual: Option<f64>,
    /// The whole night's heart rate and HRV against the wearer's usual.
    night_hr: Option<f64>,
    night_hrv: Option<f64>,
}

fn signs(epochs: &[NightEpoch], usual: &Usual) -> Vec<Signs> {
    let n = epochs.len();
    let hr: Vec<Option<f64>> = epochs.iter().map(|e| e.hr).collect();
    let hrv: Vec<Option<f64>> = epochs.iter().map(|e| e.hrv).collect();
    let rr: Vec<Option<f64>> = epochs.iter().map(|e| e.rr).collect();
    let (hr_spread, hrv_spread) = (spread(&present(&hr)), spread(&present(&hrv)));
    let local = |v: &[Option<f64>], spread: Option<f64>, i: usize| -> Option<f64> {
        let x = v[i]?;
        let base = median(&present(&v[i.saturating_sub(LOCAL)..n.min(i + LOCAL + 1)]))?;
        Some((x - base) / spread?)
    };
    let scaled = |x: Option<f64>, s: Option<(f64, f64)>| Some((x? - s?.0) / s?.1);
    let unsteady = [instability(&hr), instability(&rr)];
    let within: Vec<Option<(f64, f64)>> = unsteady.iter().map(|u| Some((median(&present(u))?, spread(&present(u))?))).collect();
    let against = [usual.hr_steadiness.or(within[0]), usual.rr_steadiness.or(within[1])];
    let night_hr = scaled(median(&present(&hr)), usual.hr);
    let night_hrv = scaled(median(&present(&hrv)), usual.hrv);
    (0..n)
        .map(|i| Signs {
            hr: local(&hr, hr_spread, i),
            hrv: local(&hrv, hrv_spread, i),
            hr_unsteady: scaled(unsteady[0][i], within[0]),
            rr_unsteady: scaled(unsteady[1][i], within[1]),
            hr_unsteady_usual: scaled(unsteady[0][i], against[0]),
            rr_unsteady_usual: scaled(unsteady[1][i], against[1]),
            night_hr,
            night_hrv,
        })
        .collect()
}

fn clip(v: Option<f64>) -> f64 {
    v.map_or(0.0, |v| v.clamp(-CLIP, CLIP))
}

/// Log-scores per epoch for awake, light, deep and REM. Light sleep is the reference at 0.
fn scores(epochs: &[NightEpoch], signs: &[Signs]) -> Vec<[f64; 4]> {
    let n = epochs.len() as f64;
    epochs
        .iter()
        .zip(signs)
        .enumerate()
        .map(|(i, (e, s))| {
            let part = (i as f64 + 0.5) / n;
            let minutes = i as f64 * EPOCH_MINUTES;
            let (hr, hrv) = (clip(s.hr), clip(s.hrv));
            // Deep: heart rate low and steady against the wearer's usual steadiness, HRV high,
            // breathing steady; the whole night against the wearer's usual night counts too.
            let mut deep = GAIN * (-hr + hrv - 0.5 * clip(s.hr_unsteady_usual) - clip(s.rr_unsteady_usual)) / 2.0;
            deep += GAIN * USUAL * (-clip(s.night_hr) + clip(s.night_hrv)) / 2.0;
            // REM: heart rate higher and less steady within the night, HRV lower, breathing
            // irregular.
            let mut rem = GAIN * (hr - hrv + clip(s.hr_unsteady) + clip(s.rr_unsteady)) / 2.0;
            deep += DEEP_EARLY - DEEP_FALL * part;
            rem += REM_BASE + REM_RISE * part - if minutes < FIRST_REM { 9.0 } else { 0.0 };
            if !e.still {
                // Deep sleep and REM both come with stillness.
                deep -= 2.0;
                rem -= 2.0;
            }
            // Movement past the restless mark is waking, as in actigraphy.
            let wake = if e.restless {
                20.0
            } else if !e.still {
                0.0
            } else {
                -3.0
            };
            [wake, 0.0, deep, rem]
        })
        .collect()
}

/// Log-probabilities of moving between stages from one epoch to the next. Deep sleep and REM do
/// not follow each other directly, and waking leads to light sleep.
fn transitions() -> [[f64; 4]; 4] {
    use Stage::*;
    let allowed: [&[Stage]; 4] = [&[Awake, Light], &[Awake, Light, Deep, Rem], &[Awake, Light, Deep], &[Awake, Light, Rem]];
    let mut t = [[f64::NEG_INFINITY; 4]; 4];
    for (from, nexts) in allowed.iter().enumerate() {
        let others = (nexts.len() - 1) as f64;
        for to in nexts.iter() {
            t[from][to.index()] = if to.index() == from { STAY } else { (1.0 - STAY) / others }.ln();
        }
    }
    t
}

/// The most likely sequence of stages; sleep begins in light sleep.
fn most_likely(scores: &[[f64; 4]]) -> Vec<Stage> {
    let t = transitions();
    let mut prev = [f64::NEG_INFINITY; 4];
    prev[Stage::Light.index()] = scores[0][Stage::Light.index()];
    let mut back: Vec<[usize; 4]> = Vec::with_capacity(scores.len());
    for s in &scores[1..] {
        let mut cur = [f64::NEG_INFINITY; 4];
        let mut bp = [0usize; 4];
        for to in 0..4 {
            let mut best = 0;
            for from in 1..4 {
                if prev[from] + t[from][to] > prev[best] + t[best][to] {
                    best = from;
                }
            }
            cur[to] = prev[best] + t[best][to] + s[to];
            bp[to] = best;
        }
        prev = cur;
        back.push(bp);
    }
    let mut state = (1..4).fold(0, |best, s| if prev[s] > prev[best] { s } else { best });
    let mut path = vec![state];
    for bp in back.iter().rev() {
        state = bp[state];
        path.push(state);
    }
    path.reverse();
    path.into_iter().map(|i| Stage::ALL[i]).collect()
}

/// A stage for each epoch of one sleep, in order. Sleeps of fewer than 8 epochs are all light.
pub fn stage(epochs: &[NightEpoch], usual: &Usual) -> Vec<Stage> {
    if epochs.len() < 8 {
        return vec![Stage::Light; epochs.len()];
    }
    // Awake is exactly the restless epochs, as in the time awake shown with each sleep.
    most_likely(&scores(epochs, &signs(epochs, usual)))
        .into_iter()
        .zip(epochs)
        .map(|(s, e)| match (s, e.restless) {
            (_, true) => Stage::Awake,
            (Stage::Awake, false) => Stage::Light,
            (s, false) => s,
        })
        .collect()
}

/// Sleeps at least this long count toward the wearer's usual night.
const LONG_SLEEP: i64 = 4 * 3600;
/// Earlier long sleeps that make the wearer's usual.
const USUAL_NIGHTS: usize = 14;
/// Earlier nights needed before a night is compared with the usual one.
const COMPARE_AFTER: usize = 5;
/// Days read before the first night shown: enough for a usual night's own usual nights.
const LOOKBACK_DAYS: i64 = 35;
/// A night counts as unsettled when its deep sleep is below this share of the usual and its
/// heart rate or HRV is this far off the usual.
const UNSETTLED_DEEP: f64 = 0.5;
const UNSETTLED_HR: f64 = 3.0;
const UNSETTLED_HRV: f64 = 5.0;

/// Minutes in each stage: awake, light, deep, REM.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Minutes(pub [f64; 4]);

impl Minutes {
    pub fn of(&self, s: Stage) -> f64 {
        self.0[s.index()]
    }

    fn add(&mut self, s: Stage, minutes: f64) {
        self.0[s.index()] += minutes;
    }
}

/// One sleep and its stages, an epoch each.
#[derive(Debug, Clone)]
pub struct StagedSleep {
    pub start: i64,
    pub end: i64,
    pub stages: Vec<(i64, Stage)>,
}

/// The sleeps that end on one day, with their stages and how they compare with the wearer's
/// usual nights.
#[derive(Debug, Clone, Default)]
pub struct Night {
    pub sleeps: Vec<StagedSleep>,
    pub minutes: Minutes,
    /// The median of each stage over the previous nights, once there are enough.
    pub usual: Option<Minutes>,
    /// Heart rate above and HRV below the wearer's usual, when the night was unsettled: deep
    /// sleep well below the usual with heart rate or HRV off the usual.
    pub unsettled: Option<(Option<f64>, Option<f64>)>,
}

fn local_day(unix: i64) -> NaiveDate {
    Local.timestamp_opt(unix, 0).single().map(|t| t.date_naive()).unwrap_or_default()
}

/// Staged nights ending on the local days that overlap `[from, to)`, keyed by that day. Only
/// RingConn epochs carry what the stager reads; other rings get none.
pub fn nights(store: &Store, ring: &str, from: i64, to: i64) -> Result<BTreeMap<NaiveDate, Night>> {
    if store.brand(ring)? != RINGCONN {
        return Ok(BTreeMap::new());
    }
    let spans = store.sleep(ring, from - LOOKBACK_DAYS * 86_400, to)?;
    let (Some(first), Some(last)) = (spans.first(), spans.last()) else { return Ok(BTreeMap::new()) };
    let epochs = store.night_epochs(ring, first.0, last.1)?;
    let mut long: Vec<&[NightEpoch]> = Vec::new();
    let mut all: BTreeMap<NaiveDate, Night> = BTreeMap::new();
    let mut usual_of_day: BTreeMap<NaiveDate, Usual> = BTreeMap::new();
    for &(start, end) in &spans {
        let a = epochs.partition_point(|e| e.unix < start);
        let b = epochs.partition_point(|e| e.unix < end);
        let slice = &epochs[a..b];
        let usual = Usual::from_nights(&long[long.len().saturating_sub(USUAL_NIGHTS)..]);
        let stages = stage(slice, &usual);
        let day = local_day(end);
        let night = all.entry(day).or_default();
        for s in &stages {
            night.minutes.add(*s, EPOCH_MINUTES);
        }
        if end - start >= LONG_SLEEP {
            usual_of_day.insert(day, usual);
            night.unsettled = night_signs(slice, &usual);
            long.push(slice);
        }
        night.sleeps.push(StagedSleep { start, end, stages: slice.iter().map(|e| e.unix).zip(stages).collect() });
    }
    // Each night against the median of the nights before it.
    let days: Vec<NaiveDate> = all.keys().copied().collect();
    for (i, day) in days.iter().enumerate() {
        let earlier: Vec<Minutes> = days[i.saturating_sub(USUAL_NIGHTS)..i].iter().map(|d| all[d].minutes).collect();
        let night = all.get_mut(day).expect("listed above");
        if earlier.len() >= COMPARE_AFTER {
            let mut usual = Minutes::default();
            for s in Stage::ALL {
                usual.0[s.index()] = median(&earlier.iter().map(|m| m.of(s)).collect::<Vec<_>>()).unwrap_or(0.0);
            }
            night.usual = Some(usual);
        }
        let deep_low = night.usual.is_some_and(|u| night.minutes.of(Stage::Deep) < UNSETTLED_DEEP * u.of(Stage::Deep));
        if !deep_low {
            night.unsettled = None;
        }
    }
    let (first_day, last_day) = (local_day(from), local_day(to - 1));
    Ok(all.into_iter().filter(|(d, _)| *d >= first_day && *d <= last_day).collect())
}

/// Heart rate above and HRV below the wearer's usual over a whole sleep, each when far enough
/// off to mention; `None` when neither is.
fn night_signs(epochs: &[NightEpoch], usual: &Usual) -> Option<(Option<f64>, Option<f64>)> {
    let hr = median(&epochs.iter().filter_map(|e| e.hr).collect::<Vec<_>>());
    let hrv = median(&epochs.iter().filter_map(|e| e.hrv).collect::<Vec<_>>());
    let above = Some(hr? - usual.heart_rate()?).filter(|d| *d >= UNSETTLED_HR);
    let below = Some(usual.hrv()? - hrv?).filter(|d| *d >= UNSETTLED_HRV);
    (above.is_some() || below.is_some()).then_some((above, below))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quiet night: heart rate settles from 62 to 54 bpm, with two calm, low stretches early on
    /// and two faster, unsteady stretches late.
    fn night(len: usize) -> Vec<NightEpoch> {
        (0..len)
            .map(|i| {
                let t = i as f64 / len as f64;
                let mut hr = 62.0 - 8.0 * t + if i % 2 == 0 { 0.5 } else { -0.5 };
                let mut hrv = 35.0 + if i % 3 == 0 { 2.0 } else { -1.0 };
                let mut rr = 15.0 + if i % 2 == 0 { 0.1 } else { -0.1 };
                let deep = (30..50).contains(&i) || (70..85).contains(&i);
                let rem = (130..145).contains(&i) || (165..180).contains(&i);
                if deep {
                    hr -= 6.0;
                    hrv += 15.0;
                }
                if rem {
                    hr += 5.0 + if i % 2 == 0 { 4.0 } else { -4.0 };
                    hrv -= 12.0;
                    rr += if i % 2 == 0 { 1.5 } else { -1.5 };
                }
                NightEpoch { unix: i as i64 * 150, hr: Some(hr), hrv: Some(hrv), rr: Some(rr), still: true, restless: false }
            })
            .collect()
    }

    fn share(path: &[Stage], s: Stage) -> f64 {
        path.iter().filter(|p| **p == s).count() as f64 / path.len() as f64
    }

    #[test]
    fn calm_stretches_are_deep_and_unsteady_late_ones_rem() {
        let n = night(200);
        let path = stage(&n, &Usual::default());
        assert_eq!(path.len(), 200);
        assert!(path[35..45].iter().all(|s| *s == Stage::Deep), "{:?}", &path[30..50]);
        assert!(path[170..176].iter().all(|s| *s == Stage::Rem), "{:?}", &path[165..180]);
        assert!(share(&path, Stage::Deep) > 0.05 && share(&path, Stage::Rem) > 0.05);
    }

    #[test]
    fn order_rules_hold() {
        let path = stage(&night(200), &Usual::default());
        assert_eq!(path[0], Stage::Light);
        // No REM in the first 70 minutes.
        assert!(path[..28].iter().all(|s| *s != Stage::Rem));
        for w in path.windows(2) {
            let pair = (w[0], w[1]);
            assert!(pair != (Stage::Deep, Stage::Rem) && pair != (Stage::Rem, Stage::Deep), "{pair:?}");
            assert!(!(w[0] == Stage::Awake && matches!(w[1], Stage::Deep | Stage::Rem)), "{pair:?}");
        }
    }

    #[test]
    fn restless_epochs_are_awake() {
        let mut n = night(200);
        n[100].restless = true;
        n[100].still = false;
        assert_eq!(stage(&n, &Usual::default())[100], Stage::Awake);
    }

    #[test]
    fn a_night_that_never_settles_has_less_deep_sleep() {
        let usual_nights: Vec<Vec<NightEpoch>> = (0..3).map(|_| night(200)).collect();
        let refs: Vec<&[NightEpoch]> = usual_nights.iter().map(Vec::as_slice).collect();
        let usual = Usual::from_nights(&refs);
        assert!(usual.heart_rate().is_some());
        let ordinary = share(&stage(&night(200), &usual), Stage::Deep);
        let mut hot = night(200);
        for e in &mut hot {
            e.hr = e.hr.map(|h| h + 6.0);
            e.hrv = e.hrv.map(|h| h * 0.7);
        }
        let restless = share(&stage(&hot, &usual), Stage::Deep);
        assert!(restless < ordinary / 2.0, "{restless} against {ordinary}");
    }

    #[test]
    fn short_sleeps_are_light() {
        assert_eq!(stage(&night(5), &Usual::default()), vec![Stage::Light; 5]);
    }

    #[test]
    fn robust_statistics() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(spread(&[1.0; 4]), None);
        let v: Vec<f64> = (0..8).map(f64::from).collect();
        assert!((spread(&v).unwrap() - 4.0 / 1.35).abs() < 1e-9);
        assert_eq!(instability(&[Some(1.0), Some(3.0), None, Some(4.0)])[0], Some(2.0));
    }
}
