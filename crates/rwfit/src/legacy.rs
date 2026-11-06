//! The legacy framing: `7E 01 <cmd> <flags> <len> <serial:2> <xor> <payload>`. Flags bit 3 marks
//! a multi-packet frame, which puts `<total:2> <current:2>` (current counted from 1) before the
//! payload; each chunk has its own length and XOR. The ring acknowledges each command with `FE`;
//! the app must acknowledge each frame or chunk with `FF` before parsing it.

use std::collections::HashMap;

pub mod cmd {
    pub const DEVICE_INFO: u8 = 0x00;
    pub const BATTERY: u8 = 0x01;
    pub const BIND_STATUS: u8 = 0x02;
    pub const FEATURES: u8 = 0x03;
    pub const BIND: u8 = 0x20;
    pub const SET_TIME: u8 = 0x21;
    pub const UNITS: u8 = 0x24;
    pub const PROFILE: u8 = 0x2e;
    pub const UNBIND: u8 = 0x44;
    /// Battery pushed by the ring.
    pub const BATTERY_PUSH: u8 = 0x60;
    pub const SYNC_MANIFEST: u8 = 0xa0;
    pub const STEPS: u8 = 0xa1;
    pub const SLEEP: u8 = 0xa2;
    pub const HEART_RATE: u8 = 0xa3;
    pub const BLOOD_PRESSURE: u8 = 0xa4;
    pub const SPO2: u8 = 0xa5;
    pub const TEMPERATURE: u8 = 0xa6;
    pub const BREATHING: u8 = 0xa7;
    pub const DEVICE_ACK: u8 = 0xfe;
    pub const APP_ACK: u8 = 0xff;
}

/// Acknowledgement status.
pub const ACK_OK: u8 = 0;
/// Asks the ring to send the frame again.
pub const ACK_BAD_CHECKSUM: u8 = 2;

pub fn xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |a, b| a ^ b)
}

/// One single-packet frame. Payloads longer than 255 bytes are only used for file transfers.
pub fn encode(cmd: u8, payload: &[u8], serial: u16) -> Vec<u8> {
    assert!(payload.len() <= 255, "legacy payload over 255 bytes");
    let mut f = vec![0x7e, 0x01, cmd, 0x00, payload.len() as u8];
    f.extend_from_slice(&serial.to_be_bytes());
    f.push(xor(payload));
    f.extend_from_slice(payload);
    f
}

/// The app's acknowledgement of a ring frame: cmd `FF`, payload `<ring serial:2> <cmd> <status>`,
/// sent with a serial of its own.
pub fn ack(cmd: u8, ring_serial: u16, status: u8, serial: u16) -> Vec<u8> {
    let s = ring_serial.to_be_bytes();
    encode(cmd::APP_ACK, &[s[0], s[1], cmd, status], serial)
}

