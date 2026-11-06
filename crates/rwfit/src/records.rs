//! Legacy reply and history layouts. Record times are local wall-clock Unix seconds, kept as the
//! ring sent them; the caller subtracts the UTC offset that was in force when the clock was set.

fn u16be(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u24be(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some((s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32)
}

fn u32be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// `01` reply or `60` push: `<low power> <power status> <percent>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub low: bool,
    pub status: u8,
    pub percent: u8,
}

pub fn battery(b: &[u8]) -> Option<Battery> {
    (b.len() >= 3).then(|| Battery { low: b[0] != 0, status: b[1], percent: b[2] })
}

/// `00` reply: `<len> <class UTF-8> <len> <number UTF-8>`.
pub fn device_info(b: &[u8]) -> Option<(String, String)> {
    let n = *b.first()? as usize;
    let class = String::from_utf8_lossy(b.get(1..1 + n)?).into_owned();
    let m = *b.get(1 + n)? as usize;
    let number = b.get(2 + n..2 + n + m).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
    Some((class, number))
}

/// `02` reply: `<bound> <bind type> <user id UTF-16LE…>`.
pub fn bind_status(b: &[u8]) -> Option<(bool, u8)> {
    (b.len() >= 2).then(|| (b[0] != 0, b[1]))
}

/// `03` reply, byte 0 from bit 0: steps, sleep, heart rate, blood pressure, SpO2, temperature,
/// ECG, breathing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features(pub u8);

impl Features {
    pub const NAMES: [&'static str; 8] = ["steps", "sleep", "heart rate", "blood pressure", "SpO2", "temperature", "ECG", "breathing"];

    pub fn has(self, bit: u8) -> bool {
        self.0 & (1 << bit) != 0
    }

    pub fn names(self) -> Vec<&'static str> {
        (0..8).filter(|b| self.has(*b)).map(|b| Self::NAMES[b as usize]).collect()
    }

    /// History commands for the streams the ring has, in the vendor's order.
    pub fn history_commands(self) -> Vec<u8> {
        use crate::legacy::cmd;
        let bits = [
            (0, cmd::STEPS),
            (1, cmd::SLEEP),
            (2, cmd::HEART_RATE),
            (3, cmd::BLOOD_PRESSURE),
            (4, cmd::SPO2),
            (5, cmd::TEMPERATURE),
            (7, cmd::BREATHING),
        ];
        bits.iter().filter(|(b, _)| self.has(*b)).map(|(_, c)| *c).collect()
    }
}

/// One timed value; `second` is set for blood pressure (diastolic).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub local: u32,
    pub value: f64,
    pub second: Option<f64>,
}

/// Day blocks `<day:4> <count:2>`, then `count` items of `stride` bytes starting with their own
/// time. Heart rate, SpO2, temperature and breathing use 5-byte items; blood pressure 6.
pub fn series(cmd: u8, b: &[u8]) -> Vec<Sample> {
    use crate::legacy::cmd as c;
    let stride = if cmd == c::BLOOD_PRESSURE { 6 } else { 5 };
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(count) = u16be(b, at + 4) {
        at += 6;
        for _ in 0..count {
            let Some(item) = b.get(at..at + stride) else { return out };
            at += stride;
            let (Some(local), raw) = (u32be(item, 0), item[4]) else { continue };
            if raw == 0 {
                continue;
            }
            let value = match cmd {
                // Tenths of a degree above 20 °C.
                c::TEMPERATURE => (raw as f64 + 200.0) / 10.0,
                _ => raw as f64,
            };
            let second = (cmd == c::BLOOD_PRESSURE).then(|| item[5] as f64).filter(|v| *v > 0.0);
            if cmd == c::BLOOD_PRESSURE && second.is_none() {
                continue;
            }
            out.push(Sample { local, value, second });
        }
    }
    out
}

/// Steps day block: `<day:4> <steps:3> <kcal:3> <distance:3> <count:2>`, then `count` 8-byte slots
/// `<slot> <steps:2> <kcal:3> <distance:2>` whose width is not known yet.
#[derive(Debug, Clone, PartialEq)]
pub struct StepsDay {
    pub local: u32,
    pub steps: u32,
    pub kcal: u32,
    pub distance: u32,
    pub slots: Vec<(u8, u16)>,
}

