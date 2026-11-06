//! Big-data packets on `DATA_NOTIFY`: `bc <type> <len:2 LE> <crc:2 LE> <payload>`, split across
//! notifications when longer than one. The CRC is CRC-16/MODBUS of the payload.

use chrono::{Duration, NaiveDate, NaiveDateTime};

pub const TEMPERATURE: u8 = 0x25;
pub const SLEEP: u8 = 0x27;
pub const SPO2: u8 = 0x2a;

const HEADER: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub kind: u8,
    pub payload: Vec<u8>,
    pub crc_ok: bool,
}

/// Joins notifications into whole packets.
#[derive(Debug, Default)]
pub struct Assembler {
    buf: Vec<u8>,
}

impl Assembler {
    pub fn feed(&mut self, frame: &[u8]) -> Option<Packet> {
        if self.buf.is_empty() && frame.first() != Some(&crate::cmd::BIG_DATA) {
            return None;
        }
        self.buf.extend_from_slice(frame);
        if self.buf.len() < HEADER {
            return None;
        }
        let len = u16::from_le_bytes([self.buf[2], self.buf[3]]) as usize;
        if self.buf.len() < HEADER + len {
            return None;
        }
        let buf = std::mem::take(&mut self.buf);
        let payload = buf[HEADER..HEADER + len].to_vec();
        let crc = u16::from_le_bytes([buf[4], buf[5]]);
        Some(Packet { kind: buf[1], crc_ok: crc16_modbus(&payload) == crc, payload })
    }
}

pub fn crc16_modbus(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xa001 } else { crc >> 1 };
        }
    }
    crc
}

/// Days of 24 hourly entries, oldest first, ending with today (`days_ago` 0).
fn days(payload: &[u8], prefix: usize, mut entry: impl FnMut(u8, usize, u8, u8)) {
    let mut i = 0;
    while i < payload.len() {
        let days_ago = payload[i];
        i += prefix;
        for hour in 0..24 {
            let (Some(&a), Some(&b)) = (payload.get(i), payload.get(i + 1)) else { return };
            entry(days_ago, hour, a, b);
            i += 2;
        }
        if days_ago == 0 {
            return;
        }
    }
}

/// Hourly SpO2: (days ago, hour, lowest, highest), hours without a reading left out.
pub fn spo2(payload: &[u8]) -> Vec<(u8, u8, u8, u8)> {
    let mut out = Vec::new();
    days(payload, 1, |d, h, lo, hi| {
        if lo > 0 && hi > 0 {
            out.push((d, h as u8, lo, hi));
        }
    });
    out
}

/// Half-hourly skin temperature: (days ago, minute of the day, degC). Each day starts with its
/// day number and one more byte, so far always `1e`.
pub fn temperature(payload: &[u8]) -> Vec<(u8, u16, f32)> {
    let mut out = Vec::new();
    // Shorter answers carry no readings.
    if payload.len() < 50 {
        return out;
    }
    days(payload, 2, |d, h, at_00, at_30| {
        for (minute, v) in [(h * 60, at_00), (h * 60 + 30, at_30)] {
            if v > 0 {
                out.push((d, minute as u16, v as f32 / 10.0 + 20.0));
            }
        }
    });
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Light,
    Deep,
    Rem,
    Awake,
    Other(u8),
}

impl Stage {
    fn from(code: u8) -> Self {
        match code {
            2 => Stage::Light,
            3 => Stage::Deep,
            4 => Stage::Rem,
            5 => Stage::Awake,
            other => Stage::Other(other),
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Stage::Light => 2,
            Stage::Deep => 3,
            Stage::Rem => 4,
            Stage::Awake => 5,
            Stage::Other(c) => c,
        }
    }
}

/// A night scored by the ring. Start and end are minutes of the day it ended on; a start later
/// than the end belongs to the evening before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub days_ago: u8,
    pub start: u16,
    pub end: u16,
    pub stages: Vec<(Stage, u8)>,
}

