//! Sleep estimated from the ring's 2.5-minute records. The ring sends no sleep data, so this is an
//! estimate: a hand that stays still while the ring reads a pulse with high signal confidence.
//! Stillness alone would count a quiet evening on the sofa as sleep, and a ring left on a
//! nightstand reads no pulse.

/// What sleep detection needs from one worn epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    pub unix: i64,
    /// Every 30-second motion count at the sensor's floor.
    pub still: bool,
    /// A heart rate was read, so the ring was on a finger.
    pub pulse: bool,
    pub confidence: u8,
}

pub(crate) const EPOCH: i64 = 150;
// Tuned against 158 nights scored by the official app (2025-12 to 2026-06, Gen 2): every night
// found, median edges 10 minutes before its sleep start and 8 after its sleep end, 91 % of the
// estimate inside its sleep. A threshold of 9 matched that data more closely but split nights
// recorded in 2026-10, when overnight confidence dips to 5 to 8 more often.
/// Confidence runs 0 to about 12 and sits at 10 through most of a night.
const CONFIDENT: u8 = 8;
/// Each epoch is judged by the share of sleep-like epochs within this distance either side.
const WINDOW: i64 = 15 * 60;
const SHARE: f64 = 0.75;
/// Epochs further apart than this break a run, so a gap in the data is not counted as sleep.
const CONTINUOUS: i64 = 10 * 60;
/// Wakings shorter than this stay inside one sleep.
const MERGE: i64 = 45 * 60;
const SHORTEST: i64 = 45 * 60;

/// Movement total in an epoch above which it counts as awake (PROTOCOL.md section 11).
const RESTLESS_MOVEMENT: u32 = 2000;

/// An epoch inside a night when the wearer was probably awake: the ring's restlessness digit
/// (units of byte 9) is set, or the five movement magnitudes add up past `RESTLESS_MOVEMENT`.
/// Against the official app's scoring this rule found awake epochs with F1 0.70.
pub fn restless(raw: &[u8]) -> bool {
    let (Some(&class), Some(packed)) = (raw.get(9), raw.get(15..23)) else { return false };
    let v = u64::from_be_bytes(packed.try_into().unwrap_or([0; 8]));
    let movement: u32 = (0..5).map(|i| ((v >> (52 - 12 * i)) & 0xfff) as u32).sum();
    class % 10 > 0 || movement > RESTLESS_MOVEMENT
}

/// Sleep periods as `[start, end)` in Unix seconds, oldest first. `epochs` must be in time order.
pub fn sessions(epochs: &[Activity]) -> Vec<(i64, i64)> {
    let sleepy: Vec<bool> = epochs.iter().map(|e| e.still && e.pulse && e.confidence >= CONFIDENT).collect();
    let (mut lo, mut hi) = (0, 0);
    // Runs of epochs judged asleep, as index ranges.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, e) in epochs.iter().enumerate() {
        while epochs[lo].unix < e.unix - WINDOW {
            lo += 1;
        }
        while hi < epochs.len() && epochs[hi].unix <= e.unix + WINDOW {
            hi += 1;
        }
        let share = sleepy[lo..hi].iter().filter(|s| **s).count() as f64 / (hi - lo) as f64;
        if share < SHARE {
            continue;
        }
        match runs.last_mut() {
            Some(run) if e.unix - epochs[run.1].unix <= CONTINUOUS => run.1 = i,
            _ => runs.push((i, i)),
        }
    }
    // The window blurs the edges; each run extends over the sleep-like epochs next to it.
    let adjacent = |a: usize, b: usize| sleepy[a] && sleepy[b] && (epochs[a].unix - epochs[b].unix).abs() <= CONTINUOUS;
    for run in &mut runs {
        while run.0 > 0 && adjacent(run.0 - 1, run.0) {
            run.0 -= 1;
        }
        while run.1 + 1 < epochs.len() && adjacent(run.1 + 1, run.1) {
            run.1 += 1;
        }
    }
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (first, last) in runs {
        let (start, end) = (epochs[first].unix, epochs[last].unix + EPOCH);
        match merged.last_mut() {
            Some(prev) if start - prev.1 <= MERGE => prev.1 = prev.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged.retain(|(start, end)| end - start >= SHORTEST);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stretch(from: i64, minutes: i64, still: bool, confidence: u8) -> Vec<Activity> {
        (0..minutes * 60 / EPOCH).map(|i| Activity { unix: from + i * EPOCH, still, pulse: true, confidence }).collect()
    }

    #[test]
    fn a_night_between_two_days() {
        let mut epochs = stretch(0, 120, false, 3);
        epochs.extend(stretch(7200, 420, true, 10));
        epochs.extend(stretch(7200 + 420 * 60, 60, false, 3));
        assert_eq!(sessions(&epochs), vec![(7200, 7200 + 420 * 60)]);
    }

    #[test]
    fn a_still_evening_is_not_sleep() {
        assert!(sessions(&stretch(0, 180, true, 3)).is_empty());
    }

    #[test]
    fn a_short_waking_stays_inside_one_night() {
        let mut epochs = stretch(0, 180, true, 10);
        epochs.extend(stretch(180 * 60, 15, false, 3));
        epochs.extend(stretch(195 * 60, 180, true, 10));
        assert_eq!(sessions(&epochs).len(), 1);
    }

    #[test]
    fn a_ring_off_the_finger_is_not_asleep() {
        let epochs: Vec<Activity> = stretch(0, 300, true, 10).into_iter().map(|e| Activity { pulse: false, ..e }).collect();
        assert!(sessions(&epochs).is_empty());
    }

    #[test]
    fn restless_epochs() {
        let mut raw = [0u8; 23];
        raw[9] = 0x0a;
        assert!(!restless(&raw));
        raw[9] = 0x0b;
        assert!(restless(&raw), "restlessness digit");
        raw[9] = 0x0a;
        // 2188, 2079, 422, 87, 90 packed as in the ring's record.
        raw[15..23].copy_from_slice(&[0x88, 0xc8, 0x1f, 0x1a, 0x60, 0x57, 0x05, 0xa0]);
        assert!(restless(&raw), "movement");
    }

    #[test]
    fn missing_data_splits_a_night() {
        let mut epochs = stretch(0, 120, true, 10);
        epochs.extend(stretch(5 * 3600, 120, true, 10));
        assert_eq!(sessions(&epochs).len(), 2);
    }
}
