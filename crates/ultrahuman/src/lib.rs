//! Ultrahuman Ring AIR: commands, responses, history records and the state characteristic.
//! Protocol facts from Gadgetbridge's experimental Ultrahuman driver (AGPL, facts only); see
//! `docs/ULTRAHUMAN.md`. Every multi-byte field is little-endian.

use uuid::Uuid;

const fn uuid(short: u32) -> Uuid {
    Uuid::from_u128(((short as u128) << 96) | 0x0000_f706_58a0_95b2_1fb9_261e_4dc7)
}

/// Command service, its write and its response characteristics.
pub const COMMAND_SERVICE: Uuid = uuid(0x86f6_5000);
pub const COMMAND: Uuid = uuid(0x86f6_5001);
pub const RESPONSE: Uuid = uuid(0x86f6_5002);
/// Battery, charging and ring temperature; read and notify.
pub const STATE_SERVICE: Uuid = uuid(0x86f6_1000);
pub const STATE: Uuid = uuid(0x86f6_1001);
/// Live exercise and breathing data; notify.
pub const DATA_SERVICE: Uuid = uuid(0x86f6_6000);
pub const DATA: Uuid = uuid(0x86f6_6001);
/// Notifications of unknown meaning.
pub const OTHER_SERVICE: Uuid = uuid(0x86f6_3000);
pub const OTHER: Uuid = uuid(0x86f6_3001);

/// Operation bytes: a command is the operation followed by its arguments, with no checksum.
pub mod op {
    pub const SET_TIME: u8 = 0x02;
    pub const RECORDS: u8 = 0x04;
    pub const FIRST_RECORD: u8 = 0x07;
    pub const LAST_RECORD: u8 = 0x08;
    pub const CHECK_DATA: u8 = 0x59;
    pub const EXERCISE_START: u8 = 0x62;
    pub const EXERCISE_STOP: u8 = 0x64;
    pub const AIRPLANE_MODE: u8 = 0x70;
    pub const BREATHING_START: u8 = 0x72;
    pub const BREATHING_STOP: u8 = 0x74;
    pub const FACTORY_RESET: u8 = 0x98;
    pub const SPO2_ON: u8 = 0xd1;
    pub const SPO2_OFF: u8 = 0xd2;

    pub fn name(op: u8) -> &'static str {
        match op {
            SET_TIME => "set time",
            RECORDS => "records",
            FIRST_RECORD => "first record number",
            LAST_RECORD => "last record number",
            CHECK_DATA => "check data",
            EXERCISE_START => "exercise start",
            EXERCISE_STOP => "exercise stop",
            AIRPLANE_MODE => "airplane mode",
            BREATHING_START => "breathing start",
            BREATHING_STOP => "breathing stop",
            FACTORY_RESET => "factory reset",
            SPO2_ON => "SpO2 on",
            SPO2_OFF => "SpO2 off",
            _ => "unknown",
        }
    }
}

/// Status byte of a records response that has nothing to send.
pub const NOTHING_RECORDED: u8 = 0xee;
/// A records response carries at most this many records; fewer means the ring has sent its last.
pub const RECORDS_PER_RESPONSE: usize = 7;
/// Kind of the records an exercise writes every 2 s when it has a reading. Exercise records
/// without one are kind 10, as are some 5-minute records, so an exercise is told by the 2 s
/// spacing rather than the kind alone.
pub const EXERCISE: u8 = 5;

/// Record times below this (2020-01-01) count from when the ring started, not from 1970: the ring
/// stamps records that way until its clock is set.
pub const CLOCK_SET_AFTER: u32 = 1_577_836_800;
/// Ordinary records are this far apart; exercise records are 2 s apart, and the 5-minute grid
/// starts again after an exercise.
pub const RECORD_INTERVAL: u32 = 300;

/// Size of one history record.
pub const RECORD_LEN: usize = 32;

/// Whether an advertisement belongs to an Ultrahuman ring: the name is `UH_` and the 12 hex
/// digits of the address, and the command service may also be advertised.
pub fn is_ultrahuman<'a>(name: Option<&str>, services: impl IntoIterator<Item = &'a Uuid>) -> bool {
    let named = name.is_some_and(|n| {
        n.strip_prefix("UH_").is_some_and(|rest| rest.len() == 12 && rest.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)))
    });
    named || services.into_iter().any(|u| *u == COMMAND_SERVICE)
}

/// Sets the ring's clock to `unix` (UTC seconds).
pub fn set_time(unix: u32) -> [u8; 5] {
    let t = unix.to_le_bytes();
    [op::SET_TIME, t[0], t[1], t[2], t[3]]
}

