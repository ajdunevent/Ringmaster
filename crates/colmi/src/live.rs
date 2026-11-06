//! Live readings, battery and unprompted notifications.

use crate::cmd;
use crate::packet::{build, LEN};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    HeartRate = 1,
    Spo2 = 3,
}

pub fn start(kind: Kind) -> [u8; LEN] {
    build(cmd::LIVE_START, &[kind as u8, 0x01])
}

/// Sent while a reading runs to keep it going.
pub fn keep_going(kind: Kind) -> [u8; LEN] {
    build(cmd::LIVE_START, &[kind as u8, 0x03])
}

pub fn stop(kind: Kind) -> [u8; LEN] {
    build(cmd::LIVE_STOP, &[kind as u8, 0, 0])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    Value(Kind, u8),
    /// 1 means the ring is not worn properly; 2 a passing gap.
    Error(Kind, u8),
}

pub fn reading(p: &[u8]) -> Option<Reading> {
    if p.len() < 4 || p[0] != cmd::LIVE_START {
        return None;
    }
    let kind = match p[1] {
        1 => Kind::HeartRate,
        3 => Kind::Spo2,
        _ => return None,
    };
    Some(match (p[2], p[3]) {
        (0, 0) => return None,
        (0, v) => Reading::Value(kind, v),
        (e, _) => Reading::Error(kind, e),
    })
}

/// Battery percentage and whether it is charging, from a battery reply or notification.
pub fn battery(p: &[u8]) -> Option<(u8, bool)> {
    match p {
        [cmd::BATTERY, level, charging, ..] => Some((*level, *charging == 1)),
        [cmd::NOTIFICATION, 0x0c, level, charging, ..] => Some((*level, *charging == 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_from_tahnok_captures() {
        let ok = [0x69, 0x01, 0x00, 0x4e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xb8];
        assert_eq!(reading(&ok), Some(Reading::Value(Kind::HeartRate, 78)));
        let worn_badly = [0x69, 0x01, 0x01, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x6b];
        assert_eq!(reading(&worn_badly), Some(Reading::Error(Kind::HeartRate, 1)));
        assert_eq!(start(Kind::Spo2)[..3], [0x69, 0x03, 0x01]);
        assert_eq!(stop(Kind::Spo2)[..4], [0x6a, 0x03, 0, 0]);
    }

    #[test]
    fn battery_levels() {
        assert_eq!(battery(&[0x03, 0x40, 0x00]), Some((64, false)));
        assert_eq!(battery(&[0x73, 0x0c, 80, 1]), Some((80, true)));
        assert_eq!(battery(&[0x73, 0x12, 80, 1]), None);
    }
}