pub fn steps(b: &[u8]) -> Vec<StepsDay> {
    let mut out = Vec::new();
    let mut at = 0;
    while let (Some(local), Some(steps), Some(kcal), Some(distance), Some(count)) =
        (u32be(b, at), u24be(b, at + 4), u24be(b, at + 7), u24be(b, at + 10), u16be(b, at + 13))
    {
        at += 15;
        let mut slots = Vec::new();
        for _ in 0..count {
            let Some(s) = b.get(at..at + 8) else { break };
            slots.push((s[0], u16::from_be_bytes([s[1], s[2]])));
            at += 8;
        }
        out.push(StepsDay { local, steps, kcal, distance, slots });
    }
    out
}

/// Sleep night block: `<night:4> <total minutes:2> <asleep:4> <awake:4> <count:2>`, then `count`
/// items `<minutes> <stage>`, consecutive from `asleep`. Stage 0 awake, 1 light, 2 deep, 3 REM.
#[derive(Debug, Clone, PartialEq)]
pub struct Night {
    pub local: u32,
    pub total_minutes: u16,
    pub asleep: u32,
    pub awake: u32,
    pub stages: Vec<(u8, u8)>,
}

pub fn sleep(b: &[u8]) -> Vec<Night> {
    let mut out = Vec::new();
    let mut at = 0;
    while let (Some(local), Some(total_minutes), Some(asleep), Some(awake), Some(count)) =
        (u32be(b, at), u16be(b, at + 4), u32be(b, at + 6), u32be(b, at + 10), u16be(b, at + 14))
    {
        at += 16;
        let mut stages = Vec::new();
        for _ in 0..count {
            let Some(s) = b.get(at..at + 2) else { break };
            stages.push((s[0], s[1]));
            at += 2;
        }
        out.push(Night { local, total_minutes, asleep, awake, stages });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy::cmd;

    #[test]
    fn heart_rate_series_skips_empty_items() {
        let mut b = vec![0x66, 0x00, 0x00, 0x00, 0x00, 0x02];
        b.extend_from_slice(&[0x66, 0x00, 0x01, 0x2c, 62]);
        b.extend_from_slice(&[0x66, 0x00, 0x02, 0x58, 0]);
        assert_eq!(series(cmd::HEART_RATE, &b), vec![Sample { local: 0x6600_012c, value: 62.0, second: None }]);
    }

    #[test]
    fn temperature_offset() {
        let b = [0, 0, 0, 0, 0, 1, 0, 0, 0, 9, 160];
        assert_eq!(series(cmd::TEMPERATURE, &b)[0].value, 36.0);
    }

    #[test]
    fn blood_pressure_pairs() {
        let b = [0, 0, 0, 0, 0, 1, 0, 0, 0, 9, 120, 80];
        assert_eq!(series(cmd::BLOOD_PRESSURE, &b), vec![Sample { local: 9, value: 120.0, second: Some(80.0) }]);
    }

    #[test]
    fn steps_and_sleep_blocks() {
        let mut s = vec![0, 0, 0, 1, 0x00, 0x10, 0x00, 0, 0, 5, 0, 0, 7, 0, 1];
        s.extend_from_slice(&[3, 0x01, 0x00, 0, 0, 0, 0, 0]);
        let d = steps(&s);
        assert_eq!((d[0].steps, d[0].slots.clone()), (4096, vec![(3, 256)]));
        let n = [0, 0, 0, 1, 0, 90, 0, 0, 0, 2, 0, 0, 0, 3, 0, 2, 30, 1, 60, 2];
        assert_eq!(sleep(&n)[0].stages, vec![(30, 1), (60, 2)]);
    }

    #[test]
    fn info_and_battery() {
        assert_eq!(device_info(&[2, b'R', b'1', 3, b'0', b'0', b'7']), Some(("R1".into(), "007".into())));
        assert_eq!(battery(&[0, 1, 87]).map(|b| b.percent), Some(87));
        assert_eq!(Features(0b1000_0101).names(), vec!["steps", "heart rate", "breathing"]);
    }
}