/// Request bodies.
pub fn set_time(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Vec<u8> {
    let y = year.to_be_bytes();
    vec![y[0], y[1], month, day, hour, minute, second]
}

/// Bind: type 1, then the user id in UTF-16LE. The ring only stores and echoes the id.
pub fn bind(user_id: &str) -> Vec<u8> {
    let mut p = vec![0x01];
    for u in user_id.encode_utf16() {
        p.extend_from_slice(&u.to_le_bytes());
    }
    p
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// Acknowledge with `ack(cmd, serial, ACK_OK, …)`; comes before the matching `Frame`.
    AckNeeded { cmd: u8, serial: u16 },
    /// Acknowledge with status `ACK_BAD_CHECKSUM`.
    ChecksumFailed { cmd: u8, serial: u16 },
    /// The ring's acknowledgement of one of our commands.
    DeviceAck { cmd: u8, serial: u16, status: u8 },
    /// A whole frame, multi-packet ones reassembled.
    Frame { cmd: u8, payload: Vec<u8> },
}

/// Deframes notifications and reassembles multi-packet frames.
#[derive(Default)]
pub struct Decoder {
    partial: HashMap<u8, Vec<(u16, Vec<u8>)>>,
}

impl Decoder {
    /// A chunk left from a dropped link must not complete a frame on the next one.
    pub fn reset(&mut self) {
        self.partial.clear();
    }

    pub fn decode(&mut self, b: &[u8]) -> Vec<Inbound> {
        if b.len() < 8 || b[0] != 0x7e {
            return Vec::new();
        }
        let (cmd, multi, len) = (b[2], b[3] & 0x08 != 0, b[4] as usize);
        let serial = u16::from_be_bytes([b[5], b[6]]);
        let check = b[7];
        if !multi {
            let Some(payload) = b.get(8..8 + len) else { return Vec::new() };
            if xor(payload) != check {
                return vec![Inbound::ChecksumFailed { cmd, serial }];
            }
            if cmd == cmd::DEVICE_ACK {
                if payload.len() < 4 {
                    return Vec::new();
                }
                return vec![Inbound::DeviceAck {
                    cmd: payload[2],
                    serial: u16::from_be_bytes([payload[0], payload[1]]),
                    status: payload[3],
                }];
            }
            return vec![Inbound::AckNeeded { cmd, serial }, Inbound::Frame { cmd, payload: payload.to_vec() }];
        }
        let (Some(head), Some(chunk)) = (b.get(8..12), b.get(12..12 + len)) else { return Vec::new() };
        if xor(chunk) != check {
            return vec![Inbound::ChecksumFailed { cmd, serial }];
        }
        let total = u16::from_be_bytes([head[0], head[1]]);
        let current = u16::from_be_bytes([head[2], head[3]]);
        let mut out = vec![Inbound::AckNeeded { cmd, serial }];
        let parts = self.partial.entry(cmd).or_default();
        parts.retain(|(i, _)| *i != current);
        parts.push((current, chunk.to_vec()));
        if parts.len() == total as usize {
            let mut parts = self.partial.remove(&cmd).unwrap_or_default();
            parts.sort_by_key(|(i, _)| *i);
            out.push(Inbound::Frame { cmd, payload: parts.into_iter().flat_map(|(_, c)| c).collect() });
        }
        out
    }
}

/// Names for the report.
pub fn name(cmd: u8) -> &'static str {
    match cmd {
        cmd::DEVICE_INFO => "device info",
        cmd::BATTERY => "battery",
        cmd::BIND_STATUS => "bind status",
        cmd::FEATURES => "features",
        cmd::BIND => "bind",
        cmd::SET_TIME => "set time",
        cmd::UNITS => "units",
        cmd::PROFILE => "profile",
        cmd::UNBIND => "unbind",
        cmd::BATTERY_PUSH => "battery push",
        cmd::SYNC_MANIFEST => "sync manifest",
        cmd::STEPS => "steps history",
        cmd::SLEEP => "sleep history",
        cmd::HEART_RATE => "heart-rate history",
        cmd::BLOOD_PRESSURE => "blood-pressure history",
        cmd::SPO2 => "SpO2 history",
        cmd::TEMPERATURE => "temperature history",
        cmd::BREATHING => "breathing history",
        cmd::DEVICE_ACK => "ring ack",
        cmd::APP_ACK => "app ack",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_and_ack_round_trip() {
        let f = encode(cmd::BATTERY, &[], 7);
        assert_eq!(f, vec![0x7e, 0x01, 0x01, 0x00, 0x00, 0x00, 0x07, 0x00]);
        let mut d = Decoder::default();
        let ring = encode(cmd::BATTERY, &[0, 1, 87], 300);
        assert_eq!(d.decode(&ring), vec![Inbound::AckNeeded { cmd: 1, serial: 300 }, Inbound::Frame { cmd: 1, payload: vec![0, 1, 87] }]);
        let a = ack(cmd::BATTERY, 300, ACK_OK, 8);
        assert_eq!(&a[8..], &[0x01, 0x2c, 0x01, 0x00]);
        assert_eq!(
            d.decode(&encode(cmd::DEVICE_ACK, &[0x00, 0x07, 0x01, 0x00], 301)),
            vec![Inbound::DeviceAck { cmd: 1, serial: 7, status: 0 }]
        );
    }

    #[test]
    fn bad_xor_asks_for_retransmission() {
        let mut f = encode(cmd::BATTERY, &[0, 1, 87], 5);
        f[7] ^= 0xff;
        assert_eq!(Decoder::default().decode(&f), vec![Inbound::ChecksumFailed { cmd: 1, serial: 5 }]);
    }

    #[test]
    fn multi_packet_reassembly() {
        let chunk = |current: u16, data: &[u8], serial: u16| {
            let mut f = vec![0x7e, 0x01, cmd::HEART_RATE, 0x08, data.len() as u8];
            f.extend_from_slice(&serial.to_be_bytes());
            f.push(xor(data));
            f.extend_from_slice(&2u16.to_be_bytes());
            f.extend_from_slice(&current.to_be_bytes());
            f.extend_from_slice(data);
            f
        };
        let mut d = Decoder::default();
        assert_eq!(d.decode(&chunk(2, &[3, 4], 11)), vec![Inbound::AckNeeded { cmd: 0xa3, serial: 11 }]);
        assert_eq!(
            d.decode(&chunk(1, &[1, 2], 10)),
            vec![Inbound::AckNeeded { cmd: 0xa3, serial: 10 }, Inbound::Frame { cmd: 0xa3, payload: vec![1, 2, 3, 4] }]
        );
    }

    #[test]
    fn bind_body() {
        assert_eq!(bind("RM"), vec![0x01, b'R', 0, b'M', 0]);
    }
}