/// Asks for every record from `number` on.
pub fn records_from(number: u16) -> [u8; 3] {
    let n = number.to_le_bytes();
    [op::RECORDS, n[0], n[1]]
}

/// A notification on the response characteristic: the operation it answers, a status byte
/// (`00` success, `ff` refused), a result byte (`01` success for most operations), and the rest.
/// The last two bytes of `rest` are a check; see [`check_ok`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response<'a> {
    pub op: u8,
    pub status: u8,
    pub result: u8,
    pub rest: &'a [u8],
}

impl<'a> Response<'a> {
    pub fn parse(raw: &'a [u8]) -> Option<Self> {
        let (&[op, status, result], rest) = raw.split_first_chunk::<3>()?;
        Some(Self { op, status, result, rest })
    }

    pub fn ok(&self) -> bool {
        self.status == 0 && self.result == 1
    }

    /// The record number in a first- or last-record response.
    pub fn record_number(&self) -> Option<u16> {
        (self.ok() && matches!(self.op, op::FIRST_RECORD | op::LAST_RECORD))
            .then(|| self.rest.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]])))
            .flatten()
    }
}

/// What a records response holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Records {
    /// Status `ee`: nothing to send.
    Nothing,
    /// The records in the response; fewer than [`RECORDS_PER_RESPONSE`] means it was the last.
    Some(Vec<Record>),
    /// Any other status, or a response too short for the count it gives.
    Error { status: u8, result: u8 },
}

/// Decodes a records response (`04 status count`, then `count` records of 32 bytes).
pub fn records(raw: &[u8]) -> Option<Records> {
    let r = Response::parse(raw)?;
    if r.op != op::RECORDS {
        return None;
    }
    if r.status == NOTHING_RECORDED {
        return Some(Records::Nothing);
    }
    let count = r.result as usize;
    if r.status != 0 || r.rest.len() < count * RECORD_LEN {
        return Some(Records::Error { status: r.status, result: r.result });
    }
    Some(Records::Some(r.rest.chunks_exact(RECORD_LEN).take(count).filter_map(Record::parse).collect()))
}

/// One history record. Each reading has its own time, so the three groups may be stamped apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Time of the optical readings (heart rate, HRV, SpO2), Unix seconds.
    pub optical_at: u32,
    /// 0 when not measured.
    pub heart_rate: u8,
    /// 0 when not measured.
    pub hrv: u8,
    /// 0 when not measured.
    pub spo2: u8,
    /// 100 and above not worn (Gadgetbridge; 100 and 110 seen). During an exercise the ring writes
    /// a record every 2 s: kind [`EXERCISE`] with a reading, 10 without one (ring off the finger).
    /// Kind 1 records carry the latest optical reading (heart rate, often SpO2, sometimes HRV);
    /// one opens each exercise. Kinds 7, 10 and 12 also came as 5-minute records without
    /// readings. Gadgetbridge names 5 breathing and 6 exercise.
    pub kind: u8,
    /// Time of the temperature readings.
    pub temperature_at: u32,
    /// Highest and lowest skin temperature over the record, °C; both 0 when not measured.
    pub temperature_max: f32,
    pub temperature_min: f32,
    /// Time of the activity readings.
    pub activity_at: u32,
    /// Movement intensity, 0 to 255.
    pub intensity: u8,
    pub steps: u16,
    /// 0 to 255. On firmware 02.00.08.53 it equalled HRV in every record that had one.
    pub stress: u8,
    /// The record's number; numbers wrap at 65535.
    pub number: u16,
    /// Bytes 25 and 29, not yet decoded.
    pub unknown: [u8; 2],
}

impl Record {
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < RECORD_LEN {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        Some(Self {
            optical_at: u32_at(0),
            heart_rate: b[4],
            hrv: b[5],
            spo2: b[6],
            kind: b[7],
            temperature_at: u32_at(8),
            temperature_max: f32::from_bits(u32_at(12)),
            temperature_min: f32::from_bits(u32_at(16)),
            activity_at: u32_at(20),
            intensity: b[24],
            steps: u16_at(26),
            stress: b[28],
            number: u16_at(30),
            unknown: [b[25], b[29]],
        })
    }

    pub fn worn(&self) -> bool {
        self.kind < 100
    }

    pub fn exercise(&self) -> bool {
        self.kind == EXERCISE
    }

    /// Whether the record was stamped before the ring's clock was set.
    pub fn undated(&self) -> bool {
        self.activity_at < CLOCK_SET_AFTER
    }
}