impl Session {
    /// Start and end in the ring's local time, given its "today".
    pub fn span(&self, today: NaiveDate) -> Option<(NaiveDateTime, NaiveDateTime)> {
        let day = today.checked_sub_days(chrono::Days::new(self.days_ago as u64))?.and_hms_opt(0, 0, 0)?;
        let end = day + Duration::minutes(self.end as i64);
        let start = if self.start > self.end {
            day + Duration::minutes(self.start as i64 - 1440)
        } else {
            day + Duration::minutes(self.start as i64)
        };
        Some((start, end))
    }
}

pub fn sleep(payload: &[u8]) -> Vec<Session> {
    let mut out = Vec::new();
    let Some(&count) = payload.first() else { return out };
    let mut i = 1;
    for _ in 0..count {
        let (Some(&days_ago), Some(&len)) = (payload.get(i), payload.get(i + 1)) else { break };
        let (Some(s), Some(e)) = (payload.get(i + 2..i + 4), payload.get(i + 4..i + 6)) else { break };
        let mut session =
            Session { days_ago, start: u16::from_le_bytes([s[0], s[1]]), end: u16::from_le_bytes([e[0], e[1]]), stages: Vec::new() };
        // `len` counts the start and end, then two bytes per stage.
        let mut j = i + 6;
        for _ in (4..len as usize).step_by(2) {
            let (Some(&code), Some(&minutes)) = (payload.get(j), payload.get(j + 1)) else { break };
            if minutes > 0 {
                session.stages.push((Stage::from(code), minutes));
            }
            j += 2;
        }
        i += 2 + len as usize;
        out.push(session);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![crate::cmd::BIG_DATA, kind];
        f.extend((payload.len() as u16).to_le_bytes());
        f.extend(crc16_modbus(payload).to_le_bytes());
        f.extend(payload);
        f
    }

    #[test]
    fn crc_reference_value() {
        assert_eq!(crc16_modbus(b"123456789"), 0x4b37);
    }

    #[test]
    fn assembles_split_packets() {
        let payload: Vec<u8> = (0..40).collect();
        let f = frame(SPO2, &payload);
        let mut a = Assembler::default();
        assert_eq!(a.feed(&[0x01, 0x02]), None, "not a big-data start");
        assert_eq!(a.feed(&f[..20]), None);
        let p = a.feed(&f[20..]).unwrap();
        assert_eq!((p.kind, p.payload, p.crc_ok), (SPO2, payload, true));
    }

    #[test]
    fn spo2_days() {
        let mut payload = vec![1u8];
        payload.extend([0u8; 48]);
        payload[1 + 2 * 3] = 94;
        payload[1 + 2 * 3 + 1] = 98;
        payload.push(0);
        payload.extend([95u8, 97]);
        assert_eq!(spo2(&payload), vec![(1, 3, 94, 98), (0, 0, 95, 97)]);
    }

    #[test]
    fn temperature_half_hours() {
        let mut payload = vec![0u8, 0x1e];
        payload.extend([0u8; 48]);
        payload[2 + 2 * 23 + 1] = 128;
        assert_eq!(temperature(&payload), vec![(0, 1410, 32.8)]);
    }

    #[test]
    fn sleep_sessions() {
        // One night: 22:30 (1350) to 06:45 (405), light 30, deep 60, awake 5.
        let mut payload = vec![1u8, 0, 10];
        payload.extend(1350u16.to_le_bytes());
        payload.extend(405u16.to_le_bytes());
        payload.extend([2, 30, 3, 60, 5, 5]);
        let s = sleep(&payload);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].stages, vec![(Stage::Light, 30), (Stage::Deep, 60), (Stage::Awake, 5)]);
        let today = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();
        let (start, end) = s[0].span(today).unwrap();
        assert_eq!((start.to_string().as_str(), end.to_string().as_str()), ("2026-10-26 22:30:00", "2026-10-27 06:45:00"));
    }
}
