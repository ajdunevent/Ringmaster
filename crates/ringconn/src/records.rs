//! Decoding of the status descriptor (section 9.1) and history pages (sections 7.4 and 10).

use crate::frame::{op, u16_be, u32_be};

/// `0x10` / `0x87` status frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Descriptor {
    pub battery: u8,
    pub state: u8,
    /// Steps in the current quarter-hour bucket.
    pub steps: u16,
    pub skin_temp: [f32; 2],
    pub millivolts: u16,
    pub case: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingState {
    Settling,
    Idle,
    Charging,
    Sport,
    Unknown(u8),
}

impl Descriptor {
    pub fn parse(frame: &[u8]) -> Option<Self> {
        if !matches!(frame.first(), Some(&op::DESCRIPTOR_QUERY | &op::DESCRIPTOR_FETCH)) || frame.len() < 18 {
            return None;
        }
        Some(Self {
            battery: frame[1],
            state: frame[2],
            steps: u16_be(frame, 4),
            skin_temp: [u16_be(frame, 6) as f32 / 10.0, u16_be(frame, 8) as f32 / 10.0],
            millivolts: u16_be(frame, 14),
            case: frame[17],
        })
    }

    pub fn ring_state(&self) -> RingState {
        match self.state {
            0x01 => RingState::Settling,
            0x02 | 0x03 => RingState::Idle,
            0x04 => RingState::Charging,
            0x06 => RingState::Sport,
            other => RingState::Unknown(other),
        }
    }

    /// Charging case battery and whether the case is plugged in; `None` when the ring is not in it.
    pub fn case_battery(&self) -> Option<(u8, bool)> {
        (self.case != 0xff).then_some((self.case & 0x7f, self.case & 0x80 != 0))
    }
}

/// A history page: `[op] [remaining:2] [records...] [xor]`.
#[derive(Debug, Clone)]
pub struct Page<'a> {
    pub op: u8,
    pub remaining: u16,
    pub records: Vec<&'a [u8]>,
}

pub const EPOCH_LEN: usize = 23;
pub const OPTICAL_LEN: usize = 47;
pub const SPORT_LEN: usize = 11;