/// Dates records stamped before the clock was set, counting back from the next dated record by
/// [`RECORD_INTERVAL`] per record number. Records with no dated record after them stay as they are.
/// Records must be in record-number order. Exercise records break the 5-minute spacing, so the
/// count back holds only across ordinary records, which is what a ring starts with.
pub fn date_undated(records: &mut [Record]) {
    let mut next: Option<(u16, u32)> = None;
    for r in records.iter_mut().rev() {
        if !r.undated() {
            next = Some((r.number, r.activity_at));
        } else if let Some((n, at)) = next {
            let back = n.wrapping_sub(r.number) as u32 * RECORD_INTERVAL;
            let t = at.saturating_sub(back);
            (r.optical_at, r.temperature_at, r.activity_at) = (t, t, t);
        }
    }
}

/// The 7-byte state value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// Battery, percent. Unreliable on the charger: a ring that read 9 % read 100 % after 40
    /// minutes of charging and 6 % ten minutes off it.
    pub battery: u8,
    /// Bytes 1 to 4 as a signed 32-bit value, meaning unknown: 4,698 to 14,097 on the charger on
    /// two rings, and 0, 585, -585 or 1,173 off it.
    pub current: i32,
    /// `00` normal, `03` charging.
    pub state: u8,
    /// The ring's own temperature, °C.
    pub temperature: u8,
}

impl State {
    pub fn parse(b: &[u8]) -> Option<Self> {
        (b.len() == 7).then(|| Self {
            battery: b[0],
            current: i32::from_le_bytes([b[1], b[2], b[3], b[4]]),
            state: b[5],
            temperature: b[6],
        })
    }

    pub fn charging(&self) -> bool {
        self.state == 3
    }
}

/// A live heart-rate frame on the data characteristic during an exercise:
/// `62 06 00 <time:4> <heart rate> <status>` and the response check. Frames come about every 2 s.
/// Status `05` came with readings; `0a` with heart rate 0 from a ring off the finger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    pub at: u32,
    pub heart_rate: u8,
    pub status: u8,
}

impl Live {
    pub fn reading(&self) -> bool {
        self.status == 0x05 && self.heart_rate != 0
    }

    pub fn parse(raw: &[u8]) -> Option<Self> {
        if raw.len() != 11 || raw[0] != op::EXERCISE_START || !check_ok(raw) {
            return None;
        }
        Some(Self { at: u32::from_le_bytes([raw[3], raw[4], raw[5], raw[6]]), heart_rate: raw[7], status: raw[8] })
    }
}

/// Whether a response's last two bytes match its body: CRC-16/CCITT-FALSE over the bytes after the
/// three-byte header, little-endian (confirmed on every response of a probe run, refusals
/// included). Commands carry no check.
pub fn check_ok(raw: &[u8]) -> bool {
    if raw.len() < 5 {
        return false;
    }
    let (data, tail) = raw.split_at(raw.len() - 2);
    crc16_ccitt_false(&data[3..]) == u16::from_le_bytes([tail[0], tail[1]])
}

