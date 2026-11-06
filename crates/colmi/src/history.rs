//! History answered in 16-byte packets: one request yields several, fed here one by one.

use crate::cmd;
use crate::packet::from_bcd;
use chrono::{NaiveDate, NaiveDateTime};

#[derive(Debug, Clone, PartialEq)]
pub enum Step<T> {
    More,
    Done(T),
    /// The ring has nothing for this request.
    Empty,
}

/// One quarter hour of activity, in the ring's local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quarter {
    pub date: NaiveDate,
    /// 0 to 95.
    pub index: u8,
    pub steps: u16,
    pub calories: u32,
    /// Metres.
    pub distance: u16,
}

impl Quarter {
    pub fn start(&self) -> Option<NaiveDateTime> {
        self.date.and_hms_opt(self.index as u32 / 4, self.index as u32 % 4 * 15, 0)
    }
}

/// Answer to `request::activity`.
#[derive(Debug, Default)]
pub struct ActivityDay {
    tenths: bool,
    quarters: Vec<Quarter>,
}

impl ActivityDay {
    pub fn feed(&mut self, p: &[u8]) -> Step<Vec<Quarter>> {
        if p.len() < 13 || p[0] != cmd::ACTIVITY_HISTORY {
            return Step::More;
        }
        match p[1] {
            0xff => return Step::Empty,
            // Header; newer firmware counts calories in tenths and says so here.
            0xf0 => {
                self.tenths = p[3] == 1;
                return Step::More;
            }
            _ => {}
        }
        let date = NaiveDate::from_ymd_opt(2000 + from_bcd(p[1]) as i32, from_bcd(p[2]), from_bcd(p[3]));
        let calories = u16::from_le_bytes([p[7], p[8]]) as u32;
        if let Some(date) = date {
            self.quarters.push(Quarter {
                date,
                index: p[4],
                steps: u16::from_le_bytes([p[9], p[10]]),
                calories: if self.tenths { calories * 10 } else { calories },
                distance: u16::from_le_bytes([p[11], p[12]]),
            });
        }
        let (current, total) = (p[5], p[6]);
        if current.saturating_add(1) >= total {
            Step::Done(std::mem::take(&mut self.quarters))
        } else {
            Step::More
        }
    }
}

/// Answer to `request::heart_rate`: (minute of the local day, bpm) for each measured slot.
#[derive(Debug, Default)]
pub struct HeartRateDay {
    total: u8,
    interval: u16,
    values: Vec<(u16, u8)>,
}

impl HeartRateDay {
    pub fn feed(&mut self, p: &[u8]) -> Step<Vec<(u16, u8)>> {
        if p.len() < 15 || p[0] != cmd::HEART_RATE_HISTORY {
            return Step::More;
        }
        let n = p[1];
        match n {
            0xff => return Step::Empty,
            0 => {
                self.total = p[2];
                self.interval = if p[3] == 0 { 5 } else { p[3] as u16 };
                return Step::More;
            }
            _ => {}
        }
        // Packet 1 carries the requested timestamp in bytes 2 to 5, then 9 slots; later packets
        // carry 13 slots each.
        let (first, before) = if n == 1 { (6, 0) } else { (2, 9 + (n as usize - 2) * 13) };
        let interval = if self.interval == 0 { 5 } else { self.interval };
        for (i, &bpm) in p[first..15].iter().enumerate() {
            let minute = (before + i) as u16 * interval;
            if bpm != 0 && minute < 1440 {
                self.values.push((minute, bpm));
            }
        }
        if self.total > 0 && n + 1 >= self.total {
            Step::Done(std::mem::take(&mut self.values))
        } else {
            Step::More
        }
    }
}

/// Answer to `request::hrv` or `request::stress`: (minute of the local day, value) every half
/// hour.
#[derive(Debug)]
pub struct HalfHourly {
    command: u8,
    total: u8,
    values: Vec<(u16, u8)>,
}

impl HalfHourly {
    pub fn hrv() -> Self {
        Self { command: cmd::HRV_HISTORY, total: 0, values: Vec::new() }
    }

    pub fn stress() -> Self {
        Self { command: cmd::STRESS_HISTORY, total: 0, values: Vec::new() }
    }