impl<'a> Page<'a> {
    pub fn parse(frame: &'a [u8]) -> Option<Self> {
        let len = match *frame.first()? {
            op::EPOCH_PAGE => EPOCH_LEN,
            op::OPTICAL_PAGE => OPTICAL_LEN,
            op::SPORT_PAGE => SPORT_LEN,
            _ => return None,
        };
        if frame.len() < 4 {
            return None;
        }
        Some(Self { op: frame[0], remaining: u16_be(frame, 1), records: frame[3..frame.len() - 1].chunks_exact(len).collect() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochLayout {
    Idle,
    Activity,
    SleepVitals,
}

/// One 2.5-minute `0x4c` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoch {
    pub counter: u32,
    pub raw: [u8; EPOCH_LEN],
}

impl Epoch {
    pub fn parse(record: &[u8]) -> Option<Self> {
        Some(Self { counter: u32_be(record, 0), raw: record.try_into().ok()? })
    }

    pub fn layout(&self) -> EpochLayout {
        let r = &self.raw;
        let idle = r[4..8] == [0x05, 0x00, 0x0c, 0x00]
            && r[9] == 0x0a
            && r[10..15].iter().all(|&b| b == 0x01)
            && r[15..22].iter().all(|&b| b == 0x00);
        if idle {
            EpochLayout::Idle
        } else if matches!(r[8], 0x11..=0x13) {
            EpochLayout::Activity
        } else {
            EpochLayout::SleepVitals
        }
    }

    /// Below 30 is the ring's "unmeasured" value.
    pub fn heart_rate(&self) -> Option<u8> {
        (self.layout() != EpochLayout::Idle && self.raw[4] >= 30).then_some(self.raw[4])
    }

    pub fn hrv(&self) -> Option<u8> {
        (self.heart_rate().is_some() && self.raw[5] > 0).then_some(self.raw[5])
    }

    pub fn confidence(&self) -> u8 {
        self.raw[6]
    }

    pub fn respiratory_rate(&self) -> Option<f32> {
        (self.heart_rate().is_some() && self.raw[7] > 0).then(|| self.raw[7] as f32 / 8.0)
    }

    /// Byte 8 is SpO2 only on sleep-vitals epochs; on activity epochs it is a layout tag.
    pub fn spo2(&self) -> Option<u8> {
        (self.layout() == EpochLayout::SleepVitals && (70..=100).contains(&self.raw[8])).then_some(self.raw[8])
    }

    /// Five 30-second motion counts.
    pub fn motion(&self) -> [u8; 5] {
        self.raw[10..15].try_into().unwrap()
    }
}

/// One 10-second `0x4d` record; only counter, heart rate and steps are decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SportRecord {
    pub counter: u32,
    pub raw: [u8; SPORT_LEN],
}

impl SportRecord {
    pub fn parse(record: &[u8]) -> Option<Self> {
        Some(Self { counter: u32_be(record, 0), raw: record.try_into().ok()? })
    }

    pub fn heart_rate(&self) -> Option<u8> {
        (30..=220).contains(&self.raw[4]).then_some(self.raw[4])
    }

    pub fn steps(&self) -> u8 {
        self.raw[5]
    }
}

/// One 15-minute `0x47` record. Earlier projects called it an optical trend; it holds skin
/// temperature (PROTOCOL.md section 10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpticalRecord {
    pub counter: u32,
    pub raw: Vec<u8>,
}

/// Seconds between two skin temperature samples in a `0x47` record.
pub const SKIN_TEMP_STEP: u32 = 60;

impl OpticalRecord {
    pub fn parse(record: &[u8]) -> Option<Self> {
        (record.len() == OPTICAL_LEN).then(|| Self { counter: u32_be(record, 0), raw: record.to_vec() })
    }
}

/// Fifteen per-minute readings of the two skin sensors in degC, oldest first, from a raw `0x47`
/// record. The record's counter marks the last reading.
pub fn skin_temperatures(raw: &[u8]) -> Option<[[f32; 2]; 15]> {
    let bits: &[u8; 38] = raw.get(9..OPTICAL_LEN)?.try_into().ok()?;
    let mut acc: u128 = 0;
    let mut out = [[0.0f32; 2]; 15];
    let mut n = 0usize;
    let mut held = 0u32;
    for &byte in bits {
        acc = (acc << 8) | byte as u128;
        held += 8;
        while held >= 10 && n < 30 {
            held -= 10;
            let value = ((acc >> held) & 0x3ff) as f32 * 0.05;
            out[n / 2][n % 2] = value;
            n += 1;
        }
    }
    Some(out)
}

/// Live heart rate from a `15 00 <hr> ...` frame; values below 30 are the warm-up placeholder.
pub fn live_heart_rate(frame: &[u8]) -> Option<u8> {
    match frame {
        [op::LIVE, 0x00, hr, ..] if *hr >= 30 => Some(*hr),
        _ => None,
    }
}

/// Live blood oxygen from a long `15 01 ...` frame, byte 14 (PROTOCOL.md section 8, unconfirmed
/// on Gen 2).
pub fn live_spo2(frame: &[u8]) -> Option<u8> {
    match frame {
        [op::LIVE, 0x01, ..] if frame.len() > 15 && (70..=100).contains(&frame[14]) => Some(frame[14]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_gen2() {
        let f = crate::frame::parse_hex("87 49 02 00 00 39 01 07 01 0a 00 00 00 00 0f fa 5b ff 3e").unwrap();
        let d = Descriptor::parse(&f).unwrap();
        assert_eq!((d.battery, d.steps, d.millivolts), (73, 57, 4090));
        assert_eq!(d.skin_temp, [26.3, 26.6]);
        assert_eq!(d.ring_state(), RingState::Idle);
        assert_eq!(d.case_battery(), None);
    }

    #[test]
    fn page_header_is_sixteen_bit() {
        let mut f = vec![0x4c, 0x0f, 0xbc];
        f.extend([0u8; EPOCH_LEN]);
        f.push(0);
        let p = Page::parse(&f).unwrap();
        assert_eq!((p.remaining, p.records.len()), (4028, 1));
    }

    #[test]
    fn epoch_layouts() {
        let mut r = [0u8; EPOCH_LEN];
        r[..4].copy_from_slice(&[0x0c, 0xcd, 0x84, 0x3f]);
        r[4..10].copy_from_slice(&[60, 64, 3, 120, 97, 0x0a]);
        let e = Epoch::parse(&r).unwrap();
        assert_eq!(e.layout(), EpochLayout::SleepVitals);
        assert_eq!((e.heart_rate(), e.hrv(), e.spo2(), e.respiratory_rate()), (Some(60), Some(64), Some(97), Some(15.0)));

        r[8] = 0x12;
        let e = Epoch::parse(&r).unwrap();
        assert_eq!((e.layout(), e.spo2(), e.heart_rate()), (EpochLayout::Activity, None, Some(60)));

        let idle = crate::frame::parse_hex("0c cd 7e 63 05 00 0c 00 00 0a 01 01 01 01 01 00 00 00 00 00 00 00 00").unwrap();
        let e = Epoch::parse(&idle).unwrap();
        assert_eq!((e.layout(), e.heart_rate()), (EpochLayout::Idle, None));
    }

    #[test]
    fn skin_temperature_record() {
        // A Gen 2 record taken while the ring was put back on: room temperature, then warming.
        let raw = crate::frame::parse_hex(
            "0c c4 2d b8 02 a5 05 18 c3 80 a0 18 0a 00 80 a0 08 06 00 95 65 29 5e 57 \
             96 65 79 6e 5a 97 25 c9 72 5b 97 65 c9 6e 5a 95 e5 59 52 51 94 64 d0",
        )
        .unwrap();
        let t = skin_temperatures(&raw).unwrap();
        assert!((t[0][0] - 25.7).abs() < 1e-4 && (t[0][1] - 25.65).abs() < 1e-4, "{:?}", t[0]);
        assert!((t[4][0] - 29.85).abs() < 1e-4, "{:?}", t[4]);
        assert!((t[14][1] - 29.45).abs() < 1e-4, "{:?}", t[14]);
        assert_eq!(skin_temperatures(&raw[..40]), None);
    }

    #[test]
    fn live_samples() {
        assert_eq!(live_heart_rate(&[0x15, 0x00, 0x44, 0x0b, 0xb0, 0xea]), Some(68));
        assert_eq!(live_heart_rate(&[0x15, 0x00, 0x08, 0x0b, 0xb0, 0xa6]), None);
        let mut spo2 = vec![0x15, 0x01, 0x00];
        spo2.resize(16, 0);
        spo2[14] = 0x61;
        assert_eq!(live_spo2(&spo2), Some(97));
        assert_eq!(live_spo2(&[0x15, 0x00, 0x44, 0x0b, 0xb0, 0xea]), None);
    }
}