/// CRC-16/CCITT-FALSE: polynomial 0x1021, initial value 0xffff, no reflection, no final XOR.
pub fn crc16_ccitt_false(data: &[u8]) -> u16 {
    data.iter().fold(0xffff, |mut crc, b| {
        crc ^= (*b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
        crc
    })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(number: u16, at: u32, hr: u8, kind: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(at.to_le_bytes());
        b.extend([hr, 45, 97, kind]);
        b.extend(at.to_le_bytes());
        b.extend(34.5f32.to_le_bytes());
        b.extend(33.25f32.to_le_bytes());
        b.extend(at.to_le_bytes());
        b.extend([30, 0xaa]);
        b.extend(120u16.to_le_bytes());
        b.extend([128, 0xbb]);
        b.extend(number.to_le_bytes());
        b
    }

    #[test]
    fn uuids_share_the_base() {
        assert_eq!(COMMAND.to_string(), "86f65001-f706-58a0-95b2-1fb9261e4dc7");
        assert_eq!(STATE_SERVICE.to_string(), "86f61000-f706-58a0-95b2-1fb9261e4dc7");
    }

    #[test]
    fn recognises_the_name() {
        assert!(is_ultrahuman(Some("UH_0123456789AB"), []));
        assert!(!is_ultrahuman(Some("UH_0123"), []));
        assert!(!is_ultrahuman(Some("RingConn Gen2"), []));
        assert!(is_ultrahuman(None, [&COMMAND_SERVICE]));
    }

    #[test]
    fn commands() {
        assert_eq!(set_time(0x6912_3456), [0x02, 0x56, 0x34, 0x12, 0x69]);
        assert_eq!(records_from(0x0102), [0x04, 0x02, 0x01]);
    }

    #[test]
    fn record_numbers() {
        let r = Response::parse(&[0x08, 0x00, 0x01, 0x34, 0x12, 0xaa, 0xbb]).unwrap();
        assert_eq!(r.record_number(), Some(0x1234));
        let failed = Response::parse(&[0x07, 0x00, 0x02, 0x34, 0x12]).unwrap();
        assert_eq!(failed.record_number(), None);
        assert_eq!(Response::parse(&[0x07, 0x00]), None);
    }

    #[test]
    fn records_response() {
        let mut raw = vec![op::RECORDS, 0x00, 0x02];
        raw.extend(record(7, 1_762_000_000, 62, 0));
        raw.extend(record(8, 1_762_000_300, 0, 101));
        raw.extend([0x12, 0x34]);
        let Some(Records::Some(rs)) = records(&raw) else { panic!() };
        assert_eq!(rs.len(), 2);
        let r = &rs[0];
        assert_eq!((r.number, r.heart_rate, r.hrv, r.spo2, r.steps, r.stress, r.intensity), (7, 62, 45, 97, 120, 128, 30));
        assert_eq!((r.temperature_max, r.temperature_min), (34.5, 33.25));
        assert_eq!(r.unknown, [0xaa, 0xbb]);
        assert!(r.worn());
        assert!(!rs[1].worn());
        assert_eq!(records(&[op::RECORDS, 0xee, 0x01, 0, 0]), Some(Records::Nothing));
        assert_eq!(records(&[op::RECORDS, 0x00, 0x03, 0, 0]), Some(Records::Error { status: 0, result: 3 }));
    }

    #[test]
    fn dates_records_from_before_the_clock() {
        // As read from a ring whose clock was set after its first two records.
        let raw = |n: u16, at: u32| Record::parse(&record(n, at, 0, 110)).unwrap();
        let mut rs = vec![raw(1, 309), raw(2, 609), raw(3, 0x6af8_8fcf)];
        assert!(rs[0].undated() && !rs[2].undated());
        date_undated(&mut rs);
        assert_eq!(rs.iter().map(|r| r.activity_at).collect::<Vec<_>>(), [0x6af8_8fcf - 600, 0x6af8_8fcf - 300, 0x6af8_8fcf]);
        assert_eq!(rs[0].optical_at, rs[0].activity_at);
    }

    #[test]
    fn state() {
        let s = State::parse(&[0x64, 0x11, 0x37, 0x00, 0x00, 0x03, 0x25]).unwrap();
        assert_eq!((s.battery, s.current, s.charging(), s.temperature), (100, 14097, true, 37));
        let off = State::parse(&[0x06, 0xb7, 0xfd, 0xff, 0xff, 0x00, 0x20]).unwrap();
        assert_eq!((off.battery, off.current, off.charging()), (6, -585, false));
        assert_eq!(State::parse(&[80, 0]), None);
    }

    #[test]
    fn live_frames() {
        let l = Live::parse(&[0x62, 0x06, 0x00, 0xe9, 0x8c, 0xf8, 0x6a, 0x4b, 0x05, 0x86, 0xa3]).unwrap();
        assert_eq!((l.at, l.heart_rate, l.status, l.reading()), (0x6af8_8ce9, 75, 5, true));
        let off = Live::parse(&[0x62, 0x06, 0x00, 0xa1, 0x92, 0xf8, 0x6a, 0x00, 0x0a, 0xff, 0x2f]).unwrap();
        assert!(!off.reading());
        assert_eq!(Live::parse(&[0x62, 0x06, 0x00, 0xe9, 0x8c, 0xf8, 0x6a, 0x4b, 0x05, 0x86, 0xa4]), None);
    }

    #[test]
    fn response_check() {
        // Responses from a Ring AIR on firmware 02.00.10.42.
        for h in ["59000161779d", "0200016c8af86a3f39", "07000101003e2e", "08000107009884", "62ff0111e0e3", "64ff01ff00ff"] {
            let raw: Vec<u8> = (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect();
            assert!(check_ok(&raw), "{h}");
        }
        assert!(!check_ok(&[0x07, 0x00, 0x01, 0x01, 0x00, 0x3e, 0x2f]));
        // Check value for "123456789" in the CRC catalogue.
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29b1);
    }
}