    pub fn feed(&mut self, p: &[u8]) -> Step<Vec<(u16, u8)>> {
        if p.len() < 15 || p[0] != self.command {
            return Step::More;
        }
        let n = p[1];
        match n {
            0xff => return Step::Empty,
            0 => {
                self.total = p[2];
                return Step::More;
            }
            _ => {}
        }
        // Packet 1 holds 12 values from byte 3; later packets 13 from byte 2. Four data packets
        // cover the day.
        let (first, before) = if n == 1 { (3, 0) } else { (2, 12 + (n as usize - 2) * 13) };
        for (i, &v) in p[first..15].iter().enumerate() {
            let minute = (before + i) as u16 * 30;
            if v != 0 && minute < 1440 {
                self.values.push((minute, v));
            }
        }
        if n >= 4 || (self.total > 0 && n + 1 >= self.total) {
            Step::Done(std::mem::take(&mut self.values))
        } else {
            Step::More
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(bytes: &[u8]) -> [u8; 16] {
        let mut p = [0u8; 16];
        p[..15].copy_from_slice(&bytes[..15]);
        p[15] = crate::packet::checksum(&p[..15]);
        p
    }

    #[test]
    fn activity_from_tahnok_capture() {
        let packets: [&[u8]; 6] = [
            b"C\xf0\x05\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
            b"C#\x08\x13\x10\x00\x05\xc8\x000\x00\x1b\x00\x00\x00",
            b"C#\x08\x13\x14\x01\x05\xb6\x18\xaa\x04i\x03\x00\x00",
            b"C#\x08\x13\x18\x02\x058\x04\xe1\x00\x95\x00\x00\x00",
            b"C#\x08\x13\x1c\x03\x05\x05\x02l\x00H\x00\x00\x00",
            b"C#\x08\x13L\x04\x05\xef\x01c\x00D\x00\x00\x00",
        ];
        let mut day = ActivityDay::default();
        let mut result = None;
        for p in packets {
            if let Step::Done(q) = day.feed(&packet(p)) {
                result = Some(q);
            }
        }
        let q = result.unwrap();
        assert_eq!(q.len(), 5);
        assert_eq!((q[0].steps, q[0].calories, q[0].distance), (48, 2000, 27));
        assert_eq!((q[1].steps, q[1].distance), (1194, 873));
        assert_eq!(q[4].start().unwrap().to_string(), "2023-08-13 19:00:00");
        assert_eq!(ActivityDay::default().feed(&packet(b"C\xff\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")), Step::Empty);
    }

    #[test]
    fn heart_rate_from_tahnok_capture() {
        let mut packets: Vec<[u8; 16]> = vec![
            packet(b"\x15\x00\x18\x05\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
            packet(b"\x15\x01\x80\xad\xb6f\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
        ];
        for n in 2u8..=0x17 {
            let mut p = [0u8; 15];
            p[0] = 0x15;
            p[1] = n;
            match n {
                0x0f => p[4] = 0x59,
                0x10 => p[3] = 0x6b,
                0x11 => (p[2], p[14]) = (0x60, 0x6b),
                0x13 => p[12] = 0x50,
                0x14 => p[11] = 0x46,
                _ => {}
            }
            packets.push(packet(&p));
        }
        let mut day = HeartRateDay::default();
        let mut result = None;
        for p in &packets {
            if let Step::Done(v) = day.feed(p) {
                result = Some(v);
            }
        }
        let v = result.unwrap();
        // Packet 15, byte 4: slot 9 + 13 * 13 + 2 = 180, 15:00.
        assert_eq!(v[0], (900, 89));
        assert_eq!(v.len(), 6);
        assert!(v.iter().all(|(m, _)| *m < 1440));
    }

    #[test]
    fn half_hourly_values() {
        let mut h = HalfHourly::hrv();
        assert_eq!(h.feed(&packet(b"\x39\x00\x05\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00")), Step::More);
        let mut p1 = [0u8; 15];
        (p1[0], p1[1], p1[3], p1[14]) = (0x39, 1, 40, 55);
        assert_eq!(h.feed(&packet(&p1)), Step::More);
        for n in 2..=4u8 {
            let mut p = [0u8; 15];
            (p[0], p[1]) = (0x39, n);
            if n == 4 {
                // Slot 47, the last of the day; slots past it fall beyond midnight.
                (p[11], p[14]) = (33, 99);
            }
            if let Step::Done(v) = h.feed(&packet(&p)) {
                assert_eq!(v, vec![(0, 40), (330, 55), (1410, 33)]);
                return;
            }
        }
        panic!("not done after packet 4");
    }
}
