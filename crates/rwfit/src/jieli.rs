//! The JieLi framing: `AB <flag> <len:2> <crc:2> <payload>`, CRC-16/ARC over the payload, which
//! starts with a `<cmd> <key> <keyflag>` triple. A payload longer than one notification continues
//! in packets with no header. Flag `01` is a request or push and is acknowledged with flag `11`
//! and the echoed triple; flag `11` replies are not acknowledged.

pub const REQUEST: u8 = 0x01;
pub const REPLY: u8 = 0x11;

/// Reflected polynomial `0xA001`, initial 0, no final XOR.
pub fn crc16_arc(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bytes {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xa001 } else { crc >> 1 };
        }
    }
    crc
}

pub fn encode(payload: &[u8], flag: u8) -> Vec<u8> {
    let crc = crc16_arc(payload);
    let mut f = vec![0xab, flag];
    f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    f.extend_from_slice(&crc.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

pub fn ack(triple: [u8; 3]) -> Vec<u8> {
    encode(&triple, REPLY)
}

/// Requests.
pub const DEVICE_INFO: [u8; 3] = [0x02, 0x04, 0x10];
pub const BATTERY: [u8; 3] = [0x02, 0x03, 0x10];
pub const FUNCTION_MENU: [u8; 3] = [0x02, 0x63, 0x10];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    /// Payload includes the triple.
    Frame {
        flag: u8,
        payload: Vec<u8>,
    },
    CrcFailed,
}

const MAX_PAYLOAD: usize = 64 * 1024;

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    pub fn decode(&mut self, data: &[u8]) -> Vec<Inbound> {
        self.buffer.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let Some(start) = self.buffer.iter().position(|b| *b == 0xab) else {
                self.buffer.clear();
                break;
            };
            self.buffer.drain(..start);
            if self.buffer.len() < 6 {
                break;
            }
            let flag = self.buffer[1];
            let len = u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize;
            if ![0x01, 0x11, 0x21].contains(&flag) || !(3..=MAX_PAYLOAD).contains(&len) {
                self.buffer.drain(..1);
                continue;
            }
            if self.buffer.len() < 6 + len {
                break;
            }
            let crc = u16::from_be_bytes([self.buffer[4], self.buffer[5]]);
            let payload = self.buffer[6..6 + len].to_vec();
            self.buffer.drain(..6 + len);
            out.push(if crc16_arc(&payload) == crc { Inbound::Frame { flag, payload } } else { Inbound::CrcFailed });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        assert_eq!(crc16_arc(b"123456789"), 0xbb3d);
    }

    #[test]
    fn continuation_packets() {
        let f = encode(&[0x05, 0x03, 0x10, 1, 2, 3, 4, 5], REQUEST);
        let mut d = Decoder::default();
        assert!(d.decode(&f[..7]).is_empty());
        assert_eq!(d.decode(&f[7..]), vec![Inbound::Frame { flag: 1, payload: vec![0x05, 0x03, 0x10, 1, 2, 3, 4, 5] }]);
    }
}
